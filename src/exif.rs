//! EXIF metadata, read from the file's own bytes.
//!
//! EXIF is a TIFF file smuggled inside another one: a JPEG carries it in an
//! `APP1` segment whose payload begins with a complete TIFF header, and a TIFF
//! simply *is* that header. So one IFD reader serves both, and the only
//! difference is where the bytes start.
//!
//! Only the tags the INFO panel shows are pulled out. The rest are skipped
//! rather than stored, because an unrecognised tag is not an error — cameras
//! write plenty we have no use for.

use std::collections::HashMap;

/// How the camera was held, as tag 0x0112. The values are the EXIF numbering,
/// not an invention of ours, so a file's raw value maps straight across.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Orientation {
    #[default]
    Normal,
    FlipHorizontal,
    Rotate180,
    FlipVertical,
    /// Mirrored along the top-left to bottom-right diagonal.
    Transpose,
    Rotate90,
    /// Mirrored along the top-right to bottom-left diagonal.
    Transverse,
    Rotate270,
}

impl Orientation {
    pub fn from_tag(value: u16) -> Self {
        match value {
            2 => Self::FlipHorizontal,
            3 => Self::Rotate180,
            4 => Self::FlipVertical,
            5 => Self::Transpose,
            6 => Self::Rotate90,
            7 => Self::Transverse,
            8 => Self::Rotate270,
            // 1, and anything a camera invented, mean "already upright".
            _ => Self::Normal,
        }
    }

    pub fn tag(self) -> u16 {
        match self {
            Self::Normal => 1,
            Self::FlipHorizontal => 2,
            Self::Rotate180 => 3,
            Self::FlipVertical => 4,
            Self::Transpose => 5,
            Self::Rotate90 => 6,
            Self::Transverse => 7,
            Self::Rotate270 => 8,
        }
    }

