use eframe::egui::{self, Frame, Margin, Panel, Ui};

use std::path::{Path, PathBuf};

use crate::cache::{TextureCache, ThumbCache};
use crate::db::{Library, LibraryStats};
use crate::files::{FileTree, Kind};
use crate::loader::{DecodeResult, Loader, Priority, Stage};
use crate::metrics::Metrics;
use crate::panels::{FILES_WIDTH, INFO_WIDTH};
use crate::scan::{Mode, ScanState};
use crate::thumbs::{ThumbResult, Thumbnailer};
use crate::viewport::Viewport;
use crate::{chrome, pages, panels, theme};

/// Both caches hold GPU textures, so these come out of the same VRAM. They are
/// budgeted separately because they behave differently: full-resolution images
/// are few and enormous, thumbnails are many and small, and one should not be
/// able to starve the other.
const DEFAULT_IMAGE_VRAM_MB: usize = 1500;
const DEFAULT_THUMBNAIL_VRAM_MB: usize = 384;

const MB: usize = 1024 * 1024;

/// The scale a held slider should be committed at, if it has been let go of.
///
/// The condition is the pointer being *up*, not the slider having changed:
/// scaling on every change is what makes the handle run away from the pointer
/// dragging it, and that is the whole reason the value is held at all.
///
/// Written against the pointer rather than the slider's own drag state so a
/// release the slider never sees — off the track, off the window, or on a frame
/// where the Config page is not being drawn — still lands.
fn release_ui_scale(pending: Option<f32>, pointer_down: bool) -> Option<f32> {
    pending.filter(|_| !pointer_down)
}

/// Which of the two ways of changing the interface scale had the last word.
///
/// `live` is what egui reports now, `applied` what we last asked it for, and
/// `config` what the setting says. A `live` that has drifted from `applied` can
/// only be egui's own Ctrl+plus / Ctrl+minus / Ctrl+0, so that wins and becomes
/// the setting; otherwise the setting wins and is pushed back into egui.
///
/// Split out from `apply_ui_scale` because it is the whole of the decision and
/// none of the plumbing, and getting it the wrong way round gives a slider that
/// snaps back or a keyboard shortcut that does not take.
fn resolve_ui_scale(live: f32, applied: f32, config: f32) -> f32 {
    // Clamped either way: egui's shortcuts run to 10×, which is well past the
    // point where this window has room to draw itself, and a hand-edited row
    // could hold anything at all.
    let chosen = if (live - applied).abs() > f32::EPSILON {
        live
    } else {
        config
    };
    if chosen.is_finite() {
        chosen.clamp(MIN_UI_SCALE, MAX_UI_SCALE)
    } else {
        1.0
    }
}

/// The range the interface can be scaled over.
///
/// Below 0.8 the small-caps rows stop being legible at all.
///
/// The ceiling is 1.4 because this scale is **on top of** whatever the desktop
/// already applies. A 4K screen at 200 % is 2× before the program says a word,
/// so 1.4 here is 2.8× device pixels — and at that point a 3072 px title bar is
/// down to 550 points, with the FILES tree and the INFO panel taking most of
/// what is left of the window. Past that the picture, which is the entire
/// point, has nowhere to go.
///
/// A saved value above the ceiling is clamped on the way in, so lowering this
/// takes effect on the next start rather than stranding anyone.
pub const MIN_UI_SCALE: f32 = 0.8;
pub const MAX_UI_SCALE: f32 = 1.4;

/// Floors for the halving that follows a refused texture.
///
/// Below these there is not enough room to hold one picture and the thumbnails
/// beside it, so shrinking further would trade a crash for a viewer that
/// cannot show anything — and a card that cannot spare this much is one to
/// leave, which is what the renderer probe is for.
const MINIMUM_IMAGE_VRAM_MB: usize = 128;
const MINIMUM_THUMBNAIL_VRAM_MB: usize = 48;

/// Video memory the viewer is holding, and where it went.
///
/// Only the first two fields have budgets. The rest are bounded by what is on
/// screen and by how many maps have been faded up, which on the map page is
/// the larger number by far — so a breakdown is the only way to tell a viewer
/// that is using its budget from one that is using ten times it.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct TexturesHeld {
    /// Full-resolution pictures, against `image_vram_mb`.
    pub images: usize,
    /// Both thumbnail caches, against `thumbnail_vram_mb`.
    pub thumbnails: usize,
    /// Squares of every layered picture in hand, the map overlays included.
    pub squares: usize,
    /// Basemap tiles.
    pub basemap: usize,
    /// Stretched whole copies of maps that have no squares to read.
    pub overlays: usize,
    /// The sharper patch drawn over the picture when zoomed in.
    pub detail: usize,
}

impl TexturesHeld {
    pub fn total(self) -> usize {
        self.images + self.thumbnails + self.squares + self.basemap + self.overlays + self.detail
    }

    /// What is held outside any budget — the part the two configured numbers
    /// do not describe.
    pub fn unbudgeted(self) -> usize {
        self.squares + self.basemap + self.overlays + self.detail
    }
}

impl std::fmt::Display for TexturesHeld {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let size = |bytes: usize| crate::files::format_bytes(bytes as u64);
        write!(
            formatter,
            "{} images, {} thumbnails, {} squares, {} basemap, {} overlays, {} detail",
            size(self.images),
            size(self.thumbnails),
            size(self.squares),
            size(self.basemap),
            size(self.overlays),
            size(self.detail),
        )
    }
}

pub const SETTING_LAST_SELECTION: &str = "last_selection";
pub const SETTING_WINDOW: &str = "window_geometry";
pub const SETTING_INFO_TAB: &str = "info_tab";
pub const SETTING_SECTIONS: &str = "info_sections";
pub const SETTING_BOOKMARKS: &str = "bookmarks";
pub const SETTING_CONFIG: &str = "config";
/// The build stamp the user last declined to install, so that build stops
/// asking and the next one still can. See [`crate::install::prompt_at_startup`].
pub const SETTING_INSTALL_DECLINED: &str = "install_declined";

/// Encodes the collapsed groups as `TITLE=1,TITLE=0`.
pub fn encode_sections<S: AsRef<str>>(states: &[(S, bool)]) -> String {
    states
        .iter()
        .map(|(title, open)| format!("{}={}", title.as_ref(), u8::from(*open)))
        .collect::<Vec<_>>()
        .join(",")
}

/// Reads back what [`encode_sections`] wrote, ignoring anything unrecognised so
/// a stale entry from an older build cannot break startup.
pub fn decode_sections(value: &str) -> Vec<(String, bool)> {
    value
        .split(',')
        .filter_map(|entry| {
            let (title, flag) = entry.rsplit_once('=')?;
            Some((title.to_owned(), flag.trim() == "1"))
        })
        .collect()
}

impl Config {
    /// `key=value` pairs, one per setting.
    ///
    /// Written by hand rather than derived, because the point of the format is
    /// that it survives the struct changing: a build that adds a setting reads
    /// an older row and keeps the default for the one it does not find, and a
    /// build that removes one ignores the leftover. A serialiser that failed
    /// the whole row on an unknown key would silently reset every preference
    /// on each upgrade.
    pub fn encode(&self) -> String {
        let flags = [
            ("preload_neighbours", self.preload_neighbours),
            ("use_gpu_decode", self.use_gpu_decode),
            ("background_thumbs", self.background_thumbs),
            ("recursive_index", self.recursive_index),
            ("checkerboard", self.checkerboard),
            ("show_filmstrip", self.show_filmstrip),
            ("mosaic_recursive", self.mosaic_recursive),
            ("show_stacks", self.show_stacks),
            ("mosaic_filenames", self.mosaic_filenames),
            ("show_debug_tab", self.show_debug_tab),
            ("anchors_locked", self.anchors_locked),
            ("map_support", self.map_support),
        ];
        let mut parts: Vec<String> = flags
            .iter()
            .map(|(key, value)| format!("{key}={}", u8::from(*value)))
            .collect();
        parts.push(format!("decode_threads={}", self.decode_threads));
        parts.push(format!("thumbnail_threads={}", self.thumbnail_threads));
        // Last, and comma-free by construction: an address with a comma in it
        // would otherwise split into two settings.
        parts.push(format!(
            "tile_contact={}",
            self.tile_contact.replace(',', " ")
        ));
        // A folder can legitimately contain a comma, and the separator here is
        // a comma — so it is taken out. Anyone who names a folder with one gets
        // a space instead, which is better than losing every setting after it.
        parts.push(format!(
            "map_export_root={}",
            self.map_export_root.replace(',', " ")
        ));
        parts.push(format!(
            "manifest_path={}",
            self.manifest_path.replace(',', " ")
        ));
        parts.push(format!(
            "auto_convert_maps={}",
            u8::from(self.auto_convert_maps)
        ));
        parts.push(format!(
            "mosaic_selected_only={}",
            u8::from(self.mosaic_selected_only)
        ));
        // Two decimals: the slider steps in twentieths, and a bare `{}` wrote
        // 1.7000000000000002 into the row for one of them.
        parts.push(format!("ui_scale={:.2}", self.ui_scale));
        parts.push(format!("image_vram_mb={}", self.image_vram_mb));
        parts.push(format!("thumbnail_vram_mb={}", self.thumbnail_vram_mb));
        parts.join(",")
    }

    /// Applies whatever of `value` is recognised, leaving the rest at default.
    pub fn apply(&mut self, value: &str) {
        for entry in value.split(',') {
            let Some((key, raw)) = entry.split_once('=') else {
                continue;
            };
            let flag = raw.trim() == "1";
            match key.trim() {
                "preload_neighbours" => self.preload_neighbours = flag,
                "use_gpu_decode" => self.use_gpu_decode = flag,
                "background_thumbs" => self.background_thumbs = flag,
                "recursive_index" => self.recursive_index = flag,
                "checkerboard" => self.checkerboard = flag,
                "show_filmstrip" => self.show_filmstrip = flag,
                "mosaic_recursive" => self.mosaic_recursive = flag,
                "show_stacks" => self.show_stacks = flag,
                "tile_contact" => self.tile_contact = raw.trim().to_owned(),
                "map_export_root" => self.map_export_root = raw.trim().to_owned(),
                "manifest_path" => self.manifest_path = raw.trim().to_owned(),
                "mosaic_filenames" => self.mosaic_filenames = flag,
                "show_debug_tab" => self.show_debug_tab = flag,
                "anchors_locked" => self.anchors_locked = flag,
                "map_support" => self.map_support = flag,
                "mosaic_selected_only" => self.mosaic_selected_only = flag,
                "auto_convert_maps" => self.auto_convert_maps = flag,
                // Clamped to the same ranges the sliders offer, so a hand-edited
                // or stale row cannot ask for a 40 GB texture budget.
                "decode_threads" => {
                    if let Ok(threads) = raw.trim().parse::<usize>() {
                        self.decode_threads = threads.clamp(1, 64);
                    }
                }
                "thumbnail_threads" => {
                    if let Ok(threads) = raw.trim().parse::<usize>() {
                        self.thumbnail_threads = threads.clamp(1, crate::thumbs::MAX_THUMB_WORKERS);
                    }
                }
                // Clamped like the sliders below, and a value that is not a
                // number at all is left alone rather than becoming zero — a
                // zero here would scale the whole interface out of existence.
                "ui_scale" => {
                    if let Ok(scale) = raw.trim().parse::<f32>()
                        && scale.is_finite()
                    {
                        self.ui_scale = scale.clamp(MIN_UI_SCALE, MAX_UI_SCALE);
                    }
                }
                "image_vram_mb" => {
                    if let Ok(mb) = raw.trim().parse::<usize>() {
                        self.image_vram_mb = mb.clamp(128, 8192);
                    }
                }
                "thumbnail_vram_mb" => {
                    if let Ok(mb) = raw.trim().parse::<usize>() {
                        self.thumbnail_vram_mb = mb.clamp(64, 4096);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Parses `x,y,width,height`, rejecting anything implausible so a stale or
/// corrupt value cannot open the window off-screen or at zero size.
/// A path's filename, falling back to the whole path when it has none.
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The window rect in the units `main` restores it in.
///
/// egui reports the viewport in points it has already divided the interface
/// scale out of; `main` hands the saved size to winit's `with_inner_size`,
/// which knows only the display's own scale factor. Multiplying the scale back
/// in is what makes a window left at 200% come back the size it was left,
/// rather than half it — and half that again on the restart after.
pub fn geometry_to_save(rect: egui::Rect, ui_scale: f32) -> (f32, f32, f32, f32) {
    (
        rect.min.x * ui_scale,
        rect.min.y * ui_scale,
        rect.width() * ui_scale,
        rect.height() * ui_scale,
    )
}

pub fn parse_geometry(value: &str) -> Option<(f32, f32, f32, f32)> {
    let numbers: Vec<f32> = value
        .split(',')
        .filter_map(|part| part.trim().parse::<f32>().ok())
        .collect();
    let [x, y, width, height] = numbers[..] else {
        return None;
    };
    if !(200.0..=32_000.0).contains(&width) || !(150.0..=32_000.0).contains(&height) {
        return None;
    }
    if !(-16_000.0..=32_000.0).contains(&x) || !(-16_000.0..=32_000.0).contains(&y) {
        return None;
    }
    Some((x, y, width, height))
}

/// `1 image` / `12 images`, so the counts in messages read as English.
/// Removes what a failed conversion left behind — and nothing else.
///
/// A half-written file is worse than none, because next time it reads as a
/// finished conversion. But a destination that was *already there* when the
/// conversion started was written by something else, and deleting it turns one
/// failed map into two.
///
/// That is not hypothetical. Two maps in the same folder differing only by
/// extension — `Plan of Singapore town - 1893 - A.png` and `.xcf` — now share
/// one `.tif` destination. The `.png` converted, the `.xcf` was refused for
/// landing on an existing file, and the refusal deleted the good `.png` output.
pub fn discard_failed_output(target: &std::path::Path, ours: bool) {
    if !ours {
        return;
    }
    let _ = std::fs::remove_file(target);
}

pub fn plural_images(count: usize) -> String {
    if count == 1 {
        "1 image".to_owned()
    } else {
        format!("{count} images")
    }
}

/// What a tag is doing in the filter.
///
/// Three states rather than two switches: a tag is required, refused, or not
/// being asked about, and those are one question with three answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TagState {
    #[default]
    Off,
    /// Every picture must carry it.
    Include,
    /// No picture may carry it.
    Exclude,
}

impl TagState {
    /// The next state when the tag is clicked.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Include,
            Self::Include => Self::Exclude,
            Self::Exclude => Self::Off,
        }
    }
}

/// The part of the window the keyboard is aimed at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    /// The FILES tree: arrows walk rows and open folders.
    Tree,
    /// The picture and its rail: arrows step through images.
    Images,
    /// The Mosaic grid: arrows move a tile at a time, left and right included,
    /// because a grid has two dimensions and stepping along the row is what
    /// those keys mean in one.
    Grid,
}

/// Every indexed picture, looked up either way round.
///
/// One walk of the tree per library load, rather than one per question.
#[derive(Default)]
pub struct ImageIndex {
    by_path: std::collections::HashMap<PathBuf, i64>,
    by_id: std::collections::HashMap<i64, (PathBuf, crate::files::ImageMeta)>,
}

impl ImageIndex {
    fn of(files: &FileTree) -> Self {
        fn walk(nodes: &[crate::files::Node], index: &mut ImageIndex) {
            for node in nodes {
                match &node.kind {
                    crate::files::Kind::Image(meta) => {
                        if let (Some(id), Some(path)) = (meta.db_id, meta.path.as_ref()) {
                            let path = PathBuf::from(path);
                            index.by_path.insert(path.clone(), id);
                            index.by_id.insert(id, (path, meta.clone()));
                        }
                    }
                    crate::files::Kind::Folder => walk(&node.children, index),
                }
            }
        }
        let mut index = Self::default();
        walk(&files.roots, &mut index);
        index
    }

    pub fn id_of(&self, path: &Path) -> Option<i64> {
        self.by_path.get(path).copied()
    }

    pub fn path_of(&self, id: i64) -> Option<&PathBuf> {
        self.by_id.get(&id).map(|(path, _)| path)
    }

    pub fn meta_of(&self, id: i64) -> Option<&(PathBuf, crate::files::ImageMeta)> {
        self.by_id.get(&id)
    }
}

pub const APP_NAME: &str = "geo-ref_mapper";
/// Placing a picture on the earth by marking the same places on both.
///
/// Held apart from the library until it is finished: half-placed anchors are a
/// working state, not a fact about the picture, and abandoning the job should
/// leave nothing behind.
pub struct Placing {
    pub path: std::path::PathBuf,
    pub image_id: Option<i64>,
    /// Full-size dimensions of the picture, for turning a click into a pixel.
    pub size: (u32, u32),
    /// Anchor id to where it falls on the picture.
    pub placed: std::collections::HashMap<i64, (f64, f64)>,
    /// The anchor being placed at the moment.
    pub choosing: Option<i64>,
    /// Keep the two halves over the same ground, at the same size.
    ///
    /// Only possible once two anchors are down, which is the point at which the
    /// pair of them fixes a scale and a turn and every other pixel of the
    /// picture has a place on the earth implied for it. Held as a wish rather
    /// than a fact: it stays set while the first anchor is being placed and
    /// simply does nothing until there is something to lock to.
    pub locked: bool,
    /// Turn the scan so its north is up, matching the basemap beside it.
    ///
    /// On by default. Off is for reading the sheet itself — a title block set
    /// at forty degrees is a good deal harder to read than one left alone.
    pub upright: bool,
    /// The mark being dragged across the picture, if a drag started on one.
    ///
    /// Decided when the drag starts and held for its whole length, so a gesture
    /// cannot change its mind half way and leave a mark somewhere it was only
    /// dragged past.
    pub dragging_mark: Option<i64>,
    /// How the picture is drawn in its half: scale and offset.
    pub zoom: f32,
    pub offset: eframe::egui::Vec2,
    /// Squares of the picture, drawn the same way everything else is.
    pub squares: crate::imagetiles::Squares,
}

impl Placing {
    /// Which pixel of the picture is in the middle of its half.
    ///
    /// The picture is drawn centred on the half and then shifted by `offset`,
    /// so what is in the middle is the middle of the picture less however far
    /// it has been pushed — in pixels, which means dividing the shift by the
    /// zoom.
    pub fn centre_pixel(&self, turn: f64) -> (f64, f64) {
        centre_pixel_of(self.size, self.offset, self.zoom, turn)
    }

    /// Shifts the picture so a given pixel of it lands in the middle — the way
    /// back through [`centre_pixel`](Self::centre_pixel).
    pub fn centre_on_pixel(&mut self, pixel: (f64, f64), turn: f64) {
        self.offset = offset_centring(self.size, pixel, self.zoom, turn);
    }
}

/// The maths of the two above, apart from the session so it can be tested
/// without a window and a graphics device to hang a picture off.
///
/// `turn` is the angle the picture is drawn at. It matters because the shift is
/// measured on the *screen* while the answer is wanted in *picture* pixels, and
/// once the picture is turned those are no longer the same directions — pushing
/// a scan turned forty degrees to the right walks diagonally across its rows.
/// Left out, the two halves drift apart the moment either is panned.
fn centre_pixel_of(size: (u32, u32), offset: egui::Vec2, zoom: f32, turn: f64) -> (f64, f64) {
    let (sin, cos) = turn.sin_cos();
    let (dx, dy) = (f64::from(offset.x / zoom), f64::from(offset.y / zoom));
    (
        f64::from(size.0) / 2.0 - (dx * cos - dy * sin),
        f64::from(size.1) / 2.0 - (dx * sin + dy * cos),
    )
}

fn offset_centring(size: (u32, u32), pixel: (f64, f64), zoom: f32, turn: f64) -> egui::Vec2 {
    let (sin, cos) = turn.sin_cos();
    let (dx, dy) = (
        f64::from(size.0) / 2.0 - pixel.0,
        f64::from(size.1) / 2.0 - pixel.1,
    );
    egui::vec2(
        ((dx * cos + dy * sin) as f32) * zoom,
        ((-dx * sin + dy * cos) as f32) * zoom,
    )
}

/// How much of the anchor library the basemap should draw.
///
/// One choice of three rather than two switches, because "hide the unused ones"
/// and "hide all of them" are points on one scale — as two checkboxes they can
/// be set to disagree, and the reader has to work out which wins.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AnchorsShown {
    /// Every marked place, wherever it is.
    #[default]
    All,
    /// Only the ones this picture is matched by.
    Used,
    /// None: the map alone, for looking at the result.
    Hidden,
}

impl AnchorsShown {
    /// Whether anything is drawn at all.
    pub fn hidden(self) -> bool {
        matches!(self, Self::Hidden)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Used => "Used here",
            Self::Hidden => "None",
        }
    }

    pub fn why(self) -> &'static str {
        match self {
            Self::All => "Every marked place in the library.",
            Self::Used => "Only the anchors this picture is placed by.",
            Self::Hidden => {
                "The map and the picture alone — for judging the placement once \
                 it is written, with nothing on top of it."
            }
        }
    }
}

/// A running conversion, as the page sees it.
pub struct Conversion {
    events: std::sync::mpsc::Receiver<ConversionEvent>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub done: usize,
    pub total: usize,
    pub current: String,
    pub progress: Option<crate::convert::Progress>,
}

enum ConversionEvent {
    Started(String),
    Progress(crate::convert::Progress),
    Finished(Result<crate::convert::Report, String>),
    AllDone,
}

pub const PAGE_LABELS: &[&str] = &[
    "Viewer",
    "Mosaic",
    "Map",
    "Map Images",
    "Formats",
    "Export",
    "Config",
];

pub struct Config {
    pub preload_neighbours: bool,
    /// nvJPEG decoding. Off by default — measured slower than the CPU pool on
    /// a many-core machine; see `Thumbnailer::new`.
    pub use_gpu_decode: bool,
    pub decode_threads: usize,
    /// Workers building thumbnails. Fewer leaves more cores for the interface
    /// while a big folder is being swept; more finishes the sweep sooner.
    pub thumbnail_threads: usize,
    pub background_thumbs: bool,
    pub recursive_index: bool,
    pub checkerboard: bool,
    pub show_filmstrip: bool,
    /// Whether the Mosaic grid and the Viewer's filmstrip gather images from
    /// subfolders. On by default: the folders here mostly hold other folders —
    /// MAPS-NEW has 211 subfolders and no images of its own — so the off state
    /// opened to an empty grid. In the Viewer it is what carries the rail on
    /// into the next folder instead of stopping at the end of this one.
    ///
    /// Stored under its original `mosaic_` key so an existing saved config
    /// keeps working.
    pub mosaic_recursive: bool,
    /// Whether this is a map program as well as an image viewer.
    ///
    /// Off, everything about maps goes away together: the Map and Map Images
    /// pages, the INFO panel's MAP and IMAGES tabs, *Add to map images* in both
    /// right-click menus, the MAP TILES settings, the keys that only mean
    /// something on the map, and the georeference sweep that reads every file
    /// looking for a position. What is left is the image viewer.
    ///
    /// One switch rather than one per feature: they are all the same feature
    /// seen from different pages, and half of them switched off is a Map page
    /// with nothing to put on it.
    ///
    /// On by default, which is what the program did before the switch existed.
    pub map_support: bool,
    /// Contact details sent to the tile server, as its usage policy requires:
    /// an email address or a project URL. Left empty the map still tries, but
    /// an agent nobody can be reached at is what gets refused.
    pub tile_contact: String,
    /// Where converted maps are written. Never the folder they came from.
    pub map_export_root: String,
    /// The last selection indexed, so it can be re-run without hunting for it.
    pub manifest_path: String,
    /// Begin converting the moment maps are put on the queue.
    pub auto_convert_maps: bool,
    /// Show stacks as one tile. Off means every picture is loose, exactly as
    /// before stacks existed — which is also the way to get at a member
    /// without opening its stack.
    pub show_stacks: bool,
    /// Filename caption under each Mosaic tile. Off by default — the caption
    /// band covers the bottom of the picture, and the Mosaic exists to be
    /// looked at rather than read.
    pub mosaic_filenames: bool,
    /// The Mosaic shows only the marked pictures.
    pub mosaic_selected_only: bool,
    /// Whether anchors are pinned down against being dragged.
    ///
    /// On by default, and saved. An anchor is a hand-made measurement that
    /// several maps can be placed by, so nudging one silently moves every
    /// picture matched to it — and a pin is a small target sitting on a surface
    /// whose whole job is to be dragged about. The two together make an
    /// accident that is not noticed until a map is in the wrong place.
    ///
    /// It does not apply to an anchor being walked into position during
    /// placing: that one belongs to the picture in hand, nothing else can be
    /// hurt by it, and moving it is the task rather than a slip.
    pub anchors_locked: bool,
    /// Whether the INFO panel carries its DEBUG tab.
    ///
    /// Off by default. It reports on the process — frame times, decoder
    /// threads, VRAM — which is for working on the program rather than for
    /// looking at maps, and a tab that is never the answer is a tab in the way
    /// of the three that are.
    pub show_debug_tab: bool,

    /// How large the interface is drawn, as a multiplier. 1.0 is the size the
    /// program was designed at.
    ///
    /// This is egui's zoom factor, which is the *whole* interface and not the
    /// glyphs alone. It has to be: this window paints its own chrome to fixed
    /// point metrics — a 28 pt tab strip, a 40 pt title bar, a 32 pt pill — and
    /// text scaled on its own would be spilling out of all three well before
    /// 2.5. Everything moving together is what makes the top of the range
    /// usable rather than merely larger.
    ///
    /// The picture is not affected. The canvas keeps the physical pixels it had
    /// — there are simply fewer points across it — so a fitted image fills the
    /// same area of glass at every setting.
    pub ui_scale: f32,

