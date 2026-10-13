//! Where a scanned map sits on the earth.
//!
//! GeoTIFF stores position in three parts, all in IFD0 alongside the ordinary
//! tags:
//!
//!   * **ModelTiepoint** (33922) — a raster point and the world point it
//!     corresponds to, normally pixel `(0,0)` and the top-left corner
//!   * **ModelPixelScale** (33550) — how much ground one pixel covers
//!   * **GeoKeyDirectory** (34735) — which coordinate system those numbers are
//!     in, as an EPSG code, with names in **GeoAsciiParams** (34737)
//!
//! There is no rotation in this model: it assumes north-up and axis-aligned.
//! A rotated or skewed raster uses `ModelTransformation` (34264) instead, which
//! is noted when present rather than silently ignored.
//!
//! The tags are read by seeking to the directory rather than from a prefix,
//! for the same reason the text tags are: a TIFF's directory is routinely at
//! the *end* of the file.

use std::path::Path;

/// EPSG:3857, the projection OpenStreetMap and every other web map uses.
pub const WEB_MERCATOR: u16 = 3857;
/// EPSG:4326, plain latitude and longitude in degrees.
pub const WGS84: u16 = 4326;

/// Half the circumference of the earth at the equator, in metres — the extent
/// of the Web Mercator plane in each direction.
const MERCATOR_EDGE: f64 = 20_037_508.342_789_244;
const EARTH_RADIUS: f64 = 6_378_137.0;

#[derive(Clone, PartialEq, Debug)]
pub struct GeoRef {
    /// Projected coordinates of the raster's top-left corner.
    pub origin: (f64, f64),
    /// Ground covered by one pixel, in the projection's units.
    pub scale: (f64, f64),
    /// EPSG code of the coordinate system, when the file states one.
    pub epsg: Option<u16>,
    /// Human-readable names from `GeoAsciiParams`.
    ///
    /// A list, not a sentence: the tag holds several `|`-terminated strings —
    /// typically the projected system and the geographic one it is built on —
    /// and different geokeys point at different entries. They are kept apart
    /// rather than joined, because which is which is not something this reader
    /// establishes and running them together invented a relationship the file
    /// never stated.
    pub citations: Vec<String>,
    pub width: u32,
    pub height: u32,
    /// Present when the raster is rotated or skewed, which the simple
    /// tiepoint-and-scale model cannot describe.
    pub transformed: bool,
}

impl GeoRef {
    /// Metres of ground per pixel, when the units are metres.
    pub fn metres_per_pixel(&self) -> Option<f64> {
        // In Web Mercator the scale is metres, but distorted by latitude: a
        // metre on the plane is less than a metre on the ground away from the
        // equator. Corrected here so the figure means what it says.
        match self.epsg {
            Some(WEB_MERCATOR) => {
                let lat = self.centre()?.0.to_radians();
                Some(self.scale.0 * lat.cos())
            }
            Some(WGS84) => None, // degrees, not metres
            _ => Some(self.scale.0),
        }
    }

    /// The top-left corner in degrees.
    pub fn top_left(&self) -> Option<(f64, f64)> {
        self.to_degrees(self.origin)
    }

    /// The bottom-right corner in degrees.
    pub fn bottom_right(&self) -> Option<(f64, f64)> {
        self.to_degrees((
            self.origin.0 + self.scale.0 * f64::from(self.width),
            self.origin.1 - self.scale.1 * f64::from(self.height),
        ))
    }

    /// The middle of the raster in degrees.
    pub fn centre(&self) -> Option<(f64, f64)> {
        self.to_degrees((
            self.origin.0 + self.scale.0 * f64::from(self.width) / 2.0,
            self.origin.1 - self.scale.1 * f64::from(self.height) / 2.0,
        ))
    }

    /// How much ground the raster covers, in metres.
    pub fn extent_metres(&self) -> Option<(f64, f64)> {
        let per_pixel = self.metres_per_pixel()?;
        Some((
            per_pixel * f64::from(self.width),
            per_pixel * f64::from(self.height),
        ))
    }

