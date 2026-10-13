//! Registering geo-ref_mapper as the system's handler for the formats it decodes.
//!
//! On Linux this is two pieces of user-level state, both under `$HOME`:
//!
//!   * `~/.local/share/applications/geo-ref_mapper.desktop` — the launcher entry
//!     that tells the desktop what we are and how to run us.
//!   * `~/.config/mimeapps.list` — the per-user default-application table.
//!
//! Nothing outside the user's own config is written, nothing needs root, and
//! [`remove`] puts both files back the way they were. The file rewriting is
//! deliberately conservative: entries belonging to other applications are
//! copied through untouched, and on removal only lines naming *our* desktop id
//! are dropped. That is what makes this safe to offer as a single button.

use std::io;
use std::path::{Path, PathBuf};

/// The desktop-entry filename, which doubles as the id used in `mimeapps.list`.
pub const DESKTOP_ID: &str = "geo-ref_mapper.desktop";

/// MIME types we claim, paired with the extensions each covers so the UI can
/// say what is actually being registered.
///
/// The names were taken from the installed `shared-mime-info` database rather
/// than invented — `image/x-hdr` and `image/qoi` in particular are easy to
/// guess wrong. A couple of extensions we open (`.pam`, `.ff`, `.psb`) have no
/// registered type at all, so they cannot be claimed and are not listed.
pub const ASSOCIATIONS: &[(&str, &str)] = &[
    ("image/png", ".png"),
    ("image/apng", ".apng"),
    ("image/jpeg", ".jpg .jpeg .jfif"),
    ("image/gif", ".gif"),
    ("image/webp", ".webp"),
    ("image/bmp", ".bmp .dib"),
    ("image/tiff", ".tif .tiff"),
    ("image/vnd.microsoft.icon", ".ico"),
    ("image/x-tga", ".tga"),
    ("image/x-dds", ".dds"),
    ("image/x-portable-anymap", ".pnm"),
    ("image/x-portable-bitmap", ".pbm"),
    ("image/x-portable-graymap", ".pgm"),
    ("image/x-portable-pixmap", ".ppm"),
    ("image/qoi", ".qoi"),
    ("image/x-exr", ".exr"),
    ("image/x-hdr", ".hdr .pic"),
    // No `image/avif`. Claiming a type we cannot decode is worse than not
    // claiming it: it takes AVIF off whatever *can* open it, and every
    // double-click then lands on a viewer that refuses the file.
    ("image/vnd.adobe.photoshop", ".psd"),
    ("image/jxl", ".jxl"),
    ("image/svg+xml", ".svg"),
    ("image/svg+xml-compressed", ".svgz"),
    ("image/x-xcf", ".xcf"),
    // Distributions disagree about HEIC: shared-mime-info maps .heic to
    // image/heif, but some desktops and browsers emit image/heic. Claim both.
    ("image/heif", ".heic .heif .hif"),
    ("image/heic", ".heic"),
];

/// Whether this build can register itself at all.
pub fn supported() -> bool {
    cfg!(target_os = "linux")
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `$XDG_DATA_HOME/applications`, falling back to the specified default.
pub fn applications_dir() -> Option<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(data) if !data.is_empty() => Some(PathBuf::from(data).join("applications")),
        _ => home().map(|home| home.join(".local/share/applications")),
    }
}

pub fn desktop_file() -> Option<PathBuf> {
    applications_dir().map(|dir| dir.join(DESKTOP_ID))
}

/// `$XDG_CONFIG_HOME/mimeapps.list`, falling back to the specified default.
pub fn mimeapps_file() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(config) if !config.is_empty() => Some(PathBuf::from(config).join("mimeapps.list")),
        _ => home().map(|home| home.join(".config/mimeapps.list")),
    }
}

/// The desktop entry's text. `exec` is the absolute path to this binary, so a
/// build run straight out of `target/release` registers itself correctly
/// without needing to be installed anywhere first.
///
/// `Icon=` is a *name*, not a path: the desktop looks it up in the icon theme,
/// which is where [`crate::install`] writes the mark at each size it wants. A
/// name that resolves to nothing shows a blank tile rather than failing, so the
/// two are pinned together by a test.
pub fn desktop_entry(exec: &Path) -> String {
    let exec = exec.display();
    let mimes: Vec<&str> = ASSOCIATIONS.iter().map(|(mime, _)| *mime).collect();
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=geo-ref_mapper\n\
         GenericName=Image Viewer\n\
         Comment=Fast viewer for large and unusual image formats\n\
         Exec=\"{exec}\" %f\n\
         Icon={}\n\
         Terminal=false\n\
         Categories=Graphics;Viewer;RasterGraphics;2DGraphics;\n\
         MimeType={};\n\
         NoDisplay=false\n\
         StartupNotify=true\n\
         StartupWMClass=geo-ref_mapper\n",
        crate::install::APP_FILE_NAME,
        mimes.join(";"),
    )
}