    /// True when applying this swaps width and height.
    pub fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Transpose | Self::Rotate90 | Self::Transverse | Self::Rotate270
        )
    }

    /// How the stored pixels must be turned to be shown the right way up —
    /// *not* whether the picture is portrait or landscape, which is its shape
    /// and lives on the STATS tab. "Normal" invited exactly that confusion, so
    /// the upright case now says there is nothing to do.
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "None — already upright",
            Self::FlipHorizontal => "Mirrored",
            Self::Rotate180 => "Rotated 180°",
            Self::FlipVertical => "Flipped",
            Self::Transpose => "Mirrored, rotated 270°",
            Self::Rotate90 => "Rotated 90° CW",
            Self::Transverse => "Mirrored, rotated 90°",
            Self::Rotate270 => "Rotated 270° CW",
        }
    }

    /// Rights the image. A no-op for [`Self::Normal`], which is the common case,
    /// so the caller can apply it unconditionally without paying for a copy.
    pub fn apply(self, image: image::DynamicImage) -> image::DynamicImage {
        use image::DynamicImage as D;
        match self {
            Self::Normal => image,
            Self::FlipHorizontal => D::ImageRgba8(image::imageops::flip_horizontal(&image)),
            Self::Rotate180 => D::ImageRgba8(image::imageops::rotate180(&image)),
            Self::FlipVertical => D::ImageRgba8(image::imageops::flip_vertical(&image)),
            Self::Rotate90 => D::ImageRgba8(image::imageops::rotate90(&image)),
            Self::Rotate270 => D::ImageRgba8(image::imageops::rotate270(&image)),
            // The two diagonal mirrors are a rotation followed by a flip.
            Self::Transpose => D::ImageRgba8(image::imageops::flip_horizontal(
                &image::imageops::rotate90(&image),
            )),
            Self::Transverse => D::ImageRgba8(image::imageops::flip_horizontal(
                &image::imageops::rotate270(&image),
            )),
        }
    }
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Gps {
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Exif {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub focal_length_mm: Option<f64>,
    pub aperture: Option<f64>,
    /// Kept as a fraction so "1/250" survives; 1/3 s would round badly.
    pub exposure_time: Option<(u32, u32)>,
    pub iso: Option<u32>,
    pub taken_at: Option<String>,
    pub orientation: Orientation,
    pub exposure_bias_ev: Option<f64>,
    pub flash: Option<u16>,
    pub white_balance: Option<u16>,
    pub colour_space: Option<u16>,
    pub gps: Option<Gps>,

    /// Who or what wrote the file. These sit in the same IFD as everything
    /// above and were being parsed and discarded; half the JPEGs in a typical
    /// library name the application that last saved them.
    pub software: Option<String>,
    pub description: Option<String>,
    pub artist: Option<String>,
    pub copyright: Option<String>,
    pub host_computer: Option<String>,
    pub document_name: Option<String>,
}

impl Exif {
    /// True when nothing beyond a default orientation was found, so the panel
    /// can say "no EXIF" rather than showing a table of blanks.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `Sony` + `ILCE-7M4` without repeating the maker, which is how cameras
    /// usually write the model field.
    pub fn camera(&self) -> Option<String> {
        match (&self.make, &self.model) {
            (Some(make), Some(model)) => {
                let first = make.split_whitespace().next().unwrap_or(make);
                if model.to_lowercase().starts_with(&first.to_lowercase()) {
                    Some(model.clone())
                } else {
                    Some(format!("{first} {model}"))
                }
            }
            (None, Some(model)) => Some(model.clone()),
            (Some(make), None) => Some(make.clone()),
            (None, None) => None,
        }
    }

    pub fn shutter_label(&self) -> Option<String> {
        let (numerator, denominator) = self.exposure_time?;
        if denominator == 0 {
            return None;
        }
        let seconds = numerator as f64 / denominator as f64;
        Some(if seconds >= 1.0 {
            format!("{seconds:.1} s")
        } else {
            format!("1/{:.0}", 1.0 / seconds.max(f64::MIN_POSITIVE))
        })
    }
}

// ------------------------------------------------------------------ parsing

const TAG_PROCESSING_SOFTWARE: u16 = 0x000B;
const TAG_DOCUMENT_NAME: u16 = 0x010D;
const TAG_IMAGE_DESCRIPTION: u16 = 0x010E;
const TAG_MAKE: u16 = 0x010F;
const TAG_MODEL: u16 = 0x0110;
const TAG_ORIENTATION: u16 = 0x0112;
const TAG_SOFTWARE: u16 = 0x0131;
const TAG_ARTIST: u16 = 0x013B;
const TAG_HOST_COMPUTER: u16 = 0x013C;
const TAG_COPYRIGHT: u16 = 0x8298;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_GPS_IFD: u16 = 0x8825;

const TAG_EXPOSURE_TIME: u16 = 0x829A;
const TAG_F_NUMBER: u16 = 0x829D;
const TAG_ISO: u16 = 0x8827;
const TAG_ISO_SENSITIVITY: u16 = 0x8833;
const TAG_DATE_TIME_ORIGINAL: u16 = 0x9003;
const TAG_EXPOSURE_BIAS: u16 = 0x9204;
const TAG_FLASH: u16 = 0x9209;
const TAG_FOCAL_LENGTH: u16 = 0x920A;
const TAG_COLOUR_SPACE: u16 = 0xA001;
const TAG_WHITE_BALANCE: u16 = 0xA403;
const TAG_LENS_MODEL: u16 = 0xA434;

const GPS_LAT_REF: u16 = 0x0001;
const GPS_LAT: u16 = 0x0002;
const GPS_LON_REF: u16 = 0x0003;
const GPS_LON: u16 = 0x0004;
const GPS_ALT_REF: u16 = 0x0005;
const GPS_ALT: u16 = 0x0006;

/// Decodes a TIFF ASCII field.
///
/// The spec says ASCII, but writers put UTF-8 in there — these archival records
/// contain em dashes, and reading them byte-by-byte as Latin-1 turned
/// `Singapore — Maps` into `Singapore â€" Maps`. UTF-8 is tried first, with
/// Latin-1 as the fallback for the files that really are 8-bit.
fn tiff_ascii(raw: &[u8]) -> String {
    let raw = raw.split(|byte| *byte == 0).next().unwrap_or_default();
    match std::str::from_utf8(raw) {
        Ok(text) => text.trim().to_owned(),
        Err(_) => raw
            .iter()
            .map(|byte| *byte as char)
            .collect::<String>()
            .trim()
            .to_owned(),
    }
}

/// The TIFF version word, which says which flavour of offsets follow.
fn reader_version(payload: &[u8], little_endian: bool) -> Option<u16> {
    let raw = payload.get(2..4)?.try_into().ok()?;
    Some(if little_endian {
        u16::from_le_bytes(raw)
    } else {
        u16::from_be_bytes(raw)
    })
}

/// One IFD entry's payload, already resolved to values.
#[derive(Clone, Debug)]
enum Value {
    Text(String),
    Unsigned(Vec<u32>),
    Rational(Vec<(u32, u32)>),
    SignedRational(Vec<(i32, i32)>),
}

impl Value {
    fn first_unsigned(&self) -> Option<u32> {
        match self {
            Self::Unsigned(values) => values.first().copied(),
            Self::Rational(values) => values.first().map(|(n, _)| *n),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Unsigned(values) => values.first().map(|v| *v as f64),
            Self::Rational(values) => values
                .first()
                .filter(|(_, d)| *d != 0)
                .map(|(n, d)| *n as f64 / *d as f64),
            Self::SignedRational(values) => values
                .first()
                .filter(|(_, d)| *d != 0)
                .map(|(n, d)| *n as f64 / *d as f64),
            Self::Text(_) => None,
        }
    }

    fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    /// Degrees / minutes / seconds, as GPS coordinates are stored.
    fn as_dms(&self) -> Option<f64> {
        let Self::Rational(parts) = self else {
            return None;
        };
        let component = |index: usize| -> f64 {
            parts
                .get(index)
                .filter(|(_, d)| *d != 0)
                .map_or(0.0, |(n, d)| *n as f64 / *d as f64)
        };
        (!parts.is_empty()).then(|| component(0) + component(1) / 60.0 + component(2) / 3600.0)
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    little_endian: bool,
    /// BigTIFF: 8-byte counts and offsets, 20-byte entries.
    ///
    /// Refusing these used to be defensible — EXIF inside a JPEG is never
    /// BigTIFF. But a BigTIFF *file* keeps its text tags in IFD0 like any other
    /// TIFF, and this library is full of them: every scanned map carries its
    /// whole archival record in `ImageDescription`, and all of it was invisible.
    big: bool,
}

impl Reader<'_> {
    /// Bytes per offset, and the size of one IFD entry.
    fn offset_width(&self) -> usize {
        if self.big { 8 } else { 4 }
    }

    fn entry_size(&self) -> usize {
        if self.big { 20 } else { 12 }
    }
}

impl<'a> Reader<'a> {
    fn u16_at(&self, offset: usize) -> Option<u16> {
        let raw = self.bytes.get(offset..offset + 2)?.try_into().ok()?;
        Some(if self.little_endian {
            u16::from_le_bytes(raw)
        } else {
            u16::from_be_bytes(raw)
        })
    }

    fn u32_at(&self, offset: usize) -> Option<u32> {
        let raw = self.bytes.get(offset..offset + 4)?.try_into().ok()?;
        Some(if self.little_endian {
            u32::from_le_bytes(raw)
        } else {
            u32::from_be_bytes(raw)
        })
    }

    fn u64_at(&self, offset: usize) -> Option<u64> {
        let raw = self.bytes.get(offset..offset + 8)?.try_into().ok()?;
        Some(if self.little_endian {
            u64::from_le_bytes(raw)
        } else {
            u64::from_be_bytes(raw)
        })
    }

    /// An offset or count, whichever width this flavour uses.
    fn pointer_at(&self, offset: usize) -> Option<usize> {
        if self.big {
            usize::try_from(self.u64_at(offset)?).ok()
        } else {
            Some(self.u32_at(offset)? as usize)
        }
    }

    /// Reads one IFD into a tag map. `seen` guards against a file whose IFD
    /// pointers form a loop, which would otherwise recurse for ever.
    fn read_ifd(&self, offset: usize, seen: &mut Vec<usize>) -> HashMap<u16, Value> {
        let mut fields = HashMap::new();
        if seen.contains(&offset) || seen.len() > 8 {
            return fields;
        }
        seen.push(offset);

        // The entry count is 2 bytes in TIFF and 8 in BigTIFF, and the entries
        // start right after it.
        let counted = if self.big {
            self.pointer_at(offset).map(|count| (count, offset + 8))
        } else {
            self.u16_at(offset)
                .map(|count| (count as usize, offset + 2))
        };
        let Some((count, first_entry)) = counted else {
            return fields;
        };
        // A corrupt header can claim millions of entries; real IFDs are tens.
        if count > 4096 {
            return fields;
        }
        for index in 0..count {
            let entry = first_entry + index * self.entry_size();
            let (Some(tag), Some(kind), Some(items)) = (
                self.u16_at(entry),
                self.u16_at(entry + 2),
                self.pointer_at(entry + 4),
            ) else {
                break;
            };
            if let Some(value) = self.value_of(kind, items, entry + 4 + self.offset_width()) {
                fields.insert(tag, value);
            }
        }
        fields
    }

    /// Entry payloads live inline when they fit in four bytes and at an offset
    /// otherwise — the single most common source of EXIF parsing bugs.
    fn value_of(&self, kind: u16, items: usize, inline_at: usize) -> Option<Value> {
        let unit = match kind {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 => 4,
            5 | 10 | 12 => 8,
            _ => return None,
        };
        // A wildly large count means a corrupt entry, not a huge value.
        let total = items.checked_mul(unit)?;
        if total > self.bytes.len() {
            return None;
        }
        // Payloads live inline when they fit in the offset field, which is four
        // bytes in TIFF and eight in BigTIFF.
        let start = if total <= self.offset_width() {
            inline_at
        } else {
            self.pointer_at(inline_at)?
        };
        if start.checked_add(total)? > self.bytes.len() {
            return None;
        }

        Some(match kind {
            2 => Value::Text(tiff_ascii(self.bytes.get(start..start + items)?)),
            1 | 6 | 7 => Value::Unsigned(
                self.bytes
                    .get(start..start + items)?
                    .iter()
                    .map(|byte| *byte as u32)
                    .collect(),
            ),
            3 | 8 => Value::Unsigned(
                (0..items)
                    .filter_map(|i| self.u16_at(start + i * 2).map(u32::from))
                    .collect(),
            ),
            4 | 9 | 11 => Value::Unsigned(
                (0..items)
                    .filter_map(|i| self.u32_at(start + i * 4))
                    .collect(),
            ),
            5 | 12 => Value::Rational(
                (0..items)
                    .filter_map(|i| {
                        Some((self.u32_at(start + i * 8)?, self.u32_at(start + i * 8 + 4)?))
                    })
                    .collect(),
            ),
            10 => Value::SignedRational(
                (0..items)
                    .filter_map(|i| {
                        Some((
                            self.u32_at(start + i * 8)? as i32,
                            self.u32_at(start + i * 8 + 4)? as i32,
                        ))
                    })
                    .collect(),
            ),
            _ => return None,
        })
    }
}

/// Extracts the TIFF block from a HEIF `Exif` item.
///
/// The item does not start with the TIFF header: a four-byte
/// `exif_tiff_header_offset` comes first, and skipping it is the whole trick.
fn heif_exif_payload<'a>(bytes: &'a [u8], heif: &crate::heif::Heif) -> Option<&'a [u8]> {
    let extent = heif.exif.as_ref()?;
    let start = extent.offset as usize;
    let end = start.checked_add(extent.length as usize)?.min(bytes.len());
    let item = bytes.get(start..end)?;

    let header_offset = u32::from_be_bytes(item.get(..4)?.try_into().ok()?) as usize;
    item.get(4usize.checked_add(header_offset)?..)
}

