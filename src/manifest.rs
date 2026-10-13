//! Reading a selection produced by mapSelector.
//!
//! A manifest names a **subset of what is already on disk** — nothing is copied
//! and no path is invented. It exists because a download of an archive's map
//! collection is mostly not the maps you want: of 10,858 files in one folder,
//! a manifest picked out 8,054, and scanning the folder instead would index the
//! 2,804 already rejected.
//!
//! So it is an alternative to walking a folder, not a new kind of thing for the
//! rest of the program to know about. What it produces is a list of paths; from
//! there the ordinary scan takes over and the pictures are in the library like
//! any others.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// What the `kind` field must say, so a JSON file picked by mistake is refused
/// with a sentence rather than indexing nothing and looking broken.
const KIND: &str = "histomap-manifest";

/// A manifest, taken apart.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Manifest {
    /// What the selection calls itself.
    pub name: String,
    /// Every picture named, in the order the manifest lists them.
    pub images: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Absolute, with the manifest's own root already joined on.
    pub path: PathBuf,
    /// mapSelector's judgement of whether the scan is good enough to use.
    ///
    /// Carried but not acted on: a picture judged unusable is still indexed, so
    /// it can be looked at and disagreed with. A reject that never appears is a
    /// reject nobody can check.
    pub usable: bool,
}

impl Manifest {
    /// How many are marked unusable, for saying so before a long import.
    pub fn unusable(&self) -> usize {
        self.images.iter().filter(|entry| !entry.usable).count()
    }
}

/// Reads one from disk.
pub fn read(path: &Path) -> Result<Manifest> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text).with_context(|| format!("reading {}", path.display()))
}

/// Takes the text apart, without touching the filesystem.
pub fn parse(text: &str) -> Result<Manifest> {
    let root: serde_json::Value = serde_json::from_str(text).context("this is not JSON")?;

    let kind = root.get("kind").and_then(serde_json::Value::as_str);
    if kind != Some(KIND) {
        bail!(
            "not a map selection — its kind is {}, expected {KIND}",
            kind.unwrap_or("missing"),
        );
    }

    // Paths in the file are relative to this, which is what lets the whole
    // collection be moved to another disk and the manifest still resolve.
    let base = root
        .get("root")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .context("the manifest does not say what its paths are relative to")?;

    let listed = root
        .get("images")
        .and_then(serde_json::Value::as_array)
        .context("the manifest lists no images")?;

    let images: Vec<Entry> = listed
        .iter()
        .filter_map(|entry| {
            let relative = entry.get("path")?.as_str()?;
            Some(Entry {
                path: base.join(relative),
                // Absent counts as usable: a manifest that does not judge is
                // not a manifest that rejects everything.
                usable: entry
                    .get("usable")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true),
            })
        })
        .collect();

    if images.is_empty() {
        bail!("the manifest names no pictures");
    }

    Ok(Manifest {
        name: root
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Selection")
            .to_owned(),
        images,
    })
}

/// Reads the real selection and reports what is in it.
///
/// `GEO_REF_MAPPER_MANIFEST=/path/to.json cargo test manifest_survey -- --ignored --nocapture`
#[cfg(test)]
mod real_file {
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_MANIFEST"]
    fn manifest_survey() {
        let Ok(file) = std::env::var("GEO_REF_MAPPER_MANIFEST") else {
            return;
        };
        let manifest = super::read(std::path::Path::new(&file)).expect("it reads");
        let present = manifest
            .images
            .iter()
            .filter(|entry| entry.path.exists())
            .count();
        println!(
            "\n  {}\n  {} named, {} on disk, {} marked unusable",
            manifest.name,
            manifest.images.len(),
            present,
            manifest.unusable(),
        );
        for entry in manifest.images.iter().take(3) {
            println!("    {}", entry.path.display());
        }
        assert_eq!(
            present,
            manifest.images.len(),
            "a selection that names files that are not there cannot be indexed whole",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape mapSelector writes, cut to three entries.
    const SAMPLE: &str = r#"{
      "version": 1,
      "kind": "histomap-manifest",
      "name": "Singapore maps — mapSelector keep set",
      "root": "/media/disk/MAPS-DL",
      "sidecar_suffix": ".histomap.json",
      "count": 3,
      "images": [
        {"path": "1607/a chart 1607 [HC000308_1].tif", "usable": true,  "year": 1607},
        {"path": "1932/plan.tif",                      "usable": false, "year": 1932},
        {"path": "1945/sheet.tif",                     "year": 1945}
      ]
    }"#;

    #[test]
    fn paths_come_out_absolute_against_the_manifests_own_root() {
        // The point of the root: the collection can be moved to another disk
        // and the same manifest still resolves.
        let manifest = parse(SAMPLE).expect("it reads");
        assert_eq!(manifest.images.len(), 3);
        assert_eq!(
            manifest.images[0].path,
            PathBuf::from("/media/disk/MAPS-DL/1607/a chart 1607 [HC000308_1].tif"),
        );
        assert_eq!(manifest.name, "Singapore maps — mapSelector keep set");
    }

    #[test]
    fn a_judgement_of_unusable_is_carried_but_nothing_is_dropped() {
        let manifest = parse(SAMPLE).unwrap();
        assert_eq!(manifest.unusable(), 1);
        assert!(!manifest.images[1].usable);
        // Missing means usable: not judging is not rejecting.
        assert!(manifest.images[2].usable);
    }

    #[test]
    fn some_other_json_is_refused_by_name() {
        let error = parse(r#"{"kind": "something-else", "images": []}"#).unwrap_err();
        assert!(
            error.to_string().contains("histomap-manifest"),
            "the message must say what was expected, got: {error}",
        );
        assert!(parse("{ not json").is_err());
    }

    #[test]
    fn a_manifest_with_nothing_in_it_says_so() {
        let error = parse(r#"{"kind":"histomap-manifest","root":"/x","images":[]}"#).unwrap_err();
        assert!(error.to_string().contains("no pictures"), "{error}");
    }

    #[test]
    fn a_manifest_with_no_root_is_refused_rather_than_guessed_at() {
        // Joining relative paths onto the working directory would index
        // whatever happened to be there.
        let error =
            parse(r#"{"kind":"histomap-manifest","images":[{"path":"a.tif"}]}"#).unwrap_err();
        assert!(error.to_string().contains("relative to"), "{error}");
    }
}
