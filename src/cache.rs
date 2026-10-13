//! GPU texture cache with a byte budget.
//!
//! Decoded images are uploaded once and kept until the budget forces them out,
//! least-recently-used first. Dropping the [`TextureHandle`] is what actually
//! frees the GPU memory, so eviction is just removing it from the map.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui::{self, TextureHandle};

use crate::loader::DecodeTimings;

pub struct Entry {
    pub texture: TextureHandle,
    pub size: [usize; 2],
    pub bytes: usize,
    pub timings: DecodeTimings,
}

pub struct TextureCache {
    entries: HashMap<PathBuf, Entry>,
    /// Most-recently-used last.
    order: Vec<PathBuf>,
    budget: usize,
    used: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

/// Halves an image until it fits, returning it untouched when it already does.
///
/// Halving rather than scaling to an exact fit: it is a handful of averaging
/// passes over shrinking data, and the result is what a mipmap would be. A
/// picture that fits — which is almost all of them — is passed straight
/// through and costs nothing.
pub fn fit_to_texture_limit(image: egui::ColorImage, limit: usize) -> egui::ColorImage {
    let [mut width, mut height] = image.size;
    if width <= limit && height <= limit {
        return image;
    }
    let mut pixels = image.pixels;
    while width > limit || height > limit {
        let (half_width, half_height) = ((width / 2).max(1), (height / 2).max(1));
        let mut out = Vec::with_capacity(half_width * half_height);
        for y in 0..half_height {
            for x in 0..half_width {
                // The mean of the 2x2 block, which is what halving means.
                let mut total = [0_u32; 4];
                let mut counted = 0_u32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let (sx, sy) = (x * 2 + dx, y * 2 + dy);
                        if sx >= width || sy >= height {
                            continue;
                        }
                        let pixel = pixels[sy * width + sx];
                        let [r, g, b, a] = pixel.to_array();
                        total[0] += u32::from(r);
                        total[1] += u32::from(g);
                        total[2] += u32::from(b);
                        total[3] += u32::from(a);
                        counted += 1;
                    }
                }
                let counted = counted.max(1);
                out.push(egui::Color32::from_rgba_premultiplied(
                    (total[0] / counted) as u8,
                    (total[1] / counted) as u8,
                    (total[2] / counted) as u8,
                    (total[3] / counted) as u8,
                ));
            }
        }
        pixels = out;
        width = half_width;
        height = half_height;
    }
    egui::ColorImage {
        size: [width, height],
        pixels,
        source_size: egui::Vec2::new(width as f32, height as f32),
    }
}

#[cfg(test)]
mod texture_limits {
    use super::*;

    fn solid(width: usize, height: usize) -> egui::ColorImage {
        egui::ColorImage {
            size: [width, height],
            pixels: vec![egui::Color32::from_rgb(200, 40, 40); width * height],
            source_size: egui::Vec2::new(width as f32, height as f32),
        }
    }

    #[test]
    fn a_picture_that_fits_is_not_touched() {
        let image = solid(4096, 2048);
        let out = fit_to_texture_limit(image, 8192);
        assert_eq!(out.size, [4096, 2048]);
    }

    #[test]
    fn an_oversized_map_is_shrunk_until_both_edges_fit() {
        // The one that aborted the process: 16,077 across against a limit of
        // 8,192. Halving once is enough, and the aspect is kept.
        let out = fit_to_texture_limit(solid(16077, 9000), 8192);
        assert!(
            out.size[0] <= 8192 && out.size[1] <= 8192,
            "got {:?}",
            out.size
        );
        assert_eq!(out.size, [8038, 4500]);
        assert_eq!(out.pixels.len(), 8038 * 4500);
    }

    #[test]
    fn a_very_long_thin_map_is_halved_until_even_the_long_edge_fits() {
        // A strip chart: one edge enormous, the other tiny. Halving has to keep
        // going, and the short edge must never reach zero.
        let out = fit_to_texture_limit(solid(70000, 3), 8192);
        assert!(out.size[0] <= 8192);
        assert!(
            out.size[1] >= 1,
            "an edge must not be halved out of existence"
        );
        assert_eq!(out.pixels.len(), out.size[0] * out.size[1]);
    }

    #[test]
    fn the_colour_survives_the_shrink() {
        let out = fit_to_texture_limit(solid(16384, 16384), 8192);
        let [r, g, b, _] = out.pixels[0].to_array();
        assert_eq!(
            (r, g, b),
            (200, 40, 40),
            "averaging one colour gives itself"
        );
    }
}

