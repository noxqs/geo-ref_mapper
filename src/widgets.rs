//! Small shared building blocks used across the panels and pages.

use eframe::egui::{
    Align2, Color32, CornerRadius, Frame, Margin, Rect, Response, Sense, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

use crate::{icons, theme};

/// Uppercase panel title with a hairline under it.
pub fn panel_header(ui: &mut Ui, title: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::hover());
    ui.painter().text(
        pos2(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        title,
        theme::panel_title_font(),
        theme::TEXT,
    );
    ui.painter()
        .line_segment([rect.left_bottom(), rect.right_bottom()], theme::hairline());
    ui.add_space(6.0);
}

/// A titled group that can be folded away. Open/closed state lives in egui's
/// memory keyed by the title, so callers need not thread it through.
/// Returns the closure's value when open.
pub fn collapsing_section<R>(
    ui: &mut Ui,
    title: &str,
    default_open: bool,
    contents: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    collapsing_header(ui, section_id(title), title, default_open, Heading::Section)
        .then(|| contents(ui))
}

/// How prominent a collapsing header is.
///
/// The two are not the same job. A [`Heading::Section`] subdivides a 268 pt
/// panel that already has a title above it, so it stays quiet. A
/// [`Heading::Group`] *is* the structure of a full-width page with no other
/// heading on it — it is what the Config page is scanned by, and drawn at the
/// panel's weight it read as one more muted row among the settings.
/// The measurements one heading weight is laid out with.
struct Metrics {
    height: f32,
    chevron: f32,
    chevron_centre: f32,
    title_left: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Heading {
    /// Inside a panel: small, dim.
    Section,
    /// A page's own groups: larger, and lighter against the background.
    Group,
}

impl Heading {
    fn font(self) -> eframe::egui::FontId {
        match self {
            Self::Section => theme::small_caps_font(),
            Self::Group => theme::group_title_font(),
        }
    }

    /// Resting and hovered ink. Both move together so hovering stays a visible
    /// step at either weight.
    fn colours(self) -> (Color32, Color32) {
        match self {
            Self::Section => (theme::TEXT_DIM, theme::TEXT_MUTED),
            Self::Group => (theme::TEXT_MUTED, theme::TEXT),
        }
    }

    /// Row height, chevron size, chevron centre from the left, and where the
    /// title starts.
    ///
    /// Scaled with the type rather than fixed: a 14 pt title in the 20 pt row
    /// the small caps used sits with its descenders against the row below, and
    /// a 12 pt chevron beside it looks like a mis-set bullet.
    ///
    /// `Section`'s numbers are the ones it has always had, spelled out rather
    /// than derived, so adding the group weight moves nothing in the eleven
    /// panel sections that were here first.
    fn metrics(self) -> Metrics {
        match self {
            Self::Section => Metrics {
                height: 20.0,
                chevron: 12.0,
                chevron_centre: 7.0,
                title_left: 18.0,
            },
            Self::Group => Metrics {
                height: 26.0,
                chevron: 14.0,
                chevron_centre: 9.0,
                title_left: 22.0,
            },
        }
    }
}

/// The clickable title row shared by [`collapsing_section`] and
/// [`collapsing_card`]. Split out so the two can key their state on different
/// ids — the DEBUG panel and the Config page both have a group called
/// THUMBNAILS, and they must fold independently — and so the two can be drawn
/// at different weights while behaving identically.
fn collapsing_header(
    ui: &mut Ui,
    id: eframe::egui::Id,
    title: &str,
    default_open: bool,
    heading: Heading,
) -> bool {
    let mut open = ui
        .ctx()
        .data_mut(|data| *data.get_temp_mut_or(id, default_open));

    let metrics = heading.metrics();

    ui.add_space(6.0);
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), metrics.height), Sense::click());
    if response.clicked() {
        open = !open;
        ui.ctx().data_mut(|data| data.insert_temp(id, open));
    }

    if response.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), theme::SURFACE_HOVER);
        ui.ctx()
            .set_cursor_icon(eframe::egui::CursorIcon::PointingHand);
    }

    let (resting, hovered) = heading.colours();
    let ink = if response.hovered() { hovered } else { resting };

    let openness = ui.ctx().animate_bool(id.with("chevron"), open);
    let chevron = Rect::from_center_size(
        pos2(rect.left() + metrics.chevron_centre, rect.center().y),
        vec2(metrics.chevron, metrics.chevron),
    );
    // The chevron follows the title rather than staying dim on its own: at the
    // group weight a dim marker beside light text reads as disabled.
    icons::chevron(ui.painter(), chevron, ink, openness);
    ui.painter().text(
        pos2(rect.left() + metrics.title_left, rect.center().y),
        Align2::LEFT_CENTER,
        title,
        heading.font(),
        ink,
    );

    open
}

