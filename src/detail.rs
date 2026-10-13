//! Sharp pixels for the part of a tiled image you are actually looking at.
//!
//! The viewer's base texture is one whole pyramid level capped at 8192 pixels,
//! which is plenty until you zoom past it — after that you are magnifying a
//! downscale and the pixels go soft. This decodes just the visible rectangle
//! from whichever level matches the current zoom and paints it over the top.
//!
//! Cost is set by the size of the window, not the size of the image: a 900x700
//! region of a 100 MP level measured 1.5 ms against 106 ms for the whole level.
//!
//! Only the most recent request matters. Panning generates them continuously,
//! so a queue would build a backlog of rectangles the user has already left;
//! instead a pending request is simply replaced.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Condvar, Mutex};

use eframe::egui;

use crate::cog::{Region, TiledTiff};

/// What to decode: a rectangle of one level of one file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Request {
    pub path: PathBuf,
    pub level: usize,
    pub region: Region,
}

/// A decoded region, ready to upload.
pub struct Ready {
    pub request: Request,
    pub image: egui::ColorImage,
    pub decode_ms: f32,
    /// Squares actually read for this view.
    pub squares: usize,
}

/// The detail currently on screen, once uploaded.
pub struct Patch {
    pub request: Request,
    pub texture: egui::TextureHandle,
    pub decode_ms: f32,
    /// Squares actually read for this view.
    pub squares: usize,
}

struct Shared {
    pending: Mutex<Option<Request>>,
    wake: Condvar,
    shutdown: AtomicBool,
}

pub struct DetailLoader {
    shared: Arc<Shared>,
    results: Receiver<Ready>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// What has been asked for but not yet drained, so the same rectangle is
    /// not requested on every frame while it is being decoded.
    in_flight: Option<Request>,
}

impl DetailLoader {
    pub fn new(ctx: egui::Context) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::new(None),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let (sender, results) = channel();

        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("geo-ref_mapper-detail".to_owned())
                .spawn(move || worker(shared, sender, ctx))
                .ok()
        };

        Self {
            shared,
            results,
            worker,
            in_flight: None,
        }
    }

    /// Replaces whatever was queued. Returns false when this is already the
    /// request in flight, so the caller can skip it.
    pub fn request(&mut self, request: Request) -> bool {
        if self.in_flight.as_ref() == Some(&request) {
            return false;
        }
        self.in_flight = Some(request.clone());
        *self.shared.pending.lock().expect("detail queue poisoned") = Some(request);
        self.shared.wake.notify_one();
        true
    }

    /// Forgets what is in flight, so the next request is always sent. Call when
    /// the image changes.
    pub fn reset(&mut self) {
        self.in_flight = None;
        *self.shared.pending.lock().expect("detail queue poisoned") = None;
    }

    pub fn poll(&mut self) -> Option<Ready> {
        let mut latest = None;
        loop {
            match self.results.try_recv() {
                Ok(ready) => latest = Some(ready),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        latest
    }
}

impl Drop for DetailLoader {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let began = std::time::Instant::now();
            let _ = worker.join();
            crate::metrics::report_slow_join("the detail loader", began.elapsed());
        }
    }
}

fn worker(shared: Arc<Shared>, sender: Sender<Ready>, ctx: egui::Context) {
    // Reopening the file per request would re-read the whole IFD chain, which
    // for a nine-level pyramid is a lot of seeking to learn nothing new.
    let mut open: Option<(PathBuf, TiledTiff)> = None;

    loop {
        let request = {
            let mut pending = shared.pending.lock().expect("detail queue poisoned");
            loop {
                if shared.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(request) = pending.take() {
                    break request;
                }
                pending = shared.wake.wait(pending).expect("detail queue poisoned");
            }
        };

        if open.as_ref().map(|(path, _)| path) != Some(&request.path) {
            match TiledTiff::open(&request.path) {
                Ok(tiff) => open = Some((request.path.clone(), tiff)),
                Err(_) => {
                    open = None;
                    continue; // Not a tiled TIFF; nothing to sharpen.
                }
            }
        }
        let Some((_, tiff)) = open.as_mut() else {
            continue;
        };

        let started = std::time::Instant::now();
        let squares = tiff.squares_for(request.level, request.region);
        let Ok(rgba) = tiff.decode_region(request.level, request.region) else {
            continue;
        };
        let size = [rgba.width() as usize, rgba.height() as usize];
        let ready = Ready {
            request,
            image: egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()),
            decode_ms: started.elapsed().as_secs_f32() * 1000.0,
            squares,
        };

        if sender.send(ready).is_err() {
            return; // The app is gone.
        }
        ctx.request_repaint();
    }
}

