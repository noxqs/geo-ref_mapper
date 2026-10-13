//! Background thumbnail generation and loading.
//!
//! Thumbnails live in SQLite as encoded blobs, not as loose files. Worker
//! threads do two jobs: sweep the library generating whatever is missing, and
//! serve on-demand loads for tiles that just scrolled into view. Loads always
//! win, so browsing stays responsive while a sweep grinds through a big folder.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Result;
use eframe::egui;
use image::{DynamicImage, ImageEncoder};

use crate::db::Library;
use crate::gpu::Gpu;

/// Longest edge of a stored thumbnail. Large enough for the biggest mosaic
/// tile, small enough that a 100k-image library stays in the low gigabytes.
pub const THUMB_EDGE: u32 = 320;

/// JPEG quality for opaque thumbnails. 82 is visually clean at this size.
const JPEG_QUALITY: u8 = 82;

/// Rows pulled from the database per sweep step, so a cancelled sweep stops
/// promptly and load requests get a look-in between batches.
const SWEEP_BATCH: u32 = 8;

/// Cap on sweep workers. Thumbnailing is CPU bound — decode dominates, disk
/// read is a few percent — and it scales nearly linearly to core count.
/// Measured on a 16-core/32-thread machine: 8 threads 173 img/s, 16 threads
/// 268 img/s, 32 threads 331 img/s. Past 16 the gain per thread collapses, so
/// that is where this caps out rather than taking the whole machine.
pub const MAX_THUMB_WORKERS: usize = 16;

/// Half the hardware threads, so a sweep never starves the UI or the
/// interactive decoder.
pub fn default_worker_count() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores / 2).clamp(1, MAX_THUMB_WORKERS)
}

pub enum ThumbResult {
    /// A thumbnail is decoded and ready to upload.
    Ready {
        id: i64,
        image: egui::ColorImage,
    },
    /// This image has no usable thumbnail, so stop asking for it.
    Unavailable {
        id: i64,
    },
    /// Progress of the background sweep.
    Swept {
        generated: u64,
        remaining: u64,
    },
    SweepFinished,
}

#[derive(Default)]
struct Queue {
    /// On-demand loads, newest first — the tiles under the cursor matter most.
    loads: VecDeque<i64>,
    sweeping: bool,
    /// Images a worker is generating right now. Without this every worker
    /// would pull the same "next missing" rows and decode them all twice.
    claimed: HashSet<i64>,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    shutdown: AtomicBool,
}

pub struct Thumbnailer {
    shared: Arc<Shared>,
    results: Receiver<ThumbResult>,
    /// Loads already queued, so a tile on screen is not requested every frame.
    in_flight: HashSet<i64>,
    /// Images known to have no thumbnail we can build.
    unavailable: HashSet<i64>,
    workers: Vec<std::thread::JoinHandle<()>>,
    pub generated: u64,
    pub remaining: u64,
    pub sweeping: bool,
}

impl Thumbnailer {
    /// `use_gpu` opts into nvJPEG decoding. It is off by default: measured on
    /// a 16-core/32-thread machine the per-image GPU path reaches 62 img/s
    /// against 218 img/s for the CPU pool, because each image round-trips
    /// through a stream synchronise instead of being batched and overlapped.
    /// It wins only where CPU threads are scarce.
    pub fn new(db_path: PathBuf, threads: usize, use_gpu: bool, ctx: egui::Context) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let (sender, results) = channel();

        let workers = (0..threads.clamp(1, MAX_THUMB_WORKERS))
            .map(|index| {
                let shared = Arc::clone(&shared);
                let sender = sender.clone();
                let ctx = ctx.clone();
                let db_path = db_path.clone();
                std::thread::Builder::new()
                    .name(format!("geo-ref_mapper-thumb-{index}"))
                    .spawn(move || worker(&db_path, shared, sender, ctx, use_gpu))
                    .expect("spawning a thumbnail worker")
            })
            .collect();

