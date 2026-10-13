//! Writing tiled, layered TIFFs.
//!
//! The `tiff` crate we read with can write BigTIFF but only in strips, and its
//! writer owns the output while a directory is being built — which rules out
//! the layout we need, where every layer's squares are written before any
//! directory is. So the bytes are written here instead. It is mechanical work:
//! a header, the compressed squares one after another, then one directory per
//! layer saying where each square landed.
//!
//! The file is checked by our own reader in the tests, which is the only
//! verification that means anything — a writer that agrees with itself proves
//! nothing.
//!
//! **BigTIFF only when needed.** Ordinary TIFF addresses 4 GB with 32-bit
//! offsets. Writing BigTIFF unconditionally would cost nothing in correctness
//! but would lock out every older program for no reason, so the format is
//! chosen from the size the file is going to be.

use std::io::{Seek, SeekFrom, Write};

/// Tags written. Named rather than numbered at the call sites, because a
/// mistyped number produces a file that is wrong in a way nothing complains
/// about until much later.
mod tag {
    pub const NEW_SUBFILE_TYPE: u16 = 254;
    pub const IMAGE_WIDTH: u16 = 256;
    pub const IMAGE_LENGTH: u16 = 257;
    pub const BITS_PER_SAMPLE: u16 = 258;
    pub const COMPRESSION: u16 = 259;
    pub const PHOTOMETRIC: u16 = 262;
    pub const SAMPLES_PER_PIXEL: u16 = 277;
    pub const PLANAR_CONFIG: u16 = 284;
    pub const TILE_WIDTH: u16 = 322;
    pub const TILE_LENGTH: u16 = 323;
    pub const TILE_OFFSETS: u16 = 324;
    pub const TILE_BYTE_COUNTS: u16 = 325;
    pub const EXTRA_SAMPLES: u16 = 338;
    /// What wrote the file.
    pub const SOFTWARE: u16 = 305;
}

/// The name this build writes into every file it makes.
pub fn software() -> String {
    format!("geo-ref_mapper {}", env!("CARGO_PKG_VERSION"))
}

/// Field types, as TIFF numbers them.
mod kind {
    pub const BYTE: u16 = 1;
    pub const ASCII: u16 = 2;
    pub const SHORT: u16 = 3;
    pub const LONG: u16 = 4;
    pub const DOUBLE: u16 = 12;
    pub const LONG8: u16 = 16;
}

/// A value on its way into a tag.
///
/// Carried as typed lists rather than raw bytes so the count and the type
/// cannot disagree with each other — which produces a file that reads as
/// nonsense with nothing to say why.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Shorts(Vec<u16>),
    Doubles(Vec<f64>),
    Ascii(String),
    /// An opaque block — an XMP packet, and nothing else so far. Not `Ascii`,
    /// which would append a terminator to something whose length is its own.
    Bytes(Vec<u8>),
}

impl Value {
    pub fn kind(&self) -> u16 {
        match self {
            Self::Shorts(_) => kind::SHORT,
            Self::Doubles(_) => kind::DOUBLE,
            Self::Ascii(_) => kind::ASCII,
            Self::Bytes(_) => kind::BYTE,
        }
    }

    pub fn count(&self) -> u64 {
        match self {
            Self::Shorts(values) => values.len() as u64,
            Self::Doubles(values) => values.len() as u64,
            // TIFF counts the terminating zero as part of the string.
            Self::Ascii(text) => text.len() as u64 + 1,
            Self::Bytes(raw) => raw.len() as u64,
        }
    }

    pub fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Shorts(values) => values.iter().flat_map(|v| v.to_le_bytes()).collect(),
            Self::Doubles(values) => values.iter().flat_map(|v| v.to_le_bytes()).collect(),
            Self::Ascii(text) => {
                let mut raw = text.as_bytes().to_vec();
                raw.push(0);
                raw
            }
            Self::Bytes(raw) => raw.clone(),
        }
    }
}

/// WebP, as libtiff and GDAL register it.
pub const COMPRESSION_WEBP: u16 = 50001;
/// The reduced-resolution flag: says a directory holds a smaller copy of the
/// picture in the first one, not a different picture.
const REDUCED_IMAGE: u32 = 1;

