//! Background folder scanning.
//!
//! *Scan* indexes a single folder, *Hunt* walks it recursively. Both run on a
//! worker thread with their own SQLite connection and report progress back over
//! a channel; the UI thread never blocks on disk.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::db::{ImageRecord, Library, is_image_extension};

/// How often the worker pushes a progress update, at most.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(80);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Just the chosen folder.
    Scan,
    /// The chosen folder and everything under it.
    Hunt,
    /// The pictures a manifest names, and nothing else in the folder.
    ///
    /// A selection is a *subset* of what is on disk — the folder it points into
    /// holds the rejects too — so this walks nothing and reads the list.
    Manifest,
}

impl Mode {
    pub fn is_recursive(self) -> bool {
        matches!(self, Self::Hunt)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Scan => "Scan",
            Self::Hunt => "Hunt",
            Self::Manifest => "Manifest",
        }
    }
}

/// Builds one image's library row, for a file that has appeared on its own —
/// a restore from the trash rather than a walk of a whole folder.
///
/// The same probe the scan uses, so a restored file gets the same dimensions,
/// format and orientation handling it would have had from a full pass.
pub fn record_for(path: &Path) -> Option<crate::db::ImageRecord> {
    let metadata = std::fs::metadata(path).ok()?;
    let exif = crate::exif::from_path_prefix(path, crate::exif::SCAN_PREFIX);
    let orientation = exif
        .as_ref()
        .filter(|_| !is_heif_file(path))
        .map_or_else(crate::exif::Orientation::default, |exif| exif.orientation);
    let probe = probe(path, orientation);
    Some(crate::db::ImageRecord {
        path: path.to_string_lossy().into_owned(),
        folder: path
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .unwrap_or_default(),
        filename: path.file_name()?.to_string_lossy().into_owned(),
        format: probe.as_ref().map(|p| p.format.to_owned()),
        width: probe.as_ref().map(|p| p.width),
        height: probe.as_ref().map(|p| p.height),
        bytes: metadata.len(),
        mtime: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_secs() as i64)
            .unwrap_or_default(),
        georeferenced: Some(is_georeferenced(path)),
        ..Default::default()
    })
}

/// The year a map was drawn, from wherever it is recorded.
///
/// The sidecar first, which is where a source map keeps it. A *converted* map
/// has no sidecar — its record went inside the file — so the XMP packet is
/// read instead, which the converter deliberately writes at the front so a
/// short read finds it.
///
/// Falls back to a four-digit run in the file name, because plenty of these are
/// called `1846.tif` and nothing else states it.
pub fn year_of(path: &Path) -> Option<i32> {
    let from_text = |text: &str| -> Option<i32> {
        let bytes = text.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            if !bytes[at].is_ascii_digit() {
                at += 1;
                continue;
            }
            let start = at;
            while at < bytes.len() && bytes[at].is_ascii_digit() {
                at += 1;
            }
            if at - start == 4 {
                return text[start..at].parse().ok();
            }
        }
        None
    };

    if let Some(sidecar) = crate::sidecar::beside(path)
        && let Some(date) = sidecar.date_iso.or(sidecar.date_text)
        && let Some(year) = from_text(&date)
    {
        return Some(year);
    }
    let meta = crate::meta::from_path(path);
    for (label, value) in meta.xmp.iter().chain(meta.iptc.iter()) {
        if label.starts_with("Date")
            && let Some(year) = from_text(value)
        {
            return Some(year);
        }
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(from_text)
}

/// Whether a file carries GeoTIFF position tags.
///
/// Only TIFFs can, so nothing else is opened — the extension check is not a
/// guess about the contents, it is the whole population. Reading the tags costs
/// a seek and a small read, which is nothing beside the decode the scan is
/// already doing.
pub fn is_georeferenced(path: &Path) -> bool {
    let tiff = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            let lowered = extension.to_ascii_lowercase();
            lowered == "tif" || lowered == "tiff"
        });
    tiff && crate::geotiff::from_path(path).is_some()
}

#[cfg(test)]
mod georeference_detection {
    use super::*;

