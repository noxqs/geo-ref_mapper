//! HEIF / HEIC container reading — the structure, not the pixels.
//!
//! An iPhone `.HEIC` is not one coded image. The primary item is usually a
//! **grid**: a derived item that says "compose these 48 HEVC tiles into a
//! 3024 x 4032 picture". Alongside it sit a thumbnail item, an auxiliary image
//! or two (depth, alpha), an `Exif` item, XMP, and on newer phones a second
//! grid holding an HDR gain map plus a `tmap` item tying them together.
//!
//! This parses all of that without decoding anything, which is enough to index
//! a library: real dimensions, rotation, bit depth, where the EXIF lives, and
//! which item to decode for a thumbnail.
//!
//! Doing it ourselves rather than leaning on libheif is not pride. The system
//! libheif (1.17.6) refuses every gain-map file outright — "too many auxiliary
//! image references" — and that is 2917 of the 9468 HEIF files in this library.
//! Parsing the container here means the decoder only ever sees a single coded
//! item and never has to agree with us about the rest.

use std::collections::HashMap;

/// A box header: what it is and where its payload lives.
struct BoxHeader {
    kind: [u8; 4],
    /// Start of the payload, past the size and type.
    body: usize,
    /// One past the last byte of the whole box.
    end: usize,
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// Reads a big-endian integer of `size` bytes, as ISOBMFF uses for the variable
/// width offset and length fields in `iloc`.
fn read_sized(bytes: &[u8], at: usize, size: usize) -> Option<u64> {
    match size {
        0 => Some(0),
        4 => read_u32(bytes, at).map(u64::from),
        8 => read_u64(bytes, at),
        2 => read_u16(bytes, at).map(u64::from),
        _ => None,
    }
}

/// Walks the boxes between `from` and `to`, calling `visit` for each.
fn each_box(bytes: &[u8], from: usize, to: usize, mut visit: impl FnMut(&BoxHeader)) {
    let mut offset = from;
    // A malformed size of zero would otherwise spin here for ever.
    while offset + 8 <= to {
        let Some(size32) = read_u32(bytes, offset) else {
            return;
        };
        let kind: [u8; 4] = match bytes
            .get(offset + 4..offset + 8)
            .and_then(|k| k.try_into().ok())
        {
            Some(kind) => kind,
            None => return,
        };
        let (size, header) = match size32 {
            1 => match read_u64(bytes, offset + 8) {
                Some(large) => (large as usize, 16),
                None => return,
            },
            // Zero means "to the end of the enclosing box".
            0 => (to - offset, 8),
            other => (other as usize, 8),
        };
        if size < header || offset + size > to {
            return;
        }
        visit(&BoxHeader {
            kind,
            body: offset + header,
            end: offset + size,
        });
        offset += size;
    }
}

/// Where an item's bytes are. `in_item_data` means they live in the `idat` box
/// rather than at a file offset — how grid descriptions are usually stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extent {
    pub offset: u64,
    pub length: u64,
    pub in_item_data: bool,
}

/// How the camera said to turn the picture. HEIF uses `irot` and `imir` rather
/// than EXIF's single orientation tag, and when both are present the file's own
/// transform is what viewers are expected to honour.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Transform {
    /// Counter-clockwise, one of 0, 90, 180, 270.
    pub rotation_ccw: u16,
    pub mirror_horizontal: bool,
    pub mirror_vertical: bool,
}

// Much of what this module extracts is consumed by the HEVC decoding pass
// rather than by the container reading itself: item extents, `hvcC` records and
// tile ids exist so a decoder can be handed one coded item at a time. They are
// parsed and tested here because getting them right is the hard part, and
// because doing so is what lets us read the files libheif refuses.
#[allow(dead_code)]
impl Transform {
    pub fn swaps_axes(self) -> bool {
        self.rotation_ccw == 90 || self.rotation_ccw == 270
    }

    /// The equivalent EXIF orientation, so the rest of the viewer has one way
    /// of talking about this rather than two.
    pub fn as_exif(self) -> crate::exif::Orientation {
        use crate::exif::Orientation as O;
        match (
            self.rotation_ccw,
            self.mirror_horizontal,
            self.mirror_vertical,
        ) {
            (90, false, false) => O::Rotate270,
            (180, false, false) => O::Rotate180,
            (270, false, false) => O::Rotate90,
            (0, true, false) => O::FlipHorizontal,
            (0, false, true) => O::FlipVertical,
            (90, true, false) | (270, false, true) => O::Transpose,
            (270, true, false) | (90, false, true) => O::Transverse,
            _ => O::Normal,
        }
    }
}

/// A derived image assembled from a lattice of coded tiles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grid {
    pub columns: u32,
    pub rows: u32,
    pub width: u32,
    pub height: u32,
    /// Tile item ids, in row-major order.
    pub tiles: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Item {
    pub kind: [u8; 4],
    pub extents: Vec<Extent>,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    /// `auxC` auxiliary type URN, when this is a depth map, alpha or gain map.
    pub auxiliary: Option<String>,
    pub transform: Transform,
    /// Decoder configuration record, needed to decode this item's bitstream.
    pub hvcc: Option<Vec<u8>>,
}