/// One layer's worth of already-compressed squares.
pub struct Layer {
    pub width: u32,
    pub height: u32,
    pub tile: u32,
    pub samples: u16,
    /// Row-major, left to right then top to bottom — the order TIFF requires.
    pub tiles: Vec<Vec<u8>>,
    /// Tags copied or added on top of the ones every layer needs — what made
    /// the file, and where on the earth it sits.
    pub extra: Vec<(u16, Value)>,
    /// Tags whose values are written at the *front* of the file rather than
    /// beside the directory at the back.
    ///
    /// Two reasons, both about what happens after the file is finished. A
    /// reader looking for a metadata packet reads the head of the file and
    /// stops — ours does, and so do plenty of others — and the back of a
    /// two-gigabyte map is nowhere near the head. And a value at a known place
    /// near the start can be overwritten later without the directory moving,
    /// which is what lets a crop be re-dragged without rewriting the map.
    pub early: Vec<(u16, Value)>,
}

impl Layer {
    pub fn across(&self) -> u32 {
        self.width.div_ceil(self.tile)
    }

    pub fn down(&self) -> u32 {
        self.height.div_ceil(self.tile)
    }

    fn expected_tiles(&self) -> usize {
        self.across() as usize * self.down() as usize
    }

    fn bytes(&self) -> u64 {
        self.tiles.iter().map(|tile| tile.len() as u64).sum()
    }
}

/// Writes the layers into one file, largest first.
///
/// Order matters: readers take the first directory as the picture and the rest
/// as smaller copies of it, so the full-size layer has to come first.
pub fn write(path: &std::path::Path, layers: &[Layer]) -> std::io::Result<u64> {
    if layers.is_empty() {
        return Err(std::io::Error::other("nothing to write"));
    }
    for layer in layers {
        if layer.tiles.len() != layer.expected_tiles() {
            return Err(std::io::Error::other(format!(
                "layer {}×{} needs {} squares but was given {}",
                layer.width,
                layer.height,
                layer.expected_tiles(),
                layer.tiles.len(),
            )));
        }
    }

    // Decided before anything is written, because it changes every offset in
    // the file. The estimate is the compressed squares plus a generous
    // allowance for the directories; being wrong in the safe direction only
    // costs a slightly larger file.
    let payload: u64 = layers.iter().map(Layer::bytes).sum();
    let big = payload + 1024 * 1024 > u64::from(u32::MAX);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path)?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);

    if big {
        out.write_all(b"II")?;
        out.write_all(&43_u16.to_le_bytes())?;
        out.write_all(&8_u16.to_le_bytes())?; // offset size
        out.write_all(&0_u16.to_le_bytes())?; // reserved, always zero
        out.write_all(&16_u64.to_le_bytes())?; // first directory, patched later
    } else {
        out.write_all(b"II")?;
        out.write_all(&42_u16.to_le_bytes())?;
        out.write_all(&8_u32.to_le_bytes())?; // first directory, patched later
    }

    // The metadata before the squares, so it lands in the first few kilobytes
    // of the file however large the map turns out to be.
    let mut early: Vec<Vec<(u16, u64)>> = Vec::with_capacity(layers.len());
    for layer in layers {
        let mut mine = Vec::with_capacity(layer.early.len());
        for (tag, value) in &layer.early {
            pad(&mut out)?;
            let at = out.stream_position()?;
            out.write_all(&value.bytes())?;
            mine.push((*tag, at));
        }
        early.push(mine);
    }

    // The squares next, all of them, so a directory can state where each one
    // is without having to go back and forth.
    let mut placed: Vec<(Vec<u64>, Vec<u64>)> = Vec::with_capacity(layers.len());
    for layer in layers {
        let mut offsets = Vec::with_capacity(layer.tiles.len());
        let mut counts = Vec::with_capacity(layer.tiles.len());
        for tile in &layer.tiles {
            let at = out.stream_position()?;
            out.write_all(tile)?;
            offsets.push(at);
            counts.push(tile.len() as u64);
        }
        placed.push((offsets, counts));
    }

    // Then one directory per layer. Each records where its link to the next
    // one sits, so the chain can be joined up once every directory exists —
    // a directory cannot say where the next one is before it has been written.
    let mut directories = Vec::with_capacity(layers.len());
    for (index, layer) in layers.iter().enumerate() {
        let (offsets, counts) = &placed[index];
        directories.push(write_directory(
            &mut out,
            layer,
            index > 0,
            offsets,
            counts,
            &early[index],
            big,
        )?);
    }

    for index in 0..directories.len() {
        let next = directories
            .get(index + 1)
            .map(|placed: &Directory| placed.at)
            .unwrap_or(0);
        out.seek(SeekFrom::Start(directories[index].link))?;
        if big {
            out.write_all(&next.to_le_bytes())?;
        } else {
            out.write_all(&(next as u32).to_le_bytes())?;
        }
    }

    // And the header points at the first.
    out.seek(SeekFrom::Start(if big { 8 } else { 4 }))?;
    if big {
        out.write_all(&directories[0].at.to_le_bytes())?;
    } else {
        out.write_all(&(directories[0].at as u32).to_le_bytes())?;
    }

    out.flush()?;
    let file = out.into_inner().map_err(|error| error.into_error())?;
    let size = file.metadata()?.len();
    file.sync_all()?;
    Ok(size)
}

