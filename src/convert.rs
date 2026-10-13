//! Turning a large map into one that zooms quickly.
//!
//! The output is a single TIFF holding the same picture several times over:
//! full size, half, quarter, and so on down to about screen size. Every layer
//! is cut into 256-pixel squares. Showing the whole map then reads the smallest
//! layer, and zooming in reads a handful of squares from a larger one — a
//! screenful either way, however many pixels the map has.
//!
//! **The source is never touched.** Everything here reads from it and writes
//! somewhere else, and the destination is refused outright if it is the source.
//!
//! Memory is bounded by working one square-row at a time and by building each
//! layer from the one above it rather than from the source. A 2.29 gigapixel
//! map would be 9 GB decoded; this never holds more than a few rows of it.

use anyhow::{Context, Result, bail};
use image::RgbaImage;
use std::path::{Path, PathBuf};

/// The edge of one square, chosen with morgan: smaller squares stick out less
/// past the edges of the window, so less is decoded for a given view.
pub const TILE: u32 = 256;

/// WebP quality. Measured on a real map: at 95 the average error is 1 unit in
/// 255 and the file is six times smaller than keeping every pixel exactly.
pub const QUALITY: f32 = 95.0;

/// Layers stop once the picture fits comfortably on screen. Going further costs
/// squares that would only ever be seen as a speck.
const SMALLEST: u32 = 512;

/// How much a picture may need in one piece before the conversion refuses it.
///
/// Only reached by pictures not stored in squares, which cannot be read a part
/// at a time. Two gigabytes covers a 500 megapixel scan; past that the honest
/// answer is that this is the wrong tool.
const WHOLE_PICTURE_CEILING: u64 = 2 * 1024 * 1024 * 1024;

/// What happened, for the queue to record.
#[derive(Clone, Debug)]
pub struct Report {
    pub target: PathBuf,
    /// A file was already there and has been replaced.
    pub replaced: bool,
    pub layers: usize,
    pub bytes: u64,
    pub source_bytes: u64,
}

/// How far along a conversion is.
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub layer: usize,
    pub layers: usize,
    pub row: u32,
    pub rows: u32,
}

impl Progress {
    /// How much of the whole picture is done, from 0 to 1.
    ///
    /// Layers are weighted rather than counted. Each is half the width and half
    /// the height of the one before, so it is a **quarter** of the work — and
    /// counting them equally would make a seven-layer map appear to crawl
    /// through the first layer and then leap through six more in a moment.
    /// Weighting by area, the full-size layer is three quarters of the job,
    /// which is what it actually is.
    pub fn fraction(self) -> f32 {
        let weight = |layer: usize| 0.25_f64.powi(layer as i32);
        let total: f64 = (0..self.layers.max(1)).map(weight).sum();
        let finished: f64 = (0..self.layer.min(self.layers)).map(weight).sum();
        let within = f64::from(self.row) / f64::from(self.rows.max(1));
        (((finished + within * weight(self.layer)) / total) as f32).clamp(0.0, 1.0)
    }
}

/// Converts one map. `watch` is called as rows are finished, and returning
/// `false` from it abandons the job.
pub fn convert(
    source: &Path,
    target: &Path,
    watch: impl FnMut(Progress) -> bool,
) -> Result<Report> {
    convert_with(source, target, threads_available(), watch)
}

/// How many workers to compress with. One core is left alone so the interface
/// still draws while a conversion runs.
fn threads_available() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().saturating_sub(1).max(1))
        .unwrap_or(1)
}

