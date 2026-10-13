//! Running an [`crate::ops::Recipe`] over many files on worker threads.
//!
//! The safety rules live here rather than in the UI, because a rule that only
//! exists in a button handler is one refactor away from being gone:
//!
//! - Originals are never written to, moved or deleted. The only filesystem
//!   writes are new files under the chosen output folder.
//! - A destination that already exists is skipped unless overwriting was asked
//!   for on this specific job.
//! - A job whose output folder would put a file on top of its own source is
//!   refused before any work starts, not part way through.
//! - [`plan`] answers "what would this do?" without touching the disk, so the
//!   UI can show the whole outcome before anything happens.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

use crate::ops::{Outcome, Recipe, Target};

/// A batch, fully specified. Everything needed to run it, and nothing about how
/// it is displayed.
#[derive(Clone, Debug)]
pub struct Job {
    pub sources: Vec<PathBuf>,
    pub output_dir: PathBuf,
    pub recipe: Recipe,
    /// Appended to the file stem, before the extension.
    pub suffix: String,
    /// Off by default, and never remembered between jobs.
    pub overwrite: bool,
}

impl Job {
    pub fn new(sources: Vec<PathBuf>, output_dir: PathBuf, recipe: Recipe) -> Self {
        Self {
            sources,
            output_dir,
            recipe,
            suffix: String::new(),
            overwrite: false,
        }
    }
}

/// What a job would do to one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    pub source: PathBuf,
    pub destination: PathBuf,
    /// The destination is already there and would be skipped, or overwritten.
    pub exists: bool,
}

/// A problem serious enough that the whole job should not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    NoSources,
    /// Two sources would produce the same output file.
    Collision {
        a: PathBuf,
        b: PathBuf,
        at: PathBuf,
    },
    /// The output would land on a file the job also reads.
    WouldOverwriteSource {
        source: PathBuf,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSources => write!(formatter, "nothing selected"),
            Self::Collision { a, b, at } => write!(
                formatter,
                "{} and {} would both be written to {}",
                a.display(),
                b.display(),
                at.display()
            ),
            Self::WouldOverwriteSource { source } => write!(
                formatter,
                "this would write over the original {} — choose another folder or add a suffix",
                source.display()
            ),
        }
    }
}

/// Works out what a job would do, without doing any of it.
///
/// Every refusal here is a case where running the job could destroy something.
/// They are returned together rather than one at a time so the UI can show the
/// whole picture instead of making the user fix them one by one.
pub fn plan(job: &Job) -> std::result::Result<Vec<Planned>, Refusal> {
    if job.sources.is_empty() {
        return Err(Refusal::NoSources);
    }

    let mut planned: Vec<Planned> = Vec::with_capacity(job.sources.len());
    for source in &job.sources {
        let Ok(destination) =
            crate::ops::output_path(source, &job.output_dir, job.recipe.target, &job.suffix)
        else {
            return Err(Refusal::WouldOverwriteSource {
                source: source.clone(),
            });
        };

        // Writing onto any source in the batch, not just this one, is fatal:
        // a later file would read bytes an earlier one had already replaced.
        if let Some(clash) = job.sources.iter().find(|other| **other == destination) {
            return Err(Refusal::WouldOverwriteSource {
                source: clash.clone(),
            });
        }
        if let Some(earlier) = planned
            .iter()
            .find(|other| other.destination == destination)
        {
            return Err(Refusal::Collision {
                a: earlier.source.clone(),
                b: source.clone(),
                at: destination,
            });
        }

        planned.push(Planned {
            exists: destination.exists(),
            source: source.clone(),
            destination,
        });
    }
    Ok(planned)
}

/// Progress and results, sent back as the job runs.
#[derive(Clone, Debug)]
pub enum Event {
    Progress {
        done: usize,
        total: usize,
        current: String,
    },
    Finished {
        written: usize,
        skipped: usize,
        failed: usize,
        bytes_written: u64,
        elapsed: Duration,
        /// The first few failures, for the UI to show without a log file.
        problems: Vec<String>,
    },
}

