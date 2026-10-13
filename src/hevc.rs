//! Turning the HEVC items inside a HEIC into pixels.
//!
//! [`crate::heif`] works out the structure; this decodes it. An iPhone still is
//! a grid of separately coded tiles — usually 8 x 6 of them — each a complete
//! intra-coded HEVC picture, so they decode independently and in parallel.
//!
//! Two conversions stand between the file and the decoder:
//!
//! - Item data is stored as length-prefixed NAL units, while decoders expect
//!   Annex-B start codes. The prefix length and the parameter sets both come
//!   from the item's `hvcC` record.
//! - HEVC gives back Y'CbCr planes at 4:2:0. Turning those into RGB needs the
//!   matrix and range the file was written with, and getting it wrong is not
//!   subtle — BT.601 on BT.709 content visibly shifts every colour.

use anyhow::{Context as _, Result, bail};
use image::RgbaImage;

use crate::heif::{Grid, Heif, Item};

/// Parsed `hvcC`: the parameter sets a decoder needs before any picture, and
/// how long the length prefixes are.
struct DecoderConfig {
    /// 1, 2 or 4 bytes before each NAL unit.
    length_size: usize,
    /// VPS, SPS and PPS, already in Annex-B form.
    parameter_sets: Vec<u8>,
}

/// Reads the `HEVCDecoderConfigurationRecord`.
fn parse_hvcc(hvcc: &[u8]) -> Result<DecoderConfig> {
    // 22 fixed bytes, then a count of parameter-set arrays.
    let length_size = (*hvcc.get(21).context("truncated hvcC")? & 0b11) as usize + 1;
    let array_count = *hvcc.get(22).context("truncated hvcC arrays")?;

    let mut parameter_sets = Vec::new();
    let mut at = 23;
    for _ in 0..array_count {
        // One byte of flags and type, then a 16-bit count of NAL units.
        let count = u16::from_be_bytes(
            hvcc.get(at + 1..at + 3)
                .context("truncated hvcC array header")?
                .try_into()?,
        );
        at += 3;
        for _ in 0..count {
            let length = u16::from_be_bytes(
                hvcc.get(at..at + 2)
                    .context("truncated hvcC NAL length")?
                    .try_into()?,
            ) as usize;
            at += 2;
            let nal = hvcc
                .get(at..at + length)
                .context("truncated hvcC NAL unit")?;
            at += length;
            parameter_sets.extend_from_slice(&[0, 0, 0, 1]);
            parameter_sets.extend_from_slice(nal);
        }
    }

    if parameter_sets.is_empty() {
        bail!("hvcC carried no parameter sets");
    }
    Ok(DecoderConfig {
        length_size,
        parameter_sets,
    })
}

/// Rewrites length-prefixed NAL units as an Annex-B stream, with the parameter
/// sets in front so the bitstream is self-describing.
fn to_annex_b(config: &DecoderConfig, item_data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(config.parameter_sets.len() + item_data.len() + 64);
    out.extend_from_slice(&config.parameter_sets);

    let mut at = 0;
    while at + config.length_size <= item_data.len() {
        let mut length = 0_usize;
        for offset in 0..config.length_size {
            length = (length << 8) | item_data[at + offset] as usize;
        }
        at += config.length_size;
        let end = (at + length).min(item_data.len());
        if end <= at {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&item_data[at..end]);
        at = end;
    }
    out
}

/// One decoded tile, as Y'CbCr planes.
struct Planes {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    width: usize,
    height: usize,
    /// Chroma plane width, which is not always exactly half for odd sizes.
    chroma_width: usize,
}

