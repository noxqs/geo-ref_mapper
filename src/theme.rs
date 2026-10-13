//! Central place for the colours and metrics the custom chrome is painted with.

use eframe::egui::{self, Color32, CornerRadius, FontId, Stroke};

pub const WINDOW_BG: Color32 = Color32::from_rgb(0x10, 0x11, 0x14);
pub const TITLE_BAR: Color32 = Color32::from_rgb(0x1b, 0x1d, 0x21);
pub const HAIRLINE: Color32 = Color32::from_rgb(0x2a, 0x2d, 0x33);
pub const PANEL_BG: Color32 = Color32::from_rgb(0x17, 0x19, 0x1d);
pub const CANVAS_BG: Color32 = Color32::from_rgb(0x0c, 0x0d, 0x0f);

pub const SURFACE: Color32 = Color32::from_rgb(0x21, 0x24, 0x29);
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(0x2b, 0x2f, 0x36);

pub const TEXT: Color32 = Color32::from_rgb(0xe2, 0xe4, 0xe8);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x87, 0x8c, 0x96);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x5c, 0x62, 0x6c);

pub const ACCENT: Color32 = Color32::from_rgb(0x5b, 0x8c, 0xff);
/// Degraded but working — distinct from [`CLOSE_HOVER`], which means trouble,
/// and from AMD's brand red, which would otherwise be indistinguishable.
pub const WARNING: Color32 = Color32::from_rgb(0xE8, 0xA3, 0x3D);
/// Rows and tiles picked out for a batch, as opposed to the one being viewed.
///
/// A different hue rather than a dimmer [`ACCENT`]: "this is the picture on
/// screen" and "these are the twelve I am about to export" are different
/// answers, and two shades of the same blue made them one glance apart.
pub const MARKED: Color32 = Color32::from_rgb(0xa9, 0x7b, 0xff);
/// Pictures that belong to a stack.
///
/// Yellow rather than red: red is [`CLOSE_HOVER`] here and means something has
/// gone wrong, and being in a stack is an ordinary state. Distinct from
/// [`WARNING`]'s amber, which is the GPU badge's "working, but slowly".
pub const STACKED: Color32 = Color32::from_rgb(0xE8, 0xD3, 0x4D);
/// The "somewhere else" green, against ACCENT's "here". Used by the filmstrip
/// to say which pictures belong to the folder you are in and which have been
/// pulled in from the ones on either side.
pub const OTHER_FOLDER: Color32 = Color32::from_rgb(0x5d, 0xc2, 0x8a);

/// A picture that says where on the earth it is.
///
/// Green rather than one of the marking colours: this is a property of the file
/// itself, not something the user did to it, so it must not be confused with a
/// selection or a stack.
pub const GEOREFERENCED: Color32 = Color32::from_rgb(0x54, 0xd1, 0x7a);

/// The one picture currently laid over the map.
///
/// Deep blue rather than the green that says "this could go on the map" — one
/// is a property of the file, the other is a thing that is true right now, and
/// they appear side by side.
/// Colours for anchors, so the same place reads the same on the basemap and on
/// the picture.
///
/// Chosen to stay apart from each other on a printed map, which is mostly
/// beige, black line-work and pale blue water — and from the reds and greens
/// the accuracy table uses, which mean something else entirely.
pub const ANCHOR_COLOURS: &[Color32] = &[
    Color32::from_rgb(0xff, 0x5c, 0xe0), // magenta
    Color32::from_rgb(0x2e, 0xd9, 0xd9), // cyan
    Color32::from_rgb(0xff, 0xc4, 0x1f), // amber
    Color32::from_rgb(0x8b, 0x6b, 0xff), // violet
    Color32::from_rgb(0x4a, 0xff, 0x7a), // mint
    Color32::from_rgb(0xff, 0x8a, 0x3d), // orange
    Color32::from_rgb(0x5b, 0xa8, 0xff), // blue
    Color32::from_rgb(0xff, 0x6b, 0x6b), // coral
];

/// The colour for an anchor, by its place in the list. Wraps round, so a ninth
/// anchor shares with the first — which is better than running out.
pub fn anchor_colour(index: usize) -> Color32 {
    ANCHOR_COLOURS[index % ANCHOR_COLOURS.len()]
}

/// How an accuracy should read: red for a bad match through to green for a
/// good one.
pub fn accuracy_colour(quality: f32) -> Color32 {
    let quality = quality.clamp(0.0, 1.0);
    Color32::from_rgb(
        (0xE0 as f32 * (1.0 - quality) + 0x54 as f32 * quality) as u8,
        (0x44 as f32 * (1.0 - quality) + 0xD1 as f32 * quality) as u8,
        (0x46 as f32 * (1.0 - quality) + 0x7A as f32 * quality) as u8,
    )
}

pub const ON_THE_MAP: Color32 = Color32::from_rgb(0x1e, 0x4d, 0xa8);

/// The filled part of a row in the IMAGES list, showing how opaque that map is
/// drawn on the basemap. Dark blue and solid: it is a level being set, not work
/// in progress, and it has to read as a level at a glance while still leaving
/// the name on top of it legible.
pub const OPACITY_FILL: Color32 = Color32::from_rgb(0x16, 0x33, 0x66);
pub const CLOSE_HOVER: Color32 = Color32::from_rgb(0xe0, 0x44, 0x46);

pub const PILL_TRACK: Color32 = Color32::from_rgb(0x0e, 0x10, 0x13);
pub const PILL_THUMB: Color32 = Color32::from_rgb(0x2e, 0x33, 0x3b);

/// Height of the custom title bar, in points.
pub const TITLE_BAR_HEIGHT: f32 = 40.0;
/// Width of the invisible border used to grab window edges for resizing.
pub const RESIZE_BORDER: f32 = 6.0;

pub fn label_font() -> FontId {
    FontId::proportional(13.0)
}

pub fn small_caps_font() -> FontId {
    FontId::proportional(11.0)
}

/// Panel titles — FILES, INFO. Larger than the small-caps used inside a panel
/// so the heading reads as a heading rather than as another dim row.
pub fn panel_title_font() -> FontId {
    FontId::proportional(14.0)
}

/// Config-page group titles — GPU MEMORY, DECODING, INSTALL.
///
/// Larger than the small caps a group inside a *panel* uses. These are the
/// whole structure of a full-width page rather than a subdivision of a 268 pt
/// column: they are what the page is scanned by, and at 11 pt dim they read as
/// another muted row rather than as the thing dividing the page up.
pub fn group_title_font() -> FontId {
    FontId::proportional(14.0)
}

pub fn heading_font() -> FontId {
    FontId::proportional(15.0)
}

pub fn card_corner() -> CornerRadius {
    CornerRadius::same(8)
}

pub fn hairline() -> Stroke {
    Stroke::new(1.0, HAIRLINE)
}

pub fn install(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        let visuals = &mut style.visuals;
        visuals.dark_mode = true;
        visuals.panel_fill = PANEL_BG;
        visuals.window_fill = WINDOW_BG;
        visuals.extreme_bg_color = CANVAS_BG;
        visuals.override_text_color = Some(TEXT);
        visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        visuals.selection.stroke = Stroke::new(1.0, ACCENT);
        visuals.widgets.noninteractive.bg_stroke = hairline();

        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    });
}