/// Locates the TIFF header inside a file, whichever container it is in.
fn tiff_payload(bytes: &[u8]) -> Option<&[u8]> {
    // A bare TIFF: the header is the file.
    if matches!(bytes.get(..2), Some(b"II") | Some(b"MM")) {
        return Some(bytes);
    }
    // HEIF keeps EXIF in its own item, located through the container.
    if crate::heif::is_heif(bytes) {
        let heif = crate::heif::parse(bytes)?;
        return heif_exif_payload(bytes, &heif);
    }
    // A JPEG: walk the marker segments looking for APP1 / "Exif\0\0".
    if bytes.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut offset = 2;
    while offset + 4 <= bytes.len() {
        if bytes[offset] != 0xFF {
            offset += 1; // Padding between segments is legal.
            continue;
        }
        let marker = bytes[offset + 1];
        // Start of scan: pixel data follows, so any EXIF is behind us.
        if marker == 0xDA || marker == 0xD9 {
            return None;
        }
        let length =
            u16::from_be_bytes(bytes.get(offset + 2..offset + 4)?.try_into().ok()?) as usize;
        if length < 2 {
            return None;
        }
        let payload = bytes.get(offset + 4..offset + 2 + length)?;
        if marker == 0xE1 && payload.starts_with(b"Exif\0\0") {
            return payload.get(6..);
        }
        offset += 2 + length;
    }
    None
}

/// Reads what the INFO panel shows. Returns `None` when the file carries no
/// EXIF at all, which is normal for PNG and for anything a screenshot produced.
pub fn parse(bytes: &[u8]) -> Option<Exif> {
    parse_tiff(tiff_payload(bytes)?)
}