    /// VRAM ceiling for full-resolution image textures, in MB.
    pub image_vram_mb: usize,
    /// VRAM ceiling for thumbnail textures, in MB.
    pub thumbnail_vram_mb: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            preload_neighbours: true,
            use_gpu_decode: false,
            // Clamped to what the loader will actually spawn, so the config
            // and the DEBUG readout never disagree.
            decode_threads: crate::loader::worker_count_for(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4),
            ),
            thumbnail_threads: crate::thumbs::default_worker_count(),
            background_thumbs: true,
            recursive_index: true,
            checkerboard: true,
            show_filmstrip: true,
            mosaic_recursive: true,
            show_stacks: true,
            map_support: true,
            tile_contact: String::new(),
            map_export_root: String::new(),
            manifest_path: String::new(),
            auto_convert_maps: false,
            mosaic_filenames: false,
            show_debug_tab: false,
            anchors_locked: true,
            mosaic_selected_only: false,
            ui_scale: 1.0,
            image_vram_mb: DEFAULT_IMAGE_VRAM_MB,
            thumbnail_vram_mb: DEFAULT_THUMBNAIL_VRAM_MB,
        }
    }
}

/// Everything the Export page is holding. Kept in one struct so the page can be
/// reasoned about without picking through the whole app.
pub struct ExportState {
    pub sources: Vec<PathBuf>,
    pub output_dir: Option<PathBuf>,
    pub recipe: crate::ops::Recipe,
    pub suffix: String,
    /// Deliberately not remembered between jobs: agreeing to overwrite once
    /// should not silently apply to the next batch.
    pub overwrite: bool,
    /// The plan, recomputed whenever the settings change.
    pub plan: Option<Result<Vec<crate::jobs::Planned>, crate::jobs::Refusal>>,
    pub runner: Option<crate::jobs::Runner>,
    pub last_result: Option<String>,
    /// Which resize control the user is driving, since they are mutually
    /// exclusive but each needs its own remembered value.
    pub resize_mode: usize,
    pub longest_edge: u32,
    pub percent: u32,
    pub fit_width: u32,
    pub fit_height: u32,
}

impl Default for ExportState {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            output_dir: None,
            recipe: crate::ops::Recipe::default(),
            suffix: String::new(),
            overwrite: false,
            plan: None,
            runner: None,
            last_result: None,
            resize_mode: 0,
            longest_edge: 2048,
            percent: 50,
            fit_width: 1920,
            fit_height: 1080,
        }
    }
}

impl ExportState {
    /// Turns the separate resize controls into the one the recipe wants.
    pub fn resize(&self) -> crate::ops::Resize {
        use crate::ops::Resize;
        match self.resize_mode {
            1 => Resize::LongestEdge(self.longest_edge),
            2 => Resize::Percent(self.percent),
            3 => Resize::Fit {
                width: self.fit_width,
                height: self.fit_height,
            },
            _ => Resize::None,
        }
    }

    /// The job as currently configured, if an output folder has been chosen.
    pub fn job(&self) -> Option<crate::jobs::Job> {
        let output_dir = self.output_dir.clone()?;
        let mut recipe = self.recipe.clone();
        recipe.resize = self.resize();
        let mut job = crate::jobs::Job::new(self.sources.clone(), output_dir, recipe);
        job.suffix = self.suffix.clone();
        job.overwrite = self.overwrite;
        Some(job)
    }

    /// Recomputes the plan. Cheap enough to do whenever a control moves, and
    /// doing so is what keeps the on-screen warning honest.
    pub fn replan(&mut self) {
        self.plan = self.job().map(|job| crate::jobs::plan(&job));
    }
}

pub struct GeoRefMapperApp {
    /// Index into [`PAGE_LABELS`].
    pub page: usize,
    /// Where Escape goes back to. One step only, deliberately: this is "undo
    /// the jump I just made", not a browser history, so a second Escape has
    /// nothing left to do.
    pub previous_page: Option<usize>,
    pub files_open: bool,
    pub info_open: bool,
    /// Index into [`panels::INFO_TABS`].
    pub info_tab: usize,
    pub files: FileTree,
    pub mosaic_tile_size: f32,
    /// Where the map is looking, and the tiles it has.
    pub map_view: crate::map::View,
    pub tiles: Option<crate::map::Tiles>,
    /// A georeferenced picture laid over the map, when one has been placed.
    pub map_overlay: Option<crate::map::Overlay>,
    /// Size of the map canvas at its last draw, so a picture can be fitted to
    /// it from a panel that cannot see it.
    pub map_window: eframe::egui::Vec2,
    /// Maps waiting to be converted, and which ids are on the queue.
    pub map_queue: Vec<crate::db::Queued>,
    pub map_queued_ids: std::collections::HashSet<i64>,
    /// The conversion currently running, if any.
    pub converting: Option<Conversion>,
    /// Running count from the background pass that fills in which files are
    /// georeferenced. `None` once it has finished.
    georeference_sweep: Option<std::sync::mpsc::Receiver<usize>>,
    /// Images that say where on the earth they are, by library id.
    ///
    /// Built once when the library is read rather than asked of the tree per
    /// tile per frame — the mosaic draws thousands of tiles and the answer only
    /// changes when the library does.
    pub georeferenced: std::collections::HashSet<i64>,
    /// Every indexed picture, both ways round.
    ///
    /// Built once when the library is loaded, not on demand: the stack code
    /// asks "which picture is this" for every visible row every frame, and
    /// rebuilding a map of 26,000 paths to answer each question cost 880 full
    /// walks of the tree in ten seconds.
    pub index: ImageIndex,
    /// Every stack, as the Mosaic needs to ask about them.
    pub stacks: crate::stacks::Stacks,
    /// The stack currently opened out, showing its members instead of the
    /// folder. Escape closes it.
    pub open_stack: Option<i64>,
    /// The folder the Mosaic is showing.
    ///
    /// Kept apart from the tree selection on purpose: clicking a tile selects
    /// that image so the Viewer and INFO panel follow it, but the grid behind
    /// it must stay exactly where it was, or Escape comes back to a different
    /// Mosaic than the one you left. The Viewer's rail deliberately does *not*
    /// use this — it follows the selection, so picking a file in the tree
    /// re-roots the rail on that file's folder.
    pub mosaic_root: Option<crate::files::NodePath>,
    /// The Mosaic's tiles in the order they are laid out, and how many fit
    /// across — what the arrow keys walk when the grid has the keyboard.
    pub mosaic_order: Vec<crate::files::NodePath>,
    pub mosaic_columns: usize,
    /// The largest useful tile for the current window — "one fills the pane".
    /// Written by the Mosaic page and read by the title-bar slider, which has
    /// no way to know the size of a pane it is not drawing.
    pub mosaic_max_tile: f32,
    pub config: Config,

    pub library: Option<Library>,
    pub library_error: Option<String>,
    pub stats: LibraryStats,
    pub scan: ScanState,
    pub metrics: Metrics,

    pub loader: Loader,
    pub textures: TextureCache,
    pub viewport: Viewport,
    /// The image the Viewer page is showing, if the selection names a real file.
    pub current: Option<PathBuf>,
    /// What that file actually is, sniffed once when the selection moves rather
    /// than every frame.
    pub current_format: Option<crate::formats::Description>,
    /// Its EXIF, read at the same time. `None` means the file carries none,
    /// which is normal for PNG and for anything a screenshot produced.
    pub current_exif: Option<crate::exif::Exif>,
    /// XMP, IPTC, ICC and free-text blocks for the same file.
    pub current_meta: crate::meta::Metadata,
    /// What the sidecar beside the picture says, when there is one.
    ///
    /// A source map keeps its archive record and mapSelector's tags in a
    /// `.histomap.json` next to it, not inside the file — the tags only move
    /// into the picture at conversion. Read here so the panel can show them
    /// while the map is still an original, which is when they are being used
    /// to decide anything.
    pub current_sidecar: Option<crate::sidecar::Sidecar>,
    /// Where the current picture sits on the earth, for a georeferenced TIFF.
    pub current_geo: Option<crate::geotiff::GeoRef>,
    /// Pyramid level sizes, when the open file is a tiled TIFF. Cached so the
    /// canvas can size a detail request without reopening the file each frame.
    pub current_levels: Vec<(u32, u32)>,

    pub detail: crate::detail::DetailLoader,
    /// The sharp patch currently painted over the base texture, if any.
    pub detail_patch: Option<crate::detail::Patch>,
    /// Squares of a layered picture, drawn at the level the zoom asks for.
    pub squares: crate::imagetiles::Squares,
    /// Level, how many levels, squares drawn, squares still waiting — for the
    /// readout.
    pub squares_shown: Option<crate::imagetiles::Painted>,
    /// Where the fade from one picture to the next has got to.
    pub crossfade: crate::crossfade::Crossfade,
    /// The picture last painted on the Viewer canvas, and where it sat.
    ///
    /// Held — the handle keeps its texture alive even once the cache has let go
    /// — so a change of image can keep it on screen through the decode instead
    /// of emptying the canvas.
    pub painted: Option<(egui::TextureHandle, egui::Rect)>,
    /// The picture being faded out from under the one arriving.
    pub leaving: Option<(egui::TextureHandle, egui::Rect)>,
    /// The tiling overlay, toggled with D. Off by default and never persisted:
    /// it is for looking at a problem, not for living with.
    pub tile_debug: bool,
    /// How opaque each map is drawn on the basemap, by path.
    ///
    /// The IMAGES rows write it and the basemap reads it. Held for maps that
    /// are not drawn at all, so one faded away and brought back comes back
    /// where it was rather than at full strength.
    pub map_opacity: std::collections::HashMap<PathBuf, f32>,
    /// A map clicked once, waiting to see whether a second click follows.
    ///
    /// A single click on a sheet reveals its file and a double click switches
    /// it off, and the first click of a double is indistinguishable from a
    /// single one until the moment for the second has passed. Revealing at once
    /// would mean every attempt to hide a map also selected it — which on a
    /// large map is a decode nobody asked for.
    pub pending_reveal: Option<(PathBuf, f64)>,
    /// Maps switched off by hand, and how opaque they were when they went.
    ///
    /// Double-clicking a row in the IMAGES list puts one here and takes it out
    /// again. Separate from the opacity itself because the timeline rewrites
    /// that every frame — a map set to nothing would be back a frame later, so
    /// what is being held is the *decision*, which the fading has to respect.
    pub map_muted: std::collections::HashMap<PathBuf, f32>,
    /// Maps drawn on the basemap besides the selected one.
    ///
    /// Oldest first, which is also the order they are drawn in — a later map
    /// sits over an earlier one, which is the way a pile of dated sheets is
    /// read.
    pub map_extras: Vec<crate::map::Overlay>,
    /// Every map covering what the basemap shows, with the year it carries.
    ///
    /// Worked out once by the map page and read by both the timeline and the
    /// IMAGES tab, so the two cannot disagree about what is out there.
    pub maps_in_view: Vec<(Option<i32>, PathBuf)>,
    /// Points per year on the timeline. Ctrl+wheel over the strip changes it.
    pub timeline_scale: f32,
    /// The year the timeline's playhead is on — the middle of the strip.
    pub timeline_playhead: f32,
    /// Whether scrolling the timeline fades the maps in and out.
    ///
    /// On by default: setting a dozen rows by hand to answer "what did this
    /// corner look like over the years" is a lot of dragging for what is really
    /// one gesture. Dragging a row switches it off, because a hand on a control
    /// beats a rule about it.
    pub timeline_fades: bool,
    /// The stretch of years the timeline is showing, first and last.
    pub timeline_span: (i32, i32),
    /// Maps under the pointer on the basemap, for the IMAGES tab to outline.
    ///
    /// A list rather than one: where sheets overlap, the pointer is over all of
    /// them, and marking only the top one would answer the wrong question.
    pub maps_under_pointer: Vec<PathBuf>,
    /// The row the pointer is over in the IMAGES tab, for the basemap to mark.
    pub map_row_hovered: Option<PathBuf>,
    /// Maps that would not open as an overlay, so the attempt is not repeated
    /// every frame for as long as the row is lit.
    pub failed_overlays: std::collections::HashSet<PathBuf>,
    /// The year at the top of the map's timeline.
    pub timeline_top: f32,
    /// Whether the timeline is drawn at all.
    pub timeline: bool,
    /// The street search: closed until the button is pressed.
    pub street_search: Option<crate::streets::Search>,
    /// A found street, marked on the map until the next search.
    pub found_street: Option<crate::streets::Place>,
    /// Which half of the FILES panel is showing: 0 the tree, 1 the tags.
    pub files_tab: usize,
    /// The tags being required or refused, and nothing about the ones that are
    /// neither — an absent tag is simply not being asked about.
    pub tag_filter: std::collections::BTreeMap<String, TagState>,
    /// Meridians and parallels drawn over the basemap.
    pub graticule: bool,
    /// Which anchors the basemap draws.
    pub anchors_shown: AnchorsShown,
    /// What to go back to when anchors are shown again.
    ///
    /// Hiding is a switch rather than a third kind of showing, so coming back
    /// has to land on the choice that was in force — being dropped onto "all"
    /// after a look at the bare map means picking the subset out again.
    pub anchors_shown_before_hiding: AnchorsShown,
    /// Marked places, kept in memory because the map draws them every frame.
    pub anchors: Vec<crate::db::Anchor>,
    /// The pin being dragged, if one is. While set, dragging moves the pin
    /// rather than the map.
    pub dragging_anchor: Option<i64>,
    /// Whether the drag in progress is scrolling the timeline.
    ///
    /// Decided when the drag starts and held for its whole length, so a gesture
    /// that begins on the strip and wanders off it goes on scrolling time
    /// rather than handing the map a jump halfway through.
    pub dragging_timeline: bool,
    /// The picture being placed by hand, when that is going on.
    pub placing: Option<Placing>,
    /// Anchors placed on the selected picture, and what they add up to.
    pub placed_anchors: Vec<crate::db::PlacedAnchor>,
    pub placement: Option<Result<crate::anchor::Fit, crate::anchor::Refused>>,

    /// Set when a right-click asked for the action menu, consumed next frame.
    /// Files the user asked to delete, waiting on the confirmation dialog.
    /// Deleting is the one destructive thing this app does, so it never
    /// happens on the same click that asks for it. Empty means no dialog.
    pub confirm_delete: Vec<PathBuf>,
    /// A move or copy that has been asked for and is waiting on confirmation:
    /// what kind, where to, and exactly what would happen to each file.
    pub confirm_transfer: Option<(
        crate::transfer::Mode,
        PathBuf,
        Vec<crate::transfer::Planned>,
    )>,
    /// Folders waiting to be re-scanned, newest request last.
    pub refresh_queue: Vec<PathBuf>,
    /// What the last delete did, shown under the FILES tree.
    pub delete_result: Option<(String, bool)>,
    /// The canvas the Viewer last drew into, so a transition that changes its
    /// size can hold the picture steady across the change.
    pub canvas_rect: Option<egui::Rect>,
    /// Where the picture was, at the moment full screen was asked for: the
    /// window and canvas it was drawn in, and its offset within them. Held
    /// until the new geometry arrives, which is a frame or two later — the
    /// window manager does not resize us synchronously.
    pub pending_anchor: Option<(egui::Rect, egui::Rect, egui::Vec2)>,
    /// An offset still gliding to where it belongs: from, to, and when it
    /// started. The picture lands on the pixels it was already on and then
    /// travels, rather than jumping the moment the window changes.
    pub offset_glide: Option<(egui::Vec2, egui::Vec2, f64)>,
    /// Set when the selection has moved somewhere the tree is not showing —
    /// a bookmark jump, or a file opened from outside. The tree scrolls to it
    /// on the next frame it draws, then clears this.
    pub reveal_selection: bool,
    /// The same for the Mosaic: arrow keys can walk the selection off the
    /// bottom of a grid of three thousand tiles, so the grid travels with it.
    pub reveal_in_grid: bool,
    /// Kept alive for as long as the app runs.
    ///
    /// On X11 the copying application serves the clipboard on request, so
    /// dropping this handle takes whatever was copied straight back off again.
    /// Created on first use, since most sessions never copy anything.
    pub clipboard: Option<arboard::Clipboard>,
    /// Which panel the arrow keys act on.
    ///
    /// Set by clicking, not by hovering: the pointer passes over the rail on
    /// its way to everywhere, and having the keys change meaning as it drifted
    /// across would make them unusable. Clicking is the deliberate act.
    pub focus: Focus,
    /// Presentation mode: the picture and nothing else.
    ///
    /// Not the same as the window being full screen — this also puts away the
    /// title bar, both side panels, the header, the toolbar and the filmstrip,
    /// so the image has the whole display. Zoom and pan keep working; there is
    /// simply nothing else drawn.
    pub presenting: bool,
    /// Set when the app was opened straight onto a picture: the first frame
    /// puts the window full screen, which cannot be asked for until the window
    /// exists.
    launch_window_pending: bool,
    /// Set when the app was opened straight onto a picture and has not been
    /// taken out of that mode yet.
    ///
    /// The window it opens is the picture and nothing else, so the two keys
    /// mean what they would in any other viewer launched from a file manager:
    /// Escape closes the app rather than revealing panels nobody asked for,
    /// and Enter is the way into the full panelled layout. Cleared by either,
    /// after which Escape goes back to unwinding the interface as usual.
    launched_on_picture: bool,
    /// Ask the window manager for the keyboard, for a window opened straight
    /// onto a picture.
    ///
    /// Launched from a file manager the window comes up without focus on most
    /// desktops — focus-stealing prevention, and this one moves and resizes
    /// itself on its first frames as well, which several window managers treat
    /// as reason enough not to hand it the keyboard. Nothing bound to a key
    /// worked until the picture had been clicked on, which for a viewer whose
    /// whole interface *is* the keyboard reads as the keys being broken.
    claim_focus: bool,
    /// When to give up asking, filled in on the first frame — there is no
    /// clock before one. A deadline rather than a single request because the
    /// resize is still in flight when the first frame runs, and a window
    /// manager that refuses focus mid-move would otherwise never be asked
    /// again.
    claim_focus_until: Option<f64>,
    /// Wheel travel banked up over the filmstrip, in points.
    ///
    /// One physical notch arrives spread across several frames — egui smooths
    /// scrolling — so acting on each frame's delta stepped four or five images
    /// per notch. Banking the travel and spending it a notch at a time makes
    /// the rail move exactly one picture per click of the wheel.
    pub rail_scroll: f32,
    /// A brief message and the time it appeared, for actions whose only visible
    /// effect happens somewhere else entirely — copying to the clipboard.
    pub toast: Option<(String, f64)>,
    pub export: ExportState,
    /// How far the image being decoded has got, 0 to 1.
    pub loading: Option<(PathBuf, f32)>,
    /// `(strip count, bitmask of finished strips)` while a parallel decode runs.
    pub loading_strips: Option<(u32, u32)>,
    /// What the loader is doing with the visible image right now.
    pub loading_stage: Option<Stage>,
    /// Decode failures, kept so the canvas can explain itself.
    pub decode_error: Option<(PathBuf, String)>,
    /// File to select once the scan triggered by opening it has finished.
    pending_selection: Option<PathBuf>,
    /// Something to say about the file we were asked to open — that it could
    /// not be opened, or that its extension was wrong and we opened it anyway.
    /// Held until there is a frame to say it on.
    opened_note: Option<String>,
    /// The install question on screen, if one was raised at startup.
    pub install_prompt: Option<crate::install::Prompt>,

    /// Last selection written to the database, so we only write on change.
    persisted_selection: Option<String>,
    /// Last panel state written, likewise.
    persisted_ui: Option<String>,
    persisted_config: Option<String>,
    /// Window geometry as of the latest frame, saved on exit.
    geometry: Option<(f32, f32, f32, f32)>,
    /// The scale last handed to egui, so a change made with Ctrl+plus can be
    /// told apart from one made with the slider. See `apply_ui_scale`.
    applied_ui_scale: f32,
    /// Where the scale slider is being held, before it is let go of.
    ///
    /// The slider writes here rather than into the config, and the config is
    /// only set once the pointer comes up. Scaling live would move the slider
    /// itself out from under the pointer holding it — the track grows, the
    /// handle slides away from the finger, and the drag chases a target that is
    /// running away because of the drag.
    pub pending_ui_scale: Option<f32>,

    pub thumbs: Option<Thumbnailer>,
    pub thumb_textures: ThumbCache,
    /// 48 px versions of the same thumbnails, for a Mosaic zoomed far enough
    /// out that the full ones would not fit in VRAM many times over.
    pub mini_textures: ThumbCache,
    /// Which adapter the viewer started on, and what it had to skip to get
    /// there. Reported in the DEBUG tab so "why is this slow today?" has an
    /// answer on screen.
    pub renderer: Option<crate::renderer::Choice>,
    /// Latest graphics memory reading, for the title bar.
    pub bookmarks: crate::bookmarks::Bookmarks,
    /// The chosen vendor's logo, decoded and uploaded on first paint. Kept here
    /// rather than rebuilt per frame — it never changes for a given run.
    pub vendor_logo: Option<egui::TextureHandle>,
    /// Where the selected photo was taken, resolved offline. Computed once when
    /// the selection moves rather than every frame.
    pub current_place: Option<crate::geo::Place>,
    /// Street-address lookups, which go over the network and so only run when
    /// the button is pressed.
    pub address: crate::geo::Address,
    pub vram: Option<crate::vram::Memory>,
    vram_read_at: Option<std::time::Instant>,
    /// Set whenever we upload or drop a texture, so the reading refreshes on
    /// the next frame instead of waiting for the timer.
    vram_dirty: bool,
}

impl GeoRefMapperApp {
    #[cfg(test)]
    pub fn new(ctx: &egui::Context) -> Self {
        Self::with_renderer(ctx, None, DEFAULT_IMAGE_VRAM_MB, DEFAULT_THUMBNAIL_VRAM_MB)
    }

    /// Starts on a known-good adapter, with texture budgets sized for it.
    ///
    /// `renderer` is `None` only in tests, which have no window and so never
    /// probed for one.
    pub fn with_renderer(
        ctx: &egui::Context,
        renderer: impl Into<Option<crate::renderer::Choice>>,
        image_vram_mb: usize,
        thumbnail_vram_mb: usize,
    ) -> Self {
        let renderer = renderer.into();
        let mut config = Config::default();
        config.image_vram_mb = image_vram_mb;
        config.thumbnail_vram_mb = thumbnail_vram_mb;
        let loader = Loader::new(config.decode_threads, ctx.clone());

        let mut app = Self {
            page: 0,
            previous_page: None,
            files_open: true,
            info_open: true,
            info_tab: 0,
            files: FileTree::default(),
            mosaic_tile_size: 148.0,
            map_view: crate::map::View::default(),
            tiles: None,
            map_overlay: None,
            map_window: eframe::egui::vec2(900.0, 600.0),
            map_queue: Vec::new(),
            map_queued_ids: std::collections::HashSet::new(),
            converting: None,
            georeference_sweep: None,
            georeferenced: std::collections::HashSet::new(),
            index: ImageIndex::default(),
            stacks: crate::stacks::Stacks::default(),
            open_stack: None,
            mosaic_order: Vec::new(),
            mosaic_columns: 1,
            mosaic_root: None,
            mosaic_max_tile: 280.0,
            config,
            library: None,
            library_error: None,
            stats: LibraryStats::default(),
            scan: ScanState::default(),
            metrics: Metrics::default(),
            loader,
            textures: TextureCache::new(image_vram_mb * MB),
            viewport: Viewport::default(),
            current: None,
            current_format: None,
            current_exif: None,
            current_meta: crate::meta::Metadata::default(),
            current_sidecar: None,
            current_geo: None,
            current_levels: Vec::new(),
            detail: crate::detail::DetailLoader::new(ctx.clone()),
            detail_patch: None,
            squares: crate::imagetiles::Squares::new(ctx),
            squares_shown: None,
            crossfade: crate::crossfade::Crossfade::default(),
            painted: None,
            leaving: None,
            tile_debug: false,
            map_opacity: std::collections::HashMap::new(),
            pending_reveal: None,
            map_muted: std::collections::HashMap::new(),
            map_extras: Vec::new(),
            maps_in_view: Vec::new(),
            timeline_scale: crate::timeline::DEFAULT_SCALE,
            timeline_playhead: 1840.0,
            timeline_fades: true,
            timeline_span: (1500, 2030),
            maps_under_pointer: Vec::new(),
            map_row_hovered: None,
            failed_overlays: std::collections::HashSet::new(),
            timeline_top: 1820.0,
            timeline: true,
            street_search: None,
            found_street: None,
            files_tab: 0,
            tag_filter: std::collections::BTreeMap::new(),
            graticule: false,
            anchors_shown: AnchorsShown::All,
            anchors_shown_before_hiding: AnchorsShown::All,
            anchors: Vec::new(),
            dragging_anchor: None,
            dragging_timeline: false,
            placing: None,
            placed_anchors: Vec::new(),
            placement: None,
            confirm_delete: Vec::new(),
            confirm_transfer: None,
            refresh_queue: Vec::new(),
            delete_result: None,
            canvas_rect: None,
            pending_anchor: None,
            offset_glide: None,
            clipboard: None,
            reveal_selection: false,
            reveal_in_grid: false,
            focus: Focus::Tree,
            presenting: false,
            launch_window_pending: false,
            launched_on_picture: false,
            claim_focus: false,
            claim_focus_until: None,
            rail_scroll: 0.0,
            toast: None,
            export: ExportState::default(),
            decode_error: None,
            loading: None,
            loading_strips: None,
            loading_stage: None,
            pending_selection: None,
            opened_note: None,
            install_prompt: None,
            persisted_selection: None,
            persisted_ui: None,
            persisted_config: None,
            geometry: None,
            applied_ui_scale: 1.0,
            pending_ui_scale: None,
            thumbs: None,
            thumb_textures: ThumbCache::new(thumbnail_vram_mb * MB),
            // A tenth of the budget holds tens of thousands of these, which is
            // more than any folder here contains.
            mini_textures: ThumbCache::new((thumbnail_vram_mb * MB / 10).max(16 * MB)),
            renderer,
            bookmarks: crate::bookmarks::Bookmarks::default(),
            vendor_logo: None,
            current_place: None,
            address: crate::geo::Address::default(),
            vram: None,
            vram_read_at: None,
            vram_dirty: true,
        };

        // A missing or unreadable library must not stop the viewer from
        // opening; the panel reports the problem instead.
        match Library::open_default() {
            Ok(library) => {
                let mut thumbnailer = Thumbnailer::new(
                    library.path().to_path_buf(),
                    app.config.thumbnail_threads,
                    app.config.use_gpu_decode,
                    ctx.clone(),
                );
                if app.config.background_thumbs {
                    thumbnailer.start_sweep();
                }
                app.thumbs = Some(thumbnailer);
                app.library = Some(library);
                // A library left mostly empty by a previous session gets its
                // space back before anything else happens. Both conditions in
                // `reclaim_space_if_worthwhile` have to hold, so an ordinary
                // library is untouched and the start is not delayed.
                app.reclaim_space_if_worthwhile();
                app.reload_library();
                app.reload_map_queue();
                app.reload_anchors();
                app.restore_selection();
                app.restore_panel_state(ctx);
                // After the config is read, not before: the sweep asks whether
                // map support is on, and until `restore_panel_state` has run
                // that is whatever the default happens to be rather than what
                // was saved.
                app.fill_in_georeferences(ctx);
                app.raise_install_prompt();
            }
            Err(error) => app.library_error = Some(format!("{error:#}")),
        }

        // `geo-ref_mapper photo.jpg` or `geo-ref_mapper ~/Pictures` indexes the folder and
        // opens straight into it.
        if let Some(argument) = std::env::args_os().nth(1) {
            let path = PathBuf::from(argument);
            if path.exists() {
                // Opened *on* a picture, the picture is the point: it starts
                // full screen with nothing but the picture on it, and the
                // panels are a press of Escape away. A folder argument is a
                // browsing session and keeps the panelled layout.
                if path.is_file() {
                    app.presenting = true;
                    app.launch_window_pending = true;
                    app.launched_on_picture = true;
                    app.claim_focus = true;
                    // The tree is not on screen, so the arrows have to mean
                    // "the next picture" from the first keypress rather than
                    // walking rows nobody can see.
                    app.focus = Focus::Images;
                }
                app.open_path(path, ctx);
            } else {
                eprintln!("geo-ref_mapper: {} does not exist", path.display());
            }
        }

        app
    }