/// The same, with the worker count given — so a test can pin it.
pub fn convert_with(
    source: &Path,
    target: &Path,
    threads: usize,
    mut watch: impl FnMut(Progress) -> bool,
) -> Result<Report> {
    if source == target {
        bail!("the converted copy would overwrite the original");
    }
    // An existing target is replaced rather than refused. There is no way to
    // tell a finished conversion from one abandoned half way — both are just a
    // file — so the only safe reading of one that is already there is that it
    // cannot be trusted.
    //
    // It is replaced by writing *beside* it and renaming on top at the end. A
    // plain overwrite would spend the whole conversion with the old file
    // already destroyed and the new one incomplete, so a crash or a stop would
    // leave nothing usable. With the rename, whatever is at the target is
    // always a whole file: the previous one until the moment there is a better
    // one, and never anything in between.
    let replacing = target.exists();
    let working = working_copy(target);

    let source_bytes = std::fs::metadata(source)
        .with_context(|| format!("reading {}", source.display()))?
        .len();

    let opening = std::time::Instant::now();
    let mut reader = Source::open(source)?;
    let open_time = opening.elapsed().as_secs_f64();
    let (width, height) = reader.size();
    if width == 0 || height == 0 {
        bail!("the picture has no size");
    }

    let sizes = layer_sizes(width, height);
    let mut layers: Vec<crate::tiffwrite::Layer> = Vec::with_capacity(sizes.len());

    // What the sidecar beside the source knows — the title, the year, what the
    // map shows, where it came from. The converted map is a new file under a
    // new name while the sidecar names the old one, so anything not carried
    // across here is lost the moment the conversion finishes.
    let mut embedded = crate::sidecar::embed(source);

    let trace = std::env::var_os("GEO_REF_MAPPER_CONVERT_TRACE").is_some();
    let (mut read_time, mut encode_time) = (0.0_f64, 0.0_f64);

    for (index, (layer_width, layer_height)) in sizes.iter().copied().enumerate() {
        let rows = layer_height.div_ceil(TILE);
        let mut tiles = Vec::new();

        for row in 0..rows {
            // Each layer after the first is built from the one above it, which
            // is already in hand — reading the source again for every layer
            // would decode the whole picture once per layer.
            let started = std::time::Instant::now();
            let band = match index {
                0 => reader.band(row * TILE, TILE.min(layer_height - row * TILE))?,
                _ => shrink_band(&layers[index - 1], row, layer_width, layer_height)?,
            };
            read_time += started.elapsed().as_secs_f64();

            // Compressing is where the time goes — measured at 87% of a real
            // conversion — and every square is independent of every other, so
            // the row is split across the machine. The results are collected in
            // order, because TIFF requires the squares in order and a shuffled
            // file would be wrong in a way that looks like corruption.
            let started = std::time::Instant::now();
            let columns = layer_width.div_ceil(TILE);
            let mut row_tiles =
                compress_row(&band, columns, layer_width, layer_height, row, threads);
            tiles.append(&mut row_tiles);
            encode_time += started.elapsed().as_secs_f64();
            if !watch(Progress {
                layer: index,
                layers: sizes.len(),
                row: row + 1,
                rows,
            }) {
                bail!("stopped");
            }
        }

        layers.push(crate::tiffwrite::Layer {
            width: layer_width,
            height: layer_height,
            tile: TILE,
            samples: 4,
            tiles,
            // Only the full-size layer. A smaller copy has a different pixel
            // scale, so the original numbers would place it in the wrong spot —
            // and the description of a map belongs to the map, not to each of
            // the seven sizes it is held at.
            extra: if index == 0 {
                let mut tags = crate::geotiff::position_tags(source);
                tags.append(&mut embedded.extra);
                tags
            } else {
                Vec::new()
            },
            early: if index == 0 {
                std::mem::take(&mut embedded.early)
            } else {
                Vec::new()
            },
        });
    }

    if trace {
        eprintln!(
            "convert: {open_time:.2}s opening the source, {read_time:.2}s reading and \
             shrinking, {encode_time:.2}s compressing squares"
        );
    }

    let writing = std::time::Instant::now();
    let bytes = crate::tiffwrite::write(&working, &layers)
        .inspect_err(|_| drop(std::fs::remove_file(&working)))
        .with_context(|| format!("writing {}", target.display()))?;
    // The one moment the target changes, and it is a rename: either the whole
    // new file is there or the whole old one still is.
    std::fs::rename(&working, target)
        .inspect_err(|_| drop(std::fs::remove_file(&working)))
        .with_context(|| format!("putting {} in place", target.display()))?;
    if trace {
        eprintln!(
            "convert: {:.2}s writing the file",
            writing.elapsed().as_secs_f64()
        );
    }

    Ok(Report {
        target: target.to_path_buf(),
        replaced: replacing,
        layers: layers.len(),
        bytes,
        source_bytes,
    })
}