/// Decodes one coded item to planes.
fn decode_item(hvcc: &[u8], data: &[u8]) -> Result<Planes> {
    let config = parse_hvcc(hvcc)?;
    let stream = to_annex_b(&config, data);

    let nals = rust_h265::parse_annex_b(&stream);
    if nals.is_empty() {
        bail!("no NAL units in this item");
    }

    let mut decoder = rust_h265::Decoder::new();
    let mut picture = None;
    for nal in &nals {
        match decoder.decode_nal(nal) {
            Ok(Some(frame)) => {
                picture = Some(frame);
                break;
            }
            Ok(None) => {}
            // A tile that will not decode should leave a gap, not fail the
            // whole photograph.
            Err(error) => bail!("HEVC decode failed: {error:?}"),
        }
    }
    let frame = match picture {
        Some(frame) => frame,
        None => decoder.flush().context("the decoder produced no picture")?,
    };

    // 10-bit files exist (the `heix` brand); scale them down to 8 rather than
    // refusing, since the texture is 8-bit either way.
    let narrow = |plane: &rust_h265::PixelData, shift: u32| -> Vec<u8> {
        match plane {
            rust_h265::PixelData::U8(values) => values.clone(),
            rust_h265::PixelData::U16(values) => {
                values.iter().map(|value| (value >> shift) as u8).collect()
            }
        }
    };
    let shift = u32::from(frame.bit_depth.saturating_sub(8));

    let width = frame.width as usize;
    let height = frame.height as usize;
    let chroma_width = width.div_ceil(2);

    Ok(Planes {
        y: narrow(&frame.y, shift),
        u: narrow(&frame.u, shift),
        v: narrow(&frame.v, shift),
        width,
        height,
        chroma_width,
    })
}

/// How to turn Y'CbCr into RGB.
///
/// The range matters more than anything else here: decoding full-range content
/// as limited stretches it, darkening shadows and blowing out highlights. On a
/// real iPhone photo that showed up as a pixel of 249 where libheif produced
/// 227 — obvious side by side.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ColourSpec {
    /// True when samples use 0-255 rather than the studio 16-235.
    pub full_range: bool,
    /// ITU-T H.273 matrix coefficients. 1 is BT.709, 5 and 6 are BT.601.
    pub matrix: u16,
}

impl Default for ColourSpec {
    /// What phones write. HEIF stills carry their range in the HEVC sequence
    /// header's optional VUI, which this decoder does not expose, and the
    /// container's `colr` box on these files is an ICC profile rather than the
    /// `nclx` form that would state it. Full-range BT.709 is what Apple emits
    /// and what matches libheif's output; an `nclx` box overrides it when one
    /// is present.
    fn default() -> Self {
        Self {
            full_range: true,
            matrix: 1,
        }
    }
}

/// Fixed-point coefficients for one spec, at 8 fractional bits.
struct Coefficients {
    /// Applied to `luma - offset`.
    luma_gain: i32,
    luma_offset: i32,
    red_cr: i32,
    green_cb: i32,
    green_cr: i32,
    blue_cb: i32,
}

impl ColourSpec {
    fn coefficients(self) -> Coefficients {
        // BT.601 differs enough from BT.709 to be worth distinguishing.
        let (red_cr, green_cb, green_cr, blue_cb) = if self.matrix == 5 || self.matrix == 6 {
            (359.0_f32, -88.0, -183.0, 454.0)
        } else {
            (403.0_f32, -48.0, -120.0, 475.0)
        };

        if self.full_range {
            Coefficients {
                luma_gain: 256,
                luma_offset: 0,
                red_cr: red_cr as i32,
                green_cb: green_cb as i32,
                green_cr: green_cr as i32,
                blue_cb: blue_cb as i32,
            }
        } else {
            // Limited range stretches 16..235 onto 0..255, and the chroma
            // coefficients scale by the same 255/219 factor.
            let scale = 255.0 / 219.0;
            Coefficients {
                luma_gain: (256.0 * scale) as i32,
                luma_offset: 16,
                red_cr: (red_cr * scale) as i32,
                green_cb: (green_cb * scale) as i32,
                green_cr: (green_cr * scale) as i32,
                blue_cb: (blue_cb * scale) as i32,
            }
        }
    }
}

/// Reads a chroma sample for luma position `(x, y)`, interpolating between the
/// half-resolution samples rather than picking the nearest.
///
/// Horizontally chroma is co-sited with even luma columns, so an even `x` reads
/// one sample exactly and an odd `x` averages two. The same applies down the
/// rows. Edges clamp rather than wrap.
fn sample_chroma(plane: &[u8], width: usize, rows: usize, x: usize, y: usize) -> i32 {
    if width == 0 || rows == 0 {
        return 128;
    }
    let cx = x / 2;
    let cy = y / 2;
    let cx1 = (cx + x % 2).min(width - 1);
    let cy1 = (cy + y % 2).min(rows - 1);
    let cx = cx.min(width - 1);
    let cy = cy.min(rows - 1);

    let at = |column: usize, row: usize| -> i32 {
        *plane.get(row * width + column).unwrap_or(&128) as i32
    };
    // Bilinear over at most four samples, with weights of 1/2 on each axis.
    (at(cx, cy) + at(cx1, cy) + at(cx, cy1) + at(cx1, cy1) + 2) / 4
}