/// The binary the launcher entry should point at.
///
/// The installed copy when there is one, so the entry keeps working after the
/// build directory it was first registered from is cleaned away. Otherwise
/// whatever is running now, which is what makes the default-viewer button
/// useful on its own without installing anything.
pub fn exec_for_entry() -> io::Result<PathBuf> {
    if crate::install::is_installed()
        && let Some(installed) = crate::install::installed_exe()
    {
        return Ok(installed);
    }
    std::env::current_exe()
}

/// Writes the launcher entry, creating the directory it lives in.
///
/// Shared by the two buttons that both need one on disk: becoming the default
/// image viewer, and installing. Whichever runs second rewrites the same file,
/// so they cannot disagree about what it says.
pub fn write_desktop_entry(exec: &Path) -> io::Result<()> {
    let dir = applications_dir().ok_or_else(|| io::Error::other("no HOME to install into"))?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(DESKTOP_ID), desktop_entry(exec))
}

// ------------------------------------------------------- mimeapps.list editing

/// Sets `desktop_id` as the default for every MIME type in `mimes`, leaving
/// every other line of the file alone.
///
/// `mimeapps.list` is a desktop-entry-format INI. Only `[Default Applications]`
/// is rewritten; other sections — notably `[Added Associations]`, which is a
/// list rather than a single choice — are copied through verbatim.
pub fn apply_defaults(existing: &str, desktop_id: &str, mimes: &[&str]) -> String {
    let mut out = String::new();
    let mut written = false;
    let mut in_defaults = false;

    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            // Leaving the section we own: emit our entries before the next one.
            if in_defaults {
                push_defaults(&mut out, desktop_id, mimes);
                written = true;
            }
            in_defaults = trimmed == "[Default Applications]";
            out.push_str(line);
            out.push('\n');
            continue;
        }
        // Inside our section, drop any prior claim on the types we are taking
        // over; anything else (other apps' defaults) is preserved.
        if in_defaults
            && let Some((key, _)) = trimmed.split_once('=')
            && mimes.contains(&key.trim())
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    if in_defaults {
        push_defaults(&mut out, desktop_id, mimes);
        written = true;
    }
    if !written {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str("[Default Applications]\n");
        push_defaults(&mut out, desktop_id, mimes);
    }
    out
}

fn push_defaults(out: &mut String, desktop_id: &str, mimes: &[&str]) {
    for mime in mimes {
        out.push_str(mime);
        out.push('=');
        out.push_str(desktop_id);
        out.push('\n');
    }
}