/// Stable id for a section's open/closed flag. Keyed by title alone rather than
/// by the surrounding `Ui`, so the state can be seeded from disk at startup.
pub fn section_id(title: &str) -> eframe::egui::Id {
    eframe::egui::Id::new(("geo-ref_mapper_section", title))
}

/// The stored title of a Config-page group. Prefixed so it cannot collide with
/// a DEBUG-panel section of the same name, and stable enough to persist.
pub fn config_section_title(title: &str) -> String {
    format!("config/{title}")
}

/// Dim uppercase label introducing a group of rows.
pub fn section_label(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
    ui.painter().text(
        pos2(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        text,
        theme::small_caps_font(),
        theme::TEXT_DIM,
    );
}

/// Key on the left, value on the right, both on one line. The value is elided
/// so a long one cannot run back underneath the key.
pub fn kv_row(ui: &mut Ui, key: &str, value: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());

    let key_width = ui
        .ctx()
        .fonts_mut(|f| f.layout_no_wrap(key.to_owned(), theme::label_font(), theme::TEXT_MUTED))
        .size()
        .x;
    let value = elide(
        ui,
        value,
        theme::label_font(),
        rect.width() - key_width - 12.0,
    );

    let painter = ui.painter();
    painter.text(
        pos2(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        key,
        theme::label_font(),
        theme::TEXT_MUTED,
    );
    painter.text(
        pos2(rect.right(), rect.center().y),
        Align2::RIGHT_CENTER,
        value,
        theme::label_font(),
        theme::TEXT,
    );
}

/// A key and value that share a line when they fit, and stack when they do not.
///
/// [`kv_row`] elides the value, which is right for a number or a short name.
/// Provenance strings are neither — `adobe:docid:photoshop:418da86b-…` and
/// `Adobe Photoshop CC 2019 (Windows)` are longer than a 268 pt panel is wide,
/// and eliding them to `adobe:docid:phot…` throws away the part worth reading.
/// Those get the full width of the panel and wrap onto as many lines as they
/// need.
pub fn kv_block(ui: &mut Ui, key: &str, value: &str) {
    let width = ui.available_width();
    let (key_width, value_width) = ui.ctx().fonts_mut(|f| {
        (
            f.layout_no_wrap(key.to_owned(), theme::label_font(), theme::TEXT_MUTED)
                .size()
                .x,
            f.layout_no_wrap(value.to_owned(), theme::label_font(), theme::TEXT)
                .size()
                .x,
        )
    });
    if key_width + value_width + 12.0 <= width {
        kv_row(ui, key, value);
        return;
    }

    let (label, _) = ui.allocate_exact_size(vec2(width, 18.0), Sense::hover());
    ui.painter().text(
        pos2(label.left(), label.center().y),
        Align2::LEFT_CENTER,
        key,
        theme::small_caps_font(),
        theme::TEXT_MUTED,
    );

    // Room for a paragraph. Past this the value gets its own scrolling box —
    // a TIFF's archival record runs to a couple of thousand characters, and
    // laid out in full it buries every row beneath it.
    const MAX_HEIGHT: f32 = 150.0;

    // Measured at the width the plain layout would use, purely to decide
    // whether this needs a box; the boxed path re-measures inside it.
    let galley = ui.ctx().fonts_mut(|f| {
        f.layout(
            value.to_owned(),
            theme::label_font(),
            theme::TEXT,
            width - 12.0,
        )
    });

    if galley.size().y <= MAX_HEIGHT {
        let (body, _) = ui.allocate_exact_size(vec2(width, galley.size().y + 4.0), Sense::hover());
        ui.painter()
            .galley(pos2(body.left(), body.top()), galley, theme::TEXT);
        return;
    }

    Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0, theme::HAIRLINE))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(6, 4))
        .show(ui, |ui| {
            eframe::egui::ScrollArea::vertical()
                // Keyed on the label so two long values on the same panel keep
                // their own scroll positions.
                .id_salt(("kv_block", key))
                .max_height(MAX_HEIGHT)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Measured inside the scroll area, which has already taken
                    // its scrollbar out of the width. Using the outer width here
                    // asked for scrollbar-many points more than there were, so
                    // the panel grew to fit and kept growing every frame.
                    let inner = ui.available_width();
                    let galley = ui.ctx().fonts_mut(|f| {
                        f.layout(value.to_owned(), theme::label_font(), theme::TEXT, inner)
                    });
                    let (body, _) =
                        ui.allocate_exact_size(vec2(inner, galley.size().y), Sense::hover());
                    ui.painter()
                        .galley(pos2(body.left(), body.top()), galley, theme::TEXT);
                });
        });
    ui.add_space(4.0);
}