/// Works out which level and rectangle the current view needs.
///
/// `visible` is the part of the canvas the image actually covers and `drawn` is
/// where the whole image would land — their ratio is the zoom. Returns `None`
/// when the base texture already has at least as many pixels as the screen can
/// show, which is the common case and must cost nothing.
pub fn needed(
    tiff_levels: &[(u32, u32)],
    base_width: u32,
    visible: egui::Rect,
    drawn: egui::Rect,
    level_of: impl Fn(u32, f64) -> usize,
) -> Option<(usize, Region)> {
    if drawn.width() <= base_width as f32 || visible.width() < 1.0 || visible.height() < 1.0 {
        return None;
    }

    let fraction = (visible.width() / drawn.width()) as f64;
    let mut level = level_of(visible.width().ceil() as u32, fraction);

    // A patch is uploaded as one texture, and a graphics card will not hold one
    // wider than its limit — asking anyway aborts the process rather than
    // failing, so the size has to be right before the decode, not after.
    //
    // Answered by dropping to a coarser layer rather than by cropping: the
    // whole visible area still gets a patch, at the resolution that fits. Each
    // layer is half the last, so at most a few steps.
    let fits = |index: usize| -> bool {
        tiff_levels.get(index).is_some_and(|(width, height)| {
            let across = ((u_span(visible, drawn) * f64::from(*width)).ceil() as u32).max(1);
            let down = ((v_span(visible, drawn) * f64::from(*height)).ceil() as u32).max(1);
            across <= crate::loader::MAX_TEXTURE_EDGE && down <= crate::loader::MAX_TEXTURE_EDGE
        })
    };
    while level + 1 < tiff_levels.len() && !fits(level) {
        level += 1;
    }
    let (level_width, level_height) = *tiff_levels.get(level)?;

    // Where the visible rectangle sits within the image, as a fraction.
    let u0 = ((visible.left() - drawn.left()) / drawn.width()).clamp(0.0, 1.0) as f64;
    let u1 = ((visible.right() - drawn.left()) / drawn.width()).clamp(0.0, 1.0) as f64;
    let v0 = ((visible.top() - drawn.top()) / drawn.height()).clamp(0.0, 1.0) as f64;
    let v1 = ((visible.bottom() - drawn.top()) / drawn.height()).clamp(0.0, 1.0) as f64;

    let x = (u0 * level_width as f64).floor() as u32;
    let y = (v0 * level_height as f64).floor() as u32;
    let region = Region {
        x,
        y,
        width: ((u1 * level_width as f64).ceil() as u32)
            .saturating_sub(x)
            .max(1),
        height: ((v1 * level_height as f64).ceil() as u32)
            .saturating_sub(y)
            .max(1),
    };
    (!region.is_empty()).then_some((level, region))
}

/// How much of the image's width the visible rectangle covers, as a fraction.
fn u_span(visible: egui::Rect, drawn: egui::Rect) -> f64 {
    let u0 = ((visible.left() - drawn.left()) / drawn.width()).clamp(0.0, 1.0) as f64;
    let u1 = ((visible.right() - drawn.left()) / drawn.width()).clamp(0.0, 1.0) as f64;
    (u1 - u0).max(0.0)
}