#[allow(dead_code)]
impl Item {
    pub fn kind_str(&self) -> String {
        self.kind.iter().map(|byte| *byte as char).collect()
    }
}

/// What the file turned out to be.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Heif {
    pub major_brand: String,
    pub compatible_brands: Vec<String>,
    pub primary: u32,
    pub items: HashMap<u32, Item>,
    /// Displayed size, after any grid composition but before rotation.
    pub width: u32,
    pub height: u32,
    pub transform: Transform,
    pub bit_depth: u8,
    /// Set when the primary item is a grid rather than a single coded image.
    pub grid: Option<Grid>,
    /// Item id of the embedded thumbnail, which is far cheaper than the primary.
    pub thumbnail: Option<u32>,
    /// Where the EXIF payload sits, ready to hand to [`crate::exif`].
    pub exif: Option<Extent>,
    pub has_gain_map: bool,
    pub has_depth: bool,
    pub has_alpha: bool,
    /// True when the coded items are HEVC. AVIF uses the same container with
    /// AV1 instead, which is why this is worth stating rather than assuming.
    pub is_hevc: bool,
    /// Byte range of the `idat` box, where grid descriptions and occasionally
    /// item data live. Offsets are from the start of the file, so they stay
    /// valid when the decoder later reads the whole thing.
    pub idat: Option<(u64, u64)>,
}

#[allow(dead_code)]
impl Heif {
    /// Size as it should be displayed, with the transform applied.
    pub fn display_size(&self) -> (u32, u32) {
        if self.transform.swaps_axes() {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    /// How many coded tiles the primary image is made of. One means a plain
    /// coded image rather than a grid.
    pub fn tile_count(&self) -> usize {
        self.grid.as_ref().map_or(1, |grid| grid.tiles.len())
    }

    /// A short description for the badge over the canvas.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(grid) = &self.grid {
            parts.push(format!("{}×{} grid", grid.columns, grid.rows));
            parts.push(format!("{} tiles", grid.tiles.len()));
        } else {
            parts.push("single tile".to_owned());
        }
        if self.bit_depth > 8 {
            parts.push(format!("{}-bit", self.bit_depth));
        }
        if self.has_gain_map {
            parts.push("HDR gain map".to_owned());
        }
        if self.has_depth {
            parts.push("depth".to_owned());
        }
        if self.has_alpha {
            parts.push("alpha".to_owned());
        }
        if self.thumbnail.is_some() {
            parts.push("thumb".to_owned());
        }
        parts.join(" · ")
    }
}

/// True when these bytes begin a HEIF-family file we would try to read.
pub fn is_heif(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[4..8] == b"ftyp"
}

/// Ceiling on the metadata read, in case a file claims a `meta` box far larger
/// than any real one.
const MAX_METADATA: u64 = 8 * 1024 * 1024;

