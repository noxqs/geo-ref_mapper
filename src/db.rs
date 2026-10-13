//! SQLite-backed library: one row per indexed image, plus EXIF and thumbnail
//! tables. Thumbnails live in the database as BLOBs rather than as loose files.
//!
//! The thumbnail and EXIF halves of this API are written and tested but not
//! called yet — background thumbnailing and EXIF extraction are the next pass.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Bumped whenever [`migrate`] gains a new step.
const SCHEMA_VERSION: i64 = 11;

/// File extensions the scanner treats as images.
///
/// Decoding sniffs magic bytes rather than trusting the extension, so aliases
/// the `image` crate does not map by name (`.dib`, `.pic`) still open fine.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "apng", "jpg", "jpeg", "jfif", "gif", "webp", "bmp", "dib", "tif", "tiff", "ico", "tga",
    "dds", "pnm", "pbm", "pgm", "ppm", "pam", "ff", "farbfeld", "qoi", "exr", "hdr", "pic",
    // `.avif` is deliberately absent. The build has no AVIF decoder — the
    // `image` crate answers "The image format Avif is not supported" to every
    // one of the 172 files in the AOMediaCodec conformance set — so indexing
    // them only fills the tree and the Mosaic with pictures that cannot be
    // shown. `formats.rs` lists AVIF as needing a feature, which is the honest
    // place to say so. Put it back here the day the decoder exists.
    // Decoded by `codecs`, not the `image` crate.
    "psd", "psb", "jxl", "svg", "svgz", "xcf", "heic", "heif", "hif",
];

/// Recorded in `thumbnails.format` when a file could not be decoded, stamped
/// with the build that failed so a newer one retries it.
pub fn failure_marker() -> String {
    format!("none@{}", env!("CARGO_PKG_VERSION"))
}

pub fn is_image_extension(extension: &str) -> bool {
    let lowered = extension.to_ascii_lowercase();
    IMAGE_EXTENSIONS.contains(&lowered.as_str())
}

/// `~/.local/share/geo-ref_mapper/library.db` and its platform equivalents.
pub fn default_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "geo-ref_mapper")
        .context("could not determine a data directory")?;
    if let Some(parent) = dirs.data_dir().parent() {
        adopt_legacy_dir(&parent.join("imgviewer"), dirs.data_dir());
    }
    Ok(dirs.data_dir().join("library.db"))
}

/// The project was called imgViewer before it was geo-ref_mapper. A directory
/// left under the old name is moved, not copied, to the new one the first time
/// it is looked for — so the library and the tile cache carry over — and only
/// when nothing exists at the new name yet, so it can never overwrite.
pub fn adopt_legacy_dir(legacy: &Path, current: &Path) {
    if current.exists() || !legacy.is_dir() {
        return;
    }
    match std::fs::rename(legacy, current) {
        Ok(()) => eprintln!("geo-ref_mapper: moved {} to {}", legacy.display(), current.display()),
        Err(error) => eprintln!("geo-ref_mapper: could not move {} — {error}", legacy.display()),
    }
}

/// One row of the `images` table.
#[derive(Clone, Debug, Default)]
pub struct ImageRecord {
    pub id: i64,
    pub path: String,
    pub folder: String,
    pub filename: String,
    pub format: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bytes: u64,
    pub mtime: i64,
    pub has_thumbnail: bool,
    /// Whether the file carries GeoTIFF position tags. `None` means nobody has
    /// looked yet — the distinction matters, because a library indexed before
    /// this column existed has to be filled in rather than assumed to be all
    /// "no".
    pub georeferenced: Option<bool>,
}

/// A marked place on the map.
#[derive(Clone, Debug, PartialEq)]
pub struct Anchor {
    pub id: i64,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// Set when the anchor was made for one picture and belongs to no other.
    /// `None` is the ordinary shared kind.
    pub owner_image_id: Option<i64>,
}

/// An anchor that has been put on a particular picture.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedAnchor {
    pub id: i64,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// Where it sits on the picture, in full-size pixels.
    pub pixel: (f64, f64),
}

/// One picture waiting to be converted, and what became of it.
#[derive(Clone, Debug)]
pub struct Queued {
    pub id: i64,
    pub path: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bytes: u64,
    /// Where the converted file was written, once it has been.
    pub written: Option<String>,
    /// Why it could not be converted.
    pub failure: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LibraryStats {
    pub images: u64,
    pub folders: u64,
    pub thumbnails: u64,
    pub thumbnail_bytes: u64,
    pub indexed_bytes: u64,
    pub db_bytes: u64,
}

pub struct Library {
    conn: Connection,
    pub path: PathBuf,
}

impl Library {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening library at {}", path.display()))?;
        configure(&conn)?;
        migrate(&conn)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&default_path()?)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Batch writes: SQLite is an order of magnitude faster when a scan groups
    /// its inserts instead of paying a transaction per row.
    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Forgets a file that is no longer there, along with its thumbnail and
    /// EXIF rows. The library is an index of what exists; a row pointing at a
    /// deleted file makes the tree lie and the thumbnail sweep retry forever.
    pub fn forget_image(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM images WHERE path = ?1", [path])?;
        Ok(())
    }

    /// One stack as stored: its id, its folder, and its members in cover-first
    /// order.
    pub fn all_stacks(&self) -> Result<Vec<StackRow>> {
        let mut statement = self.conn.prepare(
            "SELECT s.id, s.folder, m.image_id
             FROM stacks s
             JOIN stack_members m ON m.stack_id = s.id
             ORDER BY s.id, m.ordinal",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        let mut out: Vec<StackRow> = Vec::new();
        for row in rows {
            let (id, folder, image) = row?;
            match out.last_mut() {
                Some(last) if last.id == id => last.members.push(image),
                _ => out.push(StackRow {
                    id,
                    folder,
                    members: vec![image],
                }),
            }
        }
        // A stack of one is not a stack. They arise when members are deleted
        // outside geo-ref_mapper, and clearing them here keeps every other rule
        // from having to special-case a group of one.
        let orphans: Vec<i64> = out
            .iter()
            .filter(|stack| stack.members.len() < 2)
            .map(|stack| stack.id)
            .collect();
        for id in orphans {
            let _ = self.conn.execute("DELETE FROM stacks WHERE id = ?1", [id]);
        }
        out.retain(|stack| stack.members.len() >= 2);
        Ok(out)
    }

    /// Groups images into one stack, in the order given — the first is the
    /// cover. Any stacks they already belonged to are absorbed.
    pub fn create_stack(&self, folder: &str, members: &[i64]) -> Result<i64> {
        if members.len() < 2 {
            anyhow::bail!("a stack needs at least two pictures");
        }
        self.conn.execute("BEGIN", [])?;
        let result = (|| -> Result<i64> {
            // Leaving their old stacks first is what lets the unique index do
            // its job without a conflict.
            for image in members {
                self.conn
                    .execute("DELETE FROM stack_members WHERE image_id = ?1", [image])?;
            }
            self.conn.execute(
                "INSERT INTO stacks (folder, created_at) VALUES (?1, unixepoch())",
                [folder],
            )?;
            let id = self.conn.last_insert_rowid();
            for (ordinal, image) in members.iter().enumerate() {
                self.conn.execute(
                    "INSERT INTO stack_members (stack_id, image_id, ordinal) VALUES (?1, ?2, ?3)",
                    params![id, image, ordinal as i64],
                )?;
            }
            // Whatever the absorbing left behind with fewer than two members.
            self.conn.execute(
                "DELETE FROM stacks WHERE id IN (
                     SELECT s.id FROM stacks s
                     LEFT JOIN stack_members m ON m.stack_id = s.id
                     GROUP BY s.id HAVING COUNT(m.image_id) < 2
                 )",
                [],
            )?;
            Ok(id)
        })();
        match result {
            Ok(id) => {
                self.conn.execute("COMMIT", [])?;
                Ok(id)
            }
            Err(error) => {
                let _ = self.conn.execute("ROLLBACK", []);
                Err(error)
            }
        }
    }

    /// Dissolves a stack. The pictures are untouched — they become loose again.
    pub fn unstack(&self, stack: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM stacks WHERE id = ?1", [stack])?;
        Ok(())
    }

