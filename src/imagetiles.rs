//! Drawing a layered picture as squares, the way histoMap does it.
//!
//! The viewer's other path decodes a whole layer into one texture and then
//! overlays a sharper patch when you zoom past it. That works for a photograph
//! and is wrong for a map: the layer is picked once when the file opens, so
//! zooming never changes it, and opening always costs a whole layer however
//! little of it is on screen.
//!
//! Here there is no whole-layer texture at all. Every frame:
//!
//!   * the level is worked out from the zoom, so it changes as you zoom
//!   * only the squares inside the window are asked for
//!   * a coarser square already in hand is stretched over any that have not
//!     arrived, so the picture is never blank
//!
//! Which means the cost of a view is the size of the window, not the size of
//! the picture — a screenful of a gigapixel map costs the same as a screenful
//! of a small one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use eframe::egui::{Rect, pos2, vec2};

/// Threads decoding squares. Decoding one is a disk read and a decompress,
/// which is far too slow to do while drawing.
const WORKERS: usize = 4;

/// Outstanding requests before the oldest is dropped.
const QUEUE_LIMIT: usize = 512;

/// How far up the levels to look for a stand-in while a square arrives.
const MAX_COARSER_STEPS: usize = 4;

/// How many square textures to hold. Past this the ones furthest from the view
/// are dropped; the coarsest level is never dropped, because it is what covers
/// every gap.
const RESIDENT_LIMIT: usize = 512;

/// One square of one level of one file.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Square {
    pub level: usize,
    pub x: u32,
    pub y: u32,
}

/// What a level looks like, without holding the file open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LevelShape {
    pub width: u32,
    pub height: u32,
    pub tile_width: u32,
    pub tile_height: u32,
}

impl LevelShape {
    pub fn across(&self) -> u32 {
        self.width.div_ceil(self.tile_width.max(1))
    }

    pub fn down(&self) -> u32 {
        self.height.div_ceil(self.tile_height.max(1))
    }
}

/// Reads the shape of every level. Cheap: only the directories are read.
pub fn shapes(path: &Path) -> Vec<LevelShape> {
    let Ok(tiff) = crate::cog::TiledTiff::open(path) else {
        return Vec::new();
    };
    tiff.levels
        .iter()
        .map(|level| LevelShape {
            width: level.width,
            height: level.height,
            tile_width: level.tile_width,
            tile_height: level.tile_height,
        })
        .collect()
}

/// The level to draw at, given how big one full-size pixel is on screen.
///
/// `scale` is screen pixels per full-resolution image pixel: 1.0 means the
/// picture is at its own size, 0.25 means it is drawn a quarter as wide. Each
/// level is half the one above, so the level wanted is `log2(1/scale)`.
///
/// Rounded rather than floored. Flooring always errs towards the sharper level,
/// which sounds safe and means a picture drawn at 0.51 reads four times the
/// pixels it can show.
pub fn level_for_scale(levels: usize, scale: f32) -> usize {
    if levels == 0 {
        return 0;
    }
    let wanted = if scale > 0.0 {
        (1.0 / scale).log2().round()
    } else {
        levels as f32
    };
    (wanted.max(0.0) as usize).min(levels - 1)
}

/// Which squares of a level fall inside the window.
///
/// `visible` is in that level's own pixels. Nothing outside the picture is
/// returned, so an edge does not produce requests for squares that are not
/// there.
pub fn visible_squares(shape: LevelShape, visible: Rect) -> Vec<(u32, u32)> {
    let (tile_w, tile_h) = (shape.tile_width.max(1), shape.tile_height.max(1));
    if visible.width() <= 0.0 || visible.height() <= 0.0 {
        return Vec::new();
    }

    let first_x = (visible.left() / tile_w as f32).floor().max(0.0) as u32;
    let first_y = (visible.top() / tile_h as f32).floor().max(0.0) as u32;
    let last_x = ((visible.right() / tile_w as f32).ceil().max(0.0) as u32).min(shape.across());
    let last_y = ((visible.bottom() / tile_h as f32).ceil().max(0.0) as u32).min(shape.down());

    let mut out = Vec::new();
    for y in first_y..last_y {
        for x in first_x..last_x {
            out.push((x, y));
        }
    }
    out
}