    /// Counts what the sweep would mark green across a real folder tree.
    /// `GEO_REF_MAPPER_GEO_DIR=~/Pictures cargo test --release geo_survey -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_GEO_DIR"]
    fn geo_survey() {
        let Ok(root) = std::env::var("GEO_REF_MAPPER_GEO_DIR") else {
            return;
        };
        let mut tiffs = 0;
        let mut georeferenced = 0;
        let mut others = 0;
        let started = Instant::now();
        for entry in walkdir::WalkDir::new(&root).into_iter().flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let tiff = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    let lowered = extension.to_ascii_lowercase();
                    lowered == "tif" || lowered == "tiff"
                });
            if tiff {
                tiffs += 1;
                georeferenced += usize::from(is_georeferenced(path));
            } else if is_image_extension(&path.extension().unwrap_or_default().to_string_lossy()) {
                others += 1;
                assert!(
                    !is_georeferenced(path),
                    "{} is not a TIFF and must never be opened for tags",
                    path.display(),
                );
            }
        }
        println!(
            "  {tiffs} TIFFs, {georeferenced} georeferenced, {others} other images, \
             examined in {:.2}s",
            started.elapsed().as_secs_f64(),
        );
        assert!(tiffs > 0, "the folder holds no TIFFs to judge by");
    }
}

#[derive(Clone, Debug)]
pub enum ScanEvent {
    Progress {
        found: usize,
        indexed: usize,
        current: String,
    },
    Done {
        found: usize,
        added: usize,
        unchanged: usize,
        failed: usize,
        elapsed: Duration,
    },
    Failed(String),
}

/// Live view of a scan, kept in the app state and polled once per frame.
#[derive(Default)]
pub struct ScanState {
    receiver: Option<Receiver<ScanEvent>>,
    pub mode: Option<Mode>,
    pub root: Option<PathBuf>,
    pub found: usize,
    pub indexed: usize,
    pub current: String,
    /// Set when a scan finishes, so the panel can show a one-line summary.
    pub summary: Option<String>,
    pub error: Option<String>,
}

impl ScanState {
    pub fn is_running(&self) -> bool {
        self.receiver.is_some()
    }

    pub fn start(&mut self, db_path: PathBuf, root: PathBuf, mode: Mode, ctx: egui::Context) {
        self.start_with(db_path, root, mode, None, ctx);
    }

    /// The same, with the files named outright instead of found by walking.
    ///
    /// Everything after the list is identical — the same change detection, the
    /// same batching, the same progress — because the only thing a manifest
    /// changes is *which* files, not what is done with them.
    pub fn start_with(
        &mut self,
        db_path: PathBuf,
        root: PathBuf,
        mode: Mode,
        listed: Option<Vec<PathBuf>>,
        ctx: egui::Context,
    ) {
        let (sender, receiver) = channel();
        self.receiver = Some(receiver);
        self.mode = Some(mode);
        self.root = Some(root.clone());
        self.found = 0;
        self.indexed = 0;
        self.current.clear();
        self.summary = None;
        self.error = None;

        std::thread::Builder::new()
            .name(format!("geo-ref_mapper-{}", mode.label().to_lowercase()))
            .spawn(move || {
                let result = run(&db_path, &root, mode.is_recursive(), listed, &sender);
                if let Err(error) = result {
                    let _ = sender.send(ScanEvent::Failed(format!("{error:#}")));
                }
                ctx.request_repaint();
            })
            .expect("spawning the scan thread");
    }