/// Parses a bare TIFF block, once a container has been peeled away.
fn parse_tiff(payload: &[u8]) -> Option<Exif> {
    let little_endian = match payload.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    // 42 for TIFF, 43 for BigTIFF.
    let big = match reader_version(payload, little_endian) {
        Some(42) => false,
        Some(43) => true,
        _ => return None,
    };
    let reader = Reader {
        bytes: payload,
        little_endian,
        big,
    };
    let mut seen = Vec::new();
    // BigTIFF states its own offset and reserved widths before the first IFD
    // pointer, which lives at byte 8 rather than 4.
    let first_ifd = if big {
        reader.pointer_at(8)?
    } else {
        reader.pointer_at(4)?
    };
    let ifd0 = reader.read_ifd(first_ifd, &mut seen);

    let mut exif = Exif {
        make: ifd0
            .get(&TAG_MAKE)
            .and_then(Value::as_text)
            .map(str::to_owned),
        model: ifd0
            .get(&TAG_MODEL)
            .and_then(Value::as_text)
            .map(str::to_owned),
        orientation: ifd0
            .get(&TAG_ORIENTATION)
            .and_then(Value::first_unsigned)
            .map_or(Orientation::Normal, |value| {
                Orientation::from_tag(value as u16)
            }),
        ..Default::default()
    };

    // Trimmed and emptied-out: cameras pad these to a fixed width with spaces
    // and NULs, and a row reading `"   "` is worse than no row.
    let text = |tag: u16| -> Option<String> {
        ifd0.get(&tag)
            .and_then(Value::as_text)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    exif.software = text(TAG_SOFTWARE).or_else(|| text(TAG_PROCESSING_SOFTWARE));
    exif.description = text(TAG_IMAGE_DESCRIPTION);
    exif.artist = text(TAG_ARTIST);
    exif.copyright = text(TAG_COPYRIGHT);
    exif.host_computer = text(TAG_HOST_COMPUTER);
    exif.document_name = text(TAG_DOCUMENT_NAME);

    if let Some(pointer) = ifd0.get(&TAG_EXIF_IFD).and_then(Value::first_unsigned) {
        let sub = reader.read_ifd(pointer as usize, &mut seen);
        exif.lens = sub
            .get(&TAG_LENS_MODEL)
            .and_then(Value::as_text)
            .map(str::to_owned);
        exif.focal_length_mm = sub.get(&TAG_FOCAL_LENGTH).and_then(Value::as_f64);
        exif.aperture = sub.get(&TAG_F_NUMBER).and_then(Value::as_f64);
        exif.exposure_time = match sub.get(&TAG_EXPOSURE_TIME) {
            Some(Value::Rational(values)) => values.first().copied(),
            _ => None,
        };
        exif.iso = sub
            .get(&TAG_ISO)
            .or_else(|| sub.get(&TAG_ISO_SENSITIVITY))
            .and_then(Value::first_unsigned);
        exif.taken_at = sub
            .get(&TAG_DATE_TIME_ORIGINAL)
            .and_then(Value::as_text)
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        exif.exposure_bias_ev = sub.get(&TAG_EXPOSURE_BIAS).and_then(Value::as_f64);
        exif.flash = sub
            .get(&TAG_FLASH)
            .and_then(Value::first_unsigned)
            .map(|v| v as u16);
        exif.white_balance = sub
            .get(&TAG_WHITE_BALANCE)
            .and_then(Value::first_unsigned)
            .map(|v| v as u16);
        exif.colour_space = sub
            .get(&TAG_COLOUR_SPACE)
            .and_then(Value::first_unsigned)
            .map(|v| v as u16);
    }

    if let Some(pointer) = ifd0.get(&TAG_GPS_IFD).and_then(Value::first_unsigned) {
        let gps_ifd = reader.read_ifd(pointer as usize, &mut seen);
        // South and West are stored as a positive angle plus a hemisphere letter.
        let signed = |value: u16, reference: u16, negative: char| -> Option<f64> {
            let degrees = gps_ifd.get(&value)?.as_dms()?;
            let hemisphere = gps_ifd
                .get(&reference)
                .and_then(Value::as_text)
                .and_then(|text| text.chars().next())
                .unwrap_or('N');
            Some(if hemisphere.eq_ignore_ascii_case(&negative) {
                -degrees
            } else {
                degrees
            })
        };
        let gps = Gps {
            latitude: signed(GPS_LAT, GPS_LAT_REF, 'S'),
            longitude: signed(GPS_LON, GPS_LON_REF, 'W'),
            altitude_m: gps_ifd.get(&GPS_ALT).and_then(Value::as_f64).map(|metres| {
                // Reference 1 means below sea level.
                let below = gps_ifd
                    .get(&GPS_ALT_REF)
                    .and_then(Value::first_unsigned)
                    .is_some_and(|value| value == 1);
                if below { -metres } else { metres }
            }),
        };
        if gps != Gps::default() {
            exif.gps = Some(gps);
        }
    }

    Some(exif)
}

/// Just the orientation, for the decode path, which does not need the rest.
pub fn orientation(bytes: &[u8]) -> Orientation {
    parse(bytes).map_or(Orientation::Normal, |exif| exif.orientation)
}

/// How much of a file to read when only the metadata is wanted. A JPEG's `APP1`
/// segment sits within the first few kilobytes, so reading the whole of a 400 MB
/// map to answer "what lens was this?" would be absurd.
const METADATA_PREFIX: usize = 2 * 1024 * 1024;

/// A scan touches thousands of files, so it reads far less of each. An `APP1`
/// segment is within the first few kilobytes in practice.
pub const SCAN_PREFIX: usize = 64 * 1024;

/// Reads EXIF from a file without loading all of it.
///
/// A TIFF whose IFD lives past `limit` is missed rather than paid for; the
/// alternative is reading hundreds of megabytes per file.
pub fn from_path_prefix(path: &std::path::Path, limit: usize) -> Option<Exif> {
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit as u64).read_to_end(&mut bytes).ok()?;

    // A HEIF's EXIF item can sit anywhere in `mdat`, well past any prefix worth
    // reading blind. The container says exactly where, so seek to it instead.
    if crate::heif::is_heif(&bytes) {
        return heif_exif_from_path(path);
    }
    parse(&bytes).filter(|exif| !exif.is_empty())
}

