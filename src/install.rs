//! Putting geo-ref_mapper into the user's own `~/.local`, so the desktop can find
//! it, show its icon, and let it be pinned.
//!
//! Three pieces of state, all under `$HOME`, none of them needing root:
//!
//!   * `~/.local/bin/geo-ref_mapper` — the program itself, on the PATH most
//!     distributions already set up for a user.
//!   * `~/.local/share/icons/hicolor/<size>/apps/geo-ref_mapper.png` — the mark, at
//!     the sizes the icon theme looks for. Without these a launcher entry shows
//!     a blank tile or the generic cog, which is not something you pin.
//!   * `~/.local/share/applications/geo-ref_mapper.desktop` — the launcher entry,
//!     written by [`crate::desktop`], which is what the menu lists.
//!
//! This is deliberately separate from *becoming the default image viewer*: one
//! is "the desktop knows this program exists", the other is "double-clicking a
//! JPEG opens it". Either is useful without the other, so they are two buttons
//! — but both write the same launcher entry, so installing repoints it at the
//! installed copy rather than at wherever the build happened to be run from.

use image::ImageEncoder as _;
use std::io;
use std::path::{Path, PathBuf};

/// The name the binary and the icon are installed under.
///
/// The same spelling as the desktop id, so the `Icon=geo-ref_mapper` line in the
/// entry and `.../apps/geo-ref_mapper.png` on disk are the same string, and a
/// mismatch cannot creep in between them.
pub const APP_FILE_NAME: &str = "geo-ref_mapper";

/// The sizes written into the icon theme.
///
/// Every one is drawn rather than scaled from a single bitmap — see
/// [`crate::app_icon::rgba`]. 16 and 24 are left out: the mark is a photo frame
/// with a ridge and a sun inside it, and at 16 px that is a blue square.
pub const ICON_SIZES: &[u32] = &[32, 48, 64, 128, 256];

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Where a user-level binary goes. Not an XDG variable — there is no
/// `XDG_BIN_HOME` in the specification — but `~/.local/bin` is what
/// systemd's user profile and most distributions put on the PATH.
pub fn bin_dir() -> Option<PathBuf> {
    home().map(|home| home.join(".local/bin"))
}

pub fn installed_exe() -> Option<PathBuf> {
    bin_dir().map(|dir| dir.join(APP_FILE_NAME))
}

/// `$XDG_DATA_HOME/icons`, falling back to the specified default.
pub fn icons_dir() -> Option<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(data) if !data.is_empty() => Some(PathBuf::from(data).join("icons")),
        _ => home().map(|home| home.join(".local/share/icons")),
    }
}

/// Where one size of the icon lives in the hicolor theme.
pub fn icon_path(size: u32) -> Option<PathBuf> {
    icons_dir().map(|dir| {
        dir.join(format!("hicolor/{size}x{size}/apps"))
            .join(format!("{APP_FILE_NAME}.png"))
    })
}

/// Whether the binary is sitting in `~/.local/bin`.
pub fn is_installed() -> bool {
    installed_exe().is_some_and(|path| path.exists())
}

/// Whether the copy running right now *is* the installed one.
///
/// Both sides are canonicalised: `~/.local/bin` is very often a symlink, and
/// comparing the paths as written would call the same file two different
/// things — which is the difference between refreshing an install and
/// [`std::fs::copy`] truncating the running program onto itself.
pub fn running_from_install() -> bool {
    let (Ok(current), Some(installed)) = (std::env::current_exe(), installed_exe()) else {
        return false;
    };
    match (current.canonicalize(), installed.canonicalize()) {
        (Ok(current), Ok(installed)) => current == installed,
        _ => false,
    }
}