/// Drops every line that names `desktop_id`, in any section. Entries pointing
/// at other applications are untouched, so handing a format back means the
/// previous handler — or the system default — takes over again.
pub fn remove_defaults(existing: &str, desktop_id: &str) -> String {
    let mut out = String::new();
    for line in existing.lines() {
        let trimmed = line.trim();
        if let Some((_, value)) = trimmed.split_once('=')
            && value
                .split(';')
                .any(|entry| entry.trim() == desktop_id)
            // Only when *we* are the sole entry; a list naming us alongside
            // others is rewritten below rather than dropped.
            && value.split(';').filter(|entry| !entry.trim().is_empty()).count() == 1
        {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=')
            && value.contains(desktop_id)
        {
            let kept: Vec<&str> = value
                .split(';')
                .map(str::trim)
                .filter(|entry| !entry.is_empty() && *entry != desktop_id)
                .collect();
            out.push_str(key.trim());
            out.push('=');
            out.push_str(&kept.join(";"));
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Which of our MIME types currently name us in `mimeapps.list`.
pub fn registered_count() -> usize {
    let Some(path) = mimeapps_file() else {
        return 0;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return 0;
    };

    let mut in_defaults = false;
    let mut count = 0;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_defaults = trimmed == "[Default Applications]";
            continue;
        }
        if in_defaults
            && let Some((key, value)) = trimmed.split_once('=')
            && value.trim() == DESKTOP_ID
            && ASSOCIATIONS.iter().any(|(mime, _)| *mime == key.trim())
        {
            count += 1;
        }
    }
    count
}

/// True once the desktop entry exists and at least one type points at us.
pub fn installed() -> bool {
    desktop_file().is_some_and(|path| path.exists()) && registered_count() > 0
}

// ------------------------------------------------------------------ the actions

/// Writes the desktop entry and claims every supported type.
pub fn install() -> io::Result<usize> {
    if !supported() {
        return Err(io::Error::other(
            "setting the default image viewer is only implemented for Linux desktops",
        ));
    }

    write_desktop_entry(&exec_for_entry()?)?;

    let mimeapps = mimeapps_file().ok_or_else(|| io::Error::other("no HOME to install into"))?;
    if let Some(parent) = mimeapps.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&mimeapps).unwrap_or_default();
    let mimes: Vec<&str> = ASSOCIATIONS.iter().map(|(mime, _)| *mime).collect();
    std::fs::write(&mimeapps, apply_defaults(&existing, DESKTOP_ID, &mimes))?;

    // Best effort: desktops that keep a cache want to be told. Failing here
    // just means the entry shows up a little later, so the error is dropped.
    if let Some(dir) = applications_dir() {
        let _ = std::process::Command::new("update-desktop-database")
            .arg(&dir)
            .status();
    }

    Ok(mimes.len())
}

/// Gives every type back, and takes the launcher entry away with them unless
/// the program is installed.
///
/// The entry is shared. Giving up the file associations should not also take
/// geo-ref_mapper out of the menu when it has been installed there deliberately —
/// they are two decisions, and one of them is the one the user just made. So
/// with an install present the entry is rewritten rather than deleted, and only
/// the `mimeapps.list` claims go.
pub fn remove() -> io::Result<()> {
    if crate::install::is_installed() {
        write_desktop_entry(&exec_for_entry()?)?;
    } else if let Some(entry) = desktop_file()
        && entry.exists()
    {
        std::fs::remove_file(&entry)?;
    }
    if let Some(mimeapps) = mimeapps_file()
        && let Ok(existing) = std::fs::read_to_string(&mimeapps)
    {
        std::fs::write(&mimeapps, remove_defaults(&existing, DESKTOP_ID))?;
    }
    if let Some(dir) = applications_dir() {
        let _ = std::process::Command::new("update-desktop-database")
            .arg(&dir)
            .status();
    }
    Ok(())
}

// ------------------------------------------------------- showing a file to the OS

/// `file://` URI for `path`, percent-encoding everything a URI cannot carry
/// raw. Filenames here contain spaces, brackets and commas — `Singapore -
/// Sheet No. 77 (Pulau Tekong Mukim) 1933 [SP002453].tif` breaks a naive
/// concatenation, and D-Bus rejects the result rather than opening the folder.
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                uri.push(*byte as char);
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// Opens the system file manager with `path` highlighted.
///
/// The freedesktop `FileManager1` D-Bus interface is what actually selects the
/// file — Nemo, Nautilus, Dolphin and Thunar all implement it. `xdg-open` alone
/// cannot: it opens a folder, with nothing picked out. So D-Bus is tried first
/// and the folder is the fallback for a desktop that does not answer.
/// Opens the file manager on a file, without waiting for it.
///
/// Handed to a thread of its own because both steps below **block until the
/// other program answers**: `gdbus call` waits for the file manager to reply,
/// and starting one that is not already running takes seconds. Called straight
/// from a menu, that is the whole window frozen — and worst exactly when the
/// machine is busy, which is when a thumbnail sweep is running and it looks
/// like the menu item simply does not work.
///
/// Nothing useful can be returned once it is off on its own, so a failure is
/// reported the way the call sites already report one: to the log.
pub fn show_in_file_manager(path: &Path) {
    let path = path.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("geo-ref_mapper-file-manager".to_owned())
        .spawn(move || {
            if let Err(error) = open_file_manager(&path) {
                eprintln!("geo-ref_mapper: {error}");
            }
        });
    if let Err(error) = spawned {
        eprintln!("geo-ref_mapper: could not start a thread to open the file manager: {error}");
    }
}

fn open_file_manager(path: &Path) -> io::Result<()> {
    let uri = file_uri(path);
    let shown = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.FileManager1",
            "--object-path",
            "/org/freedesktop/FileManager1",
            "--method",
            "org.freedesktop.FileManager1.ShowItems",
        ])
        .arg(format!("['{uri}']"))
        .arg("")
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if shown {
        return Ok(());
    }

    let folder = path.parent().unwrap_or(path);
    let status = std::process::Command::new("xdg-open")
        .arg(folder)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "no file manager would open {}",
            folder.display()
        )))
    }
}