/// Where a conversion is written while it is being made.
///
/// Beside the target and in the same folder, so the rename at the end is within
/// one filesystem and therefore atomic — a temporary directory elsewhere would
/// turn it into a copy, which is neither atomic nor free on a 12 GB map.
///
/// The name is deliberately not something the scanner would index as a picture.
fn working_copy(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "map".to_owned());
    target.with_file_name(format!(".{name}.part"))
}

/// The size of every layer, largest first.
///
/// Each is half the one before, and they stop once the picture would fit on a
/// screen. A map already smaller than that gets one layer, which is the honest
/// answer — there is nothing to halve.
pub fn layer_sizes(width: u32, height: u32) -> Vec<(u32, u32)> {
    let mut sizes = vec![(width, height)];
    loop {
        let (last_width, last_height) = *sizes.last().expect("seeded above");
        if last_width.max(last_height) <= SMALLEST {
            break;
        }
        let next = ((last_width / 2).max(1), (last_height / 2).max(1));
        if next == (last_width, last_height) {
            break;
        }
        sizes.push(next);
    }
    sizes
}

/// Compresses one row of squares, spread across the machine.
///
/// Each worker takes every `threads`-th column, so the work is interleaved
/// rather than split into blocks — a row's squares are not all equally
/// expensive, and interleaving keeps the last worker from carrying a run of the
/// slow ones alone.
fn compress_row(
    band: &RgbaImage,
    columns: u32,
    layer_width: u32,
    layer_height: u32,
    row: u32,
    threads: usize,
) -> Vec<Vec<u8>> {
    if threads <= 1 || columns <= 1 {
        return (0..columns)
            .map(|column| encode_square(band, column, layer_width, layer_height, row))
            .collect();
    }

    let mut done: Vec<Vec<u8>> = vec![Vec::new(); columns as usize];
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(threads);
        for worker in 0..threads {
            handles.push(scope.spawn(move || {
                let mut mine = Vec::new();
                let mut column = worker as u32;
                while column < columns {
                    mine.push((
                        column,
                        encode_square(band, column, layer_width, layer_height, row),
                    ));
                    column += threads as u32;
                }
                mine
            }));
        }
        for handle in handles {
            for (column, square) in handle.join().unwrap_or_default() {
                done[column as usize] = square;
            }
        }
    });
    done
}

/// One square of a band, padded out to full size.
///
/// TIFF squares are always the full size even at the right and bottom edges,
/// where the picture runs out. The spare part is filled by repeating the last
/// real row and column outwards.
///
/// It cannot be left blank. WebP mixes neighbouring pixels, so blank padding
/// bleeds into the picture — measured on a 600-wide test image, the last pixel
/// came back 18 shades out instead of 4. Readers never show the padding, so
/// what is in it only matters for what it does to its neighbours.
fn encode_square(
    band: &RgbaImage,
    column: u32,
    layer_width: u32,
    layer_height: u32,
    row: u32,
) -> Vec<u8> {
    let left = column * TILE;
    let top_of_band = row * TILE;
    let usable_width = TILE.min(layer_width.saturating_sub(left));
    let usable_height = TILE.min(layer_height.saturating_sub(top_of_band));

    let mut square = RgbaImage::new(TILE, TILE);
    let real_width = usable_width.min(band.width().saturating_sub(left));
    let real_height = usable_height.min(band.height());
    if real_width == 0 || real_height == 0 {
        return webp::Encoder::from_rgba(square.as_raw(), TILE, TILE)
            .encode(QUALITY)
            .to_vec();
    }
    for y in 0..TILE {
        // Past the picture, keep taking the last real row and column.
        let from_y = y.min(real_height - 1);
        for x in 0..TILE {
            let from_x = x.min(real_width - 1);
            square.put_pixel(x, y, *band.get_pixel(left + from_x, from_y));
        }
    }

    webp::Encoder::from_rgba(square.as_raw(), TILE, TILE)
        .encode(QUALITY)
        .to_vec()
}

