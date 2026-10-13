//! Decoders for the formats the `image` crate does not cover.
//!
//! Each is a pure-Rust crate, so the binary still builds and runs anywhere —
//! the same rule the rest of this project follows. They share one entry point,
//! [`decode`], which the loader, the thumbnailer and the scanner all go through
//! so a format never works in one place and not another.
//!
//! Detection is by content, not extension: a `.psd` that is really a JPEG is
//! common enough, and the file itself is the only honest authority.

use anyhow::{Context as _, Result, bail};
use image::{DynamicImage, RgbaImage};

/// Formats handled here rather than by the `image` crate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Extra {
    Photoshop,
    JpegXl,
    Svg,
    Gimp,
    /// HEIF/HEIC holding HEVC images, which is what every iPhone writes.
    Heic,
}

impl Extra {
    pub fn label(self) -> &'static str {
        match self {
            Self::Photoshop => "Photoshop",
            Self::JpegXl => "JPEG XL",
            Self::Svg => "SVG",
            Self::Gimp => "GIMP XCF",
            Self::Heic => "HEIC",
        }
    }
}

/// True for a gzip stream, which is how `.svgz` is stored.
fn is_gzip(bytes: &[u8]) -> bool {
    matches!(bytes, [0x1F, 0x8B, ..])
}

/// Inflates a `.svgz`. Bounded, so a decompression bomb cannot exhaust memory.
fn gunzip(bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;

    const MAX_SVG: u64 = 64 * 1024 * 1024;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(MAX_SVG)
        .read_to_end(&mut out)
        .ok()?;
    (!out.is_empty()).then_some(out)
}

/// Identifies a file we can decode here, from its leading bytes.
pub fn sniff(bytes: &[u8]) -> Option<Extra> {
    // A gzipped SVG has to be inflated before anything can be recognised.
    if is_gzip(bytes) {
        return gunzip(bytes).and_then(|inflated| sniff(&inflated));
    }
    match bytes {
        [b'8', b'B', b'P', b'S', ..] => Some(Extra::Photoshop),
        // JPEG XL: the bare codestream signature, or the ISOBMFF wrapper.
        [0xFF, 0x0A, ..] => Some(Extra::JpegXl),
        [0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', ..] => Some(Extra::JpegXl),
        [b'g', b'i', b'm', b'p', b' ', b'x', b'c', b'f', ..] => Some(Extra::Gimp),
        // ISOBMFF. Only claim it when the container really holds HEVC stills;
        // an AVIF or a stray video would land here otherwise.
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => crate::heif::parse(bytes)
            .filter(|heif| heif.is_hevc)
            .map(|_| Extra::Heic),
        _ => {
            // SVG is text, and may open with a declaration, a comment or
            // whitespace before the root element.
            let head = bytes.get(..512.min(bytes.len()))?;
            let text = String::from_utf8_lossy(head);
            let trimmed = text.trim_start();
            (trimmed.starts_with("<?xml")
                || trimmed.starts_with("<svg")
                || trimmed.starts_with("<!--"))
            .then_some(())
            .filter(|_| text.contains("<svg"))
            .map(|_| Extra::Svg)
        }
    }
}

/// Refuse anything past this before allocating for it. A 670 MP layer needs
/// 2.7 GB as RGBA, and these formats state their size in a header that a corrupt
/// or hostile file can put anything into.
const MAX_PIXELS: u64 = 1_000_000_000;

/// Runs a third-party decoder so that a panic inside it becomes an error.
///
/// Not paranoia: the `psd` crate panics with an out-of-bounds slice index on a
/// real Photoshop file in this library. A viewer must not die because one file
/// in a folder is unusual, and these parsers all take untrusted input.
fn guarded<T>(what: &str, decode: impl FnOnce() -> Result<T>) -> Result<T> {
    // The default hook would print a backtrace for every awkward file.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(decode));
    std::panic::set_hook(previous);

    match outcome {
        Ok(result) => result,
        Err(_) => bail!("the {what} decoder crashed on this file"),
    }
}

/// Decodes one of the extra formats. `target_edge` only affects SVG, which has
/// no inherent pixel size and so is rasterised to fit.
pub fn decode(bytes: &[u8], kind: Extra, target_edge: u32) -> Result<DynamicImage> {
    if is_gzip(bytes) {
        let inflated = gunzip(bytes).context("could not inflate a compressed SVG")?;
        return decode(&inflated, kind, target_edge);
    }
    guarded(kind.label(), || match kind {
        Extra::Photoshop => decode_psd(bytes),
        Extra::JpegXl => decode_jxl(bytes),
        Extra::Svg => decode_svg(bytes, target_edge),
        Extra::Gimp => decode_xcf(bytes),
        Extra::Heic => decode_heic(bytes, target_edge),
    })
}

/// Rejects a declared size that would exhaust memory before anything allocates.
fn check_size(width: u32, height: u32, what: &str) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("{what} reported an empty image");
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_PIXELS {
        bail!(
            "{what} is {width} x {height} — {:.0} megapixels, past the {:.0} MP ceiling",
            pixels as f64 / 1e6,
            MAX_PIXELS as f64 / 1e6,
        );
    }
    Ok(())
}