    /// Looks at files indexed before the library recorded whether they are
    /// georeferenced, in the background.
    ///
    /// A library scanned by an earlier build has the column empty everywhere,
    /// and rescanning tens of thousands of pictures to learn one bit about a
    /// few thousand TIFFs would be absurd. Non-TIFFs are answered by one
    /// statement; the rest are opened a batch at a time on a thread, so a
    /// library on the USB drive does not stall the interface.
    fn fill_in_georeferences(&mut self, ctx: &egui::Context) {
        // Nothing reads the answer with map support off, and the sweep opens
        // every TIFF in the library to work it out. An image viewer should not
        // be doing that in the background for a page nobody can reach.
        if !self.config.map_support {
            return;
        }
        let Some(path) = self.library.as_ref().map(|library| library.path.clone()) else {
            return;
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        self.georeference_sweep = Some(receiver);
        let ctx = ctx.clone();

        std::thread::spawn(move || {
            let Ok(library) = Library::open(&path) else {
                return;
            };
            let mut found = 0_usize;
            loop {
                let batch = match library.unexamined_for_georeference(64) {
                    Ok(batch) if batch.is_empty() => break,
                    Ok(batch) => batch,
                    Err(_) => break,
                };
                for (id, path) in batch {
                    let georeferenced = crate::scan::is_georeferenced(std::path::Path::new(&path));
                    found += usize::from(georeferenced);
                    if library.set_georeferenced(id, georeferenced).is_err() {
                        return;
                    }
                }
                // The receiver going away means the app has closed.
                if sender.send(found).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
            let _ = sender.send(found);
            ctx.request_repaint();
        });
    }

    /// Picks up the sweep's progress, rebuilding the tree when it finishes so
    /// the newly-known files can be marked.
    pub fn poll_georeference_sweep(&mut self) {
        let Some(receiver) = &self.georeference_sweep else {
            return;
        };
        let mut done = false;
        let mut found = None;
        loop {
            match receiver.try_recv() {
                Ok(count) => found = Some(count),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
            }
        }
        if done {
            self.georeference_sweep = None;
            if found.is_some_and(|count| count > 0) {
                self.reload_library();
            }
        }
    }

    /// Re-reads the indexed rows and rebuilds the FILES tree from them.
    pub fn reload_library(&mut self) {
        let Some(library) = &self.library else {
            return;
        };

        match library.all_images() {
            Ok(records) => {
                // Carried across the rebuild by *path*, not by index: a scan
                // adds rows, which shifts every index after them, so the old
                // index would quietly address a different file.
                let expanded = self.files.expanded_paths();
                let (marked_images, marked_folders) = self.files.marked_paths();
                let selected = self
                    .files
                    .selected_node()
                    .and_then(|node| node.path())
                    .map(str::to_owned);
                let fallback = self.files.selected.clone();

                self.georeferenced = records
                    .iter()
                    .filter(|record| record.georeferenced == Some(true))
                    .map(|record| record.id)
                    .collect();
                self.files = FileTree::from_records(&records);
                self.files.restore_expanded(&expanded);
                self.files.restore_marked(&marked_images, &marked_folders);

                match selected.and_then(|path| self.files.find_by_path(std::path::Path::new(&path)))
                {
                    Some(node) => self.files.selected = node,
                    // Nothing to match on — mock data, or the row has gone.
                    None if self.files.get(&fallback).is_some() => {
                        self.files.selected = fallback;
                    }
                    None => {}
                }
            }
            Err(error) => self.library_error = Some(format!("{error:#}")),
        }

        self.index = ImageIndex::of(&self.files);
        self.stacks = library
            .all_stacks()
            .map(crate::stacks::Stacks::from_rows)
            .unwrap_or_default();
        // A stack that has lost its members while we were away is gone, so an
        // open one may no longer exist.
        if self
            .open_stack
            .is_some_and(|stack| self.stacks.members(stack).is_empty())
        {
            self.open_stack = None;
        }

        self.stats = library
            .stats()
            .inspect_err(|error| log_stats_error(error))
            .unwrap_or_default();

        // A file opened from disk only appears in the tree once its folder has
        // been indexed, so the selection waits until the scan lands.
        if let Some(path) = self.pending_selection.take() {
            match self.files.find_by_path(&path) {
                Some(node) => {
                    self.files.selected = node;
                    self.reveal_selection = true;
                    // Fitted, whatever was on screen before it.
                    //
                    // `open_path` resets the view too, but that happens when
                    // the open is *asked for* — the picture does not arrive
                    // until a folder scan and a decode later, and it is
                    // `carry` that brings it in, which keeps whatever mode is
                    // current by then. So the reset there applies to the
                    // picture being left rather than to the one being opened.
                    // Done again here, where the picture actually lands, the
                    // guarantee holds however long the scan took and whatever
                    // happened during it.
                    self.viewport.reset();
                    // The keyboard goes to the picture, not to the tree.
                    //
                    // `enter_page` aims it at the rail on the way into the
                    // Viewer, but a file opened from the file browser or the
                    // command line never goes through it: the app is built
                    // already on the Viewer, so the focus is left at its
                    // starting value of `Tree`. The arrows then walked tree
                    // rows — and with the folder still shut there was no row
                    // for the selected picture among the visible ones, so they
                    // jumped to the top of the tree instead of stepping to the
                    // next picture. Which is what "the arrows do not work"
                    // looks like.
                    self.focus = Focus::Images;
                }
                None => {
                    // Asked for by name and still not in the tree. The scan
                    // indexes by extension and `open_path` has already offered
                    // it a second chance on its bytes, so by here the file is
                    // neither named nor shaped like anything we read.
                    //
                    // Said out loud, because the alternative is what used to
                    // happen: the selection stayed wherever the scan left it,
                    // and a *different* picture came up as though it were the
                    // one asked for.
                    let name = file_name_of(&path);
                    self.opened_note = Some(format!(
                        "{name} — not one of the file types geo-ref_mapper opens"
                    ));
                }
            }
        }

        // Newly indexed rows have no thumbnails yet.
        if self.config.background_thumbs
            && let Some(thumbs) = &mut self.thumbs
        {
            thumbs.start_sweep();
        }
    }

    /// What the viewer itself is holding on the card, broken down.
    ///
    /// The two caches have budgets and enforce them. Nothing else does: map
    /// squares are trimmed by distance from the view, basemap tiles by the
    /// same, and every map faded up on the map page brings a square cache of
    /// its own. Added up, those can be several times the budgeted part — which
    /// is why they are worth being able to see, and why the shed below reaches
    /// them first.
    pub fn textures_held(&self) -> TexturesHeld {
        let overlays = self
            .map_extras
            .iter()
            .chain(self.map_overlay.as_ref())
            .collect::<Vec<_>>();
        TexturesHeld {
            images: self.textures.used(),
            thumbnails: self.thumb_textures.used() + self.mini_textures.used(),
            squares: self.squares.bytes()
                + overlays
                    .iter()
                    .map(|overlay| overlay.squares.bytes())
                    .sum::<usize>()
                + self
                    .placing
                    .as_ref()
                    .map_or(0, |session| session.squares.bytes()),
            basemap: self.tiles.as_ref().map_or(0, |tiles| tiles.bytes()),
            overlays: overlays
                .iter()
                .filter_map(|overlay| overlay.whole.as_ref())
                .map(|texture| {
                    let [width, height] = texture.size();
                    width * height * 4
                })
                .sum(),
            detail: self.detail_patch.as_ref().map_or(0, |patch| {
                let [width, height] = patch.texture.size();
                width * height * 4
            }),
        }
    }

    /// Answers a card that has just refused a texture.
    ///
    /// Called at the top of a frame, after the failure was caught by
    /// [`crate::oom`] during the last one. Everything droppable goes, and the
    /// two budgets are halved so the caches do not simply refill to the size
    /// that did not fit. What is dropped comes back by being asked for again —
    /// squares and tiles from disk, the picture from its file — so this costs
    /// a moment of softness, not the session.
    ///
    /// The budgets are written to the config rather than to the caches: the
    /// frame loop copies config into them every frame, so a cache told
    /// directly would be back to its old size before the next paint.
    fn shed_textures(&mut self, ctx: &egui::Context, source: &str) {
        let held = self.textures_held();

        self.config.image_vram_mb = (self.config.image_vram_mb / 2).max(MINIMUM_IMAGE_VRAM_MB);
        self.config.thumbnail_vram_mb =
            (self.config.thumbnail_vram_mb / 2).max(MINIMUM_THUMBNAIL_VRAM_MB);
        self.textures.set_budget(self.config.image_vram_mb * MB);
        self.thumb_textures
            .set_budget(self.config.thumbnail_vram_mb * MB);
        self.mini_textures
            .set_budget((self.config.thumbnail_vram_mb * MB / 10).max(16 * MB));

        // The unbudgeted holders, which is where the bulk of it usually is.
        self.squares.shed();
        if let Some(tiles) = &mut self.tiles {
            tiles.shed();
        }
        if let Some(overlay) = &mut self.map_overlay {
            overlay.squares.shed();
        }
        for overlay in &mut self.map_extras {
            overlay.squares.shed();
        }
        if let Some(session) = &mut self.placing {
            session.squares.shed();
        }
        // The sharper patch over the picture is the largest single texture the
        // viewer ever asks for, and the picture underneath is still there
        // without it.
        self.detail_patch = None;
        self.squares_shown = None;

        eprintln!(
            "geo-ref_mapper: the card refused a texture ({source}); let go of {} — {held}. \
             Budgets are now {} MB images, {} MB thumbnails.",
            crate::files::format_bytes(held.total() as u64),
            self.config.image_vram_mb,
            self.config.thumbnail_vram_mb,
        );

        let now = ctx.input(|input| input.time);
        self.toast_at(
            &format!(
                "The card ran out of memory — dropped {} of textures",
                crate::files::format_bytes(held.total() as u64)
            ),
            now,
        );
        self.vram_dirty = true;
    }

    /// Refreshes the graphics memory reading.
    ///
    /// Driven by two things: our own texture traffic, which changes the number
    /// immediately and so should be reflected at once, and a slow tick to catch
    /// everything else on the machine — a game allocating behind us is exactly
    /// the case this readout exists for.
    fn poll_vram(&mut self, ctx: &egui::Context) {
        const TICK: std::time::Duration = std::time::Duration::from_secs(2);

        // An integrated GPU has no memory of its own — it takes what it needs
        // from system RAM — so that is what gets reported for it. A card whose
        // driver will not answer at all still shows nothing.
        let shares_system_memory = self
            .renderer
            .as_ref()
            .is_some_and(|renderer| renderer.shares_system_memory());
        if !shares_system_memory && self.renderer.as_ref().is_some_and(|r| r.memory.is_none()) {
            return;
        }

        let due = self.vram_read_at.is_none_or(|last| last.elapsed() >= TICK);
        if self.vram_dirty || due {
            self.vram = if shares_system_memory {
                crate::vram::system_memory()
            } else {
                crate::vram::in_use()
            };
            self.vram_read_at = Some(std::time::Instant::now());
            self.vram_dirty = false;
        }

        // Keep the readout live even when nothing else is repainting.
        ctx.request_repaint_after(TICK);
    }

    /// Uploads a finished detail patch, if one landed since the last frame.
    fn drain_detail(&mut self, ctx: &egui::Context) {
        let Some(ready) = self.detail.poll() else {
            return;
        };
        // A patch decoded for the previous image must not be painted over this
        // one; the selection can move while a decode is in flight.
        if self.current.as_deref() != Some(ready.request.path.as_path()) {
            return;
        }
        // Belt and braces behind the level choice in `detail`: this is the
        // last point before the card, and the card aborts rather than refuses.
        let patch = crate::cache::fit_to_texture_limit(
            ready.image,
            crate::loader::MAX_TEXTURE_EDGE as usize,
        );
        let texture = ctx.load_texture("detail_patch", patch, egui::TextureOptions::LINEAR);
        self.vram_dirty = true;
        self.detail_patch = Some(crate::detail::Patch {
            request: ready.request,
            texture,
            decode_ms: ready.decode_ms,
            squares: ready.squares,
        });
    }

    /// Uploads thumbnails the workers finished since the last frame.
    fn drain_thumbnails(&mut self, ctx: &egui::Context) {
        let Some(thumbs) = &mut self.thumbs else {
            return;
        };
        let mut ready = Vec::new();
        for result in thumbs.poll() {
            if let ThumbResult::Ready { id, image } = result {
                ready.push((id, image));
            }
        }
        let uploaded = !ready.is_empty();
        for (id, image) in ready {
            // Both sizes from one decode: the Mosaic switches between them as
            // it zooms, and going back to disk for the other would undo the
            // point of having cached anything.
            self.mini_textures
                .insert(ctx, id, crate::cache::shrink_to_mini(&image));
            self.thumb_textures.insert(ctx, id, image);
        }
        self.vram_dirty |= uploaded;
    }

    /// Thumbnail texture for an indexed image, requesting it if absent.
    /// A texture for one tile. `small` asks for the 48 px version, which is all
    /// a tile a few points across can show.
    pub fn thumbnail(&mut self, db_id: Option<i64>, small: bool) -> Option<egui::TextureId> {
        let id = db_id?;
        if small {
            if let Some(texture) = self.mini_textures.get(id) {
                return Some(texture.id());
            }
            // Fall back to the full one while the small one is on its way, so
            // zooming out does not blank tiles that were just visible.
            if let Some(texture) = self.thumb_textures.get(id) {
                return Some(texture.id());
            }
            if self.mini_textures.has_room()
                && let Some(thumbs) = &mut self.thumbs
            {
                thumbs.request(id);
            }
            return None;
        }
        if let Some(texture) = self.thumb_textures.get(id) {
            return Some(texture.id());
        }
        // Zoomed right out, the Mosaic can ask for more tiles than the VRAM
        // budget holds — 3,839 images at 0.4 MB each is well past 384 MB. Once
        // it is full, asking for more can only displace something already on
        // screen, so the request is not made and the tile keeps its placeholder.
        if self.thumb_textures.has_room()
            && let Some(thumbs) = &mut self.thumbs
        {
            thumbs.request(id);
        }
        None
    }

    /// Restores the INFO tab and which groups were expanded.
    fn restore_panel_state(&mut self, ctx: &egui::Context) {
        let Some(library) = &self.library else { return };

        if let Ok(Some(tab)) = library.setting(SETTING_INFO_TAB)
            && let Ok(index) = tab.parse::<usize>()
            && index < panels::INFO_TABS.len()
        {
            self.info_tab = index;
        }

        if let Ok(Some(value)) = library.setting(SETTING_CONFIG) {
            self.config.apply(&value);
            self.persisted_config = Some(value);
            // The caches are sized from the config, so they have to be told.
            self.textures.set_budget(self.config.image_vram_mb * MB);
            self.thumb_textures
                .set_budget(self.config.thumbnail_vram_mb * MB);
        }

        if let Ok(Some(value)) = library.setting(SETTING_BOOKMARKS) {
            self.bookmarks = crate::bookmarks::Bookmarks::decode(&value);
        }

        if let Ok(Some(value)) = library.setting(SETTING_SECTIONS) {
            for (title, open) in decode_sections(&value) {
                // Seed egui's memory, which is where the widget reads from.
                ctx.data_mut(|data| data.insert_temp(crate::widgets::section_id(&title), open));
            }
        }
    }

    /// Writes the INFO tab and group states when either changes.
    fn persist_panel_state(&mut self, ctx: &egui::Context) {
        // The Config groups are stored under a prefixed title so a group there
        // and a section in the DEBUG panel can share a name without sharing a
        // fold state — both have one called THUMBNAILS.
        let titles = panels::DEBUG_SECTIONS
            .iter()
            .map(|(title, default_open)| ((*title).to_owned(), *default_open))
            .chain(pages::CONFIG_SECTIONS.iter().map(|(title, default_open)| {
                (crate::widgets::config_section_title(title), *default_open)
            }));
        let states: Vec<(String, bool)> = titles
            .map(|(title, default_open)| {
                let open = ctx.data_mut(|data| {
                    *data.get_temp_mut_or(crate::widgets::section_id(&title), default_open)
                });
                (title, open)
            })
            .collect();

        let encoded = format!("{}|{}", self.info_tab, encode_sections(&states));
        if self.persisted_ui.as_deref() == Some(encoded.as_str()) {
            return;
        }
        if let Some(library) = &self.library {
            let _ = library.set_setting(SETTING_INFO_TAB, &self.info_tab.to_string());
            let _ = library.set_setting(SETTING_SECTIONS, &encode_sections(&states));
            self.persisted_ui = Some(encoded);
        }
    }

    /// Writes the Config page's settings when any of them changes.
    ///
    /// Called every frame and cheap: it only touches the database when the
    /// encoded form differs from what was last written. Nothing on that page
    /// was being saved at all before this, so every preference reset itself on
    /// each restart.
    pub fn persist_config(&mut self) {
        let encoded = self.config.encode();
        if self.persisted_config.as_deref() == Some(encoded.as_str()) {
            return;
        }
        if let Some(library) = &self.library {
            let _ = library.set_setting(SETTING_CONFIG, &encoded);
            self.persisted_config = Some(encoded);
        }
    }

    /// The folder a bookmark would pin: the selection itself when it is a
    /// folder, otherwise the folder holding the selected image.
    pub fn bookmarkable_folder(&self) -> Option<PathBuf> {
        let node = self.files.selected_node()?;
        match &node.kind {
            Kind::Folder => node.abs_path.as_ref().map(PathBuf::from),
            Kind::Image(meta) => meta
                .path
                .as_ref()
                .and_then(|path| PathBuf::from(path).parent().map(Path::to_path_buf)),
        }
    }

    /// Writes the bookmark list. Called on every change, which is cheap — the
    /// list is at most a couple of dozen short lines.
    pub fn persist_bookmarks(&self) {
        if let Some(library) = &self.library {
            let _ = library.set_setting(SETTING_BOOKMARKS, &self.bookmarks.encode());
        }
    }

    /// Jumps the tree to a bookmarked folder, expanding its parents.
    /// Jumps to a bookmarked folder, showing it on its own.
    ///
    /// Everything else is folded first: the point of a bookmark is to get to
    /// one place across a tree of 415 folders, and revealing it into whatever
    /// was already unfolded buries it again.
    /// Grows a list of files so that picking a collapsed stack means picking
    /// every picture in it — the rule Delete and Export both follow.
    ///
    /// Only while stacks are collapsed: inside an open one, or with the toggle
    /// off, a member is exactly itself, which is what makes deleting one
    /// picture out of a stack possible.
    pub fn stacked_paths(&self, files: Vec<PathBuf>) -> Vec<PathBuf> {
        if !self.config.show_stacks || self.open_stack.is_some() || self.stacks.is_empty() {
            return files;
        }
        let ids: Vec<i64> = files
            .iter()
            .filter_map(|file| self.index.id_of(file))
            .collect();
        if ids.is_empty() {
            return files;
        }
        let mut out: Vec<PathBuf> = self
            .stacks
            .expand_covers(&ids)
            .iter()
            .filter_map(|id| self.index.path_of(*id).cloned())
            .collect();
        // Anything we could not resolve stays in, rather than quietly vanishing
        // from an operation the user asked for.
        for file in files {
            if !out.contains(&file) {
                out.push(file);
            }
        }
        out.dedup();
        out
    }

    fn image_ids_by_path(&self) -> &std::collections::HashMap<PathBuf, i64> {
        &self.index.by_path
    }

    /// The folders a bookmark would pin: the marked ones, or the selection.
    pub fn bookmarkable_folders(&self) -> Vec<PathBuf> {
        let marked: Vec<PathBuf> = self
            .files
            .marked_folders
            .iter()
            .filter_map(|path| self.files.get(path))
            .filter_map(|node| node.path().map(PathBuf::from))
            .collect();
        if !marked.is_empty() {
            return marked;
        }
        self.bookmarkable_folder().into_iter().collect()
    }

    /// Restores a pinned set: every folder marked, and the first one revealed.
    ///
    /// Marking rather than selecting is the point — the Mosaic's scope is the
    /// marked folders, so this brings back the whole multi-folder view in a
    /// single click.
    pub fn open_bookmark_set(&mut self, entry: &crate::bookmarks::Bookmark) {
        if let Some(only) = entry.only() {
            self.open_bookmark(only);
            return;
        }

        self.files.collapse_all();
        self.files.marked_folders.clear();
        let mut first = None;
        for folder in &entry.folders {
            if let Some(node) = self.files.reveal(&folder.to_string_lossy()) {
                self.files.marked_folders.insert(node.clone());
                first.get_or_insert(node);
            }
        }
        if let Some(node) = first {
            self.files.selected = node;
        }
        self.reveal_selection = true;
        self.mosaic_root = None;
    }

    pub fn open_bookmark(&mut self, folder: &Path) {
        self.files.collapse_all();
        if let Some(node) = self.files.reveal(&folder.to_string_lossy()) {
            self.files.clear_marks();
            // The bookmarked folder itself opens too, so its contents are
            // visible rather than one more click away.
            if let Some(target) = self.files.get_mut(&node) {
                target.expanded = true;
            }
            self.files.selected = node;
            self.reveal_selection = true;
        }
    }

    /// Re-selects whatever was open when the app last closed.
    fn restore_selection(&mut self) {
        let Some(library) = &self.library else { return };
        let Ok(Some(path)) = library.setting(SETTING_LAST_SELECTION) else {
            return;
        };
        if let Some(node) = self.files.reveal(&path) {
            self.files.selected = node;
            self.persisted_selection = Some(path);
            self.reveal_selection = true;
        }
    }

    /// Writes the selection when it changes, so a crash still leaves the last
    /// known position behind.
    fn persist_selection(&mut self) {
        let current = self
            .files
            .selected_node()
            .and_then(|node| node.path())
            .map(str::to_owned);
        if current == self.persisted_selection {
            return;
        }
        if let (Some(library), Some(path)) = (&self.library, &current)
            && library.set_setting(SETTING_LAST_SELECTION, path).is_ok()
        {
            self.persisted_selection = current;
        }
    }

    /// Path of the selection, when it names a real file on disk.
    pub fn selected_image_path(&self) -> Option<PathBuf> {
        let node = self.files.selected_node()?;
        match &node.kind {
            Kind::Image(meta) => meta.path.as_ref().map(PathBuf::from),
            // An *open* folder shows its first picture, while the folder itself
            // stays selected in the tree. Nothing is marked by it: the rail
            // fills up and you are looking at something, but no image has been
            // chosen yet.
            //
            // A **shut** folder shows nothing at all. Walking down with the
            // arrows, a picture appearing from a folder that is not open reads
            // as another picture in the folder just left — there is no sign on
            // screen that the ground moved. An empty canvas at that row says
            // plainly that one folder has ended and the next has not been
            // opened, which is the thing worth knowing at that moment.
            Kind::Folder if node.expanded => self.first_image_in_selected_folder(),
            Kind::Folder => None,
        }
    }

    /// The first indexed image directly inside the selected folder.
    fn first_image_in_selected_folder(&self) -> Option<PathBuf> {
        self.files
            .images_in(&self.files.selected, false)
            .into_iter()
            .find_map(|(_, node)| match &node.kind {
                Kind::Image(meta) => meta.path.as_ref().map(PathBuf::from),
                Kind::Folder => None,
            })
    }

    /// Indexed images in the selected folder, in tree order.
    fn sibling_paths(&self) -> Vec<PathBuf> {
        self.files
            .images_for_mosaic(false)
            .into_iter()
            .filter_map(|(_, node)| match &node.kind {
                Kind::Image(meta) => meta.path.as_ref().map(PathBuf::from),
                Kind::Folder => None,
            })
            .collect()
    }

    /// Notices that the selection moved and queues the decodes that follow from
    /// it: the new image first, then the neighbours the user is likely to reach.
    fn sync_selection(&mut self) {
        let selected = self.selected_image_path();
        if selected == self.current {
            return;
        }

        self.current = selected;
        // How the last picture was being looked at is carried to this one —
        // see `Viewport::carry` for what does and does not survive.
        self.viewport.carry();
        self.loading = None;
        self.decode_error = None;
        // Reads only the first few bytes, plus the IFD chain for a TIFF.
        self.current_format = self.current.as_deref().and_then(crate::formats::describe);
        self.current_exif = self.current.as_deref().and_then(crate::exif::from_path);
        self.current_meta = self
            .current
            .as_deref()
            .map(crate::meta::from_path)
            .unwrap_or_default();
        self.current_sidecar = self.current.as_deref().and_then(crate::sidecar::beside);
        self.current_geo = self.current.as_deref().and_then(crate::geotiff::from_path);
        self.reload_placement();
        // Offline, so it can run on every selection without a thought.
        self.current_place = self
            .current_exif
            .as_ref()
            .and_then(|exif| exif.gps.as_ref())
            .and_then(|gps| Some((gps.latitude?, gps.longitude?)))
            .and_then(|(latitude, longitude)| crate::geo::lookup(latitude, longitude));

        // Whatever was sharpened belongs to the previous image.
        self.detail_patch = None;
        self.detail.reset();
        if let Some(path) = self.current.as_deref() {
            self.squares.open(path);
        }
        self.squares_shown = None;
        self.current_levels = self
            .current
            .as_deref()
            .and_then(|path| crate::cog::TiledTiff::open(path).ok())
            .map(|tiff| tiff.level_sizes())
            .unwrap_or_default();
        // Preloads queued for the previous image are no longer worth decoding.
        self.loader.bump_generation();

        let Some(path) = self.current.clone() else {
            return;
        };
        if !self.textures.contains(&path) {
            self.loader.request(&path, Priority::Visible);
        }

        if !self.config.preload_neighbours {
            return;
        }
        let siblings = self.sibling_paths();
        if let Some(index) = siblings.iter().position(|sibling| *sibling == path) {
            // Forward first — that is the direction most people move.
            let neighbours = [
                index.checked_add(1).filter(|next| *next < siblings.len()),
                index.checked_sub(1),
            ];
            for neighbour in neighbours.into_iter().flatten() {
                let neighbour = &siblings[neighbour];
                if !self.textures.contains(neighbour) {
                    self.loader.request(neighbour, Priority::Preload);
                }
            }
        }
    }

    /// Uploads whatever the workers finished since the last frame.
    fn drain_decodes(&mut self, ctx: &egui::Context) {
        for result in self.loader.poll() {
            match result {
                DecodeResult::Progress {
                    path,
                    fraction,
                    strips,
                    stage,
                } => {
                    // Only the visible image drives the bar; preloads are silent.
                    if self.current.as_ref() == Some(&path) {
                        let previous = self
                            .loading
                            .as_ref()
                            .filter(|(loading, _)| *loading == path)
                            .map_or(0.0, |(_, value)| *value);
                        // Never let it go backwards.
                        self.loading = Some((path, fraction.max(previous)));
                        if strips.is_some() {
                            self.loading_strips = strips;
                        }
                        self.loading_stage = Some(stage);
                    }
                }
                DecodeResult::Ready(decoded) => {
                    if self
                        .loading
                        .as_ref()
                        .is_some_and(|(path, _)| *path == decoded.path)
                    {
                        self.loading = None;
                        self.loading_strips = None;
                        self.loading_stage = None;
                    }
                    self.textures
                        .insert(ctx, &decoded.path, decoded.image, decoded.timings);
                    self.vram_dirty = true;
                }
                DecodeResult::Failed { path, error } => {
                    if self.current.as_ref() == Some(&path) {
                        self.decode_error = Some((path, error));
                    }
                }
                DecodeResult::Skipped { .. } => {}
            }
        }
    }

    /// Moves the selection through the images of the current folder, wrapping.
    /// The images the Viewer walks: the filmstrip's contents and what the arrow
    /// keys step through, which must be the same list or the two disagree.
    ///
    /// With `Include subfolders` on this spans the whole tree under the Mosaic's
    /// folder, so reaching the end of one folder carries on into the next
    /// instead of stopping.
    pub fn viewer_images(&self) -> Vec<crate::files::NodePath> {
        let folder = self.files.selected_folder();
        // A picture-only launch stays in the picture's own folder, whatever
        // the recursive switch says. `geo-ref_mapper photo.jpg` is a request to
        // look at one thing and what is beside it — the file manager's own
        // sense of "next" — and rooting the rail on the folder *above* would
        // walk out of that folder into its neighbours, which from a window
        // with no tree on screen looks like the arrows wandering off. The
        // switch is honoured again the moment Enter opens the panelled app,
        // where there is a tree to show where the stepping has got to.
        let root = if self.config.mosaic_recursive && !self.launched_on_picture {
            // The folder *above* this one: the point of the switch is that the
            // end of a folder carries on into the next, and recursing into the
            // current folder alone finds nothing more than it already has.
            //
            // Derived from the selection every time rather than remembered from
            // the Mosaic, so picking an image in the tree re-roots the rail on
            // that image's folder instead of leaving it wherever the grid was.
            let mut parent = folder.clone();
            parent.pop();
            parent
        } else {
            folder
        };
        self.files
            .images_in(
                &root,
                self.config.mosaic_recursive && !self.launched_on_picture,
            )
            .into_iter()
            .map(|(path, _)| path)
            .collect()
    }

    /// Moves to another picture in the current folder, for when this one is
    /// about to go. Falls back to the folder itself when it holds nothing else,
    /// which leaves the tree where it was.
    fn step_within_folder(&mut self) {
        let folder = self.files.selected_folder();
        let images: Vec<crate::files::NodePath> = self
            .files
            .images_in(&folder, false)
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        let here = images.iter().position(|path| *path == self.files.selected);
        let next = match here {
            Some(index) => images
                .get(index + 1)
                .or_else(|| index.checked_sub(1).and_then(|i| images.get(i)))
                .cloned(),
            None => images.first().cloned(),
        };
        self.files.selected = next.unwrap_or(folder);
    }

    /// Moves the Mosaic's selection by `delta` tiles in its own order.
    fn step_grid(&mut self, delta: isize) {
        let images = self.mosaic_order.clone();
        if images.is_empty() {
            return;
        }
        let next = match images.iter().position(|path| *path == self.files.selected) {
            Some(index) => (index as isize + delta).clamp(0, images.len() as isize - 1) as usize,
            None => 0,
        };
        self.files.selected = images[next].clone();
        self.reveal_in_grid = true;
        self.follow_selection_in_tree();
    }

    /// Moves the selection one visible row up or down the tree, folders
    /// included — which is what arrows do in a file tree.
    fn step_tree_row(&mut self, delta: isize) {
        let rows = self.files.visible_rows();
        if rows.is_empty() {
            return;
        }
        let next = match rows.iter().position(|row| *row == self.files.selected) {
            Some(index) => (index as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
            None => 0,
        };
        self.files.selected = rows[next].clone();
        // Scroll the panel to keep up. Stepping with the arrows otherwise moved
        // the highlight while the tree sat still, so after a dozen presses the
        // selected row was somewhere off the top and nothing on screen said
        // where in the folder you had got to.
        //
        // `step_image` already did this through `follow_selection_in_tree`,
        // which is why walking the *pictures* kept up and walking the *rows*
        // did not. Put here rather than at the key handler so it belongs to
        // moving the selection, wherever that is asked for.
        self.reveal_selection = true;
    }

    /// Unfolds the tree down to the current selection and asks the panel to
    /// scroll to it.
    ///
    /// Stepping through a rail that spans folders walks straight past the
    /// folder boundaries, so the row can be inside something that is shut. The
    /// tree is meant to show what you are looking at; that only holds if it
    /// opens up to reach it.
    fn follow_selection_in_tree(&mut self) {
        let path = self.files.selected.clone();
        for depth in 1..path.len() {
            if let Some(node) = self.files.get_mut(&path[..depth]) {
                node.expanded = true;
            }
        }
        self.reveal_selection = true;
    }

    pub fn step_image(&mut self, delta: isize) {
        let images = self.viewer_images();
        if images.is_empty() {
            return;
        }

        let next = match images.iter().position(|path| *path == self.files.selected) {
            Some(index) => (index as isize + delta).rem_euclid(images.len() as isize) as usize,
            None => 0,
        };
        self.files.selected = images[next].clone();
        self.follow_selection_in_tree();
    }

    /// The file the delete key and the context menu act on: the current
    /// selection, when it is an image that exists on disk.
    pub fn deletable(&self) -> Vec<PathBuf> {
        // A marked set is what the user built to act on; without one it is the
        // single selected image. Never a folder: `action_paths` would expand
        // one to everything inside it, and "delete" should not quietly mean
        // "delete forty things" because a folder row happened to be selected.
        if !self.files.marked.is_empty() {
            return self.stacked_paths(self.files.action_paths());
        }
        match self.files.selected_node().map(|node| &node.kind) {
            Some(crate::files::Kind::Image(meta)) => meta.path.iter().map(PathBuf::from).collect(),
            _ => Vec::new(),
        }
    }

    /// Rebuilds the thumbnail pool when the worker count changes.
    ///
    /// The workers are spawned once, so the only way to change how many there
    /// are is to stop them and start again. Cheap enough for a slider: the
    /// queue is rebuilt, not the thumbnails, and anything already stored stays
    /// stored.
    fn apply_thumbnail_threads(&mut self, ctx: &egui::Context) {
        let wanted = self
            .config
            .thumbnail_threads
            .clamp(1, crate::thumbs::MAX_THUMB_WORKERS);
        let running = self.thumbs.as_ref().map(Thumbnailer::worker_count);
        if running == Some(wanted) || self.library.is_none() {
            return;
        }
        let Some(library) = &self.library else { return };

        let mut thumbnailer = Thumbnailer::new(
            library.path().to_path_buf(),
            wanted,
            self.config.use_gpu_decode,
            ctx.clone(),
        );
        if self.config.background_thumbs {
            thumbnailer.start_sweep();
        }
        self.thumbs = Some(thumbnailer);
    }

    /// Opens or closes the folder the selection sits in.
    ///
    /// Collapsing from inside a folder moves the selection up to it, so a
    /// second press closes the parent — which is what makes left-arrow walk
    /// back out of a deep path rather than stopping at the first level.
    fn set_folder_expanded(&mut self, open: bool) {
        let on_folder = self
            .files
            .selected_node()
            .is_some_and(crate::files::Node::is_folder);
        let target = if on_folder {
            self.files.selected.clone()
        } else {
            self.files.selected_folder()
        };
        let Some(node) = self.files.get_mut(&target) else {
            return;
        };
        if !node.is_folder() {
            return;
        }

        if open {
            node.expanded = true;
            self.files.selected = target;
            return;
        }
        if node.expanded && on_folder {
            node.expanded = false;
        } else {
            // Already shut, or we were inside it: step out to the parent.
            let mut parent = target.clone();
            parent.pop();
            if let Some(above) = self.files.get_mut(&parent) {
                above.expanded = false;
                self.files.selected = parent;
                return;
            }
            self.files.selected = target;
        }
    }

    /// Folders queued for a refresh, run one at a time because the scanner
    /// takes a single root.
    ///
    /// Re-indexes a folder and drops rows whose files have gone: a scan on its
    /// own only ever adds, so without the pruning a picture deleted outside
    /// geo-ref_mapper would sit in the tree for ever.
    pub fn refresh_folders(&mut self, folders: Vec<PathBuf>, ctx: &egui::Context) {
        if folders.is_empty() || self.scan.is_running() {
            return;
        }
        for folder in &folders {
            self.forget_missing_under(folder);
        }
        self.reload_library();

        self.refresh_queue = folders;
        self.start_next_refresh(ctx);
    }

    /// Drops rows under `folder` whose files are no longer on disk.
    fn forget_missing_under(&mut self, folder: &Path) {
        let Some(library) = &self.library else {
            return;
        };
        let Ok(paths) = library.paths_under(&folder.to_string_lossy()) else {
            return;
        };
        for path in paths {
            if !Path::new(&path).exists() {
                let _ = library.forget_image(&path);
            }
        }
    }

    /// Starts the next queued refresh, if the scanner is free.
    pub fn start_next_refresh(&mut self, ctx: &egui::Context) {
        if self.scan.is_running() || self.refresh_queue.is_empty() {
            return;
        }
        let Some(library) = &self.library else {
            self.refresh_queue.clear();
            return;
        };
        let folder = self.refresh_queue.remove(0);
        self.scan.start(
            library.path().to_path_buf(),
            folder,
            Mode::Hunt,
            ctx.clone(),
        );
    }

    /// The folder the Remove button would un-index, if one is selected.
    pub fn removable_folder(&self) -> Option<PathBuf> {
        let node = self.files.selected_node()?;
        (node.is_folder() && !self.files.is_mock)
            .then(|| node.path().map(PathBuf::from))
            .flatten()
    }

    /// Turns a tag to its next state and re-selects from scratch.
    ///
    /// The selection is *recomputed*, never nudged. Nudging is what made the
    /// first version of this unusable: narrow by the wrong tag and the only way
    /// back was to select everything and start again. Recomputing means every
    /// click is undone by clicking it again.
    pub fn cycle_tag(&mut self, tag: &str, now: f64) {
        let next = self.tag_state(tag).next();
        if next == TagState::Off {
            self.tag_filter.remove(tag);
        } else {
            self.tag_filter.insert(tag.to_owned(), next);
        }
        self.select_by_tags(now);
    }

    /// Forgets the whole filter and the selection it produced.
    pub fn clear_tag_filter(&mut self, now: f64) {
        self.tag_filter.clear();
        self.unmark_everything();
        self.toast_at("Filter cleared", now);
    }

    /// Clears the marked set *and* the grid that was showing it.
    ///
    /// The Mosaic roots itself on the marked folders when there are any, and on
    /// a remembered folder when there are not. Clearing the marks without
    /// forgetting that root left the grid showing the set that had just been
    /// thrown away — the marks were gone everywhere except the one place you
    /// were looking at them.
    pub fn unmark_everything(&mut self) {
        self.files.clear_marks();
        self.mosaic_root = None;
    }

    /// How a tag is being used, for drawing its row.
    pub fn tag_state(&self, tag: &str) -> TagState {
        self.tag_filter.get(tag).copied().unwrap_or_default()
    }

    /// Marks exactly the pictures the filter describes, and nothing else.
    fn select_by_tags(&mut self, now: f64) {
        if self.tag_filter.is_empty() {
            self.files.clear_marks();
            self.toast_at("No tags chosen", now);
            return;
        }
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        for (tag, state) in &self.tag_filter {
            match state {
                TagState::Include => include.push(tag.clone()),
                TagState::Exclude => exclude.push(tag.clone()),
                TagState::Off => {}
            }
        }

        let Some(library) = &self.library else { return };
        let matched: std::collections::HashSet<PathBuf> = library
            .paths_matching_tags(&include, &exclude)
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect();

        self.files.clear_marks();
        let count = self.files.mark_those_in(&matched);
        self.toast_at(&format!("{count} selected"), now);
    }

    /// Selects a picture in the FILES tree and scrolls to it.
    ///
    /// Opens the folders on the way down, because a map reached from the basemap
    /// is very often in a folder that is shut — the tree is meant to show what
    /// you are looking at, and that only holds if it opens up to reach it.
    pub fn reveal_in_tree(&mut self, path: &Path, now: f64) {
        let Some(node) = self.files.find_by_path(path) else {
            self.toast_at("That picture is not in the tree", now);
            return;
        };
        self.files.selected = node;
        self.follow_selection_in_tree();
        self.focus = Focus::Tree;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.toast_at(&name, now);
    }

    /// Marks every picture in the tree — the start of narrowing by tag.
    pub fn mark_everything(&mut self, now: f64) {
        self.files.mark_all_images();
        let count = self.files.action_paths().len();
        self.toast_at(&format!("{count} selected"), now);
    }

    /// Every tag in the library, commonest first, with how many carry it.
    pub fn tag_counts(&self) -> Vec<(String, usize)> {
        self.library
            .as_ref()
            .and_then(|library| library.tags_with_counts().ok())
            .unwrap_or_default()
    }

    /// Gives back the space a big removal left behind, when that is quick.
    ///
    /// Deleting rows never shrinks a SQLite file: the pages go on a free list
    /// and are handed out again as the library grows. Usually that is exactly
    /// right — re-scanning re-uses them. It stops being right when a library
    /// gets permanently smaller, and then the file sits at the size of one that
    /// no longer exists.
    ///
    /// Rebuilding copies the *live* rows, so the cost follows what is left
    /// rather than the size of the file — which is why the case worth doing is
    /// also the fast one. Both conditions are needed: plenty of waste, and
    /// little enough left that the rebuild is not felt.
    ///
    /// Not asked about, because knowing when a database wants compacting is not
    /// something anyone should have to carry in their head.
    fn reclaim_space_if_worthwhile(&mut self) -> Option<u64> {
        const ENOUGH_WASTE: u64 = 64 * 1024 * 1024;
        const QUICK_ENOUGH: u64 = 256 * 1024 * 1024;

        let library = self.library.as_ref()?;
        let free = library.free_bytes().ok()?;
        let total = std::fs::metadata(library.path()).ok()?.len();
        let live = total.saturating_sub(free);

        if free < ENOUGH_WASTE || free * 2 < total || live > QUICK_ENOUGH {
            return None;
        }
        library.compact().ok().filter(|saved| *saved > 0)
    }

    /// How opaque a map is drawn on the basemap, 0 for not drawn at all.
    ///
    /// The selected map keeps its opacity on the overlay itself, where the MAP
    /// tab's slider has always written it — so the two controls move the same
    /// number rather than each holding their own idea of it.
    pub fn map_opacity_of(&self, path: &Path) -> f32 {
        if self.map_muted.contains_key(path) {
            return 0.0;
        }
        match &self.map_overlay {
            Some(overlay) if overlay.path == path => overlay.opacity,
            _ => self.map_opacity.get(path).copied().unwrap_or(0.0),
        }
    }

    /// Sets it. Loading and dropping happen in [`settle_map_extras`], once a
    /// frame, rather than here — a drag crosses zero repeatedly and must not
    /// open and shut a file each time it does.
    ///
    /// [`settle_map_extras`]: Self::settle_map_extras
    pub fn set_map_opacity(&mut self, path: &Path, value: f32) {
        // Setting one by hand is a decision about it, so it stops being off.
        self.map_muted.remove(path);
        let value = value.clamp(0.0, 1.0);
        match &mut self.map_overlay {
            Some(overlay) if overlay.path == path => overlay.opacity = value,
            _ => {
                self.map_opacity.insert(path.to_path_buf(), value);
            }
        }
    }

    /// Whether the INFO panel is showing its IMAGES tab.
    ///
    /// The timeline and the fading it drives belong to that tab: the strip is
    /// the control and the list is the readout, and running either with the
    /// list out of sight means maps appearing and going on their own with
    /// nothing on screen saying why.
    pub fn images_tab_open(&self) -> bool {
        // `info_tab` numbers `INFO_TABS` whether or not the tab is on screen,
        // so map support is asked separately: a saved index still says IMAGES
        // after the switch has taken that tab away.
        self.config.map_support
            && crate::panels::INFO_TABS
                .get(self.info_tab)
                .is_some_and(|tab| *tab == "IMAGES")
    }

    /// Switches a map off, or back on at whatever it was.
    ///
    /// For double-clicking its name: one map in a pile is in the way, and
    /// taking it out and putting it back has to be the same gesture — nobody
    /// wants to drag a row to nothing and then remember where it stood.
    /// Reveals a map's file, once it is clear no second click is coming.
    ///
    /// Long enough that a double click has been and gone. Shorter and hiding a
    /// map would select it on the way past; much longer and a plain click feels
    /// like it was ignored.
    const DOUBLE_CLICK: f64 = 0.35;

    /// Notes a click on a sheet. Acted on by [`poll_pending_reveal`].
    ///
    /// [`poll_pending_reveal`]: Self::poll_pending_reveal
    pub fn click_on_map(&mut self, path: &Path, now: f64) {
        self.pending_reveal = Some((path.to_path_buf(), now));
    }

    /// Carries out a click that no second click overtook. Call once a frame.
    pub fn poll_pending_reveal(&mut self, now: f64) {
        let Some((path, at)) = &self.pending_reveal else {
            return;
        };
        // A clock that jumped backwards must not park a click here for ever.
        if now < *at || now - *at >= Self::DOUBLE_CLICK {
            let path = path.clone();
            self.pending_reveal = None;
            self.reveal_in_tree(&path, now);
        }
    }

    pub fn toggle_map_mute(&mut self, path: &Path) {
        // The second click of a double cancels the first: this *was* the second
        // click, so whatever the first one was going to do, it is not that.
        self.pending_reveal = None;
        match self.map_muted.remove(path) {
            // Back on at the level it left at. With the timeline doing the
            // fading that lasts one frame before the strip takes it over again,
            // which is right: what it was, was whatever the strip said.
            Some(was) => self.set_map_opacity(path, was),
            None => {
                let was = self.map_opacity_of(path);
                self.set_map_opacity(path, 0.0);
                self.map_muted.insert(path.to_path_buf(), was);
            }
        }
    }

    /// Whether anchors are off the map and pinned down, however that came about.
    ///
    /// The IMAGES tab hides and locks them on its own. That tab is for watching
    /// sheets fade over the basemap as time scrolls: a scatter of pins on top
    /// of the very thing being compared is in the way of the comparison, and a
    /// drag there is aimed at the map or the strip, never at an anchor.
    ///
    /// Nothing is written down when it does. The MAP tab's switches keep saying
    /// what was set there, and going back to that tab puts the pins and the
    /// lock where they were left.
    pub fn anchors_off_the_map(&self) -> bool {
        self.anchors_shown.hidden() || self.images_tab_open()
    }

    /// Sets every map's opacity from where the timeline's playhead is standing.
    ///
    /// Only maps that carry a year: an undated sheet has no place on a timeline
    /// and would have to be shown always or never, neither of which is an
    /// answer. It keeps whatever was set on it by hand.
    ///
    /// The selected map is left alone too. It is the one being worked on, and
    /// the MAP tab's own slider is the control for it — having the strip
    /// quietly overrule that would make the slider look broken.
    pub fn fade_maps_to_timeline(&mut self) {
        if !self.timeline_fades {
            return;
        }
        let window = crate::timeline::window_years(self.timeline_scale);
        let playhead = self.timeline_playhead;
        let selected = self
            .map_overlay
            .as_ref()
            .map(|overlay| overlay.path.clone());

        // Sheets sharing a year are spread inside it, so the playhead crosses
        // them one after another instead of lighting all four at once. Sorted
        // by path first: the order has to be the same on every frame or they
        // would swap places under the playhead and flicker past each other.
        let mut by_year: std::collections::BTreeMap<i32, Vec<&PathBuf>> = Default::default();
        for (year, path) in &self.maps_in_view {
            if let Some(year) = year {
                by_year.entry(*year).or_default().push(path);
            }
        }
        let mut wanted: Vec<(PathBuf, f32)> = Vec::new();
        for (year, mut paths) in by_year {
            paths.sort();
            let count = paths.len();
            for (index, path) in paths.into_iter().enumerate() {
                if selected.as_ref() == Some(path) || self.map_muted.contains_key(path) {
                    continue;
                }
                let at = crate::timeline::slot_year(year, index, count);
                let opacity = crate::timeline::opacity_at(at, playhead, window);
                if opacity > 0.0 {
                    wanted.push((path.clone(), opacity));
                }
            }
        }

        // Nearest the playhead first, and only so many. A busy decade can put
        // fifty sheets inside one window, and drawing them all is what turns a
        // scroll into a stall.
        wanted.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        wanted.truncate(crate::timeline::MOST_AT_ONCE);

        // Everything the strip did not ask for is let go of rather than set to
        // nothing, so a map scrolled past stops being held open. Maps the
        // selected one aside, which is not the strip's to touch.
        let keep: std::collections::HashMap<PathBuf, f32> = wanted.into_iter().collect();
        self.map_opacity
            .retain(|path, _| keep.contains_key(path) || selected.as_ref() == Some(path));
        self.map_opacity.extend(keep);
    }

    /// Brings the drawn maps into line with the opacities the IMAGES tab holds.
    ///
    /// Loads what has been faded up and lets go of what has been faded away, so
    /// nothing is held open for a map nobody is looking at. Called once a frame
    /// from the map page.
    pub fn settle_map_extras(&mut self, ctx: &egui::Context) {
        let selected = self
            .map_overlay
            .as_ref()
            .map(|overlay| overlay.path.clone());

        // Gone to nothing, or now the selected map, which draws itself.
        self.map_extras.retain(|overlay| {
            self.map_opacity
                .get(&overlay.path)
                .is_some_and(|opacity| *opacity > 0.0)
                && selected.as_ref() != Some(&overlay.path)
                && !self.map_muted.contains_key(&overlay.path)
        });

        for (path, opacity) in &self.map_opacity {
            // A map that would not open is not tried again. Without this a drag
            // across a broken row reopens the file on every frame of it.
            if *opacity <= 0.0
                || selected.as_ref() == Some(path)
                || self.failed_overlays.contains(path)
                || self.map_muted.contains_key(path)
            {
                continue;
            }
            if let Some(overlay) = self
                .map_extras
                .iter_mut()
                .find(|overlay| &overlay.path == path)
            {
                overlay.opacity = *opacity;
                continue;
            }
            // A map with no position cannot be put anywhere, and one that will
            // not open is not worth trying again every frame — both are
            // dropped back to nothing so the row returns to unlit.
            let loaded = crate::geotiff::from_path(path)
                .ok_or_else(|| "no position in this file".to_owned())
                .and_then(|geo| crate::map::load_overlay(ctx, path, geo));
            match loaded {
                Ok(mut overlay) => {
                    overlay.opacity = *opacity;
                    self.map_extras.push(overlay);
                }
                Err(_) => {
                    self.failed_overlays.insert(path.clone());
                }
            }
        }
        self.map_opacity
            .retain(|path, _| !self.failed_overlays.contains(path));

        // Oldest underneath. The year comes from the same list the timeline
        // uses, so the pile is stacked the way the list is read.
        let year_of: std::collections::HashMap<&PathBuf, Option<i32>> = self
            .maps_in_view
            .iter()
            .map(|(year, path)| (path, *year))
            .collect();
        self.map_extras.sort_by(|left, right| {
            year_of
                .get(&left.path)
                .copied()
                .flatten()
                .cmp(&year_of.get(&right.path).copied().flatten())
                .then_with(|| left.path.cmp(&right.path))
        });
    }

    /// Drops the picture being held on the Viewer canvas.
    ///
    /// For a decode that failed or a selection naming nothing: keeping the last
    /// picture up there would be showing the wrong one, and quietly — it looks
    /// exactly like a picture that opened.
    pub fn forget_painted(&mut self) {
        self.painted = None;
        self.leaving = None;
        self.crossfade.clear();
    }

    /// Takes a folder out of the library. **Nothing on disk is touched** —
    /// this is un-indexing, the opposite of Scan, and the pictures are all
    /// still there to be scanned again later.
    pub fn forget_folder(&mut self, folder: &Path) {
        let Some(library) = &self.library else {
            return;
        };
        let name = folder
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| folder.display().to_string());

        match library.forget_folder(&folder.to_string_lossy()) {
            Ok(removed) => {
                self.files.clear_marks();
                self.current = None;
                self.reload_library();
                // Said only when it happened, and in bytes rather than in the
                // name of the thing that did it.
                let reclaimed = match self.reclaim_space_if_worthwhile() {
                    Some(bytes) => format!(
                        " · {} of disk given back",
                        crate::files::format_bytes(bytes)
                    ),
                    None => String::new(),
                };
                self.scan.summary = Some(format!(
                    "Removed {name} from the tree · {} still on disk{reclaimed}",
                    plural_images(removed)
                ));
            }
            Err(error) => self.library_error = Some(format!("{error:#}")),
        }
    }

    /// What a stack action would do with the current selection, and whether it
    /// can be done at all.
    ///
    /// The rules morgan settled: whole stacks in a selection are absorbed, and
    /// the earliest one keeps its cover; but a picture that is a *member* of a
    /// stack not wholly selected is refused rather than quietly moved.
    pub fn plan_stack(&self, files: &[PathBuf]) -> Result<(String, Vec<i64>), String> {
        let by_path = self.image_ids_by_path();
        let ids: Vec<i64> = files
            .iter()
            .filter_map(|file| by_path.get(file).copied())
            .collect();
        if ids.len() < 2 {
            return Err("Select two or more pictures".to_owned());
        }

        let mut ordered: Vec<i64> = Vec::new();
        let mut covers = 0;
        for id in &ids {
            match self.stacks.of(*id) {
                Some(stack) if self.stacks.is_cover(*id) => {
                    covers += 1;
                    for member in self.stacks.members(stack) {
                        if !ordered.contains(member) {
                            ordered.push(*member);
                        }
                    }
                }
                Some(_) => {
                    return Err(
                        "One of these is already in another stack — unstack it first".to_owned(),
                    );
                }
                None => {
                    if !ordered.contains(id) {
                        ordered.push(*id);
                    }
                }
            }
        }

        // The cover of the first stack survives a merge; with no stack in the
        // selection the first picture in grid order becomes the cover.
        if covers > 0
            && let Some(first_cover) = ids.iter().find(|id| self.stacks.is_cover(**id))
        {
            ordered.retain(|member| member != first_cover);
            ordered.insert(0, *first_cover);
        }

        // How many are actually being *added*: everything in the plan that the
        // target stack does not already hold. `covers` is a count of stacks,
        // not of pictures, so subtracting it said "add 4" when two were being
        // added to a stack of three.
        let already = ids
            .iter()
            .find(|id| self.stacks.is_cover(**id))
            .and_then(|id| self.stacks.of(*id))
            .map_or(0, |stack| self.stacks.len(stack));
        let label = if covers > 0 {
            format!(
                "Add {} to stack",
                plural_images(ordered.len().saturating_sub(already))
            )
        } else {
            format!("Stack {}", plural_images(ordered.len()))
        };
        Ok((label, ordered))
    }

    /// The stack the selection belongs to, with each member's details, for the
    /// STACK tab. Ordered cover first.
    pub fn selected_stack(&self) -> Option<(i64, Vec<(PathBuf, crate::files::ImageMeta)>)> {
        let file = self.selected_image_path()?;
        let id = self.index.id_of(&file)?;
        let stack = self.stacks.of(id)?;

        let members = self
            .stacks
            .members(stack)
            .iter()
            .filter_map(|member| self.index.meta_of(*member).cloned())
            .collect();
        Some((stack, members))
    }

    /// What the stack section of the right-click menu should offer.
    pub fn stack_menu(&self, batch: &[PathBuf]) -> crate::panels::StackMenu {
        crate::panels::StackMenu {
            create: self.plan_stack(batch).map(|(label, _)| label),
            open: self.open_stack.is_some(),
        }
    }

    /// Whether one particular picture is in a stack, for the menu drawn on it.
    pub fn is_stacked(&self, file: &Path) -> bool {
        self.index
            .id_of(file)
            .is_some_and(|id| self.stacks.of(id).is_some())
    }

    /// Carries out whatever the right-click menu was asked for.
    ///
    /// Collected there and applied here because the menu is drawn inside a
    /// borrow of the tree or the grid, and every one of these needs the app.
    pub fn apply_menu(&mut self, menu: crate::panels::MenuResult, batch: &[PathBuf], now: f64) {
        if !menu.delete.is_empty() {
            self.confirm_delete = menu.delete;
        }
        if let Some((mode, files)) = menu.transfer {
            self.ask_transfer(mode, files);
        }
        if let Some(file) = menu.copy_image {
            self.copy_image(&file, now);
        }
        if menu.copied_path {
            self.toast = Some(("Path copied to clipboard".to_owned(), now));
        }
        if !menu.queue_maps.is_empty() {
            let ids: Vec<i64> = menu
                .queue_maps
                .iter()
                .filter_map(|file| self.index.id_of(file))
                .collect();
            self.queue_map_images(&ids, now);
        }
        if let Some(folder) = menu.forget_folder {
            self.forget_folder(&folder);
        }
        if let Some(folder) = menu.queue_map_folder {
            // Asked of the library rather than walked on disk: the tree is
            // built from what is indexed, so this queues exactly the pictures
            // the folder was showing — and nothing that has never been scanned.
            let ids = self
                .library
                .as_ref()
                .and_then(|library| library.image_ids_under(&folder.to_string_lossy()).ok())
                .unwrap_or_default();
            if ids.is_empty() {
                self.toast_at("Nothing indexed in that folder yet", now);
            } else {
                self.queue_map_images(&ids, now);
            }
        }
        // Aimed at the picture that was right-clicked. Using the first marked
        // one instead would unstack whichever happened to sort first, which is
        // rarely the one under the cursor.
        match menu.stack {
            Some((crate::panels::StackAction::Create, _)) => self.make_stack(batch, now),
            Some((crate::panels::StackAction::Unstack, file)) => self.unstack(&file, now),
            Some((crate::panels::StackAction::SetCover, file)) => self.set_stack_cover(&file, now),
            Some((crate::panels::StackAction::Remove, _)) => self.remove_from_stack(batch, now),
            None => {}
        }
    }

    /// Creates or grows a stack from the current selection.
    pub fn make_stack(&mut self, files: &[PathBuf], now: f64) {
        let (_, members) = match self.plan_stack(files) {
            Ok(plan) => plan,
            Err(why) => {
                self.toast = Some((why, now));
                return;
            }
        };
        let Some(library) = &self.library else { return };
        let folder = files
            .first()
            .and_then(|file| file.parent())
            .map(|folder| folder.to_string_lossy().into_owned())
            .unwrap_or_default();

        match library.create_stack(&folder, &members) {
            Ok(_) => {
                self.files.clear_marks();
                self.reload_library();
                self.toast = Some((format!("Stacked {}", plural_images(members.len())), now));
            }
            Err(error) => self.toast = Some((format!("{error:#}"), now)),
        }
    }

    /// Dissolves the stack a picture belongs to, and marks what came out of it.
    ///
    /// The freed pictures join whatever was already marked rather than
    /// replacing it, so unstacking leaves you holding them — ready to re-stack
    /// them differently, move them, or export them — instead of loose on the
    /// grid with nothing selected.
    pub fn unstack(&mut self, file: &Path, now: f64) {
        let by_path = self.image_ids_by_path();
        let Some(id) = by_path.get(file).copied() else {
            return;
        };
        let Some(stack) = self.stacks.of(id) else {
            return;
        };
        let Some(library) = &self.library else { return };

        // Taken before the stack goes, because afterwards there is nothing left
        // to ask which pictures were in it.
        let freed: Vec<PathBuf> = self
            .stacks
            .members(stack)
            .iter()
            .filter_map(|member| {
                by_path
                    .iter()
                    .find(|(_, id)| *id == member)
                    .map(|(path, _)| path.clone())
            })
            .collect();

        if let Err(error) = library.unstack(stack) {
            self.toast = Some((format!("{error:#}"), now));
            return;
        }
        if self.open_stack == Some(stack) {
            self.open_stack = None;
        }
        self.reload_library();

        // Re-found by path: the rebuild renumbers every node, so the old
        // indices would address the wrong pictures.
        for path in &freed {
            if let Some(node) = self.files.find_by_path(path) {
                self.files.marked.insert(node);
            }
        }
        self.toast = Some((
            format!(
                "Unstacked {} · {} now selected",
                plural_images(freed.len()),
                self.files.marked.len()
            ),
            now,
        ));
    }

    /// Makes one member the face of its stack.
    pub fn set_stack_cover(&mut self, file: &Path, now: f64) {
        let Some(id) = self.image_ids_by_path().get(file).copied() else {
            return;
        };
        let Some(stack) = self.stacks.of(id) else {
            return;
        };
        let Some(library) = &self.library else { return };

        if let Err(error) = library.set_stack_cover(stack, id) {
            self.toast = Some((format!("{error:#}"), now));
            return;
        }
        self.reload_library();
        self.toast = Some(("Stack cover set".to_owned(), now));
    }

    /// Takes pictures out of their stack without deleting anything.
    pub fn remove_from_stack(&mut self, files: &[PathBuf], now: f64) {
        let by_path = self.image_ids_by_path();
        let ids: Vec<i64> = files
            .iter()
            .filter_map(|file| by_path.get(file).copied())
            .collect();
        if ids.is_empty() {
            return;
        }
        let Some(library) = &self.library else { return };
        if let Err(error) = library.remove_from_stack(&ids) {
            self.toast = Some((format!("{error:#}"), now));
            return;
        }
        self.files.clear_marks();
        self.reload_library();
        self.toast = Some((
            format!("Removed {} from the stack", plural_images(ids.len())),
            now,
        ));
    }

    /// Copies one picture to the clipboard as an image.
    pub fn copy_image(&mut self, path: &Path, now: f64) {
        let clipboard = match self.clipboard.as_mut() {
            Some(clipboard) => clipboard,
            None => match arboard::Clipboard::new() {
                Ok(clipboard) => self.clipboard.insert(clipboard),
                Err(error) => {
                    self.toast = Some((format!("No clipboard — {error}"), now));
                    return;
                }
            },
        };
        self.toast = Some(
            match crate::desktop::copy_image_to_clipboard(clipboard, path) {
                Ok((width, height)) => (format!("Copied image · {width} × {height}"), now),
                Err(error) => (format!("Could not copy — {error}"), now),
            },
        );
    }

    /// Asks where to put a selection, and works out what that would mean.
    ///
    /// Nothing is touched here: the plan is computed and held for the dialog,
    /// so the count — and how many would have to be renamed to avoid landing on
    /// something already there — can be shown before anything happens.
    pub fn ask_transfer(&mut self, mode: crate::transfer::Mode, files: Vec<PathBuf>) {
        if files.is_empty() {
            return;
        }
        let title = format!("{} {} to…", mode.verb(), plural_images(files.len()));
        let start = files
            .first()
            .and_then(|file| file.parent())
            .map(Path::to_path_buf);

        let mut dialog = rfd::FileDialog::new().set_title(title);
        if let Some(start) = start {
            dialog = dialog.set_directory(start);
        }
        let Some(destination) = dialog.pick_folder() else {
            return;
        };

        let planned = crate::transfer::plan(&files, &destination);
        if planned.is_empty() {
            // No time source here; the toast starts on the next frame that
            // reads the clock, which is close enough for a two-second message.
            self.delete_result = Some(("Already in that folder".to_owned(), true));
            return;
        }
        self.confirm_transfer = Some((mode, destination, planned));
    }

    /// Carries out the confirmed move or copy, then puts the library straight.
    pub fn run_transfer(&mut self) {
        let Some((mode, destination, planned)) = self.confirm_transfer.take() else {
            return;
        };
        let report = crate::transfer::run(&planned, mode);

        if let Some(library) = &self.library {
            for item in &planned {
                // A move takes the picture away from where it was indexed, so
                // the old row has to go; a copy leaves it exactly where it was.
                if mode == crate::transfer::Mode::Move && !item.source.exists() {
                    let _ = library.forget_image(&item.source.to_string_lossy());
                }
                if item.destination.exists()
                    && let Some(record) = crate::scan::record_for(&item.destination)
                {
                    let _ = library.upsert_image(&record);
                }
            }
        }
        // The marks are deliberately *not* cleared here. After a copy the
        // originals are exactly where they were and the set is still worth
        // having — copying a batch to two places in a row was otherwise two
        // rounds of picking it out. After a move the sources are gone, so the
        // rebuild below simply fails to find them and they drop out on their
        // own. Neither case needs a rule of its own.
        self.reload_library();

        let mut message = format!(
            "{} {} to {}",
            mode.done(),
            plural_images(report.done),
            destination.display()
        );
        if report.renamed > 0 {
            // Said plainly: a renamed file is not where its name says it is.
            message.push_str(&format!(" · {} renamed to avoid a clash", report.renamed));
        }
        if let Some(first) = report.failures.first() {
            message.push_str(&format!(
                " · {} failed, first: {first}",
                report.failures.len()
            ));
        }
        self.delete_result = Some((message, report.failures.is_empty()));
    }

    /// Re-indexes one restored file so it reappears in the tree.
    ///
    /// Cheaper and less surprising than a full rescan: the file is back where
    /// it was, and only its own row is missing from the library.
    pub fn start_rescan_of(&mut self, path: &Path) {
        let Some(library) = &self.library else {
            return;
        };
        if let Some(record) = crate::scan::record_for(path) {
            let _ = library.upsert_image(&record);
        }
        self.reload_library();
    }

    /// Trashes the file the dialog was asking about, then forgets it.
    ///
    /// The order matters: the library row is only dropped once the file has
    /// actually gone. Removing it first would hide a file that a failed trash
    /// left sitting on disk.
    pub fn confirm_delete(&mut self) {
        let paths = std::mem::take(&mut self.confirm_delete);
        if paths.is_empty() {
            return;
        }

        // Move off the row before anything disappears, and stay in this folder
        // while doing it: the rail can span several, and being thrown into the
        // next one because you deleted the last picture here is not what
        // "next" should mean.
        self.step_within_folder();

        let mut moved = 0_usize;
        let mut failure: Option<String> = None;
        for path in &paths {
            match crate::desktop::move_to_trash(path) {
                Ok(()) => {
                    moved += 1;
                    if let Some(library) = &self.library {
                        let _ = library.forget_image(&path.to_string_lossy());
                    }
                    self.textures.remove(path);
                    if self.current.as_deref() == Some(path.as_path()) {
                        self.current = None;
                    }
                }
                // One awkward file must not abandon the rest of the batch, but
                // it must still be reported rather than passed over in silence.
                Err(error) => {
                    failure.get_or_insert_with(|| {
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string());
                        format!("{name} — {error}")
                    });
                }
            }
        }

        self.files.clear_marks();
        self.reload_library();
        self.delete_result = Some(match &failure {
            None => (format!("Moved {} to Trash", plural_images(moved)), true),
            Some(why) => (
                format!("Moved {} to Trash; {why}", plural_images(moved)),
                false,
            ),
        });
    }

    /// Opens a file or folder from outside the tree by indexing its folder and
    /// then selecting it, so the arrow keys still walk its siblings.
    /// Indexes the pictures a selection names, and nothing else beside them.
    ///
    /// The manifest is a *subset* of a folder — the same folder holds the maps
    /// it rejected — so this is deliberately not a scan of that folder. What it
    /// produces is an ordinary set of library rows, which is what lets the
    /// Mosaic, the marking and the map queue all work on them unchanged.
    ///
    /// Everything named is indexed, including the entries mapSelector judged
    /// unusable: they are worth looking at and disagreeing with, and a reject
    /// that never appears is a reject nobody can check.
    pub fn index_manifest(&mut self, file: PathBuf, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let Some(library) = &self.library else {
            return;
        };
        let db_path = library.path().to_path_buf();

        let manifest = match crate::manifest::read(&file) {
            Ok(manifest) => manifest,
            Err(error) => {
                self.scan.error = Some(format!("{error:#}"));
                self.toast_at(&format!("{error:#}"), now);
                return;
            }
        };

        let paths: Vec<PathBuf> = manifest
            .images
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        // The folder the manifest points into, for the panel to name while it
        // works. Only for showing — nothing is walked.
        let root = paths
            .first()
            .and_then(|path| path.parent())
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| file.clone());

        self.config.manifest_path = file.to_string_lossy().into_owned();
        self.persist_config();
        self.toast_at(
            &format!(
                "{} — indexing {} pictures{}",
                manifest.name,
                paths.len(),
                match manifest.unusable() {
                    0 => String::new(),
                    n => format!(", {n} of them marked unusable"),
                },
            ),
            now,
        );
        self.scan
            .start_with(db_path, root, Mode::Manifest, Some(paths), ctx.clone());
    }