/// Builds one square-row of a layer by halving the layer above it.
///
/// The squares of the layer above are decoded back out of what was just
/// compressed. That costs a decode, and it buys never holding a whole layer in
/// memory — which for the largest maps is the difference between 80 MB and
/// 2 GB.
fn shrink_band(
    above: &crate::tiffwrite::Layer,
    row: u32,
    layer_width: u32,
    layer_height: u32,
) -> Result<RgbaImage> {
    let height = TILE.min(layer_height - row * TILE);
    let mut band = RgbaImage::new(layer_width, height);

    // Two rows of squares above make one row here.
    let across_above = above.across();
    for source_row in row * 2..(row * 2 + 2).min(above.down()) {
        for source_column in 0..across_above {
            let index = (source_row * across_above + source_column) as usize;
            let Some(encoded) = above.tiles.get(index) else {
                continue;
            };
            let square = webp::Decoder::new(encoded)
                .decode()
                .context("reading back a square just written")?
                .to_image()
                .to_rgba8();

            // Where this square's halved pixels land in the band.
            let left = source_column * TILE / 2;
            let top = (source_row * TILE / 2).saturating_sub(row * TILE);
            for y in 0..TILE / 2 {
                for x in 0..TILE / 2 {
                    let (into_x, into_y) = (left + x, top + y);
                    if into_x >= layer_width || into_y >= height {
                        continue;
                    }
                    band.put_pixel(into_x, into_y, average(&square, x * 2, y * 2));
                }
            }
        }
    }
    Ok(band)
}

/// The mean of a 2×2 block, which is what halving a picture means.
fn average(square: &RgbaImage, x: u32, y: u32) -> image::Rgba<u8> {
    let mut totals = [0_u32; 4];
    let mut counted = 0;
    for dy in 0..2 {
        for dx in 0..2 {
            let (sx, sy) = (x + dx, y + dy);
            if sx >= square.width() || sy >= square.height() {
                continue;
            }
            let pixel = square.get_pixel(sx, sy);
            for channel in 0..4 {
                totals[channel] += u32::from(pixel[channel]);
            }
            counted += 1;
        }
    }
    if counted == 0 {
        return image::Rgba([0, 0, 0, 0]);
    }
    image::Rgba([
        (totals[0] / counted) as u8,
        (totals[1] / counted) as u8,
        (totals[2] / counted) as u8,
        (totals[3] / counted) as u8,
    ])
}

/// Reads the source a band of rows at a time.
///
/// A tiled source is read square by square, which is why the memory stays flat
/// however large it is. Anything else has to be decoded whole — acceptable
/// only because so few maps are stored that way.
enum Source {
    Tiled(Box<crate::cog::TiledTiff>, u32, u32),
    Whole(Box<RgbaImage>),
}

