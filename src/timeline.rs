//! Fading maps in and out by scrolling time.
//!
//! Setting a dozen maps by hand, one row at a time, is a lot of dragging to
//! answer a question — "what did this corner look like as the years went on" —
//! that is really one gesture. So the strip does it: whichever year is against
//! the playhead is the map that shows, and scrolling the strip walks the fade
//! from one sheet to the next.
//!
//! Neighbours overlap rather than cutting, which is the point. Two sheets
//! twenty years apart are both faintly up as the playhead passes between them,
//! and that overlap is what makes one turn into the other instead of blinking.
//!
//! The width of the fade is set **in points on the strip, not in years**. That
//! is what makes zooming the strip do something useful: at a coarse scale a
//! window covers decades and half a dozen sheets are mixed at once, and zooming
//! in narrows it until the maps come one at a time. The control for "how
//! smooth" is the same control as "how much time can I see", which is the one
//! already under the pointer.

/// The most a map is drawn at while the strip is doing the fading.
///
/// Half. The basemap underneath is the reference being compared against, and a
/// historical sheet at full strength hides the very thing it is being read
/// alongside.
pub const PEAK: f32 = 0.5;

/// How far either side of the playhead a map is still drawn, in points.
///
/// A fixed distance on the strip rather than a fixed number of years, so the
/// zoom controls it. At the default scale this is about a decade either side —
/// wide enough that a sparse stretch is not empty, narrow enough that a dense
/// one is not mud. Zoomed right in it is a couple of years; right out, most of
/// a century.
pub const FADE_POINTS: f32 = 140.0;

/// Points per year. The default strip scale.
pub const DEFAULT_SCALE: f32 = 14.0;

/// Closest the strip will zoom in.
///
/// Far enough that the fade window drops below half a year, which is what it
/// takes to separate several sheets carrying the same date — they sit inside
/// that year, so nothing coarser can tell them apart.
pub const MAX_SCALE: f32 = 300.0;

/// And out: five centuries in a window, which is the whole collection at once.
pub const MIN_SCALE: f32 = 1.6;

/// The most maps drawn at once while the strip is doing the fading.
///
/// A cap, not a preference. A busy decade in one corner of the collection can
/// put fifty sheets inside one window, and each one drawn is a file opened and
/// tiles read — enough to stall the very scroll that asked for them. The ones
/// kept are the ones nearest the playhead, which are the ones actually visible;
/// what falls off the end was down at a few percent.
pub const MOST_AT_ONCE: usize = 12;

/// Where a map sits on the strip when several share its year.
///
/// A year is a point, and four sheets dated to it are four maps at the same
/// point — all faded up together, by any scroll and at any speed, because there
/// is nothing to tell them apart with. So they are spread inside their own
/// year: `count` of them at even spacing, centred on the year so the number
/// written on the strip still means where they are.
///
/// The order is the caller's and must be stable. Sheets that swapped places
/// between frames would flicker past each other as the playhead crossed them.
pub fn slot_year(year: i32, index: usize, count: usize) -> f32 {
    if count <= 1 {
        return year as f32;
    }
    // (i + ½)/n runs from just inside one end of the year to just inside the
    // other, rather than putting the first sheet on the year mark and the last
    // one on the next year's.
    year as f32 + (index as f32 + 0.5) / count as f32 - 0.5
}

/// How much of a wheel movement becomes time.
///
/// Damped, because a notch is a coarse thing — a single one covered several
/// years at the default scale, which is enough to step over every sheet in a
/// busy decade without any of them coming up. Dragging the strip is left at
/// one-to-one: that is direct handling of the thing itself, where the hand
/// already sets the pace.
pub const WHEEL_DAMPING: f32 = 0.4;

/// How many years the fade covers either side of the playhead, at this scale.
pub fn window_years(scale: f32) -> f32 {
    FADE_POINTS / scale.max(f32::MIN_POSITIVE)
}

/// How opaque a map sitting at `at` is drawn, with the playhead at `playhead`.
///
/// A raised cosine: nothing at the edge of the window, `PEAK` at the middle,
/// and — the part that matters — no corner at either end. A triangle would
/// reach the same values but arrive at them with a kink, which reads as the
/// fade catching rather than running.
pub fn opacity_at(at: f32, playhead: f32, window: f32) -> f32 {
    if window <= 0.0 {
        return 0.0;
    }
    let distance = (at - playhead).abs() / window;
    if distance >= 1.0 {
        return 0.0;
    }
    PEAK * 0.5 * (1.0 + (std::f32::consts::PI * distance).cos())
}