    /// Indexes a file whose *name* we do not recognise but whose *bytes* we do.
    ///
    /// The library is built by extension, which is right for sweeping a folder
    /// of thousands: opening every file to find out what it is would be absurd,
    /// and a name is a good enough guess at scale. It is the wrong rule for a
    /// file someone has pointed at directly. `bronte.enc` is a plain JPEG with
    /// a misleading extension, and refusing it because of the four letters
    /// after the dot — while the bytes say `FF D8 FF` — is being wrong on
    /// purpose about a file the user named themselves.
    ///
    /// So this applies only on a deliberate open, and only to names the scan
    /// would skip. The scan upserts and never prunes, so a row put in here
    /// survives the sweep that follows it.
    ///
    /// The header is the whole test. It says what the file *is*, not that it
    /// will decode — an ISOBMFF video sitting in a photo folder gets in on the
    /// same box a HEIC does. That is the right place to draw it: this decides
    /// whether the file is worth offering to the decoder, and the decoder
    /// already says so plainly when it cannot manage one.
    fn index_by_content(&mut self, path: &Path) {
        let extension = path.extension().unwrap_or_default().to_string_lossy();
        if crate::db::is_image_extension(&extension) {
            return; // The scan will index it by name, as usual.
        }
        let Some(description) = crate::formats::describe(path) else {
            return; // Not a header we know. `reload_library` says so.
        };
        let (Some(library), Some(record)) = (&self.library, crate::scan::record_for(path)) else {
            return;
        };
        // Recognising a file is not the same as being able to show it, and
        // this has to be the second one. `describe` names anything with a
        // header it knows — AVIF included, which this build has no decoder
        // for — whereas a probe that came back with a size means something
        // actually read the image. Without this, taking `.avif` out of
        // `IMAGE_EXTENSIONS` would achieve nothing: every AVIF opened by hand
        // would be let straight back in here.
        if record.width.is_none() || record.height.is_none() {
            return;
        }
        if library.upsert_image(&record).is_err() {
            return;
        }
        // Worth saying. The file manager would not have offered to open this,
        // and the extension is about to disagree with everything the INFO panel
        // reports about it — that is confusing unless it is named once.
        let name = file_name_of(path);
        self.opened_note = Some(match extension.is_empty() {
            true => format!("{name} — opened as {} by its contents", description.name),
            false => format!(
                "{name} — a {} file despite the .{extension}, opened anyway",
                description.name
            ),
        });
    }

