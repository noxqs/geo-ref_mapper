//! Rebuilding anchor pairings from the placements the files still carry.
//!
//! Taking a folder out of the tree used to delete every pin-to-pixel pairing
//! under it, because both tables hung off the image row with `ON DELETE
//! CASCADE`. Schema 11 keys them by path so it cannot happen again; this puts
//! back what the old rule took.
//!
//! **The placements themselves were never lost** — they live in the files as
//! GeoTIFF tags, and the maps have gone on drawing in the right places
//! throughout. What went was the working-out: which marked place sat on which
//! pixel, and so any ability to adjust one.
//!
//! That can be recovered exactly, because a placement *is* the relationship
//! between the two. Put a marked place through the inverse of the saved
//! transform and out comes the pixel it sits on. Every pairing rebuilt this way
//! lies exactly on the saved placement, so re-solving any set of them returns
//! the same answer the file already holds — the map cannot be moved by getting
//! its own position back.
//!
//! What is genuinely gone is the residuals: how far each hand-placed pin sat
//! from the fit. Those said how good the placement was, not where it is.
//!
//! Which anchors belonged to a map is not recorded anywhere either, so the ones
//! taken are those that land inside the raster. An anchor that was never used
//! comes back sitting exactly where the placement says it should — which adds
//! nothing wrong to the fit, and is a pairing that would have been made anyway
//! had anyone looked for it.
//!
//! A one-time repair rather than a feature, run deliberately and with the app
//! closed:
//!
//! ```text
//! cargo test --release regain_pairings -- --ignored --nocapture
//! ```
//!
//! It reports before it writes, and writes nothing unless `GEO_REF_MAPPER_REGAIN_GO`
//! is set — so the first run is always a dry run.

#![cfg_attr(not(test), allow(dead_code))]

use crate::anchor::Placement;

#[cfg(test)]
use crate::geotiff::placement_of;

/// How many pairings one map is given back.
///
/// A hand-made set was three to five. A large sheet covering the whole island
/// has every marked place in the library inside it, and handing back
/// twenty-nine is a screen of pins to read past for work that never had them.
/// A similarity fit wants a handful spread out, not a crowd.
pub const MOST_PER_MAP: usize = 8;

/// What became of one map.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Pairings put back, and how many.
    Regained(usize),
    /// It already has some; left alone. Work done since is not overwritten.
    AlreadyHasSome,
    /// No position in the file, so there is nothing to work back from.
    NotPlaced,
    /// Placed, but no marked place falls inside it.
    NoAnchorsInside,
}

/// Where each marked place falls on one map, for those that fall on it at all.
///
/// Bounded to the raster: a placement answers for the whole plane, so without
/// this every anchor in the library would come back on every map, most of them
/// at pixel coordinates hundreds of thousands away.
///
/// Then thinned to [`MOST_PER_MAP`], keeping the ones that spread furthest
/// across the sheet. Which anchors were originally used is not recorded, so
/// there is no *right* subset to pick — but spread is what a fit actually wants,
/// and every one of them lies on the placement regardless, so no choice here
/// can move the map.
pub fn pairings_for(
    placement: &Placement,
    width: u32,
    height: u32,
    anchors: &[(i64, f64, f64)],
) -> Vec<(i64, f64, f64)> {
    let inside: Vec<(i64, f64, f64)> = anchors
        .iter()
        .filter_map(|(id, lat, lon)| {
            let world = crate::map::to_mercator(*lat, *lon);
            let (x, y) = placement.pixel_of(world);
            let on_it = x >= 0.0 && y >= 0.0 && x <= f64::from(width) && y <= f64::from(height);
            on_it.then_some((*id, x, y))
        })
        .collect();
    spread_out(inside, MOST_PER_MAP)
}

