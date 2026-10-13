//! Background image decoding.
//!
//! A small pool of worker threads turns files into RGBA8 pixel buffers. The UI
//! thread never decodes: it posts requests, drains finished ones each frame and
//! uploads them as textures.
//!
//! Two things keep the pool responsive while the user arrows through a folder:
//! jobs carry a priority (the image on screen beats a speculative preload), and
//! preloads from an older selection are dropped before they are decoded rather
//! than after.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use anyhow::Context as _;
use eframe::egui;

use crate::cog::{TiledTiff, subsample_for};

/// Longest edge we will upload. Anything larger is downscaled first — GPUs
/// reject oversized textures, and a 100 MP scan is not worth the VRAM anyway.
pub const MAX_TEXTURE_EDGE: u32 = 8192;

/// How wide the first-shown layer should be.
///
/// Comfortably past any window on a 4K screen, so the picture is sharp before
/// any zooming, and far below the texture limit, so a converted map is opened
/// by reading a small layer rather than decoding the largest one.
const BASE_TARGET: u32 = 4096;

/// The width to pick the first layer by. Overridable for measurement.
fn base_target() -> u32 {
    std::env::var("GEO_REF_MAPPER_BASE_EDGE")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|edge| *edge > 0)
        .unwrap_or(BASE_TARGET)
        .min(MAX_TEXTURE_EDGE)
}

/// Upper bound on decode workers. More threads than this mostly buys memory
/// pressure — each in-flight decode holds a full RGBA8 buffer.
pub const MAX_WORKERS: usize = 8;

/// The `image` crate defaults to a 512 MB allocation ceiling, which rejects
/// large scans outright: a 21760 × 15104 map needs 986 MB for its pixels alone
/// and failed to open at all. Lifted far enough for real photographs and maps,
/// but still finite, so a corrupt header claiming absurd dimensions is refused
/// rather than being allowed to exhaust memory.
const MAX_DECODE_ALLOC: u64 = 6 * 1024 * 1024 * 1024;

/// An `ImageReader` over `bytes` with our allocation ceiling rather than the
/// crate default.
pub fn reader_for(bytes: &[u8]) -> anyhow::Result<image::ImageReader<std::io::Cursor<&[u8]>>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    Ok(reader)
}

/// The worker count actually used for a requested thread count.
pub fn worker_count_for(requested: usize) -> usize {
    requested.clamp(1, MAX_WORKERS)
}

/// Which part of the pipeline an image is in, for the canvas to report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Reading,
    Decoding,
    Converting,
    Uploading,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Reading => "Reading file",
            Self::Decoding => "Decoding",
            Self::Converting => "Preparing texture",
            Self::Uploading => "Uploading to GPU",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Priority {
    /// The image the user is looking at.
    Visible = 0,
    /// A neighbour we expect them to reach next.
    Preload = 1,
}

/// Everything the DEBUG tab reports about one decode.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecodeTimings {
    pub read_ms: f32,
    pub decode_ms: f32,
    pub resize_ms: f32,
    /// Repacking RGBA bytes into egui's `Color32` buffer. Never free: it is a
    /// full copy of the decoded image.
    pub convert_ms: f32,
    pub upload_ms: f32,
    pub file_bytes: u64,
    pub decoded_bytes: usize,
    pub worker: usize,
    /// Set when the image was too large to upload at full resolution.
    pub downscaled_from: Option<[u32; 2]>,
    /// Threads the downscale actually used. Worth reporting: a preload running
    /// this on one thread was 13× slower than the same work spread over cores.
    pub downscale_threads: Option<u32>,
    /// For a pyramidal TIFF, `(level shown, levels available, tiles decoded)`.
    /// The level decides how sharp the pixels are, so it belongs on screen
    /// rather than buried in a log.
    pub pyramid: Option<(u32, u32, u32)>,
    /// Threads used, when the JPEG could be split at its restart markers.
    pub parallel_strips: Option<u32>,
}

impl DecodeTimings {
    pub fn total_ms(&self) -> f32 {
        self.read_ms + self.decode_ms + self.resize_ms + self.convert_ms + self.upload_ms
    }
}

pub struct Decoded {
    pub path: PathBuf,
    pub image: egui::ColorImage,
    pub timings: DecodeTimings,
}

pub enum DecodeResult {
    /// How far along the pixels for `path` are, from 0 to 1. When the file is
    /// being decoded in strips, `strips` carries the count and a bitmask of
    /// which have landed, so the UI can show the parallelism.
    Progress {
        path: PathBuf,
        fraction: f32,
        strips: Option<(u32, u32)>,
        stage: Stage,
    },
    Ready(Box<Decoded>),
    Failed {
        path: PathBuf,
        error: String,
    },
    /// Dropped before decoding because the selection moved on.
    Skipped {
        path: PathBuf,
    },
}

struct Job {
    path: PathBuf,
    priority: Priority,
    generation: u64,
}

struct Shared {
    queue: Mutex<Vec<Job>>,
    wake: Condvar,
    shutdown: AtomicBool,
    /// Bumped whenever the selection changes; preloads older than this are
    /// stale and get dropped instead of decoded.
    generation: AtomicU64,
}

impl Shared {
    /// Highest priority first, then most recently requested.
    fn take_next(queue: &mut Vec<Job>) -> Option<Job> {
        let best = queue
            .iter()
            .enumerate()
            .min_by_key(|(_, job)| (job.priority, std::cmp::Reverse(job.generation)))
            .map(|(index, _)| index)?;
        Some(queue.swap_remove(best))
    }
}