/// Reads only the `Exif` item of a HEIF, wherever in the file it lives.
fn heif_exif_from_path(path: &std::path::Path) -> Option<Exif> {
    use std::io::{Read, Seek, SeekFrom};

    let heif = crate::heif::from_path(path)?;
    let extent = heif.exif.as_ref()?;
    // A sane ceiling: real EXIF blocks are a few kilobytes.
    if extent.length == 0 || extent.length > 4 * 1024 * 1024 {
        return None;
    }

    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(extent.offset)).ok()?;
    let mut item = vec![0_u8; extent.length as usize];
    file.read_exact(&mut item).ok()?;

    let header_offset = u32::from_be_bytes(item.get(..4)?.try_into().ok()?) as usize;
    let tiff = item.get(4usize.checked_add(header_offset)?..)?;
    parse_tiff(tiff).filter(|exif| !exif.is_empty())
}

/// Orientation the decoded pixels still need, which is not always what the
/// file's EXIF says.
///
/// A HEIF records its rotation twice: once as the container's `irot` and again
/// as an EXIF tag saying the same thing. Our HEIC decoder applies the container
/// transform, so applying the EXIF tag on top would turn the picture a second
/// time and leave every portrait photo on its side.
pub fn pending_orientation(bytes: &[u8]) -> Orientation {
    if crate::heif::is_heif(bytes) {
        return Orientation::Normal;
    }
    orientation(bytes)
}

/// For the INFO panel, which asks about one file at a time and can afford more.
pub fn from_path(path: &std::path::Path) -> Option<Exif> {
    from_path_prefix(path, METADATA_PREFIX)
        .or_else(|| tiff_from_file(path).filter(|exif| !exif.is_empty()))
}

/// Reads a TIFF's IFD0 by seeking to it.
///
/// A prefix is no good here: TIFF writers routinely put the directory at the
/// *end* of the file. In the scanned maps here the first IFD sits at byte
/// 17,859,200 of a 17,859,496-byte file, so reading even a generous opening
/// slice finds nothing at all — which is why these files looked as though they
/// carried no metadata when in fact they carry the most of anything here.
fn tiff_from_file(path: &std::path::Path) -> Option<Exif> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0_u8; 16];
    file.read_exact(&mut header).ok()?;

    let little = match &header[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let int = |raw: &[u8]| -> u64 {
        let mut value = 0_u64;
        if little {
            for byte in raw.iter().rev() {
                value = (value << 8) | *byte as u64;
            }
        } else {
            for byte in raw {
                value = (value << 8) | *byte as u64;
            }
        }
        value
    };

    let (big, first) = match int(&header[2..4]) {
        42 => (false, int(&header[4..8])),
        43 => (true, int(&header[8..16])),
        _ => return None,
    };
    let width = if big { 8 } else { 4 };
    let entry_size = if big { 20 } else { 12 };

    file.seek(SeekFrom::Start(first)).ok()?;
    let mut raw_count = vec![0_u8; if big { 8 } else { 2 }];
    file.read_exact(&mut raw_count).ok()?;
    let count = int(&raw_count) as usize;
    // Real directories hold tens of entries; a huge count is a corrupt header.
    if count == 0 || count > 4096 {
        return None;
    }
    let mut table = vec![0_u8; count * entry_size];
    file.read_exact(&mut table).ok()?;

    let mut exif = Exif::default();
    for index in 0..count {
        let entry = &table[index * entry_size..(index + 1) * entry_size];
        let tag = int(&entry[0..2]) as u16;
        let kind = int(&entry[2..4]) as u16;
        let items = int(&entry[4..4 + width]) as usize;
        let value_field = &entry[4 + width..4 + width * 2];

        match kind {
            // ASCII. These are the ones worth crossing the file for.
            2 => {
                // The archival records in these maps run to a couple of
                // thousand characters; anything past this is not a caption.
                if items == 0 || items > 64 * 1024 {
                    continue;
                }
                let mut raw = vec![0_u8; items];
                if items <= width {
                    raw.copy_from_slice(&value_field[..items]);
                } else {
                    if file.seek(SeekFrom::Start(int(value_field))).is_err() {
                        continue;
                    }
                    if file.read_exact(&mut raw).is_err() {
                        continue;
                    }
                }
                let text = tiff_ascii(&raw);
                if text.is_empty() {
                    continue;
                }
                match tag {
                    TAG_SOFTWARE | TAG_PROCESSING_SOFTWARE => exif.software = Some(text),
                    TAG_IMAGE_DESCRIPTION => exif.description = Some(text),
                    TAG_ARTIST => exif.artist = Some(text),
                    TAG_COPYRIGHT => exif.copyright = Some(text),
                    TAG_HOST_COMPUTER => exif.host_computer = Some(text),
                    TAG_DOCUMENT_NAME => exif.document_name = Some(text),
                    TAG_MAKE => exif.make = Some(text),
                    TAG_MODEL => exif.model = Some(text),
                    _ => {}
                }
            }
            // SHORT, stored inline.
            3 if tag == TAG_ORIENTATION => {
                exif.orientation = Orientation::from_tag(int(&value_field[..2]) as u16);
            }
            _ => {}
        }
    }
    Some(exif)
}

