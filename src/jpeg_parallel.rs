//! Multi-threaded JPEG decoding for files that carry restart markers.
//!
//! Huffman decoding is sequential: you cannot find MCU *N* without having
//! decoded *N-1*. Restart markers break that dependency — they reset both the
//! bit reader and the DC predictor — so the scan can be cut at those points and
//! the pieces decoded independently.
//!
//! No Rust JPEG decoder exposes "start at restart interval *k*", so instead of
//! writing an entropy decoder this rebuilds each piece as a **standalone JPEG**:
//! the original header with its height patched to the strip, that strip's
//! entropy bytes, and an EOI. Those are handed to the ordinary decoder in
//! parallel and the strips are stacked.
//!
//! Two constraints decide whether a file qualifies:
//!   * a restart interval must be a whole number of MCU rows, so a strip is a
//!     rectangle;
//!   * strips must begin on an RST0 boundary, because decoders expect the
//!     marker cycle to run RST0..RST7 in order.

use image::RgbImage;

use crate::jpeg::JpegInfo;

/// Below this there is nothing to gain from splitting.
const MIN_STRIPS: usize = 2;

/// What a split produced, for tests and diagnostics.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// Byte ranges of entropy data, one per strip.
    pub strips: Vec<(usize, usize)>,
    /// Image rows each strip covers (the last may be shorter).
    pub rows_per_strip: u32,
    pub mcu_rows_per_interval: u32,
}

struct Frame {
    /// Offset of the SOF payload, where the height field lives.
    sof_payload: usize,
    width: u32,
    height: u32,
    max_h: u8,
    max_v: u8,
    /// First byte of entropy-coded data.
    scan_start: usize,
}

/// Locates the frame header and the start of the scan.
fn frame(bytes: &[u8]) -> Option<Frame> {
    let mut index = 2_usize;
    let mut found: Option<(usize, u32, u32, u8, u8)> = None;

    while index + 3 < bytes.len() {
        if bytes[index] != 0xFF {
            index += 1;
            continue;
        }
        let marker = bytes[index + 1];
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            index += 2;
            continue;
        }
        let length = u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]) as usize;
        if length < 2 || index + 2 + length > bytes.len() {
            return None;
        }
        let payload = index + 4;

        if marker == 0xDA {
            // Entropy data begins after the SOS segment.
            let (sof_payload, width, height, max_h, max_v) = found?;
            return Some(Frame {
                sof_payload,
                width,
                height,
                max_h,
                max_v,
                scan_start: index + 2 + length,
            });
        }
        // Baseline or extended sequential only. Progressive has no per-strip
        // structure to exploit.
        if (marker == 0xC0 || marker == 0xC1) && length >= 8 {
            let height = u16::from_be_bytes([bytes[payload + 1], bytes[payload + 2]]) as u32;
            let width = u16::from_be_bytes([bytes[payload + 3], bytes[payload + 4]]) as u32;
            let components = bytes[payload + 5] as usize;
            let (mut max_h, mut max_v) = (1_u8, 1_u8);
            for component in 0..components {
                let factors = bytes.get(payload + 6 + component * 3 + 1)?;
                max_h = max_h.max(factors >> 4);
                max_v = max_v.max(factors & 0x0F);
            }
            found = Some((payload, width, height, max_h, max_v));
        }
        index += 2 + length;
    }
    None
}

/// Byte offsets of every RST marker in the entropy data.
fn restart_offsets(bytes: &[u8], scan_start: usize) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut index = scan_start;
    while index + 1 < bytes.len() {
        if bytes[index] == 0xFF {
            let next = bytes[index + 1];
            if (0xD0..=0xD7).contains(&next) {
                offsets.push(index);
                index += 2;
                continue;
            }
            if next == 0xD9 {
                break; // EOI
            }
        }
        index += 1;
    }
    offsets
}

