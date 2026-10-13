//! Carrying a hand-made position from an old conversion into a new one.
#![cfg_attr(not(test), allow(dead_code))]

//!
//! A map placed by hand has its answer written into the converted file as
//! GeoTIFF tags. Convert that map again — from a re-downloaded source, into a
//! different folder — and the new file knows nothing: the placement lived in
//! the old file and nowhere else.
//!
//! This copies those tags across, matching old to new **by filename**. The
//! paths cannot be used: the old tree mirrored the whole source path and the new
//! one keeps only the year folder, so the two look nothing alike.
//!
//! **The dimensions must agree, and a pair that disagrees is refused.** A
//! position is expressed in pixels — a tiepoint at pixel (0,0) plus a scale, or
//! a full transformation — so it is only meaningful against a raster of the size
//! it was made for. A differently sized scan of the same map would take the tags
//! quite happily and sit in the wrong place, looking perfectly plausible. That
//! is the failure this refuses to make.
//!
//! A one-time migration rather than a feature, run deliberately:
//!
//! ```text
//! GEO_REF_MAPPER_CARRY_FROM=…/MAPS-OUT-OLD GEO_REF_MAPPER_CARRY_TO=…/MAPS-OUT \
//!     cargo test --release carry_positions -- --ignored --nocapture
//! ```
//!
//! It reports before it writes, and writes nothing unless `GEO_REF_MAPPER_CARRY_GO`
//! is set — so the first run is always a dry run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What became of one map.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Copied across.
    Carried,
    /// The new file already says where it is; left alone.
    AlreadyPlaced,
    /// No map of that name in the new folder.
    NoCounterpart,
    /// Found, but a different size — refused rather than misplaced.
    SizeDiffers { old: (u32, u32), new: (u32, u32) },
    /// The tags could not be written.
    Failed(String),
}

/// Every map under `root`, by file name.
///
/// A name appearing twice is dropped rather than guessed at: two maps of the
/// same name in different folders are two different maps, and picking either
/// would be picking at random.
fn by_name(root: &Path) -> HashMap<String, PathBuf> {
    let mut seen: HashMap<String, Option<PathBuf>> = HashMap::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|e| e != "tif") {
            continue;
        }
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        seen.entry(name)
            .and_modify(|slot| *slot = None)
            .or_insert_with(|| Some(path.to_path_buf()));
    }
    seen.into_iter()
        .filter_map(|(name, path)| Some((name, path?)))
        .collect()
}

/// Carries every position it safely can, and says what happened to each.
///
/// `write` false looks at everything and changes nothing, which is how this
/// should always be run first.
pub fn carry_positions(from: &Path, to: &Path, write: bool) -> Vec<(PathBuf, Outcome)> {
    let destinations = by_name(to);
    let mut out = Vec::new();

    for entry in walkdir::WalkDir::new(from)
        .into_iter()
        .filter_map(Result::ok)
    {
        let old = entry.path();
        if !old.is_file() || old.extension().is_none_or(|e| e != "tif") {
            continue;
        }
        // Only maps that actually carry a position; the rest are not migrations
        // waiting to happen, they simply were never placed.
        let Some(source) = crate::geotiff::from_path(old) else {
            continue;
        };
        let name = old
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();

        let Some(new) = destinations.get(&name) else {
            out.push((old.to_path_buf(), Outcome::NoCounterpart));
            continue;
        };

        // Already placed: leave it. Overwriting would replace a position
        // somebody may have made since with an older one.
        if crate::geotiff::from_path(new).is_some() {
            out.push((old.to_path_buf(), Outcome::AlreadyPlaced));
            continue;
        }

        let Some(target) = crate::cog::TiledTiff::open(new)
            .ok()
            .and_then(|tiff| tiff.level_sizes().first().copied())
        else {
            out.push((
                old.to_path_buf(),
                Outcome::Failed("the new map could not be opened".to_owned()),
            ));
            continue;
        };
        if (source.width, source.height) != target {
            out.push((
                old.to_path_buf(),
                Outcome::SizeDiffers {
                    old: (source.width, source.height),
                    new: target,
                },
            ));
            continue;
        }

        if !write {
            out.push((old.to_path_buf(), Outcome::Carried));
            continue;
        }
        // Byte for byte, in whichever form the original used — the simple
        // corner-and-scale pair or a full transformation. Re-deriving them
        // would be a chance to round something differently for no gain.
        let outcome = match crate::geotiff::read_raw_tags(old) {
            Some(tags) => match crate::geowrite::rewrite(new, &tags) {
                Ok(()) => Outcome::Carried,
                Err(error) => Outcome::Failed(format!("{error:#}")),
            },
            None => Outcome::Failed("the position could not be read back".to_owned()),
        };
        out.push((old.to_path_buf(), outcome));
    }

    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the migration. Reports only, unless `GEO_REF_MAPPER_CARRY_GO` is set.
    ///
    /// `GEO_REF_MAPPER_CARRY_FROM=… GEO_REF_MAPPER_CARRY_TO=… cargo test --release
    /// carry_positions -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_CARRY_FROM and GEO_REF_MAPPER_CARRY_TO"]
    fn carry_positions_across() {
        let (Ok(from), Ok(to)) = (
            std::env::var("GEO_REF_MAPPER_CARRY_FROM"),
            std::env::var("GEO_REF_MAPPER_CARRY_TO"),
        ) else {
            return;
        };
        let write = std::env::var("GEO_REF_MAPPER_CARRY_GO").is_ok();
        let report = carry_positions(Path::new(&from), Path::new(&to), write);

        let (mut carried, mut already, mut none, mut differs, mut failed) = (0, 0, 0, 0, 0);
        for (path, outcome) in &report {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            match outcome {
                Outcome::Carried => carried += 1,
                Outcome::AlreadyPlaced => already += 1,
                Outcome::NoCounterpart => none += 1,
                Outcome::SizeDiffers { old, new } => {
                    differs += 1;
                    println!("  SIZE  {name}\n          old {old:?}  new {new:?}");
                }
                Outcome::Failed(why) => {
                    failed += 1;
                    println!("  FAIL  {name}: {why}");
                }
            }
        }
        println!(
            "\n  {}: {carried} carried, {already} already placed, {none} with no \
             counterpart, {differs} refused for size, {failed} failed",
            if write { "written" } else { "DRY RUN" },
        );
        assert_eq!(failed, 0, "nothing should fail outright");
    }

    #[test]
    fn a_name_appearing_twice_is_dropped_rather_than_guessed_at() {
        // Two maps of the same name in different folders are two different
        // maps. Choosing either would be choosing at random, and a position
        // written onto the wrong map is worse than one not written at all.
        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-carry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for folder in ["1932", "1945"] {
            std::fs::create_dir_all(dir.join(folder)).unwrap();
            std::fs::write(dir.join(folder).join("plan.tif"), b"not really a tiff").unwrap();
        }
        std::fs::write(dir.join("1932").join("unique.tif"), b"nor this").unwrap();

        let found = by_name(&dir);
        assert!(
            !found.contains_key("plan.tif"),
            "an ambiguous name must not resolve to either",
        );
        assert!(found.contains_key("unique.tif"), "an unambiguous one must");
    }
}