/// Converts to the shape the library stores. `taken_at` is left to the caller —
/// parsing EXIF's `YYYY:MM:DD HH:MM:SS` into an epoch needs a calendar.
impl Exif {
    pub fn to_record(&self) -> crate::db::ExifRecord {
        crate::db::ExifRecord {
            camera: self.camera(),
            lens: self.lens.clone(),
            focal_length: self.focal_length_mm,
            aperture: self.aperture,
            shutter: self.shutter_label(),
            iso: self.iso.map(i64::from),
            taken_at: None,
            orientation: Some(i64::from(self.orientation.tag())),
            gps_lat: self.gps.as_ref().and_then(|gps| gps.latitude),
            gps_lon: self.gps.as_ref().and_then(|gps| gps.longitude),
            color_space: self.colour_space.map(|value| {
                match value {
                    1 => "sRGB",
                    2 => "Adobe RGB",
                    0xFFFF => "Uncalibrated",
                    _ => "Unknown",
                }
                .to_owned()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a little-endian EXIF APP1 payload with the given IFD0 entries,
    /// so the parser can be tested without shipping binary fixtures.
    fn exif_jpeg(entries: &[(u16, u16, u32, [u8; 4])]) -> Vec<u8> {
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42_u16.to_le_bytes());
        tiff.extend_from_slice(&8_u32.to_le_bytes()); // IFD0 at offset 8
        tiff.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (tag, kind, count, payload) in entries {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&kind.to_le_bytes());
            tiff.extend_from_slice(&count.to_le_bytes());
            tiff.extend_from_slice(payload);
        }
        tiff.extend_from_slice(&0_u32.to_le_bytes()); // no IFD1

        let mut app1 = Vec::from(*b"Exif\0\0");
        app1.extend_from_slice(&tiff);

        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&app1);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        jpeg
    }

    #[test]
    fn orientation_is_read_from_a_jpeg_app1_segment() {
        for (tag, expected) in [
            (1_u32, Orientation::Normal),
            (3, Orientation::Rotate180),
            (6, Orientation::Rotate90),
            (8, Orientation::Rotate270),
        ] {
            let mut payload = [0_u8; 4];
            payload[..2].copy_from_slice(&(tag as u16).to_le_bytes());
            let jpeg = exif_jpeg(&[(TAG_ORIENTATION, 3, 1, payload)]);
            assert_eq!(parse(&jpeg).unwrap().orientation, expected, "tag {tag}");
        }
    }

    #[test]
    fn an_unknown_orientation_value_is_treated_as_upright() {
        // Rather than panicking or rotating by something arbitrary.
        assert_eq!(Orientation::from_tag(0), Orientation::Normal);
        assert_eq!(Orientation::from_tag(99), Orientation::Normal);
    }

    #[test]
    fn a_file_with_no_exif_is_not_an_error() {
        assert!(parse(b"").is_none());
        assert!(
            parse(b"\x89PNG\r\n\x1a\n").is_none(),
            "PNG carries none here"
        );
        // A JPEG whose scan starts before any APP1.
        assert!(parse(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02]).is_none());
        assert_eq!(orientation(b"nonsense"), Orientation::Normal);
    }

    #[test]
    fn truncated_and_looping_structures_do_not_hang_or_panic() {
        // An IFD count that promises far more entries than the buffer holds.
        let mut jpeg = exif_jpeg(&[(TAG_ORIENTATION, 3, 1, [6, 0, 0, 0])]);
        let length = jpeg.len();
        jpeg.truncate(length - 6);
        let _ = parse(&jpeg);

        // An ExifIFD pointer aimed back at IFD0 would recurse for ever.
        let looping = exif_jpeg(&[(TAG_EXIF_IFD, 4, 1, 8_u32.to_le_bytes())]);
        let _ = parse(&looping);
    }