// -------------------------------------------------------------------- deleting

/// Moves a file to the desktop trash.
///
/// **This is the only thing in geo-ref_mapper that removes one of your files**, and
/// it deliberately does not unlink anything. Trashing is reversible from the
/// file manager; `remove_file` is not, and a viewer holding 26,000 photographs
/// is the wrong place to offer an irreversible one-keystroke delete.
///
/// `gio trash` is used because it implements the whole freedesktop spec —
/// including the per-volume `.Trash-$uid` directory a file on another disk
/// needs, which a home-trash-only implementation would silently get wrong. The
/// fallback below handles the home volume when `gio` is missing.
pub fn move_to_trash(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is already gone", path.display()),
        ));
    }

    let trashed = std::process::Command::new("gio")
        .arg("trash")
        .arg(path)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if trashed {
        return Ok(());
    }
    home_trash(path)
}

/// The freedesktop home trash: the file moves to `files/`, and a `.trashinfo`
/// alongside it records where it came from so the file manager can put it back.
fn home_trash(path: &Path) -> io::Result<()> {
    let home = home().ok_or_else(|| io::Error::other("no HOME to trash into"))?;
    let trash = match std::env::var_os("XDG_DATA_HOME") {
        Some(data) if !data.is_empty() => PathBuf::from(data).join("Trash"),
        _ => home.join(".local/share/Trash"),
    };
    std::fs::create_dir_all(trash.join("files"))?;
    std::fs::create_dir_all(trash.join("info"))?;

    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("no filename to trash"))?;
    // A name already in the trash must not be overwritten — that would destroy
    // whatever was deleted first, which is the opposite of the point.
    let (target, info) = unique_trash_name(&trash, &name.to_string_lossy())?;

    std::fs::write(
        &info,
        format!(
            "[Trash Info]\nPath={}\nDeletionDate={}\n",
            trash_info_path(path),
            // No clock is threaded through here and the field is only shown as
            // "deleted on"; an empty date is honest and the entry still works.
            "",
        ),
    )?;
    // Rename first: it is atomic within a volume. Across volumes it fails with
    // CrossesDevices, and the copy-then-delete fallback takes over.
    match std::fs::rename(path, &target) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(path, &target)?;
            std::fs::remove_file(path)?;
            Ok(())
        }
    }
}

fn unique_trash_name(trash: &Path, name: &str) -> io::Result<(PathBuf, PathBuf)> {
    for suffix in 0..10_000 {
        let candidate = if suffix == 0 {
            name.to_owned()
        } else {
            format!("{name}.{suffix}")
        };
        let file = trash.join("files").join(&candidate);
        let info = trash.join("info").join(format!("{candidate}.trashinfo"));
        if !file.exists() && !info.exists() {
            return Ok((file, info));
        }
    }
    Err(io::Error::other("the trash already holds this name"))
}

/// `Path=` in a `.trashinfo` is percent-encoded the same way a URI is, but
/// without the scheme.
fn trash_info_path(path: &Path) -> String {
    file_uri(path)
        .strip_prefix("file://")
        .unwrap_or_default()
        .to_owned()
}

/// One thing sitting in the desktop trash.
#[derive(Clone, PartialEq, Debug)]
pub struct Trashed {
    /// `trash:///name`, which is what `gio trash --restore` wants back.
    pub uri: String,
    /// Where it came from, and where restoring puts it.
    pub original: PathBuf,
}

impl Trashed {
    pub fn name(&self) -> String {
        self.original
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.uri.clone())
    }
}

/// Parses `gio trash --list`, which prints `trash:///name<TAB>/original/path`.
pub fn parse_trash_list(text: &str) -> Vec<Trashed> {
    text.lines()
        .filter_map(|line| {
            let (uri, original) = line.split_once('\t')?;
            (!uri.is_empty() && !original.is_empty()).then(|| Trashed {
                uri: uri.to_owned(),
                original: PathBuf::from(original),
            })
        })
        .collect()
}