    pub fn open_path(&mut self, path: PathBuf, ctx: &egui::Context) {
        // Opening something deliberately starts afresh — dropping a file on the
        // window or picking one from the dialog is not "the next picture", and
        // arriving at it still zoomed to whatever was on screen before would be
        // a surprise rather than a convenience.
        self.viewport.reset();
        // Before the scan, so the row is already there when it builds the tree.
        if !path.is_dir() {
            self.index_by_content(&path);
        }
        let Some(library) = &self.library else {
            return;
        };
        let db_path = library.path().to_path_buf();

        let folder = if path.is_dir() {
            path
        } else {
            self.pending_selection = Some(path.clone());
            match path.parent() {
                Some(parent) => parent.to_path_buf(),
                None => return,
            }
        };

        self.scan.start(db_path, folder, Mode::Scan, ctx.clone());
    }

    /// Asks for a folder and starts indexing it on a worker thread.
    pub fn start_scan(&mut self, mode: Mode, ctx: &egui::Context) {
        if self.scan.is_running() {
            return;
        }
        let Some(library) = &self.library else {
            return;
        };
        let db_path = library.path().to_path_buf();

        let title = match mode {
            Mode::Scan => "Scan a folder for images",
            Mode::Hunt => "Hunt a folder tree for images",
            Mode::Manifest => "Index the pictures a manifest names",
        };
        let Some(folder) = rfd::FileDialog::new().set_title(title).pick_folder() else {
            return;
        };

        self.scan.start(db_path, folder, mode, ctx.clone());
    }
}

#[cfg(test)]
mod placing_halves {
    use super::{centre_pixel_of, offset_centring};