/// Where a square sits in its level, and how much picture it actually holds —
/// all in that level's own pixels.
///
/// The last square in a row or column is cut short: the picture runs out before
/// the square does. Reading it back gives an image of only that much, so
/// drawing it into a full square's worth of screen stretches it sideways. This
/// is what says how wide it really is.
pub fn square_content(shape: LevelShape, x: u32, y: u32) -> Rect {
    let (tile_w, tile_h) = (shape.tile_width.max(1), shape.tile_height.max(1));
    let left = x * tile_w;
    let top = y * tile_h;
    let width = tile_w.min(shape.width.saturating_sub(left));
    let height = tile_h.min(shape.height.saturating_sub(top));
    Rect::from_min_size(
        pos2(left as f32, top as f32),
        vec2(width as f32, height as f32),
    )
}

/// Which part of a coarser square holds this square's ground, and at what
/// offset — for stretching a stand-in over a square that has not arrived.
pub fn coarser_part(square: Square, steps: usize, fine: LevelShape, coarse: LevelShape) -> Rect {
    let span = 1.0 / (1_u32 << steps) as f32;
    let left = (square.x & ((1 << steps) - 1)) as f32 * span;
    let top = (square.y & ((1 << steps) - 1)) as f32 * span;

    // Both squares can be cut short by the edge of the picture, and by
    // different amounts. The part to take is the ground this square covers,
    // measured against the ground the coarser one covers.
    let mine = square_content(fine, square.x, square.y);
    let theirs = square_content(coarse, square.x >> steps, square.y >> steps);
    let width = if theirs.width() > 0.0 {
        span * (mine.width() / fine.tile_width.max(1) as f32)
            / (theirs.width() / coarse.tile_width.max(1) as f32)
    } else {
        span
    };
    let height = if theirs.height() > 0.0 {
        span * (mine.height() / fine.tile_height.max(1) as f32)
            / (theirs.height() / coarse.tile_height.max(1) as f32)
    } else {
        span
    };
    Rect::from_min_size(pos2(left, top), vec2(width.min(1.0), height.min(1.0)))
}

/// What one frame of drawing cost, for the readout and the debug overlay.
#[derive(Clone, Copy, Debug)]
pub struct Painted {
    pub level: usize,
    pub levels: usize,
    pub drawn: usize,
    pub waiting: usize,
    /// The level being read, and how its squares are laid out.
    pub shape: LevelShape,
    /// The full-size picture, whatever level is being read.
    pub full: LevelShape,
    /// Screen pixels per full-size image pixel.
    pub scale: f32,
    /// Squares along the edge, which hold less than a whole square.
    pub partial: usize,
    /// Square textures held in memory.
    pub resident: usize,
}

/// Draws a layered picture as squares, at the level the zoom asks for.
///
/// `target` is where the whole picture goes on screen and `clip` is what can be
/// seen of it. Shared by the viewer and the map: both draw the same file the
/// same way, and the only difference between them is the rectangle and the
/// tint.
pub fn draw(
    squares: &mut Squares,
    painter: &eframe::egui::Painter,
    target: Rect,
    clip: Rect,
    tint: eframe::egui::Color32,
    debug: bool,
) -> Option<Painted> {
    let full = *squares.shapes().first()?;
    let scale = target.width() / full.width.max(1) as f32;
    draw_mapped(
        squares,
        painter,
        |pixel| {
            pos2(
                target.left() + pixel.0 as f32 * scale,
                target.top() + pixel.1 as f32 * scale,
            )
        },
        scale,
        clip,
        tint,
        debug,
    )
}