        Self {
            shared,
            results,
            in_flight: HashSet::new(),
            unavailable: HashSet::new(),
            workers,
            generated: 0,
            remaining: 0,
            sweeping: false,
        }
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    pub fn queued(&self) -> usize {
        self.in_flight.len()
    }

    /// Starts (or restarts) the sweep that fills in missing thumbnails.
    pub fn start_sweep(&mut self) {
        let mut queue = self.shared.queue.lock().expect("thumbnail queue poisoned");
        queue.sweeping = true;
        self.sweeping = true;
        drop(queue);
        self.shared.wake.notify_all();
    }

    pub fn stop_sweep(&mut self) {
        let mut queue = self.shared.queue.lock().expect("thumbnail queue poisoned");
        queue.sweeping = false;
        self.sweeping = false;
    }

    /// Asks for one image's thumbnail. Cheap to call every frame for a visible
    /// tile: repeats and known-missing images are filtered out here.
    pub fn request(&mut self, id: i64) {
        if self.in_flight.contains(&id) || self.unavailable.contains(&id) {
            return;
        }
        self.in_flight.insert(id);

        let mut queue = self.shared.queue.lock().expect("thumbnail queue poisoned");
        queue.loads.push_front(id);
        drop(queue);
        self.shared.wake.notify_one();
    }