    #[test]
    fn centring_on_a_pixel_puts_that_pixel_in_the_middle() {
        let size = (9504, 5274);
        // Every turn, including ones no sheet should have: a scan that is not
        // north up is exactly the case this has to survive, and the two halves
        // drifting apart as they are panned is how getting it wrong shows.
        for turn in [0.0, 0.35, -0.7, std::f64::consts::FRAC_PI_2, 2.9] {
            for zoom in [0.05_f32, 1.0, 6.0] {
                for pixel in [(0.0, 0.0), (4752.0, 2637.0), (9000.0, 300.0)] {
                    let offset = offset_centring(size, pixel, zoom, turn);
                    let back = centre_pixel_of(size, offset, zoom, turn);
                    assert!(
                        (back.0 - pixel.0).abs() < 0.5 && (back.1 - pixel.1).abs() < 0.5,
                        "turn {turn}, zoom {zoom}: {pixel:?} came back as {back:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn an_unmoved_picture_shows_its_own_middle_however_it_is_turned() {
        // The middle is the middle at any angle — turning is about that point.
        for turn in [0.0, 0.9, -2.2] {
            let middle = centre_pixel_of((800, 600), eframe::egui::Vec2::ZERO, 2.0, turn);
            assert_eq!(middle, (400.0, 300.0), "turn {turn}");
        }
    }

    #[test]
    fn a_turned_picture_pans_along_its_own_rows_not_the_screens() {
        // A quarter turn: pushing the picture right on screen has to walk *up*
        // its rows, not across them. This is the failure that made a rotated
        // scan drift out of step with the basemap while panning.
        let quarter = std::f64::consts::FRAC_PI_2;
        let moved = centre_pixel_of((800, 600), eframe::egui::vec2(100.0, 0.0), 1.0, quarter);
        assert!(
            (moved.0 - 400.0).abs() < 0.001,
            "the column should not have changed, got {moved:?}",
        );
        assert!(
            (moved.1 - 300.0 + 100.0).abs() < 0.001,
            "the row should have moved by the whole shift, got {moved:?}",
        );
    }
}

#[cfg(test)]
mod failed_conversions {
    use super::discard_failed_output;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-failed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn a_half_written_file_is_cleared_away() {
        let target = scratch("partial.tif");
        std::fs::write(&target, b"half a tiff").unwrap();
        discard_failed_output(&target, true);
        assert!(!target.exists(), "our own wreckage must not be left behind");
    }

    #[test]
    fn a_file_that_was_already_there_is_left_alone() {
        // The real loss this prevents: two sources landing on one destination,
        // where the second one's refusal deleted the first one's good output.
        let target = scratch("someone elses.tif");
        std::fs::write(&target, b"a finished conversion").unwrap();
        discard_failed_output(&target, false);
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"a finished conversion",
            "a failure must not destroy a map that converted fine",
        );
    }
}

#[cfg(test)]
mod putting_a_map_on_the_basemap {
    use crate::pages::{PAGE_MAP, PAGE_MOSAIC, PAGE_VIEWER};

    /// The rule `handing_over_from_the_viewer` applies, without needing a whole
    /// app to apply it against.
    fn hands_over(page: usize, picture_on_screen: bool) -> bool {
        page == PAGE_VIEWER && picture_on_screen
    }

    #[test]
    fn already_on_the_map_page_is_not_a_hand_off() {
        // What the bug was. Pressing Space with the map page already up took
        // the "the Viewer will set the view" branch — but there is no
        // transition to carry anything, so nothing set it, and the map went
        // down correctly placed on a basemap looking somewhere else. Which is
        // indistinguishable from the position being ignored.
        assert!(!hands_over(PAGE_MAP, true));
        assert!(!hands_over(PAGE_MOSAIC, true));
    }

    #[test]
    fn leaving_the_viewer_with_the_picture_up_is() {
        // The case worth protecting: the picture keeps the size it had, and
        // re-fitting would throw away the view just chosen.
        assert!(hands_over(PAGE_VIEWER, true));
    }

    #[test]
    fn leaving_the_viewer_before_the_picture_has_decoded_is_not() {
        // There is no size to match and no canvas to line up with, so the
        // hand-off quietly does nothing — and the fit has to happen instead.
        assert!(!hands_over(PAGE_VIEWER, false));
    }
}

#[cfg(test)]
mod which_tab {
    use crate::panels::{INFO_TABS, info_tabs};

    /// The check `images_tab_open` makes, without needing a whole app to make
    /// it against.
    fn is_images(index: usize, map_support: bool) -> bool {
        map_support && INFO_TABS.get(index).is_some_and(|tab| *tab == "IMAGES")
    }

    #[test]
    fn the_saved_index_means_the_same_tab_whatever_is_hidden() {
        // `info_tab` numbers `INFO_TABS`, not the tabs on screen. That is the
        // whole reason the panel translates: MAP and IMAGES come out of the
        // middle, so a position in the shown list would mean a different tab
        // from one run to the next — and the timeline and the fades hang off
        // this answer, which getting it wrong turns on under a different tab.
        let images = INFO_TABS
            .iter()
            .position(|tab| *tab == "IMAGES")
            .expect("there is one");
        for show_debug in [true, false] {
            for map_support in [true, false] {
                let shown = info_tabs(show_debug, map_support);
                assert_eq!(
                    shown.contains(&"IMAGES"),
                    map_support,
                    "{show_debug} {map_support}"
                );
                // Whatever is on screen, the number has not moved.
                assert_eq!(INFO_TABS[images], "IMAGES");
            }
        }
        assert!(is_images(images, true));
    }

    #[test]
    fn the_images_tab_takes_the_anchors_off_the_map() {
        // Belt and braces on the two conditions that hide and lock them, so a
        // later change to either cannot quietly leave pins draggable on the one
        // tab where every drag is aimed at something else.
        for (index, tab) in INFO_TABS.iter().enumerate() {
            assert_eq!(is_images(index, true), *tab == "IMAGES", "{tab}");
        }
    }

    #[test]
    fn no_other_tab_answers_to_it() {
        // An index past the end — which is what a saved tab from a build with
        // more tabs looks like — is not the IMAGES tab either.
        assert!(!is_images(99, true));
        // And with map support off nothing is, however the index was saved:
        // the tab is not on screen, so the timeline behind it must stay put.
        for index in 0..INFO_TABS.len() {
            assert!(!is_images(index, false), "{index}");
        }
    }
}

#[cfg(test)]
mod config_round_trip {
    use super::Config;

    #[test]
    fn every_setting_survives_being_written_and_read_back() {
        let before = Config {
            map_export_root: "/mnt/converted maps".to_owned(),
            tile_contact: "morgan@example.com".to_owned(),
            decode_threads: 7,
            mosaic_filenames: true,
            // Both away from their defaults, or the test would pass on a flag
            // that is never written at all: `after` starts at the defaults too.
            anchors_locked: false,
            show_debug_tab: true,
            ..Config::default()
        };

        let mut after = Config::default();
        after.apply(&before.encode());

        assert_eq!(after.map_export_root, before.map_export_root);
        assert_eq!(after.tile_contact, before.tile_contact);
        assert_eq!(after.decode_threads, before.decode_threads);
        assert_eq!(after.mosaic_filenames, before.mosaic_filenames);
        // The two newest flags travel with the rest. The lock in particular:
        // it is there to stop work being lost, and one that forgot itself
        // between runs would be off on exactly the day it was needed.
        assert_eq!(after.anchors_locked, before.anchors_locked);
        assert_eq!(after.show_debug_tab, before.show_debug_tab);
    }

    #[test]
    fn a_folder_with_a_comma_does_not_swallow_the_settings_after_it() {
        // The settings are one comma-separated line, so a comma in a value
        // would end the value early and turn the rest into nonsense.
        let before = Config {
            map_export_root: "/mnt/maps, old".to_owned(),
            image_vram_mb: 1234,
            ..Config::default()
        };

        let mut after = Config::default();
        after.apply(&before.encode());
        assert_eq!(after.image_vram_mb, 1234, "the setting after it survived");
        assert!(!after.map_export_root.contains(','));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_round_trips() {
        assert_eq!(
            parse_geometry("100,50,1280,720"),
            Some((100.0, 50.0, 1280.0, 720.0))
        );
        assert_eq!(
            parse_geometry("-20,-10,800,600"),
            Some((-20.0, -10.0, 800.0, 600.0))
        );
    }

    /// The rule `index_by_content` applies, without needing an app or a
    /// library: a name the scan would skip, a header we recognise, and a probe
    /// that actually came back with a size.
    fn opens_by_content(path: &Path) -> bool {
        let extension = path.extension().unwrap_or_default().to_string_lossy();
        !crate::db::is_image_extension(&extension)
            && crate::formats::describe(path).is_some()
            && crate::scan::record_for(path)
                .is_some_and(|record| record.width.is_some() && record.height.is_some())
    }

    /// Every AVIF in the AOMediaCodec conformance set, if it is to hand.
    ///
    /// Run against the real corpus rather than a synthetic file: AVIF is
    /// recognised by `describe` and refused by every decoder in this build, so
    /// it is the exact shape the probe gate exists to catch — and a hand-made
    /// stand-in would prove nothing about the real thing.
    ///
    /// ```text
    /// GEO_REF_MAPPER_SURVEY_DIR=…/testdata cargo test --release avif_is_not_let_in -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_SURVEY_DIR"]
    fn avif_is_not_let_in_through_the_back_door() {
        let Some(root) = std::env::var_os("GEO_REF_MAPPER_SURVEY_DIR") else {
            panic!("set GEO_REF_MAPPER_SURVEY_DIR to the downloaded test corpus");
        };
        let avifs: Vec<PathBuf> = walkdir::WalkDir::new(PathBuf::from(root))
            .into_iter()
            .filter_map(Result::ok)
            .map(|entry| entry.path().to_path_buf())
            .filter(|path| {
                path.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("avif"))
            })
            .collect();
        assert!(!avifs.is_empty(), "no AVIFs in the corpus");

        // Not indexed by name...
        assert!(!crate::db::is_image_extension("avif"));
        // ...and not indexed by content either, which is the half that a
        // change to the sniffing could silently undo.
        let leaked: Vec<&PathBuf> = avifs.iter().filter(|path| opens_by_content(path)).collect();
        assert!(
            leaked.is_empty(),
            "{} of {} AVIFs would be indexed: {:?}",
            leaked.len(),
            avifs.len(),
            &leaked[..leaked.len().min(3)],
        );
        println!(
            "\n{} AVIFs, none indexed by name or by content\n",
            avifs.len()
        );
    }

    #[test]
    fn a_misnamed_image_is_opened_on_its_bytes() {
        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-sniff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir");

        // The real case: a baseline JPEG called `.enc`, which is what a file
        // manager hands over when the name says "encrypted" and the bytes do
        // not agree.
        let mut jpeg = Vec::new();
        image::RgbImage::from_fn(16, 16, |x, y| {
            image::Rgb([(x * 16) as u8, (y * 16) as u8, 90])
        })
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .expect("encodes");
        let misnamed = dir.join("bronte.enc");
        std::fs::write(&misnamed, &jpeg).expect("written");
        assert!(
            opens_by_content(&misnamed),
            "a JPEG is a JPEG whatever it is called"
        );

        // A name the scan already handles is left to the scan — doing it twice
        // would only be slower.
        let honest = dir.join("bronte.jpg");
        std::fs::write(&honest, &jpeg).expect("written");
        assert!(!opens_by_content(&honest));

        // Something genuinely not an image stays refused, which is the whole
        // reason the header rather than the extension is the test.
        let encrypted = dir.join("secret.enc");
        std::fs::write(&encrypted, [0x27_u8; 512]).expect("written");
        assert!(!opens_by_content(&encrypted));

        // And an empty file cannot be read into a header at all.
        let empty = dir.join("empty.enc");
        std::fs::write(&empty, []).expect("written");
        assert!(!opens_by_content(&empty));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scaled_window_is_saved_at_the_size_it_will_be_restored_at() {
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(1180.0, 760.0));
        // At 100% the two units are the same thing and nothing moves.
        assert_eq!(geometry_to_save(rect, 1.0), (100.0, 50.0, 1180.0, 760.0));

        // At 200% egui has already halved what the window manager reports, so
        // saving the rect as it arrives would reopen the window at half size.
        // What goes to disk is the size winit will be asked for.
        let halved = egui::Rect::from_min_size(egui::pos2(50.0, 25.0), egui::vec2(590.0, 380.0));
        let saved = geometry_to_save(halved, 2.0);
        assert_eq!(saved, (100.0, 50.0, 1180.0, 760.0));

        // And it survives the trip through the setting, which is what `main`
        // actually reads on the way back.
        let (x, y, width, height) = saved;
        assert_eq!(
            parse_geometry(&format!("{x:.0},{y:.0},{width:.0},{height:.0}")),
            Some((100.0, 50.0, 1180.0, 760.0))
        );
    }

    #[test]
    fn implausible_geometry_is_rejected() {
        assert!(parse_geometry("0,0,0,0").is_none(), "zero size");
        assert!(parse_geometry("0,0,10,10").is_none(), "far too small");
        assert!(
            parse_geometry("0,0,999999,999999").is_none(),
            "far too large"
        );
        assert!(parse_geometry("0,0,800").is_none(), "missing a field");
        assert!(parse_geometry("").is_none());
        assert!(parse_geometry("junk,junk,junk,junk").is_none());
    }

    #[test]
    fn image_counts_read_as_english() {
        assert_eq!(plural_images(1), "1 image");
        assert_eq!(plural_images(12), "12 images");
        assert_eq!(plural_images(0), "0 images");
    }

    #[test]
    fn config_survives_a_round_trip() {
        let mut config = Config::default();
        config.checkerboard = false;
        config.mosaic_filenames = true;
        config.map_support = false;
        config.image_vram_mb = 2048;
        config.thumbnail_vram_mb = 512;

        let mut restored = Config::default();
        restored.apply(&config.encode());

        assert!(!restored.checkerboard);
        assert!(restored.mosaic_filenames);
        assert!(!restored.map_support);
        assert_eq!(restored.image_vram_mb, 2048);
        assert_eq!(restored.thumbnail_vram_mb, 512);
        assert_eq!(restored.encode(), config.encode());
    }

    #[test]
    fn the_interface_scale_survives_a_round_trip() {
        let mut config = Config::default();
        config.ui_scale = 1.25;
        let mut restored = Config::default();
        restored.apply(&config.encode());
        assert!((restored.ui_scale - 1.25).abs() < 0.001);
        assert_eq!(restored.encode(), config.encode());
    }

    #[test]
    fn a_scale_outside_the_range_is_brought_back_into_it() {
        // A hand-edited row, or one written by a build with a wider slider.
        // Zero is the one that matters: it would scale the interface out of
        // existence, and there would be no slider left to put it back with.
        for (written, expected) in [
            ("ui_scale=0", MIN_UI_SCALE),
            ("ui_scale=0.1", MIN_UI_SCALE),
            ("ui_scale=99", MAX_UI_SCALE),
            ("ui_scale=-4", MIN_UI_SCALE),
        ] {
            let mut config = Config::default();
            config.apply(written);
            assert!(
                (config.ui_scale - expected).abs() < 0.001,
                "{written} gave {}",
                config.ui_scale
            );
        }
        // Not a number at all leaves the default rather than becoming zero.
        let mut config = Config::default();
        config.apply("ui_scale=large");
        assert!((config.ui_scale - 1.0).abs() < 0.001);
    }

    #[test]
    fn the_scale_slider_is_read_when_it_is_let_go_of() {
        // A drag, frame by frame. Nothing is committed while the pointer is
        // down, however far the slider has been moved — applying it live is
        // what pulls the track out from under the pointer holding it.
        let mut config = 1.0_f32;
        let mut pending = None;
        for held in [1.05_f32, 1.2, 1.35] {
            pending = Some(held);
            if let Some(released) = release_ui_scale(pending, true) {
                config = released;
                pending = None;
            }
            assert!((config - 1.0).abs() < 0.001, "applied mid-drag at {held}");
        }

        // The pointer comes up: the value it was let go at is the setting.
        let released = release_ui_scale(pending, false).expect("commits on release");
        assert!((released - 1.35).abs() < 0.001);

        // And with nothing held, releasing the pointer does not invent a
        // change — every click anywhere in the window arrives here.
        assert!(release_ui_scale(None, false).is_none());
    }

    #[test]
    fn the_slider_and_the_keyboard_take_turns_rather_than_fighting() {
        // Nothing has moved: the setting stands.
        assert!((resolve_ui_scale(1.0, 1.0, 1.0) - 1.0).abs() < 0.001);
        // The slider moved. egui still reports the old factor this frame —
        // `set_zoom_factor` lands at the start of the next pass — so the
        // setting has to win against a `live` that has not caught up yet.
        assert!((resolve_ui_scale(1.0, 1.0, 1.25) - 1.25).abs() < 0.001);
        // Ctrl+plus moved it behind our back: egui wins, and the slider
        // follows rather than snapping it back on the next frame.
        assert!((resolve_ui_scale(1.1, 1.0, 1.0) - 1.1).abs() < 0.001);
        // And past the ends of the slider it is brought back in.
        assert!((resolve_ui_scale(8.0, 1.0, 1.0) - MAX_UI_SCALE).abs() < 0.001);
        assert!((resolve_ui_scale(1.0, 1.0, MAX_UI_SCALE + 1.0) - MAX_UI_SCALE).abs() < 0.001);
        assert!((resolve_ui_scale(0.2, 1.0, 1.0) - MIN_UI_SCALE).abs() < 0.001);
        // A setting that is not a number cannot be allowed to become the
        // scale: every rect in the window would be drawn at NaN points.
        assert!((resolve_ui_scale(1.0, 1.0, f32::NAN) - 1.0).abs() < 0.001);
    }

    #[test]
    fn a_config_saved_before_the_switch_existed_still_has_its_maps() {
        // The switch is new, so no saved row mentions it. Falling back to the
        // default has to leave the program doing what it did — anything else
        // and an existing library loses the Map page on upgrade with nothing
        // saying why.
        let mut config = Config::default();
        config.apply("checkerboard=0,mosaic_filenames=1");
        assert!(config.map_support);
    }

    #[test]
    fn an_unknown_or_missing_setting_leaves_the_default_alone() {
        // What an older or newer build's row looks like. Every setting it does
        // not mention must keep its default rather than being zeroed.
        let mut config = Config::default();
        let threads = config.decode_threads;
        config.apply("checkerboard=0,something_we_removed=1");

        assert!(!config.checkerboard, "the key it did know was applied");
        assert!(config.show_filmstrip, "an absent key keeps its default");
        assert_eq!(config.decode_threads, threads);
    }

    #[test]
    fn a_stale_budget_cannot_ask_for_more_than_the_sliders_allow() {
        let mut config = Config::default();
        config.apply("image_vram_mb=999999,thumbnail_vram_mb=0");
        assert_eq!(config.image_vram_mb, 8192);
        assert_eq!(config.thumbnail_vram_mb, 64);
    }

    #[test]
    fn thread_counts_are_clamped_to_what_the_pools_will_spawn() {
        // A saved row asking for 900 workers must not try to spawn 900 threads.
        let mut config = Config::default();
        config.apply("thumbnail_threads=900,decode_threads=0");
        assert_eq!(config.thumbnail_threads, crate::thumbs::MAX_THUMB_WORKERS);
        assert_eq!(config.decode_threads, 1);
        assert!(config.encode().contains("thumbnail_threads="));
    }

    #[test]
    fn the_version_says_which_profile_the_build_used() {
        let version = crate::version::full_version();
        // Derived from the manifest rather than spelled out: a literal here
        // would mean every version bump broke the build for no good reason.
        let expected: String = env!("CARGO_PKG_VERSION")
            .split('.')
            .take(2)
            .collect::<Vec<_>>()
            .join(".");
        assert!(version.starts_with(&expected), "{version}");
        // The tests themselves are a debug build unless run with --release, so
        // this asserts whichever way round it was compiled.
        assert_eq!(
            version.contains("(debug)"),
            cfg!(debug_assertions),
            "the profile marker must match the build: {version}"
        );
        assert_eq!(
            version.contains("(release)"),
            !cfg!(debug_assertions),
            "one of the two must always be shown: {version}"
        );
    }

    #[test]
    fn sections_round_trip() {
        let states = [("LOADING · THIS IMAGE", true), ("FRAME", false)];
        let encoded = encode_sections(&states);
        let decoded = decode_sections(&encoded);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0], ("LOADING · THIS IMAGE".to_owned(), true));
        assert_eq!(decoded[1], ("FRAME".to_owned(), false));
    }

    #[test]
    fn unrecognised_section_entries_are_skipped() {
        // Titles contain '=' nowhere, but a stale or truncated value must not
        // take the whole restore down with it.
        let decoded = decode_sections("GOOD=1,,broken,ALSO=0");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].0, "GOOD");
        assert_eq!(decoded[1].0, "ALSO");
    }
}

fn log_stats_error(error: &anyhow::Error) {
    eprintln!("geo-ref_mapper: could not read library stats: {error:#}");
}

impl eframe::App for GeoRefMapperApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let [r, g, b, a] = theme::WINDOW_BG.to_normalized_gamma_f32();
        [r, g, b, a]
    }

    fn on_exit(&mut self) {
        // Geometry is only worth one write, at the end.
        if let (Some(library), Some((x, y, width, height))) = (&self.library, self.geometry) {
            let value = format!("{x:.0},{y:.0},{width:.0},{height:.0}");
            let _ = library.set_setting(SETTING_WINDOW, &value);
        }

        eprintln!(
            "geo-ref_mapper: shutting down after {:.0}s, {} frames",
            self.metrics.uptime().as_secs_f32(),
            self.metrics.frames
        );
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.apply_ui_scale(&ctx);

        // Raised here rather than where it is noticed: the scan lands on a
        // worker's schedule, and `toast_at` needs a frame time.
        if let Some(note) = self.opened_note.take() {
            let now = ctx.input(|i| i.time);
            self.toast_at(&note, now);
        }

        // Diagnostic: report who asked the window to close.
        if ctx.input(|i| i.viewport().close_requested()) {
            let ours = chrome::CLOSE_REQUESTED_BY_BUTTON.load(std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "geo-ref_mapper: close requested ({}) at {:.0}s",
                if ours {
                    "the app itself"
                } else {
                    "external / window manager"
                },
                self.metrics.uptime().as_secs_f32(),
            );
        }
        self.metrics.begin_frame(&ctx);
        // Before anything asks for a texture: the card refused one during the
        // last frame, and drawing this one as though it had not would simply
        // ask again for what there is no room for.
        if let Some(source) = crate::oom::take_refusal() {
            self.shed_textures(&ctx, &source);
        }
        self.thumb_textures.begin_frame();
        self.mini_textures.begin_frame();
        self.handle_input(&ctx);
        if self.scan.poll() {
            self.reload_library();
            // One root at a time; the next queued folder starts as this ends.
            self.start_next_refresh(&ctx);
        }
        // Budgets are editable on the Config page, so keep the caches in step.
        if self.textures.budget() != self.config.image_vram_mb * MB {
            self.textures.set_budget(self.config.image_vram_mb * MB);
        }
        if self.thumb_textures.budget() != self.config.thumbnail_vram_mb * MB {
            self.thumb_textures
                .set_budget(self.config.thumbnail_vram_mb * MB);
            self.mini_textures
                .set_budget((self.config.thumbnail_vram_mb * MB / 10).max(16 * MB));
        }

        // Track geometry so `on_exit` can save it; the viewport reports the
        // outer rect including decorations, which is what we restore.
        //
        // Taken back out of the interface scale first. egui divides this rect
        // by `zoom_factor × native_pixels_per_point`, but `main` restores it
        // through winit's `with_inner_size`, which knows only the native
        // factor. Saved as it arrives, a window at 2.0 would come back half
        // the size it was left — and half again on the restart after that.
        //
        // Not while only the picture is on screen: that geometry — full
        // screen, or the picture-sized window an `geo-ref_mapper photo.jpg` start
        // makes for itself — belongs to the moment, and saving it would hand
        // the next panelled session a window it never asked for.
        // The window exists and is about to have a picture on it, which is
        // what the launcher's busy cursor has been waiting for.
        crate::startup::announce_ready();
        self.apply_launch_window(&ctx);
        self.claim_keyboard(&ctx);
        let scale = ctx.zoom_factor();
        if !self.presenting {
            ctx.input(|i| {
                if let Some(rect) = i.viewport().outer_rect {
                    self.geometry = Some(geometry_to_save(rect, scale));
                }
            });
        }
        self.persist_selection();
        self.persist_panel_state(&ctx);

        self.sync_selection();
        self.drain_decodes(&ctx);
        self.drain_thumbnails(&ctx);
        self.drain_detail(&ctx);
        self.poll_vram(&ctx);
        self.poll_georeference_sweep();
        self.poll_map_conversion(&ctx);
        self.squares.poll(&ctx);
        if let Some(overlay) = &mut self.map_overlay {
            overlay.squares.poll(&ctx);
        }
        // The maps faded up from the IMAGES list have squares of their own, and
        // nothing else polls them — without this they stay blank however long
        // you wait, because decoded squares are never uploaded.
        for overlay in &mut self.map_extras {
            overlay.squares.poll(&ctx);
        }
        // The placing session has squares of its own, and nothing else polls
        // them — without this the picture half stays blank however long you
        // wait, because decoded squares are never uploaded.
        if let Some(session) = &mut self.placing {
            session.squares.poll(&ctx);
        }
        // A click on a sheet that no second click overtook.
        let now = ctx.input(|input| input.time);
        if self.pending_reveal.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
        }
        self.poll_pending_reveal(now);
        self.address.poll();
        if self.scan.is_running() {
            // Keep the progress readout moving while the worker runs.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Presentation mode draws the picture and nothing else — no chrome, no
        // panels, no strip. Returning early is what makes that true rather than
        // nearly true.
        if self.presenting {
            pages::show(ui, self);
            crate::widgets::toast(ui, self);
            self.metrics.end_frame();
            return;
        }

        chrome::resize_grips(ui.ctx());

        Panel::top("title_bar")
            .exact_size(theme::TITLE_BAR_HEIGHT)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::NONE)
            .show(ui, |ui| chrome::title_bar(ui, self));

        // `show_collapsible` wants `&mut bool`, so the flag is copied out and
        // written back rather than borrowed out of `self` twice.
        let mut files_open = self.files_open;
        Panel::left("files_panel")
            .resizable(true)
            .default_size(FILES_WIDTH)
            .size_range(110.0..=560.0)
            .frame(panel_frame())
            .show_collapsible(ui, &mut files_open, |ui| panels::files_panel(ui, self));
        self.files_open = files_open;

        let mut info_open = self.info_open;
        Panel::right("info_panel")
            .resizable(true)
            .default_size(INFO_WIDTH)
            // A low floor on purpose: the old 220 pt minimum meant dragging the
            // edge in snapped the panel shut long before it looked narrow, so a
            // deliberately slim INFO panel was not something you could have.
            .size_range(110.0..=560.0)
            .frame(panel_frame())
            .show_collapsible(ui, &mut info_open, |ui| panels::info_panel(ui, self));
        self.info_open = info_open;

        pages::show(ui, self);
        // Last, so it draws over whichever page is open.
        crate::widgets::confirm_delete_dialog(ui, self);
        crate::widgets::confirm_transfer_dialog(ui, self);
        crate::widgets::install_prompt_dialog(ui, self);
        crate::widgets::toast(ui, self);
        self.apply_thumbnail_threads(&ctx);
        self.persist_config();

        self.metrics.end_frame();
    }
}