/// When the installed copy was put there, in seconds since the Unix epoch.
///
/// The file's own modification time, which `fs::copy` sets to the moment of the
/// copy. That is the *install* time rather than the build time of whatever was
/// installed — but install time is always at or after the build time of the
/// binary it put there, which is exactly what makes the comparison below sound.
pub fn installed_at() -> Option<u64> {
    let modified = std::fs::metadata(installed_exe()?).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

/// Whether the copy running now is newer than the one installed.
///
/// True when this binary was built *after* the install happened. Since the
/// install cannot have copied a binary built after itself, a later build time
/// means the installed copy cannot be this build — so it is out of date and
/// reinstalling is worth offering.
///
/// The reverse is deliberately not reported. An installed copy newer than the
/// running one is an ordinary thing to see — running an old build out of a
/// stale `target/` — and it is not a problem to fix, so it does not get a
/// warning.
pub fn update_available() -> bool {
    let (Some(installed), running) = (installed_at(), crate::version::build_unix()) else {
        return false;
    };
    // A build with no usable clock stamps zero and never claims to be newer.
    running > 0 && running > installed
}

/// What, if anything, is worth asking about at startup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prompt {
    /// Never installed. Offers to.
    NotInstalled,
    /// Installed, but this build is newer than what is installed.
    Stale,
}

/// The question to put at startup, given the build the user last said no to.
///
/// Nothing is asked when the installed copy *is* the one running: there is
/// nothing to offer someone already running what they installed.
///
/// `declined` is a build stamp rather than a plain "do not ask" flag, and that
/// is the whole of the nagging policy. Saying no silences this build and only
/// this build; the next one asks again — which is right, because the next one
/// is a different answer to a different question. A permanent flag would mean
/// one "no" during a morning's work hid every update afterwards.
pub fn prompt_at_startup(declined: Option<u64>) -> Option<Prompt> {
    if !crate::desktop::supported() || running_from_install() {
        return None;
    }
    let build = crate::version::build_unix();
    if build > 0 && declined == Some(build) {
        return None;
    }
    if !is_installed() {
        Some(Prompt::NotInstalled)
    } else if update_available() {
        Some(Prompt::Stale)
    } else {
        None
    }
}

/// Whether `~/.local/bin` is somewhere the shell will look.
///
/// Reported rather than fixed. Writing to `.profile` or `.bashrc` is editing
/// something the user owns and may generate from elsewhere, and the install is
/// perfectly usable from the menu without it — the entry carries an absolute
/// path. Only typing `geo-ref_mapper` at a prompt needs this.
pub fn on_path() -> bool {
    let Some(dir) = bin_dir() else {
        return false;
    };
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    // Compared canonically for the same reason as `running_from_install`, with
    // a plain comparison as the fallback for entries that do not exist.
    let target = dir.canonicalize().ok();
    std::env::split_paths(&path).any(|entry| match (&target, entry.canonicalize()) {
        (Some(target), Ok(entry)) => entry == *target,
        _ => entry == dir,
    })
}

/// What an install did, so the panel can say it rather than just "done".
pub struct Report {
    pub exe: PathBuf,
    pub icons: usize,
    /// The binary was already the one running, so only the icons and the
    /// launcher entry were rewritten.
    pub refreshed: bool,
    pub on_path: bool,
}

/// Copies the running binary into `~/.local/bin`, writes the icons, and points
/// the launcher entry at the installed copy.
pub fn install() -> io::Result<Report> {
    if !crate::desktop::supported() {
        return Err(io::Error::other(
            "installing is only implemented for Linux desktops",
        ));
    }

    let current = std::env::current_exe()?;
    let target = installed_exe().ok_or_else(|| io::Error::other("no HOME to install into"))?;
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::other("no HOME to install into"))?;
    std::fs::create_dir_all(dir)?;

    // Skipped when the running program is already the target. `fs::copy` opens
    // the destination truncating, so copying a file onto itself would leave
    // nothing behind — an "install" that deleted the program.
    let refreshed = running_from_install();
    if !refreshed {
        copy_binary(&current, &target)?;
    }

    let icons = write_icons()?;

    // Always repointed at the installed copy: the entry may have been written
    // by the default-viewer button while running out of `target/release`, and
    // that path stops working the moment the build directory is cleaned.
    crate::desktop::write_desktop_entry(&target)?;
    refresh_caches();

    Ok(Report {
        exe: target,
        icons,
        refreshed,
        on_path: on_path(),
    })
}