/// Horizontal meter used for the debug tab.
pub fn meter(ui: &mut Ui, key: &str, value: &str, fraction: f32, color: Color32) {
    kv_row(ui, key, value);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
    let radius = CornerRadius::same(2);
    ui.painter().rect_filled(rect, radius, theme::PILL_TRACK);
    let filled = Rect::from_min_size(
        rect.min,
        vec2(rect.width() * fraction.clamp(0.0, 1.0), rect.height()),
    );
    ui.painter().rect_filled(filled, radius, color);
    ui.add_space(4.0);
}

/// Underlined tab strip. Returns `true` when the selection changed.
pub fn tab_strip(ui: &mut Ui, selected: &mut usize, labels: &[&str]) -> bool {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
    let width = rect.width() / labels.len() as f32;
    let mut changed = false;

    for (index, label) in labels.iter().enumerate() {
        let tab = Rect::from_min_size(
            rect.min + vec2(width * index as f32, 0.0),
            vec2(width, rect.height()),
        );
        let response = ui.interact(tab, ui.id().with(("tab", index)), Sense::click());
        if response.clicked() && *selected != index {
            *selected = index;
            changed = true;
        }

        let active = index == *selected;
        let color = if active {
            theme::TEXT
        } else if response.hovered() {
            theme::TEXT_MUTED
        } else {
            theme::TEXT_DIM
        };
        ui.painter().text(
            tab.center(),
            Align2::CENTER_CENTER,
            *label,
            theme::small_caps_font(),
            color,
        );
    }

    ui.painter()
        .line_segment([rect.left_bottom(), rect.right_bottom()], theme::hairline());

    // Sliding underline under the active tab.
    let position =
        ui.ctx()
            .animate_value_with_time(ui.id().with("tab_underline"), *selected as f32, 0.12);
    let underline = Rect::from_min_size(
        pos2(rect.left() + width * position, rect.bottom() - 2.0),
        vec2(width, 2.0),
    );
    ui.painter()
        .rect_filled(underline, CornerRadius::same(1), theme::ACCENT);

    ui.add_space(6.0);
    changed
}

/// Rounded surface with a hairline border that grows to fit whatever is put
/// in it.
///
/// The previous version took a fixed height and painted the frame at that size.
/// egui does not clip to it, so any group whose contents outgrew the guessed
/// number simply spilled past the border — which is what "Combined ceiling"
/// was doing under GPU MEMORY. Sizing from the content removes the guess and
/// the whole class of bug with it.
pub fn card<R>(ui: &mut Ui, contents: impl FnOnce(&mut Ui) -> R) -> R {
    let width = ui.available_width();
    Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0, theme::HAIRLINE))
        .corner_radius(theme::card_corner())
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            // Height comes from the contents; width does not, or a card holding
            // only toggles would end up narrower than the one above it.
            ui.set_min_width(width - 26.0);
            contents(ui)
        })
        .inner
}