/// Works out how to cut the scan, or `None` when the file does not qualify.
pub fn plan(bytes: &[u8], info: &JpegInfo, threads: usize) -> Option<Plan> {
    if info.progressive || !info.is_parallel_decodable() || threads < MIN_STRIPS {
        return None;
    }
    let interval = info.restart_interval? as u32;
    let frame = frame(bytes)?;

    let mcu_width = 8 * frame.max_h as u32;
    let mcu_height = 8 * frame.max_v as u32;
    let mcus_per_row = frame.width.div_ceil(mcu_width);
    if mcus_per_row == 0 || interval % mcus_per_row != 0 {
        // A restart interval that does not end on a row boundary would make a
        // strip a ragged shape, which cannot be expressed as a sub-image.
        return None;
    }
    let mcu_rows_per_interval = interval / mcus_per_row;

    let offsets = restart_offsets(bytes, frame.scan_start);
    if offsets.len() < MIN_STRIPS {
        return None;
    }

    // Strips must start where the RST counter is back at 0, so group intervals
    // in multiples of eight.
    let intervals = offsets.len() + 1;
    let groups_of_eight = intervals / 8;
    if groups_of_eight < MIN_STRIPS {
        return None;
    }
    let strips_wanted = threads.min(groups_of_eight);
    let intervals_per_strip = (groups_of_eight / strips_wanted) * 8;
    if intervals_per_strip == 0 {
        return None;
    }

    let mut strips = Vec::new();
    let mut start = frame.scan_start;
    let mut boundary = intervals_per_strip;
    while boundary <= offsets.len() {
        let end = offsets[boundary - 1];
        strips.push((start, end));
        start = end + 2; // Skip the RST marker itself.
        boundary += intervals_per_strip;
    }
    // Whatever is left over is the final strip.
    if start < bytes.len() {
        strips.push((start, bytes.len()));
    }
    if strips.len() < MIN_STRIPS {
        return None;
    }

    Some(Plan {
        strips,
        rows_per_strip: mcu_rows_per_interval * mcu_height * intervals_per_strip as u32,
        mcu_rows_per_interval,
    })
}

/// Rebuilds one strip as a standalone JPEG.
fn strip_jpeg(
    bytes: &[u8],
    frame_sof_payload: usize,
    scan_start: usize,
    range: (usize, usize),
    rows: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(scan_start + (range.1 - range.0) + 2);
    out.extend_from_slice(&bytes[..scan_start]);
    // Patch the height so the decoder expects only this strip.
    let height = (rows as u16).to_be_bytes();
    out[frame_sof_payload + 1] = height[0];
    out[frame_sof_payload + 2] = height[1];

    out.extend_from_slice(&bytes[range.0..range.1.min(bytes.len())]);
    if !out.ends_with(&[0xFF, 0xD9]) {
        out.extend_from_slice(&[0xFF, 0xD9]);
    }
    out
}

/// Decodes a JPEG across `threads` threads. Returns `None` when the file cannot
/// be split, so the caller falls back to the ordinary decoder.
#[cfg(test)]
pub fn decode_parallel(bytes: &[u8], info: &JpegInfo, threads: usize) -> Option<RgbImage> {
    decode_parallel_with(bytes, info, threads, &|_, _| {})
}

