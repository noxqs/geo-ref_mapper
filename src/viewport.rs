//! Pan and zoom state for the image canvas.
//!
//! Zoom is stored as a mode rather than a bare number so that Fit and Fill keep
//! following the window as it resizes; the moment the user zooms by hand it
//! switches to [`ZoomMode::Free`] and stops tracking.

use eframe::egui::{Pos2, Rect, Vec2};

pub const MIN_ZOOM: f32 = 0.02;
pub const MAX_ZOOM: f32 = 64.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ZoomMode {
    /// Whole image visible.
    Fit,
    /// Cover the canvas, cropping the long axis.
    Fill,
    /// One image pixel per point.
    Actual,
    /// Whatever the user zoomed to.
    Free,
}

impl ZoomMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fit => "Fit",
            Self::Fill => "Fill",
            Self::Actual => "1:1",
            Self::Free => "Free",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub mode: ZoomMode,
    /// Only meaningful in [`ZoomMode::Free`].
    free_zoom: f32,
    /// Image centre relative to the canvas centre, in points.
    pub offset: Vec2,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            mode: ZoomMode::Fit,
            free_zoom: 1.0,
            offset: Vec2::ZERO,
        }
    }
}

impl Viewport {
    /// Back to a freshly opened image.
    pub fn reset(&mut self) {
        self.mode = ZoomMode::Fit;
        self.offset = Vec2::ZERO;
    }

    /// Moving to another picture, keeping how it is being looked at.
    ///
    /// Comparing two scans of the same map means having them at the same size,
    /// and going back to Fit between them threw that away — every step through
    /// a folder had to be followed by zooming in again to the same place.
    ///
    /// The **mode** is what carries, not the scale in points. Fit and Fill stay
    /// themselves, so the next picture is fitted to *its* own size rather than
    /// to the last one's; 1:1 stays one pixel per point, which is the same
    /// promise whatever the picture; and a free zoom keeps its factor.
    ///
    /// The pan is dropped in every case. It is measured from the centre in
    /// points, so on a picture of another size it means somewhere else entirely
    /// — and arriving at a new map already scrolled off into its margin is
    /// worse than arriving centred.
    pub fn carry(&mut self) {
        self.offset = Vec2::ZERO;
    }

    pub fn set_mode(&mut self, mode: ZoomMode) {
        self.mode = mode;
        if mode != ZoomMode::Free {
            self.offset = Vec2::ZERO;
        }
    }

    /// Scale factor actually in use, given the current canvas and image.
    pub fn zoom(&self, canvas: Vec2, image: Vec2) -> f32 {
        if image.x <= 0.0 || image.y <= 0.0 {
            return 1.0;
        }
        match self.mode {
            ZoomMode::Fit => (canvas.x / image.x).min(canvas.y / image.y),
            ZoomMode::Fill => (canvas.x / image.x).max(canvas.y / image.y),
            ZoomMode::Actual => 1.0,
            ZoomMode::Free => self.free_zoom,
        }
        .clamp(MIN_ZOOM, MAX_ZOOM)
    }

    /// The offset that leaves the picture on exactly the same screen pixels
    /// after the window and canvas have both changed.
    ///
    /// The image centre sits at `window.min + canvas.center() + offset`. Going
    /// full screen moves the window's origin, grows it over the taskbar, and
    /// takes the side panels away — so all three terms change. Solving for the
    /// new offset keeps the product of them fixed, which is what "it did not
    /// move" means.
    pub fn anchored_offset(
        window_before: Rect,
        canvas_before: Rect,
        offset_before: Vec2,
        window_after: Rect,
        canvas_after: Rect,
    ) -> Vec2 {
        let on_screen = window_before.min + canvas_before.center().to_vec2() + offset_before;
        on_screen - window_after.min - canvas_after.center().to_vec2()
    }

    /// Pins the zoom at whatever it currently works out to.
    ///
    /// [`ZoomMode::Fit`] and [`ZoomMode::Fill`] are recomputed from the canvas
    /// every frame, which is what makes them follow a window resize. Going
    /// full screen resizes the canvas, so a fitted image would re-fit and jump
    /// to a new size at the very moment the transition should be invisible.
    /// Freezing the current factor into [`ZoomMode::Free`] first keeps the
    /// picture exactly the size it already was.
    pub fn freeze(&mut self, canvas: Vec2, image: Vec2) {
        self.free_zoom = self.zoom(canvas, image);
        self.mode = ZoomMode::Free;
    }