/// The flattened composite Photoshop stores at the end of the file, which is
/// what every other viewer shows.
///
/// Read directly rather than through the `psd` crate, which panics with an
/// out-of-bounds slice on ordinary 8-bit RGB files — four of the five in this
/// library. The composite itself is simple: a header, three sections we skip by
/// their stated lengths, then planar channels that are either raw or PackBits.
fn decode_psd(bytes: &[u8]) -> Result<DynamicImage> {
    let header = bytes.get(..26).context("truncated Photoshop header")?;
    if &header[..4] != b"8BPS" {
        bail!("not a Photoshop file");
    }
    let version = u16::from_be_bytes([header[4], header[5]]);
    // Version 2 is PSB, whose only difference here is wider scanline counts.
    if version != 1 && version != 2 {
        bail!("unsupported Photoshop version {version}");
    }
    let channels = u16::from_be_bytes([header[12], header[13]]) as usize;
    let height = u32::from_be_bytes(header[14..18].try_into()?);
    let width = u32::from_be_bytes(header[18..22].try_into()?);
    let depth = u16::from_be_bytes([header[22], header[23]]);
    let mode = u16::from_be_bytes([header[24], header[25]]);
    check_size(width, height, "Photoshop")?;
    if channels == 0 {
        bail!("Photoshop file declares no channels");
    }

    // Skip colour mode data, image resources, and the layer section, each of
    // which states its own length.
    let mut at = 26;
    for _ in 0..3 {
        let length = u32::from_be_bytes(
            bytes
                .get(at..at + 4)
                .context("truncated Photoshop section")?
                .try_into()?,
        ) as usize;
        at = at
            .checked_add(4)
            .and_then(|start| start.checked_add(length))
            .context("Photoshop section length overflows the file")?;
    }

    let compression = u16::from_be_bytes(
        bytes
            .get(at..at + 2)
            .context("truncated Photoshop image data")?
            .try_into()?,
    );
    at += 2;

    let rows = height as usize;
    let per_sample = if depth == 16 { 2 } else { 1 };
    if depth != 8 && depth != 16 {
        bail!("unsupported Photoshop bit depth {depth}");
    }
    let row_bytes = width as usize * per_sample;

    // Channels are planar: every row of channel 0, then channel 1, and so on.
    let planes: Vec<Vec<u8>> = match compression {
        0 => {
            let mut planes = Vec::with_capacity(channels);
            for plane in 0..channels {
                let start = at + plane * row_bytes * rows;
                let end = start + row_bytes * rows;
                planes.push(
                    bytes
                        .get(start..end)
                        .context("truncated Photoshop channel")?
                        .to_vec(),
                );
            }
            planes
        }
        1 => {
            // PackBits, preceded by a table of per-row compressed lengths.
            let count_width = if version == 2 { 4 } else { 2 };
            let counts_len = rows * channels * count_width;
            let counts = bytes
                .get(at..at + counts_len)
                .context("truncated Photoshop scanline table")?;
            at += counts_len;

            let mut planes = Vec::with_capacity(channels);
            let mut cursor = at;
            for plane in 0..channels {
                let mut out = Vec::with_capacity(row_bytes * rows);
                for row in 0..rows {
                    let index = plane * rows + row;
                    let length = if count_width == 2 {
                        u16::from_be_bytes(
                            counts[index * 2..index * 2 + 2]
                                .try_into()
                                .unwrap_or([0, 0]),
                        ) as usize
                    } else {
                        u32::from_be_bytes(
                            counts[index * 4..index * 4 + 4]
                                .try_into()
                                .unwrap_or([0, 0, 0, 0]),
                        ) as usize
                    };
                    let packed = bytes
                        .get(cursor..cursor + length)
                        .context("truncated Photoshop scanline")?;
                    cursor += length;
                    unpack_bits(packed, row_bytes, &mut out);
                }
                planes.push(out);
            }
            planes
        }
        other => bail!("unsupported Photoshop compression {other} (ZIP is not handled)"),
    };

    // Take the top byte of 16-bit samples; the texture is 8-bit regardless.
    let sample =
        |plane: &[u8], index: usize| -> u8 { plane.get(index * per_sample).copied().unwrap_or(0) };

    let pixels = width as usize * height as usize;
    let mut rgba = Vec::with_capacity(pixels * 4);
    for index in 0..pixels {
        let (r, g, b, a) = match (mode, channels) {
            // Greyscale, with or without alpha.
            (1, 1) => {
                let value = sample(&planes[0], index);
                (value, value, value, 255)
            }
            (1, _) => {
                let value = sample(&planes[0], index);
                (value, value, value, sample(&planes[1], index))
            }
            // CMYK, which Photoshop stores inverted.
            (4, _) => {
                let c = 255 - sample(&planes[0], index) as u32;
                let m = 255 - sample(&planes[1], index) as u32;
                let y = 255 - sample(&planes[2], index) as u32;
                let k = 255 - sample(&planes[3], index) as u32;
                (
                    (255 - (c + k).min(255)) as u8,
                    (255 - (m + k).min(255)) as u8,
                    (255 - (y + k).min(255)) as u8,
                    255,
                )
            }
            // RGB, with alpha when a fourth channel is present.
            (_, 1) => {
                let value = sample(&planes[0], index);
                (value, value, value, 255)
            }
            (_, 3) => (
                sample(&planes[0], index),
                sample(&planes[1], index),
                sample(&planes[2], index),
                255,
            ),
            _ => (
                sample(&planes[0], index),
                sample(&planes[1], index),
                sample(&planes[2], index),
                sample(&planes[3], index),
            ),
        };
        rgba.extend_from_slice(&[r, g, b, a]);
    }

    RgbaImage::from_raw(width, height, rgba)
        .map(DynamicImage::ImageRgba8)
        .context("Photoshop composite did not match its declared size")
}

