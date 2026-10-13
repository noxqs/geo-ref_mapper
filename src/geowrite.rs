//! Writing a position into a TIFF that already exists.
//!
//! Once a map has been placed by hand the answer has to go somewhere, and the
//! only place worth putting it is the file itself. In our database it would be
//! lost the moment the file is opened by anything else.
//!
//! Nothing is re-compressed and no pixel is touched. A TIFF's directory says
//! where each square lives; the squares are not moved, so the directory can be
//! written afresh at the end of the file with the position tags added, and the
//! header pointed at the new one. The old directory is left behind as a few
//! hundred unused bytes, which is the price of not rewriting a gigabyte.
//!
//! Every tag that is not about position is copied across **byte for byte** —
//! including the ones whose values live elsewhere in the file, which keep
//! pointing where they always did. Re-encoding them would be a chance to get
//! something subtly wrong for no gain.

use anyhow::{Context, Result, bail};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// The tags that say where a picture is. Removed before the new ones are added,
/// so writing a position twice does not leave the first one behind.
const POSITION_TAGS: [u16; 5] = [33550, 33922, 34264, 34735, 34737];

const TAG_PIXEL_SCALE: u16 = 33550;
const TAG_TIEPOINT: u16 = 33922;
const TAG_TRANSFORM: u16 = 34264;
const TAG_GEO_KEYS: u16 = 34735;
const TAG_GEO_ASCII: u16 = 34737;

const TYPE_SHORT: u16 = 3;
const TYPE_ASCII: u16 = 2;
const TYPE_DOUBLE: u16 = 12;

/// Web Mercator, which is what the basemap is in — so a placement worked out
/// against it needs no conversion on the way in or out.
const WEB_MERCATOR: u16 = 3857;

/// Removes any position from a file, leaving everything else alone.
pub fn clear(path: &Path) -> Result<()> {
    rewrite(path, &[])
}

/// Writes a placement into a file.
///
/// A turn of any size has to be stored as a full transformation: the simpler
/// corner-and-scale pair cannot express one, and a placement stored without its
/// rotation is silently in the wrong place. Without a turn the simple form is
/// used, because far more software understands it.
pub fn write(path: &Path, placement: &crate::anchor::Placement, height: u32) -> Result<()> {
    let turned = placement.rotation.abs() > 1e-9;

    let mut tags: Vec<(u16, u16, Vec<u8>)> = Vec::new();
    if turned {
        let matrix = placement.transformation_matrix();
        tags.push((TAG_TRANSFORM, TYPE_DOUBLE, doubles(&matrix)));
    } else {
        // The corner of the picture, and how much ground a pixel covers.
        //
        // The corner is pixel (0,0) — the top-left of the picture, which is the
        // northernmost, because picture rows run down and the earth runs up.
        // Taking the bottom row instead flips the map north to south.
        tags.push((
            TAG_PIXEL_SCALE,
            TYPE_DOUBLE,
            doubles(&[placement.scale, placement.scale, 0.0]),
        ));
        let _ = height;
        let top_left = placement.apply((0.0, 0.0));
        tags.push((
            TAG_TIEPOINT,
            TYPE_DOUBLE,
            doubles(&[0.0, 0.0, 0.0, top_left.0, top_left.1, 0.0]),
        ));
    }

    // The key directory: version 1.1.0, then one key saying which coordinate
    // system the numbers above are in.
    let keys: Vec<u16> = vec![
        1,
        1,
        0,
        1, // version, revision, minor, how many keys follow
        3072,
        0,
        1,
        WEB_MERCATOR, // ProjectedCSTypeGeoKey
    ];
    tags.push((TAG_GEO_KEYS, TYPE_SHORT, shorts(&keys)));

    let mut names = b"WGS 84 / Pseudo-Mercator|".to_vec();
    names.push(0);
    tags.push((TAG_GEO_ASCII, TYPE_ASCII, names));

    rewrite(path, &tags)
}