    /// Drains queued events. Returns `true` if the library changed and the tree
    /// should be rebuilt.
    pub fn poll(&mut self) -> bool {
        let Some(receiver) = &self.receiver else {
            return false;
        };

        let mut finished = false;
        let mut changed = false;
        loop {
            match receiver.try_recv() {
                Ok(ScanEvent::Progress {
                    found,
                    indexed,
                    current,
                }) => {
                    self.found = found;
                    self.indexed = indexed;
                    self.current = current;
                }
                Ok(ScanEvent::Done {
                    found,
                    added,
                    unchanged,
                    failed,
                    elapsed,
                }) => {
                    self.found = found;
                    self.indexed = added + unchanged;
                    self.current.clear();
                    let mut summary = format!(
                        "{added} added · {unchanged} unchanged · {:.1}s",
                        elapsed.as_secs_f32()
                    );
                    if failed > 0 {
                        summary.push_str(&format!(" · {failed} unreadable"));
                    }
                    self.summary = Some(summary);
                    finished = true;
                    changed = true;
                }
                Ok(ScanEvent::Failed(message)) => {
                    self.error = Some(message);
                    finished = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }

        if finished {
            self.receiver = None;
        }
        changed
    }
}

fn run(
    db_path: &Path,
    root: &Path,
    recursive: bool,
    listed: Option<Vec<PathBuf>>,
    sender: &Sender<ScanEvent>,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let library = Library::open(db_path)?;

    // One iterator of paths, from a walk or from a list. Everything below is
    // written against that and cannot tell which it was given.
    let paths: Box<dyn Iterator<Item = PathBuf>> = match listed {
        Some(files) => Box::new(files.into_iter()),
        None => Box::new(
            walkdir::WalkDir::new(root)
                .follow_links(false)
                .max_depth(if recursive { usize::MAX } else { 1 })
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| entry.path().to_path_buf()),
        ),
    };

    let (mut found, mut added, mut unchanged, mut failed) = (0usize, 0usize, 0usize, 0usize);
    // Pictures whose row was already right but whose sidecar was re-read.
    let mut refreshed = 0usize;
    let mut last_report = Instant::now();
    let mut batch_open = false;

    for path in paths {
        let path = path.as_path();
        let is_image = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(is_image_extension);
        if !is_image {
            continue;
        }
        found += 1;

        // A manifest can name a file that has since gone; a walk cannot. Both
        // land here, and both are counted as failed rather than stopping.
        let Ok(metadata) = std::fs::metadata(path) else {
            failed += 1;
            continue;
        };
        let bytes = metadata.len();
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);

        let path_string = path.to_string_lossy().into_owned();
        if library.is_current(&path_string, bytes, mtime)? {
            unchanged += 1;
            // The *picture* is unchanged; its sidecar may not be. They are
            // separate files, written by different programs at different times,
            // and mapSelector rewrites sidecars as it learns more without ever
            // touching the map.
            //
            // Skipping this is why a re-scan left six thousand pictures with
            // tags in their sidecar and none in the library — every one of them
            // was "unchanged" and so never asked.
            if let Ok(Some(image_id)) = library.id_of_path(&path_string)
                && let Some(sidecar) = crate::sidecar::beside(path)
            {
                if !batch_open {
                    library.begin()?;
                    batch_open = true;
                }
                let _ = library.set_tags(image_id, &sidecar.keywords);
                refreshed += 1;
                // Committed on its own count: a re-scan that changes nothing
                // else never reaches the `added` counter below, and the whole
                // pass would be one transaction held open to the end.
                if refreshed % 512 == 0 {
                    library.commit()?;
                    batch_open = false;
                }
            }
        } else {
            if !batch_open {
                library.begin()?;
                batch_open = true;
            }

            // One bounded read serves both the orientation the dimensions need
            // and the metadata the INFO panel shows.
            let exif = crate::exif::from_path_prefix(path, crate::exif::SCAN_PREFIX);
            // The probe's dimensions come from whichever reader opened the file,
            // and for HEIF that reader already reports the rotated size — so the
            // swap must not be applied a second time.
            let orientation = exif
                .as_ref()
                .filter(|_| !is_heif_file(path))
                .map_or_else(crate::exif::Orientation::default, |exif| exif.orientation);
            let probe = probe(path, orientation);
            let record = ImageRecord {
                path: path_string,
                folder: path
                    .parent()
                    .map(|parent| parent.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                filename: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                format: probe.as_ref().map(|p| p.format.to_owned()),
                width: probe.as_ref().map(|p| p.width),
                height: probe.as_ref().map(|p| p.height),
                bytes,
                mtime,
                georeferenced: Some(is_georeferenced(path)),
                ..Default::default()
            };
            if probe.is_none() {
                failed += 1;
            }

            let image_id = library.upsert_image(&record)?;
            if let Some(exif) = &exif {
                // Failing to store metadata must not abandon the scan.
                let _ = library.put_exif(image_id, &exif.to_record());
            }
            // The tags a sidecar carries, brought in so a set can be picked out
            // by them without opening eight thousand small files to do it. Only
            // the ones that say something — the provider and the bare year are
            // dropped, the same as when they are written into a converted map.
            if let Some(sidecar) = crate::sidecar::beside(path) {
                let _ = library.set_tags(image_id, &sidecar.keywords);
            }
            // Where it sits and when it was drawn, for the timeline. Only for
            // maps that say where they are — a picture with no position cannot
            // be said to cover anything.
            if let Some(geo) = crate::geotiff::from_path(path)
                && let (Some(top_left), Some(bottom_right)) = (geo.top_left(), geo.bottom_right())
            {
                let (north, west) = top_left;
                let (south, east) = bottom_right;
                let _ = library.set_extent(
                    image_id,
                    year_of(path),
                    (
                        west.min(east),
                        south.min(north),
                        west.max(east),
                        south.max(north),
                    ),
                );
            }
            added += 1;

            // Commit periodically so a long hunt is not one giant transaction.
            if added % 512 == 0 {
                library.commit()?;
                batch_open = false;
            }
        }

        if last_report.elapsed() >= PROGRESS_INTERVAL {
            last_report = Instant::now();
            let _ = sender.send(ScanEvent::Progress {
                found,
                indexed: added + unchanged,
                current: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            });
        }
    }

    if batch_open {
        library.commit()?;
    }

    let _ = sender.send(ScanEvent::Done {
        found,
        added,
        unchanged,
        failed,
        elapsed: started.elapsed(),
    });
    Ok(())
}

struct Probe {
    width: u32,
    height: u32,
    format: &'static str,
}

/// Reads just enough of the file to learn its size and format — no full decode.
///
/// `orientation` comes from the caller because it has already read the EXIF; a
/// photo shot on its side stores landscape dimensions with a tag saying to turn
/// it, and recording those unswapped would have the tree and the mosaic disagree
/// with what the canvas shows.
/// The size a picture will be *shown* at, read from its header alone.
///
/// Cheap by design — the header, not the pixels — which is what lets the window
/// be sized to the picture on the frame that opens it rather than a decode
/// later. [`record_for`] answers the same question but also asks whether the
/// file is georeferenced, which opens every TIFF in full; this does not.
///
/// Orientation is applied, so a photograph the camera recorded sideways gives
/// the size it will occupy upright. Without that a portrait phone photo would
/// open in a landscape window and then be letterboxed inside it.
pub fn displayed_size(path: &Path) -> Option<(u32, u32)> {
    let exif = crate::exif::from_path_prefix(path, crate::exif::SCAN_PREFIX);
    let orientation = exif
        .as_ref()
        .filter(|_| !is_heif_file(path))
        .map_or_else(crate::exif::Orientation::default, |exif| exif.orientation);
    probe(path, orientation).map(|probe| (probe.width, probe.height))
}

fn probe(path: &Path, orientation: crate::exif::Orientation) -> Option<Probe> {
    let probed = image::ImageReader::open(path)
        .ok()
        .and_then(|reader| reader.with_guessed_format().ok())
        .and_then(|reader| {
            let format = format_name(reader.format()?);
            let (width, height) = reader.into_dimensions().ok()?;
            Some(Probe {
                width,
                height,
                format,
            })
        })
        .or_else(|| probe_tiled_tiff(path))
        .or_else(|| probe_extra_format(path))?;

    Some(if orientation.swaps_axes() {
        Probe {
            width: probed.height,
            height: probed.width,
            ..probed
        }
    } else {
        probed
    })
}

/// Cheap check for an ISOBMFF file, from its first twelve bytes.
fn is_heif_file(path: &Path) -> bool {
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0_u8; 12];
    match file.read(&mut head) {
        Ok(read) => crate::heif::is_heif(&head[..read]),
        Err(_) => false,
    }
}