/// The same, for a picture that is not square to the screen.
///
/// `project` takes a full-size image pixel to where it lands. A hand-placed map
/// is turned as well as moved, so its squares are not rectangles on screen —
/// each is drawn as a four-cornered patch instead, with the corners put through
/// the same projection as everything else.
///
/// `scale` is screen pixels per full-size image pixel, which is what chooses
/// the level; it cannot be read off `project` without assuming there is no turn.
pub fn draw_mapped(
    squares: &mut Squares,
    painter: &eframe::egui::Painter,
    project: impl Fn((f64, f64)) -> eframe::egui::Pos2,
    scale: f32,
    clip: Rect,
    tint: eframe::egui::Color32,
    debug: bool,
) -> Option<Painted> {
    let shapes = squares.shapes().to_vec();
    let full = *shapes.first()?;
    if full.width == 0 || scale <= 0.0 {
        return None;
    }

    let level = level_for_scale(shapes.len(), scale);
    let shape = shapes[level];
    // How many full-size pixels one of this level's pixels is worth.
    let coarser_by = f64::from(full.width) / f64::from(shape.width.max(1));

    // Which of this level's pixels can be seen. Worked out by projecting the
    // picture's own corners and taking what falls inside the window, so it
    // holds however the picture is turned.
    let corners = [
        project((0.0, 0.0)),
        project((f64::from(full.width), 0.0)),
        project((0.0, f64::from(full.height))),
        project((f64::from(full.width), f64::from(full.height))),
    ];
    let drawn = Rect::from_points(&corners);
    let seen = drawn.intersect(clip);
    if seen.width() <= 0.0 || seen.height() <= 0.0 {
        return None;
    }
    // A turned picture's visible part is not a rectangle in its own pixels, so
    // the bounding box is used and a few squares off the edge of the window are
    // read. Cheap, and far simpler than clipping a rotated quadrilateral.
    let visible = {
        let mut min = (f64::MAX, f64::MAX);
        let mut max = (f64::MIN, f64::MIN);
        // Walk the window's corners back through the projection by sampling —
        // an inverse is not available for an arbitrary closure.
        let steps = 8;
        for row in 0..=steps {
            for column in 0..=steps {
                let pixel = (
                    f64::from(full.width) * f64::from(column) / f64::from(steps),
                    f64::from(full.height) * f64::from(row) / f64::from(steps),
                );
                if clip.expand(64.0).contains(project(pixel)) {
                    min = (min.0.min(pixel.0), min.1.min(pixel.1));
                    max = (max.0.max(pixel.0), max.1.max(pixel.1));
                }
            }
        }
        if min.0 > max.0 {
            // Nothing sampled inside the window: the picture is either far away
            // or so small that the sampling missed it. Take the whole of it.
            min = (0.0, 0.0);
            max = (f64::from(full.width), f64::from(full.height));
        }
        // A step of the sampling grid either side, since a sample landing
        // outside does not mean the ground between it and its neighbour does.
        let margin = (
            f64::from(full.width) / f64::from(steps),
            f64::from(full.height) / f64::from(steps),
        );
        Rect::from_min_max(
            pos2(
                ((min.0 - margin.0) / coarser_by) as f32,
                ((min.1 - margin.1) / coarser_by) as f32,
            ),
            pos2(
                ((max.0 + margin.0) / coarser_by) as f32,
                ((max.1 + margin.1) / coarser_by) as f32,
            ),
        )
    };

    let whole = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    let mut drawn = 0;
    let mut waiting = 0;
    let mut partial = 0;
    for (x, y) in visible_squares(shape, visible) {
        let square = Square { level, x, y };
        // Sized by how much picture the square holds. The last one in a row is
        // cut short by the edge, and drawing it full width stretches it.
        let content = square_content(shape, x, y);
        let at =
            |px: f32, py: f32| project((f64::from(px) * coarser_by, f64::from(py) * coarser_by));
        let quad = [
            at(content.left(), content.top()),
            at(content.right(), content.top()),
            at(content.right(), content.bottom()),
            at(content.left(), content.bottom()),
        ];
        let rect = Rect::from_points(&quad);
        if !rect.intersects(clip) {
            continue;
        }

        let cut_short = content.width() < shape.tile_width as f32
            || content.height() < shape.tile_height as f32;
        if cut_short {
            partial += 1;
        }

        match squares.get(&square) {
            Some(texture) => {
                patch(painter, texture.id(), quad, whole, tint);
                drawn += 1;
            }
            None => {
                waiting += 1;
                squares.request(square, true);
                if let Some((texture, part)) = squares.stand_in(square) {
                    patch(painter, texture.id(), quad, part, tint);
                }
            }
        }

        if debug {
            // Orange for a square cut short by the edge of the picture — the
            // ones whose size is easiest to get wrong, and where a stretch
            // shows first. Red for one that has not arrived.
            let colour = match (squares.get(&square).is_some(), cut_short) {
                (false, _) => eframe::egui::Color32::from_rgb(0xE0, 0x44, 0x46),
                (true, true) => eframe::egui::Color32::from_rgb(0xE8, 0xA3, 0x3D),
                (true, false) => eframe::egui::Color32::from_rgb(0x5b, 0x8c, 0xff),
            };
            painter.rect_stroke(
                rect,
                eframe::egui::CornerRadius::ZERO,
                eframe::egui::Stroke::new(1.0, colour),
                eframe::egui::StrokeKind::Inside,
            );
            painter.text(
                rect.min + vec2(3.0, 2.0),
                eframe::egui::Align2::LEFT_TOP,
                format!("{x},{y}"),
                eframe::egui::FontId::monospace(9.0),
                colour,
            );
        }
    }

    // The coarsest level, always: a handful of squares covering the whole
    // picture, so there is never a gap to fall through.
    let coarsest = shapes.len() - 1;
    let shape = shapes[coarsest];
    for y in 0..shape.down() {
        for x in 0..shape.across() {
            squares.request(
                Square {
                    level: coarsest,
                    x,
                    y,
                },
                false,
            );
        }
    }

    squares.trim(
        level,
        (
            visible.center().x / shapes[level].tile_width.max(1) as f32,
            visible.center().y / shapes[level].tile_height.max(1) as f32,
        ),
    );

    Some(Painted {
        level,
        levels: shapes.len(),
        drawn,
        waiting,
        shape: shapes[level],
        full,
        scale,
        partial,
        resident: squares.resident(),
    })
}