    /// Projected coordinates to latitude and longitude.
    fn to_degrees(&self, point: (f64, f64)) -> Option<(f64, f64)> {
        match self.epsg? {
            WEB_MERCATOR => {
                let lon = point.0 / MERCATOR_EDGE * 180.0;
                let lat = (point.1 / EARTH_RADIUS).sinh().atan().to_degrees();
                Some((lat, lon))
            }
            // Already degrees, stored longitude-first like every other model
            // point.
            WGS84 => Some((point.1, point.0)),
            _ => None,
        }
    }
}

/// The tags that say where a picture sits on the earth, exactly as stored.
///
/// Read as values rather than interpreted, because they are being copied into a
/// converted file rather than used. Converting keeps every pixel in the same
/// place — same corner, same size — so the numbers carry over untouched, and a
/// converted map is as georeferenced as the one it came from.
///
/// The pyramid layers deliberately get none of this. A smaller copy has a
/// different pixel scale, and stating the original's would place it wrongly;
/// every tool that writes these puts them on the full-size layer alone.
pub fn position_tags(path: &Path) -> Vec<(u16, crate::tiffwrite::Value)> {
    use crate::tiffwrite::Value;

    let mut out = Vec::new();
    let Some(raw) = read_raw_tags(path) else {
        return out;
    };
    for (tag, kind, bytes) in raw {
        let value = match kind {
            // DOUBLE
            12 => Value::Doubles(
                bytes
                    .chunks_exact(8)
                    .map(|chunk| {
                        f64::from_le_bytes(chunk.try_into().expect("chunks_exact gives eight"))
                    })
                    .collect(),
            ),
            // SHORT
            3 => Value::Shorts(
                bytes
                    .chunks_exact(2)
                    .map(|chunk| u16::from_le_bytes(chunk.try_into().expect("two")))
                    .collect(),
            ),
            // ASCII
            2 => Value::Ascii(
                String::from_utf8_lossy(&bytes)
                    .trim_end_matches('\0')
                    .to_owned(),
            ),
            _ => continue,
        };
        out.push((tag, value));
    }
    out
}

/// A file's position exactly as it is stored, before anything is made of it.
///
/// [`GeoRef`] reduces a turned placement to a corner and a pixel size, which is
/// all the drawing needs and throws the rotation away. Working back from a
/// placement to the pixel a marked place sits on needs the whole of it.
pub struct RawPosition {
    /// `ModelTransformation` (34264), for a raster that is turned.
    pub transformation: Option<Vec<f64>>,
    /// `ModelTiepoint` (33922), for one that is square to north.
    pub tiepoint: Option<Vec<f64>>,
    /// `ModelPixelScale` (33550), which goes with the tiepoint.
    pub pixel_scale: Option<Vec<f64>>,
    pub width: u32,
    pub height: u32,
}

/// Reads it, or `None` when the file says nothing about where it is.
pub fn raw_position_tags(path: &Path) -> Option<RawPosition> {
    let geo = from_path(path)?;
    // `raw_tag` has already undone the file's byte order, so the bytes are
    // little-endian whatever the file was.
    let read = |tag: u16| {
        raw_tag(path, tag).map(|(_, raw)| {
            raw.chunks_exact(8)
                .map(|chunk| f64::from_le_bytes(chunk.try_into().unwrap_or_default()))
                .collect::<Vec<f64>>()
        })
    };
    Some(RawPosition {
        transformation: read(34264),
        tiepoint: read(33922),
        pixel_scale: read(33550),
        width: geo.width,
        height: geo.height,
    })
}