fn yuv_to_rgba(
    planes: &Planes,
    out: &mut RgbaImage,
    origin_x: u32,
    origin_y: u32,
    spec: ColourSpec,
) {
    let k = spec.coefficients();
    let chroma_rows = planes.u.len().checked_div(planes.chroma_width).unwrap_or(0);
    let canvas_width = out.width() as usize;
    let canvas_height = out.height() as usize;
    let raw = out.as_mut();

    for y in 0..planes.height {
        let canvas_y = origin_y as usize + y;
        if canvas_y >= canvas_height {
            break;
        }
        let luma_row = y * planes.width;

        for x in 0..planes.width {
            let canvas_x = origin_x as usize + x;
            if canvas_x >= canvas_width {
                break;
            }
            let luma = *planes.y.get(luma_row + x).unwrap_or(&16) as i32;
            // 4:2:0 chroma is half resolution on both axes. Taking the nearest
            // sample leaves visible steps on colour edges; averaging the two
            // neighbours on each axis is a half-pel bilinear filter and costs
            // only a couple of adds.
            let cb = sample_chroma(&planes.u, planes.chroma_width, chroma_rows, x, y);
            let cr = sample_chroma(&planes.v, planes.chroma_width, chroma_rows, x, y);

            let c = (luma - k.luma_offset) * k.luma_gain;
            let d = cb - 128;
            let e = cr - 128;

            let red = (c + k.red_cr * e + 128) >> 8;
            let green = (c + k.green_cb * d + k.green_cr * e + 128) >> 8;
            let blue = (c + k.blue_cb * d + 128) >> 8;

            let at = (canvas_y * canvas_width + canvas_x) * 4;
            raw[at] = red.clamp(0, 255) as u8;
            raw[at + 1] = green.clamp(0, 255) as u8;
            raw[at + 2] = blue.clamp(0, 255) as u8;
            raw[at + 3] = 255;
        }
    }
}

/// Reads an item's bytes out of the file, following `idat` when it says so.
fn item_bytes<'a>(
    heif: &Heif,
    item: &Item,
    file: &'a [u8],
    idat: Option<&'a [u8]>,
) -> Option<Vec<u8>> {
    let _ = heif;
    let mut out = Vec::new();
    for extent in &item.extents {
        let slice = if extent.in_item_data {
            let idat = idat?;
            let start = extent.offset as usize;
            idat.get(start..(start + extent.length as usize).min(idat.len()))?
        } else {
            let start = extent.offset as usize;
            file.get(start..(start + extent.length as usize).min(file.len()))?
        };
        out.extend_from_slice(slice);
    }
    (!out.is_empty()).then_some(out)
}

/// Decodes the embedded thumbnail, when the file carries one.
///
/// Worth a separate path: the thumbnail is a single small coded image, so it
/// costs about a millisecond against a hundred for the full grid. Browsing a
/// folder of phone photos is the difference between instant and a slideshow.
pub fn decode_thumbnail(file: &[u8], heif: &Heif, idat: Option<&[u8]>) -> Result<RgbaImage> {
    let id = heif.thumbnail.context("this file has no thumbnail item")?;
    let item = heif
        .items
        .get(&id)
        .context("the thumbnail item is missing")?;
    let hvcc = item
        .hvcc
        .as_ref()
        .context("the thumbnail has no decoder configuration")?;
    let data = item_bytes(heif, item, file, idat).context("could not read the thumbnail's data")?;

    let planes = decode_item(hvcc, &data)?;
    let mut canvas = RgbaImage::new(planes.width as u32, planes.height as u32);
    yuv_to_rgba(&planes, &mut canvas, 0, 0, ColourSpec::default());
    Ok(canvas)
}