/// A titled group that can be folded away, sized to its contents.
///
/// The open state is keyed on the title and lives in egui's memory, the same
/// as [`collapsing_section`], so the Config page and the DEBUG panel remember
/// themselves in the same way.
pub fn collapsing_card<R>(
    ui: &mut Ui,
    title: &str,
    default_open: bool,
    contents: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    let id = section_id(&config_section_title(title));
    if !collapsing_header(ui, id, title, default_open, Heading::Group) {
        return None;
    }
    ui.add_space(2.0);
    Some(card(ui, contents))
}

/// Pill-shaped on/off switch with a trailing label.
pub fn toggle(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let track = vec2(34.0, 18.0);
    let text_width = ui
        .ctx()
        .fonts_mut(|f| f.layout_no_wrap(label.to_owned(), theme::label_font(), theme::TEXT))
        .size()
        .x;
    let (rect, mut response) = ui.allocate_exact_size(
        vec2(track.x + 8.0 + text_width, track.y.max(20.0)),
        Sense::click(),
    );
    if response.clicked() {
        *on = !*on;
        // A click-sensing response does not set this by itself, and without it
        // a caller that watches `changed()` — rather than just handing over a
        // field to be flipped — sees a switch that never moves.
        response.mark_changed();
    }

    let track_rect = Rect::from_min_size(pos2(rect.left(), rect.center().y - track.y * 0.5), track);
    let how_on = ui
        .ctx()
        .animate_bool_with_time(response.id.with("switch"), *on, 0.12);

    // A switch inside a disabled group stops responding by itself, but nothing
    // makes it *look* stopped — and one that reads as live and does nothing is
    // worse than one that is plainly out of use.
    let live = ui.is_enabled();
    let fill = theme::PILL_TRACK.lerp_to_gamma(
        if live {
            theme::ACCENT
        } else {
            theme::SURFACE_HOVER
        },
        how_on,
    );
    ui.painter()
        .rect_filled(track_rect, CornerRadius::same(9), fill);
    ui.painter().rect_stroke(
        track_rect,
        CornerRadius::same(9),
        Stroke::new(1.0, theme::HAIRLINE),
        StrokeKind::Inside,
    );

    let knob_radius = track.y * 0.5 - 3.0;
    let knob_x = track_rect.left()
        + knob_radius
        + 3.0
        + how_on * (track_rect.width() - (knob_radius + 3.0) * 2.0);
    ui.painter().circle_filled(
        pos2(knob_x, track_rect.center().y),
        knob_radius,
        match (live, *on) {
            (false, _) => theme::TEXT_DIM,
            (true, true) => Color32::WHITE,
            (true, false) => theme::TEXT_MUTED,
        },
    );

    ui.painter().text(
        pos2(track_rect.right() + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::label_font(),
        match (live, response.hovered()) {
            (false, _) => theme::TEXT_DIM,
            (true, true) => theme::TEXT,
            (true, false) => theme::TEXT_MUTED,
        },
    );

    response
}

/// Compact filled button used in panel toolbars.
pub fn button(ui: &mut Ui, label: &str, width: f32, enabled: bool) -> Response {
    sized_button(ui, label, width, 24.0, theme::label_font(), enabled)
}

/// A button for a second rank of actions — shorter and in the smaller type.
///
/// Deliberately not the same size as [`button`]. The row above it is what the
/// panel is *for*; these are the things done occasionally, and drawing them
/// identically would make the eye weigh them the same.
pub fn small_button(ui: &mut Ui, label: &str, width: f32, enabled: bool) -> Response {
    sized_button(ui, label, width, 19.0, theme::small_caps_font(), enabled)
}

fn sized_button(
    ui: &mut Ui,
    label: &str,
    width: f32,
    height: f32,
    font: eframe::egui::FontId,
    enabled: bool,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        vec2(width, height),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );

    let fill = if !enabled {
        theme::SURFACE.gamma_multiply(0.5)
    } else if response.is_pointer_button_down_on() {
        theme::ACCENT.gamma_multiply(0.5)
    } else if response.hovered() {
        theme::SURFACE_HOVER
    } else {
        theme::SURFACE
    };
    ui.painter().rect_filled(rect, CornerRadius::same(5), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(5),
        Stroke::new(1.0, theme::HAIRLINE),
        StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        font,
        if enabled {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        },
    );

    response
}