    /// Takes pictures out of their stack without deleting them, dissolving the
    /// stack if too little is left of it.
    pub fn remove_from_stack(&self, images: &[i64]) -> Result<()> {
        for image in images {
            self.conn
                .execute("DELETE FROM stack_members WHERE image_id = ?1", [image])?;
        }
        self.conn.execute(
            "DELETE FROM stacks WHERE id IN (
                 SELECT s.id FROM stacks s
                 LEFT JOIN stack_members m ON m.stack_id = s.id
                 GROUP BY s.id HAVING COUNT(m.image_id) < 2
             )",
            [],
        )?;
        Ok(())
    }

    /// Makes one member the cover, by putting it first.
    pub fn set_stack_cover(&self, stack: i64, image: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE stack_members SET ordinal = ordinal + 1 WHERE stack_id = ?1",
            [stack],
        )?;
        self.conn.execute(
            "UPDATE stack_members SET ordinal = 0 WHERE stack_id = ?1 AND image_id = ?2",
            params![stack, image],
        )?;
        Ok(())
    }

    /// Every indexed path under a folder, for checking which still exist.
    pub fn paths_under(&self, folder: &str) -> Result<Vec<String>> {
        let prefix = format!("{}/", folder.trim_end_matches('/'));
        let mut statement = self
            .conn
            .prepare("SELECT path FROM images WHERE path = ?1 OR path LIKE ?2 || '%'")?;
        let rows = statement.query_map(params![folder, prefix], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
    }

    /// Forgets every image under a folder, without touching a single file.
    ///
    /// This is un-indexing, not deleting: the pictures stay exactly where they
    /// are on disk and only leave the library. The separator is appended so
    /// `/photos/trip` cannot also match `/photos/trip-2024`.
    pub fn forget_folder(&self, folder: &str) -> Result<usize> {
        let prefix = format!("{}/", folder.trim_end_matches('/'));
        let removed = self.conn.execute(
            "DELETE FROM images WHERE path = ?1 OR path LIKE ?2 || '%'",
            params![folder, prefix],
        )?;
        Ok(removed)
    }

    /// How much of the file is space nothing is using, in bytes.
    ///
    /// Deleting rows never shrinks a SQLite file — the pages go on a free list
    /// and are handed back out as it grows again. That is the right default:
    /// re-scanning a folder just re-uses them. It only becomes worth reclaiming
    /// after a library gets permanently smaller.
    pub fn free_bytes(&self) -> Result<u64> {
        // SQLite hands these back as signed, which is what `rusqlite` will
        // convert; negative is impossible, so the clamp is only about types.
        let page: i64 = self
            .conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))?;
        let free: i64 = self
            .conn
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        Ok(page.max(0) as u64 * free.max(0) as u64)
    }

    /// Rebuilds the file without the free space, and returns what it saved.
    ///
    /// This is `VACUUM`, which writes a fresh copy alongside and swaps it in —
    /// so it briefly needs room for a second copy, and it is not quick on a
    /// large library. A button rather than something done at startup: rewriting
    /// a gigabyte before the window opens would be a strange way to begin.
    pub fn compact(&self) -> Result<u64> {
        let before = self.size_bytes();
        self.conn.execute_batch("VACUUM")?;
        Ok(before.saturating_sub(self.size_bytes()))
    }

    fn size_bytes(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    /// Replaces a picture's tags with the ones given.
    ///
    /// Replaces rather than adds: a sidecar is rewritten as mapSelector learns
    /// more, and merging would leave a tag it has since withdrawn.
    pub fn set_tags(&self, image_id: i64, tags: &[String]) -> Result<()> {
        self.conn.execute(
            "DELETE FROM image_tags WHERE image_id = ?1",
            params![image_id],
        )?;
        for tag in tags {
            self.conn.execute(
                "INSERT OR IGNORE INTO image_tags (image_id, tag) VALUES (?1, ?2)",
                params![image_id, tag],
            )?;
        }
        Ok(())
    }

    /// Every tag in the library and how many pictures carry it, commonest
    /// first — which is the order they are useful in when narrowing a set.
    pub fn tags_with_counts(&self) -> Result<Vec<(String, usize)>> {
        let mut statement = self.conn.prepare(
            "SELECT tag, count(*) FROM image_tags
             GROUP BY tag ORDER BY count(*) DESC, tag",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The paths of every picture carrying a tag.
    ///
    /// Paths rather than ids because that is what the tree marks things by, and
    /// a set the user is building has to survive the library being rebuilt.
    pub fn paths_with_tag(&self, tag: &str) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "SELECT i.path FROM image_tags t JOIN images i ON i.id = t.image_id
             WHERE t.tag = ?1",
        )?;
        let rows = statement.query_map(params![tag], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every picture carrying all of `include` and none of `exclude`.
    ///
    /// Worked out in one query rather than by intersecting lists in the app:
    /// the answer is wanted the instant a tag is turned on, over a library of
    /// tens of thousands, and this is what a database is for.
    ///
    /// An empty `include` means "no requirement", not "nothing" — a filter that
    /// only excludes is a perfectly ordinary thing to want.
    pub fn paths_matching_tags(
        &self,
        include: &[String],
        exclude: &[String],
    ) -> Result<Vec<String>> {
        let holes = |n: usize| -> String { vec!["?"; n].join(",") };
        let mut sql = String::from("SELECT i.path FROM images i WHERE 1=1");
        if !include.is_empty() {
            // Counted rather than joined once per tag: a picture must carry
            // *all* of them, and a join would return it once per tag it has.
            sql.push_str(&format!(
                " AND (SELECT count(DISTINCT t.tag) FROM image_tags t
                       WHERE t.image_id = i.id AND t.tag IN ({})) = {}",
                holes(include.len()),
                include.len(),
            ));
        }
        if !exclude.is_empty() {
            sql.push_str(&format!(
                " AND NOT EXISTS (SELECT 1 FROM image_tags x
                                  WHERE x.image_id = i.id AND x.tag IN ({}))",
                holes(exclude.len()),
            ));
        }
        sql.push_str(" ORDER BY i.path");

        let bound: Vec<&String> = include.iter().chain(exclude.iter()).collect();
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(bound), |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Records where a placed map sits and when it was drawn.
    pub fn set_extent(
        &self,
        image_id: i64,
        year: Option<i32>,
        bounds: (f64, f64, f64, f64),
    ) -> Result<()> {
        let (west, south, east, north) = bounds;
        self.conn.execute(
            "INSERT INTO image_extent (image_id, year, west, south, east, north)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(image_id) DO UPDATE SET
                 year = excluded.year, west = excluded.west, south = excluded.south,
                 east = excluded.east, north = excluded.north",
            params![image_id, year, west, south, east, north],
        )?;
        Ok(())
    }

    /// The year and path of every placed map overlapping a view.
    ///
    /// Overlap, not containment: a sheet covering half the window is still
    /// coverage of what you are looking at, and asking for maps wholly inside a
    /// view would hide exactly the large-area ones most worth knowing about.
    pub fn maps_overlapping(
        &self,
        west: f64,
        south: f64,
        east: f64,
        north: f64,
    ) -> Result<Vec<(Option<i32>, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT e.year, i.path FROM image_extent e JOIN images i ON i.id = e.image_id
             WHERE e.west <= ?3 AND e.east >= ?1 AND e.south <= ?4 AND e.north >= ?2
             ORDER BY e.year, i.path",
        )?;
        let rows = statement.query_map(params![west, south, east, north], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Insert or refresh a row, returning its id. Rows are matched on `path`.
    pub fn upsert_image(&self, record: &ImageRecord) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO images (path, folder, filename, format, width, height, bytes, mtime, georeferenced, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, unixepoch())
             ON CONFLICT(path) DO UPDATE SET
                 folder     = excluded.folder,
                 filename   = excluded.filename,
                 format     = COALESCE(excluded.format, images.format),
                 width      = COALESCE(excluded.width, images.width),
                 height     = COALESCE(excluded.height, images.height),
                 bytes      = excluded.bytes,
                 mtime      = excluded.mtime,
                 -- A caller that did not look must not erase what a caller who
                 -- did looked up.
                 georeferenced = COALESCE(excluded.georeferenced, images.georeferenced),
                 indexed_at = unixepoch()",
            params![
                record.path,
                record.folder,
                record.filename,
                record.format,
                record.width,
                record.height,
                record.bytes as i64,
                record.mtime,
                record.georeferenced,
            ],
        )?;

        let id = self
            .conn
            .query_row(
                "SELECT id FROM images WHERE path = ?1",
                params![record.path],
                |row| row.get(0),
            )
            .context("reading back the image id")?;
        Ok(id)
    }

    /// The id of an already-indexed picture, by path.
    pub fn id_of_path(&self, path: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM images WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// True when the stored row already matches this size and mtime, meaning
    /// the file has not changed since the last scan.
    pub fn is_current(&self, path: &str, bytes: u64, mtime: i64) -> Result<bool> {
        let stored: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT bytes, mtime FROM images WHERE path = ?1",
                params![path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(stored == Some((bytes as i64, mtime)))
    }

    // ------------------------------------------------------------- settings

    /// Reads a persisted setting, or `None` if it was never written.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        let value = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn image(&self, id: i64) -> Result<Option<ImageRecord>> {
        let record = self
            .conn
            .query_row(
                "SELECT id, path, folder, filename, format, width, height, bytes, mtime,
                        georeferenced
                 FROM images WHERE id = ?1",
                params![id],
                |row| {
                    Ok(ImageRecord {
                        id: row.get(0)?,
                        path: row.get(1)?,
                        folder: row.get(2)?,
                        filename: row.get(3)?,
                        format: row.get(4)?,
                        width: row.get(5)?,
                        height: row.get(6)?,
                        bytes: row.get::<_, i64>(7)? as u64,
                        mtime: row.get(8)?,
                        has_thumbnail: false,
                        georeferenced: row.get(9)?,
                    })
                },
            )
            .optional()?;
        Ok(record)
    }

    /// Files indexed before anyone thought to ask whether they are
    /// georeferenced.
    ///
    /// Only TIFFs are returned: nothing else can carry the tags, so marking the
    /// rest answered costs one statement and saves opening 21,000 files that
    /// were never going to say yes.
    pub fn unexamined_for_georeference(&self, limit: usize) -> Result<Vec<(i64, String)>> {
        self.conn.execute(
            "UPDATE images SET georeferenced = 0
             WHERE georeferenced IS NULL
               AND LOWER(filename) NOT LIKE '%.tif'
               AND LOWER(filename) NOT LIKE '%.tiff'",
            [],
        )?;
        let mut statement = self
            .conn
            .prepare("SELECT id, path FROM images WHERE georeferenced IS NULL LIMIT ?1")?;
        let rows =
            statement.query_map(params![limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every picture the library believes carries a position, with its id.
    pub fn georeferenced_images(&self) -> Result<Vec<(i64, String)>> {
        let mut statement = self
            .conn
            .prepare("SELECT id, path FROM images WHERE georeferenced = 1 ORDER BY path")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Adds pictures to the conversion queue, ignoring ones already on it.
    ///
    /// Returns how many were actually added, so the message can say "3 added"
    /// rather than claiming 5 when 2 were already waiting.
    pub fn queue_for_conversion(&self, ids: &[i64]) -> Result<usize> {
        let mut added = 0;
        for id in ids {
            added += self.conn.execute(
                "INSERT OR IGNORE INTO map_queue (image_id, added_at) VALUES (?1, unixepoch())",
                params![id],
            )?;
        }
        Ok(added)
    }

    /// Every indexed picture in a folder and in everything beneath it.
    ///
    /// Matched on the separator rather than with `LIKE`: a folder path is
    /// allowed to contain `%` and `_`, which `LIKE` would read as wildcards, and
    /// a prefix test without the trailing slash would make `MAPS` swallow
    /// `MAPS-NEW`. Both mistakes queue hundreds of files nobody asked for.
    pub fn image_ids_under(&self, folder: &str) -> Result<Vec<i64>> {
        // A trailing slash of its own would ask for `//` below.
        let root = folder.trim_end_matches('/');
        let mut statement = self.conn.prepare(
            "SELECT id FROM images
             WHERE folder = ?1 OR substr(folder, 1, length(?1) + 1) = ?1 || '/'
             ORDER BY path",
        )?;
        let rows = statement.query_map(params![root], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<i64>>>()
            .map_err(Into::into)
    }

    pub fn unqueue_for_conversion(&self, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.conn
                .execute("DELETE FROM map_queue WHERE image_id = ?1", params![id])?;
        }
        Ok(())
    }

    pub fn clear_conversion_queue(&self) -> Result<()> {
        self.conn.execute("DELETE FROM map_queue", [])?;
        Ok(())
    }

    /// Everything on the queue, oldest first, with what happened to it.
    pub fn conversion_queue(&self) -> Result<Vec<Queued>> {
        let mut statement = self.conn.prepare(
            "SELECT q.image_id, i.path, i.width, i.height, i.bytes, q.written, q.failure
             FROM map_queue q JOIN images i ON i.id = q.image_id
             ORDER BY q.added_at, i.path",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(Queued {
                id: row.get(0)?,
                path: row.get(1)?,
                width: row.get(2)?,
                height: row.get(3)?,
                bytes: row.get::<_, i64>(4)? as u64,
                written: row.get(5)?,
                failure: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Marks a place, returning the new pin's id.
    pub fn add_anchor(&self, name: &str, lat: f64, lon: f64) -> Result<i64> {
        self.add_anchor_owned(name, lat, lon, None)
    }

    /// The same, but belonging to one picture when an owner is given.
    ///
    /// An owned anchor is listed only while that picture is being placed, and
    /// goes when the picture leaves the library.
    pub fn add_anchor_owned(
        &self,
        name: &str,
        lat: f64,
        lon: f64,
        owner: Option<i64>,
    ) -> Result<i64> {
        // Stored as a path: the row an image holds is a detail of this
        // library, and rebuilding it must not take the anchor with it.
        self.conn.execute(
            "INSERT INTO anchors (name, lat, lon, created_at, owner_path)
             VALUES (?1, ?2, ?3, unixepoch(), (SELECT path FROM images WHERE id = ?4))",
            params![name, lat, lon, owner],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rename_anchor(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE anchors SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    pub fn move_anchor(&self, id: i64, lat: f64, lon: f64) -> Result<()> {
        self.conn.execute(
            "UPDATE anchors SET lat = ?2, lon = ?3 WHERE id = ?1",
            params![id, lat, lon],
        )?;
        Ok(())
    }

    pub fn delete_anchor(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM anchors WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// The shared pins, oldest first — the order they were made, which is the
    /// order they are thought of in.
    pub fn all_anchors(&self) -> Result<Vec<Anchor>> {
        self.anchors_for_image(None)
    }

    /// The shared pins, plus the ones belonging to one picture.
    ///
    /// Never another picture's: an anchor made for a particular scan is
    /// meaningless on the rest, and offering it would be offering a place that
    /// cannot be found.
    pub fn anchors_for_image(&self, image_id: Option<i64>) -> Result<Vec<Anchor>> {
        // The owner comes back as the id it currently has, so everything above
        // this layer goes on working in ids. The path is what is kept.
        let mut statement = self.conn.prepare(
            "SELECT a.id, a.name, a.lat, a.lon,
                    (SELECT i.id FROM images i WHERE i.path = a.owner_path)
             FROM anchors a
             WHERE a.owner_path IS NULL
                OR a.owner_path = (SELECT i.path FROM images i WHERE i.id = ?1)
             ORDER BY a.created_at, a.id",
        )?;
        let rows = statement.query_map(params![image_id], |row| {
            Ok(Anchor {
                id: row.get(0)?,
                name: row.get(1)?,
                lat: row.get(2)?,
                lon: row.get(3)?,
                owner_image_id: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Where an anchor falls on one picture, in that picture's full-size
    /// pixels. Setting it again moves it.
    pub fn place_anchor_on_image(
        &self,
        image_id: i64,
        anchor_id: i64,
        x: f64,
        y: f64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO image_anchors (image_path, anchor_id, pixel_x, pixel_y)
             VALUES ((SELECT path FROM images WHERE id = ?1), ?2, ?3, ?4)
             ON CONFLICT(image_path, anchor_id) DO UPDATE SET pixel_x = ?3, pixel_y = ?4",
            params![image_id, anchor_id, x, y],
        )?;
        Ok(())
    }

    pub fn clear_anchors_on_image(&self, image_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM image_anchors
             WHERE image_path = (SELECT path FROM images WHERE id = ?1)",
            params![image_id],
        )?;
        Ok(())
    }

    pub fn unplace_anchor_on_image(&self, image_id: i64, anchor_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM image_anchors
             WHERE image_path = (SELECT path FROM images WHERE id = ?1)
               AND anchor_id = ?2",
            params![image_id, anchor_id],
        )?;
        Ok(())
    }

    /// Every anchor placed on one picture, with where it is on the earth and
    /// where it is on the picture.
    pub fn anchors_on_image(&self, image_id: i64) -> Result<Vec<PlacedAnchor>> {
        let mut statement = self.conn.prepare(
            "SELECT a.id, a.name, a.lat, a.lon, p.pixel_x, p.pixel_y
             FROM image_anchors p JOIN anchors a ON a.id = p.anchor_id
             WHERE p.image_path = (SELECT path FROM images WHERE id = ?1)
             ORDER BY a.created_at, a.id",
        )?;
        let rows = statement.query_map(params![image_id], |row| {
            Ok(PlacedAnchor {
                id: row.get(0)?,
                name: row.get(1)?,
                lat: row.get(2)?,
                lon: row.get(3)?,
                pixel: (row.get(4)?, row.get(5)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Records that a map was converted, and where the copy went.
    pub fn mark_converted(&self, id: i64, written: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE map_queue SET written = ?2, failure = NULL WHERE image_id = ?1",
            params![id, written],
        )?;
        Ok(())
    }

    pub fn mark_conversion_failed(&self, id: i64, failure: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE map_queue SET failure = ?2, written = NULL WHERE image_id = ?1",
            params![id, failure],
        )?;
        Ok(())
    }

    /// The ids currently on the queue, for marking them in the grid.
    pub fn queued_ids(&self) -> Result<std::collections::HashSet<i64>> {
        let mut statement = self.conn.prepare("SELECT image_id FROM map_queue")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records what looking at the file found.
    pub fn set_georeferenced(&self, id: i64, georeferenced: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE images SET georeferenced = ?2 WHERE id = ?1",
            params![id, georeferenced],
        )?;
        Ok(())
    }

    pub fn all_images(&self) -> Result<Vec<ImageRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT i.id, i.path, i.folder, i.filename, i.format, i.width, i.height, i.bytes, i.mtime,
                    EXISTS(SELECT 1 FROM thumbnails t WHERE t.image_id = i.id),
                    i.georeferenced
             FROM images i
             ORDER BY i.folder, i.filename",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ImageRecord {
                id: row.get(0)?,
                path: row.get(1)?,
                folder: row.get(2)?,
                filename: row.get(3)?,
                format: row.get(4)?,
                width: row.get(5)?,
                height: row.get(6)?,
                bytes: row.get::<_, i64>(7)? as u64,
                mtime: row.get(8)?,
                has_thumbnail: row.get(9)?,
                georeferenced: row.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn stats(&self) -> Result<LibraryStats> {
        let (images, indexed_bytes): (u64, u64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM images",
            [],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )?;
        let folders: u64 =
            self.conn
                .query_row("SELECT COUNT(DISTINCT folder) FROM images", [], |row| {
                    Ok(row.get::<_, i64>(0)? as u64)
                })?;
        let (thumbnails, thumbnail_bytes): (u64, u64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(data)), 0) FROM thumbnails",
            [],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )?;

        // Page count × page size covers the WAL-checkpointed main file.
        let page_count: i64 = self
            .conn
            .query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let page_size: i64 = self
            .conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))?;

        Ok(LibraryStats {
            images,
            folders,
            thumbnails,
            thumbnail_bytes,
            indexed_bytes,
            db_bytes: (page_count * page_size) as u64,
        })
    }

    // ------------------------------------------------------------ thumbnails

    /// Store an encoded thumbnail. `max_edge` is the size bucket it was made
    /// for, so several resolutions can coexist for one image.
    pub fn put_thumbnail(
        &self,
        image_id: i64,
        max_edge: u32,
        width: u32,
        height: u32,
        format: &str,
        data: &[u8],
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO thumbnails (image_id, max_edge, width, height, format, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch())
             ON CONFLICT(image_id, max_edge) DO UPDATE SET
                 width = excluded.width,
                 height = excluded.height,
                 format = excluded.format,
                 data = excluded.data,
                 created_at = unixepoch()",
            params![image_id, max_edge, width, height, format, data],
        )?;
        Ok(())
    }

    pub fn thumbnail(&self, image_id: i64, max_edge: u32) -> Result<Option<Vec<u8>>> {
        let blob = self
            .conn
            .query_row(
                "SELECT data FROM thumbnails WHERE image_id = ?1 AND max_edge = ?2",
                params![image_id, max_edge],
                |row| row.get(0),
            )
            .optional()?;
        Ok(blob)
    }

    /// How many images still need a thumbnail at this size.
    pub fn count_without_thumbnail(&self, max_edge: u32) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM images i
             WHERE NOT EXISTS (
                 SELECT 1 FROM thumbnails t WHERE t.image_id = i.id AND t.max_edge = ?1
             )",
            params![max_edge],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Images that still need a thumbnail at this size.
    pub fn images_without_thumbnail(&self, max_edge: u32, limit: u32) -> Result<Vec<ImageRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT i.id, i.path, i.folder, i.filename, i.format, i.width, i.height, i.bytes, i.mtime
             FROM images i
             WHERE NOT EXISTS (
                 SELECT 1 FROM thumbnails t
                 WHERE t.image_id = i.id AND t.max_edge = ?1
                   -- Covered by a real thumbnail, or by a failure recorded by
                   -- *this* build. A `none` row stops the sweep retrying a file
                   -- it cannot decode, but it is stamped with the version that
                   -- gave up so a later build tries again — otherwise every
                   -- decoder we add leaves behind a population of files stuck
                   -- showing a placeholder colour for ever.
                   AND (t.format NOT LIKE 'none%' OR t.format = ?3)
             )
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![max_edge, limit, failure_marker()], |row| {
            Ok(ImageRecord {
                id: row.get(0)?,
                path: row.get(1)?,
                folder: row.get(2)?,
                filename: row.get(3)?,
                format: row.get(4)?,
                width: row.get(5)?,
                height: row.get(6)?,
                bytes: row.get::<_, i64>(7)? as u64,
                mtime: row.get(8)?,
                has_thumbnail: false,
                georeferenced: None,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ------------------------------------------------------------------ exif

    pub fn put_exif(&self, image_id: i64, exif: &ExifRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO exif (image_id, camera, lens, focal_length, aperture, shutter, iso,
                               taken_at, orientation, gps_lat, gps_lon, color_space)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(image_id) DO UPDATE SET
                 camera = excluded.camera, lens = excluded.lens,
                 focal_length = excluded.focal_length, aperture = excluded.aperture,
                 shutter = excluded.shutter, iso = excluded.iso,
                 taken_at = excluded.taken_at, orientation = excluded.orientation,
                 gps_lat = excluded.gps_lat, gps_lon = excluded.gps_lon,
                 color_space = excluded.color_space",
            params![
                image_id,
                exif.camera,
                exif.lens,
                exif.focal_length,
                exif.aperture,
                exif.shutter,
                exif.iso,
                exif.taken_at,
                exif.orientation,
                exif.gps_lat,
                exif.gps_lon,
                exif.color_space,
            ],
        )?;
        Ok(())
    }

    pub fn exif(&self, image_id: i64) -> Result<Option<ExifRecord>> {
        let record = self
            .conn
            .query_row(
                "SELECT camera, lens, focal_length, aperture, shutter, iso, taken_at,
                        orientation, gps_lat, gps_lon, color_space
                 FROM exif WHERE image_id = ?1",
                params![image_id],
                |row| {
                    Ok(ExifRecord {
                        camera: row.get(0)?,
                        lens: row.get(1)?,
                        focal_length: row.get(2)?,
                        aperture: row.get(3)?,
                        shutter: row.get(4)?,
                        iso: row.get(5)?,
                        taken_at: row.get(6)?,
                        orientation: row.get(7)?,
                        gps_lat: row.get(8)?,
                        gps_lon: row.get(9)?,
                        color_space: row.get(10)?,
                    })
                },
            )
            .optional()?;
        Ok(record)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExifRecord {
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub focal_length: Option<f64>,
    pub aperture: Option<f64>,
    pub shutter: Option<String>,
    pub iso: Option<i64>,
    pub taken_at: Option<i64>,
    pub orientation: Option<i64>,
    pub gps_lat: Option<f64>,
    pub gps_lon: Option<f64>,
    pub color_space: Option<String>,
}

/// One stack as the library stores it.
#[derive(Clone, PartialEq, Debug)]
pub struct StackRow {
    pub id: i64,
    pub folder: String,
    /// Cover first.
    pub members: Vec<i64>,
}

/// Pragmas that make SQLite fast enough to sit in front of a file browser.
pub fn configure(conn: &Connection) -> Result<()> {
    // `journal_mode` returns the new mode, so it needs a query rather than execute.
    conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -32000;   -- 32 MB page cache
         PRAGMA mmap_size = 268435456; -- 256 MB
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }

    if version < 2 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
    }

    conn.execute_batch(
        "BEGIN;

         CREATE TABLE IF NOT EXISTS images (
             id         INTEGER PRIMARY KEY,
             path       TEXT    NOT NULL UNIQUE,
             folder     TEXT    NOT NULL,
             filename   TEXT    NOT NULL,
             format     TEXT,
             width      INTEGER,
             height     INTEGER,
             bytes      INTEGER NOT NULL,
             mtime      INTEGER NOT NULL,
             indexed_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS images_folder_idx   ON images(folder);
         CREATE INDEX IF NOT EXISTS images_filename_idx ON images(filename);

         CREATE TABLE IF NOT EXISTS exif (
             image_id     INTEGER PRIMARY KEY REFERENCES images(id) ON DELETE CASCADE,
             camera       TEXT,
             lens         TEXT,
             focal_length REAL,
             aperture     REAL,
             shutter      TEXT,
             iso          INTEGER,
             taken_at     INTEGER,
             orientation  INTEGER,
             gps_lat      REAL,
             gps_lon      REAL,
             color_space  TEXT
         );

         CREATE TABLE IF NOT EXISTS thumbnails (
             image_id   INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
             max_edge   INTEGER NOT NULL,
             width      INTEGER NOT NULL,
             height     INTEGER NOT NULL,
             format     TEXT    NOT NULL,
             data       BLOB    NOT NULL,
             created_at INTEGER NOT NULL,
             PRIMARY KEY (image_id, max_edge)
         );

         PRAGMA user_version = 2;
         COMMIT;",
    )?;

    if version < 3 {
        // Stacks: a group of pictures shown as one tile, the way Lightroom
        // does it. A stack belongs to one folder, and an image belongs to at
        // most one stack — both enforced here rather than only in the UI.
        //
        // There is no cover column. The cover is the member with the lowest
        // `ordinal`, so deleting the cover's row promotes the next one instead
        // of leaving the stack pointing at a picture that is gone. Setting a
        // cover is a reordering, not a foreign key change.
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS stacks (
                 id         INTEGER PRIMARY KEY,
                 folder     TEXT    NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS stack_members (
                 stack_id INTEGER NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
                 image_id INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
                 ordinal  INTEGER NOT NULL,
                 PRIMARY KEY (stack_id, image_id)
             );
             -- One stack per image, enforced by the database rather than hoped
             -- for by the code that writes it.
             CREATE UNIQUE INDEX IF NOT EXISTS stack_members_image_idx
                 ON stack_members(image_id);
             PRAGMA user_version = 3;
             COMMIT;",
        )?;
    }

    if version < 4 {
        // Whether a picture says where on the earth it is. Nullable on purpose:
        // a library indexed before this column existed has never been asked,
        // and "not looked at yet" is a different thing from "looked, and it
        // does not" — the first is worth going back for, the second is not.
        //
        // Outside the batch above and tolerant of failure: on a database that
        // already carries the column, adding it again is an error and not a
        // problem.
        let _ = conn.execute("ALTER TABLE images ADD COLUMN georeferenced INTEGER", []);
        conn.execute_batch("PRAGMA user_version = 4;")?;
    }

    if version < 5 {
        // Maps waiting to be converted into a form that zooms quickly.
        //
        // A queue rather than a flag on the image, because it is a list with an
        // order that the user builds up over time and works through — and
        // because a map can be queued again later when the conversion settings
        // change.
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS map_queue (
                 image_id INTEGER PRIMARY KEY REFERENCES images(id) ON DELETE CASCADE,
                 added_at INTEGER NOT NULL,
                 -- Where the converted file went, once it has been made.
                 written   TEXT,
                 -- Why it could not be made, if it could not.
                 failure   TEXT
             );
             PRAGMA user_version = 5;
             COMMIT;",
        )?;
    }

    if version < 6 {
        // Places worth marking on the map. Not tied to any picture: a pin is a
        // spot on the earth, and the pictures that show it come and go.
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS pins (
                 id         INTEGER PRIMARY KEY,
                 name       TEXT    NOT NULL,
                 lat        REAL    NOT NULL,
                 lon        REAL    NOT NULL,
                 created_at INTEGER NOT NULL
             );
             PRAGMA user_version = 6;
             COMMIT;",
        )?;
    }

    if version < 7 {
        // "Pin" and "anchor" were two words for the same thing, which is one
        // too many. The table is renamed rather than left as it was, so the
        // code and the interface say the same word.
        //
        // And the other half: where each anchor falls on a particular picture.
        // A place on the earth is one thing; where it lands on a scan is
        // another, and a scan can carry several.
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE pins RENAME TO anchors;
             CREATE TABLE IF NOT EXISTS image_anchors (
                 image_id  INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
                 anchor_id INTEGER NOT NULL REFERENCES anchors(id) ON DELETE CASCADE,
                 pixel_x   REAL    NOT NULL,
                 pixel_y   REAL    NOT NULL,
                 PRIMARY KEY (image_id, anchor_id)
             );
             PRAGMA user_version = 7;
             COMMIT;",
        )?;
    }

    if version < 8 {
        // An anchor made for one picture and no other.
        //
        // Most anchors are places — a jetty, a hill — worth marking once and
        // reusing on every map that shows them. But a scan often needs a spot
        // that is only meaningful on *it*: a corner of the sheet, a printed
        // cross, a building that has since gone. Adding those to the shared set
        // makes the list longer for every other picture and says a place exists
        // that nobody else can use.
        //
        // A nullable owner rather than a table of its own: everything else
        // about them — naming, moving, placing, solving — is identical, and a
        // second table would mean saying all of it twice. NULL is the ordinary
        // shared kind, which is what every existing row becomes.
        let _ = conn.execute(
            "ALTER TABLE anchors ADD COLUMN owner_image_id INTEGER
                 REFERENCES images(id) ON DELETE CASCADE",
            [],
        );
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS anchors_owner_idx ON anchors(owner_image_id);
             PRAGMA user_version = 8;",
        )?;
    }

    if version < 9 {
        // The tags a picture's sidecar carries, copied in as it is indexed.
        //
        // In the library rather than read from the sidecars on demand: picking
        // a set out of eight thousand maps by tag has to answer instantly, and
        // opening eight thousand small JSON files to do it would not.
        //
        // A row per tag rather than a list in a column, because every question
        // asked of them is "which pictures have this one".
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS image_tags (
                 image_id INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
                 tag      TEXT    NOT NULL,
                 PRIMARY KEY (image_id, tag)
             );
             CREATE INDEX IF NOT EXISTS image_tags_tag_idx ON image_tags(tag);
             PRAGMA user_version = 9;
             COMMIT;",
        )?;
    }

    if version < 10 {
        // Where a placed map sits on the earth, and when it was drawn.
        //
        // Both are already knowable — the footprint from the file's GeoTIFF
        // tags, the year from its sidecar or its own metadata — but only by
        // opening it. The timeline asks "what covers *this* view, by year" on
        // every frame of a pan, so the answer has to be a query rather than
        // twenty thousand file opens.
        //
        // Only georeferenced maps have a row: a picture that does not say where
        // it is cannot be said to cover anything.
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS image_extent (
                 image_id INTEGER PRIMARY KEY REFERENCES images(id) ON DELETE CASCADE,
                 year     INTEGER,
                 west     REAL NOT NULL,
                 south    REAL NOT NULL,
                 east     REAL NOT NULL,
                 north    REAL NOT NULL
             );
             CREATE INDEX IF NOT EXISTS image_extent_year_idx ON image_extent(year);
             PRAGMA user_version = 10;
             COMMIT;",
        )?;
    }

    if version < 11 {
        // Anchor work keyed by **path**, not by which row the picture happens
        // to hold in this library.
        //
        // Both tables hung off `images(id)` with `ON DELETE CASCADE`, so taking
        // a folder out of the tree — an operation the interface calls
        // reversible, and which touches nothing on disk — silently destroyed
        // every pin-to-pixel pairing under it, and every anchor made for one of
        // those pictures. It has now cost two lots of work, and no amount of
        // care at the call sites can fix a rule the database applies on its
        // own.
        //
        // A path outlives the row. Re-indexing the same picture finds its
        // pairings waiting, which is what "reversible" has to mean — and it is
        // the same way marks already survive a rebuild.
        //
        // The cascade from `anchors` is kept: deleting an anchor should take
        // its pairings, because that is a deliberate act on the anchor itself
        // rather than a side effect of tidying the tree.
        //
        // Foreign keys are off for the rebuild — dropping a table other rows
        // point at is exactly the case the pragma exists to allow — and the
        // whole thing is checked before it is kept.
        conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
        let rebuilt = conn.execute_batch(
            "BEGIN;
             CREATE TABLE image_anchors_by_path (
                 image_path TEXT    NOT NULL,
                 anchor_id  INTEGER NOT NULL REFERENCES anchors(id) ON DELETE CASCADE,
                 pixel_x    REAL    NOT NULL,
                 pixel_y    REAL    NOT NULL,
                 PRIMARY KEY (image_path, anchor_id)
             );
             INSERT OR IGNORE INTO image_anchors_by_path (image_path, anchor_id, pixel_x, pixel_y)
                 SELECT i.path, p.anchor_id, p.pixel_x, p.pixel_y
                 FROM image_anchors p JOIN images i ON i.id = p.image_id;
             DROP TABLE image_anchors;
             ALTER TABLE image_anchors_by_path RENAME TO image_anchors;

             CREATE TABLE anchors_by_path (
                 id         INTEGER PRIMARY KEY,
                 name       TEXT    NOT NULL,
                 lat        REAL    NOT NULL,
                 lon        REAL    NOT NULL,
                 created_at INTEGER NOT NULL,
                 owner_path TEXT
             );
             INSERT INTO anchors_by_path (id, name, lat, lon, created_at, owner_path)
                 SELECT a.id, a.name, a.lat, a.lon, a.created_at,
                        (SELECT i.path FROM images i WHERE i.id = a.owner_image_id)
                 FROM anchors a;
             DROP TABLE anchors;
             ALTER TABLE anchors_by_path RENAME TO anchors;

             CREATE INDEX IF NOT EXISTS anchors_owner_idx ON anchors(owner_path);
             CREATE INDEX IF NOT EXISTS image_anchors_path_idx ON image_anchors(image_path);
             PRAGMA user_version = 11;
             COMMIT;",
        );
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        rebuilt?;
    }

    Ok(())
}

#[cfg(test)]
mod anchors {
    use super::*;

    #[test]
    fn an_anchor_survives_being_named_moved_and_read_back() {
        let library = super::tests::memory_library();
        let id = library.add_anchor("Pin 1", 1.2905, 103.8510).unwrap();

        library.rename_anchor(id, "Fort Canning").unwrap();
        library.move_anchor(id, 1.2931, 103.8465).unwrap();

        let held = library.all_anchors().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].name, "Fort Canning");
        assert!((held[0].lat - 1.2931).abs() < 1e-9);
        assert!((held[0].lon - 103.8465).abs() < 1e-9);
    }

    #[test]
    fn taking_a_folder_out_of_the_tree_does_not_destroy_the_anchor_work() {
        // This has cost real work twice. Both tables hung off `images(id)` with
        // `ON DELETE CASCADE`, so an operation the interface calls reversible —
        // and which touches nothing on disk — silently deleted every
        // pin-to-pixel pairing under the folder, and every anchor made for one
        // of those pictures. The green "georeferenced" badge stayed, because
        // that is a flag on the row and the position is in the file, so the
        // library looked right and the maps opened with nothing on them.
        let library = super::tests::memory_library();
        let image = library
            .upsert_image(&super::tests::record("/maps/1840/harbour.tif"))
            .unwrap();
        let shared = library
            .add_anchor("Fort Canning", 1.2931, 103.8465)
            .unwrap();
        let its_own = library
            .add_anchor_owned("sheet corner", 1.30, 103.80, Some(image))
            .unwrap();
        library
            .place_anchor_on_image(image, shared, 1200.0, 800.0)
            .unwrap();
        library
            .place_anchor_on_image(image, its_own, 40.0, 40.0)
            .unwrap();
        assert_eq!(library.anchors_on_image(image).unwrap().len(), 2);

        library.forget_folder("/maps").unwrap();
        assert_eq!(
            library.all_anchors().unwrap().len(),
            1,
            "the shared pin stays"
        );

        // Indexed again — the same picture, a new row id — and the work is
        // waiting where it was left.
        let again = library
            .upsert_image(&super::tests::record("/maps/1840/harbour.tif"))
            .unwrap();
        let back = library.anchors_on_image(again).unwrap();
        assert_eq!(back.len(), 2, "both pairings survived: {back:?}");
        assert!(
            back.iter().any(|a| a.pixel == (1200.0, 800.0)),
            "and at the pixels they were put on"
        );
        // Including the one made for this picture alone, which used to be
        // deleted outright rather than merely orphaned.
        assert!(back.iter().any(|a| a.name == "sheet corner"));
        assert!(
            library
                .anchors_for_image(Some(again))
                .unwrap()
                .iter()
                .any(|a| a.owner_image_id == Some(again)),
            "and it belongs to the picture's new row"
        );
    }

    #[test]
    fn deleting_an_anchor_still_takes_its_pairings() {
        // The other cascade is kept: that one is a deliberate act on the anchor
        // itself, not a side effect of tidying the tree.
        let library = super::tests::memory_library();
        let image = library
            .upsert_image(&super::tests::record("/maps/1840/harbour.tif"))
            .unwrap();
        let anchor = library.add_anchor("Fort Canning", 1.29, 103.84).unwrap();
        library
            .place_anchor_on_image(image, anchor, 10.0, 20.0)
            .unwrap();

        library.delete_anchor(anchor).unwrap();
        assert!(library.anchors_on_image(image).unwrap().is_empty());
    }

    #[test]
    fn anchors_come_back_in_the_order_they_were_made() {
        let library = super::tests::memory_library();
        for name in ["first", "second", "third"] {
            library.add_anchor(name, 0.0, 0.0).unwrap();
        }
        let names: Vec<String> = library
            .all_anchors()
            .unwrap()
            .into_iter()
            .map(|pin| pin.name)
            .collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn deleting_one_leaves_the_rest() {
        let library = super::tests::memory_library();
        let keep = library.add_anchor("keep", 1.0, 2.0).unwrap();
        let drop = library.add_anchor("drop", 3.0, 4.0).unwrap();

        library.delete_anchor(drop).unwrap();
        let held = library.all_anchors().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].id, keep);
    }

    #[test]
    fn an_anchor_is_not_tied_to_any_picture() {
        // Anchors outlive the pictures that happen to show the place, so removing
        // every image must leave them alone.
        let library = super::tests::memory_library();
        library.add_anchor("somewhere", 1.0, 2.0).unwrap();
        library
            .upsert_image(&ImageRecord {
                path: "/photos/a.png".to_owned(),
                folder: "/photos".to_owned(),
                filename: "a.png".to_owned(),
                bytes: 1,
                ..Default::default()
            })
            .unwrap();
        library.forget_folder("/photos").unwrap();
        assert_eq!(library.all_anchors().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod georeference_column {
    use super::*;

    #[test]
    fn a_library_that_never_looked_is_asked_only_about_tiffs() {
        let library = super::tests::memory_library();
        for name in ["map.tif", "scan.TIFF", "holiday.jpg", "diagram.png"] {
            library
                .upsert_image(&ImageRecord {
                    path: format!("/photos/{name}"),
                    folder: "/photos".to_owned(),
                    filename: name.to_owned(),
                    bytes: 1,
                    // Indexed by a build that had never heard of the column.
                    georeferenced: None,
                    ..Default::default()
                })
                .unwrap();
        }

        let waiting = library.unexamined_for_georeference(100).unwrap();
        let names: Vec<&str> = waiting
            .iter()
            .map(|(_, path)| path.rsplit('/').next().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["map.tif", "scan.TIFF"],
            "only a TIFF can carry the tags, so nothing else is worth opening",
        );

        // And the rest were answered rather than left to be asked again.
        for (_, path) in [(0, "/photos/holiday.jpg"), (0, "/photos/diagram.png")] {
            let record = library
                .all_images()
                .unwrap()
                .into_iter()
                .find(|record| record.path == path)
                .unwrap();
            assert_eq!(record.georeferenced, Some(false));
        }
    }

    #[test]
    fn a_rescan_that_did_not_look_does_not_erase_what_was_found() {
        let library = super::tests::memory_library();
        let mut record = ImageRecord {
            path: "/photos/map.tif".to_owned(),
            folder: "/photos".to_owned(),
            filename: "map.tif".to_owned(),
            bytes: 1,
            georeferenced: Some(true),
            ..Default::default()
        };
        library.upsert_image(&record).unwrap();

        // A path that refreshes the row without examining the file — a restore
        // from the trash, say.
        record.georeferenced = None;
        record.bytes = 2;
        library.upsert_image(&record).unwrap();

        let held = library.all_images().unwrap();
        assert_eq!(held[0].georeferenced, Some(true));
        assert_eq!(held[0].bytes, 2, "but the rest of the row still updated");
    }

    #[test]
    fn examining_a_file_settles_the_question_either_way() {
        let library = super::tests::memory_library();
        let id = library
            .upsert_image(&ImageRecord {
                path: "/photos/plain.tif".to_owned(),
                folder: "/photos".to_owned(),
                filename: "plain.tif".to_owned(),
                bytes: 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(library.unexamined_for_georeference(10).unwrap().len(), 1);

        library.set_georeferenced(id, false).unwrap();
        assert!(
            library.unexamined_for_georeference(10).unwrap().is_empty(),
            "a definite no must not be asked again",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_dir_moves_once_and_never_overwrites() {
        let root = std::env::temp_dir().join(format!("geo-ref_mapper-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (legacy, current) = (root.join("imgviewer"), root.join("geo-ref_mapper"));
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("library.db"), b"old").unwrap();

        adopt_legacy_dir(&legacy, &current);
        assert!(!legacy.exists());
        assert_eq!(std::fs::read(current.join("library.db")).unwrap(), b"old");

        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("library.db"), b"stale").unwrap();
        adopt_legacy_dir(&legacy, &current);
        assert_eq!(std::fs::read(current.join("library.db")).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&root);
    }

    pub(super) fn memory_library() -> Library {
        let conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate(&conn).unwrap();
        Library {
            conn,
            path: PathBuf::from(":memory:"),
        }
    }

    pub(super) fn record(path: &str) -> ImageRecord {
        ImageRecord {
            path: path.to_owned(),
            folder: "/photos".to_owned(),
            filename: path.rsplit('/').next().unwrap().to_owned(),
            format: Some("PNG".to_owned()),
            width: Some(64),
            height: Some(48),
            bytes: 1234,
            mtime: 99,
            ..Default::default()
        }
    }

    /// A picture in a named folder, for the folder-prefix tests.
    fn in_folder(folder: &str, name: &str) -> ImageRecord {
        ImageRecord {
            path: format!("{folder}/{name}"),
            folder: folder.to_owned(),
            filename: name.to_owned(),
            ..record(name)
        }
    }

    #[test]
    fn a_filter_requires_every_tag_and_refuses_every_excluded_one() {
        let library = memory_library();
        let add = |name: &str, tags: &[&str]| {
            let id = library.upsert_image(&in_folder("/maps", name)).unwrap();
            let owned: Vec<String> = tags.iter().map(|t| (*t).to_owned()).collect();
            library.set_tags(id, &owned).unwrap();
        };
        add("a.tif", &["survey", "postwar", "full-colour"]);
        add("b.tif", &["survey", "low-contrast"]);
        add("c.tif", &["hydrographic", "postwar"]);
        add("d.tif", &[]);

        let names = |inc: &[&str], exc: &[&str]| -> Vec<String> {
            let inc: Vec<String> = inc.iter().map(|t| (*t).to_owned()).collect();
            let exc: Vec<String> = exc.iter().map(|t| (*t).to_owned()).collect();
            library
                .paths_matching_tags(&inc, &exc)
                .unwrap()
                .into_iter()
                .map(|p| p.rsplit('/').next().unwrap().to_owned())
                .collect()
        };

        // Every included tag must be present — not any of them.
        assert_eq!(names(&["survey"], &[]), vec!["a.tif", "b.tif"]);
        assert_eq!(names(&["survey", "postwar"], &[]), vec!["a.tif"]);
        // Excluding removes, and combines with including.
        assert_eq!(names(&["survey"], &["low-contrast"]), vec!["a.tif"]);
        // Excluding alone is a perfectly ordinary filter.
        assert_eq!(
            names(&[], &["survey"]),
            vec!["c.tif", "d.tif"],
            "an untagged picture is not excluded by a tag it does not have",
        );
        // A tag nothing carries gives nothing rather than everything.
        assert!(names(&["nonesuch"], &[]).is_empty());
    }

    #[test]
    fn re_tagging_replaces_rather_than_accumulates() {
        // Sidecars are rewritten as mapSelector learns more; a withdrawn tag
        // must actually go.
        let library = memory_library();
        let id = library.upsert_image(&in_folder("/maps", "a.tif")).unwrap();
        library
            .set_tags(id, &["survey".to_owned(), "faded".to_owned()])
            .unwrap();
        library.set_tags(id, &["survey".to_owned()]).unwrap();

        let tags = library.tags_with_counts().unwrap();
        assert_eq!(tags, vec![("survey".to_owned(), 1)]);
    }

    #[test]
    fn an_anchor_made_for_one_picture_is_offered_to_no_other() {
        let library = memory_library();
        let mine = library.upsert_image(&record("/maps/a.tif")).unwrap();
        let yours = library.upsert_image(&record("/maps/b.tif")).unwrap();

        let shared = library.add_anchor("Fort Canning", 1.29, 103.84).unwrap();
        let corner = library
            .add_anchor_owned("Sheet NW", 1.375, 103.75, Some(mine))
            .unwrap();

        let names = |for_image| -> Vec<i64> {
            library
                .anchors_for_image(for_image)
                .unwrap()
                .into_iter()
                .map(|anchor| anchor.id)
                .collect()
        };
        assert_eq!(names(Some(mine)), vec![shared, corner]);
        assert_eq!(names(Some(yours)), vec![shared], "not another picture's");
        assert_eq!(names(None), vec![shared], "nor the shared list");
        // And `all_anchors` is the shared list, as it always was.
        assert_eq!(library.all_anchors().unwrap().len(), 1);
    }

    #[test]
    fn a_pictures_own_anchors_go_when_the_picture_does() {
        let library = memory_library();
        let id = library.upsert_image(&record("/maps/a.tif")).unwrap();
        library.add_anchor("Shared", 1.29, 103.84).unwrap();
        library
            .add_anchor_owned("Sheet NW", 1.375, 103.75, Some(id))
            .unwrap();

        library.forget_image("/maps/a.tif").unwrap();
        assert_eq!(
            library.anchors_for_image(Some(id)).unwrap().len(),
            1,
            "the corner should have gone with its picture",
        );
    }

    #[test]
    fn a_folder_queues_everything_beneath_it() {
        let library = memory_library();
        let here = library.upsert_image(&in_folder("/MAPS", "a.tif")).unwrap();
        let deep = library
            .upsert_image(&in_folder("/MAPS/1871/scans", "b.tif"))
            .unwrap();
        library.upsert_image(&in_folder("/OTHER", "c.tif")).unwrap();

        // Sorted before comparing: the rows come back in path order, where a
        // subfolder called `1871` precedes a file called `a.tif`.
        let mut found = library.image_ids_under("/MAPS").unwrap();
        found.sort_unstable();
        assert_eq!(found, vec![here, deep]);
    }

    #[test]
    fn a_folder_does_not_swallow_the_one_next_to_it() {
        // The trap: `/MAPS` as a plain prefix also matches `/MAPS-NEW`, which
        // on the real library is 7,000 files nobody asked to queue.
        let library = memory_library();
        let mine = library.upsert_image(&in_folder("/MAPS", "a.tif")).unwrap();
        library
            .upsert_image(&in_folder("/MAPS-NEW/1871", "b.tif"))
            .unwrap();
        library
            .upsert_image(&in_folder("/MAPSTER", "c.tif"))
            .unwrap();

        assert_eq!(library.image_ids_under("/MAPS").unwrap(), vec![mine]);
        // And a trailing slash asks the same question.
        assert_eq!(library.image_ids_under("/MAPS/").unwrap(), vec![mine]);
    }

    #[test]
    fn a_wildcard_in_a_folder_name_is_a_character_not_a_pattern() {
        // `LIKE` would read these as "any string" and "any character".
        let library = memory_library();
        let odd = library
            .upsert_image(&in_folder("/100% scans", "a.tif"))
            .unwrap();
        library
            .upsert_image(&in_folder("/100x scans", "b.tif"))
            .unwrap();
        assert_eq!(library.image_ids_under("/100% scans").unwrap(), vec![odd]);
    }

    #[test]
    fn upsert_is_idempotent_on_path() {
        let library = memory_library();
        let first = library.upsert_image(&record("/photos/a.png")).unwrap();
        let second = library.upsert_image(&record("/photos/a.png")).unwrap();
        assert_eq!(first, second);
        assert_eq!(library.stats().unwrap().images, 1);
    }

    #[test]
    fn is_current_tracks_size_and_mtime() {
        let library = memory_library();
        library.upsert_image(&record("/photos/a.png")).unwrap();
        assert!(library.is_current("/photos/a.png", 1234, 99).unwrap());
        assert!(!library.is_current("/photos/a.png", 1234, 100).unwrap());
        assert!(!library.is_current("/photos/missing.png", 1234, 99).unwrap());
    }

    #[test]
    fn thumbnails_round_trip_and_replace() {
        let library = memory_library();
        let id = library.upsert_image(&record("/photos/a.png")).unwrap();
        assert!(library.thumbnail(id, 512).unwrap().is_none());

        library
            .put_thumbnail(id, 512, 512, 384, "webp", &[1, 2, 3])
            .unwrap();
        assert_eq!(library.thumbnail(id, 512).unwrap().unwrap(), vec![1, 2, 3]);

        library
            .put_thumbnail(id, 512, 512, 384, "webp", &[4, 5])
            .unwrap();
        assert_eq!(library.thumbnail(id, 512).unwrap().unwrap(), vec![4, 5]);
        assert_eq!(library.stats().unwrap().thumbnails, 1);
    }

    #[test]
    fn paths_under_a_folder_spare_its_lookalike_neighbour() {
        // The same prefix trap as `forget_folder`: a refresh of `/photos/trip`
        // must not go looking at `/photos/trip-2024`'s files.
        let library = memory_library();
        library.upsert_image(&record("/photos/trip/a.png")).unwrap();
        library
            .upsert_image(&record("/photos/trip-2024/b.png"))
            .unwrap();

        let mut under = library.paths_under("/photos/trip").unwrap();
        under.sort();
        assert_eq!(under, vec!["/photos/trip/a.png".to_owned()]);
    }

    #[test]
    fn forgetting_a_folder_spares_its_lookalike_neighbour() {
        // `/photos/trip` must not take `/photos/trip-2024` with it, which a
        // bare prefix match would.
        let library = memory_library();
        library.upsert_image(&record("/photos/trip/a.png")).unwrap();
        library.upsert_image(&record("/photos/trip/b.png")).unwrap();
        library
            .upsert_image(&record("/photos/trip-2024/c.png"))
            .unwrap();
        library
            .upsert_image(&record("/photos/other/d.png"))
            .unwrap();

        assert_eq!(library.forget_folder("/photos/trip").unwrap(), 2);

        let mut left: Vec<String> = library
            .all_images()
            .unwrap()
            .into_iter()
            .map(|record| record.path)
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "/photos/other/d.png".to_owned(),
                "/photos/trip-2024/c.png".to_owned()
            ],
        );
    }

    #[test]
    fn a_stack_keeps_its_cover_first_and_absorbs_old_ones() {
        let library = memory_library();
        let ids: Vec<i64> = ["a", "b", "c", "d"]
            .into_iter()
            .map(|name| {
                library
                    .upsert_image(&record(&format!("/photos/{name}.png")))
                    .unwrap()
            })
            .collect();

        library.create_stack("/photos", &[ids[0], ids[1]]).unwrap();
        // Re-stacking with a third absorbs the first stack rather than leaving
        // an image in two of them, which the unique index would refuse anyway.
        let merged = library
            .create_stack("/photos", &[ids[1], ids[0], ids[2]])
            .unwrap();

        let stacks = library.all_stacks().unwrap();
        assert_eq!(stacks.len(), 1, "the old stack was absorbed: {stacks:?}");
        assert_eq!(stacks[0].id, merged);
        assert_eq!(stacks[0].members, vec![ids[1], ids[0], ids[2]]);

        // The cover is whichever member sits first.
        library.set_stack_cover(merged, ids[2]).unwrap();
        assert_eq!(library.all_stacks().unwrap()[0].members[0], ids[2]);

        // Taking one out leaves the rest; taking enough out dissolves it.
        library.remove_from_stack(&[ids[0]]).unwrap();
        assert_eq!(library.all_stacks().unwrap()[0].members.len(), 2);
        library.remove_from_stack(&[ids[1]]).unwrap();
        assert!(
            library.all_stacks().unwrap().is_empty(),
            "a stack of one is not a stack"
        );

        assert!(ids[3] > 0);
    }

    #[test]
    fn deleting_a_picture_leaves_its_stack_consistent() {
        // Deleting the cover must promote the next member rather than leave the
        // stack pointing at something that is gone.
        let library = memory_library();
        let ids: Vec<i64> = ["a", "b", "c"]
            .into_iter()
            .map(|name| {
                library
                    .upsert_image(&record(&format!("/photos/{name}.png")))
                    .unwrap()
            })
            .collect();
        library.create_stack("/photos", &ids).unwrap();

        library.forget_image("/photos/a.png").unwrap();
        let stacks = library.all_stacks().unwrap();
        assert_eq!(stacks.len(), 1);
        assert_eq!(
            stacks[0].members,
            vec![ids[1], ids[2]],
            "b is the cover now"
        );

        library.forget_image("/photos/b.png").unwrap();
        assert!(
            library.all_stacks().unwrap().is_empty(),
            "down to one member, so it is no longer a stack",
        );
    }

    #[test]
    fn a_forgotten_image_leaves_the_tree() {
        // What has to be true after a file goes to the trash: the row is gone
        // from the library, so the tree rebuilt from it no longer offers the
        // picture in the Mosaic or the filmstrip.
        let library = memory_library();
        library.upsert_image(&record("/photos/keep.png")).unwrap();
        library.upsert_image(&record("/photos/gone.png")).unwrap();

        library.forget_image("/photos/gone.png").unwrap();

        let records = library.all_images().unwrap();
        assert_eq!(records.len(), 1);
        let tree = crate::files::FileTree::from_records(&records);
        let names: Vec<String> = tree
            .images_in(&[0], true)
            .into_iter()
            .map(|(_, node)| node.name.clone())
            .collect();
        assert_eq!(names, vec!["keep.png".to_owned()]);
    }

    #[test]
    fn a_failure_from_an_older_build_is_retried() {
        // The WebP-tiled maps in MAPS/NUS libmaps were marked undecodable
        // before the COG reader existed. Once it did, they still showed a flat
        // colour: the sweep saw a thumbnail row and never looked again.
        let library = memory_library();
        let id = library.upsert_image(&record("/photos/map.tif")).unwrap();

        library
            .put_thumbnail(id, 512, 0, 0, "none@0.1.0", &[])
            .unwrap();
        assert_eq!(
            library.images_without_thumbnail(512, 10).unwrap().len(),
            1,
            "a failure recorded by an older build must be tried again",
        );

        library
            .put_thumbnail(id, 512, 0, 0, &failure_marker(), &[])
            .unwrap();
        assert!(
            library
                .images_without_thumbnail(512, 10)
                .unwrap()
                .is_empty(),
            "this build already gave up on it; retrying every sweep is waste",
        );

        library.put_thumbnail(id, 512, 8, 8, "webp", &[0]).unwrap();
        assert!(
            library
                .images_without_thumbnail(512, 10)
                .unwrap()
                .is_empty(),
            "a real thumbnail still counts as covered",
        );
    }

    #[test]
    fn images_without_thumbnail_skips_covered_rows() {
        let library = memory_library();
        let a = library.upsert_image(&record("/photos/a.png")).unwrap();
        library.upsert_image(&record("/photos/b.png")).unwrap();
        library.put_thumbnail(a, 512, 8, 8, "webp", &[0]).unwrap();

        let pending = library.images_without_thumbnail(512, 10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].filename, "b.png");
    }

    #[test]
    fn exif_round_trips() {
        let library = memory_library();
        let id = library.upsert_image(&record("/photos/a.png")).unwrap();
        assert!(library.exif(id).unwrap().is_none());

        library
            .put_exif(
                id,
                &ExifRecord {
                    camera: Some("Nikon Z8".to_owned()),
                    iso: Some(400),
                    ..Default::default()
                },
            )
            .unwrap();
        let exif = library.exif(id).unwrap().unwrap();
        assert_eq!(exif.camera.as_deref(), Some("Nikon Z8"));
        assert_eq!(exif.iso, Some(400));
    }

    #[test]
    fn settings_round_trip_and_overwrite() {
        let library = memory_library();
        assert!(library.setting("last_selection").unwrap().is_none());

        library.set_setting("last_selection", "/photos/a").unwrap();
        assert_eq!(
            library.setting("last_selection").unwrap().as_deref(),
            Some("/photos/a")
        );

        library.set_setting("last_selection", "/photos/b").unwrap();
        assert_eq!(
            library.setting("last_selection").unwrap().as_deref(),
            Some("/photos/b"),
            "writing the same key again replaces rather than duplicates"
        );
    }

    #[test]
    fn extension_filter_is_case_insensitive() {
        assert!(is_image_extension("JPG"));
        assert!(is_image_extension("qoi"));
        assert!(!is_image_extension("txt"));
    }
}