/// Decodes a HEIC, honouring the container's own rotation.
///
/// `irot`/`imir` rather than EXIF: HEIF states the transform in the container,
/// and that is what viewers are expected to follow.
fn decode_heic(bytes: &[u8], target_edge: u32) -> Result<DynamicImage> {
    let heif = crate::heif::parse(bytes).context("could not read the HEIF structure")?;
    check_size(heif.width, heif.height, "HEIC")?;

    let idat = heif
        .idat
        .and_then(|(start, end)| bytes.get(start as usize..end as usize));

    // When only a small image is wanted and the file carries a thumbnail big
    // enough, decoding one small item beats decoding forty-eight tiles.
    let canvas = heif
        .thumbnail
        .filter(|_| target_edge <= THUMBNAIL_CUTOFF)
        .and_then(|_| crate::hevc::decode_thumbnail(bytes, &heif, idat).ok())
        .filter(|thumb| thumb.width().max(thumb.height()) >= target_edge)
        .map_or_else(|| crate::hevc::decode_primary(bytes, &heif, idat), Ok)?;

    let image = DynamicImage::ImageRgba8(canvas);
    Ok(heif.transform.as_exif().apply(image))
}

/// Above this the embedded thumbnail is too small to be worth using — phone
/// thumbnails are around 320 pixels on the long edge.
const THUMBNAIL_CUTOFF: u32 = 512;