pub struct Loader {
    shared: Arc<Shared>,
    results: Receiver<DecodeResult>,
    /// Paths queued or being decoded, so the same file is never requested twice.
    in_flight: HashSet<PathBuf>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl Loader {
    pub fn new(threads: usize, ctx: egui::Context) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        });
        let (sender, results) = channel();

        let threads = worker_count_for(threads);
        let workers = (0..threads)
            .map(|index| {
                let shared = Arc::clone(&shared);
                let sender = sender.clone();
                let ctx = ctx.clone();
                std::thread::Builder::new()
                    .name(format!("geo-ref_mapper-decode-{index}"))
                    .spawn(move || worker(index, shared, sender, ctx))
                    .expect("spawning a decode worker")
            })
            .collect();

        Self {
            shared,
            results,
            in_flight: HashSet::new(),
            workers,
        }
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    pub fn queued(&self) -> usize {
        self.in_flight.len()
    }

    /// Marks every queued preload as stale. Call when the selection moves.
    pub fn bump_generation(&self) -> u64 {
        self.shared.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Relaxed)
    }

    /// Queues a decode unless the path is already on its way.
    pub fn request(&mut self, path: &Path, priority: Priority) {
        if self.in_flight.contains(path) {
            return;
        }
        self.in_flight.insert(path.to_path_buf());

        let job = Job {
            path: path.to_path_buf(),
            priority,
            generation: self.generation(),
        };
        self.shared
            .queue
            .lock()
            .expect("decode queue poisoned")
            .push(job);
        self.shared.wake.notify_one();
    }

    /// Drains finished decodes. Call once per frame.
    pub fn poll(&mut self) -> Vec<DecodeResult> {
        let mut out = Vec::new();
        loop {
            match self.results.try_recv() {
                Ok(result) => {
                    match &result {
                        // Progress does not retire the job.
                        DecodeResult::Progress { .. } => {}
                        DecodeResult::Ready(decoded) => {
                            self.in_flight.remove(&decoded.path);
                        }
                        DecodeResult::Failed { path, .. } | DecodeResult::Skipped { path } => {
                            self.in_flight.remove(path);
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

impl Drop for Loader {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
        let began = std::time::Instant::now();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        crate::metrics::report_slow_join("the decode pool", began.elapsed());
    }
}

fn worker(index: usize, shared: Arc<Shared>, sender: Sender<DecodeResult>, ctx: egui::Context) {
    loop {
        let job = {
            let mut queue = shared.queue.lock().expect("decode queue poisoned");
            loop {
                if shared.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(job) = Shared::take_next(&mut queue) {
                    break job;
                }
                queue = shared.wake.wait(queue).expect("decode queue poisoned");
            }
        };

        // Pyramidal TIFFs get a path of their own: the layer that matches the
        // window is read rather than the whole picture, so opening a map costs
        // a couple of megapixels instead of fifty.
        if !stale_check(&shared, &job) && decode_pyramid(&job.path, index, &sender) {
            ctx.request_repaint();
            continue;
        }

        // A preload queued for an earlier selection is no longer worth the CPU.
        let stale = job.priority == Priority::Preload
            && job.generation < shared.generation.load(Ordering::Relaxed);
        let result = if stale {
            DecodeResult::Skipped { path: job.path }
        } else {
            // Only the image on screen gets the whole machine for *decoding*.
            // Preloads stay single-threaded there so a bulk pass keeps all
            // workers busy on different images rather than fighting over one.
            //
            // Downscaling is a different matter: it is a short, bandwidth-bound
            // burst at the end, and leaving it on one thread cost 644 ms of the
            // 651 ms an 80 MP preload spent past decoding. Preloads get a share
            // sized so that every worker downscaling at once adds up to roughly
            // the core count instead of oversubscribing it.
            let (threads, downscale_threads) = if job.priority == Priority::Visible {
                (parallel_threads(), parallel_threads())
            } else {
                (1, preload_downscale_threads())
            };
            match decode(&job.path, index, threads, downscale_threads, &sender) {
                Ok(decoded) => DecodeResult::Ready(Box::new(decoded)),
                Err(error) => DecodeResult::Failed {
                    path: job.path,
                    error: format!("{error:#}"),
                },
            }
        };

        if sender.send(result).is_err() {
            return; // The app is gone.
        }
        ctx.request_repaint();
    }
}

/// True when this job was superseded before it started.
fn stale_check(shared: &Arc<Shared>, job: &Job) -> bool {
    job.priority == Priority::Preload && job.generation < shared.generation.load(Ordering::Relaxed)
}

/// Handles a tiled, pyramidal TIFF by reading the layer nearest the window's
/// size. Returns false if the file is not one of those, so the caller can fall
/// back to a normal decode.
/// Which layer the app would show first, and how many pixels reading it costs.
///
/// Separate from the decoding so it can be checked without a window: claiming a
/// change made opening cheaper, without measuring it, is how the layers came to
/// be ignored in the first place.
#[cfg_attr(not(test), allow(dead_code))]
pub fn base_layer_choice(path: &Path) -> Option<(usize, u32, u32, u64)> {
    let tiff = TiledTiff::open(path).ok()?;
    let level = tiff.level_for(base_target());
    let sizes = tiff.level_sizes();
    let (width, height) = *sizes.get(level)?;
    Some((level, width, height, u64::from(width) * u64::from(height)))
}

fn decode_pyramid(path: &Path, worker: usize, sender: &Sender<DecodeResult>) -> bool {
    if !is_tiff(path) {
        return false;
    }

    let Ok(mut tiff) = TiledTiff::open(path) else {
        return false; // Not tiled, or not a TIFF we understand.
    };
    // Without this the DEBUG tab reports "0 B on disk" and an expansion ratio
    // of a hundred million, because the whole file is never read into memory
    // on this path — only the tiles of the level being shown.
    let file_bytes = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let Some(coarse) = tiff.coarsest_level() else {
        return false;
    };
    // The layer to show, chosen from the window rather than from the graphics
    // card's limit.
    //
    // This used to ask for the finest layer at least `MAX_TEXTURE_EDGE` across,
    // which on a converted map only ever matched the full-size layer — so every
    // open decoded all of it and threw three quarters away, and the smaller
    // layers were never read at all. Asking for a layer about the size of the
    // window reads a couple of megapixels instead of fifty, and the sharp
    // pixels still arrive: zooming in fetches them square by square.
    let fine = tiff.level_for(base_target());

    // No pyramid to fall back on: subsample every tile as it is composited and
    // publish the partly-filled canvas as it goes, so the image appears
    // progressively instead of after one long stall.
    if fine == coarse && tiff.levels.len() == 1 {
        let level = tiff.levels[0].clone();
        let factor = subsample_for(level.width, level.height, base_target());
        let started = Instant::now();

        let publish = |canvas: &image::RgbaImage, worker: usize| {
            let size = [canvas.width() as usize, canvas.height() as usize];
            DecodeResult::Ready(Box::new(Decoded {
                path: path.to_path_buf(),
                image: egui::ColorImage::from_rgba_unmultiplied(size, canvas.as_raw()),
                timings: DecodeTimings {
                    decode_ms: started.elapsed().as_secs_f32() * 1000.0,
                    decoded_bytes: size[0] * size[1] * 4,
                    worker,
                    file_bytes,
                    downscaled_from: (factor > 1).then_some([level.width, level.height]),
                    ..Default::default()
                },
            }))
        };

        let result = tiff.decode_level_subsampled(0, factor, |canvas, _fraction| {
            let _ = sender.send(publish(canvas, worker));
        });
        return match result {
            // Handled either way: the file *is* a tiled TIFF, so falling back to
            // the ordinary decoder would only fail again. A dead channel just
            // means the app is shutting down.
            Ok(canvas) => {
                let _ = sender.send(publish(&canvas, worker));
                true
            }
            Err(_) => false,
        };
    }

    // Only the layer the window asks for.
    //
    // The coarsest layer used to be decoded and published first, so that
    // *something* was on screen while the real one was read. What that looked
    // like was every map opening as a thumbnail blown up to full size for a
    // split second before snapping into focus. The Viewer now holds the
    // previous picture until this one is ready, which covers the same gap
    // without showing a blurred version of where you are going — so the extra
    // pass buys nothing and costs a decode and an upload.
    let levels: &[usize] = &[fine];

    let mut sent_any = false;
    for level in levels {
        let started = Instant::now();
        let Ok(rgba) = tiff.decode_level(*level) else {
            continue;
        };

        // A file with no overview pyramid can leave us holding a level that is
        // still far past the maximum texture size, so cap it here as well.
        let mut downscaled_from = None;
        let rgba = if rgba.width().max(rgba.height()) > MAX_TEXTURE_EDGE {
            downscaled_from = Some([rgba.width(), rgba.height()]);
            let [width, height] = fit_within(rgba.width(), rgba.height(), MAX_TEXTURE_EDGE);
            image::imageops::thumbnail(&rgba, width, height)
        } else {
            rgba
        };

        let size = [rgba.width() as usize, rgba.height() as usize];
        let timings = DecodeTimings {
            decode_ms: started.elapsed().as_secs_f32() * 1000.0,
            decoded_bytes: size[0] * size[1] * 4,
            worker,
            file_bytes,
            downscaled_from,
            pyramid: Some((
                *level as u32,
                tiff.levels.len() as u32,
                tiff.levels[*level].tile_count() as u32,
            )),
            ..Default::default()
        };
        let image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
        if sender
            .send(DecodeResult::Ready(Box::new(Decoded {
                path: path.to_path_buf(),
                image,
                timings,
            })))
            .is_err()
        {
            return true;
        }
        sent_any = true;
    }

    sent_any
}

/// Threads to throw at a single interactive image.
fn parallel_threads() -> usize {
    cores().clamp(1, 16)
}

fn cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Threads a preload may use for its downscale. Every decode worker could be in
/// that phase at once, so this is the core count divided between them.
fn preload_downscale_threads() -> usize {
    (cores() / MAX_WORKERS).clamp(2, 8)
}

/// Fractions reported as each stage completes. Decoding dominates, so it gets
/// the widest span; the bar reaching the end means the texture is ready.
const AFTER_READ: f32 = 0.15;
const AFTER_DECODE: f32 = 0.80;
const AFTER_CONVERT: f32 = 0.95;

fn decode(
    path: &Path,
    worker: usize,
    threads: usize,
    downscale_threads: usize,
    sender: &Sender<DecodeResult>,
) -> anyhow::Result<Decoded> {
    let report = |stage: Stage, fraction: f32| {
        let _ = sender.send(DecodeResult::Progress {
            path: path.to_path_buf(),
            fraction,
            strips: None,
            stage,
        });
    };
    report(Stage::Reading, 0.0);
    let mut timings = DecodeTimings {
        worker,
        ..Default::default()
    };

    let started = Instant::now();
    let bytes = std::fs::read(path)?;
    timings.file_bytes = bytes.len() as u64;
    timings.read_ms = started.elapsed().as_secs_f32() * 1000.0;
    report(Stage::Decoding, AFTER_READ);

    let started = Instant::now();
    // A JPEG with restart markers can be cut into strips and decoded across
    // threads; anything else goes through the ordinary decoder.
    // Strip completions arrive from several threads, so the sender is shared
    // behind a lock; the traffic is a handful of messages per image.
    let guarded = std::sync::Mutex::new(sender);
    let landed = std::sync::atomic::AtomicU32::new(0);
    let decoded = crate::jpeg::parse(&bytes)
        .filter(|_| threads > 1)
        .and_then(|info| {
            crate::jpeg_parallel::decode_parallel_with(&bytes, &info, threads, &|index, count| {
                let mask =
                    landed.fetch_or(1 << index.min(31), Ordering::Relaxed) | (1 << index.min(31));
                let done = mask.count_ones();
                if let Ok(sender) = guarded.lock() {
                    let _ = sender.send(DecodeResult::Progress {
                        path: path.to_path_buf(),
                        fraction: AFTER_READ
                            + (AFTER_DECODE - AFTER_READ) * done as f32 / count.max(1) as f32,
                        strips: Some((count.min(32) as u32, mask)),
                        stage: Stage::Decoding,
                    });
                }
            })
        })
        .map(image::DynamicImage::ImageRgb8);
    timings.parallel_strips = decoded.is_some().then_some(threads as u32);

    let decoded = match decoded {
        Some(image) => image,
        // Formats the `image` crate has no decoder for get their own pass.
        // SVG is told the texture limit, since a vector has no native size.
        None => match crate::codecs::sniff(&bytes) {
            Some(kind) => crate::codecs::decode(&bytes, kind, MAX_TEXTURE_EDGE)?,
            None => reader_for(&bytes)?.decode()?,
        },
    };
    timings.decode_ms = started.elapsed().as_secs_f32() * 1000.0;
    report(Stage::Converting, AFTER_DECODE);

    let (width, height) = (decoded.width(), decoded.height());
    let started = Instant::now();
    let scaled = if width.max(height) > MAX_TEXTURE_EDGE {
        timings.downscaled_from = Some([width, height]);
        timings.downscale_threads = Some(downscale_threads.max(1) as u32);
        let [target_width, target_height] = fit_within(width, height, MAX_TEXTURE_EDGE);
        // Area averaging across cores, which measured 33× faster than
        // `Triangle` on a 328 MP map (2110 ms -> 63 ms) and aliases less at
        // these ratios. `Triangle` remains the fallback for exotic layouts.
        downscale_dynamic(
            &decoded,
            target_width,
            target_height,
            downscale_threads.max(1),
        )
        .unwrap_or_else(|| {
            decoded.resize_exact(
                target_width,
                target_height,
                image::imageops::FilterType::Triangle,
            )
        })
    } else {
        decoded
    };
    // Righting the image *after* the downscale rather than before means the
    // rotation copies a few megapixels instead of a few hundred. Orientation
    // preserves the longest edge, so the texture limit still holds.
    //
    // `pending_orientation`, not `orientation`: a HEIC states its rotation in
    // both the container and its EXIF, and the HEIC decoder has already applied
    // the container's — turning it again would put every portrait on its side.
    let scaled = crate::exif::pending_orientation(&bytes).apply(scaled);
    timings.resize_ms = started.elapsed().as_secs_f32() * 1000.0;

    let size = [scaled.width() as usize, scaled.height() as usize];
    timings.decoded_bytes = size[0] * size[1] * 4;

    // JPEGs decode to RGB. Going through `into_rgba8` first would copy the
    // whole image just to insert an alpha channel that `from_rgb` fills in
    // anyway — measured at 12.6 ms of 66.7 ms for an 11 MP frame.
    let started = Instant::now();
    let image = match scaled {
        image::DynamicImage::ImageRgb8(buffer) => egui::ColorImage::from_rgb(size, buffer.as_raw()),
        other => egui::ColorImage::from_rgba_unmultiplied(size, other.into_rgba8().as_raw()),
    };
    timings.convert_ms = started.elapsed().as_secs_f32() * 1000.0;
    report(Stage::Uploading, AFTER_CONVERT);

    Ok(Decoded {
        path: path.to_path_buf(),
        image,
        timings,
    })
}

/// Area-averaging downscale of interleaved RGB8, spread across threads.
///
/// Every output pixel averages a disjoint rectangle of source pixels, so output
/// rows are completely independent — unlike a general resample, which needs
/// neighbouring rows and cannot be cut into bands without seams. That makes this
/// both parallel and exact, and area averaging is the right filter for a large
/// downscale ratio anyway: `Triangle` samples too few source pixels at 2.7× and
/// aliases while costing far more.
pub fn downscale_box(
    raw: &[u8],
    width: u32,
    height: u32,
    channels: usize,
    target_width: u32,
    target_height: u32,
    threads: usize,
) -> Vec<u8> {
    let target_width = target_width.max(1);
    let target_height = target_height.max(1);
    let mut out = vec![0_u8; target_width as usize * target_height as usize * channels];

    let row_bytes = target_width as usize * channels;
    let threads = threads.clamp(1, 64);
    // Chunk by output rows so each thread owns a disjoint slice of `out`.
    let rows_per_chunk = (target_height as usize).div_ceil(threads).max(1);

    std::thread::scope(|scope| {
        for (chunk_index, chunk) in out.chunks_mut(rows_per_chunk * row_bytes).enumerate() {
            let first_row = chunk_index * rows_per_chunk;
            scope.spawn(move || {
                for local_row in 0..chunk.len() / row_bytes {
                    let y = first_row + local_row;
                    // Source band for this output row, at least one row tall.
                    let y0 = (y as u64 * height as u64 / target_height as u64) as u32;
                    let y1 = (((y as u64 + 1) * height as u64).div_ceil(target_height as u64)
                        as u32)
                        .max(y0 + 1)
                        .min(height);
                    for x in 0..target_width as usize {
                        let x0 = (x as u64 * width as u64 / target_width as u64) as u32;
                        let x1 = (((x as u64 + 1) * width as u64).div_ceil(target_width as u64)
                            as u32)
                            .max(x0 + 1)
                            .min(width);

                        let mut sums = [0_u64; 4];
                        let mut n = 0_u64;
                        for sy in y0..y1 {
                            let row = sy as usize * width as usize * channels;
                            for sx in x0..x1 {
                                let i = row + sx as usize * channels;
                                for (channel, sum) in sums[..channels].iter_mut().enumerate() {
                                    *sum += raw[i + channel] as u64;
                                }
                                n += 1;
                            }
                        }
                        let n = n.max(1);
                        let o = local_row * row_bytes + x * channels;
                        for (channel, sum) in sums[..channels].iter().enumerate() {
                            chunk[o + channel] = (sum / n) as u8;
                        }
                    }
                }
            });
        }
    });

    out
}

/// True for a file the tiled-TIFF reader should be offered first.
fn is_tiff(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("tif") || extension.eq_ignore_ascii_case("tiff")
        })
}

/// Decodes any file the viewer can show, trying the readers in the same order
/// the canvas does.
///
/// Exists so the export path cannot decode a narrower set of files than the
/// viewer displays. A tiled BigTIFF is the case that proved it: the `image`
/// crate refuses it outright, so anything that only tried that crate would fail
/// on a file sitting perfectly happily on screen.
pub fn decode_still(path: &Path, target_edge: u32) -> anyhow::Result<image::DynamicImage> {
    // Tiled and pyramidal TIFFs first: they are the ones the `image` crate
    // cannot open, and the level chosen keeps a gigapixel scan affordable.
    if is_tiff(path)
        && let Ok(mut tiff) = TiledTiff::open(path)
    {
        let level = tiff.level_for(target_edge.max(1));
        if let Ok(rgba) = tiff.decode_level(level) {
            return Ok(image::DynamicImage::ImageRgba8(rgba));
        }
    }

    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    match crate::codecs::sniff(&bytes) {
        Some(kind) => crate::codecs::decode(&bytes, kind, target_edge),
        None => Ok(reader_for(&bytes)?.decode()?),
    }
}

/// Downscales any image, using the fast parallel area filter where the layout
/// allows and a general resample otherwise.
///
/// The single entry point for shrinking, so the viewer and the export path
/// cannot drift apart on which filter they use.
pub fn downscale_to(
    image: &image::DynamicImage,
    target_width: u32,
    target_height: u32,
    threads: usize,
) -> image::DynamicImage {
    downscale_dynamic(image, target_width, target_height, threads.max(1)).unwrap_or_else(|| {
        // 16-bit and floating point sources land here; `Triangle` is a
        // reasonable compromise and these are rare.
        image.resize_exact(
            target_width,
            target_height,
            image::imageops::FilterType::Triangle,
        )
    })
}

/// Applies [`downscale_box`] to a decoded image, when its layout allows it.
/// Returns `None` for anything that is not 8-bit RGB or RGBA, so the caller can
/// fall back rather than silently converting a 16-bit source down to 8.
fn downscale_dynamic(
    decoded: &image::DynamicImage,
    target_width: u32,
    target_height: u32,
    threads: usize,
) -> Option<image::DynamicImage> {
    let (width, height) = (decoded.width(), decoded.height());
    match decoded {
        image::DynamicImage::ImageRgb8(buffer) => {
            let out = downscale_box(
                buffer.as_raw(),
                width,
                height,
                3,
                target_width,
                target_height,
                threads,
            );
            image::RgbImage::from_raw(target_width, target_height, out)
                .map(image::DynamicImage::ImageRgb8)
        }
        image::DynamicImage::ImageRgba8(buffer) => {
            let out = downscale_box(
                buffer.as_raw(),
                width,
                height,
                4,
                target_width,
                target_height,
                threads,
            );
            image::RgbaImage::from_raw(target_width, target_height, out)
                .map(image::DynamicImage::ImageRgba8)
        }
        _ => None,
    }
}

/// Largest size with the same aspect ratio whose longest edge is `max_edge`.
pub fn fit_within(width: u32, height: u32, max_edge: u32) -> [u32; 2] {
    if width >= height {
        let scaled = (height as u64 * max_edge as u64 / width as u64).max(1) as u32;
        [max_edge, scaled]
    } else {
        let scaled = (width as u64 * max_edge as u64 / height as u64).max(1) as u32;
        [scaled, max_edge]
    }
}

#[cfg(test)]
mod base_layer {
    use super::*;

    /// What the app reads when a converted map is opened.
    /// `GEO_REF_MAPPER_TIFF=/path/to.tif cargo test --release which_layer -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_TIFF"]
    fn which_layer() {
        let Ok(path) = std::env::var("GEO_REF_MAPPER_TIFF") else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        let tiff = TiledTiff::open(&path).expect("it opens");
        let sizes = tiff.level_sizes();
        println!("  the file holds {} layers: {sizes:?}", sizes.len());

        let (level, width, height, pixels) = base_layer_choice(&path).expect("a layer is chosen");
        println!(
            "  opening reads layer {} ({width} x {height}) = {:.1} million pixels",
            level + 1,
            pixels as f64 / 1e6,
        );

        let (biggest_width, biggest_height) = sizes[0];
        let biggest = u64::from(biggest_width) * u64::from(biggest_height);
        println!(
            "  the biggest layer is {:.1} million pixels — {:.1}x more",
            biggest as f64 / 1e6,
            biggest as f64 / pixels.max(1) as f64,
        );

        assert!(
            level > 0 || sizes.len() == 1,
            "a map with layers must not open by reading the biggest one",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_within_preserves_aspect_and_caps_the_long_edge() {
        assert_eq!(fit_within(16000, 8000, 8192), [8192, 4096]);
        assert_eq!(fit_within(8000, 16000, 8192), [4096, 8192]);
        assert_eq!(fit_within(1000, 1000, 8192), [8192, 8192]);
    }

    #[test]
    fn fit_within_never_collapses_to_zero() {
        let [width, height] = fit_within(100_000, 1, 8192);
        assert_eq!(width, 8192);
        assert!(
            height >= 1,
            "an extreme aspect ratio must still have height"
        );
    }

    #[test]
    fn visible_jobs_are_taken_before_preloads() {
        let mut queue = vec![
            Job {
                path: PathBuf::from("preload"),
                priority: Priority::Preload,
                generation: 5,
            },
            Job {
                path: PathBuf::from("visible"),
                priority: Priority::Visible,
                generation: 1,
            },
        ];
        let job = Shared::take_next(&mut queue).unwrap();
        assert_eq!(job.path, PathBuf::from("visible"));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn newer_requests_win_within_a_priority() {
        let mut queue = vec![
            Job {
                path: PathBuf::from("old"),
                priority: Priority::Preload,
                generation: 1,
            },
            Job {
                path: PathBuf::from("new"),
                priority: Priority::Preload,
                generation: 9,
            },
        ];
        assert_eq!(
            Shared::take_next(&mut queue).unwrap().path,
            PathBuf::from("new")
        );
    }

    #[test]
    fn take_next_drains_to_empty() {
        let mut queue = vec![Job {
            path: PathBuf::from("only"),
            priority: Priority::Visible,
            generation: 0,
        }];
        assert!(Shared::take_next(&mut queue).is_some());
        assert!(Shared::take_next(&mut queue).is_none());
    }

    #[test]
    fn timings_total_sums_the_stages() {
        let timings = DecodeTimings {
            read_ms: 1.0,
            decode_ms: 2.0,
            resize_ms: 4.0,
            upload_ms: 8.0,
            ..Default::default()
        };
        assert_eq!(timings.total_ms(), 15.0);
    }

    #[test]
    fn decode_reads_a_real_file() {
        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-decode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.png");
        image::RgbImage::new(4, 3)
            .save_with_format(&path, image::ImageFormat::Png)
            .unwrap();

        let (tx, _rx) = channel();
        let decoded = decode(&path, 0, 1, 1, &tx).unwrap();
        assert_eq!(decoded.image.size, [4, 3]);
        assert_eq!(decoded.timings.decoded_bytes, 4 * 3 * 4);
        assert!(decoded.timings.downscaled_from.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 372 MB, 21760 × 15104 map failed to open at all: the `image` crate's
    /// default 512 MB allocation ceiling rejected it before decoding started.
    /// 16000 × 12000 RGB needs 576 MB, so it clears that ceiling too.
    #[test]
    fn an_image_larger_than_the_default_alloc_ceiling_still_decodes() {
        const WIDTH: u32 = 16_000;
        const HEIGHT: u32 = 12_000;
        assert!(
            u64::from(WIDTH) * u64::from(HEIGHT) * 3 > 512 * 1024 * 1024,
            "the sample has to exceed the crate default or this proves nothing"
        );

        // Uniform pixels, so the encoded file stays small and the cost is the
        // allocation rather than the compression.
        let source = image::RgbImage::from_pixel(WIDTH, HEIGHT, image::Rgb([7, 9, 11]));
        let mut encoded = Vec::new();
        image::DynamicImage::ImageRgb8(source)
            .write_to(
                &mut std::io::Cursor::new(&mut encoded),
                image::ImageFormat::Png,
            )
            .expect("encoding the sample");

        let rejected = image::ImageReader::new(std::io::Cursor::new(&encoded[..]))
            .with_guessed_format()
            .expect("guessing the format")
            .decode();
        assert!(
            matches!(rejected, Err(image::ImageError::Limits(_))),
            "the crate default should still reject this, or the test has stopped testing anything"
        );

        let decoded = reader_for(&encoded).expect("building a reader").decode();
        let decoded = decoded.expect("our limits must accept an image this size");
        assert_eq!(decoded.width(), WIDTH);
        assert_eq!(decoded.height(), HEIGHT);
    }

    /// A phone photo shot on its side stores landscape pixels plus a tag saying
    /// to turn them. Ignoring that tag is why such photos appeared sideways.
    #[test]
    fn a_rotated_jpeg_is_decoded_upright() {
        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // A wide image, so a 90° turn is unmistakable in the dimensions.
        let mut encoded = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(40, 10))
            .write_to(
                &mut std::io::Cursor::new(&mut encoded),
                image::ImageFormat::Jpeg,
            )
            .unwrap();

        // An APP1 segment declaring orientation 6 (rotate 90° clockwise).
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42_u16.to_le_bytes());
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112_u16.to_le_bytes()); // Orientation
        tiff.extend_from_slice(&3_u16.to_le_bytes()); // SHORT
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&[6, 0, 0, 0]);
        tiff.extend_from_slice(&0_u32.to_le_bytes());

        let mut app1 = Vec::from(*b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&app1);
        jpeg.extend_from_slice(&encoded[2..]); // everything after the original SOI

        let path = dir.join("sideways.jpg");
        std::fs::write(&path, &jpeg).unwrap();

        // The tag must survive the round trip, or the test proves nothing.
        assert_eq!(
            crate::exif::orientation(&jpeg),
            crate::exif::Orientation::Rotate90,
        );

        let (tx, _rx) = channel();
        let decoded = decode(&path, 0, 1, 1, &tx).unwrap();
        assert_eq!(
            decoded.image.size,
            [10, 40],
            "a 40x10 image tagged 'rotate 90' must be shown as 10x40"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_reports_a_missing_file_as_an_error() {
        let (tx, _rx) = channel();
        assert!(decode(Path::new("/nonexistent/geo-ref_mapper/none.png"), 0, 1, 1, &tx).is_err());
    }

    #[test]
    fn box_downscale_averages_and_matches_across_thread_counts() {
        // A gradient, so an averaging bug shows up as a wrong value rather than
        // a uniform block that any filter would get right.
        let source = image::RgbImage::from_fn(64, 40, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 6) as u8, ((x + y) * 2) as u8])
        });

        let one = downscale_box(source.as_raw(), 64, 40, 3, 16, 10, 1);
        assert_eq!(one.len(), 16 * 10 * 3);
        for threads in [2_usize, 3, 8, 16] {
            let many = downscale_box(source.as_raw(), 64, 40, 3, 16, 10, threads);
            assert_eq!(
                many, one,
                "{threads} threads must produce the same pixels as one"
            );
        }

        // Top-left output pixel averages the 4x4 source block at the origin.
        let expected_r = (0..4).flat_map(|_| (0..4).map(|x| x * 4)).sum::<u32>() / 16;
        assert_eq!(one[0] as u32, expected_r);
    }

    #[test]
    fn box_downscale_keeps_the_alpha_channel() {
        let source = image::RgbaImage::from_fn(8, 8, |x, _| {
            image::Rgba([255, 128, 64, if x < 4 { 0 } else { 200 }])
        });
        let out = downscale_box(source.as_raw(), 8, 8, 4, 2, 2, 4);
        assert_eq!(out.len(), 2 * 2 * 4);
        assert_eq!(out[3], 0, "the transparent half must stay transparent");
        assert_eq!(out[7], 200, "the opaque half must stay opaque");
        assert_eq!(out[0], 255, "colour must survive alongside alpha");
    }

    #[test]
    fn box_downscale_handles_awkward_ratios_without_panicking() {
        let source = image::RgbImage::from_pixel(7, 3, image::Rgb([9, 9, 9]));
        // Not a divisor, and a target larger than the source on one axis.
        let out = downscale_box(source.as_raw(), 7, 3, 3, 3, 1, 4);
        assert_eq!(out.len(), 3 * 3);
        assert_eq!(out[0], 9);

        let same = downscale_box(source.as_raw(), 7, 3, 3, 7, 3, 4);
        assert_eq!(&same, source.as_raw(), "a no-op resize must copy");
    }

    /// Prices the downscale stage, which the stage breakdown showed is 44-60%
    /// of loading a large image: current filter, area averaging, that spread
    /// across cores, and the GPU.
    ///
    /// `GEO_REF_MAPPER_PNG_BENCH="/a.png" cargo test --release bench_downscale -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_PNG_BENCH"]
    fn bench_downscale() {
        let Ok(list) = std::env::var("GEO_REF_MAPPER_PNG_BENCH") else {
            eprintln!("set GEO_REF_MAPPER_PNG_BENCH to a colon-separated list of images");
            return;
        };
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8);

        for path in list.split(':').filter(|s| !s.is_empty()).map(PathBuf::from) {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(reader) = reader_for(&bytes) else {
                continue;
            };
            let Ok(decoded) = reader.decode() else {
                eprintln!("skipping undecodable {}", path.display());
                continue;
            };
            let (w, h) = (decoded.width(), decoded.height());
            if w.max(h) <= MAX_TEXTURE_EDGE {
                continue; // Nothing to downscale.
            }
            let [tw, th] = fit_within(w, h, MAX_TEXTURE_EDGE);
            let rgb = decoded.to_rgb8();
            let name = path.file_name().unwrap_or_default().to_string_lossy();

            println!("\n=== downscale {} ===", &name[..name.len().min(50)]);
            println!(
                "  {w} x {h} -> {tw} x {th}  ({:.0} MB source RGB)",
                (w as f64 * h as f64 * 3.0) / 1e6
            );

            let t = Instant::now();
            let triangle = decoded.resize_exact(tw, th, image::imageops::FilterType::Triangle);
            let triangle_ms = t.elapsed().as_secs_f64() * 1000.0;
            println!("  Triangle (current)   {triangle_ms:>8.1} ms   1.00x");

            let t = Instant::now();
            let _ = decoded.thumbnail_exact(tw, th);
            let box_ms = t.elapsed().as_secs_f64() * 1000.0;
            println!(
                "  thumbnail_exact      {box_ms:>8.1} ms   {:.2}x",
                triangle_ms / box_ms
            );

            let mut best_parallel = f64::MAX;
            for threads in [1_usize, 4, 8, 16, 32]
                .into_iter()
                .filter(|t| *t <= cores * 2)
            {
                let t = Instant::now();
                let out = downscale_box(rgb.as_raw(), w, h, 3, tw, th, threads);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                best_parallel = best_parallel.min(ms);
                println!(
                    "  box, {threads:>2} thread(s)     {ms:>8.1} ms   {:.2}x   (checksum {})",
                    triangle_ms / ms,
                    out.iter().step_by(9973).map(|b| *b as u64).sum::<u64>(),
                );
            }

            match crate::gpu::Gpu::new() {
                None => println!("  GPU                    unavailable"),
                Some(mut gpu) => {
                    // First call also warms the CUDA context and allocations.
                    let _ = gpu.resize_rgb8(rgb.as_raw(), w, h, tw, th, false);
                    let source_mb = (w as f64 * h as f64 * 3.0) / 1e6;
                    let target_mb = (tw as f64 * th as f64 * 3.0) / 1e6;

                    for pinned in [false, true] {
                        let label = if pinned { "pinned " } else { "pageable" };
                        let t = Instant::now();
                        match gpu.resize_rgb8(rgb.as_raw(), w, h, tw, th, pinned) {
                            Ok((out, gt)) => {
                                let ms = t.elapsed().as_secs_f64() * 1000.0;
                                println!(
                                    "  GPU NPP, {label}   {ms:>8.1} ms   {:.2}x   (vs best CPU box {:.2}x)",
                                    triangle_ms / ms,
                                    best_parallel / ms,
                                );
                                println!(
                                    "    stage {:>6.1}  upload {:>6.1} ({:>5.1} GB/s)  resize {:>6.1}  download {:>5.1} ({:>4.1} GB/s)",
                                    gt.stage_ms,
                                    gt.upload_ms,
                                    source_mb / 1000.0 / (gt.upload_ms as f64 / 1000.0),
                                    gt.resize_ms,
                                    gt.download_ms,
                                    target_mb / 1000.0 / (gt.download_ms as f64 / 1000.0),
                                );
                                // What it would cost if the decoder wrote
                                // straight into the pinned buffer.
                                if pinned {
                                    let without_stage = (gt.total_ms() - gt.stage_ms) as f64;
                                    println!(
                                        "    without the staging copy: {without_stage:.1} ms  ({:.2}x vs best CPU box)",
                                        best_parallel / without_stage,
                                    );
                                }
                                // Sanity: the GPU result should look like the CPU one.
                                let cpu = downscale_box(rgb.as_raw(), w, h, 3, tw, th, cores);
                                let n = out.len().min(cpu.len());
                                let diff: u64 = (0..n)
                                    .map(|i| (out[i] as i32 - cpu[i] as i32).unsigned_abs() as u64)
                                    .sum();
                                println!(
                                    "    mean |GPU - CPU box| per byte: {:.2}",
                                    diff as f64 / n as f64
                                );
                            }
                            Err(error) => println!("  GPU NPP ({label}) failed: {error:#}"),
                        }
                    }
                }
            }
            let _ = triangle;
        }
        println!();
    }

    /// Concatenated IDAT payload — the zlib stream a PNG decoder inflates.
    /// Returns the declared size too, so the bench can report throughput.
    fn png_idat(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32, u8, u8)> {
        if bytes.len() < 33 || &bytes[1..4] != b"PNG" {
            return None;
        }
        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        let (depth, colour) = (bytes[24], bytes[25]);

        let mut idat = Vec::new();
        let mut offset = 8;
        while offset + 8 <= bytes.len() {
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().ok()?) as usize;
            let kind = &bytes[offset + 4..offset + 8];
            let start = offset + 8;
            let end = start.checked_add(length)?;
            if end > bytes.len() {
                break;
            }
            if kind == b"IDAT" {
                idat.extend_from_slice(&bytes[start..end]);
            }
            if kind == b"IEND" {
                break;
            }
            offset = end + 4; // Skip the CRC.
        }
        (!idat.is_empty()).then_some((idat, width, height, depth, colour))
    }

    /// Opt-in breakdown of where PNG load time actually goes, so "decoding is
    /// slow" can be split into inflate, unfilter, downscale and repack rather
    /// than guessed at.
    ///
    /// `GEO_REF_MAPPER_PNG_BENCH="/a.png:/b.png" cargo test --release bench_png_stages -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_PNG_BENCH"]
    fn bench_png_stages() {
        use std::io::Read;

        let Ok(list) = std::env::var("GEO_REF_MAPPER_PNG_BENCH") else {
            eprintln!("set GEO_REF_MAPPER_PNG_BENCH to a colon-separated list of PNGs");
            return;
        };
        let files: Vec<PathBuf> = list
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        assert!(!files.is_empty(), "no files given");

        for path in &files {
            let Ok(bytes) = std::fs::read(path) else {
                eprintln!("skipping unreadable {}", path.display());
                continue;
            };
            let file_mb = bytes.len() as f64 / 1e6;

            // Cold read, so the read column is not page-cache fiction.
            unsafe {
                use std::os::unix::ffi::OsStrExt;
                let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                let fd = libc::open(c.as_ptr(), libc::O_RDONLY);
                if fd >= 0 {
                    libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED);
                    libc::close(fd);
                }
            }
            let t = Instant::now();
            let cold = std::fs::read(path).expect("cold read");
            let read_ms = t.elapsed().as_secs_f64() * 1000.0;
            drop(cold);

            // Inflate alone: everything the decoder does before unfiltering.
            let Some((idat, width, height, depth, colour)) = png_idat(&bytes) else {
                eprintln!("skipping non-PNG {}", path.display());
                continue;
            };
            // Pre-sized, or `Vec` doubling would be charged to inflate and swamp
            // the figure for small files.
            let channels = match colour {
                0 | 3 => 1,
                4 => 2,
                6 => 4,
                _ => 3,
            };
            let expected = height as usize * (1 + width as usize * channels * depth as usize / 8);
            let t = Instant::now();
            let mut raw = Vec::with_capacity(expected + 64);
            let inflated = flate2::read::ZlibDecoder::new(std::io::Cursor::new(&idat))
                .read_to_end(&mut raw)
                .is_ok();
            let inflate_ms = t.elapsed().as_secs_f64() * 1000.0;
            let raw_mb = raw.len() as f64 / 1e6;
            drop(raw);

            // The decoder the viewer actually calls: inflate + unfilter + pack.
            // The viewer's exact call, to show whether default limits reject it.
            let limited = image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .expect("guessing the format")
                .decode();
            let limit_rejected = matches!(&limited, Err(image::ImageError::Limits(_)));
            drop(limited);

            let t = Instant::now();
            let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .expect("guessing the format");
            reader.no_limits();
            let decoded = reader.decode().expect("decoding");
            let decode_ms = t.elapsed().as_secs_f64() * 1000.0;

            // Downscale, exactly as `decode` does it.
            let (w, h) = (decoded.width(), decoded.height());
            let oversized = w.max(h) > MAX_TEXTURE_EDGE;
            let [tw, th] = fit_within(w, h, MAX_TEXTURE_EDGE);
            let threads = parallel_threads();
            let t = Instant::now();
            let scaled = if oversized {
                downscale_dynamic(&decoded, tw, th, threads).expect("an 8-bit source")
            } else {
                decoded.clone()
            };
            let resize_ms = t.elapsed().as_secs_f64() * 1000.0;

            // What that stage used to cost, for comparison.
            let old_ms = if oversized {
                let t = Instant::now();
                let _ = decoded.resize_exact(tw, th, image::imageops::FilterType::Triangle);
                t.elapsed().as_secs_f64() * 1000.0
            } else {
                0.0
            };
            drop(decoded);

            let size = [scaled.width() as usize, scaled.height() as usize];
            let t = Instant::now();
            let colour_image = match scaled {
                image::DynamicImage::ImageRgb8(buffer) => {
                    egui::ColorImage::from_rgb(size, buffer.as_raw())
                }
                other => {
                    egui::ColorImage::from_rgba_unmultiplied(size, other.into_rgba8().as_raw())
                }
            };
            let convert_ms = t.elapsed().as_secs_f64() * 1000.0;
            drop(colour_image);

            let total = read_ms + decode_ms + resize_ms + convert_ms;
            let pct = |ms: f64| ms / total * 100.0;
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            println!("\n=== {} ===", &name[..name.len().min(60)]);
            println!(
                "  {width} x {height}  {:.1} MP  depth {depth} colour-type {colour}  {file_mb:.0} MB file  {raw_mb:.0} MB raw{}",
                (width as f64 * height as f64) / 1e6,
                if inflated { "" } else { " (inflate failed)" },
            );
            if limit_rejected {
                println!(
                    "  !! the viewer's own decode() REJECTS this file: image-crate default limits"
                );
            }
            println!(
                "  read (cold)   {read_ms:>8.1} ms  {:>5.1}%   {:>5.0} MB/s",
                pct(read_ms),
                file_mb / (read_ms / 1000.0)
            );
            println!(
                "  decode        {decode_ms:>8.1} ms  {:>5.1}%",
                pct(decode_ms)
            );
            println!(
                "    of which inflate {inflate_ms:>8.1} ms  ({:>4.1}% of decode, {:>5.0} MB/s out)",
                inflate_ms / decode_ms * 100.0,
                raw_mb / (inflate_ms / 1000.0)
            );
            println!(
                "    unfilter+pack    {:>8.1} ms  ({:>4.1}% of decode)",
                decode_ms - inflate_ms,
                (decode_ms - inflate_ms) / decode_ms * 100.0
            );
            if oversized {
                println!(
                    "  downscale     {resize_ms:>8.1} ms  {:>5.1}%   {w}x{h} -> {tw}x{th} (box, {threads} threads)",
                    pct(resize_ms)
                );
                println!(
                    "    was {old_ms:.1} ms with Triangle — {:.1}x faster now",
                    old_ms / resize_ms
                );
            } else {
                println!(
                    "  downscale     {resize_ms:>8.1} ms  {:>5.1}%   (not oversized, no resize)",
                    pct(resize_ms)
                );
            }
            println!(
                "  convert       {convert_ms:>8.1} ms  {:>5.1}%",
                pct(convert_ms)
            );
            println!("  TOTAL         {total:>8.1} ms");
        }
        println!();
    }
}