/// The placement a file is holding, read back out of its tags.
///
/// Both spellings: a map placed square keeps a tiepoint and a pixel size, a
/// turned one a full transformation. The second is the one that carries a
/// rotation, and reading only the first would put every turned map back at no
/// turn at all — in the wrong place, and plausibly so.
pub fn placement_of(path: &Path) -> Option<(crate::anchor::Placement, u32, u32)> {
    let raw = raw_position_tags(path)?;
    let (width, height) = (raw.width, raw.height);

    if let Some(matrix) = &raw.transformation
        && matrix.len() >= 8
    {
        // Row one is how far east a step across and a step down each move; row
        // two the same going north. The corner is the fourth column.
        let (a, b) = (matrix[0], matrix[4]);
        let scale = a.hypot(b);
        if scale <= 0.0 {
            return None;
        }
        return Some((
            crate::anchor::Placement {
                scale,
                rotation: b.atan2(a),
                origin: (matrix[3], matrix[7]),
            },
            width,
            height,
        ));
    }

    let tiepoint = raw.tiepoint?;
    let scale = raw.pixel_scale?;
    if tiepoint.len() < 6 || scale.is_empty() || scale[0] <= 0.0 {
        return None;
    }
    Some((
        crate::anchor::Placement {
            scale: scale[0],
            rotation: 0.0,
            origin: (tiepoint[3], tiepoint[4]),
        },
        width,
        height,
    ))
}

/// The five position tags, as (tag, type, bytes), in whatever order they are
/// found. Little-endian only on the way out — the reader has already undone the
/// file's own byte order.
pub(crate) fn read_raw_tags(path: &Path) -> Option<Vec<(u16, u16, Vec<u8>)>> {
    const WANTED: [u16; 5] = [33550, 33922, 34264, 34735, 34737];
    let geo = from_path(path)?;
    let _ = geo; // only to confirm the file has any of this at all

    let mut out = Vec::new();
    for tag in WANTED {
        if let Some((kind, bytes)) = raw_tag(path, tag) {
            out.push((tag, kind, bytes));
        }
    }
    Some(out)
}

/// Reads the georeferencing out of a TIFF, if it has any.
pub fn from_path(path: &Path) -> Option<GeoRef> {
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
                value = (value << 8) | u64::from(*byte);
            }
        } else {
            for byte in raw {
                value = (value << 8) | u64::from(*byte);
            }
        }
        value
    };

    let (big, first) = match int(&header[2..4]) {
        42 => (false, int(&header[4..8])),
        43 => (true, int(&header[8..16])),
        _ => return None,
    };
    let width_of_offset = if big { 8 } else { 4 };
    let entry_size = if big { 20 } else { 12 };

    file.seek(SeekFrom::Start(first)).ok()?;
    let mut raw_count = vec![0_u8; if big { 8 } else { 2 }];
    file.read_exact(&mut raw_count).ok()?;
    let count = int(&raw_count) as usize;
    if count == 0 || count > 4096 {
        return None;
    }
    let mut table = vec![0_u8; count * entry_size];
    file.read_exact(&mut table).ok()?;

    let sizes = |kind: u16| -> usize {
        match kind {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 => 4,
            5 | 10 | 12 | 16 | 17 => 8,
            _ => 1,
        }
    };
    let mut read_tag = |tag_wanted: u16| -> Option<(u16, Vec<u8>)> {
        for index in 0..count {
            let entry = &table[index * entry_size..(index + 1) * entry_size];
            let tag = int(&entry[0..2]) as u16;
            if tag != tag_wanted {
                continue;
            }
            let kind = int(&entry[2..4]) as u16;
            let items = int(&entry[4..4 + width_of_offset]) as usize;
            let total = items.checked_mul(sizes(kind))?;
            // A directory claiming a colossal array is corrupt, not a map.
            if total > 8 * 1024 * 1024 {
                return None;
            }
            let field = &entry[4 + width_of_offset..4 + width_of_offset * 2];
            let mut raw = vec![0_u8; total];
            if total <= width_of_offset {
                raw.copy_from_slice(&field[..total]);
            } else {
                file.seek(SeekFrom::Start(int(field))).ok()?;
                file.read_exact(&mut raw).ok()?;
            }
            return Some((kind, raw));
        }
        None
    };

    let doubles = |raw: &[u8]| -> Vec<f64> {
        raw.chunks_exact(8)
            .map(|chunk| {
                let bits = int(chunk);
                f64::from_bits(if little {
                    bits
                } else {
                    // `int` already handled the byte order, so the bits are in
                    // native order either way.
                    bits
                })
            })
            .collect()
    };

    // Two ways of saying the same thing. A north-up map states a corner and a
    // pixel size; a turned one states a full transformation instead, and cannot
    // use the simple form at all — so a reader that only understands the simple
    // form silently decides a turned map has no position.
    let transform = read_tag(34264).map(|(_, raw)| doubles(&raw));
    let (tiepoint, scale) = match &transform {
        Some(matrix) if matrix.len() >= 8 => {
            // Row one is how far east a step across and a step down each move;
            // row two the same going north. The corner is the fourth column.
            let across = (matrix[0] * matrix[0] + matrix[4] * matrix[4]).sqrt();
            let down = (matrix[1] * matrix[1] + matrix[5] * matrix[5]).sqrt();
            (
                vec![0.0, 0.0, 0.0, matrix[3], matrix[7], 0.0],
                vec![across, down],
            )
        }
        _ => {
            let tiepoint = doubles(&read_tag(33922)?.1);
            let scale = doubles(&read_tag(33550)?.1);
            (tiepoint, scale)
        }
    };
    if tiepoint.len() < 6 || scale.len() < 2 {
        return None;
    }

    let width = read_tag(256).map(|(_, raw)| int(&raw) as u32)?;
    let height = read_tag(257).map(|(_, raw)| int(&raw) as u32)?;

    // The key directory is an array of shorts, four to a key: id, where the
    // value lives, how many, and the value itself.
    let mut epsg = None;
    if let Some((_, raw)) = read_tag(34735) {
        let keys: Vec<u16> = raw.chunks_exact(2).map(|chunk| int(chunk) as u16).collect();
        for key in keys.chunks_exact(4).skip(1) {
            match key[0] {
                // Projected system, then geographic as a fallback.
                3072 => epsg = Some(key[3]),
                2048 if epsg.is_none() => epsg = Some(key[3]),
                _ => {}
            }
        }
    }
    let citations = read_tag(34737)
        .map(|(_, raw)| {
            String::from_utf8_lossy(&raw)
                .split('|')
                .map(|part| part.trim_matches(['\0', ' ']).to_owned())
                .filter(|part| !part.is_empty())
                .collect()
        })
        .unwrap_or_default();

    Some(GeoRef {
        origin: (tiepoint[3], tiepoint[4]),
        scale: (scale[0], scale[1]),
        epsg,
        citations,
        width,
        height,
        transformed: transform.is_some(),
    })
}