/// Draws one square onto four corners.
///
/// `Painter::image` can only fill a rectangle, which is enough until a map is
/// turned. A two-triangle mesh takes any four corners, and for an unturned
/// picture it comes out identical.
fn patch(
    painter: &eframe::egui::Painter,
    texture: eframe::egui::TextureId,
    quad: [eframe::egui::Pos2; 4],
    part: Rect,
    tint: eframe::egui::Color32,
) {
    use eframe::egui::epaint::{Mesh, Vertex};

    let mut mesh = Mesh::with_texture(texture);
    let uv = [
        part.left_top(),
        part.right_top(),
        part.right_bottom(),
        part.left_bottom(),
    ];
    for (corner, uv) in quad.iter().zip(uv) {
        mesh.vertices.push(Vertex {
            pos: *corner,
            uv,
            color: tint,
        });
    }
    mesh.indices.extend([0, 1, 2, 0, 2, 3]);
    painter.add(mesh);
}

struct Job {
    path: PathBuf,
    square: Square,
}

#[derive(Default)]
struct Queue {
    urgent: Vec<Job>,
    prefetch: Vec<Job>,
}

impl Queue {
    fn push(&mut self, job: Job, urgent: bool) {
        let queue = if urgent {
            &mut self.urgent
        } else {
            &mut self.prefetch
        };
        queue.push(job);
        if queue.len() > QUEUE_LIMIT {
            queue.remove(0);
        }
    }

    /// Newest first, and the window always before the speculation.
    fn take(&mut self) -> Option<Job> {
        self.urgent.pop().or_else(|| self.prefetch.pop())
    }
}

struct Done {
    path: PathBuf,
    square: Square,
    image: Option<eframe::egui::ColorImage>,
}

/// Squares decoded and ready to draw, for one file at a time.
pub struct Squares {
    file: Option<PathBuf>,
    shapes: Vec<LevelShape>,
    wanted: Arc<Mutex<Queue>>,
    results: Receiver<Done>,
    textures: HashMap<Square, eframe::egui::TextureHandle>,
    inflight: HashSet<Square>,
    missing: HashSet<Square>,
    /// Squares read since this file was opened, for the readout.
    pub read: usize,
}