/// Photoshop, JPEG XL, SVG and XCF, which the `image` crate does not know.
/// Only the header is read where the format allows it.
fn probe_extra_format(path: &Path) -> Option<Probe> {
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    // Enough for a signature, an SVG root element, and a PSD or JXL header.
    file.take(64 * 1024).read_to_end(&mut head).ok()?;

    let kind = crate::codecs::sniff(&head)?;
    // A whole-file read is only worth it for the formats that need one.
    let (width, height) = match crate::codecs::dimensions(&head, kind) {
        Some(size) => size,
        None => {
            let bytes = std::fs::read(path).ok()?;
            crate::codecs::dimensions(&bytes, kind)?
        }
    };
    Some(Probe {
        width,
        height,
        format: kind.label(),
    })
}

/// Second chance for the files the `image` crate cannot open: a WebP-tiled
/// BigTIFF uses compression 50001, which it has no decoder for, but which
/// [`crate::cog`] handles. Without this those files are indexed with no
/// dimensions at all and never get a thumbnail.
fn probe_tiled_tiff(path: &Path) -> Option<Probe> {
    let tiff = crate::cog::TiledTiff::open(path).ok()?;
    let level = tiff.levels.first()?;
    Some(Probe {
        width: level.width,
        height: level.height,
        format: if tiff.big_tiff { "BigTIFF" } else { "TIFF" },
    })
}