/// As [`decode_parallel`], reporting `(strip index, strip count)` as each strip
/// finishes so a caller can show which parts of the image are already done.
pub fn decode_parallel_with(
    bytes: &[u8],
    info: &JpegInfo,
    threads: usize,
    on_strip: &(dyn Fn(usize, usize) + Sync),
) -> Option<RgbImage> {
    let plan = plan(bytes, info, threads)?;
    let frame = frame(bytes)?;

    let strips: Vec<Option<RgbImage>> = std::thread::scope(|scope| {
        let handles: Vec<_> = plan
            .strips
            .iter()
            .enumerate()
            .map(|(index, range)| {
                let plan = &plan;
                let frame = &frame;
                scope.spawn(move || {
                    let remaining = frame
                        .height
                        .saturating_sub(index as u32 * plan.rows_per_strip);
                    let rows = plan.rows_per_strip.min(remaining);
                    if rows == 0 {
                        return None;
                    }
                    let piece =
                        strip_jpeg(bytes, frame.sof_payload, frame.scan_start, *range, rows);
                    let decoded =
                        image::load_from_memory_with_format(&piece, image::ImageFormat::Jpeg)
                            .ok()
                            .map(|image| image.to_rgb8());
                    on_strip(index, plan.strips.len());
                    decoded
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });

    // Any failed strip means the split was wrong; fall back rather than show a
    // half-corrupt image.
    if strips.iter().any(Option::is_none) {
        return None;
    }

    let mut canvas = RgbImage::new(frame.width, frame.height);
    let mut y = 0_u32;
    for strip in strips.into_iter().flatten() {
        for row in 0..strip.height() {
            if y + row >= frame.height {
                break;
            }
            for x in 0..strip.width().min(frame.width) {
                canvas.put_pixel(x, y + row, *strip.get_pixel(x, row));
            }
        }
        y += strip.height();
    }
    Some(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder as _;

    fn encode(width: u32, height: u32) -> Vec<u8> {
        let mut image = image::RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut std::io::Cursor::new(&mut bytes),
            90,
        )
        .encode_image(&image)
        .unwrap();
        bytes
    }

    #[test]
    fn a_file_without_restart_markers_has_no_plan() {
        let bytes = encode(256, 256);
        let info = crate::jpeg::parse(&bytes).unwrap();
        assert!(plan(&bytes, &info, 8).is_none());
        assert!(decode_parallel(&bytes, &info, 8).is_none());
    }

    #[test]
    fn a_progressive_file_is_refused() {
        let bytes = encode(256, 256);
        let mut info = crate::jpeg::parse(&bytes).unwrap();
        info.progressive = true;
        info.restart_interval = Some(16);
        assert!(plan(&bytes, &info, 8).is_none());
    }

    #[test]
    fn one_thread_never_splits() {
        let bytes = encode(256, 256);
        let mut info = crate::jpeg::parse(&bytes).unwrap();
        info.restart_interval = Some(16);
        assert!(plan(&bytes, &info, 1).is_none());
    }

    #[test]
    fn the_frame_header_is_located_correctly() {
        let bytes = encode(320, 240);
        let frame = frame(&bytes).expect("a baseline JPEG has a frame");
        assert_eq!((frame.width, frame.height), (320, 240));
        assert!(frame.scan_start > 0 && frame.scan_start < bytes.len());
        assert!(frame.max_h >= 1 && frame.max_v >= 1);
    }

    #[test]
    fn patching_the_height_produces_a_decodable_jpeg() {
        let bytes = encode(64, 64);
        let frame = frame(&bytes).unwrap();
        // The whole scan, relabelled as the full height, must still decode.
        let piece = strip_jpeg(
            &bytes,
            frame.sof_payload,
            frame.scan_start,
            (frame.scan_start, bytes.len()),
            64,
        );
        let decoded = image::load_from_memory_with_format(&piece, image::ImageFormat::Jpeg)
            .expect("the rebuilt JPEG should decode");
        assert_eq!((decoded.width(), decoded.height()), (64, 64));
    }

    /// Correctness and speed against real files that carry restart markers.
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_BENCH_DIR"]
    fn parallel_matches_serial_and_is_measured() {
        use std::time::Instant;

        let Ok(dir) = std::env::var("GEO_REF_MAPPER_BENCH_DIR") else {
            return;
        };
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("jpg"))
            })
            .collect();
        files.sort();
        files.truncate(6);
        assert!(!files.is_empty());

        let bytes = std::fs::read(&files[0]).unwrap();
        let info = crate::jpeg::parse(&bytes).expect("a JPEG");
        println!(
            "\n  restart interval: {:?}  progressive: {}",
            info.restart_interval, info.progressive
        );

        match plan(&bytes, &info, 16) {
            None => {
                println!("  NOT SPLITTABLE — falls back to the single-threaded decoder");
                return;
            }
            Some(p) => println!(
                "  plan: {} strips of {} rows",
                p.strips.len(),
                p.rows_per_strip
            ),
        }

        let t = Instant::now();
        let serial = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8();
        let serial_ms = t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        let parallel = decode_parallel(&bytes, &info, 16).expect("should split");
        let parallel_ms = t.elapsed().as_secs_f64() * 1000.0;

        assert_eq!(
            serial.dimensions(),
            parallel.dimensions(),
            "size must match"
        );
        let diff: u64 = serial
            .as_raw()
            .iter()
            .zip(parallel.as_raw().iter())
            .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64)
            .sum();
        let mean = diff as f64 / serial.as_raw().len() as f64;

        println!("  serial   {serial_ms:>7.1} ms");
        println!(
            "  parallel {parallel_ms:>7.1} ms   speed-up {:.2}x",
            serial_ms / parallel_ms
        );
        println!("  mean pixel difference: {mean:.3}/255");
        assert!(
            mean < 1.0,
            "parallel output must match serial (mean {mean})"
        );
    }

    #[test]
    fn a_restart_interval_off_a_row_boundary_is_refused() {
        let bytes = encode(256, 256);
        let mut info = crate::jpeg::parse(&bytes).unwrap();
        // 256 wide at 4:2:0 is 16 MCUs per row; 7 does not divide it.
        info.restart_interval = Some(7);
        assert!(plan(&bytes, &info, 8).is_none());
    }
}