impl GeoRefMapperApp {
    /// Keyboard shortcuts and dropped files. Zoom that needs a cursor position
    /// is handled on the canvas itself.
    /// Switches page, remembering where from so Escape can undo the jump.
    /// Puts the selected picture on the map, or takes it off again.
    ///
    /// Bound to the space bar, which for a picture that says where it is is a
    /// far better use of the key than repeating what Down already does. A
    /// picture with no position is left alone rather than being complained
    /// about — a key that scolds you for pressing it is worse than one that
    /// does nothing.
    pub fn toggle_on_map(&mut self, ctx: &egui::Context) {
        if !self.config.map_support {
            return;
        }
        // Read from the tree, not from `current`.
        //
        // The keys are handled at the top of the frame and the selection is
        // copied into `current` further down it, so on the frame Space is
        // pressed `current` is still the picture that was selected before —
        // along with its position. Placing that one put the *previous* map's
        // position on the basemap and fitted the view to it; a moment later the
        // overlay was swapped to the picture actually selected, deliberately
        // without moving the view. The right map on a basemap looking at the
        // wrong place, which is what "it places it wrongly" looked like.
        //
        // The SHOW button never showed it: a click is handled while the panel
        // is drawn, which is after the selection has caught up.
        let Some(path) = self.selected_image_path() else {
            return;
        };
        if self
            .map_overlay
            .as_ref()
            .is_some_and(|overlay| overlay.path == path)
        {
            self.map_overlay = None;
            return;
        }

        // Cached only while it belongs to this picture; read from the file
        // otherwise, for the same reason the path is.
        let geo = if self.current.as_ref() == Some(&path) {
            self.current_geo.clone()
        } else {
            crate::geotiff::from_path(&path)
        };
        let Some(geo) = geo else {
            return;
        };
        let (Some(north_west), Some(south_east)) = (geo.top_left(), geo.bottom_right()) else {
            // A projection the map cannot agree with. Placing it would put the
            // picture confidently in the wrong place.
            return;
        };

        match crate::map::load_overlay(ctx, &path, geo) {
            Ok(overlay) => {
                self.map_overlay = Some(overlay);
                // Placed from the Viewer with the picture on screen, it keeps
                // the size it already had — `enter_page` carries the scale
                // across, and re-fitting would throw away the view just chosen.
                //
                // Any other way in, there is no scale worth keeping and the map
                // is fitted to the window. That includes pressing Space in the
                // tree while the Viewer happens to be the page underneath: the
                // picture is not decoded yet, so the hand-off has nothing to
                // work from — and this used to skip the fit anyway, leaving the
                // map correctly placed on a basemap looking somewhere else
                // entirely. Which reads exactly like the position being
                // ignored.
                if !self.handing_over_from_the_viewer() {
                    self.map_view.fit(north_west, south_east, self.map_window);
                }
                self.go_to_page(crate::pages::PAGE_MAP);
            }
            Err(reason) => {
                let now = ctx.input(|i| i.time);
                self.toast_at(&format!("Could not place the picture — {reason}"), now);
            }
        }
    }

