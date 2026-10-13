//! Pill-shaped segmented control used in the title bar to switch pages.

use eframe::egui::{Align2, CornerRadius, Rect, Sense, Stroke, StrokeKind, Ui, vec2};

use crate::theme;

const SEGMENT_PADDING: f32 = 18.0;
const MIN_SEGMENT_WIDTH: f32 = 74.0;
/// The narrowest a segment can be and still be worth having.
///
/// Not a comfortable size — it is a couple of letters and a target you can
/// still hit. The pill is the only navigation in the window, so a cramped one
/// beats none, which is what a wide version string and a 250 % interface used
/// to leave behind.
const MIN_USABLE_SEGMENT: f32 = 16.0;
/// Taller than the buttons beside it: this is the main navigation, and the
/// title bar has 40 pt to spend.
pub const HEIGHT: f32 = 32.0;

/// Width the pill wants for the given labels.
pub fn desired_width(ui: &Ui, labels: &[&str]) -> f32 {
    let font = theme::label_font();
    let widest = labels
        .iter()
        .map(|label| {
            ui.ctx()
                .fonts_mut(|f| f.layout_no_wrap((*label).to_owned(), font.clone(), theme::TEXT))
                .size()
                .x
        })
        .fold(0.0_f32, f32::max);

    (widest + SEGMENT_PADDING * 2.0).max(MIN_SEGMENT_WIDTH) * labels.len() as f32
}

/// Below this the pill is not worth drawing at all.
///
/// Scales with how many pages there are, because that is what the width has to
/// be divided between: five segments can live in a space seven cannot.
pub fn min_width(labels: &[&str]) -> f32 {
    MIN_USABLE_SEGMENT * labels.len() as f32
}

/// A label cut down to what its segment can hold.
///
/// `elide` gives up gracefully and returns `…`, or nothing at all, once there
/// is no room — but a row of ellipses names nothing and cannot be navigated by.
/// One letter is not the word either, yet it is the word's *first* letter,
/// which is enough to tell Viewer from Mosaic from Config.
fn fit(ui: &Ui, label: &str, width: f32) -> String {
    let elided = crate::widgets::elide(ui, label, theme::label_font(), width);
    if elided.chars().all(|glyph| glyph == '…') {
        return label.chars().next().map(String::from).unwrap_or_default();
    }
    elided
}

/// Draws the control into `rect`. Returns `true` if the selection changed.
pub fn segmented(ui: &mut Ui, rect: Rect, selected: &mut usize, labels: &[&str]) -> bool {
    if labels.is_empty() {
        return false;
    }

    let track_radius = CornerRadius::same((rect.height() * 0.5) as u8);
    ui.painter()
        .rect_filled(rect, track_radius, theme::PILL_TRACK);
    ui.painter().rect_stroke(
        rect,
        track_radius,
        Stroke::new(1.0, theme::HAIRLINE),
        StrokeKind::Inside,
    );

    let segment_width = rect.width() / labels.len() as f32;
    let segment_rect = |index: usize| {
        Rect::from_min_size(
            rect.min + vec2(segment_width * index as f32, 0.0),
            vec2(segment_width, rect.height()),
        )
    };

    // Interact first so we can render hover states in the same frame.
    let mut changed = false;
    let mut hovered = None;
    for index in 0..labels.len() {
        let id = ui.id().with(("pill_segment", index));
        let response = ui.interact(segment_rect(index), id, Sense::click());
        if response.hovered() {
            hovered = Some(index);
        }
        if response.clicked() && *selected != index {
            *selected = index;
            changed = true;
        }
    }

    // The thumb slides between segments rather than jumping.
    let thumb_index =
        ui.ctx()
            .animate_value_with_time(ui.id().with("pill_thumb"), *selected as f32, 0.14);
    let thumb = Rect::from_min_size(
        rect.min + vec2(segment_width * thumb_index, 0.0),
        vec2(segment_width, rect.height()),
    )
    .shrink(2.0);
    ui.painter().rect_filled(
        thumb,
        CornerRadius::same((thumb.height() * 0.5) as u8),
        theme::PILL_THUMB,
    );

    for (index, label) in labels.iter().enumerate() {
        let is_selected = index == *selected;
        let color = if is_selected {
            theme::TEXT
        } else if hovered == Some(index) {
            theme::TEXT
        } else {
            theme::TEXT_MUTED
        };
        // Cut to the segment. Squeezed narrow — a long version string beside it,
        // or the interface scaled up — the labels used to run straight over
        // their neighbours, because nothing here measured them against the room
        // they had.
        let text = fit(ui, label, segment_width - 6.0);
        ui.painter().text(
            segment_rect(index).center(),
            Align2::CENTER_CENTER,
            text,
            theme::label_font(),
            color,
        );
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PAGE_LABELS;

    #[test]
    fn the_floor_scales_with_how_many_pages_there_are() {
        // Five segments fit where seven do not, so the floor cannot be one
        // number. With map support off the pill needs meaningfully less room,
        // which is the case that has to keep working on a narrow window.
        let all = min_width(PAGE_LABELS);
        let without_maps: Vec<&str> = crate::pages::visible_pages(false)
            .iter()
            .map(|page| PAGE_LABELS[*page])
            .collect();
        assert!(min_width(&without_maps) < all);
        assert!(all < desired_width_floor(PAGE_LABELS.len()));
    }

    /// What `desired_width` would come to if every label were empty — the
    /// comfortable width, which the floor must sit well below.
    fn desired_width_floor(count: usize) -> f32 {
        MIN_SEGMENT_WIDTH * count as f32
    }

    #[test]
    fn a_label_is_cut_to_its_segment_rather_than_over_its_neighbour() {
        let ctx = eframe::egui::Context::default();
        let _ = ctx.run_ui(Default::default(), |ui| {
            let width_of = |text: &str| {
                ui.ctx()
                    .fonts_mut(|f| {
                        f.layout_no_wrap(text.to_owned(), theme::label_font(), theme::TEXT)
                    })
                    .size()
                    .x
            };

            // Roomy: left alone.
            assert_eq!(fit(ui, "Viewer", 200.0), "Viewer");

            // Squeezed: cut down, and it really does fit.
            let squeezed = fit(ui, "Viewer", 30.0);
            assert_ne!(squeezed, "Viewer");
            assert!(
                width_of(&squeezed) <= 30.0,
                "{squeezed:?} is {:.0} pt in 30",
                width_of(&squeezed),
            );

            // At the floor, every label still says *something*, and something
            // different from its neighbours — a row of ellipses would be a
            // control nobody can navigate by.
            let segment = MIN_USABLE_SEGMENT - 6.0;
            let cut: Vec<String> = PAGE_LABELS
                .iter()
                .map(|label| fit(ui, label, segment))
                .collect();
            for (label, text) in PAGE_LABELS.iter().zip(&cut) {
                assert!(!text.is_empty(), "{label} vanished");
                assert!(!text.chars().all(|c| c == '…'), "{label} became {text:?}");
            }
            // Viewer, Mosaic, Map, Map Images, Formats, Export, Config — the
            // two Map pages share a first letter, which is the honest limit of
            // this and worth stating rather than asserting away.
            assert_eq!(cut[0], "V");
        });
    }
}
