//! Structural facts about a JPEG, read from its markers without decoding.
//!
//! These are the properties that decide how a file can be handled: a
//! progressive JPEG cannot be decoded until the last scan arrives, and a
//! restart interval is what makes parallel entropy decoding possible at all —
//! without one, Huffman decoding is strictly sequential.

use std::path::Path;

/// Only the header is needed; no real JPEG puts SOS beyond this.
const HEADER_BUDGET: usize = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JpegInfo {
    pub progressive: bool,
    /// MCUs between restart markers, if the encoder wrote a DRI segment.
    pub restart_interval: Option<u16>,
    pub components: u8,
    pub precision: u8,
    /// Chroma subsampling in the usual `4:2:0` notation, when recognisable.
    pub subsampling: Option<&'static str>,
}

impl JpegInfo {
    /// Whether the entropy-coded data can be split across threads. Restart
    /// markers reset the Huffman state, which is the only way to know where a
    /// later chunk begins without decoding everything before it.
    pub fn is_parallel_decodable(&self) -> bool {
        self.restart_interval.is_some_and(|interval| interval > 0)
    }
}

pub fn probe(path: &Path) -> Option<JpegInfo> {
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(HEADER_BUDGET as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    parse(&bytes)
}

/// Walks the marker segments up to the start of scan.
pub fn parse(bytes: &[u8]) -> Option<JpegInfo> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None; // Not a JPEG.
    }

    let mut index = 2_usize;
    let mut info = JpegInfo {
        progressive: false,
        restart_interval: None,
        components: 0,
        precision: 8,
        subsampling: None,
    };
    let mut saw_frame = false;

    while index + 1 < bytes.len() {
        if bytes[index] != 0xFF {
            index += 1;
            continue;
        }
        let marker = bytes[index + 1];

        // Standalone markers carry no length.
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            index += 2;
            continue;
        }
        // Start of scan: the entropy data follows and there is nothing more to read.
        if marker == 0xDA {
            break;
        }
        if index + 4 > bytes.len() {
            break;
        }
        let length = u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]) as usize;
        if length < 2 {
            break;
        }
        let payload_start = index + 4;
        let payload_end = (index + 2 + length).min(bytes.len());
        let payload = &bytes[payload_start..payload_end];

        match marker {
            // Define restart interval.
            0xDD if payload.len() >= 2 => {
                info.restart_interval = Some(u16::from_be_bytes([payload[0], payload[1]]));
            }
            // Start of frame: 0xC2 is progressive, 0xC0/0xC1 baseline/extended.
            0xC0 | 0xC1 | 0xC2 if payload.len() >= 6 => {
                saw_frame = true;
                info.progressive = marker == 0xC2;
                info.precision = payload[0];
                info.components = payload[5];
                info.subsampling = subsampling(&payload[6..], info.components);
            }
            _ => {}
        }

        index = index + 2 + length;
    }

    saw_frame.then_some(info)
}

/// Reads the luma component's sampling factors, which is what the familiar
/// `4:2:0` style names describe.
fn subsampling(components: &[u8], count: u8) -> Option<&'static str> {
    if count != 3 || components.len() < 3 {
        return (count == 1).then_some("Grayscale");
    }
    // Each component is: id, sampling factors (high nibble horizontal, low
    // nibble vertical), quantisation table.
    let factors = components[1];
    Some(match (factors >> 4, factors & 0x0F) {
        (1, 1) => "4:4:4",
        (2, 1) => "4:2:2",
        (2, 2) => "4:2:0",
        (1, 2) => "4:4:0",
        (4, 1) => "4:1:1",
        _ => "unusual",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder as _;

    fn encode(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([120, 60, 30]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut std::io::Cursor::new(&mut bytes),
            80,
        )
        .encode_image(&image)
        .unwrap();
        bytes
    }

    #[test]
    fn reads_a_baseline_jpeg() {
        let info = parse(&encode(64, 48)).expect("a real JPEG parses");
        assert!(!info.progressive);
        assert_eq!(info.components, 3);
        assert_eq!(info.precision, 8);
        assert!(info.subsampling.is_some());
    }

    #[test]
    fn a_jpeg_without_a_restart_interval_is_not_parallel_decodable() {
        let info = parse(&encode(64, 48)).unwrap();
        assert_eq!(info.restart_interval, None);
        assert!(
            !info.is_parallel_decodable(),
            "no restart markers means Huffman decoding cannot be split"
        );
    }

    #[test]
    fn a_restart_interval_is_read_and_enables_parallel_decoding() {
        // Splice a DRI segment in just after SOI.
        let original = encode(64, 48);
        let mut spliced = original[..2].to_vec();
        spliced.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04, 0x01, 0x56]); // 342
        spliced.extend_from_slice(&original[2..]);

        let info = parse(&spliced).unwrap();
        assert_eq!(info.restart_interval, Some(342));
        assert!(info.is_parallel_decodable());
    }

    #[test]
    fn a_zero_restart_interval_does_not_count() {
        let original = encode(32, 32);
        let mut spliced = original[..2].to_vec();
        spliced.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04, 0x00, 0x00]);
        spliced.extend_from_slice(&original[2..]);

        let info = parse(&spliced).unwrap();
        assert_eq!(info.restart_interval, Some(0));
        assert!(!info.is_parallel_decodable(), "an interval of 0 means none");
    }

    #[test]
    fn non_jpeg_input_is_rejected() {
        assert!(parse(b"not a jpeg at all").is_none());
        assert!(parse(&[]).is_none());
        assert!(
            parse(&[0xFF, 0xD8]).is_none(),
            "SOI alone has no frame header"
        );
    }

    #[test]
    fn truncated_input_does_not_panic() {
        let full = encode(64, 48);
        for cut in [4, 8, 16, 32, 64, 100] {
            let _ = parse(&full[..cut.min(full.len())]);
        }
    }

    #[test]
    fn grayscale_is_reported() {
        let image = image::GrayImage::from_pixel(32, 32, image::Luma([128]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut std::io::Cursor::new(&mut bytes))
            .encode(&image, 32, 32, image::ExtendedColorType::L8)
            .unwrap();
        let info = parse(&bytes).unwrap();
        assert_eq!(info.components, 1);
        assert_eq!(info.subsampling, Some("Grayscale"));
    }
}