/// Where a written directory sits, and where its link to the next one goes.
struct Directory {
    at: u64,
    link: u64,
}

/// Writes one directory and returns where it starts.
///
/// Values too big to sit inside an entry are written out first and referred to
/// by position — which is the whole reason the entries cannot simply be emitted
/// in order.
fn write_directory<W: Write + Seek>(
    out: &mut W,
    layer: &Layer,
    reduced: bool,
    offsets: &[u64],
    counts: &[u64],
    early: &[(u16, u64)],
    big: bool,
) -> std::io::Result<Directory> {
    let inline = if big { 8 } else { 4 };
    let offset_kind = if big { kind::LONG8 } else { kind::LONG };
    let long8s = |values: &[u64]| -> Vec<u8> {
        if big {
            values.iter().flat_map(|v| v.to_le_bytes()).collect()
        } else {
            values
                .iter()
                .flat_map(|v| (*v as u32).to_le_bytes())
                .collect()
        }
    };

    // Every entry carries its bytes rather than a position. Whether they end up
    // inside the entry or elsewhere in the file is decided below, in one place —
    // deciding it at each call site is how a position gets written where a value
    // belongs, which produces a file that opens and then fails on the first
    // square.
    let mut entries: Vec<(u16, u16, u64, Vec<u8>)> = vec![
        (
            tag::NEW_SUBFILE_TYPE,
            kind::LONG,
            1,
            if reduced { REDUCED_IMAGE } else { 0 }
                .to_le_bytes()
                .to_vec(),
        ),
        (
            tag::IMAGE_WIDTH,
            kind::LONG,
            1,
            layer.width.to_le_bytes().to_vec(),
        ),
        (
            tag::IMAGE_LENGTH,
            kind::LONG,
            1,
            layer.height.to_le_bytes().to_vec(),
        ),
        (
            tag::BITS_PER_SAMPLE,
            kind::SHORT,
            u64::from(layer.samples),
            (0..layer.samples)
                .flat_map(|_| 8_u16.to_le_bytes())
                .collect(),
        ),
        (
            tag::COMPRESSION,
            kind::SHORT,
            1,
            COMPRESSION_WEBP.to_le_bytes().to_vec(),
        ),
        (
            tag::PHOTOMETRIC,
            kind::SHORT,
            1,
            2_u16.to_le_bytes().to_vec(),
        ),
        (
            tag::SAMPLES_PER_PIXEL,
            kind::SHORT,
            1,
            layer.samples.to_le_bytes().to_vec(),
        ),
        (
            tag::PLANAR_CONFIG,
            kind::SHORT,
            1,
            1_u16.to_le_bytes().to_vec(),
        ),
        (
            tag::TILE_WIDTH,
            kind::SHORT,
            1,
            (layer.tile as u16).to_le_bytes().to_vec(),
        ),
        (
            tag::TILE_LENGTH,
            kind::SHORT,
            1,
            (layer.tile as u16).to_le_bytes().to_vec(),
        ),
        (
            tag::TILE_OFFSETS,
            offset_kind,
            offsets.len() as u64,
            long8s(offsets),
        ),
        (
            tag::TILE_BYTE_COUNTS,
            offset_kind,
            counts.len() as u64,
            long8s(counts),
        ),
    ];
    if layer.samples == 4 {
        // 2 = unassociated alpha, which is what an ordinary RGBA picture has.
        entries.push((
            tag::EXTRA_SAMPLES,
            kind::SHORT,
            1,
            2_u16.to_le_bytes().to_vec(),
        ));
    }
    // Which build made the file. Written on every layer so a directory read on
    // its own still says where it came from.
    let stamp = Value::Ascii(software());
    entries.push((tag::SOFTWARE, stamp.kind(), stamp.count(), stamp.bytes()));
    for (tag, value) in &layer.extra {
        entries.push((*tag, value.kind(), value.count(), value.bytes()));
    }
    // The early values are already in the file. Their entries carry no bytes,
    // because the position below comes from where they were actually put — an
    // entry that carried them as well would be a second copy free to disagree.
    for (tag, value) in &layer.early {
        entries.push((*tag, value.kind(), value.count(), Vec::new()));
    }
    // TIFF requires entries in ascending tag order, and readers are entitled to
    // binary-search them.
    entries.sort_by_key(|(tag, ..)| *tag);
    entries.dedup_by_key(|(tag, ..)| *tag);

    // Anything too long to fit is written out first; the entry then holds where
    // it went.
    let mut elsewhere: Vec<Option<u64>> = Vec::with_capacity(entries.len());
    for (tag, _, _, bytes) in &entries {
        // Written at the front of the file already; the entry only has to point
        // at it. Checked before the size test, or an entry carrying no bytes
        // would look small enough to fit and be filled with zeroes.
        if let Some((_, at)) = early.iter().find(|(early_tag, _)| early_tag == tag) {
            elsewhere.push(Some(*at));
            continue;
        }
        if bytes.len() as u64 <= inline {
            elsewhere.push(None);
            continue;
        }
        let at = out.stream_position()?;
        out.write_all(bytes)?;
        pad(out)?;
        elsewhere.push(Some(at));
    }

    let at = out.stream_position()?;
    if big {
        out.write_all(&(entries.len() as u64).to_le_bytes())?;
    } else {
        out.write_all(&(entries.len() as u16).to_le_bytes())?;
    }
    for (index, (tag, kind, count, bytes)) in entries.iter().enumerate() {
        out.write_all(&tag.to_le_bytes())?;
        out.write_all(&kind.to_le_bytes())?;
        if big {
            out.write_all(&count.to_le_bytes())?;
        } else {
            out.write_all(&(*count as u32).to_le_bytes())?;
        }

        let mut field = [0_u8; 8];
        match elsewhere[index] {
            // It did not fit: the field holds where it went.
            Some(position) if big => field.copy_from_slice(&position.to_le_bytes()),
            Some(position) => field[..4].copy_from_slice(&(position as u32).to_le_bytes()),
            // It fits: the field holds the bytes themselves, packed from the
            // left, exactly as they would appear elsewhere in the file.
            None => field[..bytes.len()].copy_from_slice(bytes),
        }
        out.write_all(&field[..inline as usize])?;
    }

    // Space for the link to the next directory, filled in once it exists.
    let link = out.stream_position()?;
    if big {
        out.write_all(&0_u64.to_le_bytes())?;
    } else {
        out.write_all(&0_u32.to_le_bytes())?;
    }
    Ok(Directory { at, link })
}