/// And of its height.
fn v_span(visible: egui::Rect, drawn: egui::Rect) -> f64 {
    let v0 = ((visible.top() - drawn.top()) / drawn.height()).clamp(0.0, 1.0) as f64;
    let v1 = ((visible.bottom() - drawn.top()) / drawn.height()).clamp(0.0, 1.0) as f64;
    (v1 - v0).max(0.0)
}

/// Where a decoded region belongs on screen, given where the whole image is.
/// Derived from the region rather than from the viewport, so rounding when the
/// region was snapped to whole pixels cannot shift the patch off its base.
pub fn patch_rect(
    region: Region,
    level_width: u32,
    level_height: u32,
    drawn: egui::Rect,
) -> egui::Rect {
    let u0 = region.x as f32 / level_width.max(1) as f32;
    let v0 = region.y as f32 / level_height.max(1) as f32;
    let u1 = (region.x + region.width) as f32 / level_width.max(1) as f32;
    let v1 = (region.y + region.height) as f32 / level_height.max(1) as f32;

    egui::Rect::from_min_max(
        egui::pos2(
            drawn.left() + u0 * drawn.width(),
            drawn.top() + v0 * drawn.height(),
        ),
        egui::pos2(
            drawn.left() + u1 * drawn.width(),
            drawn.top() + v1 * drawn.height(),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Rect, pos2};

    const LEVELS: &[(u32, u32)] = &[
        (48896, 32768),
        (24448, 16384),
        (12224, 8192),
        (6112, 4096),
        (3056, 2048),
    ];

    /// The same rule the canvas and the decoder both use.
    fn level_of(screen_pixels: u32, fraction: f64) -> usize {
        crate::cog::level_for_zoom(LEVELS, screen_pixels, fraction)
    }

    #[test]
    fn a_patch_never_asks_for_more_than_a_graphics_card_will_hold() {
        let limit = crate::loader::MAX_TEXTURE_EDGE;

        // A wide window over a mostly-visible image: the level that matches the
        // zoom is far wider than the card's limit, so a coarser one must be
        // chosen instead of handing over a texture that aborts the process.
        let drawn = Rect::from_min_max(pos2(0.0, 0.0), pos2(20000.0, 13400.0));
        let visible = Rect::from_min_max(pos2(500.0, 300.0), pos2(19500.0, 13100.0));
        let (_, region) = needed(LEVELS, 8192, visible, drawn, level_of).expect("detail wanted");
        assert!(
            region.width <= limit && region.height <= limit,
            "region {}x{} exceeds the {limit} limit",
            region.width,
            region.height
        );

        // And across a sweep of zooms and window positions, never once.
        for shown in [0.05, 0.2, 0.5, 0.9, 1.0] {
            for scale in [9000.0, 20000.0, 48000.0, 120000.0] {
                let drawn = Rect::from_min_max(pos2(0.0, 0.0), pos2(scale, scale * 0.67));
                let visible =
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(scale * shown, scale * 0.67 * shown));
                if let Some((_, region)) = needed(LEVELS, 8192, visible, drawn, level_of) {
                    assert!(
                        region.width <= limit && region.height <= limit,
                        "{shown} of a {scale}px image asked for {}x{}",
                        region.width,
                        region.height
                    );
                }
            }
        }
    }

    #[test]
    fn an_image_without_a_pyramid_still_gets_a_patch() {
        // A downloaded scan has one level and nothing coarser to fall back to,
        // so the region stays as it is and the size is answered at the upload
        // instead, by scaling the decoded patch down.
        let single: &[(u32, u32)] = &[(32981, 18197)];
        let level_of =
            |pixels: u32, fraction: f64| crate::cog::level_for_zoom(single, pixels, fraction);
        let drawn = Rect::from_min_max(pos2(0.0, 0.0), pos2(40000.0, 22000.0));
        let visible = Rect::from_min_max(pos2(0.0, 0.0), pos2(20000.0, 11000.0));
        let (level, region) =
            needed(single, 8192, visible, drawn, level_of).expect("detail wanted");
        assert_eq!(level, 0);
        assert!(region.width > crate::loader::MAX_TEXTURE_EDGE);
    }

    #[test]
    fn nothing_is_needed_until_the_base_texture_runs_out_of_pixels() {
        // The whole 8192-wide base drawn at 1000px on screen: no detail needed.
        let drawn = Rect::from_min_max(pos2(0.0, 0.0), pos2(1000.0, 700.0));
        assert!(needed(LEVELS, 8192, drawn, drawn, level_of).is_none());

        // Drawn at exactly the base width is still enough.
        let exact = Rect::from_min_max(pos2(0.0, 0.0), pos2(8192.0, 5400.0));
        assert!(needed(LEVELS, 8192, exact, exact, level_of).is_none());
    }

    #[test]
    fn zooming_past_the_base_asks_for_the_visible_rectangle_only() {
        // The image is drawn 40000px wide; the window shows 2000px of it.
        let drawn = Rect::from_min_max(pos2(-10000.0, -5000.0), pos2(30000.0, 21000.0));
        let visible = Rect::from_min_max(pos2(0.0, 0.0), pos2(2000.0, 1200.0));

        let (level, region) =
            needed(LEVELS, 8192, visible, drawn, level_of).expect("detail wanted");
        let (level_width, level_height) = LEVELS[level];

        // The region must be a small slice, not the whole level.
        assert!(
            region.width < level_width / 2,
            "region {} should be far narrower than level {level_width}",
            region.width
        );
        // And it must start a quarter of the way in, where the window is.
        let expected_x = (10000.0 / 40000.0 * level_width as f64) as u32;
        assert!(
            region.x.abs_diff(expected_x) <= 2,
            "region.x {} should be about {expected_x}",
            region.x
        );
        assert!(region.y > 0 && region.y < level_height);
    }

    #[test]
    fn a_region_at_the_image_edge_stays_inside_the_level() {
        // Panned hard against the top-left corner.
        let drawn = Rect::from_min_max(pos2(0.0, 0.0), pos2(40000.0, 26000.0));
        let visible = Rect::from_min_max(pos2(0.0, 0.0), pos2(2000.0, 1200.0));
        let (level, region) = needed(LEVELS, 8192, visible, drawn, level_of).unwrap();
        let (level_width, level_height) = LEVELS[level];

        assert_eq!(region.x, 0);
        assert_eq!(region.y, 0);
        assert!(region.x + region.width <= level_width);
        assert!(region.y + region.height <= level_height);
    }

    #[test]
    fn the_patch_lands_exactly_where_its_region_sits_on_the_image() {
        let drawn = Rect::from_min_max(pos2(100.0, 50.0), pos2(1100.0, 550.0));
        // The middle quarter of a 1000x500 level.
        let region = Region {
            x: 250,
            y: 125,
            width: 500,
            height: 250,
        };
        let rect = patch_rect(region, 1000, 500, drawn);

        assert!((rect.left() - 350.0).abs() < 0.01, "{rect:?}");
        assert!((rect.top() - 175.0).abs() < 0.01, "{rect:?}");
        assert!((rect.width() - 500.0).abs() < 0.01, "{rect:?}");
        assert!((rect.height() - 250.0).abs() < 0.01, "{rect:?}");
    }

    #[test]
    fn a_full_image_region_maps_back_to_the_whole_drawn_rect() {
        let drawn = Rect::from_min_max(pos2(-20.0, -30.0), pos2(180.0, 170.0));
        let region = Region {
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };
        let rect = patch_rect(region, 800, 600, drawn);
        assert!((rect.left() - drawn.left()).abs() < 0.01);
        assert!((rect.right() - drawn.right()).abs() < 0.01);
        assert!((rect.top() - drawn.top()).abs() < 0.01);
        assert!((rect.bottom() - drawn.bottom()).abs() < 0.01);
    }
}