/// One-line status text, truncated to the available width.
/// One line of status text, truncated to `width`.
///
/// The width is asked for rather than taken. Taking `available_width()` is
/// right only when the line is alone on its row — put anything beside it and
/// the two together overrun, and a panel that sizes to its contents grows to
/// fit and keeps growing. That has been the cause of every mysterious panel
/// widening in this program, three times over, so it is now impossible to write
/// by accident: a caller sharing a row has to say how much it wants.
///
/// [`status_line`] is the whole-row form, for the common case.
pub fn status_line_sized(ui: &mut Ui, text: &str, color: Color32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width.max(0.0), 18.0), Sense::hover());
    let galley = ui.ctx().fonts_mut(|f| {
        f.layout(
            text.to_owned(),
            theme::small_caps_font(),
            color,
            rect.width(),
        )
    });
    ui.painter().galley(
        pos2(rect.left(), rect.center().y - galley.size().y * 0.5),
        galley,
        color,
    );
}

/// The whole-row form: the line is alone and may have all of it.
pub fn status_line(ui: &mut Ui, text: &str, color: Color32) {
    status_line_sized(ui, text, color, ui.available_width());
}

/// Thin determinate progress bar.
pub fn progress_bar(ui: &mut Ui, fraction: Option<f32>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 3.0), Sense::hover());
    let radius = CornerRadius::same(2);
    ui.painter().rect_filled(rect, radius, theme::PILL_TRACK);

    match fraction {
        Some(fraction) => {
            let filled = Rect::from_min_size(
                rect.min,
                vec2(rect.width() * fraction.clamp(0.0, 1.0), rect.height()),
            );
            ui.painter().rect_filled(filled, radius, theme::ACCENT);
        }
        None => {
            // Indeterminate: a short bar sweeping back and forth.
            let phase = ui.input(|i| i.time as f32) * 0.9;
            let sweep = (phase.sin() * 0.5 + 0.5) * 0.75;
            let filled = Rect::from_min_size(
                pos2(rect.left() + rect.width() * sweep, rect.top()),
                vec2(rect.width() * 0.25, rect.height()),
            );
            ui.painter().rect_filled(filled, radius, theme::ACCENT);
            ui.ctx().request_repaint();
        }
    }
}