/// Thins a set of pixel positions to `most`, keeping them as far apart as
/// possible.
///
/// Farthest-first: start at the one nearest a corner — a fixed starting point,
/// so the same map thins the same way every time — and repeatedly take
/// whichever is furthest from everything taken so far.
fn spread_out(mut from: Vec<(i64, f64, f64)>, most: usize) -> Vec<(i64, f64, f64)> {
    if from.len() <= most {
        from.sort_by_key(|(id, _, _)| *id);
        return from;
    }
    // A settled starting point rather than whichever happened to be first.
    from.sort_by(|a, b| {
        (a.1 * a.1 + a.2 * a.2)
            .partial_cmp(&(b.1 * b.1 + b.2 * b.2))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut kept = vec![from.remove(0)];
    while kept.len() < most && !from.is_empty() {
        let (index, _) = from
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let nearest = kept
                    .iter()
                    .map(|held| (held.1 - candidate.1).hypot(held.2 - candidate.2))
                    .fold(f64::MAX, f64::min);
                (index, nearest)
            })
            .max_by(|left, right| {
                left.1
                    .partial_cmp(&right.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("there is at least one left");
        kept.push(from.remove(index));
    }
    kept.sort_by_key(|(id, _, _)| *id);
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A placement, and the pixel a place lands on under it.
    fn placed() -> Placement {
        Placement {
            scale: 4.0,
            rotation: 0.21,
            origin: (11_556_000.0, 143_000.0),
        }
    }

    #[test]
    fn a_rebuilt_pairing_lands_where_the_placement_says() {
        // The property the whole repair rests on: a pairing worked back from
        // the placement sits exactly on it, so re-solving cannot move the map.
        let placement = placed();
        let pixel = (1234.0, 987.0);
        let world = placement.apply(pixel);
        let back = placement.pixel_of(world);
        assert!((back.0 - pixel.0).abs() < 1e-6, "{back:?}");
        assert!((back.1 - pixel.1).abs() < 1e-6, "{back:?}");
    }

    #[test]
    fn a_turned_placement_survives_the_round_trip_through_the_tags() {
        // Written as a transformation and read back as a placement. Reading it
        // as tiepoint-and-scale instead would drop the turn and put every
        // rotated map back square — wrong, and plausibly so.
        let placement = placed();
        let matrix = placement.transformation_matrix();
        let (a, b) = (matrix[0], matrix[4]);
        let read = Placement {
            scale: a.hypot(b),
            rotation: b.atan2(a),
            origin: (matrix[3], matrix[7]),
        };
        assert!((read.scale - placement.scale).abs() < 1e-9, "{read:?}");
        assert!(
            (read.rotation - placement.rotation).abs() < 1e-9,
            "{read:?}"
        );
        assert_eq!(read.origin, placement.origin);

        // And the pixel it gives is the pixel the original gives.
        let world = placement.apply((500.0, 400.0));
        let (x, y) = read.pixel_of(world);
        assert!((x - 500.0).abs() < 1e-6 && (y - 400.0).abs() < 1e-6);
    }

    #[test]
    fn only_the_anchors_that_land_on_the_map_are_taken() {
        let placement = placed();
        let (width, height) = (2000_u32, 1500_u32);
        // One in the middle of the sheet, one far away.
        let on_it = placement.apply((1000.0, 750.0));
        let miles_off = placement.apply((900_000.0, 900_000.0));
        let to_degrees = |world: (f64, f64)| crate::map::from_mercator(world.0, world.1);
        let (lat_a, lon_a) = to_degrees(on_it);
        let (lat_b, lon_b) = to_degrees(miles_off);

        let found = pairings_for(
            &placement,
            width,
            height,
            &[(1, lat_a, lon_a), (2, lat_b, lon_b)],
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, 1);
        assert!((found[0].1 - 1000.0).abs() < 1.0, "{found:?}");
        assert!((found[0].2 - 750.0).abs() < 1.0, "{found:?}");
    }

    #[test]
    fn a_crowd_is_thinned_to_a_spread() {
        // Nine anchors bunched in one corner and one out at the far side. The
        // far one must survive: it is the whole reason the thinning goes by
        // distance rather than by taking the first few.
        let mut anchors: Vec<(i64, f64, f64)> = (0..9)
            .map(|i| (i, 10.0 + i as f64, 10.0 + i as f64))
            .collect();
        anchors.push((99, 1900.0, 1400.0));
        let kept = spread_out(anchors, 4);
        assert_eq!(kept.len(), 4);
        assert!(kept.iter().any(|(id, _, _)| *id == 99), "{kept:?}");
    }

    #[test]
    fn a_small_set_is_left_alone() {
        let anchors = vec![(1, 10.0, 10.0), (2, 500.0, 400.0), (3, 900.0, 20.0)];
        assert_eq!(spread_out(anchors.clone(), 8).len(), anchors.len());
    }

    #[test]
    fn the_same_map_thins_the_same_way_twice() {
        // Order out must not depend on order in, or a second run would hand
        // back a different set and the map would look like it had changed.
        let anchors: Vec<(i64, f64, f64)> = (0..20)
            .map(|i| (i, (i as f64) * 37.0 % 900.0, (i as f64) * 61.0 % 700.0))
            .collect();
        let once = spread_out(anchors.clone(), 6);
        let mut shuffled = anchors;
        shuffled.reverse();
        assert_eq!(once, spread_out(shuffled, 6));
    }

    #[test]
    fn a_map_with_no_position_gives_nothing_back() {
        // Nothing to work back from, and inventing pairings for one would put
        // pins on a map nobody has placed.
        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-regain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("plain.png");
        image::RgbaImage::from_pixel(64, 48, image::Rgba([90, 90, 90, 255]))
            .save(&source)
            .unwrap();
        let path = dir.join("plain.tif");
        let _ = std::fs::remove_file(&path);
        crate::convert::convert(&source, &path, |_| true).unwrap();
        assert!(placement_of(&path).is_none());
    }

    /// Puts the pairings back. Reports only, unless `GEO_REF_MAPPER_REGAIN_GO` is
    /// set.
    ///
    /// `cargo test --release regain_pairings -- --ignored --nocapture`
    #[test]
    #[ignore = "a one-time repair, run against the real library with the app closed"]
    fn regain_pairings() {
        let write = std::env::var("GEO_REF_MAPPER_REGAIN_GO").is_ok();
        let Ok(path) = crate::db::default_path() else {
            println!("no library path");
            return;
        };
        let Ok(library) = crate::db::Library::open(&path) else {
            println!("no library to repair");
            return;
        };

        let anchors: Vec<(i64, f64, f64)> = library
            .all_anchors()
            .unwrap_or_default()
            .into_iter()
            .map(|anchor| (anchor.id, anchor.lat, anchor.lon))
            .collect();
        println!("{} marked places to work from", anchors.len());

        let maps = library.georeferenced_images().unwrap_or_default();
        let (mut regained, mut pairings, mut had, mut unplaced, mut empty) = (0, 0, 0, 0, 0);

        for (id, path) in &maps {
            let path = std::path::PathBuf::from(path);
            let outcome = if !library
                .anchors_on_image(*id)
                .map(|held| held.is_empty())
                .unwrap_or(true)
            {
                Outcome::AlreadyHasSome
            } else if let Some((placement, width, height)) = placement_of(&path) {
                let found = pairings_for(&placement, width, height, &anchors);
                if found.is_empty() {
                    Outcome::NoAnchorsInside
                } else {
                    if write {
                        for (anchor, x, y) in &found {
                            let _ = library.place_anchor_on_image(*id, *anchor, *x, *y);
                        }
                    }
                    Outcome::Regained(found.len())
                }
            } else {
                Outcome::NotPlaced
            };

            match outcome {
                Outcome::Regained(count) => {
                    regained += 1;
                    pairings += count;
                    println!(
                        "  {count:2} pins  {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
                Outcome::AlreadyHasSome => had += 1,
                Outcome::NotPlaced => unplaced += 1,
                Outcome::NoAnchorsInside => empty += 1,
            }
        }

        println!(
            "\n{}: {regained} maps given back {pairings} pairings; \
             {had} already had some, {empty} had no marked place inside them, \
             {unplaced} carried no position",
            if write { "written" } else { "DRY RUN" },
        );
    }
}