/// PackBits: a run-length scheme where a leading byte says whether the next
/// bytes are literal or a repeat. Stops at `expected` so a corrupt count cannot
/// grow the row without bound.
fn unpack_bits(packed: &[u8], expected: usize, out: &mut Vec<u8>) {
    let target = out.len() + expected;
    let mut at = 0;
    while at < packed.len() && out.len() < target {
        let control = packed[at] as i8;
        at += 1;
        if control >= 0 {
            // literal run of control + 1 bytes
            let count = control as usize + 1;
            let end = (at + count).min(packed.len());
            let room = target - out.len();
            let slice = &packed[at..end];
            out.extend_from_slice(&slice[..room.min(slice.len())]);
            at = end;
        } else if control != -128 {
            // repeat the next byte 1 - control times
            let count = (1 - control as i32) as usize;
            let Some(byte) = packed.get(at) else { break };
            at += 1;
            let room = target - out.len();
            out.extend(std::iter::repeat_n(*byte, count.min(room)));
        }
        // -128 is a no-op by the specification.
    }
    // A short row must still leave the plane the right length.
    out.resize(target, 0);
}

fn decode_jxl(bytes: &[u8]) -> Result<DynamicImage> {
    let image = jxl_oxide::JxlImage::read_with_defaults(std::io::Cursor::new(bytes))
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let render = image
        .render_frame(0)
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    // `stream` applies the file's orientation and gives colour plus alpha.
    let mut stream = render.stream();
    let (width, height) = (stream.width(), stream.height());
    let channels = stream.channels() as usize;
    check_size(width, height, "JPEG XL")?;

    let mut samples = vec![0_u8; width as usize * height as usize * channels];
    stream.write_to_buffer(&mut samples);

    // Normalise whatever came out to RGBA8, since callers should not have to
    // care whether a file was grey, RGB or RGBA.
    let pixels = width as usize * height as usize;
    let mut rgba = Vec::with_capacity(pixels * 4);
    for index in 0..pixels {
        let at = index * channels;
        let (r, g, b, a) = match channels {
            1 => (samples[at], samples[at], samples[at], 255),
            2 => (samples[at], samples[at], samples[at], samples[at + 1]),
            3 => (samples[at], samples[at + 1], samples[at + 2], 255),
            _ => (
                samples[at],
                samples[at + 1],
                samples[at + 2],
                samples[at + 3],
            ),
        };
        rgba.extend_from_slice(&[r, g, b, a]);
    }

    RgbaImage::from_raw(width, height, rgba)
        .map(DynamicImage::ImageRgba8)
        .context("JPEG XL frame did not match its declared size")
}

/// Rasterises at a size that suits the canvas rather than the nominal one: an
/// SVG has no true resolution, and rendering it at 100 x 100 to then magnify
/// would throw away the one advantage vector art has.
fn decode_svg(bytes: &[u8], target_edge: u32) -> Result<DynamicImage> {
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(bytes, &options)
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    let size = tree.size();
    if size.width() <= 0.0 || size.height() <= 0.0 {
        bail!("SVG has no drawable area");
    }

    // Scale so the longest edge lands near the target, within sane bounds.
    let longest = size.width().max(size.height());
    let target = target_edge.clamp(16, 8192) as f32;
    let scale = (target / longest).clamp(0.05, 64.0);
    let width = ((size.width() * scale).round() as u32).max(1);
    let height = ((size.height() * scale).round() as u32).max(1);

    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .context("could not allocate a canvas for the SVG")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    RgbaImage::from_raw(width, height, pixmap.take())
        .map(DynamicImage::ImageRgba8)
        .context("SVG raster did not match its canvas")
}

/// Composites the layers bottom-up. XCF has no stored flattened copy, so this
/// is the only way to get a picture — but blend modes and masks are ignored, so
/// a heavily composited file will not match GIMP exactly.
fn decode_xcf(bytes: &[u8]) -> Result<DynamicImage> {
    let xcf = xcf_rs::data::xcf::Xcf::load(std::io::Cursor::new(bytes))
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let (width, height) = xcf.dimensions();
    check_size(width, height, "XCF")?;

    let stride = width as usize * 4;
    let mut canvas = vec![0_u8; stride * height as usize];

    // Layer zero is the top of the stack, so paint from the back forwards.
    for layer in xcf.layers.iter().rev() {
        let (layer_width, layer_height) = layer.dimensions();
        if layer_width == 0 || layer_height == 0 {
            continue;
        }
        let source = layer.raw_rgba_buffer();
        let rows = layer_height.min(height) as usize;
        let columns = layer_width.min(width) as usize;

        // A row at a time across threads. Rows are disjoint, and the previous
        // per-pixel version spent 9.9 s on a 670 MP file.
        std::thread::scope(|scope| {
            for (row, line) in canvas.chunks_mut(stride).take(rows).enumerate() {
                let source = &source;
                scope.spawn(move || {
                    let start = row * layer_width as usize;
                    for column in 0..columns {
                        let Some(pixel) = source.get(start + column) else {
                            break;
                        };
                        let [r, g, b, a] = pixel.0;
                        if a == 0 {
                            continue; // Fully transparent: nothing to blend.
                        }
                        let at = column * 4;
                        if a == 255 {
                            line[at..at + 4].copy_from_slice(&[r, g, b, 255]);
                            continue;
                        }
                        // Straight alpha-over; enough for the common file, and
                        // blend modes are documented as ignored.
                        let alpha = a as u32;
                        for (channel, top) in [r, g, b].into_iter().enumerate() {
                            let bottom = line[at + channel] as u32;
                            line[at + channel] =
                                ((top as u32 * alpha + bottom * (255 - alpha)) / 255) as u8;
                        }
                        line[at + 3] = line[at + 3].max(a);
                    }
                });
            }
        });
    }

    RgbaImage::from_raw(width, height, canvas)
        .map(DynamicImage::ImageRgba8)
        .context("XCF canvas did not match its declared size")
}