fn format_name(format: image::ImageFormat) -> &'static str {
    use image::ImageFormat::*;
    match format {
        Png => "PNG",
        Jpeg => "JPEG",
        Gif => "GIF",
        WebP => "WebP",
        Pnm => "PNM",
        Tiff => "TIFF",
        Tga => "TGA",
        Dds => "DDS",
        Bmp => "BMP",
        Ico => "ICO",
        Hdr => "Radiance HDR",
        OpenExr => "OpenEXR",
        Farbfeld => "farbfeld",
        Avif => "AVIF",
        Qoi => "QOI",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_recursion() {
        assert!(!Mode::Scan.is_recursive());
        assert!(Mode::Hunt.is_recursive());
    }

    #[test]
    fn scan_indexes_only_images_in_the_top_folder() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        write_png(&dir.join("top.png"));
        write_png(&dir.join("nested/deep.png"));
        std::fs::write(dir.join("notes.txt"), b"ignore me").unwrap();

        let db_path = dir.join("library.db");
        let (sender, receiver) = channel();
        run(&db_path, &dir, false, None, &sender).unwrap();

        let done = receiver.into_iter().find_map(|event| match event {
            ScanEvent::Done { found, added, .. } => Some((found, added)),
            _ => None,
        });
        assert_eq!(done, Some((1, 1)));

        let library = Library::open(&db_path).unwrap();
        let images = library.all_images().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].filename, "top.png");
        assert_eq!(images[0].width, Some(2));
        assert_eq!(images[0].format.as_deref(), Some("PNG"));
    }

    #[test]
    fn hunt_recurses_and_rescanning_reports_unchanged() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        write_png(&dir.join("top.png"));
        write_png(&dir.join("a/one.png"));
        write_png(&dir.join("a/b/two.png"));

        let db_path = dir.join("library.db");
        let (sender, receiver) = channel();
        run(&db_path, &dir, true, None, &sender).unwrap();
        drop(sender);
        let first = receiver.into_iter().find_map(done).unwrap();
        assert_eq!(first, (3, 3, 0));

        let (sender, receiver) = channel();
        run(&db_path, &dir, true, None, &sender).unwrap();
        drop(sender);
        let second = receiver.into_iter().find_map(done).unwrap();
        assert_eq!(second, (3, 0, 3), "a second pass should add nothing");
    }

    fn done(event: ScanEvent) -> Option<(usize, usize, usize)> {
        match event {
            ScanEvent::Done {
                found,
                added,
                unchanged,
                ..
            } => Some((found, added, unchanged)),
            _ => None,
        }
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "geo-ref_mapper-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn write_png(path: &Path) {
        let image = image::RgbImage::new(2, 3);
        image
            .save_with_format(path, image::ImageFormat::Png)
            .unwrap();
    }
}