/// What is in the trash that this application would have shown.
///
/// Filtered to images on purpose: the trash is shared with everything else on
/// the desktop, and a list of someone's deleted spreadsheets is not this
/// application's business to display.
pub fn trashed_images() -> Vec<Trashed> {
    let Ok(output) = std::process::Command::new("gio")
        .args(["trash", "--list"])
        .output()
    else {
        return Vec::new();
    };
    parse_trash_list(&String::from_utf8_lossy(&output.stdout))
        .into_iter()
        .filter(|entry| {
            entry
                .original
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(crate::db::is_image_extension)
        })
        .collect()
}

/// Puts one file back where it came from.
///
/// `gio` does the work rather than a hand-rolled move: it recreates the
/// original directory if it has gone, and it refuses to overwrite a file that
/// has since taken the same name — both of which a naive rename would get
/// wrong, in the direction of losing data.
pub fn restore_from_trash(entry: &Trashed) -> io::Result<()> {
    let status = std::process::Command::new("gio")
        .args(["trash", "--restore"])
        .arg(&entry.uri)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "could not restore {} — something may already be at that name",
            entry.original.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trash_list_is_parsed_as_gio_prints_it() {
        let text = "trash:///M5.png\t/home/morgan/icons/M5.png\n\
                    trash:///a b.tif\t/home/morgan/Pictures/a b.tif\n\
                    rubbish-without-a-tab\n";
        let entries = parse_trash_list(text);
        assert_eq!(
            entries.len(),
            2,
            "the malformed line is skipped: {entries:?}"
        );
        assert_eq!(entries[0].uri, "trash:///M5.png");
        assert_eq!(
            entries[0].original,
            PathBuf::from("/home/morgan/icons/M5.png")
        );
        assert_eq!(entries[1].name(), "a b.tif");
    }

    #[test]
    fn file_uris_survive_the_names_in_this_library() {
        // A real filename from ~/Pictures/MAPS-NEW, which has every character
        // that makes naive URI building fail.
        assert_eq!(
            file_uri(Path::new(
                "/home/morgan/Pictures/Sheet No. 77 (Pulau Tekong) 1933 [SP002453].tif"
            )),
            "file:///home/morgan/Pictures/\
             Sheet%20No.%2077%20%28Pulau%20Tekong%29%201933%20%5BSP002453%5D.tif",
        );
        assert_eq!(file_uri(Path::new("/tmp/a.png")), "file:///tmp/a.png");
        // Non-ASCII is UTF-8 then percent-encoded, byte by byte.
        assert_eq!(file_uri(Path::new("/tmp/é.png")), "file:///tmp/%C3%A9.png");
    }

    #[test]
    fn trash_never_overwrites_an_earlier_deletion() {
        let trash =
            std::env::temp_dir().join(format!("geo-ref_mapper-trash-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&trash);
        std::fs::create_dir_all(trash.join("files")).unwrap();
        std::fs::create_dir_all(trash.join("info")).unwrap();

        let (first, first_info) = unique_trash_name(&trash, "map.tif").unwrap();
        std::fs::write(&first, b"one").unwrap();
        std::fs::write(&first_info, b"").unwrap();

        // Deleting a second file of the same name must land somewhere else.
        let (second, _) = unique_trash_name(&trash, "map.tif").unwrap();
        assert_ne!(
            first, second,
            "the first deletion would have been destroyed"
        );
        assert_eq!(std::fs::read(&first).unwrap(), b"one");

        let _ = std::fs::remove_dir_all(&trash);
    }

    #[test]
    fn trashinfo_path_is_encoded_without_the_scheme() {
        assert_eq!(
            trash_info_path(Path::new("/home/morgan/Pictures/a map (1933).tif")),
            "/home/morgan/Pictures/a%20map%20%281933%29.tif",
        );
    }

    #[test]
    fn trashing_something_that_is_gone_is_an_error_not_a_success() {
        let missing = std::env::temp_dir().join("geo-ref_mapper-no-such-file-ever.png");
        let _ = std::fs::remove_file(&missing);
        assert!(move_to_trash(&missing).is_err());
    }

    #[test]
    fn every_association_is_a_format_we_open() {
        // The point of the table is that clicking a file we claim opens here
        // and actually decodes. A claimed extension we cannot read would be a
        // worse experience than not claiming it at all.
        for (mime, extensions) in ASSOCIATIONS {
            for extension in extensions.split_whitespace() {
                let bare = extension.trim_start_matches('.');
                assert!(
                    crate::db::is_image_extension(bare),
                    "{mime} claims {extension}, which we do not open",
                );
            }
        }
    }

    #[test]
    fn desktop_entry_lists_every_mime() {
        let text = desktop_entry(Path::new("/opt/geo-ref_mapper/geo-ref_mapper"));
        assert!(text.contains("Exec=\"/opt/geo-ref_mapper/geo-ref_mapper\" %f"));
        for (mime, _) in ASSOCIATIONS {
            assert!(text.contains(mime), "{mime} missing from the desktop entry");
        }
    }

    #[test]
    fn defaults_are_added_to_an_empty_file() {
        let out = apply_defaults("", DESKTOP_ID, &["image/png"]);
        assert_eq!(out, "[Default Applications]\nimage/png=geo-ref_mapper.desktop\n");
    }

    #[test]
    fn other_applications_survive() {
        let existing = "[Default Applications]\n\
                        image/png=org.gnome.eog.desktop\n\
                        text/html=firefox.desktop\n\
                        \n\
                        [Added Associations]\n\
                        image/png=org.gnome.eog.desktop;\n";
        let out = apply_defaults(existing, DESKTOP_ID, &["image/png"]);
        // We took PNG...
        assert!(out.contains("image/png=geo-ref_mapper.desktop\n"));
        assert!(!out.contains("image/png=org.gnome.eog.desktop\n"));
        // ...and left everything else exactly as it was.
        assert!(out.contains("text/html=firefox.desktop"));
        assert!(out.contains("[Added Associations]\nimage/png=org.gnome.eog.desktop;"));
    }

    #[test]
    fn applying_twice_does_not_duplicate() {
        let mimes = ["image/png", "image/jpeg"];
        let once = apply_defaults("", DESKTOP_ID, &mimes);
        let twice = apply_defaults(&once, DESKTOP_ID, &mimes);
        assert_eq!(once, twice);
    }

    #[test]
    fn removal_restores_the_previous_state() {
        let before = "[Default Applications]\n\
                      image/png=org.gnome.eog.desktop\n\
                      text/html=firefox.desktop\n";
        let after = apply_defaults(before, DESKTOP_ID, &["image/jpeg"]);
        let restored = remove_defaults(&after, DESKTOP_ID);
        assert!(restored.contains("image/png=org.gnome.eog.desktop"));
        assert!(restored.contains("text/html=firefox.desktop"));
        assert!(!restored.contains(DESKTOP_ID));
    }

    #[test]
    fn removal_keeps_us_out_of_lists_without_deleting_them() {
        // `[Added Associations]` values are lists. Dropping the whole line
        // would take another application's association down with us.
        let existing = "[Added Associations]\nimage/png=geo-ref_mapper.desktop;org.gnome.eog.desktop;\n";
        let out = remove_defaults(existing, DESKTOP_ID);
        assert_eq!(
            out,
            "[Added Associations]\nimage/png=org.gnome.eog.desktop\n"
        );
    }
}

// ------------------------------------------------------------- the clipboard

/// Longest edge of an image put on the clipboard.
///
/// Full resolution is the wrong thing here: the destinations are a web search
/// box and a chat window, neither of which wants a 24,000-pixel scan, and the
/// encode alone would take seconds. Anything smaller than this is copied
/// untouched.
pub const CLIPBOARD_EDGE: u32 = 4096;

/// Puts a picture on the clipboard as an image, so it pastes into a browser or
/// a chat rather than arriving as a filename.
///
/// The clipboard handle is owned by the caller and has to outlive the call: on
/// X11 the copying application *is* the clipboard, serving the data on request
/// until something else takes over. Creating a handle here and dropping it
/// would put the image on the clipboard and take it straight off again.
pub fn copy_image_to_clipboard(
    clipboard: &mut arboard::Clipboard,
    path: &Path,
) -> io::Result<(u32, u32)> {
    let image = crate::loader::decode_still(path, CLIPBOARD_EDGE)
        .map_err(|error| io::Error::other(format!("{error:#}")))?;
    let image = if image.width().max(image.height()) > CLIPBOARD_EDGE {
        let [width, height] =
            crate::loader::fit_within(image.width(), image.height(), CLIPBOARD_EDGE);
        image.thumbnail_exact(width, height)
    } else {
        image
    };

    let rgba = image.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    clipboard
        .set_image(arboard::ImageData {
            width: width as usize,
            height: height as usize,
            bytes: rgba.into_raw().into(),
        })
        .map_err(|error| io::Error::other(format!("{error}")))?;
    Ok((width, height))
}