fn doubles(values: &[f64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn shorts(values: &[u16]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Writes a fresh first directory at the end of the file, carrying every tag
/// that is not about position plus whatever is given.
pub(crate) fn rewrite(path: &Path, add: &[(u16, u16, Vec<u8>)]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;

    let mut header = [0_u8; 16];
    file.read_exact(&mut header)
        .with_context(|| format!("reading the start of {}", path.display()))?;
    if &header[..2] != b"II" {
        // Only files written little-endian are touched. Rewriting a big-endian
        // one means byte-swapping every value copied across, which is a lot of
        // chances to be wrong for a case that has not come up.
        bail!("this TIFF is stored the other way round, and cannot be edited here");
    }

    let (big, first) = match u16::from_le_bytes([header[2], header[3]]) {
        42 => (
            false,
            u64::from(u32::from_le_bytes([
                header[4], header[5], header[6], header[7],
            ])),
        ),
        43 => (
            true,
            u64::from_le_bytes(header[8..16].try_into().expect("eight bytes")),
        ),
        other => bail!("not a TIFF: version {other}"),
    };
    let entry_size = if big { 20 } else { 12 };
    let pointer_size = if big { 8 } else { 4 };
    let inline = pointer_size as usize;

    // The existing first directory, entry by entry.
    file.seek(SeekFrom::Start(first))?;
    let mut raw_count = vec![0_u8; if big { 8 } else { 2 }];
    file.read_exact(&mut raw_count)?;
    let count = if big {
        u64::from_le_bytes(raw_count[..8].try_into().expect("eight")) as usize
    } else {
        u16::from_le_bytes(raw_count[..2].try_into().expect("two")) as usize
    };
    if count == 0 || count > 4096 {
        bail!("the directory claims {count} entries, which is not believable");
    }
    let mut table = vec![0_u8; count * entry_size];
    file.read_exact(&mut table)?;
    let mut next = vec![0_u8; pointer_size as usize];
    file.read_exact(&mut next)?;

    // Kept byte for byte. Values stored elsewhere in the file are not moved, so
    // the offsets in these entries stay true.
    let mut entries: Vec<Vec<u8>> = Vec::with_capacity(count + add.len());
    for index in 0..count {
        let entry = &table[index * entry_size..(index + 1) * entry_size];
        let tag = u16::from_le_bytes([entry[0], entry[1]]);
        if POSITION_TAGS.contains(&tag) {
            continue;
        }
        entries.push(entry.to_vec());
    }

    // New values go at the end, before the directory that points at them.
    // TIFF wants values to start on an even byte.
    if file.seek(SeekFrom::End(0))? % 2 == 1 {
        file.write_all(&[0])?;
    }
    for (tag, kind, bytes) in add {
        let unit = match *kind {
            TYPE_ASCII => 1,
            TYPE_SHORT => 2,
            _ => 8,
        };
        let items = (bytes.len() / unit) as u64;

        let mut entry = Vec::with_capacity(entry_size);
        entry.extend_from_slice(&tag.to_le_bytes());
        entry.extend_from_slice(&kind.to_le_bytes());
        if big {
            entry.extend_from_slice(&items.to_le_bytes());
        } else {
            entry.extend_from_slice(&(items as u32).to_le_bytes());
        }

        if bytes.len() <= inline {
            let mut field = vec![0_u8; inline];
            field[..bytes.len()].copy_from_slice(bytes);
            entry.extend_from_slice(&field);
        } else {
            let position = file.seek(SeekFrom::End(0))?;
            file.write_all(bytes)?;
            if bytes.len() % 2 == 1 {
                file.write_all(&[0])?;
            }
            if big {
                entry.extend_from_slice(&position.to_le_bytes());
            } else {
                entry.extend_from_slice(&(position as u32).to_le_bytes());
            }
        }
        entries.push(entry);
    }

    // Ascending by tag, which TIFF requires and readers are entitled to rely on.
    entries.sort_by_key(|entry| u16::from_le_bytes([entry[0], entry[1]]));

    let directory_at = file.seek(SeekFrom::End(0))?;
    let directory_at = if directory_at % 2 == 1 {
        file.write_all(&[0])?;
        directory_at + 1
    } else {
        directory_at
    };
    if big {
        file.write_all(&(entries.len() as u64).to_le_bytes())?;
    } else {
        file.write_all(&(entries.len() as u16).to_le_bytes())?;
    }
    for entry in &entries {
        file.write_all(entry)?;
    }
    // The rest of the chain is untouched, so the smaller copies still follow.
    file.write_all(&next)?;

    // Only now does the header change. Until this write the file still reads as
    // it did, so a failure part way through leaves it whole.
    file.seek(SeekFrom::Start(if big { 8 } else { 4 }))?;
    if big {
        file.write_all(&directory_at.to_le_bytes())?;
    } else {
        file.write_all(&(directory_at as u32).to_le_bytes())?;
    }
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::Placement;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-geowrite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// A small layered TIFF, made the way the converter makes them.
    ///
    /// **`name` must be unique to the test using it.** The scratch directory is
    /// per-process, not per-test, so two tests asking for the same name get the
    /// same file — and the harness runs them on threads at the same time. Two
    /// tests both called it "turned", and one would intermittently read the
    /// map while the other was still writing it. It failed perhaps one run in
    /// three and passed every time it was run on its own, which is the shape
    /// this kind of collision always has.
    fn a_map(name: &str) -> std::path::PathBuf {
        let source = scratch(&format!("{name}-source.png"));
        image::RgbaImage::from_fn(1100, 900, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        })
        .save(&source)
        .unwrap();
        let target = scratch(&format!("{name}.tif"));
        let _ = std::fs::remove_file(&target);
        crate::convert::convert(&source, &target, |_| true).unwrap();
        target
    }

    #[test]
    fn a_turned_map_comes_back_turned_and_not_square_to_north() {
        // The bug this pins. `GeoRef` reduces a transformation to a corner and
        // a pixel size — everything the upright case needs, and no turn. The
        // overlay drew from that, so every hand-turned map was laid on the
        // basemap square to north: correctly positioned, visibly lining up with
        // nothing, and indistinguishable from the saved placement being
        // ignored.
        let path = a_map("turned");
        let placement = Placement {
            rotation: 0.15, // about 8.6 degrees
            ..singapore()
        };
        write(&path, &placement, 900).unwrap();

        let (read, _, _) = crate::geotiff::placement_of(&path).expect("a placement comes back");
        assert!(
            (read.rotation - placement.rotation).abs() < 1e-9,
            "the turn was lost: {} instead of {}",
            read.rotation,
            placement.rotation
        );
        assert!((read.scale - placement.scale).abs() < 1e-6, "{read:?}");

        // And the corner-and-scale reading, which is what used to be the only
        // reading, genuinely cannot express it — so the overlay had to be given
        // the placement rather than taught to turn the rectangle.
        let geo = crate::geotiff::from_path(&path).expect("it still reads as placed");
        assert!(geo.transformed, "the file says it is turned");
    }

    #[test]
    fn an_upright_map_still_reads_as_upright() {
        // No turn written, none invented: an upright map given a rotation would
        // be just as wrong, and harder to notice.
        let path = a_map("upright");
        write(&path, &singapore(), 900).unwrap();
        let (read, _, _) = crate::geotiff::placement_of(&path).expect("a placement comes back");
        assert_eq!(read.rotation, 0.0);
        assert!((read.scale - singapore().scale).abs() < 1e-6);
    }

    fn singapore() -> Placement {
        // About where a map of Singapore sits, in Web Mercator metres.
        Placement {
            scale: 0.3,
            rotation: 0.0,
            origin: (11_554_603.0, 148_440.0),
        }
    }

    #[test]
    fn a_position_written_can_be_read_back() {
        let path = a_map("readback");
        assert!(
            crate::geotiff::from_path(&path).is_none(),
            "a converted map starts with no position",
        );

        write(&path, &singapore(), 900).unwrap();
        let geo = crate::geotiff::from_path(&path).expect("it now has a position");
        assert_eq!(geo.epsg, Some(3857));
        assert!((geo.scale.0 - 0.3).abs() < 1e-9);
        assert_eq!(geo.citations, vec!["WGS 84 / Pseudo-Mercator"]);
    }

    #[test]
    fn writing_a_position_leaves_the_picture_alone() {
        let path = a_map("pixels");
        let before = {
            let mut tiff = crate::cog::TiledTiff::open(&path).unwrap();
            tiff.decode_level(0).unwrap()
        };
        let sizes_before = crate::cog::TiledTiff::open(&path).unwrap().level_sizes();

        write(&path, &singapore(), 900).unwrap();

        let mut tiff = crate::cog::TiledTiff::open(&path).unwrap();
        assert_eq!(
            tiff.level_sizes(),
            sizes_before,
            "every layer must still be there",
        );
        let after = tiff.decode_level(0).unwrap();
        assert_eq!(before.dimensions(), after.dimensions());
        assert_eq!(
            before.get_pixel(500, 400),
            after.get_pixel(500, 400),
            "not one pixel should have changed",
        );
    }

    #[test]
    fn writing_twice_replaces_rather_than_accumulates() {
        let path = a_map("twice");
        write(&path, &singapore(), 900).unwrap();
        let moved = Placement {
            origin: (11_500_000.0, 140_000.0),
            ..singapore()
        };
        write(&path, &moved, 900).unwrap();

        let geo = crate::geotiff::from_path(&path).unwrap();
        // The second one wins outright — a leftover first would make the map
        // land in the wrong place with nothing to say why.
        assert!((geo.origin.0 - 11_500_000.0).abs() < 1.0);
    }

    #[test]
    fn a_position_can_be_taken_off_again() {
        let path = a_map("cleared");
        write(&path, &singapore(), 900).unwrap();
        assert!(crate::geotiff::from_path(&path).is_some());

        clear(&path).unwrap();
        assert!(
            crate::geotiff::from_path(&path).is_none(),
            "the position must be gone, not merely emptied",
        );
        // And the picture is still readable.
        let tiff = crate::cog::TiledTiff::open(&path).unwrap();
        assert_eq!(tiff.level_sizes().len(), 3);
    }

    #[test]
    fn a_turned_placement_is_stored_as_a_transformation() {
        let path = a_map("turned-transform");
        write(
            &path,
            &Placement {
                rotation: 0.15,
                ..singapore()
            },
            900,
        )
        .unwrap();

        let geo = crate::geotiff::from_path(&path).expect("still georeferenced");
        assert!(
            geo.transformed,
            "a turn cannot be held by a corner and a scale, so the full form \
             must have been used",
        );
    }

    #[test]
    fn the_corner_written_is_the_top_left_of_the_picture() {
        // Picture rows run down and the earth runs up, so pixel (0,0) is the
        // *northernmost* corner — which is exactly what GeoTIFF's tiepoint
        // wants. Taking the bottom row instead flips the map north to south.
        let path = a_map("corner");
        let placement = singapore();
        write(&path, &placement, 900).unwrap();

        let geo = crate::geotiff::from_path(&path).unwrap();
        let expected = placement.apply((0.0, 0.0));
        assert!((geo.origin.0 - expected.0).abs() < 1.0);
        assert!((geo.origin.1 - expected.1).abs() < 1.0);
        // And north must be above south.
        let (north, _) = geo.top_left().unwrap();
        let (south, _) = geo.bottom_right().unwrap();
        assert!(north > south, "the map is upside down");
    }
}