    /// Pins the zoom at an exact factor.
    ///
    /// Used when arriving from somewhere that already decided how big the
    /// picture should be — the map, which was drawing the same picture at a
    /// known scale a moment ago.
    pub fn set_zoom(&mut self, zoom: f32) {
        self.free_zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.mode = ZoomMode::Free;
    }

    /// Where the image lands on screen.
    pub fn image_rect(&self, canvas: Rect, image: Vec2) -> Rect {
        let zoom = self.zoom(canvas.size(), image);
        Rect::from_center_size(canvas.center() + self.offset, image * zoom)
    }

    /// Multiplies the zoom while keeping `pointer` over the same image pixel.
    pub fn zoom_at(&mut self, pointer: Pos2, canvas: Rect, image: Vec2, factor: f32) {
        let old_zoom = self.zoom(canvas.size(), image);
        let new_zoom = (old_zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if (new_zoom - old_zoom).abs() < f32::EPSILON {
            return;
        }

        // The point under the cursor stays fixed when the image centre moves to
        // `pointer - k * (pointer - centre)`.
        let ratio = new_zoom / old_zoom;
        let centre = canvas.center() + self.offset;
        let new_centre = pointer - ratio * (pointer - centre);

        self.mode = ZoomMode::Free;
        self.free_zoom = new_zoom;
        self.offset = new_centre - canvas.center();
        self.clamp_offset(canvas, image);
    }

    /// Whether any of the picture is off the canvas — that is, whether there
    /// is anywhere to pan to.
    ///
    /// Asked of the pixels rather than of the mode. Fit is the usual way to
    /// have the whole picture on screen, but wheeling backwards lands in
    /// [`ZoomMode::Free`] with just as little hanging over the edge, and the
    /// two should behave the same: dragging a picture that is entirely visible
    /// cannot mean "move the picture", because it is already all there.
    ///
    /// The half-point of slack keeps a fitted picture — whose scaled size
    /// equals the canvas on its binding axis to within a rounding error — from
    /// reporting itself as pannable by a fraction of a pixel.
    pub fn pannable(&self, canvas: Rect, image: Vec2) -> bool {
        let scaled = image * self.zoom(canvas.size(), image);
        scaled.x > canvas.width() + 0.5 || scaled.y > canvas.height() + 0.5
    }

    pub fn pan(&mut self, delta: Vec2, canvas: Rect, image: Vec2) {
        // Panning a fitted image is meaningless — there is nothing off-screen.
        if self.mode == ZoomMode::Fit {
            return;
        }
        self.offset += delta;
        self.clamp_offset(canvas, image);
    }

    /// Keeps the image from being dragged off the canvas: axes that overflow
    /// may pan up to their overhang, axes that fit stay centred.
    pub fn clamp_offset(&mut self, canvas: Rect, image: Vec2) {
        let scaled = image * self.zoom(canvas.size(), image);

        let overflow_x = (scaled.x - canvas.width()) * 0.5;
        self.offset.x = if overflow_x > 0.0 {
            self.offset.x.clamp(-overflow_x, overflow_x)
        } else {
            0.0
        };

        let overflow_y = (scaled.y - canvas.height()) * 0.5;
        self.offset.y = if overflow_y > 0.0 {
            self.offset.y.clamp(-overflow_y, overflow_y)
        } else {
            0.0
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{pos2, vec2};

    fn canvas() -> Rect {
        Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))
    }

    #[test]
    fn fit_shows_the_whole_image() {
        let viewport = Viewport::default();
        // 1600x600 into 800x600 is limited by width.
        assert_eq!(viewport.zoom(canvas().size(), vec2(1600.0, 600.0)), 0.5);
    }

    #[test]
    fn fill_covers_the_canvas() {
        let mut viewport = Viewport::default();
        viewport.set_mode(ZoomMode::Fill);
        // Height is the binding axis when filling this one.
        assert_eq!(viewport.zoom(canvas().size(), vec2(1600.0, 600.0)), 1.0);
    }

    #[test]
    fn a_fitted_picture_has_nowhere_to_pan() {
        let viewport = Viewport::default();
        // The binding axis lands exactly on the canvas edge; a hair of
        // rounding there must not read as "there is more to see".
        assert!(!viewport.pannable(canvas(), vec2(1600.0, 600.0)));
        assert!(!viewport.pannable(canvas(), vec2(4000.0, 3000.0)));
    }

    #[test]
    fn a_picture_zoomed_past_the_canvas_can_be_pulled_about() {
        let mut viewport = Viewport::default();
        viewport.set_mode(ZoomMode::Actual);
        assert!(viewport.pannable(canvas(), vec2(4000.0, 3000.0)));
        // Only one axis need overflow: a long panorama fitted vertically
        // still has its ends off the sides.
        viewport.set_mode(ZoomMode::Fill);
        assert!(viewport.pannable(canvas(), vec2(1600.0, 600.0)));
    }

    #[test]
    fn zooming_out_by_hand_is_as_unpannable_as_fitting() {
        // The mode says Free, but the whole picture is on screen just the
        // same — which is the case that made asking the mode the wrong
        // question.
        let mut viewport = Viewport::default();
        viewport.set_zoom(0.1);
        assert!(!viewport.pannable(canvas(), vec2(4000.0, 3000.0)));
    }

    #[test]
    fn actual_size_is_one_to_one() {
        let mut viewport = Viewport::default();
        viewport.set_mode(ZoomMode::Actual);
        assert_eq!(viewport.zoom(canvas().size(), vec2(4000.0, 3000.0)), 1.0);
    }

    #[test]
    fn zoom_at_keeps_the_pointer_over_the_same_pixel() {
        let canvas = canvas();
        let image = vec2(800.0, 600.0);
        let mut viewport = Viewport::default();

        let pointer = pos2(200.0, 150.0);
        let before = viewport.image_rect(canvas, image);
        let uv_before = (pointer - before.min) / before.size();

        viewport.zoom_at(pointer, canvas, image, 2.0);

        let after = viewport.image_rect(canvas, image);
        let uv_after = (pointer - after.min) / after.size();
        assert!(
            (uv_before.x - uv_after.x).abs() < 1e-4 && (uv_before.y - uv_after.y).abs() < 1e-4,
            "pointer moved from {uv_before:?} to {uv_after:?}"
        );
    }

    #[test]
    fn zooming_by_hand_switches_to_free_mode() {
        let mut viewport = Viewport::default();
        assert_eq!(viewport.mode, ZoomMode::Fit);
        viewport.zoom_at(pos2(400.0, 300.0), canvas(), vec2(800.0, 600.0), 2.0);
        assert_eq!(viewport.mode, ZoomMode::Free);
        assert_eq!(viewport.zoom(canvas().size(), vec2(800.0, 600.0)), 2.0);
    }

    #[test]
    fn zoom_is_clamped_at_both_ends() {
        let mut viewport = Viewport::default();
        let image = vec2(800.0, 600.0);
        viewport.zoom_at(pos2(0.0, 0.0), canvas(), image, 1000.0);
        assert_eq!(viewport.zoom(canvas().size(), image), MAX_ZOOM);

        viewport.zoom_at(pos2(0.0, 0.0), canvas(), image, 0.000_01);
        assert_eq!(viewport.zoom(canvas().size(), image), MIN_ZOOM);
    }

    #[test]
    fn a_fitted_image_cannot_be_panned() {
        let mut viewport = Viewport::default();
        viewport.pan(vec2(100.0, 100.0), canvas(), vec2(800.0, 600.0));
        assert_eq!(viewport.offset, Vec2::ZERO);
    }

    #[test]
    fn panning_stops_at_the_image_edge() {
        let canvas = canvas();
        let image = vec2(800.0, 600.0);
        let mut viewport = Viewport::default();
        viewport.zoom_at(canvas.center(), canvas, image, 2.0);

        // Zoomed 2x, the image is 1600x1200, so it overhangs by 400x300.
        viewport.pan(vec2(10_000.0, 10_000.0), canvas, image);
        assert_eq!(viewport.offset, vec2(400.0, 300.0));

        viewport.pan(vec2(-100_000.0, -100_000.0), canvas, image);
        assert_eq!(viewport.offset, vec2(-400.0, -300.0));
    }

    #[test]
    fn an_axis_that_fits_stays_centred() {
        let canvas = canvas();
        // Wide and short: after zooming, only x overflows.
        let image = vec2(1600.0, 100.0);
        let mut viewport = Viewport::default();
        viewport.set_mode(ZoomMode::Actual);
        viewport.pan(vec2(500.0, 500.0), canvas, image);

        assert_eq!(viewport.offset.x, 400.0, "x overhangs by (1600-800)/2");
        assert_eq!(viewport.offset.y, 0.0, "y fits, so it stays centred");
    }

    #[test]
    fn anchoring_holds_the_picture_on_the_same_pixels() {
        // A window inset on the desktop, with panels either side, going full
        // screen over the taskbar.
        let window_before = Rect::from_min_size(pos2(100.0, 80.0), vec2(1600.0, 900.0));
        let canvas_before = Rect::from_min_size(pos2(248.0, 74.0), vec2(1084.0, 706.0));
        let window_after = Rect::from_min_size(pos2(0.0, 0.0), vec2(1920.0, 1080.0));
        let canvas_after = Rect::from_min_size(pos2(0.0, 0.0), vec2(1920.0, 1080.0));

        let offset_before = vec2(30.0, -12.0);
        let after = Viewport::anchored_offset(
            window_before,
            canvas_before,
            offset_before,
            window_after,
            canvas_after,
        );

        // The image centre, in desktop coordinates, before and after.
        let screen_before = window_before.min + canvas_before.center().to_vec2() + offset_before;
        let screen_after = window_after.min + canvas_after.center().to_vec2() + after;
        assert!(
            (screen_before - screen_after).length() < 1e-3,
            "moved from {screen_before:?} to {screen_after:?}",
        );
    }

    #[test]
    fn freezing_keeps_the_size_when_the_canvas_changes() {
        let image = vec2(4000.0, 3000.0);
        let small = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let full = Rect::from_min_size(pos2(0.0, 0.0), vec2(2560.0, 1440.0));

        let mut viewport = Viewport::default(); // Fit
        let before = viewport.zoom(small.size(), image);

        // Without freezing, the bigger canvas re-fits and the picture jumps.
        assert!(
            viewport.zoom(full.size(), image) > before * 1.5,
            "a fitted image really does resize with the canvas",
        );

        viewport.freeze(small.size(), image);
        assert_eq!(viewport.mode, ZoomMode::Free);
        assert!(
            (viewport.zoom(full.size(), image) - before).abs() < 1e-4,
            "frozen, it stays the size it was when the window grew",
        );
    }

    #[test]
    fn a_deliberately_opened_picture_arrives_fitted() {
        // What opening from the file browser has to end at, from the worst
        // state the previous picture could have been left in: zoomed by hand
        // and panned into a corner.
        let canvas = canvas();
        let image = vec2(4000.0, 3000.0);
        let mut viewport = Viewport::default();
        viewport.zoom_at(pos2(0.0, 0.0), canvas, image, 8.0);
        viewport.pan(vec2(-5_000.0, -5_000.0), canvas, image);
        assert_eq!(viewport.mode, ZoomMode::Free);
        assert_ne!(viewport.offset, Vec2::ZERO);

        viewport.reset();
        assert_eq!(viewport.mode, ZoomMode::Fit);
        assert_eq!(viewport.offset, Vec2::ZERO);

        // And a `carry` afterwards — the selection landing runs one — must not
        // undo it. This is the ordering the two calls actually happen in.
        viewport.carry();
        assert_eq!(viewport.mode, ZoomMode::Fit);
        assert_eq!(viewport.zoom(canvas.size(), image), 0.2, "800/4000");
    }

    #[test]
    fn fit_follows_the_next_picture_rather_than_the_last_one() {
        // What stepping through a folder in Fit is supposed to do: the mode
        // carries, and each picture is fitted to *its own* size. Pinned because
        // it is the promise `carry` makes and nothing else asserts it.
        let canvas = canvas();
        let mut viewport = Viewport::default(); // Fit
        let wide = vec2(1600.0, 600.0);
        let tall = vec2(600.0, 2400.0);

        assert_eq!(viewport.zoom(canvas.size(), wide), 0.5, "width-limited");

        viewport.carry();
        assert_eq!(viewport.mode, ZoomMode::Fit, "the mode carries");
        assert_eq!(
            viewport.zoom(canvas.size(), tall),
            0.25,
            "the next picture is fitted to its own size, not the last one's"
        );
        assert_eq!(viewport.offset, Vec2::ZERO, "and arrives centred");
    }

    #[test]
    fn switching_to_a_preset_recentres() {
        let canvas = canvas();
        let image = vec2(1600.0, 1200.0);
        let mut viewport = Viewport::default();
        viewport.set_mode(ZoomMode::Actual);
        viewport.pan(vec2(100.0, 100.0), canvas, image);
        assert_ne!(viewport.offset, Vec2::ZERO);

        viewport.set_mode(ZoomMode::Fit);
        assert_eq!(viewport.offset, Vec2::ZERO);
    }

    #[test]
    fn a_degenerate_image_does_not_divide_by_zero() {
        let viewport = Viewport::default();
        assert_eq!(viewport.zoom(canvas().size(), Vec2::ZERO), 1.0);
    }
}