impl Squares {
    pub fn new(ctx: &eframe::egui::Context) -> Self {
        let wanted = Arc::new(Mutex::new(Queue::default()));
        let (sender, results) = channel();
        for _ in 0..WORKERS {
            let wanted = Arc::clone(&wanted);
            let sender: Sender<Done> = sender.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || worker(&wanted, &sender, &ctx));
        }
        Self {
            file: None,
            shapes: Vec::new(),
            wanted,
            results,
            textures: HashMap::new(),
            inflight: HashSet::new(),
            missing: HashSet::new(),
            read: 0,
        }
    }

    /// Points at a file, forgetting everything about the last one.
    pub fn open(&mut self, path: &Path) {
        if self.file.as_deref() == Some(path) {
            return;
        }
        self.file = Some(path.to_path_buf());
        self.shapes = shapes(path);
        self.textures.clear();
        self.inflight.clear();
        self.missing.clear();
        self.read = 0;
        if let Ok(mut queue) = self.wanted.lock() {
            queue.urgent.clear();
            queue.prefetch.clear();
        }
    }

    pub fn shapes(&self) -> &[LevelShape] {
        &self.shapes
    }

    pub fn is_layered(&self) -> bool {
        self.shapes.len() > 1
    }

    pub fn request(&mut self, square: Square, urgent: bool) {
        if self.textures.contains_key(&square)
            || self.inflight.contains(&square)
            || self.missing.contains(&square)
        {
            return;
        }
        let Some(path) = &self.file else { return };
        self.inflight.insert(square);
        if let Ok(mut queue) = self.wanted.lock() {
            queue.push(
                Job {
                    path: path.clone(),
                    square,
                },
                urgent,
            );
        }
    }

    /// Uploads whatever arrived since the last frame.
    pub fn poll(&mut self, ctx: &eframe::egui::Context) {
        while let Ok(done) = self.results.try_recv() {
            // A square from the file we were looking at a moment ago.
            if self.file.as_deref() != Some(done.path.as_path()) {
                continue;
            }
            self.inflight.remove(&done.square);
            match done.image {
                Some(image) => {
                    let name = format!(
                        "square{}_{}_{}",
                        done.square.level, done.square.x, done.square.y
                    );
                    let texture =
                        ctx.load_texture(name, image, eframe::egui::TextureOptions::LINEAR);
                    self.textures.insert(done.square, texture);
                    self.read += 1;
                }
                // Recorded so a square that cannot be read is not asked for on
                // every frame for ever.
                None => {
                    self.missing.insert(done.square);
                }
            }
        }
    }

    pub fn get(&self, square: &Square) -> Option<&eframe::egui::TextureHandle> {
        self.textures.get(square)
    }

    /// A coarser square covering the same ground, and which part of it to draw.
    pub fn stand_in(&self, square: Square) -> Option<(&eframe::egui::TextureHandle, Rect)> {
        for steps in 1..=MAX_COARSER_STEPS {
            let level = square.level + steps;
            if level >= self.shapes.len() {
                break;
            }
            let coarser = Square {
                level,
                x: square.x >> steps,
                y: square.y >> steps,
            };
            if let Some(texture) = self.textures.get(&coarser) {
                let (Some(fine), Some(coarse)) =
                    (self.shapes.get(square.level), self.shapes.get(level))
                else {
                    break;
                };
                return Some((texture, coarser_part(square, steps, *fine, *coarse)));
            }
        }
        None
    }

    pub fn resident(&self) -> usize {
        self.textures.len()
    }

    /// Video memory the squares in hand are holding.
    ///
    /// Squares are not in any budget — they are trimmed by distance from the
    /// view, not by bytes — so this is the only place their cost is visible.
    /// Every one of them is RGBA, which is what the card is holding whatever
    /// the file stored.
    pub fn bytes(&self) -> usize {
        self.textures
            .values()
            .map(|texture| {
                let [width, height] = texture.size();
                width * height * 4
            })
            .sum()
    }

    /// Lets go of every square, for a card that has run out of room.
    ///
    /// The squares themselves are still on disk and the requests to read them
    /// back are cheap, so this costs a moment of softness — the coarser
    /// stand-ins go too, so there is a frame or two with nothing under the
    /// view — and buys back everything this picture was holding.
    pub fn shed(&mut self) {
        self.textures.clear();
    }

    /// Drops squares far from what is being looked at.
    ///
    /// The coarsest level is spared whatever happens: it is a handful of
    /// squares and it is what covers every gap while sharper ones arrive.
    pub fn trim(&mut self, level: usize, centre: (f32, f32)) {
        if self.textures.len() <= RESIDENT_LIMIT {
            return;
        }
        let coarsest = self.shapes.len().saturating_sub(1);
        let mut ranked: Vec<(i64, Square)> = self
            .textures
            .keys()
            .map(|square| {
                if square.level == coarsest {
                    return (i64::MIN, *square);
                }
                let steps = square.level as i32 - level as i32;
                let scale = 2_f32.powi(-steps);
                let dx = square.x as f32 - centre.0 * scale;
                let dy = square.y as f32 - centre.1 * scale;
                (((dx * dx + dy * dy) * 100.0) as i64, *square)
            })
            .collect();
        ranked.sort_unstable_by_key(|(distance, _)| *distance);
        for (_, square) in ranked.drain(RESIDENT_LIMIT..) {
            self.textures.remove(&square);
        }
    }
}