/// Writes `from` to `to` even when `to` is a program that is running.
///
/// Straight into place, Linux returns `ETXTBSY` for a running executable. The
/// way round it is the one `convert` uses for maps: write a new file beside the
/// target and rename on top, which replaces the *name* and leaves the running
/// process holding the old inode until it exits.
fn copy_binary(from: &Path, to: &Path) -> io::Result<()> {
    let staging = to.with_extension("part");
    // A leftover from an install that died half way; it is ours, named after
    // our own target, so replacing it is safe.
    let _ = std::fs::remove_file(&staging);
    std::fs::copy(from, &staging)?;
    permit_execute(&staging)?;
    match std::fs::rename(&staging, to) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&staging);
            Err(error)
        }
    }
}

#[cfg(unix)]
fn permit_execute(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    // `fs::copy` carries the mode over already; set it anyway, since a build
    // artefact fetched from somewhere else may arrive without it and a launcher
    // entry pointing at a non-executable file fails silently.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn permit_execute(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Renders the mark into the hicolor theme at every size. Returns how many were
/// written.
fn write_icons() -> io::Result<usize> {
    let mut written = 0;
    for size in ICON_SIZES {
        let Some(path) = icon_path(*size) else {
            continue;
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, png(*size)?)?;
        written += 1;
    }
    Ok(written)
}

/// The mark at `size`, PNG-encoded.
fn png(size: u32) -> io::Result<Vec<u8>> {
    let rgba = crate::app_icon::rgba(size as usize);
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&rgba, size, size, image::ExtendedColorType::Rgba8)
        .map_err(io::Error::other)?;
    Ok(out)
}

/// Takes the binary, the icons and the launcher entry back out.
///
/// The MIME defaults go too, through [`crate::desktop::remove`]: they name a
/// desktop id whose file is about to stop existing, and leaving them behind
/// gives a system whose default image viewer is nothing at all.
pub fn uninstall() -> io::Result<()> {
    // The binary and the icons go first, and the launcher entry last.
    //
    // The order matters and is not the intuitive one. `desktop::remove` keeps
    // the entry when the program is installed — giving up the file
    // associations should not un-list it from the menu — so calling it while
    // the binary is still there would rewrite the entry rather than delete it,
    // and the menu would keep an item pointing at nothing.
    for size in ICON_SIZES {
        if let Some(path) = icon_path(*size)
            && path.exists()
        {
            std::fs::remove_file(&path)?;
        }
    }

    if let Some(exe) = installed_exe()
        && exe.exists()
    {
        // Removing the running program is allowed here — the name goes and the
        // inode stays alive until this process exits, so the window carries on
        // working. It just will not start again.
        std::fs::remove_file(&exe)?;
    }

    // Now that nothing is installed, this takes the entry away as well as the
    // MIME defaults — which have to go too, since they name a desktop id that
    // has just stopped existing.
    crate::desktop::remove()?;
    refresh_caches();
    Ok(())
}

/// Tells the desktop to re-read what we just wrote.
///
/// Best effort throughout: both tools are optional, and a desktop without them
/// picks the change up on its own eventually. A failure here means the entry
/// appears a little later, which is not worth failing an install over.
fn refresh_caches() {
    if let Some(dir) = crate::desktop::applications_dir() {
        run_quietly(std::process::Command::new("update-desktop-database").arg(&dir));
    }

    // Only where the theme has an `index.theme`. Without one there is no cache
    // to rebuild — gtk-update-icon-cache refuses, and rightly — and the icons
    // are still found by walking the directories, which is what every desktop
    // does when no cache is present. Writing an index ourselves would mean
    // putting a file into a directory shared with every other application's
    // icons to fix something that is not broken.
    if let Some(theme) = icons_dir().map(|dir| dir.join("hicolor"))
        && theme.join("index.theme").exists()
    {
        run_quietly(std::process::Command::new("gtk-update-icon-cache").arg(&theme));
    }
}