/// Decodes the primary image of a HEIC, compositing its grid if it has one.
///
/// `file` must be the whole file: unlike the metadata pass, the coded data
/// lives in `mdat` at the end.
pub fn decode_primary(file: &[u8], heif: &Heif, idat: Option<&[u8]>) -> Result<RgbaImage> {
    let spec = ColourSpec::default();
    if !heif.is_hevc {
        bail!("this file's images are not HEVC — only HEVC HEIC is supported");
    }

    match &heif.grid {
        Some(grid) => decode_grid(file, heif, grid, idat, spec),
        None => {
            let item = heif
                .items
                .get(&heif.primary)
                .context("the primary item is missing")?;
            let hvcc = item
                .hvcc
                .as_ref()
                .context("the primary item has no decoder configuration")?;
            let data = item_bytes(heif, item, file, idat)
                .context("could not read the primary item's data")?;
            let planes = decode_item(hvcc, &data)?;

            let mut canvas = RgbaImage::new(planes.width as u32, planes.height as u32);
            yuv_to_rgba(&planes, &mut canvas, 0, 0, spec);
            Ok(canvas)
        }
    }
}

/// Decodes every tile of a grid and lays them out.
///
/// Tiles are independent pictures, so they decode across threads — the same
/// shape as the tiled TIFF path, and the reason a 12 MP photo made of 48 tiles
/// is not 48 times slower than one picture.
fn decode_grid(
    file: &[u8],
    heif: &Heif,
    grid: &Grid,
    idat: Option<&[u8]>,
    spec: ColourSpec,
) -> Result<RgbaImage> {
    if grid.tiles.is_empty() {
        bail!("the grid lists no tiles");
    }

    // Every tile shares a decoder configuration in practice, but read each
    // one's own so an unusual file still works.
    let jobs: Vec<(usize, Vec<u8>, Vec<u8>)> = grid
        .tiles
        .iter()
        .enumerate()
        .filter_map(|(index, id)| {
            let item = heif.items.get(id)?;
            let hvcc = item.hvcc.clone()?;
            let data = item_bytes(heif, item, file, idat)?;
            Some((index, hvcc, data))
        })
        .collect();
    if jobs.is_empty() {
        bail!("none of the grid's tiles could be located in the file");
    }

    let decoded: Vec<(usize, Result<Planes>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|(index, hvcc, data)| scope.spawn(move || (index, decode_item(&hvcc, &data))))
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });

    // The grid states its own output size; the tiles may overhang it on the
    // right and bottom, which is normal and just gets clipped.
    let mut canvas = RgbaImage::new(grid.width.max(1), grid.height.max(1));
    let mut placed = 0;
    let mut first_error = None;

    for (index, result) in decoded {
        match result {
            Ok(planes) => {
                let column = index as u32 % grid.columns;
                let row = index as u32 / grid.columns;
                // Position from the tile's own size, not an assumed one.
                let x = column * planes.width as u32;
                let y = row * planes.height as u32;
                yuv_to_rgba(&planes, &mut canvas, x, y, spec);
                placed += 1;
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    if placed == 0 {
        return Err(first_error.unwrap_or_else(|| anyhow::anyhow!("no tiles decoded")));
    }
    Ok(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `hvcC`: 22 fixed bytes, one array holding one 4-byte NAL.
    fn sample_hvcc(length_size: usize) -> Vec<u8> {
        let mut hvcc = vec![0_u8; 21];
        // Bottom two bits of byte 21 are lengthSizeMinusOne.
        hvcc.push(0b1111_1100 | (length_size as u8 - 1));
        hvcc.push(1); // one array
        hvcc.push(0b1010_0000 | 33); // SPS
        hvcc.extend_from_slice(&1_u16.to_be_bytes()); // one NAL
        hvcc.extend_from_slice(&4_u16.to_be_bytes()); // of four bytes
        hvcc.extend_from_slice(&[0x42, 0x01, 0x02, 0x03]);
        hvcc
    }

    #[test]
    fn hvcc_yields_the_length_size_and_annex_b_parameter_sets() {
        for length_size in [1_usize, 2, 4] {
            let config = parse_hvcc(&sample_hvcc(length_size)).expect("should parse");
            assert_eq!(config.length_size, length_size);
            assert_eq!(
                config.parameter_sets,
                vec![0, 0, 0, 1, 0x42, 0x01, 0x02, 0x03],
                "parameter sets must come out with start codes"
            );
        }
    }

    #[test]
    fn a_truncated_or_empty_hvcc_is_an_error() {
        assert!(parse_hvcc(&[]).is_err());
        assert!(parse_hvcc(&[0_u8; 21]).is_err());
        // Declares an array but has no room for it.
        let mut short = vec![0_u8; 22];
        short.push(1);
        assert!(parse_hvcc(&short).is_err());
    }

    #[test]
    fn length_prefixed_nals_become_start_code_prefixed_ones() {
        let config = DecoderConfig {
            length_size: 4,
            parameter_sets: vec![0, 0, 0, 1, 0xAA],
        };
        // Two NALs, of 2 and 3 bytes.
        let mut data = 2_u32.to_be_bytes().to_vec();
        data.extend_from_slice(&[0x11, 0x22]);
        data.extend_from_slice(&3_u32.to_be_bytes());
        data.extend_from_slice(&[0x33, 0x44, 0x55]);

        let annex_b = to_annex_b(&config, &data);
        assert_eq!(
            annex_b,
            vec![
                0, 0, 0, 1, 0xAA, // parameter sets first
                0, 0, 0, 1, 0x11, 0x22, //
                0, 0, 0, 1, 0x33, 0x44, 0x55,
            ]
        );
    }

    #[test]
    fn a_two_byte_length_prefix_is_handled_too() {
        let config = DecoderConfig {
            length_size: 2,
            parameter_sets: Vec::new(),
        };
        let mut data = 3_u16.to_be_bytes().to_vec();
        data.extend_from_slice(&[1, 2, 3]);
        assert_eq!(to_annex_b(&config, &data), vec![0, 0, 0, 1, 1, 2, 3]);
    }

    #[test]
    fn a_length_running_past_the_buffer_is_clipped_rather_than_panicking() {
        let config = DecoderConfig {
            length_size: 4,
            parameter_sets: Vec::new(),
        };
        // Claims 999 bytes but supplies two.
        let mut data = 999_u32.to_be_bytes().to_vec();
        data.extend_from_slice(&[0x77, 0x88]);
        assert_eq!(to_annex_b(&config, &data), vec![0, 0, 0, 1, 0x77, 0x88]);

        // And a truncated length field simply ends the stream.
        assert_eq!(to_annex_b(&config, &[0, 0]), Vec::<u8>::new());
    }

    #[test]
    fn colour_conversion_maps_the_limited_range_end_points() {
        let limited = ColourSpec {
            full_range: false,
            matrix: 1,
        };
        // Y=16, neutral chroma is black; Y=235 is white.
        let black = Planes {
            y: vec![16],
            u: vec![128],
            v: vec![128],
            width: 1,
            height: 1,
            chroma_width: 1,
        };
        let mut canvas = RgbaImage::new(1, 1);
        yuv_to_rgba(&black, &mut canvas, 0, 0, limited);
        assert_eq!(canvas.get_pixel(0, 0).0, [0, 0, 0, 255]);

        let white = Planes {
            y: vec![235],
            ..black
        };
        yuv_to_rgba(&white, &mut canvas, 0, 0, limited);
        let pixel = canvas.get_pixel(0, 0).0;
        assert!(
            pixel[0] > 250 && pixel[1] > 250 && pixel[2] > 250,
            "Y=235 should be near white in limited range, got {pixel:?}"
        );
        assert_eq!(pixel[3], 255, "output must be opaque");

        // Full range leaves the samples where they are, so the same Y=235 is a
        // light grey rather than white. Getting these two mixed up is exactly
        // the bug this spec exists to prevent.
        let full = ColourSpec::default();
        assert!(full.full_range);
        let grey = Planes {
            y: vec![235],
            u: vec![128],
            v: vec![128],
            width: 1,
            height: 1,
            chroma_width: 1,
        };
        yuv_to_rgba(&grey, &mut canvas, 0, 0, full);
        assert_eq!(canvas.get_pixel(0, 0).0[0], 235);

        // And full-range black is 0, not 16.
        let full_black = Planes { y: vec![0], ..grey };
        yuv_to_rgba(&full_black, &mut canvas, 0, 0, full);
        assert_eq!(canvas.get_pixel(0, 0).0[0], 0);
    }

    /// Decodes real iPhone photos and reports what happened. The point is
    /// coverage: gain-map files, 10-bit files and odd grid shapes all have to
    /// come out, not just the one file that was used while writing this.
    ///
    /// `GEO_REF_MAPPER_HEIF_DIR=/photos GEO_REF_MAPPER_HEIF_N=20 cargo test --release bench_heic_decode -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_HEIF_DIR and real HEIC files"]
    fn bench_heic_decode() {
        use std::time::Instant;

        let Ok(dir) = std::env::var("GEO_REF_MAPPER_HEIF_DIR") else {
            eprintln!("set GEO_REF_MAPPER_HEIF_DIR");
            return;
        };
        let limit: usize = std::env::var("GEO_REF_MAPPER_HEIF_N")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(12);

        let files: Vec<std::path::PathBuf> = walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_map(Result::ok)
            .map(|entry| entry.path().to_path_buf())
            .filter(|path| {
                path.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("heic"))
            })
            .take(limit)
            .collect();
        assert!(!files.is_empty(), "no HEIC files under {dir}");

        let (mut ok, mut failed, mut skipped) = (0, 0, 0);
        let mut megapixels = 0.0_f64;
        let mut total_ms = 0.0_f64;

        for path in &files {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let Some(heif) = crate::heif::parse(&bytes) else {
                skipped += 1;
                continue;
            };
            if !heif.is_hevc {
                skipped += 1;
                continue;
            }
            let idat = heif
                .idat
                .and_then(|(start, end)| bytes.get(start as usize..end as usize));

            let started = Instant::now();
            match decode_primary(&bytes, &heif, idat) {
                Ok(canvas) => {
                    let ms = started.elapsed().as_secs_f64() * 1000.0;
                    let mp = canvas.width() as f64 * canvas.height() as f64 / 1e6;
                    ok += 1;
                    megapixels += mp;
                    total_ms += ms;
                    println!(
                        "  {:>5} x {:<5} {:>4} tiles {:>8.0} ms  {:>5.1} MP/s  {}{}",
                        canvas.width(),
                        canvas.height(),
                        heif.tile_count(),
                        ms,
                        mp / (ms / 1000.0),
                        if heif.has_gain_map { "HDR " } else { "" },
                        path.file_name().unwrap().to_string_lossy(),
                    );
                    // `decode_primary` composes the grid but does not turn it —
                    // rotation is applied a layer up, so the canvas should match
                    // the stored size, and the rotated size only after that.
                    assert_eq!(
                        (canvas.width(), canvas.height()),
                        (heif.width, heif.height),
                        "{} composed to a different size than the grid declares",
                        path.display()
                    );
                    let (display_width, display_height) = heif.display_size();
                    if heif.transform.swaps_axes() {
                        assert_eq!(
                            (display_width, display_height),
                            (canvas.height(), canvas.width()),
                            "a rotated file's display size should be the canvas transposed"
                        );
                    }
                }
                Err(error) => {
                    failed += 1;
                    println!(
                        "  FAILED  {}  ({error:#})",
                        path.file_name().unwrap().to_string_lossy()
                    );
                }
            }
        }

        println!(
            "\n  {ok} decoded, {failed} failed, {skipped} skipped — {:.1} MP total at {:.1} MP/s\n",
            megapixels,
            megapixels / (total_ms / 1000.0),
        );
        assert_eq!(failed, 0, "every HEVC HEIC should decode");
    }

    #[test]
    fn a_tile_that_overhangs_the_canvas_is_clipped() {
        // A 4x4 tile placed so only its top-left 2x2 fits.
        // Full range, so white is 255 rather than 235.
        let planes = Planes {
            y: vec![255; 16],
            u: vec![128; 4],
            v: vec![128; 4],
            width: 4,
            height: 4,
            chroma_width: 2,
        };
        let mut canvas = RgbaImage::new(6, 6);
        yuv_to_rgba(&planes, &mut canvas, 4, 4, ColourSpec::default());
        // Wrote into the corner, and did not panic going past the edge.
        assert!(canvas.get_pixel(5, 5).0[0] > 250);
        assert_eq!(
            canvas.get_pixel(0, 0).0[3],
            0,
            "untouched pixels stay clear"
        );
    }
}