    pub fn poll(&mut self) -> Vec<ThumbResult> {
        let mut out = Vec::new();
        loop {
            match self.results.try_recv() {
                Ok(result) => {
                    match &result {
                        ThumbResult::Ready { id, .. } => {
                            self.in_flight.remove(id);
                        }
                        ThumbResult::Unavailable { id } => {
                            self.in_flight.remove(id);
                            self.unavailable.insert(*id);
                        }
                        ThumbResult::Swept {
                            generated,
                            remaining,
                        } => {
                            self.generated = *generated;
                            self.remaining = *remaining;
                        }
                        ThumbResult::SweepFinished => {
                            self.remaining = 0;
                            self.sweeping = false;
                        }
                    }
                    out.push(result);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        out
    }
}

impl Drop for Thumbnailer {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
        let began = std::time::Instant::now();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        crate::metrics::report_slow_join("the thumbnail sweep", began.elapsed());
    }
}

enum Task {
    Load(i64),
    Sweep,
}

fn worker(
    db_path: &Path,
    shared: Arc<Shared>,
    sender: Sender<ThumbResult>,
    ctx: egui::Context,
    use_gpu: bool,
) {
    let library = match Library::open(db_path) {
        Ok(library) => library,
        Err(error) => {
            eprintln!("geo-ref_mapper: thumbnail worker could not open the library: {error:#}");
            return;
        }
    };
    let mut generated = 0_u64;
    // One context per worker: nvJPEG state and its device buffers are not
    // shareable between threads.
    let mut gpu = if use_gpu { Gpu::new() } else { None };

    loop {
        let task = {
            let mut queue = shared.queue.lock().expect("thumbnail queue poisoned");
            loop {
                if shared.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                // Loads before sweeping: the user is waiting on those.
                if let Some(id) = queue.loads.pop_front() {
                    break Task::Load(id);
                }
                if queue.sweeping {
                    break Task::Sweep;
                }
                queue = shared.wake.wait(queue).expect("thumbnail queue poisoned");
            }
        };

        match task {
            Task::Load(id) => {
                let result = match load_or_build(&library, id) {
                    Ok(Some(image)) => ThumbResult::Ready { id, image },
                    _ => ThumbResult::Unavailable { id },
                };
                if sender.send(result).is_err() {
                    return; // The app is gone.
                }
                ctx.request_repaint();
            }
            Task::Sweep => {
                match sweep_step(&library, &sender, &shared, &mut generated, gpu.as_mut()) {
                    SweepOutcome::Progressed | SweepOutcome::Finished => ctx.request_repaint(),
                    SweepOutcome::Disconnected => return,
                    SweepOutcome::Idle => {
                        // Every remaining row belongs to another worker. Sleep on
                        // the condvar instead of spinning; whoever finishes a batch
                        // releases its claims and wakes us. The timeout is a
                        // backstop in case that notification is missed.
                        let queue = shared.queue.lock().expect("thumbnail queue poisoned");
                        let _ = shared
                            .wake
                            .wait_timeout(queue, std::time::Duration::from_millis(50))
                            .expect("thumbnail queue poisoned");
                    }
                }
            }
        }
    }
}

/// What one pass of the sweep achieved.
enum SweepOutcome {
    /// A batch was generated.
    Progressed,
    /// Nothing left to claim, but other workers are still busy.
    Idle,
    /// The whole library has thumbnails.
    Finished,
    /// The app has shut down.
    Disconnected,
}

/// Reads a stored thumbnail, building and storing it first if it is missing.
fn load_or_build(library: &Library, id: i64) -> Result<Option<egui::ColorImage>> {
    if let Some(blob) = library.thumbnail(id, THUMB_EDGE)? {
        return Ok(Some(decode_blob(&blob)?));
    }

    let Some(record) = library.image(id)? else {
        return Ok(None);
    };
    let Some(built) = build(Path::new(&record.path))? else {
        return Ok(None);
    };

    library.put_thumbnail(
        id,
        THUMB_EDGE,
        built.width,
        built.height,
        built.format,
        &built.data,
    )?;
    Ok(Some(decode_blob(&built.data)?))
}

/// Generates one batch of missing thumbnails.
fn sweep_step(
    library: &Library,
    sender: &Sender<ThumbResult>,
    shared: &Arc<Shared>,
    generated: &mut u64,
    mut gpu: Option<&mut Gpu>,
) -> SweepOutcome {
    // Over-fetch so that after discarding rows another worker already claimed
    // there is still a full batch left to do.
    let candidates = match library
        .images_without_thumbnail(THUMB_EDGE, SWEEP_BATCH * MAX_THUMB_WORKERS as u32)
    {
        Ok(candidates) => candidates,
        Err(error) => {
            eprintln!("geo-ref_mapper: thumbnail sweep query failed: {error:#}");
            let mut queue = shared.queue.lock().expect("thumbnail queue poisoned");
            queue.sweeping = false;
            drop(queue);
            return match sender.send(ThumbResult::SweepFinished) {
                Ok(()) => SweepOutcome::Finished,
                Err(_) => SweepOutcome::Disconnected,
            };
        }
    };

    let pending: Vec<_> = {
        let mut queue = shared.queue.lock().expect("thumbnail queue poisoned");
        let mine: Vec<_> = candidates
            .into_iter()
            .filter(|record| !queue.claimed.contains(&record.id))
            .take(SWEEP_BATCH as usize)
            .collect();
        for record in &mine {
            queue.claimed.insert(record.id);
        }

        if mine.is_empty() {
            if queue.claimed.is_empty() {
                // Genuinely nothing left: the sweep is done.
                queue.sweeping = false;
                drop(queue);
                return match sender.send(ThumbResult::SweepFinished) {
                    Ok(()) => SweepOutcome::Finished,
                    Err(_) => SweepOutcome::Disconnected,
                };
            }
            // Everything left is being handled by another worker. Returning
            // straight away would spin this thread against SQLite, so wait to
            // be woken when those claims are released.
            return SweepOutcome::Idle;
        }
        mine
    };

    let mut aborted = false;
    for record in &pending {
        if shared.shutdown.load(Ordering::Relaxed) {
            aborted = true;
            break;
        }

        match build_with(gpu.as_deref_mut(), Path::new(&record.path)) {
            Ok(Some(built)) => {
                match library.put_thumbnail(
                    record.id,
                    THUMB_EDGE,
                    built.width,
                    built.height,
                    built.format,
                    &built.data,
                ) {
                    Ok(()) => *generated += 1,
                    Err(error) => eprintln!("geo-ref_mapper: storing a thumbnail failed: {error:#}"),
                }
            }
            // An unreadable file would be retried forever; store a marker so
            // the sweep moves past it.
            Ok(None) | Err(_) => {
                let _ = library.put_thumbnail(
                    record.id,
                    THUMB_EDGE,
                    0,
                    0,
                    &crate::db::failure_marker(),
                    &[],
                );
            }
        }
    }

    {
        let mut queue = shared.queue.lock().expect("thumbnail queue poisoned");
        for record in &pending {
            queue.claimed.remove(&record.id);
        }
    }
    // Wake any worker parked because everything was claimed.
    shared.wake.notify_all();

    if aborted {
        return SweepOutcome::Disconnected;
    }

    let remaining = library
        .count_without_thumbnail(THUMB_EDGE)
        .unwrap_or_default();
    match sender.send(ThumbResult::Swept {
        generated: *generated,
        remaining,
    }) {
        Ok(()) => SweepOutcome::Progressed,
        Err(_) => SweepOutcome::Disconnected,
    }
}

pub struct Built {
    pub data: Vec<u8>,
    pub format: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Decodes a source image and encodes a thumbnail of it, on the CPU.
pub fn build(path: &Path) -> Result<Option<Built>> {
    build_with(None, path)
}

/// True for a JPEG, which is the only thing nvJPEG can decode.
fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// Builds a thumbnail, using the GPU when one is supplied and the file is a
/// JPEG. Any GPU failure falls back to the CPU rather than losing the image.
pub fn build_with(mut gpu: Option<&mut Gpu>, path: &Path) -> Result<Option<Built>> {
    let bytes = std::fs::read(path)?;

    if let Some(gpu) = gpu.as_deref_mut()
        && is_jpeg(&bytes)
    {
        match gpu.thumbnail_rgb(&bytes, THUMB_EDGE) {
            Ok((pixels, width, height)) => {
                if let Some(buffer) = image::RgbImage::from_raw(width, height, pixels) {
                    return Ok(Some(encode_scaled(&DynamicImage::ImageRgb8(buffer))?));
                }
            }
            Err(error) => {
                // A single awkward file must not disable the whole pipeline.
                eprintln!(
                    "geo-ref_mapper: GPU decode failed for {}: {error:#}",
                    path.display()
                );
            }
        }
    }

    // Formats outside the `image` crate come first, since it would refuse them.
    if let Some(kind) = crate::codecs::sniff(&bytes)
        && let Ok(source) = crate::codecs::decode(&bytes, kind, THUMB_EDGE)
    {
        return Ok(Some(encode(&source)?));
    }

    // Generous limits, or a large map is refused before it is ever decoded.
    let source = match crate::loader::reader_for(&bytes)?.decode() {
        Ok(source) => source,
        // A WebP-tiled BigTIFF is not something the `image` crate can open, but
        // it already carries a small overview level that decodes in milliseconds
        // — far cheaper than the full image would be even if it could.
        Err(_) => match thumbnail_from_tiled_tiff(path) {
            Some(source) => source,
            None => return Ok(None),
        },
    };
    Ok(Some(encode(&source)?))
}

/// Decodes the smallest pyramid level of a tiled TIFF, which is what makes a
/// thumbnail of a gigapixel map essentially free — a nine-level COG stores a
/// 191 x 128 overview that is a single tile.
fn thumbnail_from_tiled_tiff(path: &std::path::Path) -> Option<DynamicImage> {
    let mut tiff = crate::cog::TiledTiff::open(path).ok()?;
    // Prefer the smallest level that still has enough detail for the tile.
    let level = tiff.level_for(THUMB_EDGE);
    let rgba = match tiff.decode_level(level) {
        Ok(rgba) => rgba,
        // That level may use a compression we cannot read; the overview is
        // usually plainer, so it is worth one more try.
        Err(_) => tiff.decode_level(tiff.coarsest_level()?).ok()?,
    };
    Some(DynamicImage::ImageRgba8(rgba))
}

/// Downscales to [`THUMB_EDGE`] and encodes: JPEG when the image is opaque,
/// PNG when it has transparency worth keeping.
pub fn encode(source: &DynamicImage) -> Result<Built> {
    let (width, height) = (source.width().max(1), source.height().max(1));
    let scaled = if width.max(height) > THUMB_EDGE {
        let [target_width, target_height] = crate::loader::fit_within(width, height, THUMB_EDGE);
        // `thumbnail_exact` is a box filter. A general resample's cost grows
        // with the downscale ratio — at 20× that dominates the whole job, and
        // the quality difference is invisible at this size.
        source.thumbnail_exact(target_width, target_height)
    } else {
        source.clone()
    };

    encode_scaled(&scaled)
}

/// Encodes an image that is already at thumbnail size.
pub fn encode_scaled(scaled: &DynamicImage) -> Result<Built> {
    let rgba = scaled.to_rgba8();
    let opaque = rgba.pixels().all(|pixel| pixel[3] == 255);
    let (width, height) = (scaled.width(), scaled.height());
    let mut data = Vec::new();

    let format = if opaque {
        let rgb = scaled.to_rgb8();
        image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut std::io::Cursor::new(&mut data),
            JPEG_QUALITY,
        )
        .encode_image(&rgb)?;
        "jpeg"
    } else {
        image::codecs::png::PngEncoder::new(&mut std::io::Cursor::new(&mut data)).write_image(
            rgba.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )?;
        "png"
    };

    Ok(Built {
        data,
        format,
        width,
        height,
    })
}

fn decode_blob(blob: &[u8]) -> Result<egui::ColorImage> {
    let decoded = image::ImageReader::new(std::io::Cursor::new(blob))
        .with_guessed_format()?
        .decode()?;
    let rgba = decoded.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        size,
        rgba.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    /// Evicts these files from the page cache so a read measurement reflects
    /// the storage device rather than RAM. Needs no privileges; a no-op if the
    /// kernel declines.
    fn drop_from_page_cache(paths: &[PathBuf]) {
        use std::os::unix::io::AsRawFd;

        for path in paths {
            let Ok(file) = std::fs::File::open(path) else {
                continue;
            };
            // SAFETY: a valid fd owned by `file`, and a length of 0 means
            // "to end of file". POSIX_FADV_DONTNEED only drops clean pages.
            unsafe {
                libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
            }
        }
    }

    fn opaque_image(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            width,
            height,
            Rgba([200, 40, 80, 255]),
        ))
    }