/// How often the strip writes a year, at this scale.
///
/// Zoomed out, a label a year would be five hundred numbers on top of each
/// other. Stepped through the intervals people actually count in — ones, fives,
/// tens, quarter-centuries, centuries — rather than whatever number the
/// arithmetic lands on, because a strip marked every 7 years is one nobody can
/// read a date off.
pub fn label_every(scale: f32) -> i32 {
    /// Points a label needs to itself before the next one touches it.
    const ROOM: f32 = 13.0;
    for step in [1, 5, 10, 25, 50, 100] {
        if step as f32 * scale >= ROOM {
            return step;
        }
    }
    100
}

/// The strip's scale after a zoom gesture, kept inside what is usable.
pub fn zoomed(scale: f32, factor: f32) -> f32 {
    (scale * factor).clamp(MIN_SCALE, MAX_SCALE)
}

/// The year at the top of the strip that keeps `at_year` under the pointer.
///
/// Zooming about the pointer rather than about the top edge: the year being
/// looked at is the one the gesture is about, and letting it slide away means
/// chasing it back down afterwards.
pub fn top_after_zoom(at_year: f32, pointer_offset: f32, scale: f32) -> f32 {
    at_year - pointer_offset / scale.max(f32::MIN_POSITIVE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_map_is_brightest_when_the_playhead_is_on_its_year() {
        let window = window_years(DEFAULT_SCALE);
        assert!((opacity_at(1840.0, 1840.0, window) - PEAK).abs() < 1e-6);
        // And never brighter than half, whatever is asked of it.
        for year in 1500..2030 {
            assert!(opacity_at(year as f32, 1840.0, window) <= PEAK + 1e-6);
        }
    }

    #[test]
    fn it_falls_to_nothing_at_the_edge_of_the_window_and_stays_there() {
        let window = window_years(DEFAULT_SCALE);
        let edge = 1840.0 + window;
        assert_eq!(opacity_at(edge.ceil(), 1840.0, window), 0.0);
        assert_eq!(opacity_at(1840.0 + 400.0, 1840.0, window), 0.0);
        // Symmetric: a sheet is no more visible from one side than the other.
        assert!(opacity_at(1835.0, 1840.0, window) > 0.0);
        assert!(
            (opacity_at(1835.0, 1840.0, window) - opacity_at(1845.0, 1840.0, window)).abs() < 1e-6
        );
    }

    #[test]
    fn neighbours_overlap_rather_than_cutting() {
        // The whole point of the shape. Put the playhead between two sheets and
        // both must be drawn — if only one ever were, scrolling would blink
        // from map to map instead of crossing between them.
        let window = window_years(DEFAULT_SCALE);
        let (earlier, later) = (1830.0, 1840.0);
        let between = 1835.0;
        assert!(opacity_at(earlier, between, window) > 0.0);
        assert!(opacity_at(later, between, window) > 0.0);
    }

    #[test]
    fn the_fade_has_no_corner_in_it() {
        // Sampled across the window: the change from one step to the next must
        // itself change smoothly. A triangle would hold one slope and then flip
        // its sign, which the eye reads as the fade catching.
        let window = 20.0;
        let at = |year: f32| {
            let distance = (year - 1840.0).abs() / window;
            if distance >= 1.0 {
                0.0
            } else {
                PEAK * 0.5 * (1.0 + (std::f32::consts::PI * distance).cos())
            }
        };
        let mut previous_slope: Option<f32> = None;
        for step in 0..40 {
            let year = 1840.0 + step as f32 * 0.5;
            let slope = at(year + 0.01) - at(year);
            if let Some(previous) = previous_slope {
                assert!(
                    (slope - previous).abs() < 0.01,
                    "slope jumped at {year}: {previous} to {slope}"
                );
            }
            previous_slope = Some(slope);
        }
    }

    #[test]
    fn zooming_in_narrows_the_window_so_fewer_maps_are_mixed() {
        // This is the reason the zoom exists: at a coarse scale a stretch of
        // decades is on at once, and going in separates them.
        let wide = window_years(MIN_SCALE);
        let tight = window_years(MAX_SCALE);
        assert!(wide > tight * 10.0, "{wide} vs {tight}");

        // Two sheets ten years apart: mixed when zoomed out, separated when in.
        assert!(opacity_at(1850.0, 1840.0, wide) > 0.0);
        assert_eq!(opacity_at(1850.0, 1840.0, tight), 0.0);
    }

    #[test]
    fn the_years_written_on_the_strip_thin_out_as_it_shrinks() {
        // Every year when there is room for it.
        assert_eq!(label_every(MAX_SCALE), 1);
        assert_eq!(label_every(DEFAULT_SCALE), 1);
        // And a round interval when there is not — never 7 or 13, which are
        // intervals nobody counts in.
        let coarse = label_every(MIN_SCALE);
        assert!([5, 10, 25, 50, 100].contains(&coarse), "{coarse}");
        // Whatever the scale, two labels never land on top of each other.
        for tenth in 16..=640 {
            let scale = tenth as f32 / 10.0;
            assert!(
                label_every(scale) as f32 * scale >= 13.0 || label_every(scale) == 100,
                "labels collide at {scale}"
            );
        }
    }

    #[test]
    fn sheets_sharing_a_year_are_spread_inside_it() {
        // The complaint this answers: four maps dated 1936 came up together
        // and no amount of scrolling separated them, because they were at the
        // same point on the strip.
        let slots: Vec<f32> = (0..4).map(|i| slot_year(1936, i, 4)).collect();
        for pair in slots.windows(2) {
            assert!(pair[1] > pair[0], "{slots:?} must be in order and distinct");
        }
        // Inside their own year, so the number written on the strip still says
        // where they are.
        assert!(
            slots.iter().all(|at| (*at - 1936.0).abs() < 0.5),
            "{slots:?}"
        );
        // And centred on it, rather than drifting towards the next year.
        let middle: f32 = slots.iter().sum::<f32>() / slots.len() as f32;
        assert!((middle - 1936.0).abs() < 1e-5, "{middle}");

        // The ordinary case is untouched.
        assert_eq!(slot_year(1840, 0, 1), 1840.0);
    }

    #[test]
    fn zoomed_right_in_the_playhead_can_pick_one_of_four_out() {
        // Four sheets of one year, and a window narrow enough to separate
        // them: standing on one, it must be the brightest by a clear margin
        // rather than one of four identical maps.
        let window = window_years(MAX_SCALE);
        let slots: Vec<f32> = (0..4).map(|i| slot_year(1936, i, 4)).collect();
        let on_second = slots[1];
        let lit: Vec<f32> = slots
            .iter()
            .map(|at| opacity_at(*at, on_second, window))
            .collect();
        assert!((lit[1] - PEAK).abs() < 1e-6, "{lit:?}");
        assert!(lit[0] < lit[1] * 0.6, "{lit:?}");
        assert_eq!(lit[3], 0.0, "the far one is off entirely: {lit:?}");

        // At the default scale they are all up together — which is the state
        // being complained about, and the reason the zoom is the way out of it.
        let wide = window_years(DEFAULT_SCALE);
        assert!(
            slots
                .iter()
                .all(|at| opacity_at(*at, on_second, wide) > 0.0)
        );
    }

    #[test]
    fn the_zoom_stays_inside_what_is_usable() {
        assert_eq!(zoomed(DEFAULT_SCALE, 1000.0), MAX_SCALE);
        assert_eq!(zoomed(DEFAULT_SCALE, 0.0001), MIN_SCALE);
        assert!((zoomed(DEFAULT_SCALE, 2.0) - DEFAULT_SCALE * 2.0).abs() < 1e-6);
    }

    #[test]
    fn the_year_under_the_pointer_does_not_move_while_zooming() {
        // 200 points down a strip whose top is 1800, at 14 points a year.
        let (top, scale, offset) = (1800.0_f32, DEFAULT_SCALE, 200.0_f32);
        let under = top + offset / scale;

        let closer = zoomed(scale, 2.0);
        let top = top_after_zoom(under, offset, closer);
        assert!(
            ((top + offset / closer) - under).abs() < 1e-3,
            "the year under the pointer slid away"
        );
    }
}
