//! Moving and copying pictures between folders.
//!
//! This is the one place geo-ref_mapper relocates an original. Everything about it
//! is built so that a mistake costs nothing:
//!
//!   * **Nothing is ever overwritten.** A name already taken in the
//!     destination gets a `-1`, `-2` suffix, so a file at the target can never
//!     be replaced by one being moved onto it.
//!   * **A copy is verified before the original is removed.** A move across
//!     devices is a copy followed by a delete, and the delete only happens once
//!     the new file exists at the right size.
//!   * **The plan is separable from the doing.** [`plan`] works out every
//!     destination without touching the disk, so the UI can say exactly what
//!     will happen and how many, before anything happens.
//!
//! Nothing here writes to the *source* folder, and nothing deletes anything on
//! a copy — a failed copy leaves the original exactly where it was.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Copy,
    Move,
}

impl Mode {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::Move => "Move",
        }
    }

    /// Past tense, for the message after the fact.
    pub fn done(self) -> &'static str {
        match self {
            Self::Copy => "Copied",
            Self::Move => "Moved",
        }
    }
}

/// One file and where it is going.
#[derive(Clone, PartialEq, Debug)]
pub struct Planned {
    pub source: PathBuf,
    pub destination: PathBuf,
    /// True when the destination had to be renamed to avoid a collision.
    pub renamed: bool,
}

/// Works out where each file would go, without touching anything.
///
/// Collisions are resolved within the plan as well as against the disk, so two
/// files of the same name arriving from different folders do not both claim it.
pub fn plan(sources: &[PathBuf], destination: &Path) -> Vec<Planned> {
    let mut taken: Vec<PathBuf> = Vec::new();
    let mut out = Vec::new();

    for source in sources {
        let Some(name) = source.file_name() else {
            continue;
        };
        // Already there: nothing to do, and certainly nothing to duplicate.
        let direct = destination.join(name);
        if source == &direct {
            continue;
        }

        let mut candidate = direct.clone();
        let mut suffix = 0;
        while candidate.exists() || taken.contains(&candidate) {
            suffix += 1;
            candidate = destination.join(numbered(name.to_string_lossy().as_ref(), suffix));
        }
        taken.push(candidate.clone());
        out.push(Planned {
            source: source.clone(),
            destination: candidate.clone(),
            renamed: candidate != direct,
        });
    }
    out
}

/// `map.tif` → `map-1.tif`, keeping the extension where it belongs.
fn numbered(name: &str, suffix: usize) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem}-{suffix}.{extension}"),
        _ => format!("{name}-{suffix}"),
    }
}

/// Carries out one planned transfer.
///
/// A move is a rename where it can be, which is atomic and instant within a
/// volume. Across volumes — a picture going from the NVMe to the T5 — a rename
/// is impossible, so it copies and then removes the original, and only after
/// checking the copy arrived whole.
pub fn run_one(item: &Planned, mode: Mode) -> io::Result<()> {
    if let Some(parent) = item.destination.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if mode == Mode::Move && std::fs::rename(&item.source, &item.destination).is_ok() {
        return Ok(());
    }

    let copied = std::fs::copy(&item.source, &item.destination)?;
    let expected = std::fs::metadata(&item.source)?.len();
    if copied != expected {
        // A short copy is a corrupt file. Take it away rather than leave a
        // broken picture sitting where a good one is supposed to be.
        let _ = std::fs::remove_file(&item.destination);
        return Err(io::Error::other(format!(
            "{} copied {copied} of {expected} bytes",
            item.source.display()
        )));
    }

    if mode == Mode::Move {
        std::fs::remove_file(&item.source)?;
    }
    Ok(())
}

/// What happened, for the message afterwards.
#[derive(Clone, Default, Debug)]
pub struct Report {
    pub done: usize,
    pub renamed: usize,
    pub failures: Vec<String>,
}