    fn transparent_image(width: u32, height: u32) -> DynamicImage {
        let mut image = RgbaImage::from_pixel(width, height, Rgba([200, 40, 80, 255]));
        image.put_pixel(0, 0, Rgba([0, 0, 0, 0]));
        DynamicImage::ImageRgba8(image)
    }

    #[test]
    fn oversized_images_are_capped_and_keep_their_aspect() {
        let built = encode(&opaque_image(2000, 1000)).unwrap();
        assert_eq!(built.width, THUMB_EDGE);
        assert_eq!(built.height, THUMB_EDGE / 2);
    }

    #[test]
    fn small_images_are_not_upscaled() {
        let built = encode(&opaque_image(64, 48)).unwrap();
        assert_eq!((built.width, built.height), (64, 48));
    }

    #[test]
    fn opaque_images_become_jpeg_and_transparent_ones_png() {
        assert_eq!(encode(&opaque_image(100, 100)).unwrap().format, "jpeg");
        assert_eq!(encode(&transparent_image(100, 100)).unwrap().format, "png");
    }

    #[test]
    fn a_thumbnail_is_far_smaller_than_the_raw_pixels() {
        let built = encode(&opaque_image(2000, 2000)).unwrap();
        let raw = (built.width * built.height * 4) as usize;
        assert!(
            built.data.len() < raw / 10,
            "expected heavy compression, got {} bytes for {raw} raw",
            built.data.len()
        );
    }

