//! Holding one picture on screen while the next one arrives.
//!
//! Stepping to the next map used to empty the canvas for as long as the decode
//! took, so walking a folder read as a series of flashes rather than as one
//! picture after another. The picture being left is kept up instead, and the
//! new one comes up over it.
//!
//! Never through black: the one being left stays fully opaque underneath for
//! the whole crossing, and only the arriving picture changes. There is no
//! moment when neither is drawn.

use std::path::{Path, PathBuf};

/// How long the crossing takes.
///
/// Short enough to feel like a cut rather than an effect, long enough that the
/// eye reads it as one picture becoming another instead of a jump.
pub const SECONDS: f64 = 0.14;

/// One frame of a crossing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    /// How opaque to draw the arriving picture, 0 to 1.
    pub alpha: f32,
    /// True on the one frame the picture first appeared — the moment to take
    /// hold of whatever was on the canvas, so it can be faded out from under
    /// it.
    pub arrived: bool,
}

impl Step {
    /// Whether the crossing is still running and the canvas needs redrawing.
    pub fn running(&self) -> bool {
        self.alpha < 1.0
    }
}

/// Which picture is on screen, and when its texture first appeared.
#[derive(Default)]
pub struct Crossfade {
    showing: Option<(PathBuf, f64)>,
}

impl Crossfade {
    /// Where the crossing to `path` has got to.
    ///
    /// Call once a frame, and **only once the texture is actually ready**. The
    /// crossing belongs to the arrival, not to the selection moving: those are
    /// a whole decode apart on a large map, and starting the clock at the
    /// selection would spend the fade on an empty canvas.
    pub fn advance(&mut self, path: &Path, now: f64) -> Step {
        let started = match &self.showing {
            Some((showing, started)) if showing == path => *started,
            _ => {
                self.showing = Some((path.to_owned(), now));
                return Step {
                    alpha: 0.0,
                    arrived: true,
                };
            }
        };
        // Clamped both ways: a clock that jumps backwards — which is what a
        // suspend and resume looks like from here — must not run the fade in
        // reverse or park it at nothing.
        let alpha = ((now - started) / SECONDS.max(f64::MIN_POSITIVE)) as f32;
        Step {
            alpha: alpha.clamp(0.0, 1.0),
            arrived: false,
        }
    }

    /// Forgets what is on screen, so the next picture arrives fresh.
    ///
    /// For a decode that failed or a selection that names nothing: holding the
    /// last picture there would be showing the wrong one, and quietly — it
    /// looks exactly like a picture that opened.
    pub fn clear(&mut self) {
        self.showing = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(name: &str) -> PathBuf {
        PathBuf::from(format!("/maps/{name}.tif"))
    }

    #[test]
    fn a_picture_arrives_at_nothing_and_reaches_full() {
        let mut fade = Crossfade::default();
        let first = fade.advance(&map("a"), 100.0);
        assert_eq!(first.alpha, 0.0);
        assert!(
            first.arrived,
            "the first frame is where the old one is taken"
        );

        // Halfway through, and not claiming to have arrived again.
        let middle = fade.advance(&map("a"), 100.0 + SECONDS / 2.0);
        assert!((middle.alpha - 0.5).abs() < 0.01, "{}", middle.alpha);
        assert!(!middle.arrived);
        assert!(middle.running());

        let over = fade.advance(&map("a"), 100.0 + SECONDS);
        assert_eq!(over.alpha, 1.0);
        assert!(
            !over.running(),
            "a finished fade must stop asking to redraw"
        );
    }

    #[test]
    fn a_different_picture_starts_its_own_crossing() {
        let mut fade = Crossfade::default();
        fade.advance(&map("a"), 0.0);
        fade.advance(&map("a"), 10.0);

        let next = fade.advance(&map("b"), 10.0);
        assert_eq!(next.alpha, 0.0);
        assert!(next.arrived, "the picture before it must be taken hold of");
    }

    #[test]
    fn the_same_picture_is_not_restarted_by_being_drawn_again() {
        // Every frame calls this. Treating each one as an arrival would hold
        // the previous picture forever and never let the fade finish.
        let mut fade = Crossfade::default();
        fade.advance(&map("a"), 0.0);
        for frame in 1..20 {
            let step = fade.advance(&map("a"), f64::from(frame) * 0.016);
            assert!(!step.arrived);
        }
        assert_eq!(fade.advance(&map("a"), 100.0).alpha, 1.0);
    }

    #[test]
    fn clearing_makes_the_next_picture_arrive_fresh() {
        let mut fade = Crossfade::default();
        fade.advance(&map("a"), 0.0);
        fade.clear();
        // Same picture, but nothing is on screen any more, so it is an
        // arrival: after a failed decode the canvas is empty and there is
        // nothing to fade out from under.
        let again = fade.advance(&map("a"), 0.0);
        assert!(again.arrived);
        assert_eq!(again.alpha, 0.0);
    }

    #[test]
    fn a_clock_that_jumps_backwards_does_not_run_the_fade_in_reverse() {
        let mut fade = Crossfade::default();
        fade.advance(&map("a"), 500.0);
        let back = fade.advance(&map("a"), 400.0);
        assert_eq!(back.alpha, 0.0, "held at the start rather than negative");
    }
}