    #[test]
    fn text_values_longer_than_four_bytes_are_followed_to_their_offset() {
        // "Canon EOS R5" cannot fit inline, so the entry holds an offset. IFD0
        // starts at 8: 2 count + 12 entry + 4 next = data begins at 26.
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42_u16.to_le_bytes());
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&TAG_MODEL.to_le_bytes());
        tiff.extend_from_slice(&2_u16.to_le_bytes()); // ASCII
        tiff.extend_from_slice(&13_u32.to_le_bytes()); // includes the NUL
        tiff.extend_from_slice(&26_u32.to_le_bytes());
        tiff.extend_from_slice(&0_u32.to_le_bytes());
        tiff.extend_from_slice(b"Canon EOS R5\0");

        let mut app1 = Vec::from(*b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&app1);

        assert_eq!(parse(&jpeg).unwrap().model.as_deref(), Some("Canon EOS R5"));
    }

    #[test]
    fn big_endian_files_are_read_too() {
        // Nikon and Canon both write MM; getting this wrong reads garbage.
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"MM");
        tiff.extend_from_slice(&42_u16.to_be_bytes());
        tiff.extend_from_slice(&8_u32.to_be_bytes());
        tiff.extend_from_slice(&1_u16.to_be_bytes());
        tiff.extend_from_slice(&TAG_ORIENTATION.to_be_bytes());
        tiff.extend_from_slice(&3_u16.to_be_bytes());
        tiff.extend_from_slice(&1_u32.to_be_bytes());
        tiff.extend_from_slice(&[0x00, 0x06, 0x00, 0x00]); // SHORT 6, left-packed

        let mut app1 = Vec::from(*b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&app1);

        assert_eq!(parse(&jpeg).unwrap().orientation, Orientation::Rotate90);
    }

    #[test]
    fn rotating_by_ninety_swaps_the_axes() {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(40, 10));
        let rotated = Orientation::Rotate90.apply(image.clone());
        assert_eq!((rotated.width(), rotated.height()), (10, 40));
        assert!(Orientation::Rotate90.swaps_axes());

        let untouched = Orientation::Normal.apply(image);
        assert_eq!((untouched.width(), untouched.height()), (40, 10));
        assert!(!Orientation::Normal.swaps_axes());
        assert!(!Orientation::Rotate180.swaps_axes());
    }

    #[test]
    fn rotation_moves_the_corner_pixel_the_right_way() {
        // A marker in the top-left must end up top-right after a 90° CW turn.
        let mut source = image::RgbaImage::from_pixel(4, 2, image::Rgba([0, 0, 0, 255]));
        source.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        let rotated = Orientation::Rotate90.apply(image::DynamicImage::ImageRgba8(source));
        let rgba = rotated.to_rgba8();
        assert_eq!(rgba.dimensions(), (2, 4));
        assert_eq!(*rgba.get_pixel(1, 0), image::Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn shutter_reads_as_a_fraction_below_a_second_and_a_decimal_above() {
        let fast = Exif {
            exposure_time: Some((1, 250)),
            ..Default::default()
        };
        assert_eq!(fast.shutter_label().as_deref(), Some("1/250"));

        let slow = Exif {
            exposure_time: Some((5, 2)),
            ..Default::default()
        };
        assert_eq!(slow.shutter_label().as_deref(), Some("2.5 s"));

        let broken = Exif {
            exposure_time: Some((1, 0)),
            ..Default::default()
        };
        assert!(
            broken.shutter_label().is_none(),
            "a zero denominator must not divide"
        );
    }

    #[test]
    fn camera_name_does_not_repeat_the_maker() {
        let sony = Exif {
            make: Some("SONY".to_owned()),
            model: Some("ILCE-7M4".to_owned()),
            ..Default::default()
        };
        assert_eq!(sony.camera().as_deref(), Some("SONY ILCE-7M4"));

        let canon = Exif {
            make: Some("Canon".to_owned()),
            model: Some("Canon EOS R5".to_owned()),
            ..Default::default()
        };
        assert_eq!(canon.camera().as_deref(), Some("Canon EOS R5"));

        assert!(Exif::default().camera().is_none());
    }

    /// Reads real photographs and prints what we extracted, so the output can
    /// be diffed against `exiftool`. Synthetic fixtures prove the shape of the
    /// parser; only real camera files prove it survives what cameras write.
    ///
    /// `GEO_REF_MAPPER_EXIF_DIR=/path/to/photos cargo test --release bench_exif -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_EXIF_DIR and real photos"]
    fn bench_exif_against_real_photos() {
        let Ok(dir) = std::env::var("GEO_REF_MAPPER_EXIF_DIR") else {
            eprintln!("set GEO_REF_MAPPER_EXIF_DIR");
            return;
        };
        let mut files: Vec<std::path::PathBuf> = walkdir::WalkDir::new(&dir)
            .max_depth(3)
            .into_iter()
            .filter_map(Result::ok)
            .map(|entry| entry.path().to_path_buf())
            .filter(|path| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        extension.eq_ignore_ascii_case("jpg")
                            || extension.eq_ignore_ascii_case("jpeg")
                    })
            })
            .collect();
        files.sort();
        files.truncate(25);

        let mut with_exif = 0;
        let mut rotated = 0;
        for path in &files {
            let Some(exif) = from_path(path) else {
                println!("  (none)  {}", path.file_name().unwrap().to_string_lossy());
                continue;
            };
            with_exif += 1;
            if exif.orientation != Orientation::Normal {
                rotated += 1;
            }
            println!(
                "  {:<28} {:<22} {:<16} {:>6} {:>7} {:>6}  {}  {}",
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .chars()
                    .take(28)
                    .collect::<String>(),
                exif.camera()
                    .unwrap_or_default()
                    .chars()
                    .take(22)
                    .collect::<String>(),
                exif.lens
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .take(16)
                    .collect::<String>(),
                exif.aperture
                    .map(|f| format!("f/{f:.1}"))
                    .unwrap_or_default(),
                exif.shutter_label().unwrap_or_default(),
                exif.iso.map(|i| i.to_string()).unwrap_or_default(),
                exif.orientation.label(),
                exif.gps
                    .as_ref()
                    .and_then(|gps| Some(format!("{:.4},{:.4}", gps.latitude?, gps.longitude?)))
                    .unwrap_or_default(),
            );
        }
        println!(
            "\n  {} of {} files carried EXIF, {rotated} needed rotating\n",
            with_exif,
            files.len()
        );
    }

    /// A HEIF records its rotation twice — `irot` in the container and an EXIF
    /// tag saying the same thing. The decoder applies the container's, so the
    /// pixels need nothing further; applying the tag too would turn every
    /// portrait photo onto its side.
    #[test]
    fn a_heif_reports_no_pending_orientation_even_when_its_exif_says_otherwise() {
        // Enough of a HEIF for `is_heif` to recognise it.
        let mut heic = vec![0, 0, 0, 0x18];
        heic.extend_from_slice(b"ftypheic");
        heic.extend_from_slice(&0_u32.to_be_bytes());
        heic.extend_from_slice(b"mif1heic");
        assert!(crate::heif::is_heif(&heic));
        assert_eq!(pending_orientation(&heic), Orientation::Normal);

        // A JPEG still gets its EXIF orientation applied, as before.
        let mut payload = [0_u8; 4];
        payload[..2].copy_from_slice(&6_u16.to_le_bytes());
        let jpeg = exif_jpeg(&[(TAG_ORIENTATION, 3, 1, payload)]);
        assert_eq!(orientation(&jpeg), Orientation::Rotate90);
        assert_eq!(
            pending_orientation(&jpeg),
            Orientation::Rotate90,
            "only HEIF is exempt"
        );
    }

    /// Reads EXIF out of real HEIC files. Until this worked, the INFO panel
    /// claimed every iPhone photo carried no metadata at all.
    ///
    /// `GEO_REF_MAPPER_HEIF_DIR=/photos cargo test --release heic_exif -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_HEIF_DIR and real HEIC files"]
    fn heic_exif_is_read_from_the_container_item() {
        let Ok(dir) = std::env::var("GEO_REF_MAPPER_HEIF_DIR") else {
            eprintln!("set GEO_REF_MAPPER_HEIF_DIR");
            return;
        };
        let files: Vec<std::path::PathBuf> = walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_map(Result::ok)
            .map(|entry| entry.path().to_path_buf())
            .filter(|path| {
                path.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("heic"))
            })
            .take(12)
            .collect();
        assert!(!files.is_empty(), "no HEIC files under {dir}");

        let mut with_exif = 0;
        for path in &files {
            let Some(exif) = from_path(path) else {
                println!("  (none)  {}", path.file_name().unwrap().to_string_lossy());
                continue;
            };
            with_exif += 1;
            println!(
                "  {:<22} {:<16} {:>6} {:>7} {:>6}  {}  {}",
                exif.camera()
                    .unwrap_or_default()
                    .chars()
                    .take(22)
                    .collect::<String>(),
                exif.lens
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .take(16)
                    .collect::<String>(),
                exif.aperture
                    .map(|f| format!("f/{f:.1}"))
                    .unwrap_or_default(),
                exif.shutter_label().unwrap_or_default(),
                exif.iso.map(|i| i.to_string()).unwrap_or_default(),
                exif.orientation.label(),
                exif.taken_at.clone().unwrap_or_default(),
            );
        }
        println!("\n  {with_exif} of {} carried EXIF\n", files.len());
        assert_eq!(with_exif, files.len(), "every iPhone HEIC has an Exif item");
    }

    #[test]
    fn an_exif_with_nothing_in_it_reports_empty() {
        assert!(Exif::default().is_empty());
        assert!(
            !Exif {
                iso: Some(100),
                ..Default::default()
            }
            .is_empty()
        );
    }
}