impl Source {
    fn open(path: &Path) -> Result<Self> {
        if let Ok(tiff) = crate::cog::TiledTiff::open(path) {
            let (width, height) = *tiff
                .level_sizes()
                .first()
                .context("the picture has no layers")?;
            return Ok(Self::Tiled(Box::new(tiff), width, height));
        }
        // Not stored in squares, so there is no way to read part of it — the
        // whole picture has to be decoded at once. The `image` crate refuses
        // large pictures by default, which is right for opening whatever a user
        // drops on the window and wrong here, where the size is known first and
        // the job is deliberate.
        let reader = image::ImageReader::open(path)
            .with_context(|| format!("opening {}", path.display()))?
            .with_guessed_format()
            .with_context(|| format!("reading the start of {}", path.display()))?;
        let (width, height) = reader
            .into_dimensions()
            .with_context(|| format!("reading the size of {}", path.display()))?;

        let needed = u64::from(width) * u64::from(height) * 4;
        if needed > WHOLE_PICTURE_CEILING {
            bail!(
                "{width} × {height} needs {:.1} GB to decode in one piece, and this \
                 picture is not stored in squares — convert it to a tiled TIFF first",
                needed as f64 / 1e9,
            );
        }

        let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
        // The ceiling above is the limit that matters, and it has already been
        // checked. Setting a byte figure here as well only re-imposes a guess
        // about the decoder's working memory, which is not the same as the size
        // of the picture.
        reader.limits(image::Limits::no_limits());
        let image = reader
            .decode()
            .with_context(|| format!("decoding {}", path.display()))?
            .to_rgba8();
        Ok(Self::Whole(Box::new(image)))
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Self::Tiled(_, width, height) => (*width, *height),
            Self::Whole(image) => (image.width(), image.height()),
        }
    }

    /// Rows `top..top + height` of the full-size picture.
    fn band(&mut self, top: u32, height: u32) -> Result<RgbaImage> {
        let (width, full_height) = self.size();
        let height = height.min(full_height.saturating_sub(top));
        match self {
            Self::Tiled(tiff, ..) => tiff
                .decode_region(
                    0,
                    crate::cog::Region {
                        x: 0,
                        y: top,
                        width,
                        height,
                    },
                )
                .map_err(|error| anyhow::anyhow!("{error}")),
            Self::Whole(image) => {
                Ok(image::imageops::crop_imm(image.as_ref(), 0, top, width, height).to_image())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-convert-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// A picture with a recognisable pattern, so a mangled layout shows up as
    /// wrong colours rather than as a plausible blur.
    fn striped(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        })
    }

    #[test]
    fn the_layers_halve_until_they_fit_on_screen() {
        let sizes = layer_sizes(40448, 32256);
        assert_eq!(sizes[0], (40448, 32256));
        assert!(
            sizes.last().unwrap().0.max(sizes.last().unwrap().1) <= SMALLEST,
            "the smallest layer must fit on screen, got {:?}",
            sizes.last(),
        );
        // Each is half the one before.
        for pair in sizes.windows(2) {
            assert_eq!(pair[1].0, pair[0].0 / 2);
            assert_eq!(pair[1].1, pair[0].1 / 2);
        }
    }

    #[test]
    fn progress_runs_forwards_and_reaches_both_ends() {
        let layers = 7;
        let mut last = -1.0_f32;
        for layer in 0..layers {
            let rows = 40 >> layer;
            for row in 0..=rows.max(1) {
                let at = Progress {
                    layer,
                    layers,
                    row,
                    rows: rows.max(1),
                }
                .fraction();
                assert!(at >= last, "went backwards at layer {layer} row {row}");
                assert!((0.0..=1.0).contains(&at));
                last = at;
            }
        }
        assert!(last > 0.999, "the last row must finish the bar, got {last}");
    }

    #[test]
    fn the_full_size_layer_is_most_of_the_work() {
        // Counting layers equally would put a seven-layer map at 1/7 when the
        // big one is done. It is really about three quarters.
        let done_with_the_first = Progress {
            layer: 1,
            layers: 7,
            row: 0,
            rows: 20,
        }
        .fraction();
        assert!(
            (0.70..0.80).contains(&done_with_the_first),
            "finishing the full-size layer should read about 3/4, got {done_with_the_first}",
        );
    }

    #[test]
    fn a_single_layer_picture_is_just_its_rows() {
        let half = Progress {
            layer: 0,
            layers: 1,
            row: 5,
            rows: 10,
        }
        .fraction();
        assert!((half - 0.5).abs() < 1e-6, "got {half}");
    }

    #[test]
    fn a_picture_already_small_gets_one_layer() {
        assert_eq!(layer_sizes(300, 200), vec![(300, 200)]);
    }

    #[test]
    fn a_converted_map_opens_and_has_every_layer() {
        let source = scratch("source.png");
        striped(1100, 900).save(&source).unwrap();
        let target = scratch("converted.tif");
        let _ = std::fs::remove_file(&target);

        let report = convert(&source, &target, |_| true).expect("it converts");
        // 550 is still larger than a screenful, so it halves once more.
        assert_eq!(report.layers, 3);
        assert!(report.bytes > 0);

        let tiff = crate::cog::TiledTiff::open(&target).expect("our reader opens it");
        assert_eq!(
            tiff.level_sizes(),
            vec![(1100, 900), (550, 450), (275, 225)],
        );
    }

    #[test]
    fn the_picture_survives_the_trip() {
        let source = scratch("colours.png");
        striped(600, 600).save(&source).unwrap();
        let target = scratch("colours.tif");
        let _ = std::fs::remove_file(&target);

        convert(&source, &target, |_| true).unwrap();
        let mut tiff = crate::cog::TiledTiff::open(&target).unwrap();
        let out = tiff.decode_level(0).unwrap();
        assert_eq!(out.dimensions(), (600, 600));

        for (x, y) in [(10, 10), (300, 300), (599, 599), (0, 599)] {
            let expected = striped(600, 600).get_pixel(x, y).0;
            let actual = out.get_pixel(x, y).0;
            for channel in 0..3 {
                assert!(
                    actual[channel].abs_diff(expected[channel]) < 12,
                    "pixel {x},{y} channel {channel}: expected {} got {}",
                    expected[channel],
                    actual[channel],
                );
            }
        }
    }

    #[test]
    fn the_smaller_layer_is_a_shrunk_copy_not_a_crop() {
        // A halved layer must show the *whole* picture. Reading the wrong
        // squares would give a corner of it, at the right size, which looks
        // plausible until you put it on a map.
        let source = scratch("shrink.png");
        striped(1024, 1024).save(&source).unwrap();
        let target = scratch("shrink.tif");
        let _ = std::fs::remove_file(&target);

        convert(&source, &target, |_| true).unwrap();
        let mut tiff = crate::cog::TiledTiff::open(&target).unwrap();
        let half = tiff.decode_level(1).unwrap();
        assert_eq!(half.dimensions(), (512, 512));

        // Bottom-right of the half must match bottom-right of the whole.
        let corner = half.get_pixel(500, 500).0;
        let expected = striped(1024, 1024).get_pixel(1000, 1000).0;
        assert!(
            corner[0].abs_diff(expected[0]) < 20 && corner[1].abs_diff(expected[1]) < 20,
            "the far corner reads {corner:?}, expected about {expected:?} — \
             the layer is a crop, not a shrink",
        );
    }

    /// Converts a real map and checks the result with our reader.
    /// `GEO_REF_MAPPER_CONVERT=/path/to.tif cargo test --release real_map -- --ignored --nocapture`
    #[test]
    #[ignore = "converts a real map"]
    fn real_map() {
        let Ok(source) = std::env::var("GEO_REF_MAPPER_CONVERT") else {
            return;
        };
        let source = PathBuf::from(source);
        let target = scratch("real.tif");
        let _ = std::fs::remove_file(&target);

        let started = std::time::Instant::now();
        let report = convert(&source, &target, |_| true).expect("it converts");
        println!(
            "  {} layers, {:.1} MB from {:.1} MB, in {:.1}s",
            report.layers,
            report.bytes as f64 / 1e6,
            report.source_bytes as f64 / 1e6,
            started.elapsed().as_secs_f64(),
        );

        let mut tiff = crate::cog::TiledTiff::open(&target).expect("our reader opens it");
        println!("  layer sizes: {:?}", tiff.level_sizes());

        match crate::sidecar::beside(&source) {
            Some(side) => {
                println!("  sidecar: {:?}, {:?}", side.title, side.date_text);
                let carried = crate::meta::from_path(&target);
                assert!(
                    !carried.xmp.is_empty(),
                    "the source had a sidecar and the copy carried none of it",
                );
                // Both containers, separately. The panel drops an XMP row whose
                // text has already appeared above it, so what is listed twice
                // here is shown once there — and seeing both is the point, since
                // this is checking what went into the file.
                if let Some(exif) = crate::exif::from_path(&target) {
                    for (label, value) in [
                        ("Description", &exif.description),
                        ("Copyright", &exif.copyright),
                        ("Document", &exif.document_name),
                    ] {
                        if let Some(value) = value {
                            let short: String = value.chars().take(70).collect();
                            println!("    SOFTWARE / {label}: {short}");
                        }
                    }
                }
                for (label, value) in &carried.xmp {
                    let short: String = value.chars().take(70).collect();
                    println!("    XMP / {label}: {short}");
                }
            }
            None => println!("  source has no sidecar, so there is nothing to carry"),
        }

        match (
            crate::geotiff::from_path(&source),
            crate::geotiff::from_path(&target),
        ) {
            (Some(before), Some(after)) => {
                println!(
                    "  position kept: EPSG {:?} -> {:?}",
                    before.epsg, after.epsg
                );
                assert_eq!(before.origin, after.origin, "the corner must not move");
                assert_eq!(before.scale, after.scale, "nor the scale");
                assert_eq!(before.epsg, after.epsg);
                assert_eq!(before.citations, after.citations);
            }
            (Some(_), None) => panic!("the source had a position and the copy lost it"),
            (None, _) => println!("  source has no position, so neither has the copy"),
        }
        let last = tiff.level_sizes().len() - 1;
        let small = tiff.decode_level(last).expect("the smallest layer decodes");
        println!(
            "  smallest layer decodes at {}x{}",
            small.width(),
            small.height()
        );
        assert!(small.width() > 0);
    }

    /// A sidecar in the shape the real ones take, cut to the fields that have
    /// to come out the other side.
    const SIDECAR: &str = r#"{
      "version": 1,
      "name": "",
      "date": "c.1873-1881",
      "source": {
        "url": "https://www.nas.gov.sg/record/49598c64",
        "provider": "National Archives of Singapore — Maps & Building Plans",
        "provider_info": "Title: A General Plan of the Town; Type of Map: Survey Map; Scope and Content: Map shows contours, roads & jetties; Conditions Governing Access: Viewing permitted.",
        "accession_number": "SP004422_1",
        "iiif": "https://iiif.micr.io/abc"
      },
      "tags": ["nas", "1881", "SG", "full island"],
      "crop": [[0.05, 0.05], [0.95, 0.05], [0.95, 0.9], [0.05, 0.9]],
      "invert": true
    }"#;

    #[test]
    fn what_the_sidecar_knew_is_inside_the_converted_map() {
        let source = scratch("described.png");
        striped(300, 200).save(&source).unwrap();
        std::fs::write(crate::sidecar::path_for(&source).unwrap(), SIDECAR).unwrap();
        let target = scratch("described.tif");
        let _ = std::fs::remove_file(&target);

        convert(&source, &target, |_| true).expect("it converts");

        // Read back the way the panel reads it, not the way it was written —
        // a writer agreeing with itself proves nothing.
        let meta = crate::meta::from_path(&target);
        let row = |label: &str| {
            meta.xmp
                .iter()
                .find(|(key, _)| key == label)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(row("Title").as_deref(), Some("A General Plan of the Town"));
        assert_eq!(row("Type").as_deref(), Some("Survey Map"));
        assert_eq!(row("Accession").as_deref(), Some("SP004422_1"));
        assert_eq!(
            row("Record").as_deref(),
            Some("https://www.nas.gov.sg/record/49598c64"),
        );
        // The ampersand and the em dash arrive as themselves, not as escapes.
        assert_eq!(
            row("Held by").as_deref(),
            Some("National Archives of Singapore — Maps & Building Plans"),
        );
        assert_eq!(
            row("Description").as_deref(),
            Some("Map shows contours, roads & jetties"),
        );
        // The year the archive wrote, and the sortable form beside it.
        assert_eq!(row("Date").as_deref(), Some("1873/1881"));
        assert_eq!(row("Date as written").as_deref(), Some("c.1873-1881"));
        // Only the tags that said something the other fields do not.
        assert_eq!(row("Keywords").as_deref(), Some("SG · full island"));
    }

    #[test]
    fn the_description_is_in_the_tag_other_programs_read() {
        let source = scratch("tagged.png");
        striped(300, 200).save(&source).unwrap();
        std::fs::write(crate::sidecar::path_for(&source).unwrap(), SIDECAR).unwrap();
        let target = scratch("tagged.tif");
        let _ = std::fs::remove_file(&target);
        convert(&source, &target, |_| true).unwrap();

        // Followed through the directory rather than found by searching the
        // bytes, so a tag pointing at the wrong place would fail here.
        let (_, raw) = crate::geotiff::raw_tag(&target, crate::sidecar::tag::IMAGE_DESCRIPTION)
            .expect("ImageDescription");
        let text = String::from_utf8_lossy(&raw);
        assert!(
            text.starts_with("Map shows contours"),
            "ImageDescription reads {text:?}",
        );

        let (_, raw) = crate::geotiff::raw_tag(&target, crate::sidecar::tag::CROP).expect("crop");
        assert_eq!(raw.len(), 8 * 8, "four corners, two doubles each");
        let corner = f64::from_le_bytes(raw[..8].try_into().unwrap());
        assert!((corner - 0.05).abs() < 1e-12, "top left reads {corner}");

        let (_, raw) = crate::geotiff::raw_tag(&target, crate::sidecar::tag::INVERT).unwrap();
        assert_eq!(u16::from_le_bytes(raw[..2].try_into().unwrap()), 1);
    }

    #[test]
    fn a_map_with_no_sidecar_converts_and_says_nothing_untrue() {
        let source = scratch("bare.png");
        striped(300, 200).save(&source).unwrap();
        let _ = std::fs::remove_file(crate::sidecar::path_for(&source).unwrap());
        let target = scratch("bare.tif");
        let _ = std::fs::remove_file(&target);

        convert(&source, &target, |_| true).expect("it converts anyway");

        // No invented title, and no crop claimed on a map nobody has cropped.
        assert!(crate::meta::from_path(&target).xmp.is_empty());
        let (_, raw) = crate::geotiff::raw_tag(&target, crate::sidecar::tag::CROP).unwrap();
        let corners: Vec<f64> = raw
            .chunks_exact(8)
            .map(|eight| f64::from_le_bytes(eight.try_into().unwrap()))
            .collect();
        assert_eq!(corners, vec![0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0]);
    }

    #[test]
    fn the_original_is_never_the_destination() {
        let source = scratch("same.png");
        striped(64, 64).save(&source).unwrap();
        let error = convert(&source, &source, |_| true).unwrap_err();
        assert!(error.to_string().contains("overwrite the original"));
        assert!(source.exists(), "and it is still there");
    }

    #[test]
    fn an_existing_file_is_replaced_and_said_to_have_been() {
        // There is no telling a finished conversion from an abandoned one, so
        // whatever is already there is treated as untrustworthy and replaced.
        let source = scratch("src2.png");
        striped(64, 64).save(&source).unwrap();
        let target = scratch("taken.tif");
        std::fs::write(&target, b"a half-written map from last time").unwrap();

        let report = convert(&source, &target, |_| true).expect("it replaces");
        assert!(report.replaced, "and says so, so a toast can mention it");
        assert!(
            crate::cog::TiledTiff::open(&target).is_ok(),
            "what is there now must be the new map",
        );
    }

    #[test]
    fn a_conversion_that_fails_leaves_the_previous_file_untouched() {
        // The reason for writing beside the target and renaming at the end: a
        // plain overwrite would destroy the old file first and spend the whole
        // conversion with nothing usable at that path.
        // A directory of its own: the leftovers check below reads the whole
        // folder, and the other conversion tests run at the same time — their
        // half-written files would be counted as this one's.
        let mine = scratch("guarded").join("run");
        std::fs::create_dir_all(&mine).unwrap();
        let source = mine.join("stop2.png");
        striped(1024, 1024).save(&source).unwrap();
        let target = mine.join("guarded.tif");
        std::fs::write(&target, b"the previous map").unwrap();

        let error = convert(&source, &target, |progress| progress.row < 2).unwrap_err();
        assert!(error.to_string().contains("stopped"));
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"the previous map",
            "a stopped conversion must not have eaten what was there",
        );
        // And nothing half-written is left lying beside it.
        let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    fn stopping_leaves_nothing_behind() {
        let source = scratch("stop.png");
        striped(1024, 1024).save(&source).unwrap();
        let target = scratch("stopped.tif");
        let _ = std::fs::remove_file(&target);

        let error = convert(&source, &target, |progress| progress.row < 2).unwrap_err();
        assert!(error.to_string().contains("stopped"));
        assert!(
            !target.exists(),
            "an abandoned job must not leave half a file",
        );
    }
}