/// A running batch. Dropping it asks the workers to stop.
pub struct Runner {
    events: Receiver<Event>,
    cancel: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
    pub latest: Option<Event>,
}

impl Runner {
    /// Starts a job that has already been planned. Takes the plan rather than
    /// re-deriving it, so what runs is exactly what the user was shown.
    pub fn start(job: Job, plan: Vec<Planned>, threads: usize, ctx: eframe::egui::Context) -> Self {
        let (sender, events) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let total = plan.len();

        let queue = Arc::new(Mutex::new(plan));
        let done = Arc::new(AtomicUsize::new(0));
        let written = Arc::new(AtomicUsize::new(0));
        let skipped = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let problems = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        let finished_workers = Arc::new(AtomicUsize::new(0));

        let threads = threads.clamp(1, 16).min(total.max(1));
        let workers = (0..threads)
            .map(|_| {
                let job = job.clone();
                let queue = Arc::clone(&queue);
                let cancel = Arc::clone(&cancel);
                let done = Arc::clone(&done);
                let written = Arc::clone(&written);
                let skipped = Arc::clone(&skipped);
                let failed = Arc::clone(&failed);
                let bytes = Arc::clone(&bytes);
                let problems = Arc::clone(&problems);
                let finished_workers = Arc::clone(&finished_workers);
                let sender = sender.clone();
                let ctx = ctx.clone();

                std::thread::Builder::new()
                    .name("geo-ref_mapper-batch".to_owned())
                    .spawn(move || {
                        loop {
                            if cancel.load(Ordering::Relaxed) {
                                break;
                            }
                            let Some(item) = queue.lock().ok().and_then(|mut q| q.pop()) else {
                                break;
                            };

                            let name = item
                                .source
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();

                            match run_one(&item, &job) {
                                Ok(Outcome::Written { bytes: size, .. }) => {
                                    written.fetch_add(1, Ordering::Relaxed);
                                    bytes.fetch_add(size as usize, Ordering::Relaxed);
                                }
                                Ok(Outcome::Skipped { .. }) => {
                                    skipped.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(error) => {
                                    failed.fetch_add(1, Ordering::Relaxed);
                                    if let Ok(mut problems) = problems.lock()
                                        && problems.len() < 10
                                    {
                                        problems.push(format!("{name}: {error:#}"));
                                    }
                                }
                            }

                            let completed = done.fetch_add(1, Ordering::Relaxed) + 1;
                            let _ = sender.send(Event::Progress {
                                done: completed,
                                total,
                                current: name,
                            });
                            ctx.request_repaint();
                        }

                        // The last worker out reports the totals.
                        if finished_workers.fetch_add(1, Ordering::Relaxed) + 1 == threads {
                            let _ = sender.send(Event::Finished {
                                written: written.load(Ordering::Relaxed),
                                skipped: skipped.load(Ordering::Relaxed),
                                failed: failed.load(Ordering::Relaxed),
                                bytes_written: bytes.load(Ordering::Relaxed) as u64,
                                elapsed: started.elapsed(),
                                problems: problems.lock().map(|p| p.clone()).unwrap_or_default(),
                            });
                            ctx.request_repaint();
                        }
                    })
                    .expect("spawning a batch worker")
            })
            .collect();

        Self {
            events,
            cancel,
            workers,
            latest: None,
        }
    }

    /// Drains progress. Call once a frame.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(event) => {
                    self.latest = Some(event.clone());
                    out.push(event);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        out
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_running(&self) -> bool {
        !self.workers.iter().all(|worker| worker.is_finished())
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.cancel();
        let began = std::time::Instant::now();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        // This one is worth waiting for: it is writing the user's files, and
        // stopping mid-write would leave a half-exported picture behind.
        crate::metrics::report_slow_join("the export job", began.elapsed());
    }
}

/// Decodes, applies the recipe and writes one file.
fn run_one(item: &Planned, job: &Job) -> Result<Outcome> {
    // Skip before reading: no point decoding a 300 MB file to then not write it.
    if item.exists && !job.overwrite {
        return Ok(Outcome::Skipped {
            path: item.destination.clone(),
        });
    }

    let bytes = std::fs::read(&item.source)
        .with_context(|| format!("reading {}", item.source.display()))?;

    // A pure copy needs no decoding at all, and is the only way to move a file
    // whose format we can read but not write.
    if job.recipe.target == Target::Original && !job.recipe.changes_pixels() {
        return crate::ops::write_output(&item.destination, &bytes, job.overwrite);
    }

    // The same reader order the viewer uses, so anything on screen can also be
    // exported — a tiled BigTIFF is unreadable to the `image` crate alone.
    let decoded = crate::loader::decode_still(&item.source, 8192)?;

    // Orientation from the file, unless the recipe set one explicitly.
    let mut recipe = job.recipe.clone();
    if recipe.orientation == crate::exif::Orientation::Normal {
        recipe.orientation = crate::exif::orientation(&bytes);
    }

    let processed = crate::ops::apply(&decoded, &recipe, 4)?;
    let target = if recipe.target == Target::Original {
        // Cannot keep the original container once pixels changed, so fall back
        // to the lossless default rather than silently re-encoding as JPEG.
        Target::Png
    } else {
        recipe.target
    };
    let encoded = crate::ops::encode(&processed, target, recipe.quality)?;

    crate::ops::write_output(&item.destination, &encoded, job.overwrite)
}

/// Human-readable summary of a plan, for the confirmation the UI shows.
pub fn describe(plan: &[Planned], job: &Job) -> String {
    let existing = plan.iter().filter(|item| item.exists).count();
    let mut parts = vec![format!("{} file(s)", plan.len())];
    parts.push(format!("to {}", job.output_dir.display()));
    if existing > 0 {
        parts.push(if job.overwrite {
            format!("{existing} will be OVERWRITTEN")
        } else {
            format!("{existing} already exist and will be skipped")
        });
    }
    parts.join(" · ")
}

/// Rejects an output folder that is obviously a bad idea before the file dialog
/// result is accepted.
pub fn check_output_dir(dir: &Path) -> Result<()> {
    if dir.as_os_str().is_empty() {
        bail!("no output folder chosen");
    }
    if dir.is_file() {
        bail!("{} is a file, not a folder", dir.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{Recipe, Resize};

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("geo-ref_mapper-jobs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_png(path: &Path, width: u32, height: u32) {
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            width,
            height,
            image::Rgba([10, 120, 200, 255]),
        ))
        .save_with_format(path, image::ImageFormat::Png)
        .unwrap();
    }

    #[test]
    fn a_plan_reports_every_destination_without_touching_the_disk() {
        let dir = scratch("plan");
        let source = dir.join("a.png");
        write_png(&source, 8, 8);
        let out = dir.join("exports");

        let job = Job::new(vec![source.clone()], out.clone(), Recipe::default());
        let plan = plan(&job).expect("should plan");

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].destination, out.join("a.png"));
        assert!(!plan[0].exists);
        assert!(!out.exists(), "planning must not create the output folder");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rule that matters most: a job must never be able to eat its inputs.
    #[test]
    fn a_job_that_would_write_over_its_own_source_is_refused() {
        let dir = scratch("clobber");
        let source = dir.join("photo.png");
        write_png(&source, 4, 4);

        // Same folder, same format, no suffix: the output is the input.
        let job = Job::new(vec![source.clone()], dir.clone(), Recipe::default());
        assert!(matches!(
            plan(&job),
            Err(Refusal::WouldOverwriteSource { .. })
        ));

        // A suffix makes it safe.
        let mut safe = job.clone();
        safe.suffix = "_web".to_owned();
        assert!(plan(&safe).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_job_that_would_write_over_another_selected_file_is_refused() {
        let dir = scratch("cross");
        let a = dir.join("one.jpg");
        let b = dir.join("one.png");
        write_png(&a, 4, 4);
        write_png(&b, 4, 4);

        // Converting one.jpg to PNG in the same folder would land on one.png,
        // which is also in the selection.
        let job = Job::new(vec![a, b], dir.clone(), Recipe::default());
        assert!(matches!(
            plan(&job),
            Err(Refusal::WouldOverwriteSource { .. })
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_sources_landing_on_one_destination_are_refused() {
        let dir = scratch("collide");
        let sub_a = dir.join("a");
        let sub_b = dir.join("b");
        std::fs::create_dir_all(&sub_a).unwrap();
        std::fs::create_dir_all(&sub_b).unwrap();
        // Same stem in different folders, flattened into one output folder.
        write_png(&sub_a.join("shot.png"), 4, 4);
        write_png(&sub_b.join("shot.png"), 4, 4);

        let job = Job::new(
            vec![sub_a.join("shot.png"), sub_b.join("shot.png")],
            dir.join("out"),
            Recipe::default(),
        );
        assert!(matches!(plan(&job), Err(Refusal::Collision { .. })));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_selection_is_refused() {
        let job = Job::new(Vec::new(), PathBuf::from("/tmp"), Recipe::default());
        assert_eq!(plan(&job), Err(Refusal::NoSources));
    }

    #[test]
    fn running_a_job_writes_the_outputs_and_leaves_sources_untouched() {
        let dir = scratch("run");
        let out = dir.join("exports");
        let sources: Vec<PathBuf> = (0..4)
            .map(|index| {
                let path = dir.join(format!("shot{index}.png"));
                write_png(&path, 40, 20);
                path
            })
            .collect();
        let originals: Vec<Vec<u8>> = sources.iter().map(|p| std::fs::read(p).unwrap()).collect();

        let mut recipe = Recipe {
            resize: Resize::LongestEdge(10),
            target: Target::Png,
            ..Default::default()
        };
        recipe.quality = 90;

        let job = Job::new(sources.clone(), out.clone(), recipe);
        let planned = plan(&job).unwrap();
        let ctx = eframe::egui::Context::default();
        let mut runner = Runner::start(job, planned, 3, ctx);

        // Wait for it, but never for ever.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut finished = None;
        while Instant::now() < deadline {
            for event in runner.poll() {
                if let Event::Finished { .. } = event {
                    finished = Some(event);
                }
            }
            if finished.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let Some(Event::Finished {
            written,
            skipped,
            failed,
            ..
        }) = finished
        else {
            panic!("the job never finished");
        };
        assert_eq!((written, skipped, failed), (4, 0, 0));

        // Outputs exist and are the resized size.
        for index in 0..4 {
            let produced = out.join(format!("shot{index}.png"));
            assert!(produced.exists(), "{} missing", produced.display());
            let image = image::open(&produced).unwrap();
            assert_eq!((image.width(), image.height()), (10, 5));
        }

        // And every original is byte-for-byte what it was.
        for (path, before) in sources.iter().zip(&originals) {
            assert_eq!(
                &std::fs::read(path).unwrap(),
                before,
                "{} was modified",
                path.display()
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_destinations_are_skipped_unless_overwrite_is_set() {
        let dir = scratch("skip");
        let out = dir.join("exports");
        std::fs::create_dir_all(&out).unwrap();
        let source = dir.join("a.png");
        write_png(&source, 8, 8);

        // Something already at the destination, with contents we can check.
        let destination = out.join("a.png");
        std::fs::write(&destination, b"do not touch me").unwrap();

        let job = Job::new(vec![source.clone()], out.clone(), Recipe::default());
        let planned = plan(&job).unwrap();
        assert!(planned[0].exists, "the plan should notice it is there");

        let outcome = run_one(&planned[0], &job).unwrap();
        assert!(matches!(outcome, Outcome::Skipped { .. }));
        assert_eq!(std::fs::read(&destination).unwrap(), b"do not touch me");

        // With overwrite, it goes through.
        let mut forceful = job.clone();
        forceful.overwrite = true;
        let outcome = run_one(&planned[0], &forceful).unwrap();
        assert!(matches!(outcome, Outcome::Written { .. }));
        assert_ne!(std::fs::read(&destination).unwrap(), b"do not touch me");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_of_an_unreadable_format_still_works() {
        let dir = scratch("copy");
        let out = dir.join("exports");
        // Bytes no decoder understands.
        let source = dir.join("mystery.dat");
        std::fs::write(&source, b"not an image at all").unwrap();

        let job = Job::new(
            vec![source.clone()],
            out.clone(),
            Recipe {
                target: Target::Original,
                ..Default::default()
            },
        );
        let planned = plan(&job).unwrap();
        let outcome = run_one(&planned[0], &job).unwrap();

        assert!(matches!(outcome, Outcome::Written { .. }));
        assert_eq!(
            std::fs::read(out.join("mystery.dat")).unwrap(),
            b"not an image at all",
            "a copy must be byte-exact"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs a real batch over a real folder, then checks every source is
    /// byte-for-byte unchanged. The safety rule is only worth anything if it
    /// holds against actual files, not just fixtures.
    ///
    /// `GEO_REF_MAPPER_BATCH_SRC=/dir GEO_REF_MAPPER_BATCH_OUT=/dir cargo test --release bench_batch -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_BATCH_SRC and GEO_REF_MAPPER_BATCH_OUT"]
    fn bench_batch_over_a_real_folder() {
        let (Ok(source_dir), Ok(out_dir)) = (
            std::env::var("GEO_REF_MAPPER_BATCH_SRC"),
            std::env::var("GEO_REF_MAPPER_BATCH_OUT"),
        ) else {
            eprintln!("set GEO_REF_MAPPER_BATCH_SRC and GEO_REF_MAPPER_BATCH_OUT");
            return;
        };

        let sources: Vec<PathBuf> = std::fs::read_dir(&source_dir)
            .expect("reading the source folder")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        assert!(!sources.is_empty(), "no files in {source_dir}");

        // Fingerprint every input so any modification is detectable.
        let before: Vec<(PathBuf, Vec<u8>)> = sources
            .iter()
            .map(|path| (path.clone(), std::fs::read(path).unwrap()))
            .collect();

        let recipe = Recipe {
            resize: Resize::LongestEdge(1200),
            target: Target::Jpeg,
            quality: 88,
            ..Default::default()
        };
        let job = Job::new(sources.clone(), PathBuf::from(&out_dir), recipe);
        let planned = plan(&job).expect("the plan should be accepted");
        println!("\n  {}", describe(&planned, &job));

        let ctx = eframe::egui::Context::default();
        let mut runner = Runner::start(job, planned, 8, ctx);
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut summary = None;
        while Instant::now() < deadline {
            for event in runner.poll() {
                if let Event::Finished { .. } = event {
                    summary = Some(event);
                }
            }
            if summary.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let Some(Event::Finished {
            written,
            skipped,
            failed,
            bytes_written,
            elapsed,
            problems,
        }) = summary
        else {
            panic!("the batch never finished");
        };
        println!(
            "  {written} written, {skipped} skipped, {failed} failed — {:.1} MB in {:.2}s",
            bytes_written as f64 / 1e6,
            elapsed.as_secs_f64()
        );
        for problem in &problems {
            println!("    {problem}");
        }
        for entry in std::fs::read_dir(&out_dir).into_iter().flatten().flatten() {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            println!(
                "    -> {:<44} {:>9} bytes",
                entry.file_name().to_string_lossy(),
                size
            );
        }

        // The whole point.
        for (path, original) in &before {
            assert_eq!(
                &std::fs::read(path).unwrap(),
                original,
                "{} was modified by the batch",
                path.display()
            );
        }
        println!("  every source verified unchanged\n");
    }

    #[test]
    fn the_description_warns_about_existing_files() {
        let dir = scratch("describe");
        let out = dir.join("exports");
        std::fs::create_dir_all(&out).unwrap();
        let source = dir.join("a.png");
        write_png(&source, 4, 4);
        std::fs::write(out.join("a.png"), b"here already").unwrap();

        let mut job = Job::new(vec![source], out, Recipe::default());
        let planned = plan(&job).unwrap();
        assert!(describe(&planned, &job).contains("skipped"));

        job.overwrite = true;
        assert!(describe(&planned, &job).contains("OVERWRITTEN"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