/// One tag's type and bytes, in little-endian order whatever the file used.
///
/// Not only about position: this is the one place that walks a directory and
/// follows an entry to wherever its value actually lives, which is what reading
/// any tag needs — the description and the crop written by `sidecar` included.
pub(crate) fn raw_tag(path: &Path, wanted: u16) -> Option<(u16, Vec<u8>)> {
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
                value = (value << 8) | u64::from(*byte);
            }
        } else {
            for byte in raw {
                value = (value << 8) | u64::from(*byte);
            }
        }
        value
    };

    let (big, first) = match int(&header[2..4]) {
        42 => (false, int(&header[4..8])),
        43 => (true, int(&header[8..16])),
        _ => return None,
    };
    let width_of_offset = if big { 8 } else { 4 };
    let entry_size = if big { 20 } else { 12 };

    file.seek(SeekFrom::Start(first)).ok()?;
    let mut raw_count = vec![0_u8; if big { 8 } else { 2 }];
    file.read_exact(&mut raw_count).ok()?;
    let count = int(&raw_count) as usize;
    if count == 0 || count > 4096 {
        return None;
    }
    let mut table = vec![0_u8; count * entry_size];
    file.read_exact(&mut table).ok()?;

    let size_of = |kind: u16| -> usize {
        match kind {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 => 4,
            5 | 10 | 12 | 16 | 17 => 8,
            _ => 1,
        }
    };

    for index in 0..count {
        let entry = &table[index * entry_size..(index + 1) * entry_size];
        if int(&entry[0..2]) as u16 != wanted {
            continue;
        }
        let kind = int(&entry[2..4]) as u16;
        let items = int(&entry[4..4 + width_of_offset]) as usize;
        let total = items.checked_mul(size_of(kind))?;
        if total > 8 * 1024 * 1024 {
            return None;
        }
        let field = &entry[4 + width_of_offset..4 + width_of_offset * 2];
        let mut raw = vec![0_u8; total];
        if total <= width_of_offset {
            raw.copy_from_slice(&field[..total]);
        } else {
            file.seek(SeekFrom::Start(int(field))).ok()?;
            file.read_exact(&mut raw).ok()?;
        }

        // A big-endian file stores each value the other way round; the values
        // are re-emitted little-endian so the writer can copy them straight in.
        if !little {
            let unit = size_of(kind);
            if unit > 1 {
                for chunk in raw.chunks_exact_mut(unit) {
                    chunk.reverse();
                }
            }
        }
        return Some((kind, raw));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real numbers out of `MAPS/NUS libmaps/1846.tif`.
    fn singapore() -> GeoRef {
        GeoRef {
            origin: (11_554_603.380_728_67, 148_440.708_929_810_67),
            scale: (0.298_582_141_738_918_4, 0.298_582_141_738_965_2),
            epsg: Some(WEB_MERCATOR),
            citations: vec!["WGS 84 / Pseudo-Mercator".to_owned(), "WGS 84".to_owned()],
            width: 40448,
            height: 32256,
            transformed: false,
        }
    }

    #[test]
    fn the_citation_stays_a_list_rather_than_becoming_a_sentence() {
        let geo = singapore();
        assert_eq!(geo.citations.len(), 2);
        assert_eq!(geo.citations[0], "WGS 84 / Pseudo-Mercator");
        assert_eq!(geo.citations[1], "WGS 84", "the datum is its own entry");
    }

    #[test]
    fn the_corner_lands_on_singapore() {
        let (lat, lon) = singapore().top_left().expect("web mercator converts");
        assert!((lon - 103.788).abs() < 0.01, "longitude came out at {lon}",);
        assert!((lat - 1.333).abs() < 0.01, "latitude came out at {lat}");
    }

    #[test]
    fn the_map_covers_about_twelve_kilometres() {
        let (across, down) = singapore().extent_metres().expect("metres are known");
        // 40448 px at ~0.2986 m, less the Mercator stretch at this latitude.
        assert!(
            (12_000.0..12_200.0).contains(&across),
            "across came out at {across} m",
        );
        assert!(
            (9_500.0..9_700.0).contains(&down),
            "down came out at {down} m",
        );
    }

    #[test]
    fn mercator_scale_is_corrected_for_latitude() {
        // On the plane a pixel is 0.29858; on the ground near the equator that
        // is almost the same, but the correction must be applied rather than
        // the plane figure reported as ground truth.
        let metres = singapore().metres_per_pixel().unwrap();
        assert!(metres < 0.298_582, "must be smaller than the plane scale");
        assert!(
            metres > 0.298,
            "and only slightly, this close to the equator"
        );
    }

    #[test]
    fn a_bottom_right_corner_is_south_and_east_of_the_top_left() {
        let geo = singapore();
        let (top, left) = geo.top_left().unwrap();
        let (bottom, right) = geo.bottom_right().unwrap();
        assert!(bottom < top, "y grows northwards in the projection");
        assert!(right > left);
    }

    #[test]
    fn plain_degrees_are_passed_through_rather_than_projected() {
        let geo = GeoRef {
            origin: (103.8, 1.35),
            scale: (0.0001, 0.0001),
            epsg: Some(WGS84),
            citations: Vec::new(),
            width: 1000,
            height: 1000,
            transformed: false,
        };
        let (lat, lon) = geo.top_left().unwrap();
        assert!((lat - 1.35).abs() < 1e-9 && (lon - 103.8).abs() < 1e-9);
        assert_eq!(
            geo.metres_per_pixel(),
            None,
            "degrees are not metres, and saying so beats inventing a figure",
        );
    }

    #[test]
    fn an_unknown_projection_reports_no_position_rather_than_a_wrong_one() {
        let geo = GeoRef {
            epsg: Some(27700), // British National Grid
            ..singapore()
        };
        assert_eq!(geo.top_left(), None);
    }
}