    /// Starts converting everything on the queue that has not been done yet.
    ///
    /// One at a time on one thread. Converting is heavy — decoding a whole map
    /// and compressing every square of every layer — and running several at
    /// once would take the interface down with it for no gain, since the work
    /// is the same either way.
    pub fn start_map_conversion(&mut self, now: f64) {
        if self.converting.is_some() || !self.config.map_support {
            return;
        }
        let root = std::path::PathBuf::from(&self.config.map_export_root);
        if root.as_os_str().is_empty() {
            self.toast_at("Choose a folder to write converted maps to first", now);
            return;
        }
        let waiting: Vec<(i64, std::path::PathBuf)> = self
            .map_queue
            .iter()
            .filter(|item| item.written.is_none())
            .map(|item| (item.id, std::path::PathBuf::from(&item.path)))
            .collect();
        if waiting.is_empty() {
            self.toast_at("Nothing left to convert", now);
            return;
        }

        let (sender, receiver) = std::sync::mpsc::channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.converting = Some(Conversion {
            events: receiver,
            stop: std::sync::Arc::clone(&stop),
            done: 0,
            total: waiting.len(),
            current: String::new(),
            progress: None,
        });

        let library_path = self.library.as_ref().map(|library| library.path.clone());
        std::thread::spawn(move || {
            let library = library_path
                .as_deref()
                .and_then(|path| Library::open(path).ok());
            for (id, source) in waiting {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let name = source
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if sender.send(ConversionEvent::Started(name)).is_err() {
                    return;
                }

                let Some(target) = crate::pages::target_for(&source, &root) else {
                    continue;
                };
                // Asked *before* converting, because it decides who owns the
                // file if this goes wrong. See `discard_failed_output`.
                let ours = !target.exists();
                let watching = sender.clone();
                let flag = std::sync::Arc::clone(&stop);
                let outcome = crate::convert::convert(&source, &target, move |progress| {
                    let carry_on = !flag.load(std::sync::atomic::Ordering::Relaxed);
                    watching.send(ConversionEvent::Progress(progress)).is_ok() && carry_on
                });

                match outcome {
                    Ok(report) => {
                        if let Some(library) = &library {
                            let _ = library.mark_converted(id, &report.target.to_string_lossy());
                        }
                        if sender.send(ConversionEvent::Finished(Ok(report))).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let why = format!("{error:#}");
                        if let Some(library) = &library {
                            let _ = library.mark_conversion_failed(id, &why);
                        }
                        discard_failed_output(&target, ours);
                        if sender.send(ConversionEvent::Finished(Err(why))).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = sender.send(ConversionEvent::AllDone);
        });
    }

    pub fn stop_map_conversion(&mut self) {
        if let Some(running) = &self.converting {
            running
                .stop
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Picks up whatever the conversion thread has reported.
    pub fn poll_map_conversion(&mut self, ctx: &egui::Context) {
        let Some(running) = &mut self.converting else {
            return;
        };
        let mut finished = false;
        let mut refresh = false;
        let mut said = None;
        loop {
            match running.events.try_recv() {
                Ok(ConversionEvent::Started(name)) => {
                    running.current = name;
                    running.progress = None;
                }
                Ok(ConversionEvent::Progress(progress)) => running.progress = Some(progress),
                Ok(ConversionEvent::Finished(outcome)) => {
                    running.done += 1;
                    refresh = true;
                    said = Some(match outcome {
                        Ok(report) => format!(
                            "{} — {} layers, {} from {}{}",
                            report
                                .target
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            report.layers,
                            crate::files::format_bytes(report.bytes),
                            crate::files::format_bytes(report.source_bytes),
                            // Said out loud. Replacing is the right thing to do
                            // with a file of unknown provenance, but it is still
                            // a file destroyed, and two sources that land on one
                            // name would otherwise do it in silence.
                            if report.replaced { " · replaced" } else { "" },
                        ),
                        Err(why) => why,
                    });
                }
                Ok(ConversionEvent::AllDone) => finished = true,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if refresh {
            self.reload_map_queue();
        }
        if let Some(message) = said {
            let now = ctx.input(|i| i.time);
            self.toast_at(&message, now);
        }
        if finished {
            self.converting = None;
            // Anything added while that run was going is still waiting: the
            // worker took its list of work when it started. With auto-convert
            // on, pick those up now rather than leaving them to look queued and
            // forgotten.
            //
            // It cannot spin: a run that converts nothing marks each map either
            // written or failed, and both come off the waiting list.
            let now = ctx.input(|i| i.time);
            self.maybe_auto_convert(now);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    pub fn reload_anchors(&mut self) {
        let Some(library) = &self.library else { return };
        // The shared ones, plus any belonging to the picture in hand. Another
        // picture's own anchors are deliberately absent: they mark places that
        // exist only on it.
        let mine = self
            .placing
            .as_ref()
            .and_then(|session| session.image_id)
            .or_else(|| {
                self.current
                    .as_deref()
                    .and_then(|path| self.index.id_of(path))
            });
        self.anchors = library.anchors_for_image(mine).unwrap_or_default();
    }

    /// Adds or removes the picture on screen from the marked set.
    ///
    /// The point of it is going through a folder one picture at a time and
    /// picking out the keepers as they come — which is the Viewer's whole job,
    /// and until now needed the tree open and a ctrl-click each time.
    ///
    /// It says how many are marked afterwards, because the Viewer can be full
    /// screen with no tree in sight, and a keystroke with no visible effect is
    /// indistinguishable from one that did nothing.
    pub fn toggle_mark_on_current(&mut self, now: f64) {
        let Some(path) = self.current.clone() else {
            return;
        };
        let Some(node) = self.files.find_by_path(&path) else {
            return;
        };
        self.files.toggle_mark(&node);

        let marked = self.files.action_paths().len();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let held = self.files.is_marked(&node);
        self.toast_at(
            &match (held, marked) {
                (true, 1) => format!("{name} marked"),
                (true, count) => format!("{name} marked — {count} in all"),
                (false, 0) => format!("{name} unmarked — none left"),
                (false, count) => format!("{name} unmarked — {count} left"),
            },
            now,
        );
    }

    /// Marks a place. The name is a starting point, not a decision — the panel
    /// lets it be changed straight away.
    pub fn add_anchor(&mut self, lat: f64, lon: f64) {
        let Some(library) = &self.library else { return };
        let name = format!("Anchor {}", self.anchors.len() + 1);
        if library.add_anchor(&name, lat, lon).is_ok() {
            self.reload_anchors();
        }
    }

    /// Takes an anchor back off the picture being placed.
    ///
    /// A shared anchor is only unmarked — it is a place on the earth that other
    /// pictures use, and this one is simply no longer among them.
    ///
    /// One made *for* this picture is deleted outright. Unmarking it would
    /// leave a pin on the basemap standing for nothing, and — now that the list
    /// shows only the anchors in use — with no row left anywhere to reach it
    /// by. An anchor that cannot be seen and cannot be removed is worse than no
    /// anchor at all.
    pub fn unmark_anchor(&mut self, id: i64) {
        let mine = self.placing.as_ref().and_then(|session| session.image_id);
        let owned = self.anchors.iter().any(|held| {
            held.id == id && held.owner_image_id.is_some() && held.owner_image_id == mine
        });

        if let Some(session) = &mut self.placing {
            session.placed.remove(&id);
            if session.choosing == Some(id) {
                session.choosing = None;
            }
        }
        if owned {
            if let Some(library) = &self.library {
                let _ = library.delete_anchor(id);
            }
            self.reload_anchors();
        }
    }

    /// Marks a place that belongs to this picture and no other.
    ///
    /// For the spot that is only meaningful on *this* scan — a printed cross, a
    /// corner of the sheet, a building since demolished. Adding those to the
    /// shared set would make the list longer for every other map and claim a
    /// place exists that nobody else can find.
    ///
    /// Chosen at once, so the next click on the picture puts it down: making one
    /// is only ever the first half of using it.
    pub fn add_anchor_here_only(&mut self, lat: f64, lon: f64, now: f64) {
        let Some(image_id) = self
            .placing
            .as_ref()
            .and_then(|session| session.image_id)
            .or_else(|| {
                self.current
                    .as_deref()
                    .and_then(|path| self.index.id_of(path))
            })
        else {
            self.toast_at("The picture is not in the library yet", now);
            return;
        };
        let Some(library) = &self.library else { return };

        let mine = self
            .anchors
            .iter()
            .filter(|held| held.owner_image_id == Some(image_id))
            .count();
        let name = format!("Here {}", mine + 1);
        match library.add_anchor_owned(&name, lat, lon, Some(image_id)) {
            Ok(id) => {
                self.reload_anchors();
                if let Some(session) = &mut self.placing {
                    session.choosing = Some(id);
                }
                self.toast_at("Added for this picture — now click it on the scan", now);
            }
            Err(error) => self.toast_at(&format!("{error:#}"), now),
        }
    }

    pub fn rename_anchor(&mut self, id: i64, name: &str) {
        if let Some(library) = &self.library {
            let _ = library.rename_anchor(id, name);
        }
        if let Some(pin) = self.anchors.iter_mut().find(|pin| pin.id == id) {
            pin.name = name.to_owned();
        }
    }

    /// Moves a pin, in memory only. Dragging fires every frame, and writing to
    /// the library each time would be hundreds of writes for one gesture.
    pub fn drag_anchor_to(&mut self, id: i64, lat: f64, lon: f64) {
        if let Some(pin) = self.anchors.iter_mut().find(|pin| pin.id == id) {
            pin.lat = lat;
            pin.lon = lon;
        }
    }

    /// Writes a dragged anchor's new place, once the hand is off it.
    pub fn finish_anchor_drag(&mut self, id: i64) {
        let Some(pin) = self.anchors.iter().find(|pin| pin.id == id).cloned() else {
            return;
        };
        if let Some(library) = &self.library {
            let _ = library.move_anchor(id, pin.lat, pin.lon);
        }
    }

    pub fn delete_anchor(&mut self, id: i64) {
        if let Some(library) = &self.library {
            let _ = library.delete_anchor(id);
        }
        self.anchors.retain(|pin| pin.id != id);
    }

    /// Starts placing the selected picture by hand.
    pub fn start_placing(&mut self, ctx: &egui::Context) {
        let Some(path) = self.current.clone() else {
            return;
        };
        let mut squares = crate::imagetiles::Squares::new(ctx);
        squares.open(&path);
        let size = squares
            .shapes()
            .first()
            .map(|shape| (shape.width, shape.height))
            .unwrap_or_default();
        if size.0 == 0 {
            let now = ctx.input(|i| i.time);
            self.toast_at(
                "That picture is not stored in squares, so it cannot be placed",
                now,
            );
            return;
        }

        let image_id = self.index.id_of(&path);
        // Anything placed on this picture before is picked up, so a session can
        // be left and come back to.
        let placed = image_id
            .and_then(|id| {
                self.library
                    .as_ref()
                    .and_then(|l| l.anchors_on_image(id).ok())
            })
            .map(|held| held.into_iter().map(|a| (a.id, a.pixel)).collect())
            .unwrap_or_default();

        self.placing = Some(Placing {
            path,
            image_id,
            size,
            placed,
            choosing: None,
            locked: true,
            upright: true,
            dragging_mark: None,
            zoom: 0.0, // worked out on the first draw, once the half is known
            offset: egui::Vec2::ZERO,
            squares,
        });
        self.switch_page(crate::pages::PAGE_MAP);
    }

    /// Finishes placing: works out where the picture goes and keeps the result.
    pub fn finish_placing(&mut self, ctx: &egui::Context) {
        let Some(session) = self.placing.take() else {
            return;
        };
        let now = ctx.input(|i| i.time);

        // Written now rather than as they were placed: until Done is pressed
        // this was a working state, and abandoning it leaves the picture as it
        // was.
        if let (Some(id), Some(library)) = (session.image_id, self.library.as_ref()) {
            let _ = library.clear_anchors_on_image(id);
            for (anchor, pixel) in &session.placed {
                let _ = library.place_anchor_on_image(id, *anchor, pixel.0, pixel.1);
            }
        }
        self.reload_placement();

        match &self.placement {
            Some(Ok(fit)) => self.toast_at(
                &format!(
                    "Placed within {:.0} m on average, worst {:.0} m",
                    fit.average_metres, fit.worst_metres
                ),
                now,
            ),
            Some(Err(refused)) => self.toast_at(refused.why(), now),
            None => {}
        }
    }

    pub fn cancel_placing(&mut self) {
        self.placing = None;
    }

    /// Re-solves the placement for the selected picture.
    pub fn reload_placement(&mut self) {
        self.placed_anchors = Vec::new();
        self.placement = None;

        let (Some(path), Some(library)) = (self.current.clone(), self.library.as_ref()) else {
            return;
        };
        let Some(id) = self.index.id_of(&path) else {
            return;
        };
        let held = library.anchors_on_image(id).unwrap_or_default();
        if held.is_empty() {
            return;
        }

        let pairs: Vec<crate::anchor::Pair> = held
            .iter()
            .map(|placed| crate::anchor::Pair {
                id: placed.id,
                name: placed.name.clone(),
                world: crate::map::to_mercator(placed.lat, placed.lon),
                pixel: placed.pixel,
            })
            .collect();
        self.placed_anchors = held;
        self.placement = Some(crate::anchor::solve(&pairs));
    }

    /// What the anchors placed *so far in this session* add up to.
    ///
    /// Worked out afresh rather than kept, because it changes with every anchor
    /// put down and a stale copy would lock the halves to an answer that has
    /// since been improved. Nothing until two are down — one anchor fixes a
    /// spot but neither the turn nor the size.
    pub fn placing_placement(&self) -> Option<crate::anchor::Placement> {
        let session = self.placing.as_ref()?;
        let pairs: Vec<crate::anchor::Pair> = session
            .placed
            .iter()
            .filter_map(|(id, pixel)| {
                let anchor = self.anchors.iter().find(|held| held.id == *id)?;
                Some(crate::anchor::Pair {
                    id: *id,
                    name: anchor.name.clone(),
                    world: crate::map::to_mercator(anchor.lat, anchor.lon),
                    pixel: *pixel,
                })
            })
            .collect();
        crate::anchor::solve(&pairs).ok().map(|fit| fit.placement)
    }

    /// Takes one anchor back off the picture, leaving the rest.
    ///
    /// For the one that was clicked in the wrong spot. Its own large residual
    /// is what points it out, and removing it re-solves from what is left —
    /// which is the whole reason residuals are worth showing.
    ///
    /// The file's written position is not touched. This changes what the
    /// placement *would* be; writing it is still a separate, deliberate act.
    pub fn unplace_anchor(&mut self, anchor_id: i64) {
        let Some(path) = self.current.clone() else {
            return;
        };
        let Some(id) = self.index.id_of(&path) else {
            return;
        };
        if let Some(library) = &self.library {
            let _ = library.unplace_anchor_on_image(id, anchor_id);
        }
        self.reload_placement();
    }

    /// Takes every anchor off the picture, so the mapping can be started again.
    ///
    /// Only the pairings go. The anchors themselves are places on the earth and
    /// stay in the list — except the ones made for this picture alone, which
    /// have nothing left to mean and go with it.
    pub fn clear_mapping(&mut self, now: f64) {
        let Some(path) = self.current.clone() else {
            return;
        };
        let Some(id) = self.index.id_of(&path) else {
            return;
        };
        let mut owned = 0;
        if let Some(library) = &self.library {
            let _ = library.clear_anchors_on_image(id);
            for anchor in &self.anchors {
                if anchor.owner_image_id == Some(id) && library.delete_anchor(anchor.id).is_ok() {
                    owned += 1;
                }
            }
        }
        self.placement = None;
        self.placed_anchors.clear();
        self.reload_anchors();
        self.reload_placement();
        self.toast_at(
            &if owned > 0 {
                format!("Mapping cleared, with {owned} anchors made for this picture")
            } else {
                "Mapping cleared".to_owned()
            },
            now,
        );
    }

    /// Moves the basemap to sit under whatever the picture half is showing.
    ///
    /// Called after the picture has been dragged or zoomed, and only then, so
    /// the two halves never chase each other: each gesture has exactly one
    /// leader and the follower is worked out from the leader's state rather
    /// than nudged by the same amount, which is what keeps them from drifting
    /// apart over a long session.
    pub fn map_follows_picture(&mut self) {
        let Some(placement) = self.placing_placement() else {
            return;
        };
        let Some(session) = self.placing.as_ref() else {
            return;
        };
        if !session.locked || session.zoom <= 0.0 {
            return;
        }

        // The same angle the picture is drawn at, or the two halves disagree
        // about which pixel is in the middle.
        let turn = if session.upright {
            placement.rotation
        } else {
            0.0
        };
        let world = placement.apply(session.centre_pixel(turn));
        let (lat, lon) = crate::map::from_mercator(world.0, world.1);
        let metres = placement.scale / f64::from(session.zoom);
        self.map_view.centre_on(lat, lon);
        self.map_view.zoom = crate::map::clamp_zoom(crate::map::zoom_for_metres_per_point(metres));
    }

    /// The same the other way: the picture follows the basemap.
    ///
    /// **The turn is not matched, because the basemap has none.** A scan that
    /// sits at an angle to north stays at that angle while the map stays north
    /// up, so the two halves cover the same ground at the same size but not in
    /// the same direction. Rotating the picture instead would mean showing the
    /// scan at an angle it was never drawn at, which is worse.
    pub fn picture_follows_map(&mut self) {
        let Some(placement) = self.placing_placement() else {
            return;
        };
        let view = self.map_view;
        let Some(session) = self.placing.as_mut() else {
            return;
        };
        if !session.locked || session.zoom <= 0.0 {
            return;
        }

        let world = crate::map::to_mercator(view.lat, view.lon);
        let pixel = placement.pixel_of(world);
        let metres = crate::map::metres_per_point(view.zoom);
        session.zoom = ((placement.scale / metres) as f32).clamp(0.01, 40.0);
        let turn = if session.upright {
            placement.rotation
        } else {
            0.0
        };
        session.centre_on_pixel(pixel, turn);
    }

    /// Lays the picture on the basemap using the placement worked out by hand.
    ///
    /// Separate from the ordinary overlay, which reads the position out of the
    /// file: this one shows a placement that has not been written down yet,
    /// which is the only way to judge whether it is worth writing down.
    pub fn show_placement_on_map(&mut self, ctx: &egui::Context) {
        let (Some(path), Some(Ok(fit))) = (self.current.clone(), self.placement.clone()) else {
            return;
        };
        if self
            .map_overlay
            .as_ref()
            .is_some_and(|overlay| overlay.path == path && overlay.placement.is_some())
        {
            self.map_overlay = None;
            return;
        }

        // A placement of its own, so `load_overlay` has something to hold; the
        // numbers in it are ignored in favour of the fit.
        let stand_in = crate::geotiff::GeoRef {
            origin: fit.placement.origin,
            scale: (fit.placement.scale, fit.placement.scale),
            epsg: Some(3857),
            citations: vec!["WGS 84 / Pseudo-Mercator".to_owned()],
            width: 0,
            height: 0,
            transformed: fit.placement.rotation.abs() > 1e-9,
        };
        match crate::map::load_overlay(ctx, &path, stand_in) {
            Ok(mut overlay) => {
                overlay.placement = Some(fit.placement);
                // Look at the middle of it, at a size that holds the whole map.
                let centre = fit.placement.apply((
                    f64::from(overlay.size.0) / 2.0,
                    f64::from(overlay.size.1) / 2.0,
                ));
                let (lat, lon) = crate::map::from_mercator(centre.0, centre.1);
                let corner = fit.placement.apply((0.0, 0.0));
                let far = fit
                    .placement
                    .apply((f64::from(overlay.size.0), f64::from(overlay.size.1)));
                let (north, west) =
                    crate::map::from_mercator(corner.0.min(far.0), corner.1.max(far.1));
                let (south, east) =
                    crate::map::from_mercator(corner.0.max(far.0), corner.1.min(far.1));
                let _ = (lat, lon);
                self.map_view
                    .fit((north, west), (south, east), self.map_window);
                self.map_overlay = Some(overlay);
                self.switch_page(crate::pages::PAGE_MAP);
            }
            Err(reason) => {
                let now = ctx.input(|i| i.time);
                self.toast_at(&format!("Could not place the picture — {reason}"), now);
            }
        }
    }

    /// Writes the worked-out position into the picture's own file.
    pub fn save_placement(&mut self, now: f64) {
        let (Some(path), Some(Ok(fit))) = (self.current.clone(), self.placement.clone()) else {
            return;
        };
        let height = self
            .squares
            .shapes()
            .first()
            .map(|shape| shape.height)
            .unwrap_or(0);
        if height == 0 {
            self.toast_at("The picture's size is not known yet", now);
            return;
        }

        match crate::geowrite::write(&path, &fit.placement, height) {
            Ok(()) => {
                self.current_geo = crate::geotiff::from_path(&path);
                self.mark_georeferenced(&path, true);
                self.toast_at("Position written into the file", now);
            }
            Err(error) => self.toast_at(&format!("{error:#}"), now),
        }
    }

    /// Takes the position back out of the file.
    pub fn clear_placement(&mut self, now: f64) {
        let Some(path) = self.current.clone() else {
            return;
        };
        match crate::geowrite::clear(&path) {
            Ok(()) => {
                self.current_geo = None;
                self.mark_georeferenced(&path, false);
                self.toast_at("Position removed from the file", now);
            }
            Err(error) => self.toast_at(&format!("{error:#}"), now),
        }
    }

    /// Keeps the library's record in step with the file, so the green marking
    /// changes at once rather than after the next scan.
    fn mark_georeferenced(&mut self, path: &Path, georeferenced: bool) {
        let Some(id) = self.index.id_of(path) else {
            return;
        };
        if let Some(library) = &self.library {
            let _ = library.set_georeferenced(id, georeferenced);
        }
        if georeferenced {
            self.georeferenced.insert(id);
        } else {
            self.georeferenced.remove(&id);
        }
        // And the tree's own copy, which is what the FILES row is coloured
        // from. The database and the set above are not read by it.
        self.files.set_georeferenced(path, georeferenced);
    }

    /// Adds pictures to the map conversion queue.
    pub fn queue_map_images(&mut self, ids: &[i64], now: f64) {
        if !self.config.map_support {
            return;
        }
        let Some(library) = &self.library else { return };
        match library.queue_for_conversion(ids) {
            Ok(0) => self.toast_at("Already queued", now),
            Ok(added) => {
                self.reload_map_queue();
                self.toast_at(&format!("{added} added to map images"), now);
                self.maybe_auto_convert(now);
            }
            Err(error) => self.toast_at(&format!("{error:#}"), now),
        }
    }

    /// Starts a conversion by itself, when that has been asked for.
    ///
    /// Only when one is not already running: the running job takes its list of
    /// work when it starts, but the queue it takes from is re-read each time, so
    /// anything added meanwhile is picked up by the *next* run rather than
    /// needing a second thread now.
    ///
    /// Silent about a missing output folder. The setting cannot be turned on
    /// without one, but it can be turned on and the folder forgotten later, and
    /// a complaint on every add would be worse than doing nothing.
    fn maybe_auto_convert(&mut self, now: f64) {
        if !self.config.auto_convert_maps
            || self.converting.is_some()
            || self.config.map_export_root.is_empty()
        {
            return;
        }
        self.start_map_conversion(now);
    }

    pub fn unqueue_map_images(&mut self, ids: &[i64]) {
        if let Some(library) = &self.library {
            let _ = library.unqueue_for_conversion(ids);
        }
        self.reload_map_queue();
    }

    pub fn clear_map_queue(&mut self) {
        if let Some(library) = &self.library {
            let _ = library.clear_conversion_queue();
        }
        self.reload_map_queue();
    }

    /// Re-reads the queue from the library. Held in memory because the page and
    /// the grid both ask about it every frame.
    pub fn reload_map_queue(&mut self) {
        let Some(library) = &self.library else { return };
        self.map_queue = library.conversion_queue().unwrap_or_default();
        self.map_queued_ids = library.queued_ids().unwrap_or_default();
    }

    /// Says something in the corner for a few seconds.
    pub fn toast_at(&mut self, message: &str, now: f64) {
        self.toast = Some((message.to_owned(), now));
    }

    /// Decides whether to ask about installing, once at startup.
    ///
    /// Read from the library rather than kept in the config string: it is a
    /// record of an answer given, not a setting anyone would go looking for in
    /// the Config page.
    fn raise_install_prompt(&mut self) {
        let declined = self
            .library
            .as_ref()
            .and_then(|library| library.setting(SETTING_INSTALL_DECLINED).ok().flatten())
            .and_then(|value| value.trim().parse::<u64>().ok());
        self.install_prompt = crate::install::prompt_at_startup(declined);
    }

    /// "Not now" — remembers which build was turned down and takes the dialog
    /// away. The next build asks again; this one will not.
    pub fn decline_install(&mut self) {
        self.install_prompt = None;
        let build = crate::version::build_unix();
        if build == 0 {
            return;
        }
        if let Some(library) = &self.library {
            let _ = library.set_setting(SETTING_INSTALL_DECLINED, &build.to_string());
        }
    }

    /// "Yes" — installs, says how it went, and takes the dialog away.
    pub fn accept_install(&mut self, now: f64) {
        self.install_prompt = None;
        match crate::install::install() {
            Ok(report) => {
                let what = if report.refreshed {
                    "Icons and menu entry rewritten"
                } else {
                    "Installed — geo-ref_mapper is in your applications menu"
                };
                self.toast_at(what, now);
            }
            Err(error) => self.toast_at(&format!("Could not install — {error}"), now),
        }
    }

    /// Puts the rest of the program in step with the IMAGE MAPS switch.
    ///
    /// The switch decides what is *drawn*, and everything above reads it before
    /// drawing. What it cannot do on its own is the work already in flight
    /// behind those pages — a conversion running, a picture half placed, a map
    /// laid over the basemap. Left alone, turning the switch off would hide the
    /// page and leave the conversion grinding through 7,000 maps with no way
    /// left to stop it.
    pub fn map_support_changed(&mut self, ctx: &egui::Context) {
        if self.config.map_support {
            // Back on: the sweep is skipped while it is off, so a library
            // indexed in the meantime has never been asked the question.
            self.fill_in_georeferences(ctx);
            return;
        }

        // Stop rather than kill: it finishes the map it is on and leaves the
        // rest queued, so nothing is half-written and the queue is intact if
        // the switch goes back on.
        self.stop_map_conversion();
        self.cancel_placing();
        self.map_overlay = None;
        self.maps_in_view.clear();
        if crate::pages::is_map_page(self.page) {
            self.page = crate::pages::PAGE_VIEWER;
        }
        if self.previous_page.is_some_and(crate::pages::is_map_page) {
            self.previous_page = None;
        }
    }

    /// Whether `page` can be opened at all. Only the map pages ever say no.
    pub fn page_available(&self, page: usize) -> bool {
        self.config.map_support || !crate::pages::is_map_page(page)
    }

    pub fn go_to_page(&mut self, page: usize) {
        if page == self.page || !self.page_available(page) {
            return;
        }
        self.previous_page = Some(self.page);
        self.enter_page(page);
    }

    /// Switches page without leaving a way back.
    ///
    /// For the page pill, which is deliberate navigation rather than drilling
    /// into something. Escape backs out of things you *entered* — a dialog, a
    /// stack, full screen, a picture opened from the grid — and a page you
    /// chose from the title bar is not one of those. Recording it meant
    /// pressing Escape in the Mosaic threw you into the Viewer.
    pub fn switch_page(&mut self, page: usize) {
        if page == self.page || !self.page_available(page) {
            return;
        }
        self.previous_page = None;
        self.enter_page(page);
    }

    /// Carries the scale across between the Viewer and the map.
    ///
    /// Both are drawing the same picture, so the toggle should change what is
    /// *around* it, not its size. Without this the map re-fitted the picture to
    /// the window every time and the Viewer re-fitted it back, so flipping
    /// between them was two different pictures rather than two views of one.
    ///
    /// Only applies to the picture actually on the map, and only when the
    /// Viewer is showing that same file.
    /// Whether the Viewer and the map can hand a scale to one another.
    ///
    /// Both directions need the picture actually on screen in the Viewer — its
    /// size, and the canvas it was drawn on. Selecting one in the tree and
    /// putting it straight on the map gives neither: the decode has not
    /// finished, so there is no size to match and nothing to line up with.
    ///
    /// Asked by `toggle_on_map` as well as by the hand-off itself, so the two
    /// cannot disagree about whether it happened.
    /// Whether putting a picture on the map is about to be handed a view.
    ///
    /// Only one thing sets the view for you: coming to the map **from the
    /// Viewer**, which carries the size and position the picture already had.
    /// Any other way in has nothing to hand over, so the map has to be fitted
    /// to the window or it lands on a basemap looking somewhere else entirely.
    ///
    /// Both halves are needed. Being on the map page already is not a
    /// hand-off — there is no transition, so `carry_scale_between_views` does
    /// nothing and the view stays wherever it was. And leaving the Viewer is
    /// only a hand-off if the picture was actually on screen there to be
    /// measured.
    fn handing_over_from_the_viewer(&self) -> bool {
        // Not for a turned map. The hand-off matches scale against the
        // raster's upright extent, which is not where a turned one is drawn, so
        // it aims at a rectangle the picture does not fill.
        let upright = self
            .map_overlay
            .as_ref()
            .is_none_or(|overlay| overlay.placement.is_none_or(|p| p.rotation.abs() < 1e-9));
        upright && self.page == crate::pages::PAGE_VIEWER && self.can_carry_scale()
    }

    fn can_carry_scale(&self) -> bool {
        let Some(overlay) = &self.map_overlay else {
            return false;
        };
        self.current.as_ref() == Some(&overlay.path)
            && self.canvas_rect.is_some()
            && self.current_image_size().is_some()
    }

    fn carry_scale_between_views(&mut self, leaving: usize, arriving: usize) {
        use crate::pages::{PAGE_MAP, PAGE_VIEWER};

        if !self.can_carry_scale() {
            return;
        }
        let Some(overlay) = &self.map_overlay else {
            return;
        };
        let geo = overlay.geo.clone();
        let (Some(canvas), Some(image)) = (self.canvas_rect, self.current_image_size()) else {
            return;
        };

        match (leaving, arriving) {
            (PAGE_VIEWER, PAGE_MAP) => {
                let zoom = self.viewport.zoom(canvas.size(), image);
                self.map_view.match_scale(&geo, f64::from(zoom));
                // The picture pixel under the middle of the canvas, as a
                // fraction of the raster, becomes the middle of the map.
                let across = 0.5 - f64::from(self.viewport.offset.x / (image.x * zoom));
                let down = 0.5 - f64::from(self.viewport.offset.y / (image.y * zoom));
                self.map_view.look_at_fraction(&geo, across, down);
            }
            (PAGE_MAP, PAGE_VIEWER) => {
                let Some(per_pixel) = self.map_view.scale_against(&geo) else {
                    return;
                };
                self.viewport.set_zoom(per_pixel as f32);
                if let Some((across, down)) = self.map_view.fraction_within(&geo) {
                    let zoom = self.viewport.zoom(canvas.size(), image);
                    self.viewport.offset = egui::vec2(
                        (0.5 - across as f32) * image.x * zoom,
                        (0.5 - down as f32) * image.y * zoom,
                    );
                    self.viewport.clamp_offset(canvas, image);
                }
            }
            _ => {}
        }
    }

    /// The part both share: what a page needs set up on arrival.
    fn enter_page(&mut self, page: usize) {
        // The one gate every way onto a map page goes through — the pill, the
        // Space key, placing, a pin in the panel, a saved position being
        // opened. Refusing here rather than at each caller is what makes the
        // switch a switch rather than a list of places to remember.
        if !self.config.map_support && crate::pages::is_map_page(page) {
            return;
        }
        self.carry_scale_between_views(self.page, page);

        // Opening the Mosaic shows the folder you were just in, rather than
        // whichever folder the grid happened to be left on last time.
        if page == crate::pages::PAGE_MOSAIC {
            self.mosaic_root = Some(self.files.selected_folder());
            self.focus = Focus::Grid;
        }
        // Arriving at the Viewer aims the keyboard at the picture, so the
        // arrows step through the rail without having to click it first.
        if page == crate::pages::PAGE_VIEWER {
            self.focus = Focus::Images;
        }
        self.page = page;
    }

    /// Escape: back to the page the last jump came from, once.
    ///
    /// The return point is cleared on use, so pressing Escape again does
    /// nothing rather than walking further back — Mosaic → picture → Escape
    /// lands on the Mosaic and stays there.
    pub fn go_back(&mut self) {
        // The return point can outlive the switch being thrown, so Escape
        // checks the page is still one that exists before landing on it.
        if let Some(page) = self.previous_page.take()
            && self.page_available(page)
        {
            self.page = page;
        }
    }

    pub fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        let is_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!is_fullscreen));
    }

    /// Keeps the interface scale and egui's zoom factor saying the same thing.
    ///
    /// There are two ways to change it and they have to agree. The slider sets
    /// `config.ui_scale` and this pushes it into egui. Ctrl+plus, Ctrl+minus
    /// and Ctrl+0 are egui's own and change the zoom factor behind our back —
    /// pushing the config unconditionally would undo them on the next frame,
    /// so a zoom factor that moved on its own is adopted as the setting
    /// instead, and saved with the rest of the config.
    ///
    /// `applied_ui_scale` is what tells the two apart: it is what we last asked
    /// for, so a live value that differs from it came from somewhere else.
    /// `set_zoom_factor` lands at the start of the next pass rather than
    /// immediately, which is why the comparison is against our own record and
    /// not against the config.
    fn apply_ui_scale(&mut self, ctx: &egui::Context) {
        // A held slider becomes the setting when the pointer comes up.
        //
        // Asked here rather than of the slider's own response, because the
        // release does not always reach it: the pointer can come up outside the
        // track, off the window entirely, or on a frame where the Config page
        // is no longer being drawn. Any of those would leave the scale stuck at
        // whatever it was before the drag, with the slider showing the value it
        // was let go at. The pointer being up is the whole condition.
        let pointer_down = ctx.input(|i| i.pointer.any_down());
        if let Some(released) = release_ui_scale(self.pending_ui_scale, pointer_down) {
            self.config.ui_scale = released;
            self.pending_ui_scale = None;
        }

        let live = ctx.zoom_factor();
        self.config.ui_scale = resolve_ui_scale(live, self.applied_ui_scale, self.config.ui_scale);
        if (self.config.ui_scale - live).abs() > f32::EPSILON {
            ctx.set_zoom_factor(self.config.ui_scale);
        }
        self.applied_ui_scale = self.config.ui_scale;
    }

    /// Puts the window full screen for an app opened straight onto a picture.
    ///
    /// Done on a frame rather than at startup because the window has to exist
    /// before it can be told to fill the screen.
    ///
    /// Presentation mode gives the canvas the whole window — no title bar, no
    /// panels, no strip — so full screen means the picture and nothing else,
    /// fitted to the display, which is what a viewer opened from a file
    /// manager is for.
    fn apply_launch_window(&mut self, ctx: &egui::Context) {
        if !self.launch_window_pending {
            return;
        }
        self.launch_window_pending = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
    }

    /// Asks for the keyboard until the window has it, for a picture-only
    /// launch. Gives up after a couple of seconds either way: a window that
    /// still has not been focused by then was left in the background
    /// deliberately, and going on asking would be a window fighting the person
    /// using the computer for their attention.
    fn claim_keyboard(&mut self, ctx: &egui::Context) {
        const PATIENCE: f64 = 2.0;

        if !self.claim_focus {
            return;
        }
        let now = ctx.input(|i| i.time);
        let deadline = *self.claim_focus_until.get_or_insert(now + PATIENCE);
        // `focused` is unknown on some backends; only a definite yes counts as
        // the window having the keyboard.
        if ctx.input(|i| i.viewport().focused) == Some(true) || now >= deadline {
            self.claim_focus = false;
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        // Nothing else is asking for frames on an idle picture, and a request
        // that is never followed by another frame cannot notice it worked.
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    /// Shows the picture on its own, full screen, and puts it back again.
    ///
    /// The zoom is pinned first. Both the window size and the space the panels
    /// take change across this, so a fitted picture would re-fit to the new
    /// canvas and visibly jump — exactly when the change should pass unnoticed.
    pub fn toggle_presentation(&mut self, ctx: &egui::Context) {
        if let (Some(canvas), Some(size)) = (self.canvas_rect, self.current_image_size()) {
            self.viewport.freeze(canvas.size(), size);
            if let Some(window) = self.outer_rect(ctx) {
                self.pending_anchor = Some((window, canvas, self.viewport.offset));
            }
        }
        self.presenting = !self.presenting;
        // Matched, not toggled: an app opened straight onto a picture is
        // presenting in an ordinary window, and Escape from there must show
        // the panels, not throw the window full screen.
        let is_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        if self.presenting != is_fullscreen {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.presenting));
        }
    }

    fn outer_rect(&self, ctx: &egui::Context) -> Option<egui::Rect> {
        ctx.input(|i| i.viewport().outer_rect)
    }

    /// Once the new geometry has arrived, put the picture back on the pixels it
    /// was on and start it gliding to where it now belongs.
    pub fn settle_anchor(&mut self, ctx: &egui::Context, canvas: egui::Rect) {
        let Some((window_before, canvas_before, offset_before)) = self.pending_anchor else {
            return;
        };
        let Some(window_after) = self.outer_rect(ctx) else {
            return;
        };
        // Nothing has changed yet — the window manager has not acted.
        if window_after == window_before && canvas == canvas_before {
            return;
        }
        self.pending_anchor = None;

        let held = crate::viewport::Viewport::anchored_offset(
            window_before,
            canvas_before,
            offset_before,
            window_after,
            canvas,
        );
        // Where it will end up once the usual rules apply again.
        let mut settled = self.viewport;
        settled.offset = held;
        if let Some(size) = self.current_image_size() {
            settled.clamp_offset(canvas, size);
        }
        self.offset_glide = Some((held, settled.offset, ctx.input(|i| i.time)));
    }

    /// Advances the glide, if one is running. Returns the offset to draw with.
    pub fn glide_offset(&mut self, ctx: &egui::Context) -> Option<egui::Vec2> {
        const SECONDS: f64 = 0.22;
        let (from, to, started) = self.offset_glide?;
        let elapsed = ctx.input(|i| i.time) - started;
        if elapsed >= SECONDS {
            self.offset_glide = None;
            return Some(to);
        }
        ctx.request_repaint();
        // Ease out, so it leaves quickly and arrives gently.
        let t = (elapsed / SECONDS) as f32;
        let eased = 1.0 - (1.0 - t).powi(3);
        Some(from + (to - from) * eased)
    }

    /// Pixel dimensions of the picture on screen, if one is decoded.
    fn current_image_size(&self) -> Option<egui::Vec2> {
        let entry = self.textures.peek(self.current.as_deref()?)?;
        Some(egui::vec2(entry.size[0] as f32, entry.size[1] as f32))
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        use egui::Key;

        let (dropped, keys, open, fullscreen) = ctx.input(|i| {
            let dropped: Vec<PathBuf> = i
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect();
            let keys = [
                // Down and up step through the pictures; left and right work
                // the folder the tree is sitting in, the way a file manager's
                // tree does.
                //
                // Space is deliberately not bound. It did the same as Down,
                // which spent the one key every viewer reserves for something
                // of its own on a duplicate.
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::F) || i.key_pressed(Key::Num0),
                i.key_pressed(Key::Num1),
                i.key_pressed(Key::Num2),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowLeft),
            ];
            (
                dropped,
                keys,
                i.modifiers.command && i.key_pressed(Key::O),
                i.key_pressed(Key::F11),
            )
        });
        // Not while something is being typed into — the tile-server contact
        // field would otherwise lose every space it is given.
        let typing = ctx.egui_wants_keyboard_input();
        let place = !typing && ctx.input(|i| i.key_pressed(Key::Space));
        let enter = !typing && ctx.input(|i| i.key_pressed(Key::Enter));
        // D belongs to the picture, not to the basemap.
        //
        // It draws the tiling over whatever is on screen — the squares of a
        // pyramidal TIFF in the Viewer as much as a map on the tiles — and the
        // Viewer is where it earns its keep, on a gigapixel scan that is
        // loading the wrong level. Gating it on map support took the overlay
        // away from a page that has nothing to do with maps.
        if !typing && ctx.input(|i| i.key_pressed(Key::D)) {
            self.tile_debug = !self.tile_debug;
        }
        // G and L really are basemap-only: a graticule is drawn on the map page
        // alone, and locking anchors is meaningless without them. With map
        // support off they would be plain letters quietly changing a setting
        // for a page that is not there.
        let map_keys = !typing && self.config.map_support;
        if map_keys && ctx.input(|i| i.key_pressed(Key::G)) {
            self.graticule = !self.graticule;
        }
        // Not while they are hidden: the panel's switch is disabled then, and a
        // key that quietly does what the switch refuses is the same setting
        // with two different answers.
        if map_keys && ctx.input(|i| i.key_pressed(Key::L)) && !self.anchors_off_the_map() {
            self.config.anchors_locked = !self.config.anchors_locked;
            let now = ctx.input(|i| i.time);
            self.toast_at(
                if self.config.anchors_locked {
                    "Anchors locked"
                } else {
                    "Anchors unlocked — dragging one moves every map placed by it"
                },
                now,
            );
        }
        let back = ctx.input(|i| i.key_pressed(Key::Escape));
        let delete = ctx.input(|i| i.key_pressed(Key::Delete));

        if let Some(path) = dropped.into_iter().next() {
            self.open_path(path, ctx);
        }
        if open
            && let Some(path) = rfd::FileDialog::new()
                .set_title("Open an image")
                .add_filter("Images", crate::db::IMAGE_EXTENSIONS)
                .pick_file()
        {
            self.open_path(path, ctx);
        }
        if fullscreen {
            self.toggle_fullscreen(ctx);
        }
        if back {
            // Escape unwinds the nearest thing first: a dialog, then the
            // picture-only view, then the page you came from.
            if !std::mem::take(&mut self.confirm_delete).is_empty() {
                // Handled.
            } else if self.placing.is_some() {
                // Placing is the most modal thing on screen, so Escape leaves
                // it before anything else. Nothing was written, so nothing is
                // lost.
                self.cancel_placing();
            } else if self.open_stack.take().is_some() {
                // Closing an open stack is the nearest thing Escape can undo
                // when you are inside one.
            } else if self.launched_on_picture {
                // `geo-ref_mapper photo.jpg` opened a window that is nothing but the
                // picture. There is no interface behind it to unwind, so
                // Escape means what it does in any viewer launched from a file
                // manager: shut the window.
                crate::chrome::CLOSE_REQUESTED_BY_BUTTON
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else if self.presenting {
                self.toggle_presentation(ctx);
            } else {
                self.go_back();
            }
        }
        // Enter is the way out of a picture-only launch and into the ordinary
        // panelled app, with the picture that was opened still on screen. Only
        // from that launch: elsewhere Enter belongs to whatever is focused.
        if enter && self.launched_on_picture {
            self.launched_on_picture = false;
            if self.presenting {
                self.toggle_presentation(ctx);
            }
        }
        if delete && self.confirm_delete.is_empty() {
            self.confirm_delete = self.deletable();
        }
        if place {
            // Space means "this one" — which is a different thing in the
            // Viewer, where you are looking at one picture and building up a
            // set of them, than on the Map, where the question is whether this
            // picture is laid over the tiles.
            //
            // With map support off there is no second meaning left, so Space
            // marks the picture from wherever you are rather than doing
            // nothing outside the Viewer.
            if self.page == crate::pages::PAGE_VIEWER || !self.config.map_support {
                let now = ctx.input(|i| i.time);
                self.toggle_mark_on_current(now);
            } else {
                self.toggle_on_map(ctx);
            }
        }

        let [next, previous, fit, actual, fill, right, left] = keys;

        // The arrows work wherever there is something to walk: the Viewer's
        // rail, the Mosaic's grid, and the FILES tree beside both. Returning on
        // any page but the Viewer — which is what this used to do — left the
        // tree dead the moment you were looking at the Mosaic, and made the
        // grid arm below unreachable.
        if !matches!(
            self.page,
            crate::pages::PAGE_VIEWER | crate::pages::PAGE_MOSAIC
        ) {
            return;
        }

        // The zoom presets are about the picture, so they belong to the page
        // that has one.
        if self.page != crate::pages::PAGE_VIEWER {
        } else if fit {
            self.viewport.set_mode(crate::viewport::ZoomMode::Fit);
        }
        if actual {
            self.viewport.set_mode(crate::viewport::ZoomMode::Actual);
        }
        if fill {
            self.viewport.set_mode(crate::viewport::ZoomMode::Fill);
        }

        // The arrows belong to whichever panel was last clicked in, and each
        // one owns all four. Sending left and right to the tree regardless —
        // which is what this used to do — meant they stopped meaning "the next
        // picture" the moment you were looking at a picture.
        // Presentation mode draws the picture and nothing else, so all four
        // arrows walk the rail whatever was last clicked in. Left to the tree,
        // they would step rows that are not on screen — which is exactly what
        // an `geo-ref_mapper photo.jpg` start did, since the tree holds the focus
        // from the moment the app opens.
        let focus = if self.presenting {
            Focus::Images
        } else {
            self.focus
        };
        // Nothing to step through yet.
        //
        // A file opened by name selects itself only once its folder has been
        // indexed, and until then the tree still holds the last session's
        // selection. The arrows would walk *that* folder — a different set of
        // pictures entirely, in a window opened to look at one particular
        // one — and the scan would then snap the selection back to the opened
        // picture and undo it. Better to do nothing for the moment it takes.
        if self.pending_selection.is_some() {
            return;
        }
        match focus {
            Focus::Tree => {
                if next {
                    self.step_tree_row(1);
                }
                if previous {
                    self.step_tree_row(-1);
                }
                if right {
                    self.set_folder_expanded(true);
                }
                if left {
                    self.set_folder_expanded(false);
                }
            }
            // A grid has two dimensions: down and up by a row, left and right
            // by one tile.
            Focus::Grid if self.page == crate::pages::PAGE_MOSAIC => {
                let columns = self.mosaic_columns.max(1) as isize;
                let step = match (next, previous, right, left) {
                    (true, _, _, _) => columns,
                    (_, true, _, _) => -columns,
                    (_, _, true, _) => 1,
                    (_, _, _, true) => -1,
                    _ => 0,
                };
                if step != 0 {
                    self.step_grid(step);
                }
            }
            // The rail is one long line, so all four arrows walk it.
            Focus::Images | Focus::Grid => {
                if next || right {
                    self.step_image(1);
                }
                if previous || left {
                    self.step_image(-1);
                }
            }
        }

        if next || previous || left || right {
            self.follow_selection_on_map(ctx);
        }
    }

    /// Moves the map overlay to whatever is now selected.
    ///
    /// Put one map on the basemap with Space and the arrows then walk through
    /// the others, each landing in its own place — which is the whole point of
    /// stepping while the map is up.
    ///
    /// The view is deliberately left where it is. Re-fitting to each map would
    /// throw away the position you had just chosen, and comparing two maps of
    /// the same ground is the thing this is for.
    fn follow_selection_on_map(&mut self, ctx: &egui::Context) {
        let Some(overlay) = &self.map_overlay else {
            return;
        };
        // Read from the tree rather than `current`: the arrows are handled
        // before the selection is copied across, so `current` is still the
        // picture we were on a moment ago and the map would lag a keypress.
        let Some(path) = self
            .files
            .selected_node()
            .and_then(|node| node.path())
            .map(std::path::PathBuf::from)
        else {
            return;
        };
        if overlay.path == path {
            return;
        }
        let opacity = overlay.opacity;

        // A picture with no position cannot go on the map. The one already
        // there stays rather than vanishing, so stepping past a photograph does
        // not clear the screen.
        let Some(geo) = crate::geotiff::from_path(&path) else {
            return;
        };
        if geo.top_left().is_none() || geo.bottom_right().is_none() {
            return;
        }

        match crate::map::load_overlay(ctx, &path, geo) {
            Ok(mut placed) => {
                placed.opacity = opacity;
                self.map_overlay = Some(placed);
            }
            Err(reason) => {
                let now = ctx.input(|i| i.time);
                self.toast_at(&format!("Could not place the picture — {reason}"), now);
            }
        }
    }
}

fn panel_frame() -> Frame {
    Frame::new()
        .fill(theme::PANEL_BG)
        .inner_margin(Margin::symmetric(10, 6))
}