/// Runs a whole plan, carrying on past a file that will not go.
pub fn run(items: &[Planned], mode: Mode) -> Report {
    let mut report = Report::default();
    for item in items {
        match run_one(item, mode) {
            Ok(()) => {
                report.done += 1;
                if item.renamed {
                    report.renamed += 1;
                }
            }
            Err(error) => {
                let name = item
                    .source
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| item.source.display().to_string());
                report.failures.push(format!("{name} — {error}"));
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("geo-ref_mapper-transfer-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_name_already_taken_is_never_overwritten() {
        let from = scratch("from");
        let to = scratch("to");
        std::fs::write(from.join("map.tif"), b"new").unwrap();
        std::fs::write(to.join("map.tif"), b"the one already there").unwrap();

        let items = plan(&[from.join("map.tif")], &to);
        assert_eq!(items.len(), 1);
        assert!(items[0].renamed);
        assert_eq!(items[0].destination, to.join("map-1.tif"));

        run(&items, Mode::Move);
        assert_eq!(
            std::fs::read(to.join("map.tif")).unwrap(),
            b"the one already there",
            "the file that was there first must survive untouched",
        );
        assert_eq!(std::fs::read(to.join("map-1.tif")).unwrap(), b"new");

        let _ = std::fs::remove_dir_all(&from);
        let _ = std::fs::remove_dir_all(&to);
    }

    #[test]
    fn two_files_of_the_same_name_do_not_collide_with_each_other() {
        // They come from different folders, so nothing on disk warns of it —
        // the plan itself has to keep track.
        let a = scratch("a");
        let b = scratch("b");
        let to = scratch("both");
        std::fs::write(a.join("1930.tif"), b"a").unwrap();
        std::fs::write(b.join("1930.tif"), b"b").unwrap();

        let items = plan(&[a.join("1930.tif"), b.join("1930.tif")], &to);
        assert_eq!(items[0].destination, to.join("1930.tif"));
        assert_eq!(items[1].destination, to.join("1930-1.tif"));

        let report = run(&items, Mode::Copy);
        assert_eq!(report.done, 2);
        assert_eq!(std::fs::read(to.join("1930.tif")).unwrap(), b"a");
        assert_eq!(std::fs::read(to.join("1930-1.tif")).unwrap(), b"b");

        for dir in [a, b, to] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_copy_leaves_the_original_where_it_was() {
        let from = scratch("keep");
        let to = scratch("dest");
        std::fs::write(from.join("photo.jpg"), b"pixels").unwrap();

        run(&plan(&[from.join("photo.jpg")], &to), Mode::Copy);
        assert!(from.join("photo.jpg").exists(), "a copy must not remove it");
        assert!(to.join("photo.jpg").exists());

        let _ = std::fs::remove_dir_all(&from);
        let _ = std::fs::remove_dir_all(&to);
    }

    #[test]
    fn a_file_already_in_the_destination_is_left_alone() {
        let here = scratch("same");
        std::fs::write(here.join("a.png"), b"x").unwrap();
        assert!(
            plan(&[here.join("a.png")], &here).is_empty(),
            "moving a file into the folder it is already in is not a move",
        );
        let _ = std::fs::remove_dir_all(&here);
    }

    #[test]
    fn numbering_keeps_the_extension_on_the_end() {
        assert_eq!(numbered("map.tif", 1), "map-1.tif");
        assert_eq!(numbered("a.b.png", 2), "a.b-2.png");
        assert_eq!(numbered("noextension", 3), "noextension-3");
        assert_eq!(numbered(".hidden", 1), ".hidden-1");
    }

    #[test]
    fn one_awkward_file_does_not_abandon_the_rest() {
        let from = scratch("mixed");
        let to = scratch("mixed-to");
        std::fs::write(from.join("good.png"), b"ok").unwrap();

        let items = vec![
            Planned {
                source: from.join("missing.png"),
                destination: to.join("missing.png"),
                renamed: false,
            },
            Planned {
                source: from.join("good.png"),
                destination: to.join("good.png"),
                renamed: false,
            },
        ];
        let report = run(&items, Mode::Move);
        assert_eq!(report.done, 1);
        assert_eq!(report.failures.len(), 1);
        assert!(to.join("good.png").exists());

        let _ = std::fs::remove_dir_all(&from);
        let _ = std::fs::remove_dir_all(&to);
    }
}