impl TextureCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: Vec::new(),
            budget: budget_bytes,
            used: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Applies a new budget, evicting immediately if it shrank.
    pub fn set_budget(&mut self, budget_bytes: usize) {
        self.budget = budget_bytes.max(1);
        self.evict_to_budget();
    }

    pub fn used(&self) -> usize {
        self.used
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Does not affect the eviction order — use for "is this already loaded?".
    pub fn contains(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    /// Reads an entry without disturbing the eviction order — for panels that
    /// display an entry rather than draw it.
    pub fn peek(&self, path: &Path) -> Option<&Entry> {
        self.entries.get(path)
    }

    /// Looks an entry up and marks it as most recently used.
    pub fn get(&mut self, path: &Path) -> Option<&Entry> {
        if self.entries.contains_key(path) {
            self.touch(path);
            self.hits += 1;
            self.entries.get(path)
        } else {
            self.misses += 1;
            None
        }
    }

    /// Uploads pixels as a texture and stores it, evicting if needed.
    pub fn insert(
        &mut self,
        ctx: &egui::Context,
        path: &Path,
        image: egui::ColorImage,
        mut timings: DecodeTimings,
    ) {
        // A graphics card will not hold a texture wider or taller than its own
        // limit — 8192 on plenty of hardware, and these maps are routinely more
        // than that. Asking anyway is not an error that can be caught: wgpu
        // reports it through a handler whose default is to abort, so the window
        // dies with a validation message instead of showing the picture.
        //
        // Shrunk to fit rather than refused. A map too large for one texture is
        // exactly the map somebody wants to look at, and a soft version of it
        // beats a crash. The tiled path is the real answer for these, and this
        // is the floor under everything that does not take it.
        let image = fit_to_texture_limit(image, crate::loader::MAX_TEXTURE_EDGE as usize);
        let size = image.size;
        let bytes = size[0] * size[1] * 4;

        let started = std::time::Instant::now();
        let texture = ctx.load_texture(
            path.to_string_lossy().into_owned(),
            image,
            egui::TextureOptions::LINEAR,
        );
        timings.upload_ms = started.elapsed().as_secs_f32() * 1000.0;

        if let Some(previous) = self.entries.remove(path) {
            self.used = self.used.saturating_sub(previous.bytes);
            self.order.retain(|queued| queued != path);
        }

        self.entries.insert(
            path.to_path_buf(),
            Entry {
                texture,
                size,
                bytes,
                timings,
            },
        );
        self.order.push(path.to_path_buf());
        self.used += bytes;

        self.evict_to_budget();
    }

    /// Forgets one image, for when the file behind it has gone.
    pub fn remove(&mut self, path: &Path) {
        if let Some(entry) = self.entries.remove(path) {
            self.used = self.used.saturating_sub(entry.bytes);
            self.order.retain(|queued| queued != path);
        }
    }

    /// Drops least-recently-used entries until the budget is met. The most
    /// recent entry always survives, so a single oversized image still shows.
    fn evict_to_budget(&mut self) {
        while self.used > self.budget && self.order.len() > 1 {
            let oldest = self.order.remove(0);
            if let Some(entry) = self.entries.remove(&oldest) {
                self.used = self.used.saturating_sub(entry.bytes);
                self.evictions += 1;
            }
        }
    }

    fn touch(&mut self, path: &Path) {
        if let Some(index) = self.order.iter().position(|queued| queued == path) {
            let path = self.order.remove(index);
            self.order.push(path);
        }
    }
}

/// Thumbnail textures, bounded by count rather than bytes — they are all
/// roughly the same small size, so a simple cap is enough.
pub struct ThumbCache {
    entries: HashMap<i64, TextureHandle>,
    /// Most-recently-used last.
    order: Vec<i64>,
    /// Which frame each entry was last drawn in, so eviction can tell "old" from
    /// "on screen right now".
    used_at: HashMap<i64, u64>,
    generation: u64,
    budget: usize,
    used: usize,
}

impl ThumbCache {
    /// Bounded by bytes rather than by count, so it can share a VRAM budget
    /// with the full-resolution cache and be compared against it directly.
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: Vec::new(),
            used_at: HashMap::new(),
            generation: 0,
            budget: budget_bytes.max(1),
            used: 0,
        }
    }

    /// Starts a new frame. Everything drawn from here on is "current" and is
    /// protected from eviction until the next one.
    pub fn begin_frame(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// Whether another thumbnail would fit. Callers ask before requesting one:
    /// past this point a request cannot be satisfied without evicting something
    /// already on screen, which is exactly the trade that causes flicker.
    pub fn has_room(&self) -> bool {
        self.used < self.budget
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    pub fn used(&self) -> usize {
        self.used
    }

    /// Applies a new budget, evicting immediately if it shrank.
    pub fn set_budget(&mut self, budget_bytes: usize) {
        self.budget = budget_bytes.max(1);
        self.evict_to_budget();
    }

    /// Drops least-recently-used entries, but never one drawn recently.
    ///
    /// Without that guard, a Mosaic showing more tiles than the budget holds
    /// evicts a texture drawn moments earlier to make room for the next one,
    /// then needs it back immediately. On screen every tile flickers between
    /// its picture and its placeholder colour, forever.
    ///
    /// "Recently" has to mean the previous frame as well as this one. Arriving
    /// thumbnails are inserted at the top of a frame, before a single tile has
    /// been drawn — so at that moment nothing carries the current generation
    /// and a this-frame-only guard protects nothing at all. That was the first
    /// attempt, and it did not stop the flicker.
    fn evict_to_budget(&mut self) {
        while self.used > self.budget && self.order.len() > 1 {
            let oldest = self.order[0];
            let drawn_recently = self
                .used_at
                .get(&oldest)
                .is_some_and(|at| self.generation.saturating_sub(*at) <= 1);
            if drawn_recently {
                break;
            }
            self.order.remove(0);
            if let Some(entry) = self.entries.remove(&oldest) {
                self.used = self.used.saturating_sub(thumb_bytes(&entry));
            }
            self.used_at.remove(&oldest);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Looks up a thumbnail and marks it as recently used.
    pub fn get(&mut self, id: i64) -> Option<&TextureHandle> {
        if !self.entries.contains_key(&id) {
            return None;
        }
        if let Some(index) = self.order.iter().position(|queued| *queued == id) {
            let id = self.order.remove(index);
            self.order.push(id);
        }
        self.used_at.insert(id, self.generation);
        self.entries.get(&id)
    }

    pub fn insert(&mut self, ctx: &egui::Context, id: i64, image: egui::ColorImage) {
        if let Some(previous) = self.entries.remove(&id) {
            self.used = self.used.saturating_sub(thumb_bytes(&previous));
            self.order.retain(|queued| *queued != id);
            self.used_at.remove(&id);
        }

        let bytes = image.size[0] * image.size[1] * 4;
        let texture = ctx.load_texture(format!("thumb:{id}"), image, egui::TextureOptions::LINEAR);
        self.entries.insert(id, texture);
        self.order.push(id);
        // Deliberately *not* stamped with the current generation: this texture
        // has not been drawn yet, and protecting it would let a frame that
        // inserts many thumbnails run past the budget with nothing evictable.
        // It earns its protection on the frame it is actually shown.
        self.used += bytes;

        self.evict_to_budget();
    }
}

/// Edge length of the small textures the Mosaic uses when zoomed out.
///
/// A 320 px thumbnail is 409 KB of VRAM. Drawn into a 10 pt tile that is a
/// thousand times more pixels than reach the screen, and a 384 MB budget only
/// stretches to about 900 of them — so a folder of 3,839 maps showed 900
/// pictures and 2,900 coloured squares. At 48 px an entry costs 9 KB and the
/// same budget holds forty thousand, which is the whole library.
pub const MINI_EDGE: usize = 48;

/// Shrinks a decoded thumbnail to [`MINI_EDGE`] by box-averaging.
///
/// Nearest-neighbour would alias badly on the fine linework in a scanned map —
/// at this size the average is what makes a tile read as a picture rather than
/// as noise.
pub fn shrink_to_mini(image: &egui::ColorImage) -> egui::ColorImage {
    let (width, height) = (image.size[0].max(1), image.size[1].max(1));
    let scale = (width.max(height) as f32 / MINI_EDGE as f32).max(1.0);
    let out_width = ((width as f32 / scale).round() as usize).max(1);
    let out_height = ((height as f32 / scale).round() as usize).max(1);

    let source = image.as_raw();
    let mut pixels = Vec::with_capacity(out_width * out_height * 4);
    for y in 0..out_height {
        let y0 = y * height / out_height;
        let y1 = (((y + 1) * height) / out_height).max(y0 + 1).min(height);
        for x in 0..out_width {
            let x0 = x * width / out_width;
            let x1 = (((x + 1) * width) / out_width).max(x0 + 1).min(width);

            let mut totals = [0_u32; 4];
            let mut count = 0_u32;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let at = (sy * width + sx) * 4;
                    if let Some(pixel) = source.get(at..at + 4) {
                        for channel in 0..4 {
                            totals[channel] += pixel[channel] as u32;
                        }
                        count += 1;
                    }
                }
            }
            let count = count.max(1);
            for total in totals {
                pixels.push((total / count) as u8);
            }
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([out_width, out_height], &pixels)
}

fn thumb_bytes(texture: &TextureHandle) -> usize {
    let size = texture.size();
    size[0] * size[1] * 4
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        // A frame has to run before textures can be allocated.
        let _ = ctx.run_ui(Default::default(), |_| {});
        ctx
    }

    fn image(edge: usize) -> egui::ColorImage {
        egui::ColorImage::new([edge, edge], vec![egui::Color32::RED; edge * edge])
    }

    /// Each 8×8 image is 8 * 8 * 4 = 256 bytes.
    const ENTRY_BYTES: usize = 8 * 8 * 4;

    #[test]
    fn insert_then_get_is_a_hit() {
        let ctx = context();
        let mut cache = TextureCache::new(1024 * 1024);
        cache.insert(
            &ctx,
            Path::new("/a.png"),
            image(8),
            DecodeTimings::default(),
        );

        assert!(cache.contains(Path::new("/a.png")));
        assert!(cache.get(Path::new("/a.png")).is_some());
        assert_eq!(cache.hits, 1);
        assert_eq!(cache.used(), ENTRY_BYTES);
    }

    #[test]
    fn missing_entry_counts_as_a_miss() {
        let mut cache = TextureCache::new(1024);
        assert!(cache.get(Path::new("/nope.png")).is_none());
        assert_eq!(cache.misses, 1);
    }

    #[test]
    fn budget_evicts_least_recently_used_first() {
        let ctx = context();
        // Room for two entries only.
        let mut cache = TextureCache::new(ENTRY_BYTES * 2);
        for name in ["/a.png", "/b.png"] {
            cache.insert(&ctx, Path::new(name), image(8), DecodeTimings::default());
        }

        // Touching `a` makes `b` the oldest.
        assert!(cache.get(Path::new("/a.png")).is_some());
        cache.insert(
            &ctx,
            Path::new("/c.png"),
            image(8),
            DecodeTimings::default(),
        );

        assert!(
            cache.contains(Path::new("/a.png")),
            "recently used survives"
        );
        assert!(cache.contains(Path::new("/c.png")), "newest survives");
        assert!(!cache.contains(Path::new("/b.png")), "oldest is evicted");
        assert_eq!(cache.evictions, 1);
        assert_eq!(cache.used(), ENTRY_BYTES * 2);
    }

    #[test]
    fn an_oversized_image_is_still_kept() {
        let ctx = context();
        let mut cache = TextureCache::new(16); // Smaller than any real image.
        cache.insert(
            &ctx,
            Path::new("/big.png"),
            image(8),
            DecodeTimings::default(),
        );

        assert!(
            cache.contains(Path::new("/big.png")),
            "the newest entry must never be evicted, or nothing would display"
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn reinserting_the_same_path_does_not_double_count() {
        let ctx = context();
        let mut cache = TextureCache::new(1024 * 1024);
        cache.insert(
            &ctx,
            Path::new("/a.png"),
            image(8),
            DecodeTimings::default(),
        );
        cache.insert(
            &ctx,
            Path::new("/a.png"),
            image(8),
            DecodeTimings::default(),
        );

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.used(), ENTRY_BYTES);
    }

    #[test]
    fn thumb_cache_evicts_the_least_recently_used() {
        let ctx = context();
        // Room for two 4x4 thumbnails (64 bytes each).
        let mut cache = ThumbCache::new(4 * 4 * 4 * 2);
        cache.insert(&ctx, 1, image(4));
        cache.insert(&ctx, 2, image(4));

        assert!(cache.get(1).is_some(), "touch 1 so 2 becomes the oldest");
        cache.insert(&ctx, 3, image(4));

        assert_eq!(cache.len(), 2);
        assert!(cache.get(1).is_some());
        assert!(cache.get(3).is_some());
        assert!(cache.get(2).is_none());
    }

    /// The Mosaic flicker, reduced to its bones.
    ///
    /// Zoomed right out, more tiles are on screen than the budget holds. Each
    /// frame drew a tile, evicted a neighbour drawn moments earlier to make
    /// room, and needed it back immediately — so every tile alternated between
    /// its picture and its placeholder colour.
    #[test]
    fn a_thumbnail_drawn_this_frame_is_never_evicted() {
        let ctx = context();
        let mut cache = ThumbCache::new(4 * 4 * 4 * 2); // room for two
        cache.begin_frame();
        cache.insert(&ctx, 1, image(4));
        cache.insert(&ctx, 2, image(4));

        // Both drawn in this frame...
        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_some());

        // ...so a third arriving in the same frame must not displace either.
        cache.insert(&ctx, 3, image(4));
        assert!(cache.get(1).is_some(), "1 was on screen and got evicted");
        assert!(cache.get(2).is_some(), "2 was on screen and got evicted");

        // Two frames on, nothing is protected any more and the cache trims
        // itself back down to the budget.
        cache.begin_frame();
        cache.begin_frame();
        cache.insert(&ctx, 4, image(4));
        assert!(cache.used() <= cache.budget(), "the budget must still hold");
    }

    #[test]
    fn a_full_thumbnail_cache_stops_asking_for_more() {
        let ctx = context();
        let mut cache = ThumbCache::new(4 * 4 * 4);
        assert!(cache.has_room(), "empty");
        cache.insert(&ctx, 1, image(4));
        assert!(
            !cache.has_room(),
            "full — the Mosaic must stop requesting rather than thrash",
        );
    }

    #[test]
    fn shrinking_to_mini_keeps_the_shape_and_averages_the_colour() {
        // A 320x160 image: half black, half white, split down the middle.
        let (width, height) = (320_usize, 160_usize);
        let mut pixels = Vec::with_capacity(width * height * 4);
        for _ in 0..height {
            for x in 0..width {
                let value = if x < width / 2 { 0 } else { 255 };
                pixels.extend_from_slice(&[value, value, value, 255]);
            }
        }
        let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &pixels);
        let mini = shrink_to_mini(&image);

        assert_eq!(mini.size, [MINI_EDGE, MINI_EDGE / 2], "aspect must survive");
        let raw = mini.as_raw();
        assert_eq!(raw[0], 0, "the left edge is still black");
        let last = (MINI_EDGE - 1) * 4;
        assert_eq!(raw[last], 255, "the right edge is still white");
    }

    #[test]
    fn a_mini_thumbnail_is_a_fraction_of_the_memory() {
        let image = image(320);
        let mini = shrink_to_mini(&image);
        let full_bytes = image.size[0] * image.size[1] * 4;
        let mini_bytes = mini.size[0] * mini.size[1] * 4;
        assert!(
            mini_bytes * 40 < full_bytes,
            "a 48px entry must be tiny enough that the whole library fits: \
             {mini_bytes} vs {full_bytes}",
        );
    }

    #[test]
    fn thumb_cache_replaces_without_growing() {
        let ctx = context();
        let mut cache = ThumbCache::new(1024 * 1024);
        cache.insert(&ctx, 7, image(4));
        cache.insert(&ctx, 7, image(4));
        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.used(),
            4 * 4 * 4,
            "re-inserting must not double count"
        );
    }

    #[test]
    fn shrinking_a_budget_evicts_straight_away() {
        let ctx = context();
        let mut cache = ThumbCache::new(1024 * 1024);
        for id in 0..6 {
            cache.insert(&ctx, id, image(4));
        }
        assert_eq!(cache.len(), 6);

        cache.set_budget(4 * 4 * 4 * 2);
        assert_eq!(
            cache.len(),
            2,
            "lowering the limit takes effect immediately"
        );
        assert!(cache.used() <= cache.budget());
    }

    #[test]
    fn shrinking_the_texture_budget_evicts_straight_away() {
        let ctx = context();
        let mut cache = TextureCache::new(1024 * 1024);
        for name in ["/a.png", "/b.png", "/c.png"] {
            cache.insert(&ctx, Path::new(name), image(8), DecodeTimings::default());
        }
        cache.set_budget(ENTRY_BYTES);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn insert_records_upload_time() {
        let ctx = context();
        let mut cache = TextureCache::new(1024 * 1024);
        cache.insert(
            &ctx,
            Path::new("/a.png"),
            image(8),
            DecodeTimings::default(),
        );
        let entry = cache.get(Path::new("/a.png")).unwrap();
        assert!(entry.timings.upload_ms >= 0.0);
        assert_eq!(entry.size, [8, 8]);
    }
}