/// Decodes a whole folder of test images and reports what happened to each.
///
/// A harness for conformance suites — PngSuite, the OpenEXR images, the AVIF
/// and HEIF test sets — which exist to be run in bulk rather than looked at one
/// at a time. It answers three questions the app cannot be asked by hand: what
/// decodes, what refuses, and what *crashes*, which for a suite that
/// deliberately includes malformed files is the one that matters.
///
/// ```text
/// GEO_REF_MAPPER_SURVEY_DIR=~/testdata cargo test --release decode_survey -- --ignored --nocapture
/// ```
#[cfg(test)]
mod decode_survey {
    use std::path::{Path, PathBuf};

    /// What became of one file.
    enum Outcome {
        Decoded {
            width: u32,
            height: u32,
        },
        Refused(String),
        /// The decoder panicked. Caught rather than allowed to take the run
        /// down: a suite of deliberately-broken files would otherwise stop at
        /// the first one, and which files panic is the point of running it.
        Panicked,
    }

    impl Outcome {
        fn tag(&self) -> &'static str {
            match self {
                Self::Decoded { .. } => "ok",
                Self::Refused(_) => "refused",
                Self::Panicked => "PANIC",
            }
        }
    }

    fn attempt(path: &Path) -> Outcome {
        let path = path.to_path_buf();
        // The hook is silenced for the duration: a panicking decoder would
        // otherwise print a backtrace per file and bury the report.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(|| super::decode_still(&path, 2048));
        std::panic::set_hook(previous);

        match result {
            Ok(Ok(image)) => Outcome::Decoded {
                width: image.width(),
                height: image.height(),
            },
            Ok(Err(error)) => Outcome::Refused(format!("{error:#}").replace('\n', " ")),
            Err(_) => Outcome::Panicked,
        }
    }

    fn files_under(root: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| entry.path().to_path_buf())
            // Suites ship their own documentation and checksums alongside the
            // images; neither is a decode failure worth reporting.
            .filter(|path| {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let skip = [
                    ".md",
                    ".txt",
                    ".html",
                    ".json",
                    ".yml",
                    ".yaml",
                    ".sha1",
                    ".gitignore",
                ];
                !skip.iter().any(|suffix| name.ends_with(suffix))
            })
            .collect();
        found.sort();
        found
    }

    #[test]
    #[ignore = "needs GEO_REF_MAPPER_SURVEY_DIR"]
    fn decode_survey() {
        let Some(root) = std::env::var_os("GEO_REF_MAPPER_SURVEY_DIR") else {
            panic!("set GEO_REF_MAPPER_SURVEY_DIR to a folder of test images");
        };
        let root = PathBuf::from(root);
        let files = files_under(&root);
        assert!(!files.is_empty(), "no files under {}", root.display());

        // Keyed by extension, since that is how a suite is organised and how a
        // gap in the decoders shows up.
        let mut by_extension: std::collections::BTreeMap<String, (usize, usize, usize)> =
            Default::default();
        let mut notable: Vec<(PathBuf, Outcome)> = Vec::new();

        for path in &files {
            let outcome = attempt(path);
            let extension = path
                .extension()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            let entry = by_extension.entry(extension).or_default();
            match &outcome {
                Outcome::Decoded { .. } => entry.0 += 1,
                Outcome::Refused(_) => entry.1 += 1,
                Outcome::Panicked => entry.2 += 1,
            }
            if !matches!(outcome, Outcome::Decoded { .. }) {
                notable.push((path.clone(), outcome));
            }
        }

        println!("\n=== {} files under {} ===\n", files.len(), root.display());
        println!(
            "{:<12} {:>7} {:>9} {:>7}",
            "EXTENSION", "ok", "refused", "PANIC"
        );
        for (extension, (ok, refused, panicked)) in &by_extension {
            println!(
                "{:<12} {ok:>7} {refused:>9} {panicked:>7}",
                format!(".{extension}"),
            );
        }

        let (ok, refused, panicked) = by_extension.values().fold((0, 0, 0), |sum, count| {
            (sum.0 + count.0, sum.1 + count.1, sum.2 + count.2)
        });
        println!("{:<12} {ok:>7} {refused:>9} {panicked:>7}", "TOTAL");

        if !notable.is_empty() {
            println!("\n--- not decoded ---");
            for (path, outcome) in &notable {
                let name = path.strip_prefix(&root).unwrap_or(path).display();
                match outcome {
                    Outcome::Refused(why) => {
                        // Truncated: some decoders return a paragraph, and the
                        // first line is what identifies the failure.
                        let why: String = why.chars().take(110).collect();
                        println!("  {:<7} {name}\n            {why}", outcome.tag());
                    }
                    _ => println!("  {:<7} {name}", outcome.tag()),
                }
            }
        }
        println!();
    }
}