    #[test]
    fn encoded_thumbnails_decode_back_to_the_same_size() {
        for source in [opaque_image(500, 250), transparent_image(500, 250)] {
            let built = encode(&source).unwrap();
            let image = decode_blob(&built.data).unwrap();
            assert_eq!(image.size, [built.width as usize, built.height as usize]);
        }
    }

    #[test]
    fn transparency_survives_the_round_trip() {
        let built = encode(&transparent_image(50, 50)).unwrap();
        let image = decode_blob(&built.data).unwrap();
        assert_eq!(image.pixels[0].a(), 0, "the clear pixel should stay clear");
    }

    /// The sweep must finish and cover every image. Several workers competing
    /// for the same "next missing" rows previously either duplicated work or
    /// spun without making progress, so this runs a real pool end to end.
    #[test]
    fn a_multi_worker_sweep_finishes_and_covers_everything() {
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        const IMAGES: usize = 40;
        let db_path = dir.join("library.db");
        {
            let library = Library::open(&db_path).unwrap();
            for index in 0..IMAGES {
                let path = dir.join(format!("image-{index:03}.png"));
                RgbaImage::from_pixel(64, 48, Rgba([index as u8, 40, 80, 255]))
                    .save_with_format(&path, image::ImageFormat::Png)
                    .unwrap();
                library
                    .upsert_image(&crate::db::ImageRecord {
                        path: path.to_string_lossy().into_owned(),
                        folder: dir.to_string_lossy().into_owned(),
                        filename: format!("image-{index:03}.png"),
                        bytes: 1,
                        mtime: 1,
                        ..Default::default()
                    })
                    .unwrap();
            }
            assert_eq!(
                library.count_without_thumbnail(THUMB_EDGE).unwrap(),
                IMAGES as u64
            );
        }

        let mut thumbnailer = Thumbnailer::new(db_path.clone(), 4, false, egui::Context::default());
        thumbnailer.start_sweep();

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut finished = false;
        while Instant::now() < deadline && !finished {
            for result in thumbnailer.poll() {
                if matches!(result, ThumbResult::SweepFinished) {
                    finished = true;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(finished, "the sweep never reported that it finished");
        assert!(
            !thumbnailer.sweeping,
            "the sweep flag should be cleared once it finishes"
        );

        drop(thumbnailer);
        let library = Library::open(&db_path).unwrap();
        assert_eq!(
            library.count_without_thumbnail(THUMB_EDGE).unwrap(),
            0,
            "every image should have a thumbnail"
        );
        assert_eq!(library.stats().unwrap().thumbnails, IMAGES as u64);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Opt-in benchmark against a real folder, to answer "is this I/O bound or
    /// CPU bound, and does adding threads help?".
    ///
    /// `GEO_REF_MAPPER_BENCH_DIR=/path/to/photos cargo test --release bench_thumbnail -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_BENCH_DIR and real photos"]
    fn bench_thumbnail_pipeline() {
        use std::time::Instant;

        let Ok(dir) = std::env::var("GEO_REF_MAPPER_BENCH_DIR") else {
            eprintln!("set GEO_REF_MAPPER_BENCH_DIR to run this");
            return;
        };
        let sample: usize = std::env::var("GEO_REF_MAPPER_BENCH_N")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(48);

        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("reading the benchmark directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(crate::db::is_image_extension)
            })
            .collect();
        files.sort();
        files.truncate(sample);
        assert!(!files.is_empty(), "no images found in {dir}");

        // Phase 1 — a genuinely cold read. The page cache will happily serve a
        // folder that was read moments ago and report absurd throughput, so
        // evict these files first. `POSIX_FADV_DONTNEED` does that without root.
        drop_from_page_cache(&files);
        let mut total_bytes = 0_u64;
        let started = Instant::now();
        for path in &files {
            total_bytes += std::fs::read(path).expect("reading a sample file").len() as u64;
        }
        let cold_read = started.elapsed();

        let started = Instant::now();
        let mut blobs = Vec::with_capacity(files.len());
        for path in &files {
            blobs.push(std::fs::read(path).expect("reading a sample file"));
        }
        let warm_read = started.elapsed();

        // Phase 2 — decode from memory, so no I/O is involved at all.
        let started = Instant::now();
        for blob in &blobs {
            let _ = image::ImageReader::new(std::io::Cursor::new(blob))
                .with_guessed_format()
                .expect("guessing the format")
                .decode();
        }
        let decode_only = started.elapsed();

        // Phase 3 — downscale and encode, from already-decoded pixels.
        let decoded: Vec<DynamicImage> = blobs
            .iter()
            .filter_map(|blob| {
                image::ImageReader::new(std::io::Cursor::new(blob))
                    .with_guessed_format()
                    .ok()?
                    .decode()
                    .ok()
            })
            .collect();
        let started = Instant::now();
        for source in &decoded {
            let _ = encode(source).expect("encoding a thumbnail");
        }
        let resize_encode = started.elapsed();

        let count = files.len() as f64;
        let megabytes = total_bytes as f64 / 1_048_576.0;
        println!(
            "\n=== thumbnail pipeline: {} files, {megabytes:.0} MB ===",
            files.len()
        );
        println!(
            "  read (cold)      {:>8.1} ms total  {:>6.1} ms/img  {:>6.0} MB/s",
            cold_read.as_secs_f64() * 1000.0,
            cold_read.as_secs_f64() * 1000.0 / count,
            megabytes / cold_read.as_secs_f64(),
        );
        println!(
            "  read (warm)      {:>8.1} ms total  {:>6.1} ms/img  {:>6.0} MB/s",
            warm_read.as_secs_f64() * 1000.0,
            warm_read.as_secs_f64() * 1000.0 / count,
            megabytes / warm_read.as_secs_f64(),
        );
        println!(
            "  decode           {:>8.1} ms total  {:>6.1} ms/img",
            decode_only.as_secs_f64() * 1000.0,
            decode_only.as_secs_f64() * 1000.0 / count,
        );
        println!(
            "  resize + encode  {:>8.1} ms total  {:>6.1} ms/img",
            resize_encode.as_secs_f64() * 1000.0,
            resize_encode.as_secs_f64() * 1000.0 / count,
        );

        // Phase 3b — the viewer's own path: full-resolution decode, repack and
        // what the eventual texture costs.
        {
            let blob = &blobs[0];
            let t = Instant::now();
            let decoded = image::ImageReader::new(std::io::Cursor::new(blob))
                .with_guessed_format()
                .unwrap()
                .decode()
                .unwrap();
            let decode_ms = t.elapsed().as_secs_f64() * 1000.0;
            let (w, h) = (decoded.width(), decoded.height());

            let t = Instant::now();
            let rgba = decoded.clone().into_rgba8();
            let to_rgba_ms = t.elapsed().as_secs_f64() * 1000.0;

            let t = Instant::now();
            let color = eframe::egui::ColorImage::from_rgba_unmultiplied(
                [w as usize, h as usize],
                rgba.as_raw(),
            );
            let convert_ms = t.elapsed().as_secs_f64() * 1000.0;
            drop(color);

            // The direct path the loader now takes for RGB sources.
            let t = Instant::now();
            let color = match decoded {
                image::DynamicImage::ImageRgb8(buffer) => {
                    eframe::egui::ColorImage::from_rgb([w as usize, h as usize], buffer.as_raw())
                }
                other => eframe::egui::ColorImage::from_rgba_unmultiplied(
                    [w as usize, h as usize],
                    other.into_rgba8().as_raw(),
                ),
            };
            let direct_ms = t.elapsed().as_secs_f64() * 1000.0;
            println!("\n  [direct RGB -> Color32: {direct_ms:.1} ms]");

            println!(
                "\n=== one image through the viewer path ({w} x {h}, {:.1} MP) ===",
                (w as f64 * h as f64) / 1e6
            );
            println!("  decode            {decode_ms:>7.1} ms");
            println!("  into_rgba8        {to_rgba_ms:>7.1} ms");
            println!(
                "  -> Color32 buffer {convert_ms:>7.1} ms   ({:.0} MB allocated)",
                (color.pixels.len() * 4) as f64 / 1e6
            );
            println!(
                "  total before GPU upload: {:.1} ms",
                decode_ms + to_rgba_ms + convert_ms
            );
        }

        // Phase 4 — the GPU path, if this machine has one.
        match crate::gpu::Gpu::new() {
            None => println!("\n=== GPU: unavailable, CPU only ==="),
            Some(_) => {
                println!("\n=== GPU (nvJPEG decode + NPP resize) ===");
                for threads in [1_usize, 2, 4] {
                    let started = Instant::now();
                    std::thread::scope(|scope| {
                        for worker in 0..threads {
                            let files = &files;
                            scope.spawn(move || {
                                let mut gpu = crate::gpu::Gpu::new();
                                for path in files.iter().skip(worker).step_by(threads) {
                                    let _ = build_with(gpu.as_mut(), path);
                                }
                            });
                        }
                    });
                    let elapsed = started.elapsed().as_secs_f64();
                    println!(
                        "  {threads:>2} GPU context(s)  {:>7.2} s  {:>6.1} img/s",
                        elapsed,
                        count / elapsed,
                    );
                }
            }
        }

        // Phase 5 — the whole job at several thread counts, CPU only.
        println!("\n=== full build() scaling ===");
        let mut baseline = 0.0_f64;
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8);
        let ladder: Vec<usize> = [1, 2, 4, 8, 12, 16, 24, 32]
            .into_iter()
            .filter(|threads| *threads <= cores * 2)
            .collect();
        for threads in ladder {
            let started = Instant::now();
            std::thread::scope(|scope| {
                for worker in 0..threads {
                    let files = &files;
                    scope.spawn(move || {
                        // Stripe the work so every thread gets a fair mix.
                        for path in files.iter().skip(worker).step_by(threads) {
                            let _ = build(path);
                        }
                    });
                }
            });
            let elapsed = started.elapsed().as_secs_f64();
            if threads == 1 {
                baseline = elapsed;
            }
            println!(
                "  {threads:>2} thread(s)  {:>8.2} s  {:>6.1} img/s  speed-up {:.2}×",
                elapsed,
                count / elapsed,
                baseline / elapsed,
            );
        }
        println!();
    }

    #[test]
    fn build_reads_a_file_and_rejects_a_non_image() {
        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-thumbs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let image_path = dir.join("source.png");
        RgbaImage::from_pixel(800, 400, Rgba([10, 20, 30, 255]))
            .save_with_format(&image_path, image::ImageFormat::Png)
            .unwrap();
        let built = build(&image_path).unwrap().expect("a real PNG builds");
        assert_eq!(built.width, THUMB_EDGE);

        let junk_path = dir.join("junk.png");
        std::fs::write(&junk_path, b"this is not a png").unwrap();
        assert!(
            build(&junk_path).is_err() || build(&junk_path).unwrap().is_none(),
            "a corrupt file must not produce a thumbnail"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod one_thumb {
    /// `GEO_REF_MAPPER_THUMB_FILE=/path/to.tif cargo test --release thumb_one -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_THUMB_FILE"]
    fn thumb_one() {
        let Ok(path) = std::env::var("GEO_REF_MAPPER_THUMB_FILE") else {
            return;
        };
        let path = std::path::Path::new(&path);
        println!(
            "build(): {:?}",
            super::build(path).map(|b| b.map(|b| (b.width, b.height, b.format, b.data.len())))
        );

        match crate::cog::TiledTiff::open(path) {
            Ok(mut tiff) => {
                println!("TiledTiff opened, coarsest = {:?}", tiff.coarsest_level());
                let level = tiff.level_for(super::THUMB_EDGE);
                println!("level_for({}) = {level}", super::THUMB_EDGE);
                match tiff.decode_level(level) {
                    Ok(rgba) => println!(
                        "  decode_level({level}) ok: {}x{}",
                        rgba.width(),
                        rgba.height()
                    ),
                    Err(error) => println!("  decode_level({level}) FAILED: {error:#}"),
                }
                if let Some(coarse) = tiff.coarsest_level() {
                    match tiff.decode_level(coarse) {
                        Ok(rgba) => println!(
                            "  decode_level({coarse}) ok: {}x{}",
                            rgba.width(),
                            rgba.height()
                        ),
                        Err(error) => println!("  decode_level({coarse}) FAILED: {error:#}"),
                    }
                }
            }
            Err(error) => println!("TiledTiff::open FAILED: {error:#}"),
        }
    }
}