/// Shortens `text` with a trailing ellipsis until it fits `max_width`.
pub fn elide(ui: &Ui, text: &str, font: eframe::egui::FontId, max_width: f32) -> String {
    let width_of = |candidate: &str| {
        ui.ctx()
            .fonts_mut(|f| f.layout_no_wrap(candidate.to_owned(), font.clone(), theme::TEXT))
            .size()
            .x
    };

    if max_width <= 0.0 {
        return String::new();
    }
    if width_of(text) <= max_width {
        return text.to_owned();
    }

    let characters: Vec<char> = text.chars().collect();
    let mut keep = characters.len();
    while keep > 0 {
        keep -= 1;
        let candidate: String = characters[..keep].iter().collect::<String>() + "…";
        if width_of(&candidate) <= max_width {
            return candidate;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui_context() -> eframe::egui::Context {
        let ctx = eframe::egui::Context::default();
        let _ = ctx.run_ui(Default::default(), |_| {});
        ctx
    }

    #[test]
    fn elide_shortens_only_when_needed() {
        let ctx = ui_context();
        ctx.run_ui(Default::default(), |ui| {
            let short = elide(ui, "cat.png", theme::label_font(), 500.0);
            assert_eq!(short, "cat.png", "text that fits is untouched");

            let long = "a-really-very-extremely-long-photograph-filename-2026.jpeg";
            let cut = elide(ui, long, theme::label_font(), 60.0);
            assert!(cut.ends_with('…'), "expected an ellipsis, got {cut:?}");
            assert!(cut.chars().count() < long.chars().count());
        });
    }

    #[test]
    fn elide_handles_no_room_at_all() {
        let ctx = ui_context();
        ctx.run_ui(Default::default(), |ui| {
            assert_eq!(elide(ui, "anything", theme::label_font(), 0.0), "");
            assert_eq!(elide(ui, "anything", theme::label_font(), -10.0), "");
        });
    }

    #[test]
    fn elide_keeps_multi_byte_characters_intact() {
        let ctx = ui_context();
        ctx.run_ui(Default::default(), |ui| {
            // Truncating by bytes rather than characters would panic here.
            let cut = elide(ui, "日本語のファイル名です.png", theme::label_font(), 40.0);
            assert!(cut.is_empty() || cut.ends_with('…'));
        });
    }
    #[test]
    fn a_switch_that_moves_says_it_moved() {
        // Most callers hand `toggle` a field and let it flip it, so nobody
        // noticed it never reported the change. A caller that has to *act* on
        // the change — one where the switch stands for something that is not a
        // plain bool — saw a switch that could not be moved at all.
        let ctx = ui_context();
        let mut on = false;
        let mut reported = None;
        let _ = ctx.run_ui(Default::default(), |ui| {
            let response = toggle(ui, &mut on, "Hide anchors");
            reported = Some(response.changed());
        });
        // Untouched: nothing changed, and it must not claim otherwise or every
        // frame would look like a click.
        assert_eq!(reported, Some(false));
        assert!(!on);
    }
}

/// Centred placeholder text, for parts of a mockup with nothing in them yet.
pub fn placeholder_note(ui: &mut Ui, rect: Rect, text: &str) {
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        theme::label_font(),
        theme::TEXT_DIM,
    );
}

/// Asks before the one action in this application that takes a file away.
///
/// A modal rather than an inline row: everything else here is reversible or
/// writes somewhere new, and a delete that could be dismissed by clicking past
/// it is not the same promise. `Escape` cancels, and cancelling is the default —
/// nothing happens until the destructive button itself is pressed.
pub fn confirm_delete_dialog(ui: &mut Ui, app: &mut crate::app::GeoRefMapperApp) {
    let paths = app.confirm_delete.clone();
    if paths.is_empty() {
        return;
    }
    // One file is named; a batch is counted and the first few listed, because
    // "Move 12 images to Trash?" without saying which is not a question anyone
    // can answer.
    let name = match paths.as_slice() {
        [only] => only
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| only.display().to_string()),
        many => {
            let listed: Vec<String> = many
                .iter()
                .take(6)
                .map(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            if many.len() > listed.len() {
                format!(
                    "{}  and {} more",
                    listed.join(",  "),
                    many.len() - listed.len()
                )
            } else {
                listed.join(",  ")
            }
        }
    };

    let mut confirmed = false;
    let mut cancelled = false;

    eframe::egui::Modal::new(eframe::egui::Id::new("confirm_delete"))
        .frame(
            Frame::new()
                .fill(theme::PANEL_BG)
                .stroke(Stroke::new(1.0, theme::HAIRLINE))
                .corner_radius(theme::card_corner())
                .inner_margin(Margin::same(18)),
        )
        .show(ui.ctx(), |ui| {
            ui.set_width(380.0);
            ui.painter();

            let (title, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
            ui.painter().text(
                pos2(title.left(), title.center().y),
                Align2::LEFT_CENTER,
                &format!("Move {} to Trash?", crate::app::plural_images(paths.len())),
                theme::heading_font(),
                theme::TEXT,
            );

            // The whole filename, wrapped: the point of the dialog is that you
            // can see exactly which file is about to go.
            let width = ui.available_width();
            let galley = ui.ctx().fonts_mut(|fonts| {
                fonts.layout(name.clone(), theme::label_font(), theme::TEXT, width)
            });
            let (body, _) =
                ui.allocate_exact_size(vec2(width, galley.size().y + 8.0), Sense::hover());
            ui.painter()
                .galley(pos2(body.left(), body.top() + 4.0), galley, theme::TEXT);

            let note = ui.ctx().fonts_mut(|fonts| {
                fonts.layout(
                    "It goes to the desktop Trash, so you can put it back from your file manager."
                        .to_owned(),
                    theme::small_caps_font(),
                    theme::TEXT_DIM,
                    width,
                )
            });
            let (hint, _) =
                ui.allocate_exact_size(vec2(width, note.size().y + 14.0), Sense::hover());
            ui.painter()
                .galley(pos2(hint.left(), hint.top() + 6.0), note, theme::TEXT_DIM);

            ui.horizontal(|ui| {
                if button(ui, "Cancel", 110.0, true).clicked() {
                    cancelled = true;
                }
                ui.add_space(8.0);
                if button(ui, "Move to Trash", 150.0, true).clicked() {
                    confirmed = true;
                }
            });
        });

    if confirmed {
        app.confirm_delete();
    } else if cancelled {
        app.confirm_delete.clear();
    }
}

/// Offers to install at startup, or to install again over an older copy.
///
/// Raised once when the window opens and never again for the same build — see
/// `GeoRefMapperApp::decline_install`. "Not now" rather than "No": the question
/// comes back for the *next* build, and a plain no would suggest otherwise.
pub fn install_prompt_dialog(ui: &mut Ui, app: &mut crate::app::GeoRefMapperApp) {
    use crate::install::Prompt;
    let Some(prompt) = app.install_prompt else {
        return;
    };

    let (title, body) = match prompt {
        Prompt::NotInstalled => (
            "Install geo-ref_mapper for your user?",
            format!(
                "It goes in your home directory — the program in ~/.local/bin, its \
                 icon in your icon theme, and an entry in your applications menu so \
                 you can pin it. Nothing is installed system-wide and nothing asks \
                 for a password.\n\n\
                 This build: {}",
                crate::version::full_version(),
            ),
        ),
        Prompt::Stale => (
            "Update the installed copy?",
            format!(
                "The copy you are running is newer than the one installed, so the \
                 menu entry still starts the old one.\n\n\
                 Running:  {}\nInstalled:  {}",
                crate::version::full_version(),
                crate::install::installed_at()
                    .map(crate::version::stamp_of)
                    .unwrap_or_else(|| "unknown".to_owned()),
            ),
        ),
    };
    let confirm = match prompt {
        Prompt::NotInstalled => "Install",
        Prompt::Stale => "Update",
    };

    let mut accepted = false;
    let mut declined = false;

    eframe::egui::Modal::new(eframe::egui::Id::new("install_prompt"))
        .frame(
            Frame::new()
                .fill(theme::PANEL_BG)
                .stroke(Stroke::new(1.0, theme::HAIRLINE))
                .corner_radius(theme::card_corner())
                .inner_margin(Margin::same(18)),
        )
        .show(ui.ctx(), |ui| {
            ui.set_width(420.0);

            let (heading, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
            ui.painter().text(
                pos2(heading.left(), heading.center().y),
                Align2::LEFT_CENTER,
                title,
                theme::heading_font(),
                theme::TEXT,
            );

            let width = ui.available_width();
            let galley = ui.ctx().fonts_mut(|fonts| {
                fonts.layout(body.clone(), theme::label_font(), theme::TEXT_MUTED, width)
            });
            let (text, _) =
                ui.allocate_exact_size(vec2(width, galley.size().y + 14.0), Sense::hover());
            ui.painter().galley(
                pos2(text.left(), text.top() + 6.0),
                galley,
                theme::TEXT_MUTED,
            );

            ui.horizontal(|ui| {
                if button(ui, "Not now", 110.0, true)
                    .on_hover_text("The next build will ask again.")
                    .clicked()
                {
                    declined = true;
                }
                ui.add_space(8.0);
                if button(ui, confirm, 150.0, true).clicked() {
                    accepted = true;
                }
            });
        });

    if accepted {
        let now = ui.input(|i| i.time);
        app.accept_install(now);
    } else if declined {
        app.decline_install();
    }
}

/// Asks before moving or copying originals into another folder.
///
/// Says how many, where to, and how many would have to be renamed — a renamed
/// file is not where its name says it is, and that is worth knowing before
/// rather than after.
pub fn confirm_transfer_dialog(ui: &mut Ui, app: &mut crate::app::GeoRefMapperApp) {
    let Some((mode, destination, planned)) = app.confirm_transfer.clone() else {
        return;
    };
    let renamed = planned.iter().filter(|item| item.renamed).count();

    let mut go = false;
    let mut cancelled = false;

    eframe::egui::Modal::new(eframe::egui::Id::new("confirm_transfer"))
        .frame(
            Frame::new()
                .fill(theme::PANEL_BG)
                .stroke(Stroke::new(1.0, theme::HAIRLINE))
                .corner_radius(theme::card_corner())
                .inner_margin(Margin::same(18)),
        )
        .show(ui.ctx(), |ui| {
            ui.set_width(420.0);
            let (title, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
            ui.painter().text(
                pos2(title.left(), title.center().y),
                Align2::LEFT_CENTER,
                format!(
                    "{} {} to this folder?",
                    mode.verb(),
                    crate::app::plural_images(planned.len())
                ),
                theme::heading_font(),
                theme::TEXT,
            );

            kv_block(ui, "Destination", &destination.display().to_string());
            let note = match (mode, renamed) {
                (crate::transfer::Mode::Move, 0) => {
                    "The originals move. Nothing already in that folder is touched.".to_owned()
                }
                (crate::transfer::Mode::Copy, 0) => "The originals stay where they are.".to_owned(),
                (_, n) => format!(
                    "{n} of them would land on a name already in use, so those get a number \
                     added. Nothing already in that folder is overwritten."
                ),
            };
            let galley = ui.ctx().fonts_mut(|fonts| {
                fonts.layout(
                    note,
                    theme::small_caps_font(),
                    theme::TEXT_DIM,
                    ui.available_width(),
                )
            });
            let (body, _) = ui.allocate_exact_size(
                vec2(ui.available_width(), galley.size().y + 16.0),
                Sense::hover(),
            );
            ui.painter()
                .galley(pos2(body.left(), body.top() + 8.0), galley, theme::TEXT_DIM);

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if button(ui, "Cancel", 110.0, true).clicked() {
                    cancelled = true;
                }
                ui.add_space(8.0);
                if button(ui, mode.verb(), 130.0, true).clicked() {
                    go = true;
                }
            });
        });

    if go {
        app.run_transfer();
    } else if cancelled {
        app.confirm_transfer = None;
    }
}

/// How long a toast stays up. Long enough to read four words, short enough that
/// it is gone before it becomes furniture.
const TOAST_SECONDS: f64 = 2.0;

/// A brief confirmation, centred at the bottom of the window.
///
/// For actions whose entire effect is somewhere the user cannot see — the
/// clipboard being the obvious one. Without it, clicking the path looks like
/// clicking did nothing at all.
pub fn toast(ui: &mut Ui, app: &mut crate::app::GeoRefMapperApp) {
    let Some((message, shown_at)) = app.toast.clone() else {
        return;
    };
    let now = ui.input(|i| i.time);
    let age = now - shown_at;
    if age > TOAST_SECONDS {
        app.toast = None;
        return;
    }

    // Fades out over the last third rather than vanishing mid-read.
    let fade = (((TOAST_SECONDS - age) / (TOAST_SECONDS / 3.0)) as f32).clamp(0.0, 1.0);
    let screen = ui.ctx().viewport_rect();
    let galley = ui
        .ctx()
        .fonts_mut(|fonts| fonts.layout_no_wrap(message, theme::label_font(), theme::TEXT));
    let plate = Rect::from_center_size(
        pos2(screen.center().x, screen.bottom() - 64.0),
        galley.size() + vec2(28.0, 16.0),
    );

    let painter = ui.ctx().layer_painter(eframe::egui::LayerId::new(
        eframe::egui::Order::Foreground,
        eframe::egui::Id::new("toast"),
    ));
    painter.rect_filled(
        plate,
        CornerRadius::same(6),
        theme::SURFACE.gamma_multiply(fade),
    );
    painter.rect_stroke(
        plate,
        CornerRadius::same(6),
        Stroke::new(1.0, theme::HAIRLINE.gamma_multiply(fade)),
        StrokeKind::Inside,
    );
    painter.galley(
        plate.center() - galley.size() / 2.0,
        galley,
        theme::TEXT.gamma_multiply(fade),
    );
    // Keeps repainting so the fade actually runs and the toast clears itself.
    ui.ctx().request_repaint();
}