#[cfg(test)]
mod bigtiff {
    use super::*;

    /// Builds a BigTIFF whose only IFD sits at the very end of the file, with a
    /// long UTF-8 `ImageDescription` stored out of line — the exact shape of
    /// the scanned maps in this library.
    fn map_like(description: &str, software: &str) -> Vec<u8> {
        let mut file = Vec::new();
        file.extend_from_slice(b"II");
        file.extend_from_slice(&43_u16.to_le_bytes());
        file.extend_from_slice(&8_u16.to_le_bytes()); // offset size
        file.extend_from_slice(&0_u16.to_le_bytes()); // reserved
        file.extend_from_slice(&0_u64.to_le_bytes()); // first IFD, patched below

        // Pixel data, so the directory really is at the end of a large file.
        file.extend_from_slice(&vec![0xAB; 200_000]);

        let description_at = file.len() as u64;
        file.extend_from_slice(description.as_bytes());
        file.push(0);
        let software_at = file.len() as u64;
        file.extend_from_slice(software.as_bytes());
        file.push(0);

        let ifd_at = file.len() as u64;
        file[8..16].copy_from_slice(&ifd_at.to_le_bytes());
        file.extend_from_slice(&3_u64.to_le_bytes()); // entry count

        let mut entry = |tag: u16, kind: u16, items: u64, value: u64| {
            file.extend_from_slice(&tag.to_le_bytes());
            file.extend_from_slice(&kind.to_le_bytes());
            file.extend_from_slice(&items.to_le_bytes());
            file.extend_from_slice(&value.to_le_bytes());
        };
        entry(
            TAG_IMAGE_DESCRIPTION,
            2,
            description.len() as u64 + 1,
            description_at,
        );
        entry(TAG_ORIENTATION, 3, 1, 6); // Rotate 90 CW, stored inline
        entry(TAG_SOFTWARE, 2, software.len() as u64 + 1, software_at);
        file
    }

    #[test]
    fn a_bigtiff_with_its_directory_at_the_end_is_read() {
        // Both halves of the bug this covers: BigTIFF was refused outright, and
        // even once accepted, a directory 200 KB into the file is past any
        // prefix worth reading — it has to be sought.
        let description = "Plan De La Ville De Malaca 1764\nCollection: \
                           National Archives of Singapore — Maps and Building Plans";
        let bytes = map_like(description, "mapDownloader 0.1.0");

        let path =
            std::env::temp_dir().join(format!("geo-ref_mapper-bigtiff-{}.tif", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let exif = from_path(&path).expect("a map this full of metadata must not read as empty");

        assert_eq!(exif.software.as_deref(), Some("mapDownloader 0.1.0"));
        assert_eq!(exif.description.as_deref(), Some(description));
        assert!(
            exif.description.unwrap().contains("Singapore — Maps"),
            "the em dash must survive: TIFF says ASCII, writers write UTF-8",
        );
        assert_eq!(exif.orientation, Orientation::Rotate90);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_prefix_read_still_misses_it_which_is_why_the_seek_exists() {
        let bytes = map_like("something", "writer");
        // `parse` hands back an empty record rather than `None`, and the
        // callers treat empty as "nothing found" — either way a prefix read
        // comes up with nothing, which is what the seek exists to fix.
        assert!(
            parse(&bytes[..100_000]).is_none_or(|exif| exif.is_empty()),
            "the directory is past the prefix, so this cannot find anything",
        );
    }
}

#[cfg(test)]
mod tiff_survey {
    /// `GEO_REF_MAPPER_TIFF_DIR=~/Pictures cargo test --release tiff_meta_survey -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_TIFF_DIR"]
    fn tiff_meta_survey() {
        let Ok(root) = std::env::var("GEO_REF_MAPPER_TIFF_DIR") else {
            return;
        };
        let mut files = Vec::new();
        collect(std::path::Path::new(&root), &mut files);
        let (mut with, mut without) = (0, 0);
        for path in files.iter().take(4000) {
            match super::from_path(path) {
                Some(exif) if exif.software.is_some() || exif.description.is_some() => {
                    if with < 3 {
                        println!(
                            "{}\n   software: {:?}\n   description: {}\n",
                            path.display(),
                            exif.software,
                            exif.description
                                .as_deref()
                                .map(|d| d.chars().take(90).collect::<String>())
                                .unwrap_or_default(),
                        );
                    }
                    with += 1;
                }
                _ => without += 1,
            }
        }
        println!("{with} TIFFs carry software/description, {without} do not");
    }

    fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        if out.len() >= 4000 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, out);
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
            {
                out.push(path);
            }
        }
    }
}