/// TIFF requires values to start on an even byte.
fn pad<W: Write + Seek>(out: &mut W) -> std::io::Result<()> {
    if out.stream_position()? % 2 == 1 {
        out.write_all(&[0])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-tiffwrite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// A solid-colour square, compressed the way the converter will compress
    /// them.
    fn square(edge: u32, shade: u8) -> Vec<u8> {
        let pixels = vec![shade; (edge * edge * 4) as usize];
        webp::Encoder::from_rgba(&pixels, edge, edge)
            .encode(95.0)
            .to_vec()
    }

    #[test]
    fn an_early_value_lands_at_the_front_with_the_squares_behind_it() {
        // The point of `early`: a reader that gives up after the head of the
        // file still finds the metadata, however many megabytes follow.
        let mut first = layer(1024, 1024, 256, 90);
        let packet = format!("<x:xmpmeta>{}</x:xmpmeta>", "-".repeat(2000));
        first.early = vec![(700, Value::Bytes(packet.clone().into_bytes()))];

        let target = scratch("early.tif");
        let _ = std::fs::remove_file(&target);
        write(&target, &[first, layer(512, 512, 256, 90)]).unwrap();

        let raw = std::fs::read(&target).unwrap();
        let at = raw
            .windows(10)
            .position(|window| window == b"<x:xmpmeta")
            .expect("the packet is in the file");
        assert!(at < 64, "the packet starts at {at}, which is not the front");

        // And the squares really are behind it, rather than the packet having
        // been appended after them.
        let (_, offsets) = crate::geotiff::raw_tag(&target, 324).expect("the square offsets");
        let first_square = u32::from_le_bytes(offsets[..4].try_into().unwrap()) as usize;
        assert!(
            first_square >= at + packet.len(),
            "the first square is at {first_square}, inside the packet at {at}",
        );

        // And the entry points at it rather than at a second copy.
        let (kind, bytes) = crate::geotiff::raw_tag(&target, 700).expect("tag 700");
        assert_eq!(kind, kind::BYTE);
        assert_eq!(String::from_utf8_lossy(&bytes), packet);
    }

    fn layer(width: u32, height: u32, tile: u32, shade: u8) -> Layer {
        let across = width.div_ceil(tile);
        let down = height.div_ceil(tile);
        Layer {
            width,
            height,
            tile,
            samples: 4,
            tiles: (0..across * down).map(|_| square(tile, shade)).collect(),
            extra: Vec::new(),
            early: Vec::new(),
        }
    }

    #[test]
    fn our_own_reader_can_open_what_we_write() {
        let path = scratch("two-layers.tif");
        write(
            &path,
            &[layer(512, 512, 256, 200), layer(256, 256, 256, 100)],
        )
        .unwrap();

        let tiff = crate::cog::TiledTiff::open(&path).expect("our reader opens it");
        let sizes = tiff.level_sizes();
        assert_eq!(
            sizes,
            vec![(512, 512), (256, 256)],
            "both layers must be found, largest first",
        );
    }

    #[test]
    fn the_pixels_come_back() {
        let path = scratch("pixels.tif");
        write(&path, &[layer(256, 256, 256, 173)]).unwrap();

        let mut tiff = crate::cog::TiledTiff::open(&path).unwrap();
        let image = tiff.decode_level(0).expect("it decodes");
        assert_eq!(image.dimensions(), (256, 256));
        let pixel = image.get_pixel(128, 128);
        // WebP at 95 is not exact, so this is "the shade we wrote", not "173".
        assert!(
            pixel[0].abs_diff(173) < 6,
            "middle pixel came back as {:?}",
            pixel,
        );
    }

    #[test]
    fn a_short_layer_is_refused_rather_than_written_wrong() {
        let path = scratch("short.tif");
        let mut broken = layer(512, 512, 256, 10);
        broken.tiles.pop();
        let error = write(&path, &[broken]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("needs 4 squares but was given 3"),
            "got: {error}",
        );
    }

    #[test]
    fn an_edge_that_does_not_divide_still_gets_whole_squares() {
        // 300 wide needs two squares across, the second mostly empty. TIFF
        // requires the squares be full size regardless.
        let path = scratch("ragged.tif");
        write(&path, &[layer(300, 300, 256, 42)]).unwrap();
        let tiff = crate::cog::TiledTiff::open(&path).unwrap();
        assert_eq!(tiff.level_sizes(), vec![(300, 300)]);
    }

    #[test]
    fn the_file_says_which_build_made_it() {
        let path = scratch("stamped.tif");
        write(&path, &[layer(256, 256, 256, 9)]).unwrap();
        let raw = std::fs::read(&path).unwrap();
        let expected = software();
        assert!(
            raw.windows(expected.len())
                .any(|window| window == expected.as_bytes()),
            "the file should carry {expected:?}",
        );
        assert!(expected.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn a_copied_tag_survives_the_round_trip() {
        // The numbers that say where a map sits are copied verbatim; getting
        // them subtly wrong would put a map in the sea with nothing to say why.
        let path = scratch("positioned.tif");
        let mut with_position = layer(256, 256, 256, 60);
        with_position.extra = vec![
            (33550, Value::Doubles(vec![0.298_582_141_738_918_4; 3])),
            (
                33922,
                Value::Doubles(vec![
                    0.0,
                    0.0,
                    0.0,
                    11_554_603.380_728_67,
                    148_440.708_929_81,
                    0.0,
                ]),
            ),
            (
                34737,
                Value::Ascii("WGS 84 / Pseudo-Mercator|WGS 84|".to_owned()),
            ),
        ];
        write(&path, &[with_position]).unwrap();

        let geo = crate::geotiff::from_path(&path).expect("it reads back as georeferenced");
        assert!((geo.origin.0 - 11_554_603.380_728_67).abs() < 1e-6);
        assert!((geo.origin.1 - 148_440.708_929_81).abs() < 1e-6);
        assert!((geo.scale.0 - 0.298_582_141_738_918_4).abs() < 1e-12);
        assert_eq!(geo.citations, vec!["WGS 84 / Pseudo-Mercator", "WGS 84"]);
    }

    #[test]
    fn a_small_file_is_not_written_as_bigtiff() {
        let path = scratch("small.tif");
        write(&path, &[layer(256, 256, 256, 5)]).unwrap();
        let header = std::fs::read(&path).unwrap();
        assert_eq!(
            u16::from_le_bytes([header[2], header[3]]),
            42,
            "ordinary TIFF, so older programs can still open it",
        );
    }
}