fn worker(wanted: &Arc<Mutex<Queue>>, sender: &Sender<Done>, ctx: &eframe::egui::Context) {
    // One file open per worker, reopened only when the file changes — opening
    // reads the directories, which is wasted work per square otherwise.
    let mut open: Option<(PathBuf, crate::cog::TiledTiff)> = None;

    loop {
        let next = wanted.lock().ok().and_then(|mut queue| queue.take());
        let Some(job) = next else {
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        };

        if open.as_ref().map(|(path, _)| path) != Some(&job.path) {
            open = crate::cog::TiledTiff::open(&job.path)
                .ok()
                .map(|tiff| (job.path.clone(), tiff));
        }
        let image = open.as_mut().and_then(|(_, tiff)| {
            let shape = tiff.levels.get(job.square.level)?;
            let region = crate::cog::Region {
                x: job.square.x * shape.tile_width,
                y: job.square.y * shape.tile_height,
                width: shape.tile_width,
                height: shape.tile_height,
            };
            let rgba = tiff.decode_region(job.square.level, region).ok()?;
            let size = [rgba.width() as usize, rgba.height() as usize];
            Some(eframe::egui::ColorImage::from_rgba_unmultiplied(
                size,
                rgba.as_raw(),
            ))
        });

        if sender
            .send(Done {
                path: job.path,
                square: job.square,
                image,
            })
            .is_err()
        {
            return; // The app has gone.
        }
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(width: u32, height: u32) -> LevelShape {
        LevelShape {
            width,
            height,
            tile_width: 256,
            tile_height: 256,
        }
    }

    #[test]
    fn the_level_follows_the_zoom() {
        // Six levels, as a converted map has.
        assert_eq!(level_for_scale(6, 1.0), 0, "full size reads the finest");
        assert_eq!(level_for_scale(6, 0.5), 1);
        assert_eq!(level_for_scale(6, 0.25), 2);
        assert_eq!(level_for_scale(6, 0.125), 3);
    }

    #[test]
    fn the_level_is_rounded_not_floored() {
        // Drawn slightly over half size. Flooring would read the full-size
        // level and four times the pixels it can show.
        assert_eq!(level_for_scale(6, 0.51), 1);
        // And slightly under: still the half-size level, not the quarter.
        assert_eq!(level_for_scale(6, 0.49), 1);
    }

    #[test]
    fn zooming_past_the_finest_level_stays_on_it() {
        assert_eq!(level_for_scale(6, 4.0), 0, "there is nothing sharper");
    }

    #[test]
    fn zooming_out_past_the_coarsest_stays_on_it() {
        assert_eq!(level_for_scale(6, 0.0005), 5);
        assert_eq!(
            level_for_scale(1, 0.0005),
            0,
            "one level is the only answer"
        );
    }

    #[test]
    fn only_the_squares_in_the_window_are_asked_for() {
        // A 2560-wide level is ten squares across; a window over the first two.
        let visible = Rect::from_min_max(pos2(0.0, 0.0), pos2(400.0, 300.0));
        let squares = visible_squares(shape(2560, 2560), visible);
        assert_eq!(squares.len(), 4, "two across, two down: {squares:?}");
        assert!(squares.contains(&(0, 0)) && squares.contains(&(1, 1)));
        assert!(!squares.contains(&(2, 0)), "past the window");
    }

    #[test]
    fn a_window_hanging_off_the_edge_asks_for_nothing_that_is_not_there() {
        // 300 wide is two squares across, the second mostly empty.
        let visible = Rect::from_min_max(pos2(-500.0, -500.0), pos2(5000.0, 5000.0));
        let squares = visible_squares(shape(300, 300), visible);
        assert_eq!(squares.len(), 4, "2x2 and no more: {squares:?}");
    }

    #[test]
    fn an_empty_window_asks_for_nothing() {
        let empty = Rect::from_min_max(pos2(10.0, 10.0), pos2(10.0, 10.0));
        assert!(visible_squares(shape(2560, 2560), empty).is_empty());
    }

    #[test]
    fn a_square_reads_the_matching_part_of_a_coarser_one() {
        // Sizes chosen so nothing is cut short: 1024 is four whole squares.
        let fine = shape(1024, 1024);
        let coarse = shape(512, 512);
        let square = Square {
            level: 0,
            x: 3,
            y: 3,
        };
        let part = coarser_part(square, 1, fine, coarse);
        assert_eq!(part.min, pos2(0.5, 0.5), "odd column, odd row");
        assert_eq!(part.size(), vec2(0.5, 0.5));

        let part = coarser_part(
            Square {
                level: 0,
                x: 2,
                y: 3,
            },
            1,
            fine,
            coarse,
        );
        assert_eq!(part.min, pos2(0.0, 0.5), "even column is the left half");
    }

    #[test]
    fn a_square_at_the_edge_holds_less_than_a_whole_square() {
        // 600 wide is three squares across; the last holds only 88 pixels.
        let shape = shape(600, 600);
        assert_eq!(square_content(shape, 0, 0).size(), vec2(256.0, 256.0));

        let edge = square_content(shape, 2, 0);
        assert_eq!(edge.min, pos2(512.0, 0.0));
        assert_eq!(
            edge.size(),
            vec2(88.0, 256.0),
            "the picture runs out before the square does",
        );
        assert_eq!(
            square_content(shape, 2, 2).size(),
            vec2(88.0, 88.0),
            "short both ways",
        );
    }

    #[test]
    fn a_square_past_the_edge_holds_nothing() {
        assert_eq!(square_content(shape(600, 600), 9, 9).size(), vec2(0.0, 0.0));
    }

    #[test]
    fn a_stand_in_for_an_edge_square_is_not_stretched() {
        let fine = shape(600, 600);
        let coarse = shape(300, 300);

        // A square in the middle: whole, and half of a whole parent.
        let middle = coarser_part(
            Square {
                level: 0,
                x: 1,
                y: 0,
            },
            1,
            fine,
            coarse,
        );
        assert_eq!(middle.min.x, 0.5, "the right half of its parent");
        assert!((middle.width() - 0.5).abs() < 1e-6);

        // The last square: it holds 88 of 256 pixels, and its parent holds 44
        // of 256 — the same ground. So the stand-in is the *whole* of the
        // parent's picture, not half of it. Taking half would draw the ground
        // at twice the size, which is the stretching along the edge.
        let edge = coarser_part(
            Square {
                level: 0,
                x: 2,
                y: 0,
            },
            1,
            fine,
            coarse,
        );
        assert_eq!(edge.min.x, 0.0, "it starts at its parent's left");
        assert!(
            (edge.width() - 1.0).abs() < 1e-6,
            "the whole of the parent covers it — got {}",
            edge.width(),
        );
    }

    #[test]
    fn a_map_shrunk_onto_the_basemap_reads_the_coarsest_level() {
        // A gigapixel map drawn a few hundred pixels wide on the OpenStreetMap
        // view. It must land on the smallest level, not read the biggest and
        // shrink it — which is what the overlay used to do.
        let levels = 8;
        let full_width = 40448.0;
        let drawn = 300.0;
        let level = level_for_scale(levels, drawn / full_width);
        assert_eq!(level, levels - 1, "the smallest copy is the right one");
    }

    #[test]
    fn zooming_the_basemap_in_walks_the_map_up_the_levels() {
        // The same map as the basemap is zoomed in on: the level must climb
        // towards the sharp one rather than staying put.
        let levels = 8;
        let full_width = 40448.0;
        let seen: Vec<usize> = [300.0, 1200.0, 5000.0, 20000.0, 40448.0]
            .into_iter()
            .map(|drawn: f32| level_for_scale(levels, drawn / full_width))
            .collect();
        assert_eq!(seen, vec![7, 5, 3, 1, 0]);
        for pair in seen.windows(2) {
            assert!(pair[1] < pair[0], "each step in must be sharper: {seen:?}");
        }
    }

    #[test]
    fn a_window_costs_the_same_however_big_the_picture_is() {
        // The point of the whole exercise: a screenful of a gigapixel map and a
        // screenful of a small one ask for the same number of squares.
        let window = Rect::from_min_max(pos2(0.0, 0.0), pos2(1600.0, 1000.0));
        let small = visible_squares(shape(4000, 4000), window).len();
        let huge = visible_squares(shape(200_000, 200_000), window).len();
        assert_eq!(small, huge);
    }
}