/// Reads only the boxes that carry structure, which is a small fraction of the
/// file.
///
/// Worth the care: a blind 512 KB prefix per file cost 23.6 ms each when the
/// library lives on a slow external drive, which is over three minutes to index
/// ten thousand photos. Every box announces its own length, so walking the
/// headers and reading exactly `ftyp` + `meta` is typically 30-60 KB instead.
pub fn from_path(path: &std::path::Path) -> Option<Heif> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0_u8; 16];
    file.read_exact(&mut header).ok()?;
    if !is_heif(&header) {
        return None;
    }

    // Walk the top-level box headers until `meta` is found, reading 16 bytes at
    // a time rather than the boxes themselves.
    let mut offset = 0_u64;
    let mut wanted = 0_u64;
    for _ in 0..16 {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut head = [0_u8; 16];
        if file.read(&mut head).ok()? < 8 {
            return None;
        }
        let size32 = read_u32(&head, 0)?;
        let size = if size32 == 1 {
            read_u64(&head, 8)?
        } else {
            u64::from(size32)
        };
        if size < 8 {
            return None;
        }
        if &head[4..8] == b"meta" {
            wanted = offset.checked_add(size)?;
            break;
        }
        // `mdat` means the pixel data started and no `meta` preceded it.
        if &head[4..8] == b"mdat" {
            return None;
        }
        offset = offset.checked_add(size)?;
    }
    if wanted == 0 || wanted > MAX_METADATA {
        return None;
    }

    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = vec![0_u8; wanted as usize];
    // A short read is fine: the parser is written to survive truncation.
    let mut filled = 0;
    while filled < bytes.len() {
        match file.read(&mut bytes[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    bytes.truncate(filled);
    parse(&bytes)
}

/// Parses the container. `bytes` needs to cover the metadata, not the whole
/// file — `meta` sits before `mdat` in every file Apple writes.
pub fn parse(bytes: &[u8]) -> Option<Heif> {
    if !is_heif(bytes) {
        return None;
    }

    let mut major_brand = String::new();
    let mut compatible_brands = Vec::new();
    let mut meta: Option<(usize, usize)> = None;

    each_box(bytes, 0, bytes.len(), |header| match &header.kind {
        b"ftyp" => {
            major_brand = fourcc(bytes, header.body);
            let mut at = header.body + 8;
            while at + 4 <= header.end {
                compatible_brands.push(fourcc(bytes, at));
                at += 4;
            }
        }
        // `meta` is a FullBox, so its contents start four bytes in.
        b"meta" => meta = Some((header.body + 4, header.end)),
        _ => {}
    });

    let (meta_start, meta_end) = meta?;
    let mut primary = 0_u32;
    let mut items: HashMap<u32, Item> = HashMap::new();
    let mut properties: Vec<Property> = Vec::new();
    let mut associations: Vec<(u32, Vec<u16>)> = Vec::new();
    let mut references: Vec<(String, u32, Vec<u32>)> = Vec::new();
    let mut item_data: Option<(usize, usize)> = None;

    each_box(bytes, meta_start, meta_end, |header| match &header.kind {
        b"pitm" => {
            let version = bytes.get(header.body).copied().unwrap_or(0);
            primary = if version == 0 {
                read_u16(bytes, header.body + 4).unwrap_or(0) as u32
            } else {
                read_u32(bytes, header.body + 4).unwrap_or(0)
            };
        }
        b"iinf" => parse_iinf(bytes, header, &mut items),
        b"iloc" => parse_iloc(bytes, header, &mut items),
        b"iref" => parse_iref(bytes, header, &mut references),
        b"iprp" => each_box(bytes, header.body, header.end, |inner| match &inner.kind {
            b"ipco" => parse_ipco(bytes, inner, &mut properties),
            b"ipma" => parse_ipma(bytes, inner, &mut associations),
            _ => {}
        }),
        b"idat" => item_data = Some((header.body, header.end)),
        _ => {}
    });

    // Attach each property to the items that claim it. Association indices are
    // one-based, and index zero means "no property".
    for (item_id, indices) in &associations {
        let Some(item) = items.get_mut(item_id) else {
            continue;
        };
        for index in indices {
            let Some(property) = index
                .checked_sub(1)
                .and_then(|i| properties.get(i as usize))
            else {
                continue;
            };
            match property {
                Property::Size { width, height } => {
                    item.width = *width;
                    item.height = *height;
                }
                Property::Rotation(quarters) => {
                    item.transform.rotation_ccw = (*quarters as u16 % 4) * 90;
                }
                Property::Mirror { vertical } => {
                    if *vertical {
                        item.transform.mirror_vertical = true;
                    } else {
                        item.transform.mirror_horizontal = true;
                    }
                }
                Property::BitDepth(depth) => item.bit_depth = *depth,
                Property::Auxiliary(urn) => item.auxiliary = Some(urn.clone()),
                Property::Hvcc(config) => item.hvcc = Some(config.clone()),
            }
        }
    }

    let primary_item = items.get(&primary)?.clone();

    // A grid item's payload describes the lattice; the tiles themselves come
    // from its `dimg` references, in order.
    let grid = if &primary_item.kind == b"grid" {
        let payload = extent_bytes(bytes, primary_item.extents.first()?, item_data)?;
        let tiles = references
            .iter()
            .find(|(kind, from, _)| kind == "dimg" && *from == primary)
            .map(|(_, _, to)| to.clone())
            .unwrap_or_default();
        parse_grid(payload, tiles)
    } else {
        None
    };

    let thumbnail = references
        .iter()
        .find(|(kind, _, to)| kind == "thmb" && to.contains(&primary))
        .map(|(_, from, _)| *from);

    let exif = references
        .iter()
        .filter(|(kind, _, to)| kind == "cdsc" && to.contains(&primary))
        .find_map(|(_, from, _)| {
            let item = items.get(from)?;
            (&item.kind == b"Exif").then(|| item.extents.first().cloned())?
        });

    // Auxiliary images announce what they are in a URN. Apple's depth maps and
    // alpha use the standard ones; the HDR gain map arrives as a `tmap` item.
    let mut has_depth = false;
    let mut has_alpha = false;
    let mut aux_gain_map = false;
    for item in items.values() {
        if let Some(urn) = &item.auxiliary {
            if urn.contains("depth") || urn.contains("disparity") {
                has_depth = true;
            }
            if urn.contains("alpha") {
                has_alpha = true;
            }
            // Phones before the `tmap` era carried the gain map as a plain
            // auxiliary image: `urn:com:apple:photo:2020:aux:hdrgainmap`.
            if urn.contains("hdrgainmap") || urn.contains("gainmap") {
                aux_gain_map = true;
            }
        }
    }
    let has_gain_map = aux_gain_map
        || items.values().any(|item| &item.kind == b"tmap")
        || compatible_brands.iter().any(|brand| brand == "tmap");

    // Grid dimensions win: the primary item's own `ispe` describes the derived
    // image, but a malformed one should not beat what the grid actually says.
    let (width, height) = match &grid {
        Some(grid) if grid.width > 0 => (grid.width, grid.height),
        _ => (primary_item.width, primary_item.height),
    };

    let bit_depth = if primary_item.bit_depth > 0 {
        primary_item.bit_depth
    } else {
        grid.as_ref()
            .and_then(|grid| grid.tiles.first())
            .and_then(|id| items.get(id))
            .map(|tile| tile.bit_depth)
            .filter(|depth| *depth > 0)
            .unwrap_or(8)
    };

    let is_hevc = items
        .values()
        .any(|item| &item.kind == b"hvc1" || &item.kind == b"hev1");

    Some(Heif {
        major_brand,
        compatible_brands,
        primary,
        width,
        height,
        transform: primary_item.transform,
        bit_depth,
        grid,
        thumbnail,
        exif,
        has_gain_map,
        has_depth,
        has_alpha,
        is_hevc,
        idat: item_data.map(|(start, end)| (start as u64, end as u64)),
        items,
    })
}

fn fourcc(bytes: &[u8], at: usize) -> String {
    bytes
        .get(at..at + 4)
        .map(|slice| slice.iter().map(|byte| *byte as char).collect())
        .unwrap_or_default()
}

enum Property {
    Size { width: u32, height: u32 },
    Rotation(u8),
    Mirror { vertical: bool },
    BitDepth(u8),
    Auxiliary(String),
    Hvcc(Vec<u8>),
}

fn parse_ipco(bytes: &[u8], header: &BoxHeader, out: &mut Vec<Property>) {
    each_box(bytes, header.body, header.end, |property| {
        // Order matters: `ipma` refers to these by position, so an unrecognised
        // property still has to occupy its slot.
        let parsed = match &property.kind {
            b"ispe" => Some(Property::Size {
                width: read_u32(bytes, property.body + 4).unwrap_or(0),
                height: read_u32(bytes, property.body + 8).unwrap_or(0),
            }),
            b"irot" => Some(Property::Rotation(
                bytes.get(property.body).copied().unwrap_or(0) & 0b11,
            )),
            b"imir" => Some(Property::Mirror {
                vertical: bytes.get(property.body).copied().unwrap_or(0) & 1 == 0,
            }),
            b"pixi" => Some(Property::BitDepth(
                bytes.get(property.body + 5).copied().unwrap_or(0),
            )),
            b"auxC" => {
                let start = property.body + 4;
                let text: String = bytes
                    .get(start..property.end)
                    .unwrap_or_default()
                    .iter()
                    .take_while(|byte| **byte != 0)
                    .map(|byte| *byte as char)
                    .collect();
                Some(Property::Auxiliary(text))
            }
            b"hvcC" => Some(Property::Hvcc(
                bytes
                    .get(property.body..property.end)
                    .unwrap_or_default()
                    .to_vec(),
            )),
            _ => None,
        };
        // Push a placeholder for anything we skip so indices stay aligned.
        out.push(parsed.unwrap_or(Property::Rotation(0)));
    });
}

fn parse_ipma(bytes: &[u8], header: &BoxHeader, out: &mut Vec<(u32, Vec<u16>)>) {
    let version = bytes.get(header.body).copied().unwrap_or(0);
    let flags = read_u32(bytes, header.body).unwrap_or(0) & 0x00FF_FFFF;
    let wide_indices = flags & 1 == 1;

    let mut at = header.body + 4;
    let Some(count) = read_u32(bytes, at) else {
        return;
    };
    at += 4;

    for _ in 0..count {
        let item_id = if version < 1 {
            let Some(id) = read_u16(bytes, at) else {
                return;
            };
            at += 2;
            id as u32
        } else {
            let Some(id) = read_u32(bytes, at) else {
                return;
            };
            at += 4;
            id
        };
        let Some(association_count) = bytes.get(at).copied() else {
            return;
        };
        at += 1;

        let mut indices = Vec::with_capacity(association_count as usize);
        for _ in 0..association_count {
            if wide_indices {
                let Some(value) = read_u16(bytes, at) else {
                    return;
                };
                at += 2;
                indices.push(value & 0x7FFF);
            } else {
                let Some(value) = bytes.get(at).copied() else {
                    return;
                };
                at += 1;
                indices.push((value & 0x7F) as u16);
            }
        }
        out.push((item_id, indices));
    }
}

fn parse_iinf(bytes: &[u8], header: &BoxHeader, items: &mut HashMap<u32, Item>) {
    let version = bytes.get(header.body).copied().unwrap_or(0);
    let mut at = header.body + 4;
    if version == 0 {
        at += 2;
    } else {
        at += 4;
    }

    each_box(bytes, at, header.end, |entry| {
        if &entry.kind != b"infe" {
            return;
        }
        let entry_version = bytes.get(entry.body).copied().unwrap_or(0);
        let mut cursor = entry.body + 4;
        let id = if entry_version < 3 {
            let Some(id) = read_u16(bytes, cursor) else {
                return;
            };
            cursor += 2;
            id as u32
        } else {
            let Some(id) = read_u32(bytes, cursor) else {
                return;
            };
            cursor += 4;
            id
        };
        cursor += 2; // protection index
        let Some(kind) = bytes
            .get(cursor..cursor + 4)
            .and_then(|k| k.try_into().ok())
        else {
            return;
        };
        items.entry(id).or_default().kind = kind;
    });
}

fn parse_iloc(bytes: &[u8], header: &BoxHeader, items: &mut HashMap<u32, Item>) {
    let version = bytes.get(header.body).copied().unwrap_or(0);
    let mut at = header.body + 4;

    let Some(sizes) = bytes.get(at).copied() else {
        return;
    };
    let offset_size = (sizes >> 4) as usize;
    let length_size = (sizes & 0xF) as usize;
    let Some(sizes2) = bytes.get(at + 1).copied() else {
        return;
    };
    let base_offset_size = (sizes2 >> 4) as usize;
    let index_size = if version >= 1 {
        (sizes2 & 0xF) as usize
    } else {
        0
    };
    at += 2;

    let count = if version < 2 {
        let Some(count) = read_u16(bytes, at) else {
            return;
        };
        at += 2;
        count as u32
    } else {
        let Some(count) = read_u32(bytes, at) else {
            return;
        };
        at += 4;
        count
    };

    for _ in 0..count {
        let id = if version < 2 {
            let Some(id) = read_u16(bytes, at) else {
                return;
            };
            at += 2;
            id as u32
        } else {
            let Some(id) = read_u32(bytes, at) else {
                return;
            };
            at += 4;
            id
        };

        // Construction method 1 means the bytes live in `idat`, which is how
        // grid descriptions are stored.
        let mut in_item_data = false;
        if version >= 1 {
            let Some(raw) = read_u16(bytes, at) else {
                return;
            };
            in_item_data = (raw & 0xF) == 1;
            at += 2;
        }
        at += 2; // data reference index

        let Some(base_offset) = read_sized(bytes, at, base_offset_size) else {
            return;
        };
        at += base_offset_size;

        let Some(extent_count) = read_u16(bytes, at) else {
            return;
        };
        at += 2;

        let mut extents = Vec::with_capacity(extent_count as usize);
        for _ in 0..extent_count {
            at += index_size;
            let Some(offset) = read_sized(bytes, at, offset_size) else {
                return;
            };
            at += offset_size;
            let Some(length) = read_sized(bytes, at, length_size) else {
                return;
            };
            at += length_size;
            extents.push(Extent {
                offset: base_offset.saturating_add(offset),
                length,
                in_item_data,
            });
        }
        items.entry(id).or_default().extents = extents;
    }
}

fn parse_iref(bytes: &[u8], header: &BoxHeader, out: &mut Vec<(String, u32, Vec<u32>)>) {
    let version = bytes.get(header.body).copied().unwrap_or(0);
    each_box(bytes, header.body + 4, header.end, |entry| {
        let mut at = entry.body;
        let from = if version == 0 {
            let Some(id) = read_u16(bytes, at) else {
                return;
            };
            at += 2;
            id as u32
        } else {
            let Some(id) = read_u32(bytes, at) else {
                return;
            };
            at += 4;
            id
        };
        let Some(count) = read_u16(bytes, at) else {
            return;
        };
        at += 2;

        let mut to = Vec::with_capacity(count as usize);
        for _ in 0..count {
            if version == 0 {
                let Some(id) = read_u16(bytes, at) else {
                    return;
                };
                at += 2;
                to.push(id as u32);
            } else {
                let Some(id) = read_u32(bytes, at) else {
                    return;
                };
                at += 4;
                to.push(id);
            }
        }
        out.push((
            entry.kind.iter().map(|byte| *byte as char).collect(),
            from,
            to,
        ));
    });
}

/// The payload of a `grid` item: a tiny header giving the lattice shape and the
/// composed output size.
fn parse_grid(payload: &[u8], tiles: Vec<u32>) -> Option<Grid> {
    let flags = *payload.get(1)?;
    let rows = u32::from(*payload.get(2)?) + 1;
    let columns = u32::from(*payload.get(3)?) + 1;

    // Bit 0 of the flags chooses 16- or 32-bit output dimensions.
    let (width, height) = if flags & 1 == 1 {
        (read_u32(payload, 4)?, read_u32(payload, 8)?)
    } else {
        (
            u32::from(read_u16(payload, 4)?),
            u32::from(read_u16(payload, 6)?),
        )
    };

    Some(Grid {
        columns,
        rows,
        width,
        height,
        tiles,
    })
}

/// Resolves an extent to the bytes it points at, following `idat` when the item
/// said its data lives there.
fn extent_bytes<'a>(
    bytes: &'a [u8],
    extent: &Extent,
    item_data: Option<(usize, usize)>,
) -> Option<&'a [u8]> {
    let (start, end) = if extent.in_item_data {
        let (idat_start, idat_end) = item_data?;
        let start = idat_start.checked_add(extent.offset as usize)?;
        (
            start,
            start.checked_add(extent.length as usize)?.min(idat_end),
        )
    } else {
        let start = extent.offset as usize;
        (start, start.checked_add(extent.length as usize)?)
    };
    bytes.get(start..end.min(bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but structurally real HEIF: `ftyp`, then a `meta` holding a
    /// primary item that is a 2x2 grid of four tiles, with a rotation and an
    /// `Exif` item hanging off it.
    fn synthetic_heif() -> Vec<u8> {
        fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            out
        }

        // ftyp: major brand heic, compatible mif1 / tmap.
        let mut ftyp_body = b"heic".to_vec();
        ftyp_body.extend_from_slice(&0_u32.to_be_bytes());
        ftyp_body.extend_from_slice(b"mif1");
        ftyp_body.extend_from_slice(b"tmap");
        let ftyp = boxed(b"ftyp", &ftyp_body);

        // pitm -> item 9
        let mut pitm = vec![0, 0, 0, 0];
        pitm.extend_from_slice(&9_u16.to_be_bytes());
        let pitm = boxed(b"pitm", &pitm);

        // iinf with five items: four tiles, the grid, and an Exif blob.
        let mut infes = Vec::new();
        for (id, kind) in [
            (1_u16, b"hvc1"),
            (2, b"hvc1"),
            (3, b"hvc1"),
            (4, b"hvc1"),
            (9, b"grid"),
            (20, b"Exif"),
        ] {
            let mut body = vec![2, 0, 0, 0]; // version 2
            body.extend_from_slice(&id.to_be_bytes());
            body.extend_from_slice(&0_u16.to_be_bytes());
            body.extend_from_slice(kind);
            infes.extend_from_slice(&boxed(b"infe", &body));
        }
        let mut iinf_body = vec![0, 0, 0, 0];
        iinf_body.extend_from_slice(&6_u16.to_be_bytes());
        iinf_body.extend_from_slice(&infes);
        let iinf = boxed(b"iinf", &iinf_body);

        // iref: dimg 9 -> 1,2,3,4 and cdsc 20 -> 9
        let mut dimg = 9_u16.to_be_bytes().to_vec();
        dimg.extend_from_slice(&4_u16.to_be_bytes());
        for tile in [1_u16, 2, 3, 4] {
            dimg.extend_from_slice(&tile.to_be_bytes());
        }
        let mut cdsc = 20_u16.to_be_bytes().to_vec();
        cdsc.extend_from_slice(&1_u16.to_be_bytes());
        cdsc.extend_from_slice(&9_u16.to_be_bytes());
        let mut iref_body = vec![0, 0, 0, 0];
        iref_body.extend_from_slice(&boxed(b"dimg", &dimg));
        iref_body.extend_from_slice(&boxed(b"cdsc", &cdsc));
        let iref = boxed(b"iref", &iref_body);

        // ipco: ispe (property 1), irot 270 ccw (property 2)
        let mut ispe = vec![0, 0, 0, 0];
        ispe.extend_from_slice(&1024_u32.to_be_bytes());
        ispe.extend_from_slice(&768_u32.to_be_bytes());
        let ipco_body = [boxed(b"ispe", &ispe), boxed(b"irot", &[3])].concat();
        let ipco = boxed(b"ipco", &ipco_body);

        // ipma: item 9 uses properties 1 and 2.
        let mut ipma_body = vec![0, 0, 0, 0];
        ipma_body.extend_from_slice(&1_u32.to_be_bytes());
        ipma_body.extend_from_slice(&9_u16.to_be_bytes());
        ipma_body.push(2);
        ipma_body.push(1);
        ipma_body.push(2);
        let ipma = boxed(b"ipma", &ipma_body);
        let iprp = boxed(b"iprp", &[ipco, ipma].concat());

        // The grid payload lives in idat: 2x2, 1024x768 output.
        let grid_payload = [0_u8, 0, 1, 1, 4, 0, 3, 0];
        let idat = boxed(b"idat", &grid_payload);

        // iloc v1: item 9 in idat at offset 0, item 20 at a file offset.
        let mut iloc_body = vec![1, 0, 0, 0];
        iloc_body.push(0x44); // 4-byte offsets and lengths
        iloc_body.push(0x00); // no base offset, no index
        iloc_body.extend_from_slice(&2_u16.to_be_bytes());
        // item 9, construction method 1 (idat)
        iloc_body.extend_from_slice(&9_u16.to_be_bytes());
        iloc_body.extend_from_slice(&1_u16.to_be_bytes());
        iloc_body.extend_from_slice(&0_u16.to_be_bytes());
        iloc_body.extend_from_slice(&1_u16.to_be_bytes());
        iloc_body.extend_from_slice(&0_u32.to_be_bytes());
        iloc_body.extend_from_slice(&(grid_payload.len() as u32).to_be_bytes());
        // item 20, construction method 0, pointing anywhere
        iloc_body.extend_from_slice(&20_u16.to_be_bytes());
        iloc_body.extend_from_slice(&0_u16.to_be_bytes());
        iloc_body.extend_from_slice(&0_u16.to_be_bytes());
        iloc_body.extend_from_slice(&1_u16.to_be_bytes());
        iloc_body.extend_from_slice(&4096_u32.to_be_bytes());
        iloc_body.extend_from_slice(&128_u32.to_be_bytes());
        let iloc = boxed(b"iloc", &iloc_body);

        let mut meta_body = vec![0, 0, 0, 0];
        for part in [pitm, iinf, iref, iprp, idat, iloc] {
            meta_body.extend_from_slice(&part);
        }
        let meta = boxed(b"meta", &meta_body);

        [ftyp, meta].concat()
    }

    #[test]
    fn a_grid_primary_reports_the_composed_size_and_its_tiles() {
        let heif = parse(&synthetic_heif()).expect("should parse");
        assert_eq!(heif.major_brand, "heic");
        assert_eq!(heif.primary, 9);

        let grid = heif.grid.as_ref().expect("primary is a grid");
        assert_eq!((grid.columns, grid.rows), (2, 2));
        assert_eq!(grid.tiles, vec![1, 2, 3, 4]);
        assert_eq!((grid.width, grid.height), (1024, 768));
        assert_eq!(heif.tile_count(), 4);
    }

    #[test]
    fn rotation_comes_from_irot_and_swaps_the_display_size() {
        let heif = parse(&synthetic_heif()).unwrap();
        // irot stored 3 quarter-turns counter-clockwise.
        assert_eq!(heif.transform.rotation_ccw, 270);
        assert!(heif.transform.swaps_axes());
        assert_eq!(heif.display_size(), (768, 1024));
        // 270 counter-clockwise is 90 clockwise in EXIF's numbering.
        assert_eq!(heif.transform.as_exif(), crate::exif::Orientation::Rotate90);
    }

    #[test]
    fn the_exif_item_is_found_through_its_cdsc_reference() {
        let heif = parse(&synthetic_heif()).unwrap();
        let exif = heif.exif.expect("an Exif item is linked to the primary");
        assert_eq!(exif.offset, 4096);
        assert_eq!(exif.length, 128);
        assert!(!exif.in_item_data);
    }

    #[test]
    fn the_gain_map_brand_is_noticed() {
        let heif = parse(&synthetic_heif()).unwrap();
        assert!(heif.has_gain_map, "tmap was in the compatible brands");
        assert!(heif.is_hevc, "the tiles are hvc1");
        assert!(heif.summary().contains("2×2 grid"), "{}", heif.summary());
        assert!(heif.summary().contains("HDR gain map"));
    }

    #[test]
    fn junk_and_truncation_are_refused_rather_than_panicking() {
        assert!(parse(b"").is_none());
        assert!(parse(b"not a heif file at all").is_none());
        assert!(!is_heif(b"\xff\xd8\xff\xe1 a jpeg"));

        // Every truncation of a real file must be survivable.
        let full = synthetic_heif();
        for cut in (8..full.len()).step_by(7) {
            let _ = parse(&full[..cut]);
        }
    }

    #[test]
    fn a_box_claiming_an_absurd_size_does_not_run_away() {
        let mut bytes = synthetic_heif();
        // The four bytes before the `meta` tag are its length.
        let meta_at = bytes
            .windows(4)
            .position(|window| window == b"meta")
            .expect("the fixture has a meta box")
            - 4;

        // A length reaching far past the buffer must be refused, not followed.
        bytes[meta_at..meta_at + 4].copy_from_slice(&0xFFFF_FFFF_u32.to_be_bytes());
        assert!(
            parse(&bytes).is_none(),
            "an overlong box should be rejected"
        );

        // A length of zero means "to the end", which is legal and must not spin.
        bytes[meta_at..meta_at + 4].copy_from_slice(&0_u32.to_be_bytes());
        let _ = parse(&bytes);

        // A length shorter than the header is nonsense and must stop the walk.
        bytes[meta_at..meta_at + 4].copy_from_slice(&3_u32.to_be_bytes());
        assert!(parse(&bytes).is_none());
    }

    /// Runs the parser over a real library and reports what it found. Synthetic
    /// fixtures prove the shape; only phone files prove it survives what phones
    /// actually write.
    ///
    /// `GEO_REF_MAPPER_HEIF_DIR=/photos cargo test --release bench_heif -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_HEIF_DIR and real HEIC files"]
    fn bench_heif_against_a_real_library() {
        use std::collections::BTreeMap;
        use std::time::Instant;

        let Ok(dir) = std::env::var("GEO_REF_MAPPER_HEIF_DIR") else {
            eprintln!("set GEO_REF_MAPPER_HEIF_DIR");
            return;
        };
        let limit: usize = std::env::var("GEO_REF_MAPPER_HEIF_N")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(400);

        let files: Vec<std::path::PathBuf> = walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_map(Result::ok)
            .map(|entry| entry.path().to_path_buf())
            .filter(|path| {
                path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    e.eq_ignore_ascii_case("heic") || e.eq_ignore_ascii_case("heif")
                })
            })
            .take(limit)
            .collect();
        assert!(!files.is_empty(), "no HEIC files under {dir}");

        let mut parsed = 0;
        let mut not_heif = 0;
        let mut failed = 0;
        let mut grids = 0;
        let mut with_thumb = 0;
        let mut with_exif = 0;
        let mut gain_maps = 0;
        let mut depth = 0;
        let mut rotated = 0;
        let mut tiles: BTreeMap<usize, usize> = BTreeMap::new();
        let mut shapes: BTreeMap<String, usize> = BTreeMap::new();

        let started = Instant::now();
        for path in &files {
            // Sniff cheaply first so mislabelled JPEGs are not charged to the
            // parser, then let `from_path` read only the boxes it needs.
            let mut magic = [0_u8; 12];
            {
                use std::io::Read;
                let Ok(mut file) = std::fs::File::open(path) else {
                    continue;
                };
                if file.read(&mut magic).is_err() {
                    continue;
                }
            }
            if !is_heif(&magic) {
                not_heif += 1;
                continue;
            }
            let Some(heif) = from_path(path) else {
                failed += 1;
                eprintln!("  FAILED to parse {}", path.display());
                continue;
            };
            parsed += 1;
            if let Some(grid) = &heif.grid {
                grids += 1;
                *tiles.entry(grid.tiles.len()).or_default() += 1;
                *shapes
                    .entry(format!("{}x{}", grid.columns, grid.rows))
                    .or_default() += 1;
            }
            if heif.thumbnail.is_some() {
                with_thumb += 1;
            }
            if heif.exif.is_some() {
                with_exif += 1;
            }
            if heif.has_gain_map {
                gain_maps += 1;
            }
            if heif.has_depth {
                depth += 1;
            }
            if heif.transform != Transform::default() {
                rotated += 1;
            }

            // Every parse must be self-consistent, or the index would lie.
            assert!(heif.width > 0 && heif.height > 0, "{}", path.display());
            if let Some(grid) = &heif.grid {
                assert_eq!(
                    grid.tiles.len(),
                    (grid.columns * grid.rows) as usize,
                    "{} says {}x{} but lists {} tiles",
                    path.display(),
                    grid.columns,
                    grid.rows,
                    grid.tiles.len()
                );
            }
        }
        let elapsed = started.elapsed().as_secs_f64();

        println!("\n=== {} files ===", files.len());
        println!("  parsed          {parsed}");
        println!("  not HEIF at all {not_heif}   (mislabelled JPEG and friends)");
        println!("  failed to parse {failed}");
        println!(
            "  {:.2} ms per file, metadata only\n",
            elapsed * 1000.0 / files.len() as f64
        );
        println!("  grid primaries  {grids}");
        println!("  embedded thumb  {with_thumb}");
        println!("  EXIF item       {with_exif}");
        println!("  HDR gain map    {gain_maps}");
        println!("  depth map       {depth}");
        println!("  rotated         {rotated}");
        println!("\n  grid shapes:");
        for (shape, count) in &shapes {
            println!("    {shape:>8}  {count}");
        }
        println!("  tile counts:");
        for (count, files) in &tiles {
            println!("    {count:>4} tiles  {files}");
        }
        println!();

        assert_eq!(failed, 0, "every genuine HEIF should parse");
    }

    #[test]
    fn transform_maps_onto_every_exif_orientation_it_should() {
        use crate::exif::Orientation as O;
        let t = |rotation, horizontal, vertical| {
            Transform {
                rotation_ccw: rotation,
                mirror_horizontal: horizontal,
                mirror_vertical: vertical,
            }
            .as_exif()
        };
        assert_eq!(t(0, false, false), O::Normal);
        assert_eq!(t(90, false, false), O::Rotate270);
        assert_eq!(t(180, false, false), O::Rotate180);
        assert_eq!(t(270, false, false), O::Rotate90);
        assert_eq!(t(0, true, false), O::FlipHorizontal);
        assert_eq!(t(0, false, true), O::FlipVertical);
        assert!(!Transform::default().swaps_axes());
    }
}