/// Runs a cache refresher, discarding whatever it says.
///
/// `output()` rather than `status()` so its stdout and stderr are captured
/// instead of landing on our terminal: these are best-effort niceties, and a
/// tool grumbling about a theme index is not something to show for an install
/// that worked.
fn run_quietly(command: &mut std::process::Command) {
    let _ = command.output();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_drawn_square_and_opaque_in_the_middle() {
        for size in ICON_SIZES {
            let pixels = crate::app_icon::rgba(*size as usize);
            assert_eq!(pixels.len(), (*size as usize).pow(2) * 4, "{size}");
            // The centre of the mark is inside the rounded rectangle at every
            // size, so a fully transparent middle would mean the unit
            // coordinates had stopped tracking the size.
            let middle = ((*size as usize / 2) * *size as usize + *size as usize / 2) * 4;
            assert_eq!(
                pixels[middle + 3],
                255,
                "{size} is transparent in the middle"
            );
        }
    }

    #[test]
    fn every_size_encodes_to_a_png() {
        for size in ICON_SIZES {
            let bytes = png(*size).expect("encodes");
            // The PNG signature, so this is checking the file rather than just
            // that the call returned.
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{size}");
        }
    }

    #[test]
    fn the_icon_name_matches_the_entry_that_asks_for_it() {
        // The desktop entry says `Icon=geo-ref_mapper` and the file is written to
        // `.../apps/geo-ref_mapper.png`. They are the same string or the launcher
        // shows a blank tile, which is exactly the failure this whole module
        // exists to avoid — and it is silent.
        let entry = crate::desktop::desktop_entry(Path::new("/usr/bin/geo-ref_mapper"));
        assert!(
            entry.contains(&format!("\nIcon={APP_FILE_NAME}\n")),
            "{entry}"
        );
        let path = icon_path(128).expect("a HOME in the test environment");
        assert!(path.ends_with(format!("{APP_FILE_NAME}.png")), "{path:?}");
    }

    /// The real thing, against a throwaway `$HOME`.
    ///
    /// Ignored because it moves `HOME` and `XDG_DATA_HOME` out from under the
    /// whole process, so it cannot run beside tests that read either. Run it
    /// on its own:
    ///
    /// ```text
    /// cargo test install_round_trip -- --ignored --test-threads=1
    /// ```
    #[test]
    #[ignore = "moves HOME; run with --test-threads=1"]
    fn install_round_trip() {
        let root =
            std::env::temp_dir().join(format!("geo-ref_mapper-install-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a temp dir");

        // SAFETY: single-threaded by the `--test-threads=1` this test is run
        // with, and nothing else in the process reads these while it runs.
        unsafe {
            std::env::set_var("HOME", &root);
            std::env::set_var("XDG_DATA_HOME", root.join(".local/share"));
            std::env::set_var("XDG_CONFIG_HOME", root.join(".config"));
        }

        assert!(!is_installed(), "nothing installed in a fresh HOME");

        // Nothing installed, and this build not turned down: the startup
        // dialog offers to install.
        assert_eq!(prompt_at_startup(None), Some(Prompt::NotInstalled));
        assert_eq!(
            prompt_at_startup(Some(crate::version::build_unix())),
            None,
            "a build that was turned down does not ask again"
        );

        let report = install().expect("installs");
        assert!(is_installed());

        // Now installed *from this build*, so there is nothing to update and
        // nothing to ask. The file was written a moment ago, so its timestamp
        // is at or after this build's.
        assert!(!update_available(), "just installed from this very build");
        assert_eq!(prompt_at_startup(None), None);
        assert_eq!(report.icons, ICON_SIZES.len());
        assert!(report.exe.exists(), "the binary was copied");
        assert!(!report.refreshed, "this is a first install, not a refresh");

        // The binary has to be runnable, or the menu entry does nothing.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&report.exe)
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "not executable: {mode:o}");
        }
        // And no staging file left lying about beside it.
        assert!(!report.exe.with_extension("part").exists());

        let keep = std::env::var_os("GEO_REF_MAPPER_KEEP_INSTALL_TEST").is_some();
        for size in ICON_SIZES {
            let icon = icon_path(*size).expect("a path");
            let bytes = std::fs::read(&icon).unwrap_or_else(|_| panic!("{icon:?} missing"));
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{size}");
            // Put aside before the uninstall below deletes them, so they can
            // be looked at afterwards.
            if keep {
                std::fs::write(root.join(format!("icon-{size}.png")), &bytes).expect("kept");
            }
        }

        // The entry the menu reads: it must point at the *installed* copy, not
        // at the test binary that did the installing.
        let entry_path = crate::desktop::desktop_file().expect("a path");
        let entry = std::fs::read_to_string(&entry_path).expect("the entry");
        assert!(
            entry.contains(&format!("Exec=\"{}\" %f", report.exe.display())),
            "{entry}"
        );
        assert!(
            entry.contains(&format!("\nIcon={APP_FILE_NAME}\n")),
            "{entry}"
        );

        // Giving up the file associations must leave the install alone — the
        // two buttons share this file and are not the same decision.
        crate::desktop::install().expect("claims the types");
        assert!(crate::desktop::registered_count() > 0);
        crate::desktop::remove().expect("gives them back");
        assert_eq!(crate::desktop::registered_count(), 0);
        assert!(entry_path.exists(), "the menu entry survived");
        assert!(is_installed(), "and so did the install");

        uninstall().expect("uninstalls");
        assert!(!is_installed());
        assert!(!entry_path.exists(), "the menu entry went with it");
        for size in ICON_SIZES {
            assert!(!icon_path(*size).expect("a path").exists(), "{size}");
        }

        // Kept when asked, so the icons it wrote can be looked at rather than
        // only asserted about — a PNG with the right signature and the wrong
        // picture in it passes every check above.
        if keep {
            eprintln!("icons left in {}", root.display());
        } else {
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// `update_available` without touching the filesystem: the rule is a
    /// comparison, and it is the rule that is worth pinning.
    fn newer_than_install(running: u64, installed: u64) -> bool {
        running > 0 && running > installed
    }

    #[test]
    fn only_a_build_made_after_the_install_counts_as_newer() {
        // Installed at 10:05 from a build made at 09:00.
        let install = 1_000_500;

        // A build made afterwards: the installed copy cannot be this one.
        assert!(newer_than_install(install + 1, install));
        // The build that was installed, run from wherever it was built. The
        // install happened after it, so its stamp is older — and there is
        // nothing to update.
        assert!(!newer_than_install(1_000_000, install));
        // An installed copy newer than the running one is ordinary — an old
        // build out of a stale target directory — and is not reported.
        assert!(!newer_than_install(install - 50_000, install));
        // Exactly equal cannot happen through a real install, but off-by-one
        // here would nag forever.
        assert!(!newer_than_install(install, install));
        // A build with no usable clock never claims anything.
        assert!(!newer_than_install(0, install));
    }

    #[test]
    fn saying_no_silences_this_build_and_not_the_next() {
        // The policy `prompt_at_startup` applies to `declined`, checked
        // directly so it does not depend on what is installed on this machine.
        let build = 1_234_567_u64;
        let silenced = |declined: Option<u64>| build > 0 && declined == Some(build);

        assert!(silenced(Some(build)), "this build was turned down");
        assert!(!silenced(None), "nothing turned down yet");
        assert!(
            !silenced(Some(build - 1)),
            "an older build was turned down, not this one — so this one asks"
        );
    }

    #[test]
    fn the_staging_file_sits_beside_the_target_rather_than_in_a_temp_dir() {
        // The rename has to be within one filesystem to be atomic, and `/tmp`
        // is very often a different one.
        let target = Path::new("/home/someone/.local/bin/geo-ref_mapper");
        assert_eq!(
            target.with_extension("part").parent(),
            target.parent(),
            "staging must be beside the target"
        );
    }
}