/// Dimensions without a full decode, where the format makes that cheap.
pub fn dimensions(bytes: &[u8], kind: Extra) -> Option<(u32, u32)> {
    match kind {
        // The PSD header carries the size in its first 26 bytes.
        Extra::Photoshop => {
            let height = u32::from_be_bytes(bytes.get(14..18)?.try_into().ok()?);
            let width = u32::from_be_bytes(bytes.get(18..22)?.try_into().ok()?);
            (width > 0 && height > 0).then_some((width, height))
        }
        Extra::JpegXl => {
            let image =
                jxl_oxide::JxlImage::read_with_defaults(std::io::Cursor::new(bytes)).ok()?;
            Some((image.width(), image.height()))
        }
        Extra::Svg => {
            let tree =
                resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default()).ok()?;
            let size = tree.size();
            Some((size.width().ceil() as u32, size.height().ceil() as u32))
        }
        Extra::Gimp => {
            let xcf = xcf_rs::data::xcf::Xcf::load(std::io::Cursor::new(bytes)).ok()?;
            Some(xcf.dimensions())
        }
        // From the container, which states the displayed size directly — no
        // HEVC decoding needed to answer this.
        Extra::Heic => {
            let heif = crate::heif::parse(bytes)?;
            let (width, height) = heif.display_size();
            (width > 0 && height > 0).then_some((width, height))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffing_recognises_each_format_by_its_signature() {
        assert_eq!(sniff(b"8BPS\x00\x01rest of a psd"), Some(Extra::Photoshop));
        assert_eq!(sniff(&[0xFF, 0x0A, 0x00, 0x00]), Some(Extra::JpegXl));
        assert_eq!(
            sniff(b"\x00\x00\x00\x0CJXL \x0d\x0a\x87\x0a"),
            Some(Extra::JpegXl)
        );
        assert_eq!(sniff(b"gimp xcf v011\0"), Some(Extra::Gimp));
        assert_eq!(
            sniff(br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"></svg>"#),
            Some(Extra::Svg)
        );
        assert_eq!(sniff(b"<svg viewBox=\"0 0 1 1\"></svg>"), Some(Extra::Svg));
    }

    #[test]
    fn sniffing_does_not_claim_formats_it_cannot_read() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), None);
        assert_eq!(sniff(b""), None);
        // XML that is not SVG must not be claimed.
        assert_eq!(sniff(br#"<?xml version="1.0"?><rss></rss>"#), None);
    }

    #[test]
    fn svg_rasterises_to_the_requested_size_and_keeps_its_aspect() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100">
            <rect width="200" height="100" fill="#3060c0"/></svg>"##;
        let image = decode(svg, Extra::Svg, 400).expect("should rasterise");
        assert_eq!(image.width(), 400, "longest edge should hit the target");
        assert_eq!(image.height(), 200, "aspect ratio must survive");

        // And it must actually have painted something.
        let pixel = image.to_rgba8();
        assert_eq!(pixel.get_pixel(200, 100).0[2], 0xc0);
    }

    #[test]
    fn a_gzipped_svg_is_inflated_before_anything_looks_at_it() {
        use std::io::Write;

        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="60"></svg>"#;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(svg).unwrap();
        let gzipped = encoder.finish().unwrap();

        assert_eq!(sniff(&gzipped), Some(Extra::Svg), "a .svgz is still an SVG");
        let image = decode(&gzipped, Extra::Svg, 240).expect("should rasterise");
        assert_eq!(image.width(), 240);
        assert_eq!(image.height(), 120);
    }

    #[test]
    fn a_gzip_stream_that_is_not_an_svg_is_not_claimed() {
        use std::io::Write;

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"just some compressed text").unwrap();
        assert_eq!(sniff(&encoder.finish().unwrap()), None);
    }

    #[test]
    fn svg_size_is_reported_without_rendering() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="640" height="480"></svg>"#;
        assert_eq!(dimensions(svg, Extra::Svg), Some((640, 480)));
    }

    #[test]
    fn a_tiny_target_edge_still_produces_a_usable_raster() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="4000" height="20">
            <rect width="4000" height="20" fill="red"/></svg>"#;
        // An extreme aspect ratio must not collapse the short edge to zero.
        let image = decode(svg, Extra::Svg, 64).expect("should rasterise");
        assert!(image.width() >= 1 && image.height() >= 1);
    }

    #[test]
    fn malformed_input_is_an_error_rather_than_a_panic() {
        assert!(decode(b"8BPS not really", Extra::Photoshop, 512).is_err());
        assert!(decode(&[0xFF, 0x0A, 0x01], Extra::JpegXl, 512).is_err());
        assert!(decode(b"<svg", Extra::Svg, 512).is_err());
        assert!(decode(b"gimp xcf broken", Extra::Gimp, 512).is_err());
        assert!(dimensions(b"8BPS", Extra::Photoshop).is_none());
    }

    /// Real files, since a synthetic PSD proves only that my fixture matches my
    /// parser. Reports rather than asserts on decode success, because a corrupt
    /// file in a real folder is not this crate's fault — but any panic is.
    ///
    /// `GEO_REF_MAPPER_CODEC_DIR=/photos cargo test --release bench_codecs -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_CODEC_DIR and real files"]
    fn bench_codecs_against_real_files() {
        use std::time::Instant;

        let Ok(dir) = std::env::var("GEO_REF_MAPPER_CODEC_DIR") else {
            eprintln!("set GEO_REF_MAPPER_CODEC_DIR");
            return;
        };
        let limit: usize = std::env::var("GEO_REF_MAPPER_CODEC_N")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(40);

        let files: Vec<std::path::PathBuf> = walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_map(Result::ok)
            .map(|entry| entry.path().to_path_buf())
            .filter(|path| {
                path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    ["psd", "psb", "jxl", "svg", "svgz", "xcf"]
                        .contains(&e.to_ascii_lowercase().as_str())
                })
            })
            .take(limit)
            .collect();
        assert!(!files.is_empty(), "no candidate files under {dir}");

        let (mut ok, mut failed, mut unclaimed) = (0, 0, 0);
        for path in &files {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let Some(kind) = sniff(&bytes) else {
                unclaimed += 1;
                println!("  UNCLAIMED  {}", path.display());
                continue;
            };
            let started = Instant::now();
            match decode(&bytes, kind, 2048) {
                Ok(image) => {
                    ok += 1;
                    println!(
                        "  {:>10}  {:>5} x {:<5} {:>7.1} ms  {}",
                        kind.label(),
                        image.width(),
                        image.height(),
                        started.elapsed().as_secs_f64() * 1000.0,
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .chars()
                            .take(40)
                            .collect::<String>(),
                    );
                    assert!(image.width() > 0 && image.height() > 0);
                }
                Err(error) => {
                    failed += 1;
                    println!(
                        "  FAILED {:>8}  {}  ({error})",
                        kind.label(),
                        path.display()
                    );
                }
            }
        }
        println!("\n  {ok} decoded, {failed} failed, {unclaimed} not recognised\n");
    }

    #[test]
    fn psd_dimensions_come_from_the_header() {
        // 8BPS, version, 6 reserved, channels, height, width.
        let mut header = b"8BPS\x00\x01\0\0\0\0\0\0\x00\x03".to_vec();
        header.extend_from_slice(&768_u32.to_be_bytes());
        header.extend_from_slice(&1024_u32.to_be_bytes());
        assert_eq!(dimensions(&header, Extra::Photoshop), Some((1024, 768)));
    }
}
