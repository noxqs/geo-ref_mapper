//! The four pages reachable from the title-bar pill. All of them are mockups
//! for now — real decoding, browsing and settings get wired in behind them.

use eframe::egui::{
    Align2, Color32, CornerRadius, Rect, ScrollArea, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::app::GeoRefMapperApp;
use crate::files::Kind;
use crate::viewport::ZoomMode;
use crate::{formats, icons, theme, widgets};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ZoomAction {
    Mode(ZoomMode),
    In,
    Out,
    /// The read-out in the middle of the toolbar.
    None,
}

/// Indices into `chrome::PAGE_LABELS`, named where the code has to reason about
/// which page it is rather than just switch on it.
pub const PAGE_VIEWER: usize = 0;
pub const PAGE_MOSAIC: usize = 1;
pub const PAGE_MAP: usize = 2;
/// Maps queued to be converted into a form that zooms quickly.
pub const PAGE_MAP_IMAGES: usize = 3;
pub const PAGE_FORMATS: usize = 4;
pub const PAGE_EXPORT: usize = 5;

/// Whether a page only exists because map support is on.
pub fn is_map_page(page: usize) -> bool {
    matches!(page, PAGE_MAP | PAGE_MAP_IMAGES)
}

/// The pages the pill offers, as indices into `app::PAGE_LABELS`.
///
/// Indices rather than a shortened label list: `PAGE_MAP` and the rest are
/// positions in that array, and every one of them is compared against
/// `app.page` somewhere. Dropping two entries would renumber Formats, Export
/// and Config, so the pill translates between what it shows and what the page
/// is called instead.
pub fn visible_pages(map_support: bool) -> Vec<usize> {
    (0..crate::app::PAGE_LABELS.len())
        .filter(|page| map_support || !is_map_page(*page))
        .collect()
}

pub fn show(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    // Nothing should be able to reach a map page with the switch off —
    // `enter_page` turns those away — but a page number is a plain `usize` and
    // this is the one place it becomes a screen.
    if !app.config.map_support && is_map_page(app.page) {
        app.page = PAGE_VIEWER;
    }
    match app.page {
        PAGE_VIEWER => viewer(ui, app),
        PAGE_MOSAIC => mosaic(ui, app),
        PAGE_MAP => map(ui, app),
        PAGE_MAP_IMAGES => map_images(ui, app),
        PAGE_FORMATS => pipelines(ui),
        PAGE_EXPORT => export(ui, app),
        _ => config(ui, app),
    }
}

#[cfg(test)]
mod which_pages {
    use super::*;
    use crate::app::PAGE_LABELS;

    #[test]
    fn the_pill_offers_every_page_when_maps_are_on() {
        assert_eq!(
            visible_pages(true),
            (0..PAGE_LABELS.len()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_two_map_pages_are_the_only_ones_taken_away() {
        let shown = visible_pages(false);
        assert!(!shown.contains(&PAGE_MAP));
        assert!(!shown.contains(&PAGE_MAP_IMAGES));
        for page in [PAGE_VIEWER, PAGE_MOSAIC, PAGE_FORMATS, PAGE_EXPORT] {
            assert!(shown.contains(&page), "{}", PAGE_LABELS[page]);
        }
        // Config is whatever is left over in `show`, so it is named by
        // position rather than by a constant — and it has to still be there,
        // since it is where the switch lives.
        assert!(shown.contains(&(PAGE_LABELS.len() - 1)));
    }

    #[test]
    fn a_page_still_answers_to_its_own_number() {
        // The pill shows a shortened list but acts on page numbers, so what it
        // hands back must be the constant and not the segment. This is the
        // translation it makes, and getting it wrong sends Formats to the Map.
        let shown = visible_pages(false);
        for (segment, page) in shown.iter().enumerate() {
            assert_eq!(PAGE_LABELS[*page], PAGE_LABELS[shown[segment]]);
            assert!(!is_map_page(*page));
        }
        // Config was last with maps on and is still last with them off, two
        // segments earlier.
        assert_eq!(shown.last().copied(), Some(PAGE_LABELS.len() - 1));
        assert_eq!(shown.len(), PAGE_LABELS.len() - 2);
    }
}

// --------------------------------------------------------------------- Viewer

/// Draws the decoded image and handles pan / zoom. Returns the zoom to show in
/// the toolbar, if anything is on screen.
fn image_canvas(ui: &mut Ui, app: &mut GeoRefMapperApp, canvas: Rect) -> Option<f32> {
    let response = ui.interact(
        canvas,
        ui.id().with("image_canvas"),
        Sense::click_and_drag(),
    );
    // Both mouse buttons hang off one question: is any of the picture off the
    // edge of the canvas? Asked first, and from last frame's texture — `peek`
    // rather than `get`, so asking does not disturb which textures the cache
    // thinks are in use.
    //
    // Zoomed in there is picture to reach, so the left button drags towards it
    // and the right button undoes the zoom. Zoomed out there is neither: the
    // left button has nothing to pan and moves the *window* instead, and the
    // right button keeps the menu it has always had.
    let zoomed_in = app
        .current
        .as_deref()
        .and_then(|path| app.textures.peek(path))
        .map(|entry| vec2(entry.size[0] as f32, entry.size[1] as f32))
        .is_some_and(|size| app.viewport.pannable(canvas, size));

    if zoomed_in {
        // Right-click is the way back out of a zoom, matching the wheel that
        // got you into it — no journey to the toolbar, and the pointer is
        // already over the picture.
        if response.secondary_clicked() {
            app.viewport.set_mode(ZoomMode::Fit);
        }
    } else if let Some(file) = app.current.clone() {
        // The picture itself gets the same menu as its tile does elsewhere:
        // this is where you are actually looking at it, and it is the most
        // likely place to want it on the clipboard.
        let batch = app.files.action_paths();
        let stack_menu = app.stack_menu(&batch);
        let mut menu = crate::panels::MenuResult::default();
        crate::panels::file_context_menu(
            ui,
            &response,
            &file,
            &batch,
            Some(&stack_menu),
            app.is_stacked(&file),
            app.config.map_support,
            &mut menu,
        );
        let now = ui.input(|i| i.time);
        app.apply_menu(menu, &batch, now);
    }

    // Nothing to pan, so the drag is about the window. Which is the whole
    // point of a picture-only launch: the window has no title bar of its own
    // on screen, and without this there is nowhere on it to take hold of.
    //
    // Not while full screen, where there is no window to move and the gesture
    // would be a drag that quietly did nothing.
    let fullscreen = ui.ctx().input(|i| i.viewport().fullscreen) == Some(true);
    let moves_window = !zoomed_in && !fullscreen;
    if moves_window && response.drag_started() {
        ui.ctx()
            .send_viewport_cmd(eframe::egui::ViewportCommand::StartDrag);
    }

    let Some(path) = app.current.clone() else {
        app.forget_painted();
        // A shut folder says so by name. This is the row the arrows land on
        // between one folder and the next, and naming it is the whole point of
        // showing nothing here: it is what tells you the ground has moved.
        let shut = match app.files.selected_node() {
            Some(node) if node.is_folder() && !node.expanded => Some(format!(
                "{}  ·  {} inside\n\nPress → or click the arrow to open it",
                node.name,
                crate::app::plural_images(node.nested_images),
            )),
            _ => None,
        };
        let note = match &shut {
            Some(text) => text.as_str(),
            None => match app.files.selected_node() {
                Some(node) if !node.is_folder() => {
                    "This is sample data — scan a folder to view real images"
                }
                _ => "Pick an image in the FILES panel  ·  drag and drop  ·  Ctrl+O",
            },
        };
        widgets::placeholder_note(ui, canvas, note);
        return None;
    };

    if let Some((failed, error)) = &app.decode_error
        && *failed == path
    {
        let note = format!("Could not decode this file — {error}");
        app.forget_painted();
        widgets::placeholder_note(ui, canvas, &note);
        return None;
    }

    let Some(entry) = app.textures.get(&path) else {
        // Still decoding: say so rather than flashing an empty canvas.
        let fraction = app
            .loading
            .as_ref()
            .filter(|(loading, _)| *loading == path)
            .map_or(0.0, |(_, fraction)| *fraction);
        // Say what is actually happening rather than a generic "decoding".
        let stage = app
            .loading_stage
            .map_or("Opening", crate::loader::Stage::label);
        let detail = match (app.loading_stage, app.loading_strips) {
            (Some(crate::loader::Stage::Decoding), Some((count, mask))) => {
                format!("  ·  {} of {count} strips", mask.count_ones())
            }
            _ => String::new(),
        };
        // The canvas used to empty here for the whole decode, which is what
        // reads as a flash between one map and the next. The picture being
        // left is kept up instead — the bar below still says work is going on,
        // so nothing is hidden by it.
        if let Some((texture, rect)) = &app.painted {
            ui.painter().with_clip_rect(canvas).image(
                texture.id(),
                *rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            widgets::placeholder_note(
                ui,
                canvas,
                &format!("{stage}{detail}  ·  {:.0}%", fraction * 100.0),
            );
        }
        loading_bar(ui, canvas, &path, fraction, app.loading_strips);
        ui.ctx().request_repaint();
        return None;
    };

    let texture = entry.texture.id();
    let image_size = vec2(entry.size[0] as f32, entry.size[1] as f32);
    // Copied out before anything else borrows `app` mutably.
    let handle = entry.texture.clone();
    let pyramid = entry.timings.pyramid;

    // Both spellings zoom here: the wheel on its own, and Ctrl+wheel, which
    // egui reports as a zoom rather than a scroll. On this page there is
    // nothing to scroll, so there is no reason to make people learn which is
    // which.
    let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
    let factor = if pinch != 1.0 {
        pinch
    } else {
        (scroll * 0.004).exp()
    };
    if response.hovered()
        && factor != 1.0
        && let Some(pointer) = response.hover_pos()
    {
        // Zooming out never leaves the page. It used to hand back to the
        // Mosaic once the picture was fully out, which meant an ordinary wheel
        // gesture could throw you off the image you were looking at. Escape is
        // the way back, and it is the only way back.
        app.viewport.zoom_at(pointer, canvas, image_size, factor);
    }
    if response.clicked() || response.dragged() {
        app.focus = crate::app::Focus::Images;
    }
    if response.dragged() && zoomed_in {
        app.viewport.pan(response.drag_delta(), canvas, image_size);
    }
    // Double-click goes full screen. 1:1 is still a click away on the toolbar
    // and on the `1` key, which is where people look for it.
    if response.double_clicked() {
        app.toggle_presentation(ui.ctx());
    }
    // The same open hand either way — both gestures are "take hold and pull",
    // one of the picture and one of the window. Left alone when neither is on
    // offer, which is a picture that fits inside a full-screen window: an open
    // hand there would promise a drag that does nothing.
    if zoomed_in || moves_window {
        if response.dragged() {
            ui.ctx().set_cursor_icon(eframe::egui::CursorIcon::Grabbing);
        } else if response.hovered() {
            ui.ctx().set_cursor_icon(eframe::egui::CursorIcon::Grab);
        }
    }

    // Re-clamp after a resize so the image never drifts off the new canvas —
    // except while a glide is running, which is deliberately outside the rules
    // on its way back into them.
    if app.offset_glide.is_none() {
        app.viewport.clamp_offset(canvas, image_size);
    }

    let target = app.viewport.image_rect(canvas, image_size);
    let painter = ui.painter().with_clip_rect(canvas);
    let whole = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

    // Where the change from the last picture has got to. Asked here, on the
    // first frame this picture's texture exists, rather than when the
    // selection moved: on a large map those are a whole decode apart.
    let step = app.crossfade.advance(&path, ui.input(|input| input.time));
    if step.arrived {
        // Whatever was on the canvas is what fades out from underneath.
        app.leaving = app.painted.take();
    }
    app.painted = Some((handle, target));
    if step.running() {
        // The one being left stays fully opaque for the whole crossing, so
        // there is never a frame with neither picture on it — that is the
        // difference between a fade and a blink through black.
        if let Some((leaving, rect)) = &app.leaving {
            painter.image(leaving.id(), *rect, whole, Color32::WHITE);
        }
        ui.ctx().request_repaint();
    } else {
        app.leaving = None;
    }

    // A layered picture is drawn as squares at the level the zoom asks for.
    // The base texture is still painted underneath as the first stand-in, so
    // there is something on screen from the opening frame.
    let layered = app.squares.is_layered();
    painter.image(
        texture,
        target,
        whole,
        Color32::WHITE.gamma_multiply(step.alpha),
    );

    let visible = target.intersect(canvas);
    // Not while the crossing runs: a sharp square at full strength over a
    // picture that is still coming up would be the pop the fade is there to
    // avoid, and a seventh of a second later they arrive anyway.
    if layered && !step.running() && visible.width() >= 1.0 && visible.height() >= 1.0 {
        draw_squares(app, &painter, target, canvas);
        if app.tile_debug
            && let Some(painted) = app.squares_shown
            && let Some(path) = app.current.clone()
        {
            tiling_hud(&painter, canvas, &painted, &path);
        }
    } else if !app.current_levels.is_empty() && visible.width() >= 1.0 && visible.height() >= 1.0 {
        // Not layered: the older path, which sharpens a patch over the base.
        detail_layer(app, &painter, target, visible, image_size.x as u32);
    }

    if let Some(description) = &app.current_format {
        // The pyramid level is only known once a level has actually been
        // decoded, so it is appended here rather than sniffed with the format.
        // What is actually on screen: the sharpened patch's level when one is
        // painted, otherwise the level the base texture came from.
        let level = app
            .squares_shown
            .map(|painted| {
                // What this view actually cost: which level, and how many
                // squares of it were read — not the level's total, which is
                // what it used to show and made a cheap view look expensive.
                let mut text = format!(
                    "level {} of {}  ·  {} squares  ·  {} held",
                    painted.level + 1,
                    painted.levels,
                    painted.drawn + painted.waiting,
                    painted.resident,
                );
                if painted.waiting > 0 {
                    text.push_str(&format!("  ·  {} loading", painted.waiting));
                }
                text
            })
            .or_else(|| {
                app.detail_patch.as_ref().map(|patch| {
                    format!(
                        "level {} of {}  ·  {} squares read",
                        patch.request.level + 1,
                        app.current_levels.len(),
                        patch.squares,
                    )
                })
            })
            .or_else(|| {
                pyramid.map(|(level, levels, tiles)| {
                    // The whole layer is read to make the first picture, so
                    // here the layer's own square count *is* what it cost.
                    format!("level {} of {}  ·  {tiles} squares read", level + 1, levels)
                })
            });
        // Not in presentation mode: "only the picture" has to mean it.
        if !app.presenting {
            format_badge(ui, canvas, description, level.as_deref());
        }
    }

    Some(app.viewport.zoom(canvas.size(), image_size) * 100.0)
}

/// Names what is on screen, above the picture, with the subfolder switch on
/// the right.
///
/// Laid out like the Mosaic's header — same font, same inset, same toggle in
/// the same place — because they answer the same questions and switching pages
/// should not move the furniture.
fn viewer_header(ui: &mut Ui, app: &mut GeoRefMapperApp, header: Rect) {
    ui.painter()
        .rect_filled(header, CornerRadius::ZERO, theme::PANEL_BG);
    ui.painter().line_segment(
        [header.left_bottom(), header.right_bottom()],
        theme::hairline(),
    );

    const RECURSIVE_WIDTH: f32 = 132.0;
    let controls = Rect::from_min_max(
        pos2(header.right() - 14.0 - RECURSIVE_WIDTH, header.top()),
        pos2(header.right() - 14.0, header.bottom()),
    );
    ui.scope_builder(
        eframe::egui::UiBuilder::new().max_rect(controls).layout(
            eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
        ),
        |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            widgets::toggle(ui, &mut app.config.mosaic_recursive, "Recursive");
        },
    );

    let Some(path) = app.current.clone() else {
        return;
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    // With a trailing separator, so the two read as one path rather than as two
    // separate fields that happen to be adjacent.
    let folder = path
        .parent()
        .map(|folder| format!("{}/", folder.to_string_lossy()))
        .unwrap_or_default();

    let text_rect = Rect::from_min_max(
        pos2(header.left() + 14.0, header.top()),
        pos2(controls.left() - 12.0, header.bottom()),
    );
    let response = ui.interact(text_rect, ui.id().with("viewer_header"), Sense::click());
    if response.hovered() {
        ui.ctx()
            .set_cursor_icon(eframe::egui::CursorIcon::PointingHand);
    }
    if response.clicked() {
        ui.ctx().copy_text(path.to_string_lossy().into_owned());
        app.toast = Some(("Path copied to clipboard".to_owned(), ui.input(|i| i.time)));
    }

    let font = theme::small_caps_font();
    let (folder_ink, name_ink) = if response.hovered() {
        (theme::ACCENT, theme::ACCENT)
    } else {
        (theme::TEXT_MUTED, theme::TEXT)
    };

    // The name wraps rather than being cut off. Some of these run to two
    // hundred characters — "A Mapp or Generall Carte of the World. Designed in
    // two plaine hemispheres by Monsieur Sanson…" — and eliding one leaves the
    // half that distinguishes it from its neighbours on the cutting-room floor.
    //
    // The folder is put on its own line above rather than run in front, which
    // is what let the name start halfway across and wrap raggedly.
    let painter = ui.painter().with_clip_rect(header);
    let folder = widgets::elide(ui, &folder, font.clone(), text_rect.width());
    painter.text(
        pos2(text_rect.left(), text_rect.top() + 11.0),
        Align2::LEFT_CENTER,
        folder,
        font.clone(),
        folder_ink,
    );

    let galley = ui
        .ctx()
        .fonts_mut(|fonts| fonts.layout(name, theme::label_font(), name_ink, text_rect.width()));
    painter.galley(
        pos2(text_rect.left(), text_rect.top() + 20.0),
        galley,
        name_ink,
    );
}

/// How tall the Viewer's header has to be for this picture's name.
///
/// Measured rather than fixed, because the answer is one line for most files
/// and four for a seventeenth-century title. Capped so a single absurd name
/// cannot take half the window from the picture it names.
fn viewer_header_height(ui: &Ui, app: &GeoRefMapperApp, width: f32) -> f32 {
    const ONE_LINE: f32 = 34.0;
    const MOST: f32 = 96.0;
    let Some(name) = app
        .current
        .as_deref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return ONE_LINE;
    };
    let galley = ui.ctx().fonts_mut(|fonts| {
        fonts.layout(
            name,
            theme::label_font(),
            theme::TEXT,
            (width - 160.0).max(80.0),
        )
    });
    (26.0 + galley.size().y).clamp(ONE_LINE, MOST)
}

/// What the tiling is doing, drawn over the picture. Toggled with D.
///
/// Ported from histoMap, which has had this since the start — and which is why
/// a level that never changed was obvious there and invisible here.
fn tiling_hud(
    painter: &eframe::egui::Painter,
    area: Rect,
    painted: &crate::imagetiles::Painted,
    path: &std::path::Path,
) {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut text = String::from("TILING  (D to hide)\n");
    text.push_str(&format!(
        "level {} of {}   scale {:.4} screen px per image px\n",
        painted.level + 1,
        painted.levels,
        painted.scale,
    ));
    text.push_str(&format!(
        "level {} x {} px   grid {} x {}\n",
        painted.shape.width,
        painted.shape.height,
        painted.shape.across(),
        painted.shape.down(),
    ));
    text.push_str(&format!(
        "drawn {}   waiting {}   cut short {}   held {}\n",
        painted.drawn, painted.waiting, painted.partial, painted.resident,
    ));
    text.push_str(&format!(
        "picture {} x {} px   square {} px\n",
        painted.full.width, painted.full.height, painted.shape.tile_width,
    ));
    text.push_str("blue = whole   orange = cut short by the edge   red = not here yet\n");
    text.push_str(&name);

    let font = eframe::egui::FontId::monospace(11.0);
    let galley = painter.layout(text, font, theme::TEXT, area.width() - 24.0);
    let plate = Rect::from_min_size(
        area.left_top() + vec2(10.0, 10.0),
        galley.size() + vec2(12.0, 10.0),
    );
    painter.rect_filled(plate, CornerRadius::same(4), Color32::from_black_alpha(210));
    painter.galley(plate.min + vec2(6.0, 5.0), galley, theme::TEXT);
}

/// Draws the picture as squares. The shared code does the work; this only
/// records what it cost so the readout can say.
fn draw_squares(
    app: &mut GeoRefMapperApp,
    painter: &eframe::egui::Painter,
    target: Rect,
    canvas: Rect,
) {
    app.squares_shown = crate::imagetiles::draw(
        &mut app.squares,
        painter,
        target,
        canvas,
        Color32::WHITE,
        app.tile_debug,
    );
}

/// Asks for the sharp version of what is on screen, and paints the last one we
/// were given. The patch is drawn even while a newer one is being decoded — a
/// slightly stale sharp patch beats flicking back to a blurry base.
fn detail_layer(
    app: &mut GeoRefMapperApp,
    painter: &eframe::egui::Painter,
    target: Rect,
    visible: Rect,
    base_width: u32,
) {
    let levels = app.current_levels.clone();
    let wanted = crate::detail::needed(&levels, base_width, visible, target, |pixels, fraction| {
        crate::cog::level_for_zoom(&levels, pixels, fraction)
    });

    match wanted {
        Some((level, region)) => {
            if let Some(path) = app.current.clone() {
                app.detail.request(crate::detail::Request {
                    path,
                    level,
                    region,
                });
            }
        }
        // Zoomed back out far enough that the base texture is sharp again.
        None => {
            app.detail_patch = None;
            app.detail.reset();
            return;
        }
    }

    if let Some(patch) = &app.detail_patch
        && let Some((level_width, level_height)) = levels.get(patch.request.level).copied()
    {
        let rect =
            crate::detail::patch_rect(patch.request.region, level_width, level_height, target);
        painter.image(
            patch.texture.id(),
            rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

/// Small label in the top-right corner of the canvas naming what the file is.
///
/// It sits over the image, so it is drawn on a translucent plate rather than
/// bare text — a light photograph would otherwise swallow it.
fn format_badge(ui: &Ui, canvas: Rect, description: &formats::Description, extra: Option<&str>) {
    const MARGIN: f32 = 10.0;
    const PAD_X: f32 = 8.0;
    const PAD_Y: f32 = 4.0;
    const GAP: f32 = 6.0;

    let painter = ui.painter().with_clip_rect(canvas);
    let name_font = theme::small_caps_font();
    let detail_font = theme::small_caps_font();

    let width_of = |text: &str, font: &eframe::egui::FontId| {
        ui.ctx().fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(text.to_owned(), font.clone(), theme::TEXT)
                .size()
                .x
        })
    };

    let name = description.name.to_uppercase();
    // The live pyramid level supersedes the static one sniffed from the header:
    // "level 3 of 9" says more than "5 levels" once something is on screen.
    let detail = extra
        .map(str::to_owned)
        .or_else(|| description.detail.clone());
    let name_width = width_of(&name, &name_font);
    let detail_width = detail
        .as_ref()
        .map_or(0.0, |detail| width_of(detail, &detail_font) + GAP);

    let height = name_font.size + PAD_Y * 2.0;
    let width = name_width + detail_width + PAD_X * 2.0;
    // Never let a long detail string run off the left edge of the canvas.
    let width = width.min((canvas.width() - MARGIN * 2.0).max(0.0));
    let badge = Rect::from_min_size(
        pos2(canvas.right() - MARGIN - width, canvas.top() + MARGIN),
        vec2(width, height),
    );

    painter.rect_filled(
        badge,
        CornerRadius::same(4),
        theme::WINDOW_BG.gamma_multiply(0.78),
    );
    painter.rect_stroke(
        badge,
        CornerRadius::same(4),
        Stroke::new(1.0, theme::HAIRLINE),
        StrokeKind::Inside,
    );

    painter.text(
        pos2(badge.left() + PAD_X, badge.center().y),
        Align2::LEFT_CENTER,
        &name,
        name_font,
        theme::TEXT,
    );
    if let Some(detail) = &detail {
        painter.text(
            pos2(badge.left() + PAD_X + name_width + GAP, badge.center().y),
            Align2::LEFT_CENTER,
            detail,
            detail_font,
            theme::TEXT_MUTED,
        );
    }
}

/// Thin bar across the top of the canvas showing how far the load has got.
///
/// It grows left to right and reaches the right edge exactly when the texture
/// is ready. When the JPEG was split at its restart markers the bar is drawn as
/// one segment per strip, each lighting up as that strip lands — so it shows
/// the decode happening in parallel rather than pretending it is sequential.
fn loading_bar(
    ui: &Ui,
    canvas: Rect,
    path: &std::path::Path,
    fraction: f32,
    strips: Option<(u32, u32)>,
) {
    const HEIGHT: f32 = 5.0;
    let track = Rect::from_min_size(canvas.left_top(), vec2(canvas.width(), HEIGHT));
    ui.painter()
        .rect_filled(track, CornerRadius::ZERO, theme::PILL_TRACK);

    match strips {
        Some((count, mask)) if count > 1 => {
            let gap = 1.0;
            let width = (track.width() - gap * (count - 1) as f32) / count as f32;
            for index in 0..count {
                let done = mask & (1 << index.min(31)) != 0;
                let segment = Rect::from_min_size(
                    pos2(track.left() + index as f32 * (width + gap), track.top()),
                    vec2(width.max(1.0), HEIGHT),
                );
                // Strips finish out of order, so each segment is drawn where
                // its part of the image actually is.
                ui.painter().rect_filled(
                    segment,
                    CornerRadius::ZERO,
                    if done {
                        theme::ACCENT
                    } else {
                        theme::ACCENT.gamma_multiply(0.18)
                    },
                );
            }
        }
        _ => {
            // Key the animation by file. A single shared id would carry the
            // previous image's value over and animate *down* to the new one's,
            // which looked like the bar running backwards.
            let id = ui.id().with(("load_progress", path));
            let target = fraction.clamp(0.0, 1.0);
            let shown = ui.ctx().animate_value_with_time(id, target, 0.25);
            // The first frame for a file must start empty, not wherever the
            // easing happens to be.
            let shown = if target <= f32::EPSILON {
                0.0
            } else {
                shown.min(target.max(shown))
            };
            let filled = Rect::from_min_size(track.min, vec2(track.width() * shown, HEIGHT));
            ui.painter()
                .rect_filled(filled, CornerRadius::ZERO, theme::ACCENT);
        }
    }
    ui.ctx().request_repaint();
}

/// The two things presentation mode otherwise hides: which folder this is, and
/// whether it is marked.
///
/// Presentation mode takes away the header, which is the only place either was
/// said. Going through a folder marking keepers with the spacebar then had no
/// way of telling you whether the picture in front of you was already in the
/// set — and pressing it twice by mistake silently takes one back out.
///
/// Drawn on the picture rather than beside it, because there is no beside: a
/// plate behind each one so they stay legible over whatever is underneath.
fn presentation_badges(ui: &mut Ui, app: &GeoRefMapperApp, canvas: Rect) {
    let Some(path) = app.current.as_deref() else {
        return;
    };
    let folder = path
        .parent()
        .and_then(|folder| folder.file_name())
        .map(|name| name.to_string_lossy().into_owned());
    let marked = app
        .files
        .find_by_path(path)
        .is_some_and(|node| app.files.is_marked(&node));
    if folder.is_none() && !marked {
        return;
    }

    let painter = ui.painter().with_clip_rect(canvas);
    let font = theme::small_caps_font();
    let mut at = canvas.left_top() + vec2(12.0, 12.0);

    let mut badge = |text: &str, ink: Color32| {
        let galley = painter.layout_no_wrap(text.to_owned(), font.clone(), ink);
        let plate = Rect::from_min_size(at, galley.size() + vec2(14.0, 8.0));
        painter.rect_filled(plate, CornerRadius::same(4), Color32::from_black_alpha(150));
        painter.galley(plate.min + vec2(7.0, 4.0), galley, ink);
        at.y += plate.height() + 6.0;
    };

    if marked {
        badge("● MARKED", theme::MARKED);
    }
    if let Some(folder) = folder {
        badge(&folder, theme::TEXT);
    }
}

fn viewer(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let full = ui.available_rect_before_wrap();
    // The flag was being written by two different toggles and read by nobody,
    // so the strip was always drawn. Reserving no height for it is what makes
    // hiding it actually give the picture the space.
    // Presentation mode: the canvas takes everything.
    let bare = app.presenting;
    let strip_height = if app.config.show_filmstrip && !bare {
        84.0
    } else {
        0.0
    };
    let toolbar_height = if bare { 0.0 } else { 36.0 };
    // A header of its own, matching the Mosaic's. The filename was only ever
    // in the INFO panel, elided to fit 268 pt — so with that panel closed there
    // was nothing on screen saying which file you were looking at.
    let header_height = if bare {
        0.0
    } else {
        viewer_header_height(ui, app, full.width())
    };
    let header = Rect::from_min_size(full.min, vec2(full.width(), header_height));
    let canvas = Rect::from_min_max(
        pos2(full.left(), header.bottom()),
        pos2(full.right(), full.bottom() - strip_height - toolbar_height),
    );

    // Remembered so entering and leaving presentation mode can hold the zoom
    // steady across the change in canvas size.
    app.settle_anchor(ui.ctx(), canvas);
    if let Some(offset) = app.glide_offset(ui.ctx()) {
        app.viewport.offset = offset;
    }
    app.canvas_rect = Some(canvas);

    if !bare {
        viewer_header(ui, app, header);
    }

    // Canvas.
    ui.painter()
        .rect_filled(canvas, CornerRadius::ZERO, theme::CANVAS_BG);
    icons::checkerboard(
        ui.painter(),
        canvas,
        16.0,
        theme::CANVAS_BG,
        Color32::from_rgb(0x12, 0x13, 0x16),
    );

    let zoom_percent = image_canvas(ui, app, canvas);

    // Zoom / fit toolbar. Nothing below this belongs in presentation mode.
    if bare {
        presentation_badges(ui, app, canvas);
        ui.advance_cursor_after_rect(full);
        return;
    }
    let toolbar = Rect::from_min_size(
        pos2(full.left(), canvas.bottom()),
        vec2(full.width(), toolbar_height),
    );
    ui.painter()
        .rect_filled(toolbar, CornerRadius::ZERO, theme::PANEL_BG);
    ui.painter()
        .line_segment([toolbar.left_top(), toolbar.right_top()], theme::hairline());

    let zoom_label = zoom_percent
        .map(|percent| format!("{percent:.0}%"))
        .unwrap_or_else(|| "—".to_owned());
    let controls: [(&str, ZoomAction); 6] = [
        ("−", ZoomAction::Out),
        (zoom_label.as_str(), ZoomAction::None),
        ("+", ZoomAction::In),
        ("Fit", ZoomAction::Mode(ZoomMode::Fit)),
        ("Fill", ZoomAction::Mode(ZoomMode::Fill)),
        ("1:1", ZoomAction::Mode(ZoomMode::Actual)),
    ];

    let image_size = app
        .current
        .as_ref()
        .and_then(|path| app.textures.get(path))
        .map(|entry| vec2(entry.size[0] as f32, entry.size[1] as f32));

    let mut x = toolbar.left() + 12.0;
    for (label, action) in controls {
        let width = if label.chars().count() > 2 {
            52.0
        } else {
            30.0
        };
        let rect = Rect::from_min_size(pos2(x, toolbar.top() + 5.0), vec2(width, 26.0));
        x += width + 4.0;

        let interactive = action != ZoomAction::None && image_size.is_some();
        let response = ui.interact(
            rect,
            ui.id().with(("zoom", label)),
            if interactive {
                Sense::click()
            } else {
                Sense::hover()
            },
        );

        let active = matches!(action, ZoomAction::Mode(mode) if mode == app.viewport.mode);
        if active {
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(5),
                theme::ACCENT.gamma_multiply(0.25),
            );
        } else if response.hovered() && interactive {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(5), theme::SURFACE);
        }
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            theme::label_font(),
            if active || (response.hovered() && interactive) {
                theme::TEXT
            } else {
                theme::TEXT_MUTED
            },
        );

        if response.clicked()
            && let Some(image_size) = image_size
        {
            match action {
                ZoomAction::Mode(mode) => app.viewport.set_mode(mode),
                // Stepping from the toolbar zooms about the canvas centre.
                ZoomAction::In => app
                    .viewport
                    .zoom_at(canvas.center(), canvas, image_size, 1.25),
                ZoomAction::Out => app
                    .viewport
                    .zoom_at(canvas.center(), canvas, image_size, 0.8),
                ZoomAction::None => {}
            }
        }
    }
    // Right-aligned status, elided so it can never run into the zoom buttons.
    // No filename: the header above the picture carries it, and repeating it
    // here only pushed the numbers — the part this row is for — off the end.
    let status = match (&app.current, image_size) {
        (Some(_), Some(size)) => format!(
            "{} × {}  ·  {}",
            size.x as u32,
            size.y as u32,
            app.viewport.mode.label(),
        ),
        _ => app.files.label_for(&app.files.selected_folder()),
    };
    let available = toolbar.right() - 12.0 - x - 16.0;
    ui.painter().text(
        pos2(toolbar.right() - 12.0, toolbar.center().y),
        Align2::RIGHT_CENTER,
        widgets::elide(ui, &status, theme::small_caps_font(), available),
        theme::small_caps_font(),
        theme::TEXT_DIM,
    );

    // Filmstrip. Skipped entirely when it has no height: the tiles are a fixed
    // 60 pt and are centred on the strip, so drawing them into a zero-height
    // one would lay them across the picture.
    if !app.config.show_filmstrip || bare {
        ui.advance_cursor_after_rect(full);
        return;
    }
    let strip = Rect::from_min_size(
        pos2(full.left(), toolbar.bottom()),
        vec2(full.width(), strip_height),
    );
    ui.painter()
        .rect_filled(strip, CornerRadius::ZERO, theme::PANEL_BG);
    ui.painter()
        .line_segment([strip.left_top(), strip.right_top()], theme::hairline());

    // The wheel over the rail steps through pictures rather than scrolling it:
    // the strip already keeps the selection centred, so scrolling it would
    // fight itself. What the gesture means here is "show me the next one".
    //
    // One notch, one picture: the travel is banked until a full notch has
    // arrived, because a single physical click of the wheel is delivered over
    // several frames.
    const NOTCH: f32 = 40.0;
    let (raw_scroll, pointer, control, shift) = ui.input(|i| {
        (
            i.smooth_scroll_delta.y,
            i.pointer.hover_pos(),
            i.modifiers.command || i.modifiers.ctrl,
            i.modifiers.shift,
        )
    });
    if pointer.is_some_and(|p| strip.contains(p)) {
        app.rail_scroll += raw_scroll;
        while app.rail_scroll.abs() >= NOTCH {
            // Wheel *up* goes to the next picture. The rail runs left to right
            // and slides leftwards as you advance, so up-is-forward is what
            // matches the movement on screen.
            let forward = app.rail_scroll > 0.0;
            app.rail_scroll -= if forward { NOTCH } else { -NOTCH };
            app.step_image(if forward { 1 } else { -1 });
        }
    } else {
        // Travel does not carry over to the next time the cursor comes back.
        app.rail_scroll = 0.0;
    }

    // The same list the arrow keys walk, so the rail and the keyboard cannot
    // disagree about what comes next. With subfolders included it runs straight
    // on into the following folder rather than stopping at the end of this one.
    let waiting = placeholder_note_text(app);
    let order = app.viewer_images();
    // The picture currently laid over the map, matched by path.
    let on_the_map = app.map_overlay.as_ref().map(|overlay| overlay.path.clone());
    let on_the_map = on_the_map.as_deref();
    let rail_batch = app.files.action_paths();
    let rail_stack = Some(app.stack_menu(&rail_batch));
    let map_support = app.config.map_support;
    let mut rail_menu = crate::panels::MenuResult::default();
    // Owned so the loop below can write back to `app.files.selected`.
    let siblings: Vec<(
        crate::files::NodePath,
        String,
        Option<std::path::PathBuf>,
        Option<i64>,
    )> = order
        .iter()
        .filter_map(|path| Some((path.clone(), app.files.get(path)?)))
        .map(|(path, node)| {
            let (file, db_id) = match &node.kind {
                Kind::Image(meta) => (meta.path.as_ref().map(std::path::PathBuf::from), meta.db_id),
                Kind::Folder => (None, None),
            };
            (path, node.name.clone(), file, db_id)
        })
        .collect();
    if siblings.is_empty() {
        widgets::placeholder_note(ui, strip, "No images in this folder");
    } else {
        // Which folder the picture on screen is in. With subfolders included
        // the rail spans several, and without a mark it is impossible to tell
        // where one ends and the next begins.
        let here = app
            .current
            .as_deref()
            .and_then(|path| path.parent())
            .map(std::path::Path::to_path_buf);

        let thumb = vec2(88.0, 60.0);
        let step = thumb.x + 8.0;
        let total = siblings.len() as f32 * step;

        // The rail slides so the current picture sits in the middle, rather
        // than always starting at the first image and running off the end. Once
        // there are more pictures than fit, this is the only way the one you are
        // looking at stays on screen at all.
        let selected = siblings
            .iter()
            .position(|(path, ..)| *path == app.files.selected)
            .unwrap_or(0);
        let centred = strip.center().x - (selected as f32 + 0.5) * step;
        // Clamped at both ends so the strip is never padded with empty space
        // when there is enough to fill it.
        let first = strip.left() + 12.0;
        let last = strip.right() - 12.0 - total;
        let target = if total <= strip.width() - 24.0 {
            first
        } else {
            centred.clamp(last, first)
        };
        // Slide to the new position rather than snapping to it. Without this
        // the pictures simply swap places and there is nothing to tell you
        // which way you moved along the rail.
        let origin = ui
            .ctx()
            .animate_value_with_time(ui.id().with("rail_origin"), target, 0.16);

        // Everything below is clipped to the strip.
        //
        // The loop stops at the first tile whose *left* edge is past the right
        // end, so the one before it is drawn in full and hangs 88 pt over the
        // edge — across the INFO panel, on top of a panel it knows nothing
        // about. It was always wrong and always invisible at a comfortable
        // width, where the overhang landed in the gap; at 200 % the canvas is
        // barely three tiles across and the last one straddled the divider.
        //
        // Clipped rather than stopped short. A rail that scrolls should cut its
        // end tiles off — that is what says there are more — and stopping a
        // whole tile early would leave up to 88 pt of empty strip, which reads
        // as the rail failing to fill the width it was given.
        //
        // This also fixes the clicking: `Ui::interact` takes its hit rectangle
        // as `clip_rect ∩ rect`, so the hidden part of a tile stops answering
        // to a pointer that is really over the INFO panel.
        let unclipped = ui.clip_rect();
        ui.set_clip_rect(strip.intersect(unclipped));

        for (index, (path, _name, file, db_id)) in siblings.iter().enumerate() {
            let x = origin + index as f32 * step;
            if x + thumb.x < strip.left() {
                continue;
            }
            if x > strip.right() {
                break;
            }
            let rect = Rect::from_min_size(pos2(x, strip.center().y - thumb.y * 0.5), thumb);
            let response = ui.interact(rect, ui.id().with(("film", path)), Sense::click());
            if let Some(file) = file {
                crate::panels::file_context_menu(
                    ui,
                    &response,
                    file,
                    &rail_batch,
                    rail_stack.as_ref(),
                    db_id.is_some_and(|id| app.stacks.of(id).is_some()),
                    map_support,
                    &mut rail_menu,
                );
            }
            // Ctrl and shift pick a batch out of the rail, as they do in the
            // tree and the Mosaic.
            //
            // A plain click only moves to that picture and leaves the marked set
            // exactly as it was. The rail is what you scrub through a folder
            // with — the gesture is "show me this one", made dozens of times
            // while deciding — and having each of those quietly throw away a set
            // built up over a hundred pictures is the wrong answer to it.
            if response.clicked() {
                app.focus = crate::app::Focus::Images;
                if control {
                    app.files.toggle_mark(path);
                } else if shift {
                    app.files.mark_range(&order, path);
                } else {
                    app.files.anchor = Some(path.clone());
                    app.files.selected = path.clone();
                }
            }

            // Deep blue *behind* the picture — the letterbox margins around a
            // thumbnail are what carry it, so it marks the tile without
            // tinting the photograph.
            if on_the_map.is_some() && on_the_map == file.as_deref() {
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(4), theme::ON_THE_MAP);
            }

            // A stored thumbnail is the cheap path; a full decode already in
            // the texture cache also works; otherwise show a placeholder.
            let full = file
                .as_ref()
                .and_then(|file| app.textures.peek(file))
                .map(|entry| (entry.texture.id(), entry.size));
            let drawn = match full {
                Some((texture, size)) => {
                    draw_contained(ui, rect, texture, size);
                    true
                }
                None => draw_thumbnail(ui, app, rect, *db_id, false),
            };
            if !drawn {
                placeholder_tile(ui, rect, CornerRadius::same(4), waiting);
            }
            if db_id.is_some_and(|id| app.georeferenced.contains(&id)) {
                geo_badge(ui.painter(), rect);
            }

            // Blue for this folder, green for one either side of it. Drawn as
            // a bar under the picture rather than as the frame, so it does not
            // compete with the selection outline above it.
            if app.config.mosaic_recursive
                && let Some(here) = &here
            {
                let mine = file
                    .as_ref()
                    .and_then(|file| file.parent())
                    .is_some_and(|folder| folder == here);
                ui.painter().rect_filled(
                    Rect::from_min_max(
                        pos2(rect.left(), rect.bottom() + 3.0),
                        pos2(rect.right(), rect.bottom() + 6.0),
                    ),
                    CornerRadius::same(1),
                    if mine {
                        theme::ACCENT
                    } else {
                        theme::OTHER_FOLDER
                    },
                );
            }

            // A yellow wash plus the same badge the cover carries in the grid,
            // so a picture that belongs to a group says so wherever you meet
            // it. Drawn under the outlines, which still carry selection.
            if app.config.show_stacks
                && let Some(stack) = db_id.and_then(|id| app.stacks.of(id))
            {
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::same(4),
                    theme::STACKED.gamma_multiply(0.22),
                );
                stack_badge(ui, rect, app.stacks.len(stack));
            }

            let active = app.files.selected == *path;
            let marked = app.files.mark_of(path);
            if let Some(mark) = marked {
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::same(4),
                    mark_colour(mark).gamma_multiply(0.30),
                );
            }
            ui.painter().rect_stroke(
                rect,
                CornerRadius::same(4),
                Stroke::new(
                    if active || marked.is_some() { 2.0 } else { 1.0 },
                    match (active, marked, response.hovered()) {
                        (true, _, _) => theme::ACCENT,
                        (false, Some(mark), _) => mark_colour(mark),
                        (false, None, true) => theme::TEXT_MUTED,
                        (false, None, false) => theme::HAIRLINE,
                    },
                ),
                StrokeKind::Outside,
            );
        }

        // Put back before anything else is drawn into this `Ui`.
        ui.set_clip_rect(unclipped);
    }

    let now = ui.input(|i| i.time);
    app.apply_menu(rail_menu, &rail_batch, now);

    ui.advance_cursor_after_rect(full);
}

// ------------------------------------------------------------------------ Map

/// An OpenStreetMap slippy map: drag to pan, wheel to zoom.
fn map(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    use crate::map::{TILE, TileId, lat_to_tile, lon_to_tile};

    let mut full = ui.available_rect_before_wrap();

    // Placing a picture by hand splits the page: basemap on the left, picture
    // on the right, the same anchor marked on both. Side by side rather than
    // one after the other, because the whole task is comparing two things.
    if app.placing.is_some() {
        let (left, right) = split_for_placing(full);
        placing_half(ui, app, right);
        full = left;
    }
    // Remembered so the MAP tab can fit a picture to the window without being
    // able to see it.
    app.map_window = full.size();
    ui.painter()
        .rect_filled(full, CornerRadius::ZERO, theme::CANVAS_BG);

    // The tile source is created and polled inside a scope of its own: the pin
    // handling below needs the whole app, and holding a borrow of one field
    // across it would rule that out.
    {
        let contact = app.config.tile_contact.clone();
        let tiles = app.tiles.get_or_insert_with(|| {
            crate::map::Tiles::new(ui.ctx().clone(), crate::map::user_agent(&contact))
        });
        tiles.poll(ui.ctx());
    }

    // Every map covering the window, for the timeline and the IMAGES list.
    // Overlapping rather than contained: a sheet covering half of what you are
    // looking at is coverage of it.
    {
        let (north, west) = app
            .map_view
            .at_offset(vec2(-full.width() / 2.0, -full.height() / 2.0));
        let (south, east) = app
            .map_view
            .at_offset(vec2(full.width() / 2.0, full.height() / 2.0));
        app.maps_in_view = app
            .library
            .as_ref()
            .and_then(|library| library.maps_overlapping(west, south, east, north).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(year, path)| (year, std::path::PathBuf::from(path)))
            .collect();
    }
    // Where the strip stands, worked out here rather than while drawing it: it
    // is not drawn while a map is being placed, and a playhead frozen at
    // whatever it last was would freeze the fades with it.
    app.timeline_playhead = app.timeline_top + full.height() / 2.0 / app.timeline_scale;
    let rows = (full.height() / app.timeline_scale).ceil() as i32 + 1;
    let first = app.timeline_top.floor() as i32;
    app.timeline_span = (first, first + rows);

    // All of it belongs to the IMAGES tab. With the list out of sight there is
    // nothing on screen to say which map is which, so maps coming and going as
    // time scrolls would be movement with no explanation attached to it.
    //
    // The opacities are kept rather than cleared: coming back to the tab lands
    // on what was set, instead of everything being at nothing again.
    if app.images_tab_open() {
        app.fade_maps_to_timeline();
        app.settle_map_extras(ui.ctx());
    } else {
        app.map_extras.clear();
    }

    let response = ui.interact(full, ui.id().with("map_canvas"), Sense::click_and_drag());

    // A drag that starts on a pin moves the pin; anywhere else moves the map.
    // Decided once, when the drag starts, so a gesture cannot change its mind
    // half way through and leave a pin somewhere it was dragged past.
    // While a picture is being placed, the *shared* anchors are the reference —
    // the fixed truth the picture is matched against — and moving one silently
    // changes the answer for every other picture that uses it. So those are
    // frozen: a drag pans the map, and a click chooses one to copy across.
    //
    // An anchor made for this picture alone is a different matter. It was put
    // down a moment ago at the middle of the map and has to be walked to the
    // place it means, and nothing else in the library can be hurt by moving it.
    //
    // And outside placing, nothing moves at all unless the lock is off. A pin
    // is a small target sitting on a surface whose whole job is to be dragged,
    // so a gesture meant for the map lands on one often enough to matter — and
    // the damage is silent, since a moved anchor looks exactly like one that
    // was always there.
    let placing = app.placing.is_some();
    let mine = app.placing.as_ref().and_then(|session| session.image_id);
    let locked = app.config.anchors_locked || app.anchors_off_the_map();
    let movable = |app: &GeoRefMapperApp, id: i64| -> bool {
        mine.is_some()
            && app
                .anchors
                .iter()
                .any(|held| held.id == id && held.owner_image_id == mine)
    };
    let mut led = false;

    // The strip down the left is a control of its own, so a drag that starts on
    // it scrolls time instead of panning the map — the same claim it already
    // makes on the wheel, and for the same reason: a gesture aimed at the years
    // must not quietly move the ground under them.
    let timeline_up = app.timeline && app.placing.is_none() && app.images_tab_open();
    if response.drag_started()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        app.dragging_timeline = timeline_up && timeline_hot_zone(full).contains(pointer);
    }
    if response.drag_stopped() {
        app.dragging_timeline = false;
    }

    if response.drag_started()
        && !app.dragging_timeline
        && let Some(pointer) = response.interact_pointer_pos()
    {
        app.dragging_anchor = match anchor_under(app, full, pointer) {
            Some(id) if placing => movable(app, id).then_some(id),
            Some(id) if !locked => Some(id),
            _ => None,
        };
    }
    if placing
        && response.clicked()
        && let Some(pointer) = response.interact_pointer_pos()
        && let Some(id) = anchor_under(app, full, pointer)
        && let Some(session) = &mut app.placing
    {
        // The same thing choosing its name in the list does — the map is just
        // the other way in to the same choice.
        session.choosing = Some(id);
    }
    if response.drag_stopped()
        && let Some(id) = app.dragging_anchor.take()
    {
        app.finish_anchor_drag(id);
    }

    match (response.dragged(), app.dragging_anchor) {
        (true, _) if app.dragging_timeline => {
            // Pulled down brings earlier years into view, the same way the
            // wheel reads — the strip moves with the hand, not against it.
            app.timeline_top -= response.drag_delta().y / app.timeline_scale;
            ui.ctx()
                .set_cursor_icon(eframe::egui::CursorIcon::ResizeVertical);
        }
        (true, Some(id)) => {
            if let Some(pointer) = response.interact_pointer_pos() {
                let (lat, lon) = app.map_view.at_offset(pointer - full.center());
                app.drag_anchor_to(id, lat, lon);
            }
            ui.ctx().set_cursor_icon(eframe::egui::CursorIcon::Grabbing);
        }
        (true, None) => {
            app.map_view.pan(response.drag_delta());
            led = true;
            ui.ctx().set_cursor_icon(eframe::egui::CursorIcon::Grabbing);
        }
        _ if response.hovered() => {
            let over_pin = response
                .hover_pos()
                .and_then(|pointer| anchor_under(app, full, pointer))
                .is_some();
            // A locked pin gets no cursor of its own. Nothing about pointing at
            // one is a refusal worth announcing — it is still a thing to click,
            // and the lock is stated where it is set rather than under the
            // pointer every time it passes.
            let over_strip = response
                .hover_pos()
                .is_some_and(|pointer| timeline_up && timeline_hot_zone(full).contains(pointer));
            ui.ctx().set_cursor_icon(match (over_strip, over_pin) {
                (true, _) => eframe::egui::CursorIcon::ResizeVertical,
                (false, true) => eframe::egui::CursorIcon::PointingHand,
                (false, false) => eframe::egui::CursorIcon::Grab,
            });
        }
        _ => {}
    }

    // What a double-click on a sheet means depends on what you are doing with
    // the sheets.
    //
    // On the IMAGES tab there is a pile of them fading over the basemap, and
    // the thing wanted of any one is to get it out of the way and put it back —
    // the same act as double-clicking its name in the list. Anywhere else there
    // is one map laid over the tiles, and the thing wanted is the file it came
    // from: looking at the map is where you notice something about a sheet, and
    // otherwise the only way back to it is to remember its name and hunt.
    // The topmost sheet under the pointer, oldest drawn first so the last that
    // answers is the one being looked at.
    let sheet_at =
        |app: &GeoRefMapperApp, pointer: eframe::egui::Pos2| -> Option<std::path::PathBuf> {
            app.map_extras
                .iter()
                .map(|overlay| (overlay.path.clone(), overlay.geo.clone()))
                .chain(
                    app.map_overlay
                        .iter()
                        .map(|overlay| (overlay.path.clone(), overlay.geo.clone())),
                )
                .filter(|(_, geo)| {
                    overlay_footprint(&app.map_view, full, geo)
                        .is_some_and(|rect| rect.contains(pointer))
                })
                .next_back()
                .map(|(path, _)| path)
        };

    // One click on a sheet selects its file. Held back a moment in case a
    // second is coming — see `click_on_map`.
    if response.clicked()
        && app.images_tab_open()
        && let Some(pointer) = response.interact_pointer_pos()
        && let Some(path) = sheet_at(app, pointer)
    {
        let now = ui.input(|i| i.time);
        app.click_on_map(&path, now);
    }

    if response.double_clicked()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        if app.images_tab_open() {
            if let Some(path) = sheet_at(app, pointer) {
                app.toggle_map_mute(&path);
            }
        } else if let Some(overlay) = &app.map_overlay
            && let (Some((north, west)), Some((south, east))) =
                (overlay.geo.top_left(), overlay.geo.bottom_right())
        {
            let (lat, lon) = app.map_view.at_offset(pointer - full.center());
            // Its footprint, not the whole window: a double-click on open sea
            // is not a request for whatever happens to be overlaid elsewhere.
            let inside = lon >= west.min(east)
                && lon <= west.max(east)
                && lat >= south.min(north)
                && lat <= south.max(north);
            if inside {
                let path = overlay.path.clone();
                let now = ui.input(|i| i.time);
                app.reveal_in_tree(&path, now);
            }
        }
    }

    // Right-click puts a pin where the pointer is, which is the only place a
    // pin can be put exactly.
    let mut add_at: Option<(f64, f64)> = None;
    response.context_menu(|ui| {
        if ui.button("Add an anchor here").clicked() {
            if let Some(pointer) = ui.ctx().pointer_latest_pos() {
                add_at = Some(app.map_view.at_offset(pointer - full.center()));
            }
            ui.close();
        }
    });
    if let Some((lat, lon)) = add_at {
        app.add_anchor(lat, lon);
    }

    // Both the wheel and Ctrl+wheel zoom, as on the picture: there is nothing
    // else here to scroll.
    let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
    let by = if pinch != 1.0 {
        pinch.log2() as f64
    } else {
        f64::from(scroll) * 0.004
    };
    // The timeline claims the wheel when the pointer is over it, and the map
    // does not get it. Decided here, before the zoom, because the timeline is
    // drawn much later in the frame — acting on the same event in both places
    // meant scrolling through the years also zoomed the map underneath.
    let over_timeline = app.timeline
        && app.placing.is_none()
        && ui
            .ctx()
            .pointer_latest_pos()
            .is_some_and(|pointer| timeline_hot_zone(full).contains(pointer));

    if response.hovered()
        && by != 0.0
        && !over_timeline
        && let Some(pointer) = response.hover_pos()
    {
        app.map_view.zoom_at(pointer - full.center(), by);
        led = true;
    }

    // The basemap moved, so the picture beside it follows. Only when this half
    // was the one touched — see `map_follows_picture` for why the leader has to
    // be decided per gesture.
    if led {
        app.picture_follows_map();
    }

    // Which anchors this picture actually uses — the ones being placed now, or
    // the ones already saved for it. A library of marked places grows without
    // limit; the handful that answer *this* map are what matter while it is on
    // screen.
    let used = anchors_on_screen(app);

    // Which tiles cover the window. The centre sits at a fractional tile
    // position, so the grid is offset by whatever the fraction is.
    let view = app.map_view;
    let zoom = view.tile_zoom();
    let scale = view.scale();
    let size = TILE * scale;
    let centre_x = lon_to_tile(view.lon, zoom);
    let centre_y = lat_to_tile(view.lat, zoom);
    let across = (full.width() / size / 2.0).ceil() as i64 + 1;
    let down = (full.height() / size / 2.0).ceil() as i64 + 1;
    let world = 1_i64 << zoom;

    let painter = ui.painter().with_clip_rect(full);
    // Re-taken now that the pin handling is done with the app.
    let tiles = app
        .tiles
        .as_mut()
        .expect("the tile source was created above");
    let mut missing = 0;
    // One ring past the edge of the window, fetched at low priority. Panning is
    // continuous, so the ground just off screen is where the map is about to be
    // — asking for it a second early is the difference between a map that
    // slides and one that fills in behind you.
    const RING: i64 = 1;
    for dy in -down - RING..=down + RING {
        for dx in -across - RING..=across + RING {
            let tx = centre_x.floor() as i64 + dx;
            let ty = centre_y.floor() as i64 + dy;
            // Wraps east to west, as a globe does; but there is nothing above
            // the north edge or below the south.
            if ty < 0 || ty >= world {
                continue;
            }
            let id = TileId {
                zoom,
                x: tx.rem_euclid(world) as u32,
                y: ty as u32,
            };

            let left = full.center().x + (tx as f64 - centre_x) as f32 * size;
            let top = full.center().y + (ty as f64 - centre_y) as f32 * size;
            let rect = Rect::from_min_size(pos2(left, top), vec2(size, size));

            // Off screen: wanted soon, but not drawn and not urgent.
            if !rect.intersects(full) {
                tiles.request(id, crate::map::Want::Prefetch);
                continue;
            }

            let whole = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            match tiles.get(&id) {
                Some(texture) => {
                    painter.image(texture.id(), rect, whole, Color32::WHITE);
                }
                None => {
                    missing += 1;
                    tiles.request(id, crate::map::Want::Visible);
                    // A coarser tile covers the same ground. Stretching it is
                    // far better than a grey square: the map reads correctly at
                    // once and only sharpens afterwards.
                    match tiles.ancestor(&id) {
                        Some((texture, part)) => {
                            painter.image(texture.id(), rect, part, Color32::WHITE);
                        }
                        None => {
                            painter.rect_filled(rect, CornerRadius::ZERO, theme::SURFACE);
                        }
                    }
                }
            }
        }
    }

    prefetch_neighbouring_zooms(tiles, &view, full);
    // The disk holds everything; video memory holds what is near. Sized to a
    // few screenfuls so panning back and forth never re-uploads.
    tiles.trim((centre_x, centre_y), zoom, 600);

    // Under the picture and the pins: these are a reference, not the subject.
    if app.graticule {
        draw_graticule(&painter, full, &view, size, centre_x, centre_y);
    }

    let debug = app.tile_debug;
    // The extras first, oldest underneath, and the selected map last so it
    // stays the one on top — it is the one every other control acts on.
    if app.placing.is_none() {
        for overlay in &mut app.map_extras {
            draw_overlay(&painter, full, &view, Some(overlay), debug);
        }
    }
    draw_overlay(&painter, full, &view, app.map_overlay.as_mut(), debug);
    mark_maps_under_pointer(ui, app, &painter, full, &view);
    // Not while a map is being placed. The two halves are already tight, the
    // basemap is being clicked on directly, and a strip down the left of it is
    // both in the way of the pointer and answering a question nobody is asking
    // at that moment.
    if app.timeline && app.placing.is_none() && app.images_tab_open() {
        draw_timeline(ui, app, full);
    }
    // The found street, marked until the next search. Drawn before the anchors
    // so a pin sitting on it stays on top.
    if let Some(place) = app.found_street.clone() {
        let painter = ui.painter().with_clip_rect(full);
        let at = full.center() + {
            let (lat, lon) = (place.lat, place.lon);
            let zoom = app.map_view.tile_zoom();
            let size = f64::from(crate::map::TILE * app.map_view.scale());
            vec2(
                ((crate::map::lon_to_tile(lon, zoom)
                    - crate::map::lon_to_tile(app.map_view.lon, zoom))
                    * size) as f32,
                ((crate::map::lat_to_tile(lat, zoom)
                    - crate::map::lat_to_tile(app.map_view.lat, zoom))
                    * size) as f32,
            )
        };
        if full.expand(60.0).contains(at) {
            painter.circle_stroke(at, 14.0, eframe::egui::Stroke::new(2.0, theme::WARNING));
            painter.circle_filled(at, 3.0, theme::WARNING);
            let galley =
                painter.layout_no_wrap(place.name.clone(), theme::small_caps_font(), theme::TEXT);
            let plate = Rect::from_min_size(at + vec2(18.0, -9.0), galley.size() + vec2(10.0, 4.0));
            painter.rect_filled(plate, CornerRadius::same(4), Color32::from_black_alpha(190));
            painter.galley(plate.min + vec2(5.0, 2.0), galley, theme::TEXT);
        }
    }
    if app.placing.is_none() {
        street_search(ui, app, full);
    }
    let painter = ui.painter().with_clip_rect(full);
    draw_anchors(
        &painter,
        full,
        &view,
        &app.anchors,
        app.dragging_anchor,
        app.placing.as_ref().and_then(|session| session.choosing),
        used.as_ref(),
    );

    let tiles = app
        .tiles
        .as_ref()
        .expect("the tile source was just created");
    let status = crate::map::Status {
        pending: tiles.pending(),
        queued: tiles.queued(),
        resident: tiles.resident(),
        refused: tiles.refused,
    };
    map_overlay(ui, full, &view, missing, status);
    ui.advance_cursor_after_rect(full);
}

/// Splits the page for placing: basemap left, picture right, with a gap.
fn split_for_placing(full: Rect) -> (Rect, Rect) {
    let middle = full.center().x;
    (
        Rect::from_min_max(full.min, pos2(middle - 3.0, full.max.y)),
        Rect::from_min_max(pos2(middle + 3.0, full.min.y), full.max),
    )
}

/// The picture half while placing: click to put the chosen anchor down.
fn placing_half(ui: &mut Ui, app: &mut GeoRefMapperApp, area: Rect) {
    ui.painter()
        .rect_filled(area, CornerRadius::ZERO, theme::CANVAS_BG);

    // How far the scan is turned from north, solved from *every* anchor placed
    // so far — not the first two. Worked out before the session is borrowed,
    // because solving reads the whole app.
    //
    // Not every sheet is drawn north up, and one that is not looks nothing like
    // the basemap beside it until it is turned. Zero until two anchors are
    // down, which is the first moment there is an angle to know.
    let turn = if app.placing.as_ref().is_some_and(|session| session.upright) {
        app.placing_placement()
            .map(|placement| placement.rotation as f32)
            .unwrap_or(0.0)
    } else {
        0.0
    };

    let Some(session) = &mut app.placing else {
        return;
    };
    let (width, height) = (session.size.0 as f32, session.size.1 as f32);
    if width <= 0.0 {
        return;
    }

    // The picture is drawn turned about its own middle, so where a pixel lands
    // is no longer a matter of adding an offset.
    //
    // The screen's y runs down and the earth's runs up, so the turn appears on
    // screen as its own negative — which is why the matrix below is not the one
    // in `anchor::apply` but its transpose.
    let to_screen = move |pixel: (f64, f64), zoom: f32, offset: eframe::egui::Vec2| {
        let (sin, cos) = turn.sin_cos();
        let (dx, dy) = (pixel.0 as f32 - width / 2.0, pixel.1 as f32 - height / 2.0);
        area.center() + offset + vec2(dx * cos + dy * sin, -dx * sin + dy * cos) * zoom
    };
    // And the way back, for turning a click into a pixel.
    let to_pixel = move |at: eframe::egui::Pos2, zoom: f32, offset: eframe::egui::Vec2| {
        let (sin, cos) = turn.sin_cos();
        let v = (at - area.center() - offset) / zoom;
        (
            f64::from(v.x * cos - v.y * sin + width / 2.0),
            f64::from(v.x * sin + v.y * cos + height / 2.0),
        )
    };

    // First draw: fit the picture to its half.
    if session.zoom <= 0.0 {
        session.zoom = (area.width() / width).min(area.height() / height);
        session.offset = eframe::egui::Vec2::ZERO;
    }

    let response = ui.interact(
        area,
        ui.id().with("placing_canvas"),
        Sense::click_and_drag(),
    );
    // A drag that starts on a mark moves that mark; anywhere else moves the
    // picture. Decided once, when the drag starts.
    if response.drag_started()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        session.dragging_mark = session
            .placed
            .iter()
            .find(|(_, pixel)| {
                to_screen(**pixel, session.zoom, session.offset).distance(pointer) <= 11.0
            })
            .map(|(id, _)| *id);
    }
    if response.drag_stopped() {
        session.dragging_mark = None;
    }

    // Dragging moves the picture, or the mark it started on. A click puts an
    // anchor down, so the two never fight over the same gesture.
    let mut led = false;
    if let (true, Some(id)) = (response.dragged(), session.dragging_mark) {
        if let Some(pointer) = response.interact_pointer_pos() {
            let pixel = to_pixel(pointer, session.zoom, session.offset);
            if pixel.0 >= 0.0
                && pixel.1 >= 0.0
                && pixel.0 <= f64::from(width)
                && pixel.1 <= f64::from(height)
            {
                session.placed.insert(id, pixel);
            }
        }
        ui.ctx().set_cursor_icon(eframe::egui::CursorIcon::Grabbing);
    } else if response.dragged() {
        session.offset += response.drag_delta();
        led = true;
    }
    let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
    if response.hovered() {
        let by = if pinch != 1.0 {
            pinch
        } else {
            1.0 + scroll * 0.002
        };
        if by != 1.0
            && let Some(pointer) = response.hover_pos()
        {
            // Zoom about the pointer, so what is under it stays under it.
            let before = (pointer - area.center() - session.offset) / session.zoom;
            session.zoom = (session.zoom * by).clamp(0.01, 40.0);
            session.offset = pointer - area.center() - before * session.zoom;
            led = true;
        }
    }

    // This half moved, so the other one follows it. Done here rather than once
    // for both halves because whichever was touched has to be the leader —
    // syncing in both directions in the same frame is how two views end up
    // fighting each other.
    if led {
        app.map_follows_picture();
    }
    let Some(session) = &mut app.placing else {
        return;
    };

    let painter = ui.painter().with_clip_rect(area);
    let (zoom, offset) = (session.zoom, session.offset);
    // Each square goes down as a four-cornered patch rather than a rectangle,
    // which is what lets the picture be turned. The same path the basemap uses
    // to lay a hand-placed map over the tiles.
    crate::imagetiles::draw_mapped(
        &mut session.squares,
        &painter,
        |pixel| to_screen(pixel, zoom, offset),
        zoom,
        area,
        Color32::WHITE,
        false,
    );

    // A click puts the chosen anchor where the pointer is.
    if response.clicked()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let pixel = to_pixel(pointer, session.zoom, session.offset);
        let inside = pixel.0 >= 0.0
            && pixel.1 >= 0.0
            && pixel.0 <= f64::from(width)
            && pixel.1 <= f64::from(height);
        if inside && let Some(anchor) = session.choosing {
            session.placed.insert(anchor, pixel);
            // Put down, so it stops being the one waiting to be put down. Left
            // set, the next click anywhere on the picture would pick it up and
            // move it — which reads as the map losing an anchor rather than as
            // a mode nobody knew they were in. Moving one afterwards is a drag
            // on the mark itself.
            session.choosing = None;
        }
    }

    let chosen = session.choosing;
    let zoom = session.zoom;
    let placed: Vec<(i64, (f64, f64))> = session
        .placed
        .iter()
        .map(|(id, pixel)| (*id, *pixel))
        .collect();

    // Every anchor already down, each in its own colour — the same colour it
    // has on the basemap, which is what makes a pair readable at a glance.
    let order: Vec<i64> = app.anchors.iter().map(|anchor| anchor.id).collect();
    for (id, pixel) in placed {
        let index = order.iter().position(|held| *held == id).unwrap_or(0);
        let at = to_screen(pixel, zoom, offset);
        if area.contains(at) {
            marker(
                &painter,
                at,
                theme::anchor_colour(index),
                chosen == Some(id),
            );
        }
    }

    hint_line(
        ui,
        area,
        match chosen {
            Some(_) => "Click the same place on the picture",
            None => "Choose an anchor in the panel, then click it here",
        },
    );

    // Done and Cancel on the canvas itself, not only in the panel. The panel
    // can be hidden or on another tab, and there has to be a way out of a mode
    // from inside it.
    let down = app
        .placing
        .as_ref()
        .map(|session| session.placed.len())
        .unwrap_or(0);
    let bar = Rect::from_min_size(
        pos2(area.right() - 206.0, area.top() + 10.0),
        vec2(196.0, 28.0),
    );
    ui.painter()
        .rect_filled(bar, CornerRadius::same(5), Color32::from_black_alpha(200));
    ui.scope_builder(
        eframe::egui::UiBuilder::new().max_rect(bar.shrink(4.0)),
        |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if widgets::button(ui, "Done", 88.0, down >= 2)
                    .on_disabled_hover_text("At least two anchors have to be on the picture")
                    .clicked()
                {
                    let ctx = ui.ctx().clone();
                    app.finish_placing(&ctx);
                }
                if widgets::button(ui, "Cancel", 88.0, true)
                    .on_hover_text("Leave without keeping any of it — Escape does the same")
                    .clicked()
                {
                    app.cancel_placing();
                }
            });
        },
    );
}

/// A cross with a ring: precise to click against, and it does not hide what is
/// underneath the way a filled blob does.
fn marker(painter: &eframe::egui::Painter, at: eframe::egui::Pos2, colour: Color32, chosen: bool) {
    let arm = if chosen { 11.0 } else { 8.0 };
    let stroke = eframe::egui::Stroke::new(if chosen { 2.0 } else { 1.5 }, colour);
    painter.line_segment([at - vec2(arm, 0.0), at + vec2(arm, 0.0)], stroke);
    painter.line_segment([at - vec2(0.0, arm), at + vec2(0.0, arm)], stroke);
    painter.circle_stroke(at, arm * 0.55, stroke);
    if chosen {
        painter.circle_stroke(
            at,
            arm + 4.0,
            eframe::egui::Stroke::new(1.0, colour.gamma_multiply(0.5)),
        );
    }
}

/// A line of guidance along the bottom of a half.
fn hint_line(ui: &Ui, area: Rect, text: &str) {
    let font = theme::small_caps_font();
    let galley = ui
        .ctx()
        .fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font, theme::TEXT));
    let plate = Rect::from_min_size(
        pos2(
            area.center().x - galley.size().x / 2.0 - 8.0,
            area.bottom() - 30.0,
        ),
        galley.size() + vec2(16.0, 8.0),
    );
    ui.painter()
        .rect_filled(plate, CornerRadius::same(4), Color32::from_black_alpha(190));
    ui.painter()
        .galley(plate.min + vec2(8.0, 4.0), galley, theme::TEXT);
}

/// Draws a map laid down by hand, turn and all.
///
/// Every square's corners go image pixel → place on the earth → screen, through
/// exactly the transforms the basemap uses. A turned map therefore lands turned,
/// which the corner-and-scale form could never show.
fn draw_placed_overlay(
    painter: &eframe::egui::Painter,
    full: Rect,
    view: &crate::map::View,
    overlay: &mut crate::map::Overlay,
    placement: crate::anchor::Placement,
    debug: bool,
) {
    use crate::map::{TILE, lat_to_tile, lon_to_tile};

    let zoom = view.tile_zoom();
    let size = TILE * view.scale();
    let centre = (lon_to_tile(view.lon, zoom), lat_to_tile(view.lat, zoom));

    let project = |pixel: (f64, f64)| {
        let (x, y) = placement.apply(pixel);
        let (lat, lon) = crate::map::from_mercator(x, y);
        pos2(
            full.center().x + (lon_to_tile(lon, zoom) - centre.0) as f32 * size,
            full.center().y + (lat_to_tile(lat, zoom) - centre.1) as f32 * size,
        )
    };

    // Screen pixels per full-size image pixel, taken along the picture's own
    // width so a turn does not make it look smaller than it is.
    let across = project((f64::from(overlay.size.0), 0.0)) - project((0.0, 0.0));
    let scale = across.length() / overlay.size.0.max(1) as f32;

    let tint = Color32::from_white_alpha((overlay.opacity.clamp(0.0, 1.0) * 255.0) as u8);
    crate::imagetiles::draw_mapped(
        &mut overlay.squares,
        painter,
        project,
        scale,
        full,
        tint,
        debug,
    );

    // The outline, so the edge is findable when the picture is faint — and so a
    // wrong placement is obvious as a shape in the wrong spot.
    let corners = [
        project((0.0, 0.0)),
        project((f64::from(overlay.size.0), 0.0)),
        project((f64::from(overlay.size.0), f64::from(overlay.size.1))),
        project((0.0, f64::from(overlay.size.1))),
    ];
    for index in 0..4 {
        painter.line_segment(
            [corners[index], corners[(index + 1) % 4]],
            eframe::egui::Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.7)),
        );
    }
}

/// How close the pointer has to be to a pin to take hold of it.
const PIN_GRAB: f32 = 14.0;

/// Where a pin sits on screen.
fn anchor_position(
    full: Rect,
    view: &crate::map::View,
    pin: &crate::db::Anchor,
) -> eframe::egui::Pos2 {
    use crate::map::{TILE, lat_to_tile, lon_to_tile};

    let zoom = view.tile_zoom();
    let size = TILE * view.scale();
    pos2(
        full.center().x + (lon_to_tile(pin.lon, zoom) - lon_to_tile(view.lon, zoom)) as f32 * size,
        full.center().y + (lat_to_tile(pin.lat, zoom) - lat_to_tile(view.lat, zoom)) as f32 * size,
    )
}

/// The pin under the pointer, nearest first — so overlapping pins pick the one
/// actually aimed at rather than whichever happens to be drawn last.
/// Which anchors are on screen, or `None` for all of them.
///
/// Asked once and used for both the drawing and the grabbing. They used to be
/// worked out only for the drawing, so a hidden pin was still under the
/// pointer: you could take hold of something invisible and drag it, and the
/// only sign was the placement quietly changing.
fn anchors_on_screen(app: &GeoRefMapperApp) -> Option<std::collections::HashSet<i64>> {
    if app.anchors_off_the_map() {
        return Some(std::collections::HashSet::new());
    }
    match app.anchors_shown {
        crate::app::AnchorsShown::All => None,
        crate::app::AnchorsShown::Used => Some(match &app.placing {
            Some(session) => session.placed.keys().copied().collect(),
            None => app.placed_anchors.iter().map(|held| held.id).collect(),
        }),
        // An empty set matches nothing, which is the whole point.
        crate::app::AnchorsShown::Hidden => Some(std::collections::HashSet::new()),
    }
}

fn anchor_under(app: &GeoRefMapperApp, full: Rect, pointer: eframe::egui::Pos2) -> Option<i64> {
    let shown = anchors_on_screen(app);
    let mut best: Option<(f32, i64)> = None;
    for pin in &app.anchors {
        if shown.as_ref().is_some_and(|shown| !shown.contains(&pin.id)) {
            continue;
        }
        let at = anchor_position(full, &app.map_view, pin);
        // The point of the pin is at `at`; the body sits above it.
        let distance = (at - vec2(0.0, 8.0)).distance(pointer);
        if distance <= PIN_GRAB && best.is_none_or(|(held, _)| distance < held) {
            best = Some((distance, pin.id));
        }
    }
    best.map(|(_, id)| id)
}

/// Width of the timeline strip down the left of the map.
const TIMELINE_WIDTH: f32 = 78.0;

/// The span the timeline covers, fixed rather than fitted to what is in view.
///
/// From before the earliest sheet in the collection to a little past the
/// latest, so the scale means the same thing wherever the map is panned and a
/// stretch with nothing in it reads as "nothing was made here" rather than as
/// the timeline having stopped.
const TIMELINE_FIRST: f32 = 1500.0;
const TIMELINE_LAST: f32 = 2030.0;

/// Where the wheel belongs to the timeline rather than to the map.
///
/// Wider than the strip itself: the years are small targets, and a gesture
/// aimed at them that lands just outside should not silently zoom the map
/// instead. One definition, used both to claim the event and to draw.
fn timeline_hot_zone(full: Rect) -> Rect {
    Rect::from_min_size(full.left_top(), vec2(TIMELINE_WIDTH, full.height()))
        .expand2(vec2(28.0, 0.0))
}

/// A scrollable century down the left of the map, saying what covers the view.
///
/// The question it answers is "what have I got for *here*, and when" — which
/// neither the tree nor the Mosaic can answer, because both are organised by
/// where a file sits rather than by where the map it holds points.
///
/// Counts are of maps **overlapping** the window, not contained by it: a sheet
/// covering half of what you are looking at is coverage of it, and requiring
/// containment would hide exactly the large-area maps most worth knowing about.
///
/// The wheel moves it when the pointer is over or near it, so scrolling time
/// never fights zooming the map.
fn draw_timeline(ui: &mut Ui, app: &mut GeoRefMapperApp, full: Rect) {
    let strip = Rect::from_min_size(full.left_top(), vec2(TIMELINE_WIDTH, full.height()));
    let hot = timeline_hot_zone(full);

    // Which maps cover what is on screen. Worked out once by the page and
    // shared, so the strip and the IMAGES list cannot disagree about what is
    // out there.
    let mut per_year: std::collections::BTreeMap<i32, usize> = std::collections::BTreeMap::new();
    for (year, _) in &app.maps_in_view {
        if let Some(year) = year {
            *per_year.entry(*year).or_default() += 1;
        }
    }

    // The wheel scrolls time only when the pointer is near the strip; anywhere
    // else it belongs to the map's zoom.
    //
    // Held down, Ctrl stretches the strip instead of scrolling it. That is the
    // control for how smooth the fades are: the fade covers a fixed distance in
    // points, so stretching the years apart puts fewer of them inside it and
    // the maps come one at a time rather than in a heap.
    if let Some(pointer) = ui.ctx().pointer_latest_pos()
        && hot.contains(pointer)
    {
        let (scroll, zoom, ctrl) = ui.input(|i| {
            (
                i.smooth_scroll_delta.y,
                i.zoom_delta(),
                i.modifiers.command || i.modifiers.ctrl,
            )
        });
        // egui reports Ctrl+wheel as a zoom rather than a scroll, so both
        // spellings are read and whichever arrives is used.
        let factor = if zoom != 1.0 {
            zoom
        } else if ctrl && scroll != 0.0 {
            (scroll * 0.004).exp()
        } else {
            1.0
        };
        if factor != 1.0 {
            // About the pointer, so the year being looked at stays under it.
            let offset = pointer.y - strip.top();
            let under = app.timeline_top + offset / app.timeline_scale;
            app.timeline_scale = crate::timeline::zoomed(app.timeline_scale, factor);
            app.timeline_top = crate::timeline::top_after_zoom(under, offset, app.timeline_scale);
        } else if scroll != 0.0 {
            app.timeline_top -= scroll * crate::timeline::WHEEL_DAMPING / app.timeline_scale;
        }
    }
    // A fixed span rather than one derived from what happens to be in view.
    //
    // Deriving it meant the scale changed as you panned — the same wheel
    // movement covered a different stretch of time from one moment to the next,
    // and an empty view had no timeline at all. A century and a half is the
    // whole period these maps come from, so the strip means the same thing
    // wherever you are and an empty stretch is information rather than absence.
    let visible = full.height() / app.timeline_scale;
    app.timeline_top = app.timeline_top.clamp(
        TIMELINE_FIRST,
        (TIMELINE_LAST - visible).max(TIMELINE_FIRST),
    );

    let painter = ui.painter().with_clip_rect(strip);
    painter.rect_filled(strip, CornerRadius::ZERO, Color32::from_black_alpha(150));
    painter.line_segment(
        [strip.right_top(), strip.right_bottom()],
        eframe::egui::Stroke::new(1.0, theme::HAIRLINE),
    );

    let first = app.timeline_top.floor() as i32;
    let rows = (full.height() / app.timeline_scale).ceil() as i32 + 1;

    // How often a year is written. Squeezed together the labels would sit on
    // top of one another and the strip would read as a grey smear.
    let every = crate::timeline::label_every(app.timeline_scale);
    for step in 0..rows {
        let year = first + step;
        if (year as f32) < TIMELINE_FIRST || (year as f32) > TIMELINE_LAST {
            continue;
        }
        let y = strip.top() + (year as f32 - app.timeline_top) * app.timeline_scale;
        if y < strip.top() - app.timeline_scale || y > strip.bottom() {
            continue;
        }
        // Every fifth one of whatever is being written is the one the eye
        // counts by, so it gets the ink.
        let labelled = (year as i64).rem_euclid(i64::from(every)) == 0;
        if labelled {
            let counted = (year as i64).rem_euclid(i64::from(every) * 5) == 0;
            painter.text(
                pos2(strip.right() - 6.0, y),
                Align2::RIGHT_CENTER,
                year.to_string(),
                theme::small_caps_font(),
                if counted {
                    theme::TEXT
                } else {
                    theme::TEXT_DIM
                },
            );
        }
        // Counts follow the labels: with the strip squeezed, one number a year
        // would smear the same way the years do, so they are added up over the
        // stretch each written year stands for.
        let count: usize = if every == 1 {
            per_year.get(&year).copied().unwrap_or(0)
        } else if labelled {
            (year..year + every).filter_map(|y| per_year.get(&y)).sum()
        } else {
            0
        };
        if count > 0 {
            painter.text(
                pos2(strip.left() + 6.0, y),
                Align2::LEFT_CENTER,
                count.to_string(),
                theme::small_caps_font(),
                theme::GEOREFERENCED,
            );
        }
    }

    // The playhead. Drawn after the years so it sits over them, and only when
    // it is doing something — an inert line across the strip would be a control
    // that looks live and is not.
    if app.timeline_fades {
        let y = strip.top() + full.height() / 2.0;
        painter.line_segment(
            [pos2(strip.left(), y), pos2(strip.right(), y)],
            eframe::egui::Stroke::new(1.0, theme::ACCENT),
        );
        // The stretch of years currently faded up, so the width of the fade is
        // visible rather than something to be inferred from the maps.
        let window = crate::timeline::window_years(app.timeline_scale) * app.timeline_scale;
        painter.rect_filled(
            Rect::from_min_max(
                pos2(strip.left(), y - window),
                pos2(strip.left() + 3.0, y + window),
            ),
            CornerRadius::same(1),
            theme::ACCENT.gamma_multiply(0.5),
        );
    }

    // How much there is altogether for this view, so an empty strip is
    // distinguishable from one scrolled past everything.
    let total: usize = per_year.values().sum();
    painter.text(
        pos2(strip.left() + 6.0, strip.bottom() - 10.0),
        Align2::LEFT_CENTER,
        &match total {
            0 => "no maps here".to_owned(),
            1 => "1 map here".to_owned(),
            many => format!("{many} maps here"),
        },
        theme::small_caps_font(),
        theme::TEXT_MUTED,
    );
}

/// The street search: a small button that becomes a field when pressed.
///
/// Shut until asked for. The map is a picture, and a search box sitting open
/// across the top of it takes room from the thing being looked at for a feature
/// used occasionally.
///
/// Nothing is asked of the network until the button is pressed — no
/// search-as-you-type. The service is donated, and a request per keystroke is
/// the way to be refused by it.
fn street_search(ui: &mut Ui, app: &mut GeoRefMapperApp, full: Rect) {
    const WIDTH: f32 = 260.0;
    let shut = app.street_search.is_none();
    let box_rect = Rect::from_min_size(
        pos2(
            full.right() - 14.0 - if shut { 26.0 } else { WIDTH },
            full.top() + 14.0,
        ),
        vec2(if shut { 26.0 } else { WIDTH }, 26.0),
    );

    if shut {
        let response = ui.interact(box_rect, ui.id().with("street_open"), Sense::click());
        let painter = ui.painter();
        painter.rect_filled(
            box_rect,
            CornerRadius::same(13),
            if response.hovered() {
                theme::SURFACE_HOVER
            } else {
                Color32::from_black_alpha(170)
            },
        );
        painter.text(
            box_rect.center(),
            Align2::CENTER_CENTER,
            "?",
            theme::label_font(),
            theme::TEXT,
        );
        if response.on_hover_text("Find a street by name").clicked() {
            app.street_search = Some(crate::streets::Search::default());
        }
        return;
    }

    let mut close = false;
    let mut go: Option<String> = None;
    let mut chosen: Option<crate::streets::Place> = None;

    if let Some(search) = &mut app.street_search {
        search.poll();
        let mut area = Rect::from_min_size(box_rect.min, vec2(WIDTH, 26.0));
        ui.painter().rect_filled(
            area.expand(4.0),
            CornerRadius::same(6),
            Color32::from_black_alpha(190),
        );

        ui.scope_builder(
            eframe::egui::UiBuilder::new().max_rect(area).layout(
                eframe::egui::Layout::left_to_right(eframe::egui::Align::Center),
            ),
            |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let field = ui.add(
                    eframe::egui::TextEdit::singleline(&mut search.query)
                        .hint_text("street name")
                        .desired_width(WIDTH - 78.0),
                );
                // Enter searches, Escape shuts it — the two keys anyone tries.
                if field.lost_focus() && ui.input(|i| i.key_pressed(eframe::egui::Key::Enter)) {
                    go = Some(search.query.clone());
                }
                if ui.input(|i| i.key_pressed(eframe::egui::Key::Escape)) {
                    close = true;
                }
                if widgets::small_button(ui, "Find", 40.0, !search.query.trim().is_empty())
                    .clicked()
                {
                    go = Some(search.query.clone());
                }
                if widgets::small_button(ui, "\u{d7}", 22.0, true).clicked() {
                    close = true;
                }
            },
        );

        // The answers, under the field.
        let rows = search.results.len();
        if search.running || search.error.is_some() || rows > 0 {
            area = Rect::from_min_size(
                pos2(area.left(), area.bottom() + 8.0),
                vec2(WIDTH, (rows.max(1) as f32 * 22.0 + 8.0).min(200.0)),
            );
            ui.painter()
                .rect_filled(area, CornerRadius::same(6), Color32::from_black_alpha(200));
            if search.running {
                ui.painter().text(
                    area.left_top() + vec2(8.0, 12.0),
                    Align2::LEFT_CENTER,
                    "searching…",
                    theme::small_caps_font(),
                    theme::TEXT_MUTED,
                );
            } else if let Some(why) = &search.error {
                ui.painter().text(
                    area.left_top() + vec2(8.0, 12.0),
                    Align2::LEFT_CENTER,
                    widgets::elide(ui, why, theme::small_caps_font(), WIDTH - 16.0),
                    theme::small_caps_font(),
                    theme::WARNING,
                );
            } else if rows == 0 {
                ui.painter().text(
                    area.left_top() + vec2(8.0, 12.0),
                    Align2::LEFT_CENTER,
                    "nothing found",
                    theme::small_caps_font(),
                    theme::TEXT_DIM,
                );
            }
            for (index, place) in search.results.iter().enumerate() {
                let row = Rect::from_min_size(
                    pos2(area.left() + 4.0, area.top() + 4.0 + index as f32 * 22.0),
                    vec2(WIDTH - 8.0, 22.0),
                );
                let response = ui.interact(row, ui.id().with(("street", index)), Sense::click());
                if response.hovered() {
                    ui.painter()
                        .rect_filled(row, CornerRadius::same(4), theme::SURFACE_HOVER);
                    ui.ctx()
                        .set_cursor_icon(eframe::egui::CursorIcon::PointingHand);
                }
                ui.painter().text(
                    pos2(row.left() + 6.0, row.center().y),
                    Align2::LEFT_CENTER,
                    widgets::elide(
                        ui,
                        &place.name,
                        theme::small_caps_font(),
                        row.width() - 12.0,
                    ),
                    theme::small_caps_font(),
                    theme::TEXT,
                );
                if response.clicked() {
                    chosen = Some(place.clone());
                }
            }
        }
    }

    if let Some(query) = go {
        let view = (
            app.map_view.at_offset(vec2(-full.width() / 2.0, 0.0)).1,
            app.map_view.at_offset(vec2(0.0, full.height() / 2.0)).0,
            app.map_view.at_offset(vec2(full.width() / 2.0, 0.0)).1,
            app.map_view.at_offset(vec2(0.0, -full.height() / 2.0)).0,
        );
        let agent = crate::map::user_agent(&app.config.tile_contact);
        if let Some(search) = &mut app.street_search {
            search.start(&query, view, agent);
        }
    }
    if let Some(place) = chosen {
        // Its extent when it has one: a street is a line, and centring on the
        // middle of a long one at the wrong zoom shows a stretch of nothing.
        match place.bounds {
            Some((west, south, east, north)) => {
                app.map_view.fit((north, west), (south, east), full.size());
            }
            None => app.map_view.centre_on(place.lat, place.lon),
        }
        app.found_street = Some(place);
    }
    if close {
        app.street_search = None;
    }
}

fn draw_anchors(
    painter: &eframe::egui::Painter,
    full: Rect,
    view: &crate::map::View,
    anchors: &[crate::db::Anchor],
    dragging: Option<i64>,
    chosen: Option<i64>,
    // When set, only these are drawn — the ones this picture actually uses.
    //
    // The colour still comes from the anchor's place in the full list, so
    // hiding the rest never re-colours the ones that stay. The pairing between
    // a pin here and a mark on the picture is the only thing saying which
    // answers which, and it has to survive the list being filtered.
    only: Option<&std::collections::HashSet<i64>>,
) {
    for (index, pin) in anchors.iter().enumerate() {
        // The one being placed stays visible whatever the filter says: it is
        // the one being looked for, and hiding it would be hiding the answer to
        // the question just asked.
        if only.is_some_and(|used| !used.contains(&pin.id)) && chosen != Some(pin.id) {
            continue;
        }
        let at = anchor_position(full, view, pin);
        if !full.expand(40.0).contains(at) {
            continue;
        }
        let held = dragging == Some(pin.id);
        // Its own colour, the same one it wears on the picture — that pairing
        // is the only thing saying which mark answers which.
        let colour = theme::anchor_colour(index);

        // A teardrop: a circle with a point at the bottom, so the place it
        // marks is the tip rather than the middle of a blob.
        let centre = at - vec2(0.0, 8.0);
        painter.add(eframe::egui::Shape::convex_polygon(
            vec![at, centre + vec2(-5.0, 2.0), centre + vec2(5.0, 2.0)],
            colour,
            eframe::egui::Stroke::NONE,
        ));
        painter.circle_filled(centre, 7.0, colour);
        painter.circle_filled(centre, 3.0, Color32::from_black_alpha(140));
        if held || chosen == Some(pin.id) {
            painter.circle_stroke(centre, 11.0, eframe::egui::Stroke::new(2.0, colour));
        }

        if !pin.name.is_empty() {
            let font = theme::small_caps_font();
            let galley = painter.layout_no_wrap(pin.name.clone(), font.clone(), theme::TEXT);
            let plate = Rect::from_min_size(
                pos2(centre.x + 11.0, centre.y - galley.size().y / 2.0 - 2.0),
                galley.size() + vec2(8.0, 4.0),
            );
            painter.rect_filled(plate, CornerRadius::same(3), Color32::from_black_alpha(180));
            painter.galley(plate.min + vec2(4.0, 2.0), galley, theme::TEXT);
        }
    }
}

/// Lays a georeferenced picture over the map, in its real place.
///
/// No reprojection: the pictures worth doing this to are Web Mercator, which is
/// what the tiles are, so the corners go through exactly the same transform the
/// tile grid uses and the two agree by construction.
/// Meridians and parallels over the basemap, labelled in degrees and minutes.
///
/// Drawn from the view rather than from the tiles: the lines are wanted at
/// round positions on the earth, and the tile grid is at round positions in the
/// projection, which are not the same thing at any zoom.
///
/// Both families are straight in Web Mercator — that is the property the
/// projection is chosen for — so each is one segment across the window rather
/// than a curve to walk along.
fn draw_graticule(
    painter: &eframe::egui::Painter,
    full: Rect,
    view: &crate::map::View,
    size: f32,
    centre_x: f64,
    centre_y: f64,
) {
    use crate::map::{dms, graticule_step, lat_to_tile, lon_to_tile};

    let zoom = view.tile_zoom();
    let west = view.at_offset(vec2(-full.width() / 2.0, 0.0)).1;
    let east = view.at_offset(vec2(full.width() / 2.0, 0.0)).1;
    let north = view.at_offset(vec2(0.0, -full.height() / 2.0)).0;
    let south = view.at_offset(vec2(0.0, full.height() / 2.0)).0;
    if !(west < east && south < north) {
        return;
    }

    let ink = Color32::from_black_alpha(60);
    let stroke = eframe::egui::Stroke::new(1.0, ink);
    let font = theme::small_caps_font();

    // Twice, the second time a point down and right in near-white: the labels
    // sit on whatever the basemap happens to be under them, and dark text on a
    // dark park is unreadable without something behind it.
    let label = |at: eframe::egui::Pos2, align: Align2, text: String| {
        painter.text(
            at + vec2(1.0, 1.0),
            align,
            &text,
            font.clone(),
            Color32::from_white_alpha(200),
        );
        painter.text(
            at,
            align,
            &text,
            font.clone(),
            Color32::from_black_alpha(220),
        );
    };

    let step = graticule_step(east - west);
    let mut lon = (west / step).ceil() * step;
    while lon <= east {
        let x = full.center().x + ((lon_to_tile(lon, zoom) - centre_x) * f64::from(size)) as f32;
        painter.line_segment([pos2(x, full.top()), pos2(x, full.bottom())], stroke);
        label(
            pos2(x + 3.0, full.top() + 3.0),
            Align2::LEFT_TOP,
            dms(lon, ('E', 'W'), step),
        );
        lon += step;
    }

    // Its own spacing: a window twice as wide as it is tall would otherwise get
    // a grid of long thin rectangles.
    let step = graticule_step(north - south);
    let mut lat = (south / step).ceil() * step;
    while lat <= north {
        let y = full.center().y + ((lat_to_tile(lat, zoom) - centre_y) * f64::from(size)) as f32;
        painter.line_segment([pos2(full.left(), y), pos2(full.right(), y)], stroke);
        label(
            pos2(full.left() + 3.0, y - 3.0),
            Align2::LEFT_BOTTOM,
            dms(lat, ('N', 'S'), step),
        );
        lat += step;
    }
}

/// Where a map placed on the basemap lands, in screen points.
///
/// The bounding box, which for a hand-turned placement is larger than the sheet
/// itself. Good enough for both jobs it does — saying what the pointer is over,
/// and outlining one map among several — and an exact outline of a turned
/// quadrilateral would be a lot of arithmetic for a hairline.
fn overlay_footprint(
    view: &crate::map::View,
    full: Rect,
    geo: &crate::geotiff::GeoRef,
) -> Option<Rect> {
    use crate::map::{TILE, lat_to_tile, lon_to_tile};
    let (north, west) = geo.top_left()?;
    let (south, east) = geo.bottom_right()?;
    let zoom = view.tile_zoom();
    let size = TILE * view.scale();
    let centre_x = lon_to_tile(view.lon, zoom);
    let centre_y = lat_to_tile(view.lat, zoom);
    let at = |lon: f64, lat: f64| {
        pos2(
            full.center().x + (lon_to_tile(lon, zoom) - centre_x) as f32 * size,
            full.center().y + (lat_to_tile(lat, zoom) - centre_y) as f32 * size,
        )
    };
    Some(Rect::from_two_pos(at(west, north), at(east, south)))
}

/// Ties the maps on the basemap to their rows in the IMAGES list, both ways.
///
/// Pointing at the basemap marks every map underneath the pointer — every one,
/// because where sheets overlap the question "which map is this" has more than
/// one answer and picking the top one would be answering a different question.
/// Pointing at a row does the reverse, outlining that map out on the tiles.
fn mark_maps_under_pointer(
    ui: &Ui,
    app: &mut GeoRefMapperApp,
    painter: &eframe::egui::Painter,
    full: Rect,
    view: &crate::map::View,
) {
    let pointer = ui
        .ctx()
        .pointer_latest_pos()
        .filter(|at| full.contains(*at));
    let mut under = Vec::new();

    // Only maps actually drawn. A row faded to nothing is not on the canvas, so
    // the pointer cannot be over it however much of the window its extent
    // covers.
    let drawn = app
        .map_extras
        .iter()
        .map(|overlay| (overlay.path.clone(), overlay.geo.clone(), overlay.opacity))
        .chain(
            app.map_overlay
                .iter()
                .map(|overlay| (overlay.path.clone(), overlay.geo.clone(), overlay.opacity)),
        )
        .collect::<Vec<_>>();

    for (path, geo, opacity) in &drawn {
        let Some(rect) = overlay_footprint(view, full, geo) else {
            continue;
        };
        if *opacity > 0.0
            && let Some(at) = pointer
            && rect.contains(at)
        {
            under.push(path.clone());
        }
        // The other direction: the row the pointer is on, outlined out here.
        if app.map_row_hovered.as_deref() == Some(path.as_path()) {
            painter.rect_stroke(
                rect,
                CornerRadius::ZERO,
                eframe::egui::Stroke::new(2.0, theme::ACCENT),
                eframe::egui::StrokeKind::Outside,
            );
        }
    }
    app.maps_under_pointer = under;
}

fn draw_overlay(
    painter: &eframe::egui::Painter,
    full: Rect,
    view: &crate::map::View,
    overlay: Option<&mut crate::map::Overlay>,
    debug: bool,
) {
    use crate::map::{TILE, lat_to_tile, lon_to_tile};

    let Some(overlay) = overlay else { return };

    // A hand-worked placement wins over anything in the file: it is the one
    // being judged, and it can be turned, which the file's simple form cannot.
    if let Some(placement) = overlay.placement {
        draw_placed_overlay(painter, full, view, overlay, placement, debug);
        return;
    }

    let (Some((north, west)), Some((south, east))) =
        (overlay.geo.top_left(), overlay.geo.bottom_right())
    else {
        return;
    };

    let zoom = view.tile_zoom();
    let size = TILE * view.scale();
    let centre_x = lon_to_tile(view.lon, zoom);
    let centre_y = lat_to_tile(view.lat, zoom);
    let at = |lon: f64, lat: f64| {
        pos2(
            full.center().x + (lon_to_tile(lon, zoom) - centre_x) as f32 * size,
            full.center().y + (lat_to_tile(lat, zoom) - centre_y) as f32 * size,
        )
    };

    let rect = Rect::from_two_pos(at(west, north), at(east, south));
    if !rect.intersects(full) {
        return;
    }

    let tint = Color32::from_white_alpha((overlay.opacity.clamp(0.0, 1.0) * 255.0) as u8);
    match &overlay.whole {
        // No squares to read: one copy, stretched.
        Some(texture) => {
            painter.image(
                texture.id(),
                rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                tint,
            );
        }
        // Squares, at the level this zoom asks for — the same drawing the
        // viewer does, so a map on the basemap sharpens as you go in instead of
        // turning to mush.
        None => {
            let painted =
                crate::imagetiles::draw(&mut overlay.squares, painter, rect, full, tint, debug);
            if debug && let Some(painted) = painted {
                tiling_hud(painter, full, &painted, &overlay.path);
            }
        }
    }
    // An outline, so the edge of the picture is findable when it is faint.
    painter.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.7)),
        eframe::egui::StrokeKind::Inside,
    );
}

// ----------------------------------------------------------------- Map images

/// Maps queued to be converted into a form that zooms quickly.
///
/// Nothing is automatic. Most pictures in a library are not maps, and of the
/// ones that are, only some are worth the disk — so the queue is built by hand,
/// picture by picture, and it survives restarts so it can be built over weeks.
///
/// The source is never touched. Converted files go under a separate root,
/// mirroring the folders they came from so the year in the path carries over.
fn map_images(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let full = ui.available_rect_before_wrap();
    ui.painter()
        .rect_filled(full, CornerRadius::ZERO, theme::WINDOW_BG);
    ui.add_space(8.0);

    target_folder(ui, app);
    ui.add_space(12.0);

    let queue = app.map_queue.clone();
    let waiting = queue.iter().filter(|item| item.written.is_none()).count();
    let running = app.converting.is_some();
    let now = ui.input(|i| i.time);

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        wrapped(
            ui,
            &match (queue.len(), waiting) {
                (0, _) => "Nothing queued".to_owned(),
                (all, 0) => format!("{all} converted"),
                (all, left) => format!("{all} queued  ·  {left} still to do"),
            },
            theme::label_font(),
            theme::TEXT,
            260.0,
            0.0,
        );
        ui.add_space(12.0);
        if running {
            if widgets::button(ui, "Stop", 110.0, true).clicked() {
                app.stop_map_conversion();
            }
        } else if widgets::button(ui, "Convert", 110.0, waiting > 0).clicked() {
            app.start_map_conversion(now);
        }
        ui.add_space(8.0);
        // Clearing the finished ones leaves the failures behind, which is the
        // point: after a long run the queue is mostly done, and what is worth
        // looking at is the handful that were not. "Empty the queue" throws
        // those away with everything else.
        let completed: Vec<i64> = queue
            .iter()
            .filter(|item| item.written.is_some())
            .map(|item| item.id)
            .collect();
        let label = if completed.is_empty() {
            "Remove completed".to_owned()
        } else {
            format!("Remove completed ({})", completed.len())
        };
        if widgets::button(ui, &label, 190.0, !completed.is_empty() && !running)
            .on_hover_text(
                "Takes the finished maps off the queue. The converted files stay \
                 where they were written; anything that failed stays on the list.",
            )
            .clicked()
        {
            app.unqueue_map_images(&completed);
        }
        ui.add_space(8.0);
        if widgets::button(ui, "Empty the queue", 150.0, !queue.is_empty() && !running).clicked() {
            app.clear_map_queue();
        }
    });

    if let Some(job) = &app.converting {
        ui.add_space(6.0);
        conversion_progress(ui, job);
    }
    ui.add_space(8.0);

    if queue.is_empty() {
        widgets::placeholder_note(
            ui,
            ui.available_rect_before_wrap(),
            "Right-click a map in the Mosaic or the FILES tree\nand choose \u{201c}Add to map images\u{201d}.",
        );
        return;
    }

    let root = std::path::PathBuf::from(&app.config.map_export_root);
    let mut drop: Vec<i64> = Vec::new();
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            queue_table(ui, &queue, &root, &mut drop);
        });
    if !drop.is_empty() {
        app.unqueue_map_images(&drop);
    }
}

/// What the conversion is doing right now.
///
/// Two bars rather than one: how far through the queue, and how far through the
/// map in hand. A single bar would sit still for minutes on a large map and
/// look stuck.
fn conversion_progress(ui: &mut Ui, job: &crate::app::Conversion) {
    widgets::card(ui, |ui| {
        widgets::kv_row(
            ui,
            "Converting",
            &format!("{} of {}", job.done + 1, job.total),
        );
        if !job.current.is_empty() {
            widgets::kv_block(ui, "Now", &job.current);
        }
        // One bar for the whole picture rather than one per layer. Which layer
        // is being written is an implementation detail of the file format; the
        // question being asked is how long this map has left, and a bar that
        // filled and reset seven times answered a different one.
        match job.progress {
            Some(progress) => bar(ui, progress.fraction()),
            None => widgets::kv_row(ui, "Progress", "reading the picture"),
        }
    });
}

/// A plain progress bar.
fn bar(ui: &mut Ui, fraction: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(3), theme::SURFACE);
    let filled = Rect::from_min_size(
        rect.min,
        vec2(rect.width() * fraction.clamp(0.0, 1.0), rect.height()),
    );
    ui.painter()
        .rect_filled(filled, CornerRadius::same(3), theme::ACCENT);
}

/// Indexing a selection made somewhere else.
///
/// Its own section rather than a third button beside Scan: this does not walk
/// anything. A download of an archive's collection is mostly maps you do not
/// want — 10,858 files against a selection of 8,054 — and scanning the folder
/// would index the ones already rejected.
fn manifest_section(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    widgets::section_label(ui, "INDEX FROM A SELECTION");
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let shown = if app.config.manifest_path.is_empty() {
            "— none chosen —".to_owned()
        } else {
            tilde(std::path::Path::new(&app.config.manifest_path))
        };
        wrapped(
            ui,
            &shown,
            theme::label_font(),
            if app.config.manifest_path.is_empty() {
                theme::TEXT_DIM
            } else {
                theme::TEXT
            },
            ui.available_width() - 130.0,
            0.0,
        );
        ui.add_space(8.0);
        if widgets::button(ui, "Choose…", 110.0, !app.scan.is_running()).clicked()
            && let Some(file) = rfd::FileDialog::new()
                .set_title("Choose a map selection")
                .add_filter("Map selection", &["json"])
                .pick_file()
        {
            let ctx = ui.ctx().clone();
            app.index_manifest(file, &ctx);
        }
    });

    // Offered again once one has been chosen: a selection is regenerated as the
    // collection changes, and re-indexing it is the ordinary thing to do next.
    if !app.config.manifest_path.is_empty() {
        ui.add_space(4.0);
        if widgets::button(ui, "Index it again", 150.0, !app.scan.is_running()).clicked() {
            let (file, ctx) = (
                std::path::PathBuf::from(&app.config.manifest_path),
                ui.ctx().clone(),
            );
            app.index_manifest(file, &ctx);
        }
    }

    ui.add_space(4.0);
    wrapped(
        ui,
        "Indexes only the pictures the selection names — the folder it points \
         into holds the rejected ones too. Nothing is copied or moved. They then \
         appear in the tree and the Mosaic like any other pictures, to be looked \
         through and added to the map queue.",
        theme::small_caps_font(),
        theme::TEXT_DIM,
        ui.available_width(),
        0.0,
    );
    ui.add_space(10.0);
}

/// Where converted maps are written, and the warning if it is not set.
fn target_folder(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    manifest_section(ui, app);
    widgets::section_label(ui, "WRITE CONVERTED MAPS TO");
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let shown = if app.config.map_export_root.is_empty() {
            "— not set —".to_owned()
        } else {
            tilde(std::path::Path::new(&app.config.map_export_root))
        };
        wrapped(
            ui,
            &shown,
            theme::label_font(),
            if app.config.map_export_root.is_empty() {
                theme::WARNING
            } else {
                theme::TEXT
            },
            ui.available_width() - 130.0,
            0.0,
        );
        ui.add_space(8.0);
        if widgets::button(ui, "Choose…", 110.0, true).clicked()
            && let Some(folder) = rfd::FileDialog::new()
                .set_title("Where should converted maps go?")
                .pick_folder()
        {
            app.config.map_export_root = folder.to_string_lossy().into_owned();
            app.persist_config();
        }
    });
    ui.add_space(4.0);
    wrapped(
        ui,
        "Separate from the Export page. Originals are never written to, moved or \
         renamed — the converted copy is a new file under this folder, in the same \
         folder structure it came from.",
        theme::small_caps_font(),
        theme::TEXT_DIM,
        ui.available_width(),
        0.0,
    );

    ui.add_space(8.0);
    // Needs somewhere to write before it can start on its own, so the switch is
    // dead until that is chosen — with the reason on it rather than left to be
    // worked out from a toast that never comes.
    let ready = !app.config.map_export_root.is_empty();
    ui.add_enabled_ui(ready, |ui| {
        if ui
            .checkbox(
                &mut app.config.auto_convert_maps,
                "Start converting as soon as maps are added",
            )
            .on_hover_text(if ready {
                "Adding maps to the queue begins the conversion straight away. \
                 Anything added while one is already running joins the queue and \
                 is picked up in turn."
            } else {
                "Choose a folder to write converted maps to first."
            })
            .changed()
        {
            app.persist_config();
        }
    });
}

/// The queue, as a table.
///
/// Columns rather than a card each: the whole point of a queue is comparing
/// what is in it, and stacked cards made twelve maps into a page of scrolling.
fn queue_table(
    ui: &mut Ui,
    queue: &[crate::db::Queued],
    root: &std::path::Path,
    drop: &mut Vec<i64>,
) {
    const SIZE: f32 = 130.0;
    const STATE: f32 = 190.0;
    const REMOVE: f32 = 30.0;
    let gap = 10.0;

    let head = ui.available_width();
    let name_width = (head - SIZE - STATE - REMOVE - gap * 3.0).max(120.0);

    // A heading row, so the columns are named once rather than repeated on
    // every line as labels.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (label, width) in [
            ("MAP", name_width),
            ("SIZE", SIZE),
            ("STATE", STATE),
            ("", REMOVE),
        ] {
            let (rect, _) = ui.allocate_exact_size(vec2(width, 16.0), Sense::hover());
            ui.painter().text(
                pos2(rect.left(), rect.center().y),
                Align2::LEFT_CENTER,
                label,
                theme::small_caps_font(),
                theme::TEXT_DIM,
            );
            ui.add_space(gap);
        }
    });
    ui.add_space(2.0);
    let line = ui.available_rect_before_wrap();
    ui.painter().hline(
        line.left()..=line.right(),
        line.top(),
        eframe::egui::Stroke::new(1.0, theme::HAIRLINE),
    );
    ui.add_space(4.0);

    for (index, item) in queue.iter().enumerate() {
        let source = std::path::Path::new(&item.path);
        let row = ui.available_rect_before_wrap();
        let row = Rect::from_min_size(row.min, vec2(row.width(), 22.0));
        // Banded, so the eye can follow a line across four columns.
        if index % 2 == 1 {
            ui.painter().rect_filled(
                row,
                CornerRadius::same(3),
                theme::SURFACE.gamma_multiply(0.4),
            );
        }

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let name = source
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();

            let (rect, response) = ui.allocate_exact_size(vec2(name_width, 22.0), Sense::hover());
            ui.painter().text(
                pos2(rect.left(), rect.center().y),
                Align2::LEFT_CENTER,
                widgets::elide(ui, &name, theme::label_font(), name_width - 4.0),
                theme::label_font(),
                theme::TEXT,
            );
            // The full paths on hover rather than on the row: they are long,
            // and they are only wanted when something looks wrong.
            let target = target_for(source, root);
            response.on_hover_text(format!(
                "From  {}\nTo      {}",
                tilde(source),
                target
                    .as_ref()
                    .map(|path| tilde(path))
                    .unwrap_or_else(|| "— no folder chosen —".to_owned()),
            ));
            ui.add_space(gap);

            let (rect, _) = ui.allocate_exact_size(vec2(SIZE, 22.0), Sense::hover());
            let size = match (item.width, item.height) {
                (Some(width), Some(height)) => format!(
                    "{:.0} MP  ·  {}",
                    f64::from(width) * f64::from(height) / 1e6,
                    crate::files::format_bytes(item.bytes),
                ),
                _ => crate::files::format_bytes(item.bytes),
            };
            ui.painter().text(
                pos2(rect.left(), rect.center().y),
                Align2::LEFT_CENTER,
                &size,
                theme::small_caps_font(),
                theme::TEXT_MUTED,
            );
            ui.add_space(gap);

            let (rect, response) = ui.allocate_exact_size(vec2(STATE, 22.0), Sense::hover());
            let (state, colour) = match (&item.written, &item.failure) {
                (Some(_), _) => ("Converted".to_owned(), theme::GEOREFERENCED),
                (None, Some(why)) => (
                    widgets::elide(ui, why, theme::small_caps_font(), STATE - 4.0),
                    theme::CLOSE_HOVER,
                ),
                (None, None) => ("Waiting".to_owned(), theme::TEXT_DIM),
            };
            ui.painter().text(
                pos2(rect.left(), rect.center().y),
                Align2::LEFT_CENTER,
                &state,
                theme::small_caps_font(),
                colour,
            );
            match (&item.written, &item.failure) {
                (Some(written), _) => {
                    response.on_hover_text(tilde(std::path::Path::new(written)));
                }
                (None, Some(why)) => {
                    response.on_hover_text(why);
                }
                _ => {}
            }
            ui.add_space(gap);

            if widgets::button(ui, "×", REMOVE, true)
                .on_hover_text("Take this one off the queue")
                .clicked()
            {
                drop.push(item.id);
            }
        });
        ui.add_space(2.0);
    }
}

/// Where a source map's converted copy belongs.
///
/// The source path is mirrored under the target root, keeping every folder
/// level — the year is in the path, so flattening would lose it. Only the
/// leading `/` is dropped, which is what makes the join work at all.
/// Where a converted map is written.
///
/// Under the output folder, in a folder named after the one the source sat in:
/// `…/MAPS-DL/1932/plan.png` becomes `…/MAPS-OUT/1932/plan.tif`. What the
/// folder is *called* is the part worth keeping — for these maps it is the year
/// — and the road to the file is not.
///
/// Always `.tif`, whatever went in: the output is a tiled, layered TIFF, and a
/// name saying otherwise is a file every other program opens by its extension
/// and fails on.
///
/// Two sources whose parent folders share a name are gathered into one, which
/// is generally the point. If two files then share a name as well, the second
/// replaces the first and the queue says `replaced`.
pub fn target_for(source: &std::path::Path, root: &std::path::Path) -> Option<std::path::PathBuf> {
    if root.as_os_str().is_empty() {
        return None;
    }
    let name = source.file_name()?;
    let folder = source
        .parent()
        .and_then(|parent| parent.file_name())
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    Some(root.join(folder).join(name).with_extension("tif"))
}

#[cfg(test)]
mod map_image_targets {
    use super::target_for;
    use std::path::{Path, PathBuf};

    #[test]
    fn a_png_source_produces_a_tif_because_that_is_what_was_written() {
        // The bug this pins: the name was mirrored from the source, so a PNG
        // map came out as a `.png` holding TIFF bytes.
        for (source, expected) in [
            (
                "/pics/1892 Town of Singapore.png",
                "1892 Town of Singapore.tif",
            ),
            (
                "/pics/3569_Singapore_63.36K_3L-12.jpg",
                "3569_Singapore_63.36K_3L-12.tif",
            ),
            ("/pics/1887.1%252Fitem.png", "1887.1%252Fitem.tif"),
            ("/pics/no extension at all", "no extension at all.tif"),
            ("/pics/already.tif", "already.tif"),
        ] {
            let target = target_for(Path::new(source), Path::new("/out")).unwrap();
            assert_eq!(
                target.file_name().unwrap().to_string_lossy(),
                expected,
                "{source}",
            );
        }
    }

    #[test]
    fn the_year_folder_is_all_that_is_kept() {
        // There is no other arrangement to choose: the folder name is the part
        // that means something and the path to it is not.
        let target = target_for(
            Path::new("/media/disk/MAPS-DL/1932/plan.png"),
            Path::new("/out"),
        );
        assert_eq!(target, Some(PathBuf::from("/out/1932/plan.tif")));
    }

    #[test]
    fn the_source_folder_layout_keeps_the_year_and_drops_the_road_to_it() {
        // The point of it: `1932/…` stays `1932/…` instead of arriving under a
        // rebuilt copy of `/home/morgan/Pictures/MAPS-NEW/`.
        let target = target_for(
            Path::new("/home/morgan/Pictures/MAPS-NEW/1932/Kranjie River.png"),
            Path::new("/mnt/maps"),
        );
        assert_eq!(
            target,
            Some(PathBuf::from("/mnt/maps/1932/Kranjie River.tif")),
        );
    }

    #[test]
    fn sources_from_several_folders_each_get_their_own() {
        // A batch spanning three years produces three folders, named after the
        // folders the maps came from and nothing else.
        let root = Path::new("/out");
        let folders: Vec<PathBuf> = ["1932", "1945", "1958"]
            .into_iter()
            .map(|year| {
                let source = PathBuf::from(format!("/pics/MAPS/{year}/plan.tif"));
                let target = target_for(&source, root).unwrap();
                target.parent().unwrap().to_path_buf()
            })
            .collect();
        assert_eq!(
            folders,
            vec![
                PathBuf::from("/out/1932"),
                PathBuf::from("/out/1945"),
                PathBuf::from("/out/1958"),
            ],
        );
    }

    #[test]
    fn two_folders_of_the_same_name_are_gathered_together() {
        // Stated rather than guarded against: `MAPS/1932` and `MAPS-NEW/1932`
        // both become `/out/1932`, which is usually what is wanted. Two files
        // of the same name within it collide, and `convert` refuses the second
        // rather than overwriting the first.
        let a = target_for(Path::new("/pics/MAPS/1932/a.tif"), Path::new("/out"));
        let b = target_for(Path::new("/pics/MAPS-NEW/1932/b.tif"), Path::new("/out"));
        assert_eq!(a.unwrap().parent(), b.unwrap().parent());
    }

    #[test]
    fn two_maps_of_the_same_name_in_different_years_stay_apart() {
        let a = target_for(Path::new("/pics/1954/map.tif"), Path::new("/out")).unwrap();
        let b = target_for(Path::new("/pics/1961/map.tif"), Path::new("/out")).unwrap();
        assert_ne!(a, b, "mirroring is what keeps these from colliding");
    }

    #[test]
    fn no_target_folder_means_no_target() {
        assert_eq!(
            target_for(Path::new("/pics/map.tif"), Path::new("")),
            None,
            "an unset folder must not resolve to the root of the disk",
        );
    }

    #[test]
    fn the_converted_copy_never_lands_on_the_original() {
        let source = Path::new("/home/morgan/Pictures/MAPS/1846.tif");
        for root in ["/mnt/maps", "/home/morgan/Converted"] {
            let target = target_for(source, Path::new(root)).unwrap();
            assert_ne!(target, source, "the source must never be the destination");
        }
    }
}

/// Asks for the zoom levels either side of this one.
///
/// Zooming out is the cheap direction and the one worth covering completely: a
/// level up is a quarter of the tiles for the same ground, so two levels cost
/// less than a third of one screenful and make every zoom-out instant. They
/// double as the stand-ins [`crate::map::Tiles::ancestor`] draws while sharp
/// tiles arrive, so they earn their keep twice.
///
/// Zooming in cannot be covered — a level down is four times the tiles — so
/// only the middle of the screen is fetched, which is where a zoom lands.
fn prefetch_neighbouring_zooms(tiles: &mut crate::map::Tiles, view: &crate::map::View, full: Rect) {
    use crate::map::{TILE, TileId, Want, lat_to_tile, lon_to_tile};

    let zoom = view.tile_zoom();
    let size = TILE * view.scale();

    for step in 1..=2_u32 {
        if step > zoom {
            break;
        }
        let coarser = zoom - step;
        let world = 1_i64 << coarser;
        let cx = lon_to_tile(view.lon, coarser);
        let cy = lat_to_tile(view.lat, coarser);
        // The same window, in a level whose tiles cover twice the ground each.
        let reach = 2_f32.powi(step as i32);
        let across = (full.width() / (size * reach) / 2.0).ceil() as i64 + 1;
        let down = (full.height() / (size * reach) / 2.0).ceil() as i64 + 1;
        for dy in -down..=down {
            for dx in -across..=across {
                let ty = cy.floor() as i64 + dy;
                if ty < 0 || ty >= world {
                    continue;
                }
                tiles.request(
                    TileId {
                        zoom: coarser,
                        x: (cx.floor() as i64 + dx).rem_euclid(world) as u32,
                        y: ty as u32,
                    },
                    Want::Prefetch,
                );
            }
        }
    }

    // One level in, centre only.
    let finer = zoom + 1;
    if finer <= crate::map::MAX_ZOOM {
        let world = 1_i64 << finer;
        let cx = lon_to_tile(view.lon, finer).floor() as i64;
        let cy = lat_to_tile(view.lat, finer).floor() as i64;
        for dy in -1..=1 {
            for dx in -1..=1 {
                let ty = cy + dy;
                if ty < 0 || ty >= world {
                    continue;
                }
                tiles.request(
                    TileId {
                        zoom: finer,
                        x: (cx + dx).rem_euclid(world) as u32,
                        y: ty as u32,
                    },
                    Want::Prefetch,
                );
            }
        }
    }
}

/// Where we are, and the attribution the licence requires.
fn map_overlay(
    ui: &Ui,
    full: Rect,
    view: &crate::map::View,
    missing: usize,
    tiles: crate::map::Status,
) {
    let crate::map::Status {
        pending,
        queued,
        resident,
        refused,
    } = tiles;
    // Required by the ODbL: the data is free, and saying whose it is is the
    // condition of using it.
    let credit = "© OpenStreetMap contributors";
    let font = theme::small_caps_font();
    let width = ui
        .ctx()
        .fonts_mut(|f| f.layout_no_wrap(credit.to_owned(), font.clone(), theme::TEXT))
        .size()
        .x;
    let plate = Rect::from_min_size(
        pos2(full.right() - width - 16.0, full.bottom() - 24.0),
        vec2(width + 12.0, 18.0),
    );
    ui.painter()
        .rect_filled(plate, CornerRadius::same(3), Color32::from_black_alpha(160));
    ui.painter().text(
        plate.center(),
        Align2::CENTER_CENTER,
        credit,
        font.clone(),
        theme::TEXT_MUTED,
    );

    let mut status = format!("{:.5}, {:.5}  ·  zoom {:.1}", view.lat, view.lon, view.zoom);
    if missing > 0 || pending > 0 {
        status.push_str(&format!("  ·  {pending} loading"));
        // The prefetch queue is deliberately long, so saying only how many are
        // in flight would look stalled when it is working through a backlog.
        if queued > pending {
            status.push_str(&format!(" of {queued}"));
        }
    }
    status.push_str(&format!("  ·  {resident} held"));
    // A refusal used to arrive as a picture saying "access denied", which drew
    // like any other tile. Now it is thrown away, so without this line the map
    // would simply be blank with no explanation.
    if refused > 0 {
        status.push_str(&format!("  ·  {refused} refused — set a contact in Config"));
    }
    let plate = Rect::from_min_size(
        pos2(full.left() + 8.0, full.bottom() - 24.0),
        vec2(if refused > 0 { 420.0 } else { 260.0 }, 18.0),
    );
    ui.painter()
        .rect_filled(plate, CornerRadius::same(3), Color32::from_black_alpha(160));
    ui.painter().text(
        pos2(plate.left() + 6.0, plate.center().y),
        Align2::LEFT_CENTER,
        status,
        font,
        theme::TEXT_MUTED,
    );
}

// --------------------------------------------------------------------- Mosaic

/// Tile rounding, tracking the tile size.
///
/// Rounding is a fraction of the shape it is applied to, not a constant: 6 pt
/// on a 240 pt tile is a hint, and on a 24 pt tile it is most of the edge.
/// Below 48 pt it is dropped altogether — at that size the corner eats more of
/// the picture than it is worth.
fn tile_corner(tile: f32) -> CornerRadius {
    if tile < 48.0 {
        return CornerRadius::ZERO;
    }
    CornerRadius::same((tile * 0.04).round().clamp(1.0, 6.0) as u8)
}

/// At or below this tile size the Mosaic uses the 48 px textures. Set a little
/// above [`crate::cache::MINI_EDGE`] so a tile is never magnified beyond its
/// source, which would look softer than the placeholder it replaced.
const MINI_TILE: f32 = 56.0;

/// Smallest Mosaic tile. At this size a picture is a coloured smudge, but a
/// thousand of them on screen at once is a shape you can navigate by — which
/// is the whole point of zooming out this far.
pub const MIN_TILE: f32 = 10.0;

fn mosaic(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let full = ui.available_rect_before_wrap();
    ui.painter()
        .rect_filled(full, CornerRadius::ZERO, theme::WINDOW_BG);

    let toolbar = Rect::from_min_size(full.min, vec2(full.width(), 34.0));
    // The upper end is "one tile fills the pane", not a fixed number, so the
    // zoom has somewhere to arrive at whatever the window size.
    let max_tile = (full.width().min(full.height() - toolbar.height()) - 32.0).max(MIN_TILE + 1.0);
    ui.painter()
        .rect_filled(toolbar, CornerRadius::ZERO, theme::PANEL_BG);
    ui.painter().line_segment(
        [toolbar.left_bottom(), toolbar.right_bottom()],
        theme::hairline(),
    );

    // Selecting a folder in the tree moves the grid; selecting an image — which
    // is what clicking a tile does — leaves it exactly where it was.
    if app
        .files
        .selected_node()
        .is_some_and(crate::files::Node::is_folder)
    {
        app.mosaic_root = Some(app.files.selected.clone());
    }
    let root = app
        .mosaic_root
        .clone()
        .unwrap_or_else(|| app.files.selected_folder());
    app.mosaic_root = Some(root.clone());

    // Every marked folder, when folders have been marked: picking three in the
    // tree and seeing all three folders' pictures at once is the point of being
    // able to mark them. Otherwise just the one the grid is rooted on.
    let marked_folders: Vec<crate::files::NodePath> =
        app.files.marked_folders.iter().cloned().collect();
    let roots = if marked_folders.is_empty() {
        vec![root.clone()]
    } else {
        marked_folders
    };

    // What has been picked out, when the grid is being narrowed to it. Taken
    // as paths so a set spanning folders is answered by one lookup per tile.
    let only_selected: Option<std::collections::HashSet<std::path::PathBuf>> = app
        .config
        .mosaic_selected_only
        .then(|| app.files.marked_paths_only().into_iter().collect());

    // Owned so the grid below can borrow `app` mutably for thumbnail requests.
    let waiting = placeholder_note_text(app);
    let mut seen = std::collections::BTreeSet::new();
    // Collapsed: a stack shows only its cover. Inside an open one: only that
    // stack. With the toggle off: everything, loose, exactly as before stacks
    // existed — which is also how you reach a member without opening it.
    let stacked = app.config.show_stacks && app.open_stack.is_none();
    let inside = app.open_stack;
    // Showing the selection means the *whole* selection, wherever it lives.
    //
    // Scoping it to the folder as well made the grid change under you: flip to
    // a picture in another year and the grid re-roots there, leaving whatever
    // handful of the set happens to live in that one folder. A set built by tag
    // is spread across dozens of folders, and seeing it together is the entire
    // reason for the mode.
    let sources: Vec<crate::files::NodePath> = if app.config.mosaic_selected_only {
        app.files.marked.iter().cloned().collect()
    } else {
        Vec::new()
    };
    let from_selection = !sources.is_empty();

    let mut images: Vec<(crate::files::NodePath, String, Option<i64>, Option<String>)> = roots
        .iter()
        .flat_map(|folder| {
            if from_selection {
                Vec::new()
            } else {
                app.files.images_in(folder, app.config.mosaic_recursive)
            }
        })
        .chain(
            sources
                .iter()
                .filter_map(|path| app.files.get(path).map(|node| (path.clone(), node)))
                .filter(|(_, node)| !node.is_folder()),
        )
        // Marked folders can nest, so the same picture can arrive twice.
        .filter(|(path, _)| seen.insert(path.clone()))
        .map(|(path, node)| {
            let (db_id, file) = match &node.kind {
                Kind::Image(meta) => (meta.db_id, meta.path.clone()),
                Kind::Folder => (None, None),
            };
            (path, node.name.clone(), db_id, file)
        })
        .filter(|(_, _, db_id, _)| match (inside, db_id) {
            (Some(stack), Some(id)) => app.stacks.of(*id) == Some(stack),
            (Some(_), None) => false,
            (None, Some(id)) => {
                !stacked || app.stacks.of(*id).is_none() || app.stacks.is_cover(*id)
            }
            (None, None) => true,
        })
        // Still needed for the one case the source above cannot express: the
        // mode is on and *nothing* is marked, so the grid falls back to the
        // folder and this empties it. With a selection, it passes everything
        // through — the source already is the selection.
        .filter(|(_, _, _, file)| match (&only_selected, file) {
            (Some(picked), Some(path)) => picked.contains(std::path::Path::new(path)),
            (Some(_), None) => false,
            (None, _) => true,
        })
        .collect();
    // An open stack lists its members in the stack's own order, so the cover
    // stays first however the folder happens to sort.
    if let Some(stack) = inside {
        let order = app.stacks.members(stack).to_vec();
        images.sort_by_key(|(_, _, db_id, _)| {
            db_id
                .and_then(|id| order.iter().position(|member| *member == id))
                .unwrap_or(usize::MAX)
        });
    }

    // One row: the folder on the left where reading starts, the toggle parked
    // on the right. The path gets everything up to it, which on a wide window
    // is most of the bar.
    const RECURSIVE_WIDTH: f32 = 400.0;
    let controls = Rect::from_min_max(
        pos2(toolbar.right() - 14.0 - RECURSIVE_WIDTH, toolbar.top()),
        pos2(toolbar.right() - 14.0, toolbar.bottom()),
    );
    ui.scope_builder(
        eframe::egui::UiBuilder::new().max_rect(controls).layout(
            eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
        ),
        |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            widgets::toggle(ui, &mut app.config.mosaic_recursive, "Recursive");
            ui.add_space(14.0);
            widgets::toggle(ui, &mut app.config.show_stacks, "Stacks");
            ui.add_space(14.0);
            // Narrows the grid to what has been picked out. With a set built by
            // tag across a whole library, the pictures in it are scattered
            // through dozens of folders and there is otherwise nowhere to see
            // them together — which is the moment you most want to look at them.
            widgets::toggle(ui, &mut app.config.mosaic_selected_only, "Selected only");
        },
    );

    let path_rect = Rect::from_min_max(
        pos2(toolbar.left() + 14.0, toolbar.top()),
        pos2(controls.left() - 12.0, toolbar.bottom()),
    );
    let path_response = ui.interact(path_rect, ui.id().with("mosaic_path"), Sense::click());
    if path_response.hovered() {
        ui.ctx()
            .set_cursor_icon(eframe::egui::CursorIcon::PointingHand);
    }
    // Clicking it copies the real path — worth having when the folder is thirty
    // characters deep and you want it in a terminal.
    if path_response.clicked()
        && let Some(folder) = app.files.get(&root).and_then(|node| node.path())
    {
        ui.ctx().copy_text(folder.to_owned());
        app.toast = Some(("Path copied to clipboard".to_owned(), ui.input(|i| i.time)));
    }
    ui.painter().text(
        pos2(path_rect.left(), path_rect.center().y),
        Align2::LEFT_CENTER,
        widgets::elide(
            ui,
            &app.files.label_for(&root),
            theme::small_caps_font(),
            path_rect.width(),
        ),
        theme::small_caps_font(),
        if path_response.hovered() {
            theme::ACCENT
        } else {
            theme::TEXT
        },
    );

    app.mosaic_max_tile = max_tile;
    app.mosaic_tile_size = app.mosaic_tile_size.clamp(MIN_TILE, max_tile);

    let grid_area = Rect::from_min_max(pos2(full.left(), toolbar.bottom()), full.max);

    // The wheel zooms rather than scrolls, so the grid runs continuously from
    // a contact sheet down to a single picture filling the pane. Shift+wheel
    // keeps the ordinary scroll, and the scrollbar and drag still work.
    // Ctrl+wheel zooms the tiles; the wheel on its own scrolls the grid, which
    // is what a wheel does everywhere else. egui hands the two over separately:
    // with the zoom modifier held it fills `zoom_delta` and leaves the scroll
    // delta at zero, so reading only one of them would lose the gesture.
    let (zoom, shift, control, pointer) = ui.input(|i| {
        (
            i.zoom_delta(),
            i.modifiers.shift,
            i.modifiers.command || i.modifiers.ctrl,
            i.pointer.hover_pos(),
        )
    });
    let over_grid = pointer.is_some_and(|p| grid_area.contains(p));
    let zooming = over_grid && zoom != 1.0;
    let mut handover = false;
    if zooming {
        let previous = app.mosaic_tile_size;
        app.mosaic_tile_size = (previous * zoom).clamp(MIN_TILE, max_tile);
        // Already as large as the pane allows and still being pushed inwards:
        // the next step is into the picture itself, so hand over to the Viewer.
        handover = zoom > 1.0 && previous >= max_tile - 0.5;
    }

    if images.is_empty() {
        // Empty for two quite different reasons, and saying which saves looking
        // for pictures that were never going to be there.
        let note = if app.config.mosaic_selected_only && !app.files.has_marks() {
            "Nothing is selected  ·  turn off “Selected only” to see the folder"
        } else if app.config.mosaic_selected_only {
            "Nothing selected in this folder"
        } else {
            "No images in the current FILES selection"
        };
        widgets::placeholder_note(ui, grid_area, note);
        ui.advance_cursor_after_rect(full);
        return;
    }

    let mut select: Option<crate::files::NodePath> = None;
    let mut open_stack: Option<i64> = None;
    let mut mark: Option<crate::files::NodePath> = None;
    let mut range: Option<crate::files::NodePath> = None;
    let mut open = false;
    // The grid's own order is what a shift-range runs along — the order the
    // pictures are in on screen, not the order the tree happens to hold them.
    let grid_order: Vec<crate::files::NodePath> =
        images.iter().map(|(path, ..)| path.clone()).collect();
    // Remembered for the arrow keys, which need the grid's own order and its
    // width to move by a row rather than by one tile.
    app.mosaic_order = grid_order.clone();
    // Taken once, so a scroll made by hand afterwards is not fought.
    let reveal = std::mem::take(&mut app.reveal_in_grid);
    // What a right-click on a marked tile would act on.
    let batch = app.files.action_paths();
    let grid_stack = Some(app.stack_menu(&batch));
    let map_support = app.config.map_support;
    let mut menu = crate::panels::MenuResult::default();
    // Which picture the zoom is heading into. A selected one wins over whatever
    // the cursor happens to be over: having picked an image, that is the one
    // you mean, and the alternative was landing on the first tile in the grid
    // because that is what sits under the cursor at the top of the scroll.
    let mut handover_selected: Option<crate::files::NodePath> = None;
    let mut handover_hovered: Option<crate::files::NodePath> = None;

    ui.scope_builder(
        eframe::egui::UiBuilder::new().max_rect(grid_area.shrink(12.0)),
        |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                // The wheel scrolls, always. Zooming is Ctrl+wheel, which egui
                // never reports as a scroll, so the two cannot collide.
                .show(ui, |ui| {
                    let tile = app.mosaic_tile_size;
                    // Proportional too: a fixed 8 pt gap between 10 pt tiles is
                    // a grid made mostly of background.
                    let gap = (tile * 0.06).clamp(2.0, 8.0);
                    let captions = app.config.mosaic_filenames;
                    // The picture currently laid over the map, matched by path.
                    let placed = app.map_overlay.as_ref().map(|o| o.path.clone());
                    let placed = placed.as_deref();
                    // A fixed 6 pt radius reads as a heavy bevel once the tiles
                    // are 24 pt across, so it tracks the tile and disappears
                    // entirely on the smallest ones.
                    let corner = tile_corner(tile);
                    let columns = ((ui.available_width() + gap) / (tile + gap))
                        .floor()
                        .max(1.0) as usize;
                    app.mosaic_columns = columns;
                    let rows = images.len().div_ceil(columns);

                    let origin = ui.cursor().left_top();
                    let (_, _) = ui.allocate_exact_size(
                        vec2(ui.available_width(), rows as f32 * (tile + gap)),
                        Sense::hover(),
                    );

                    for (position, (path, name, db_id, file)) in images.iter().enumerate() {
                        let row = position / columns;
                        let column = position % columns;
                        let rect = Rect::from_min_size(
                            origin + vec2(column as f32 * (tile + gap), row as f32 * (tile + gap)),
                            vec2(tile, tile),
                        );
                        let active = app.files.selected == *path;
                        // Both of these have to happen before the visibility
                        // test. The selected picture is usually the one that is
                        // *not* on screen — that is precisely why the view has
                        // to travel to it — and skipping it here left the zoom
                        // handing over to whatever the cursor was resting on.
                        if active && (zooming || reveal) {
                            ui.scroll_to_rect(rect, Some(eframe::egui::Align::Center));
                        }
                        if handover && active {
                            handover_selected = Some(path.clone());
                        }

                        if !ui.is_rect_visible(rect) {
                            continue;
                        }

                        let response = ui.interact(
                            rect,
                            ui.id().with(("mosaic", path)),
                            Sense::click().union(Sense::hover()),
                        );
                        // The same menu the FILES tree offers, from one place,
                        // so the two surfaces cannot drift apart.
                        if let Some(file) = file {
                            crate::panels::file_context_menu(
                                ui,
                                &response,
                                std::path::Path::new(file),
                                &batch,
                                grid_stack.as_ref(),
                                db_id.is_some_and(|id| app.stacks.of(id).is_some()),
                                map_support,
                                &mut menu,
                            );
                        }
                        // Behind the thumbnail, so the margins around it carry
                        // the mark and the picture itself stays untinted.
                        if placed.is_some() && placed == file.as_deref().map(std::path::Path::new) {
                            ui.painter().rect_filled(rect, corner, theme::ON_THE_MAP);
                        }
                        if !draw_thumbnail(ui, app, rect, *db_id, tile <= MINI_TILE) {
                            placeholder_tile(ui, rect, corner, waiting);
                        }
                        // Not on the smallest tiles: the mark would be a third
                        // of the picture and every tile would read as a dot.
                        if tile > MINI_TILE
                            && db_id.is_some_and(|id| app.georeferenced.contains(&id))
                        {
                            geo_badge(ui.painter(), rect);
                        }

                        // Selected is the picture the Viewer would show;
                        // marked is the set a batch would act on. Different
                        // colours, not two shades of one, because they answer
                        // different questions.
                        let marked = app.files.mark_of(path);
                        if let Some(mark) = marked {
                            ui.painter().rect_filled(
                                rect,
                                corner,
                                mark_colour(mark).gamma_multiply(0.30),
                            );
                        }
                        if active || marked.is_some() || response.hovered() {
                            ui.painter().rect_stroke(
                                rect,
                                corner,
                                Stroke::new(
                                    if active { 2.0 } else { 1.5 },
                                    match (active, marked) {
                                        (true, _) => theme::ACCENT,
                                        (false, Some(mark)) => mark_colour(mark),
                                        (false, None) => theme::TEXT_MUTED,
                                    },
                                ),
                                StrokeKind::Inside,
                            );
                        }

                        // A stack says how many are behind it, top-right the
                        // way Lightroom marks one.
                        if stacked
                            && let Some(stack) = db_id.and_then(|id| app.stacks.of(id))
                            && tile >= 40.0
                        {
                            stack_badge(ui, rect, app.stacks.len(stack));
                        }

                        // Caption band along the bottom of the tile. Off unless
                        // asked for: it covers the bottom of every picture.
                        if captions && tile >= 96.0 {
                            let band = Rect::from_min_max(
                                pos2(rect.left(), rect.bottom() - 22.0),
                                rect.max,
                            );
                            ui.painter().rect_filled(
                                band,
                                CornerRadius {
                                    nw: 0,
                                    ne: 0,
                                    sw: corner.sw,
                                    se: corner.se,
                                },
                                Color32::from_black_alpha(150),
                            );
                            // Long names must not spill onto neighbouring tiles.
                            ui.painter().text(
                                band.center(),
                                Align2::CENTER_CENTER,
                                widgets::elide(
                                    ui,
                                    name,
                                    theme::small_caps_font(),
                                    band.width() - 10.0,
                                ),
                                theme::small_caps_font(),
                                // Green when the picture says where it is, the
                                // same signal the FILES tree gives.
                                match db_id {
                                    Some(id) if app.georeferenced.contains(id) => {
                                        theme::GEOREFERENCED
                                    }
                                    _ => theme::TEXT,
                                },
                            );
                        }

                        // The same three gestures the FILES tree uses, because
                        // building a selection has to work the same way in both
                        // places. A plain click only selects; opening is the
                        // double click, which leaves single clicks free for
                        // picking out a batch.
                        if response.clicked() {
                            app.focus = crate::app::Focus::Grid;
                            if control {
                                mark = Some(path.clone());
                            } else if shift {
                                range = Some(path.clone());
                            } else {
                                select = Some(path.clone());
                            }
                        }
                        if response.double_clicked() {
                            match db_id.filter(|_| stacked).and_then(|id| app.stacks.of(id)) {
                                // Opening a stack shows what is inside it; the
                                // picture itself is one more double-click away.
                                Some(stack) => open_stack = Some(stack),
                                None => {
                                    select = Some(path.clone());
                                    open = true;
                                }
                            }
                        }
                        // Only as a fallback: with nothing selected, the picture
                        // under the cursor is the one being zoomed into.
                        if handover && !active && pointer.is_some_and(|p| rect.contains(p)) {
                            handover_hovered = Some(path.clone());
                        }
                    }
                });
        },
    );

    if let Some(path) = handover_selected.or(handover_hovered) {
        select = Some(path);
        open = true;
    }
    if let Some(stack) = open_stack {
        app.open_stack = Some(stack);
        app.files.clear_marks();
    }
    let now = ui.input(|i| i.time);
    app.apply_menu(menu, &batch, now);
    if let Some(path) = mark {
        app.files.toggle_mark(&path);
    }
    if let Some(path) = range {
        app.files.mark_range(&grid_order, &path);
    }
    if let Some(path) = select {
        // Starts a fresh batch of pictures; the folder range that produced this
        // grid is a separate thing and stays.
        app.files.clear_marks_like(&path);
        app.files.anchor = Some(path.clone());
        app.files.selected = path;
    }
    if open {
        app.go_to_page(PAGE_VIEWER);
    }
    ui.advance_cursor_after_rect(full);
}

// --------------------------------------------------------------------- Config

/// How each family of files is decoded, and what was done to make it fast.
///
/// Deliberately specific: every number here came from a benchmark in the repo,
/// so the page stays checkable rather than becoming marketing.
fn pipelines(ui: &mut Ui) {
    let full = ui.available_rect_before_wrap();
    ui.painter()
        .rect_filled(full, CornerRadius::ZERO, theme::WINDOW_BG);

    ui.scope_builder(
        eframe::egui::UiBuilder::new().max_rect(full.shrink(16.0)),
        |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Everything here is prose, so it has to wrap. A fixed
                    // measure also keeps the lines short enough to read.
                    let measure = ui.available_width().min(600.0);
                    ui.set_max_width(measure);

                    ui.add_space(2.0);
                    wrapped(
                        ui,
                        "What happens between a file on disk and pixels on screen.",
                        theme::heading_font(),
                        theme::TEXT,
                        measure,
                        0.0,
                    );
                    ui.add_space(12.0);

                    for pipeline in formats::PIPELINES {
                        widgets::section_label(ui, pipeline.title);

                        for step in pipeline.steps {
                            // A middle dot rather than an arrow: the bundled
                            // font has one, and a missing glyph draws as a box.
                            wrapped(
                                ui,
                                &format!("·  {step}"),
                                theme::label_font(),
                                theme::TEXT_MUTED,
                                measure,
                                4.0,
                            );
                            ui.add_space(3.0);
                        }

                        ui.add_space(5.0);
                        for (what, result) in pipeline.measured {
                            wrapped(
                                ui,
                                &format!("{what} — {result}"),
                                theme::small_caps_font(),
                                theme::TEXT,
                                measure,
                                4.0,
                            );
                            ui.add_space(2.0);
                        }

                        ui.add_space(5.0);
                        wrapped(
                            ui,
                            &format!("Still costs: {}", pipeline.limit),
                            theme::small_caps_font(),
                            theme::TEXT_DIM,
                            measure,
                            4.0,
                        );
                        ui.add_space(16.0);
                    }

                    widgets::section_label(ui, "ACROSS EVERY FORMAT");
                    for (what, detail) in formats::SHARED {
                        ui.add_space(2.0);
                        wrapped(ui, what, theme::label_font(), theme::TEXT, measure, 0.0);
                        wrapped(
                            ui,
                            detail,
                            theme::small_caps_font(),
                            theme::TEXT_MUTED,
                            measure,
                            10.0,
                        );
                        ui.add_space(10.0);
                    }

                    let (decodes, opt_in, planned) = formats::counts();
                    ui.add_space(6.0);
                    widgets::section_label(ui, "CATALOGUE");
                    widgets::kv_row(ui, "Decode today", &decodes.to_string());
                    widgets::kv_row(ui, "Behind a feature flag", &opt_in.to_string());
                    widgets::kv_row(ui, "Recognised, not decoded", &planned.to_string());
                    ui.add_space(6.0);
                    wrapped(
                        ui,
                        "The Config page lists every format and what supporting the rest \
                         would take.",
                        theme::small_caps_font(),
                        theme::TEXT_DIM,
                        measure,
                        0.0,
                    );
                    ui.add_space(16.0);
                });
        },
    );
}

/// Lays out a paragraph that wraps at `measure`, optionally indented.
///
/// `ui.label` alone would run off the side and paint over the INFO panel: egui
/// only wraps at the available width, and inside a horizontal layout that is
/// effectively infinite.
fn wrapped(
    ui: &mut Ui,
    text: &str,
    font: eframe::egui::FontId,
    colour: Color32,
    measure: f32,
    indent: f32,
) {
    let galley = ui.ctx().fonts_mut(|fonts| {
        fonts.layout(text.to_owned(), font, colour, (measure - indent).max(32.0))
    });
    let (rect, _) = ui.allocate_exact_size(vec2(measure, galley.size().y), Sense::hover());
    ui.painter()
        .galley(pos2(rect.left() + indent, rect.top()), galley, colour);
}

/// Convert, resize and crop a selection into another folder.
///
/// The page always shows what would happen before it can happen: the
/// destination of the first few files, how many already exist, and any reason
/// the job is refused. Originals are never written to — see [`crate::jobs`].
fn export(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    use crate::ops::Target;

    let full = ui.available_rect_before_wrap();
    ui.painter()
        .rect_filled(full, CornerRadius::ZERO, theme::WINDOW_BG);

    // Drain progress from a running job before drawing anything about it.
    let mut finished = None;
    if let Some(runner) = &mut app.export.runner {
        for event in runner.poll() {
            if let crate::jobs::Event::Finished {
                written,
                skipped,
                failed,
                bytes_written,
                elapsed,
                problems,
            } = &event
            {
                let mut summary = format!(
                    "{written} written, {skipped} skipped, {failed} failed — {:.1} MB in {:.1}s",
                    *bytes_written as f64 / 1e6,
                    elapsed.as_secs_f64(),
                );
                if let Some(first) = problems.first() {
                    summary.push_str(&format!("  ·  {first}"));
                }
                finished = Some(summary);
            }
        }
    }
    if let Some(summary) = finished {
        app.export.last_result = Some(summary);
        app.export.runner = None;
        // The plan is stale now that files exist that did not before.
        app.export.replan();
    }

    ui.scope_builder(
        eframe::egui::UiBuilder::new().max_rect(full.shrink(16.0)),
        |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_max_width(560.0);

                    if app.export.sources.is_empty() {
                        widgets::placeholder_note(
                            ui,
                            full.shrink(40.0),
                            "Select images in the FILES panel, then press Export.\n\
                             Ctrl-click to add, shift-click for a run, right-click for actions.",
                        );
                        return;
                    }

                    widgets::section_label(ui, "SOURCE");
                    widgets::kv_row(
                        ui,
                        "Selected",
                        &format!("{} image(s)", app.export.sources.len()),
                    );
                    if let Some(first) = app.export.sources.first() {
                        widgets::kv_row(
                            ui,
                            "First",
                            &first.file_name().unwrap_or_default().to_string_lossy(),
                        );
                    }

                    // ------------------------------------------------ output
                    widgets::section_label(ui, "OUTPUT FOLDER");
                    let chosen = app
                        .export
                        .output_dir
                        .clone()
                        .map(|dir| dir.display().to_string())
                        .unwrap_or_else(|| "not chosen".to_owned());
                    widgets::kv_row(ui, "Folder", &chosen);
                    if widgets::button(ui, "Choose folder…", 180.0, true).clicked()
                        && let Some(dir) = rfd::FileDialog::new()
                            .set_title("Where should the exported files go?")
                            .pick_folder()
                    {
                        match crate::jobs::check_output_dir(&dir) {
                            Ok(()) => {
                                app.export.output_dir = Some(dir);
                                app.export.last_result = None;
                            }
                            Err(error) => {
                                app.export.output_dir = None;
                                app.export.last_result = Some(format!("{error:#}"));
                            }
                        }
                        app.export.replan();
                    }

                    // ------------------------------------------------ format
                    widgets::section_label(ui, "FORMAT");
                    let mut target_changed = false;
                    ui.horizontal_wrapped(|ui| {
                        for target in Target::ALL {
                            let selected = app.export.recipe.target == *target;
                            if widgets::button(ui, target.label(), 92.0, !selected).clicked() {
                                app.export.recipe.target = *target;
                                target_changed = true;
                            }
                        }
                    });
                    let target = app.export.recipe.target;
                    widgets::kv_row(
                        ui,
                        "Loss",
                        if target.is_lossy() {
                            "lossy — pixels are approximated"
                        } else if target == Target::Original {
                            "none — the bytes are copied unchanged"
                        } else {
                            "lossless — pixels are preserved exactly"
                        },
                    );
                    if !target.keeps_alpha() {
                        widgets::status_line(
                            ui,
                            "Transparency will be flattened onto white.",
                            theme::TEXT_MUTED,
                        );
                    }
                    if target.is_lossy() {
                        ui.add(
                            eframe::egui::Slider::new(&mut app.export.recipe.quality, 1..=100)
                                .text("quality"),
                        );
                    }

                    // ------------------------------------------------ resize
                    widgets::section_label(ui, "RESIZE");
                    let mut resize_changed = false;
                    ui.horizontal_wrapped(|ui| {
                        for (index, label) in ["None", "Longest edge", "Percent", "Fit box"]
                            .iter()
                            .enumerate()
                        {
                            let selected = app.export.resize_mode == index;
                            if widgets::button(ui, label, 108.0, !selected).clicked() {
                                app.export.resize_mode = index;
                                resize_changed = true;
                            }
                        }
                    });
                    match app.export.resize_mode {
                        1 => {
                            resize_changed |= ui
                                .add(
                                    eframe::egui::Slider::new(
                                        &mut app.export.longest_edge,
                                        64..=8192,
                                    )
                                    .text("pixels"),
                                )
                                .changed();
                        }
                        2 => {
                            resize_changed |= ui
                                .add(
                                    eframe::egui::Slider::new(&mut app.export.percent, 1..=400)
                                        .text("%"),
                                )
                                .changed();
                        }
                        3 => {
                            resize_changed |= ui
                                .add(
                                    eframe::egui::Slider::new(&mut app.export.fit_width, 64..=8192)
                                        .text("width"),
                                )
                                .changed();
                            resize_changed |= ui
                                .add(
                                    eframe::egui::Slider::new(
                                        &mut app.export.fit_height,
                                        64..=8192,
                                    )
                                    .text("height"),
                                )
                                .changed();
                        }
                        _ => {}
                    }
                    // Show the effect on a real file rather than in the abstract.
                    if let Some(meta) = app
                        .files
                        .selected_node()
                        .and_then(|node| match &node.kind {
                            crate::files::Kind::Image(meta) => Some(meta.clone()),
                            crate::files::Kind::Folder => None,
                        })
                        .filter(|meta| meta.width > 0)
                    {
                        let (width, height) = crate::ops::resized_dimensions(
                            meta.width,
                            meta.height,
                            app.export.resize(),
                        );
                        widgets::kv_row(
                            ui,
                            "Selected image becomes",
                            &format!(
                                "{} × {} (from {} × {})",
                                width, height, meta.width, meta.height
                            ),
                        );
                    }

                    // ------------------------------------------------ naming
                    widgets::section_label(ui, "NAMING");
                    let suffix_changed = ui
                        .add(
                            eframe::egui::TextEdit::singleline(&mut app.export.suffix)
                                .hint_text("suffix, e.g. _web"),
                        )
                        .changed();

                    let overwrite_changed =
                        widgets::toggle(ui, &mut app.export.overwrite, "Overwrite existing files")
                            .clicked();
                    if app.export.overwrite {
                        widgets::status_line(
                            ui,
                            "Existing files in the output folder will be replaced.",
                            theme::CLOSE_HOVER,
                        );
                    }

                    if target_changed || resize_changed || suffix_changed || overwrite_changed {
                        app.export.replan();
                    }
                    if app.export.plan.is_none() && app.export.output_dir.is_some() {
                        app.export.replan();
                    }

                    // ------------------------------------------------- plan
                    widgets::section_label(ui, "WHAT WILL HAPPEN");
                    let mut runnable = false;
                    match &app.export.plan {
                        None => widgets::status_line(
                            ui,
                            "Choose an output folder to see the plan.",
                            theme::TEXT_DIM,
                        ),
                        Some(Err(refusal)) => {
                            widgets::status_line(ui, &refusal.to_string(), theme::CLOSE_HOVER);
                        }
                        Some(Ok(plan)) => {
                            let existing = plan.iter().filter(|item| item.exists).count();
                            if let Some(job) = app.export.job() {
                                widgets::status_line(
                                    ui,
                                    &crate::jobs::describe(plan, &job),
                                    theme::TEXT_MUTED,
                                );
                            }
                            if existing > 0 {
                                widgets::status_line(
                                    ui,
                                    &if app.export.overwrite {
                                        format!("{existing} existing file(s) will be OVERWRITTEN")
                                    } else {
                                        format!("{existing} already exist and will be skipped")
                                    },
                                    if app.export.overwrite {
                                        theme::CLOSE_HOVER
                                    } else {
                                        theme::TEXT_MUTED
                                    },
                                );
                            }
                            for item in plan.iter().take(4) {
                                widgets::kv_row(
                                    ui,
                                    &item
                                        .source
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy(),
                                    &item
                                        .destination
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy(),
                                );
                            }
                            if plan.len() > 4 {
                                widgets::status_line(
                                    ui,
                                    &format!("… and {} more", plan.len() - 4),
                                    theme::TEXT_DIM,
                                );
                            }
                            runnable = !plan.is_empty();
                        }
                    }

                    // -------------------------------------------------- run
                    ui.add_space(8.0);
                    let running = app
                        .export
                        .runner
                        .as_ref()
                        .is_some_and(|runner| runner.is_running());

                    if running {
                        if let Some(crate::jobs::Event::Progress {
                            done,
                            total,
                            current,
                        }) = app.export.runner.as_ref().and_then(|r| r.latest.clone())
                        {
                            widgets::status_line(
                                ui,
                                &format!("{done} of {total} · {current}"),
                                theme::TEXT_MUTED,
                            );
                            widgets::progress_bar(ui, Some(done as f32 / total.max(1) as f32));
                        }
                        if widgets::button(ui, "Cancel", 180.0, true).clicked()
                            && let Some(runner) = &app.export.runner
                        {
                            runner.cancel();
                        }
                    } else {
                        if widgets::button(ui, "Run export", 180.0, runnable).clicked()
                            && let (Some(job), Some(Ok(plan))) =
                                (app.export.job(), app.export.plan.clone())
                        {
                            app.export.last_result = None;
                            app.export.runner =
                                Some(crate::jobs::Runner::start(job, plan, 8, ui.ctx().clone()));
                        }
                        if let Some(result) = &app.export.last_result {
                            ui.add_space(6.0);
                            widgets::status_line(ui, result, theme::TEXT);
                        }
                    }

                    ui.add_space(10.0);
                    wrapped(
                        ui,
                        "Originals are never modified, moved or deleted. Everything is \
                         written as new files in the output folder.",
                        theme::small_caps_font(),
                        theme::TEXT_DIM,
                        520.0,
                        0.0,
                    );
                    ui.add_space(16.0);
                });
        },
    );
}

/// The Config page's groups and whether each starts expanded.
///
/// One list rather than a default at each call site, because the same defaults
/// are needed twice: when drawing the page, and when saving which groups the
/// user left folded. Titles here are the bare ones shown on screen; storage
/// prefixes them, see [`widgets::config_section_title`].
pub const CONFIG_SECTIONS: &[(&str, bool)] = &[
    ("GPU MEMORY", true),
    ("DECODING", true),
    ("THUMBNAILS", true),
    ("LIBRARY DATABASE", false),
    ("APPEARANCE", true),
    ("MOSAIC", true),
    ("IMAGE MAPS", true),
    ("MAP TILES", false),
    ("TRASH", false),
    ("INSTALL", true),
    ("DEFAULT IMAGE VIEWER", true),
    ("SUPPORTED FORMATS", false),
];

/// Whether `title` starts expanded. Unknown titles are a programming slip —
/// a group left out of [`CONFIG_SECTIONS`] would silently stop being saved.
fn config_open(title: &str) -> bool {
    match CONFIG_SECTIONS.iter().find(|(name, _)| *name == title) {
        Some((_, open)) => *open,
        None => {
            debug_assert!(false, "{title} is missing from CONFIG_SECTIONS");
            true
        }
    }
}

/// Megabytes as GB with one decimal. The sliders run into the thousands, where
/// four digits of MB are harder to compare at a glance than "1.5 GB".
fn gb(megabytes: usize) -> String {
    format!("{:.1} GB", megabytes as f32 / 1024.0)
}

fn config(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let full = ui.available_rect_before_wrap();
    ui.painter()
        .rect_filled(full, CornerRadius::ZERO, theme::WINDOW_BG);

    ui.scope_builder(
        eframe::egui::UiBuilder::new().max_rect(full.shrink(16.0)),
        |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_max_width(560.0);

                    widgets::collapsing_card(ui, "GPU MEMORY", config_open("GPU MEMORY"), |ui| {
                        ui.spacing_mut().slider_width = 220.0;
                        widgets::kv_row(
                            ui,
                            "Full-resolution images",
                            &gb(app.config.image_vram_mb),
                        );
                        ui.add(
                            eframe::egui::Slider::new(&mut app.config.image_vram_mb, 128..=8192)
                                .show_value(false),
                        );
                        widgets::kv_row(ui, "Thumbnails", &gb(app.config.thumbnail_vram_mb));
                        ui.add(
                            eframe::egui::Slider::new(&mut app.config.thumbnail_vram_mb, 64..=4096)
                                .show_value(false),
                        );
                        widgets::kv_row(
                            ui,
                            "Combined ceiling",
                            &gb(app.config.image_vram_mb + app.config.thumbnail_vram_mb),
                        );
                    });

                    widgets::collapsing_card(ui, "DECODING", config_open("DECODING"), |ui| {
                        widgets::toggle(
                            ui,
                            &mut app.config.preload_neighbours,
                            "Preload neighbouring images",
                        );
                        widgets::toggle(
                            ui,
                            &mut app.config.use_gpu_decode,
                            "GPU decoding (nvJPEG)",
                        );
                        ui.spacing_mut().slider_width = 220.0;
                        widgets::kv_row(
                            ui,
                            "Decode threads",
                            &app.config.decode_threads.to_string(),
                        );
                        ui.add(
                            eframe::egui::Slider::new(&mut app.config.decode_threads, 1..=32)
                                .show_value(false),
                        );
                    });

                    let sweep_toggled = widgets::collapsing_card(
                        ui,
                        "THUMBNAILS",
                        config_open("THUMBNAILS"),
                        |ui| {
                            let toggled = widgets::toggle(
                                ui,
                                &mut app.config.background_thumbs,
                                "Generate thumbnails in the background",
                            )
                            .clicked();
                            widgets::toggle(
                                ui,
                                &mut app.config.recursive_index,
                                "Index subfolders recursively",
                            );
                            ui.spacing_mut().slider_width = 220.0;
                            widgets::kv_row(
                                ui,
                                "Worker threads",
                                &app.config.thumbnail_threads.to_string(),
                            );
                            ui.add(
                                eframe::egui::Slider::new(
                                    &mut app.config.thumbnail_threads,
                                    1..=crate::thumbs::MAX_THUMB_WORKERS,
                                )
                                .show_value(false),
                            );
                            widgets::kv_row(
                                ui,
                                "Thumbnail size",
                                &format!("{} px", crate::thumbs::THUMB_EDGE),
                            );
                            widgets::kv_row(ui, "Stored in", "SQLite (BLOB, JPEG or PNG)");
                            toggled
                        },
                    );
                    // `None` when the group is folded away, so nothing to do.
                    if sweep_toggled == Some(true)
                        && let Some(thumbs) = &mut app.thumbs
                    {
                        if app.config.background_thumbs {
                            thumbs.start_sweep();
                        } else {
                            thumbs.stop_sweep();
                        }
                    }

                    widgets::collapsing_card(
                        ui,
                        "LIBRARY DATABASE",
                        config_open("LIBRARY DATABASE"),
                        |ui| {
                            widgets::kv_row(ui, "Engine", "SQLite · WAL");
                            widgets::kv_row(ui, "Location", "~/.local/share/geo-ref_mapper/library.db");
                            widgets::kv_row(ui, "Indexed images", &app.stats.images.to_string());
                            // The file's size is not shown either: it manages
                            // its own space now, and a number nobody can act on
                            // is a number nobody needs.
                            widgets::kv_row(
                                ui,
                                "Size",
                                &crate::files::format_bytes(
                                    app.library
                                        .as_ref()
                                        .and_then(|library| std::fs::metadata(library.path()).ok())
                                        .map(|meta| meta.len())
                                        .unwrap_or(0),
                                ),
                            );
                        },
                    );

                    widgets::collapsing_card(ui, "APPEARANCE", config_open("APPEARANCE"), |ui| {
                        widgets::toggle(
                            ui,
                            &mut app.config.checkerboard,
                            "Checkerboard behind transparency",
                        );
                        widgets::toggle(
                            ui,
                            &mut app.config.show_filmstrip,
                            "Show filmstrip in Viewer",
                        );
                        // Off by default: it reports on the program rather than
                        // on the pictures, so for anyone using the program it
                        // is a tab in the way of the ones that answer things.
                        widgets::toggle(
                            ui,
                            &mut app.config.show_debug_tab,
                            "Show DEBUG tab in INFO panel",
                        );

                        ui.add_space(8.0);
                        interface_scale(ui, app);
                    });

                    widgets::collapsing_card(ui, "MOSAIC", config_open("MOSAIC"), |ui| {
                        widgets::toggle(
                            ui,
                            &mut app.config.mosaic_recursive,
                            "Include subfolders (Mosaic and filmstrip)",
                        );
                        widgets::toggle(
                            ui,
                            &mut app.config.mosaic_filenames,
                            "Show filenames on tiles",
                        );
                        widgets::kv_row(
                            ui,
                            "Tile size",
                            &format!("{:.0} px", app.mosaic_tile_size),
                        );
                        widgets::kv_row(ui, "Zoom", "Ctrl+wheel  ·  wheel scrolls");
                    });

                    image_maps(ui, app);
                    // Only meaningful while there is a basemap to fetch tiles
                    // for. The switch above stays put so there is always a way
                    // back.
                    if app.config.map_support {
                        map_tiles(ui, app);
                    }
                    trash_bin(ui, app);
                    install_app(ui);
                    default_application(ui);
                    supported_formats(ui);
                    ui.add_space(16.0);
                });
        },
    );
    ui.advance_cursor_after_rect(full);
}

/// The one switch deciding whether this is a map program as well as an image
/// viewer.
///
/// It is a whole group of its own rather than a line in APPEARANCE, because it
/// does not change how anything looks: it takes two pages, two INFO tabs, an
/// entry in both right-click menus, three keys and a background pass over the
/// library out of the program together. Nothing else here is that large.
///
/// The group itself is always drawn, switch on or off — a setting you can only
/// find while it is on is a setting you cannot undo.
fn image_maps(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let changed = widgets::collapsing_card(ui, "IMAGE MAPS", config_open("IMAGE MAPS"), |ui| {
        let changed = widgets::toggle(ui, &mut app.config.map_support, "Image map support")
            .on_hover_text(
                "The Map and Map Images pages, placing pictures on the basemap, \
                 and converting maps into tiles.",
            )
            .changed();
        ui.add_space(4.0);
        wrapped(
            ui,
            "Off, this is an image viewer: the Map and Map Images pages go from \
             the title bar, the MAP and IMAGES tabs from the INFO panel, \u{201c}Add \
             to map images\u{201d} from both right-click menus, and the background \
             pass that opens every TIFF looking for a position stops running. \
             Nothing on disk is touched \u{2014} the queue, the anchors and every map \
             already converted are still there when it goes back on.",
            theme::small_caps_font(),
            theme::TEXT_DIM,
            ui.available_width(),
            0.0,
        );
        changed
    });
    // `None` while the group is folded away, so there is nothing to react to.
    if changed == Some(true) {
        app.map_support_changed(ui.ctx());
    }
}

/// Where map tiles are kept, and who the tile server is told we are.
///
/// The OpenStreetMap tile policy asks for a User-Agent that names the
/// application *and* carries contact information. Without one the server
/// serves a "blocked" tile instead of a map, which is the polite version of
/// being cut off — so the field is here rather than buried in a config file.
fn map_tiles(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let id = ui.id().with("tile_cache_size");
    let mut size: Option<(u64, usize)> = ui.ctx().data_mut(|data| data.get_temp(id));

    widgets::collapsing_card(ui, "MAP TILES", config_open("MAP TILES"), |ui| {
        wrapped(
            ui,
            "An email address or project URL. The tile server needs a way to              reach whoever is making the requests; without one it refuses them.",
            theme::small_caps_font(),
            theme::TEXT_DIM,
            ui.available_width(),
            0.0,
        );
        ui.add_space(4.0);
        ui.add(
            eframe::egui::TextEdit::singleline(&mut app.config.tile_contact)
                .hint_text("you@example.com")
                .desired_width(ui.available_width()),
        );
        widgets::kv_block(
            ui,
            "Sent as",
            &crate::map::user_agent(&app.config.tile_contact),
        );

        ui.add_space(8.0);
        let folder = crate::map::cache_dir();
        widgets::kv_block(
            ui,
            "Cached in",
            &folder
                .as_ref()
                .map(|path| tilde(path))
                .unwrap_or_else(|| "—".to_owned()),
        );
        if size.is_none()
            && let Some(folder) = &folder
        {
            size = Some(directory_size(folder));
        }
        if let Some((bytes, count)) = size {
            widgets::kv_row(
                ui,
                "Held",
                &format!("{count} tiles · {}", crate::files::format_bytes(bytes)),
            );
        }

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if widgets::button(ui, "Empty cache", 140.0, size.is_some_and(|(_, n)| n > 0))
                .on_hover_text("Tiles are re-fetched as needed")
                .clicked()
                && let Some(folder) = &folder
            {
                let _ = std::fs::remove_dir_all(folder);
                size = Some((0, 0));
            }
            ui.add_space(8.0);
            if widgets::button(ui, "Retry blocked", 140.0, true)
                .on_hover_text("Ask again for tiles that were refused")
                .clicked()
                && let Some(tiles) = &mut app.tiles
            {
                tiles.retry_failed();
            }
        });
    });

    ui.ctx().data_mut(|data| data.insert_temp(id, size));
}

/// Bytes and file count under a folder, one level deep — which is all the tile
/// cache is.
fn directory_size(folder: &std::path::Path) -> (u64, usize) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return (0, 0);
    };
    entries
        .flatten()
        // Tiles only — the folder also holds the stamp that says which build
        // wrote it, and counting that as a tile would be a small lie.
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "png"))
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .fold((0, 0), |(bytes, count), meta| {
            (bytes + meta.len(), count + 1)
        })
}

/// Images sitting in the desktop trash, with a way to put each one back.
///
/// Only images, and only their names and where they came from: the trash is
/// shared with the whole desktop, and listing someone's other deleted files
/// here would be neither useful nor this application's business.
fn trash_bin(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let id = ui.id().with("trash_bin");
    // Read once and kept, not re-run every frame: it shells out to `gio`, and
    // the panel is redrawn sixty times a second.
    let mut entries: Vec<crate::desktop::Trashed> = ui
        .ctx()
        .data_mut(|data| data.get_temp(id))
        .unwrap_or_else(crate::desktop::trashed_images);

    let mut restore: Option<crate::desktop::Trashed> = None;
    let mut refresh = false;

    widgets::collapsing_card(ui, "TRASH", config_open("TRASH"), |ui| {
        widgets::kv_row(ui, "Images in the trash", &entries.len().to_string());
        if widgets::button(ui, "Refresh", 120.0, true).clicked() {
            refresh = true;
        }
        ui.add_space(6.0);

        if entries.is_empty() {
            wrapped(
                ui,
                "Nothing of ours is in the trash.",
                theme::small_caps_font(),
                theme::TEXT_DIM,
                ui.available_width(),
                0.0,
            );
            return;
        }

        // Newest first is what people look for, and `gio` lists oldest first.
        for entry in entries.iter().rev().take(30) {
            widgets::kv_block(ui, &entry.name(), &entry.original.display().to_string());
            if widgets::button(ui, "Restore", 110.0, true)
                .on_hover_text("Put it back where it came from")
                .clicked()
            {
                restore = Some(entry.clone());
            }
            ui.add_space(8.0);
        }
        if entries.len() > 30 {
            wrapped(
                ui,
                &format!("{} older ones not shown", entries.len() - 30),
                theme::small_caps_font(),
                theme::TEXT_DIM,
                ui.available_width(),
                0.0,
            );
        }
    });

    if let Some(entry) = restore {
        app.toast = Some(match crate::desktop::restore_from_trash(&entry) {
            Ok(()) => {
                // The file is back, so the library should know about it again.
                app.start_rescan_of(&entry.original);
                refresh = true;
                (format!("Restored {}", entry.name()), ui.input(|i| i.time))
            }
            Err(error) => (format!("{error}"), ui.input(|i| i.time)),
        });
    }
    if refresh {
        entries = crate::desktop::trashed_images();
    }
    ui.ctx().data_mut(|data| data.insert_temp(id, entries));
}

/// What the last install / remove attempt did, kept between frames.
#[derive(Clone, Default)]
struct DefaultAppState {
    /// `Some(true)` while asking to register, `Some(false)` while asking to
    /// hand the formats back, `None` when no question is on screen.
    confirming: Option<bool>,
    message: Option<(String, bool)>,
}

/// How large the interface is drawn.
///
/// The slider does not write the setting. It writes `pending_ui_scale`, and the
/// setting is taken from that when the pointer comes up — see
/// `GeoRefMapperApp::apply_ui_scale`. Applying it live would rescale the slider
/// while it is being dragged: the track lengthens, the handle slides out from
/// under the pointer, and the reading chases a value that is moving because it
/// is being read. The number above updates all the way through, so the drag
/// still says where it will land.
fn interface_scale(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let held = app.pending_ui_scale;
    let mut scale = held.unwrap_or(app.config.ui_scale);

    widgets::kv_row(
        ui,
        "Interface scale",
        &match held {
            Some(_) => format!("{:.0}%  ·  on release", scale * 100.0),
            None => format!("{:.0}%", scale * 100.0),
        },
    );

    ui.spacing_mut().slider_width = 220.0;
    let response = ui.add(
        eframe::egui::Slider::new(
            &mut scale,
            crate::app::MIN_UI_SCALE..=crate::app::MAX_UI_SCALE,
        )
        .step_by(0.05)
        .show_value(false),
    );

    if response.changed() || response.dragged() {
        // Held rather than applied, whatever moved it. A slider can also be
        // walked with the arrow keys once it has focus, and that never has a
        // pointer down — so the release check above lets it through on the
        // same frame, which is the immediate response a key press wants.
        app.pending_ui_scale = Some(scale);
    }

    wrapped(
        ui,
        "Text and everything holding it \u{2014} rows, panels, the title bar \u{2014} \
         together, and applied when you let go of the slider. The picture is not \
         scaled: it keeps the screen it had, with less of the window left around \
         it. Ctrl+plus and Ctrl+minus move this too, and Ctrl+0 puts it back to \
         100%.",
        theme::small_caps_font(),
        theme::TEXT_DIM,
        ui.available_width(),
        0.0,
    );
}

/// Putting the program into `~/.local` so the desktop can list it.
///
/// Sits immediately above the default-viewer group because the two are read
/// together and are easily confused: this one is "the menu knows geo-ref_mapper
/// exists and has its icon", that one is "double-clicking a JPEG opens it".
/// Neither needs root and neither needs the other.
fn install_app(ui: &mut Ui) {
    let id = ui.id().with("install_app");
    let mut state: DefaultAppState = ui
        .ctx()
        .data_mut(|data| data.get_temp(id))
        .unwrap_or_default();

    let supported = crate::desktop::supported();
    let installed = crate::install::is_installed();
    let running_installed = crate::install::running_from_install();
    let stale = installed && crate::install::update_available();

    widgets::collapsing_card(ui, "INSTALL", config_open("INSTALL"), |ui| {
        widgets::kv_row(
            ui,
            "Status",
            &if !supported {
                "Only available on Linux desktops".to_owned()
            } else if stale {
                "Installed — older than this build".to_owned()
            } else if installed && running_installed {
                "Installed — this is the installed copy".to_owned()
            } else if installed {
                "Installed — running a different copy".to_owned()
            } else {
                "Not installed".to_owned()
            },
        );
        widgets::kv_row(
            ui,
            "Program",
            &crate::install::installed_exe()
                .map(|path| tilde(&path))
                .unwrap_or_else(|| "—".to_owned()),
        );
        widgets::kv_row(ui, "This build", &crate::version::full_version());
        if installed {
            widgets::kv_row(
                ui,
                "Installed",
                &crate::install::installed_at()
                    .map(crate::version::stamp_of)
                    .unwrap_or_else(|| "unknown".to_owned()),
            );
        }
        widgets::kv_row(
            ui,
            "Icon",
            &crate::install::icons_dir()
                .map(|dir| {
                    format!(
                        "{}  ·  {} sizes",
                        tilde(&dir.join("hicolor")),
                        crate::install::ICON_SIZES.len()
                    )
                })
                .unwrap_or_else(|| "—".to_owned()),
        );
        widgets::kv_row(
            ui,
            "Menu entry",
            &crate::desktop::desktop_file()
                .map(|path| tilde(&path))
                .unwrap_or_else(|| "—".to_owned()),
        );

        // The same thing the startup dialog offers, for anyone who said "not
        // now" and came looking for it afterwards.
        if stale {
            ui.add_space(6.0);
            widgets::status_line(
                ui,
                "The menu entry still starts the older copy. Install again to update it.",
                theme::WARNING,
            );
        }

        // Only once installed, and only when it is actually missing: the
        // launcher entry carries an absolute path and works regardless, so
        // this is about typing the name at a prompt and nothing else.
        if installed && !crate::install::on_path() {
            ui.add_space(6.0);
            wrapped(
                ui,
                "The menu entry works, but ~/.local/bin is not on your PATH, so \
                 typing \u{201c}geo-ref_mapper\u{201d} in a terminal will not find it. Most \
                 shells add that directory from ~/.profile at login.",
                theme::small_caps_font(),
                theme::TEXT_DIM,
                ui.available_width(),
                0.0,
            );
        }

        ui.add_space(8.0);
        match state.confirming {
            None => {
                // Updating is offered before removing when the installed copy
                // is out of date: it is what someone reading the warning above
                // has come here to do.
                if stale && widgets::button(ui, "Update the installation", 220.0, true).clicked() {
                    state.confirming = Some(true);
                    state.message = None;
                }
                let label = if installed {
                    "Remove the installation"
                } else {
                    "Install for this user"
                };
                if widgets::button(ui, label, 220.0, supported).clicked() {
                    state.confirming = Some(!installed);
                    state.message = None;
                }
            }
            Some(install) => {
                let question = if install {
                    let where_to = crate::install::installed_exe()
                        .map(|path| tilde(&path))
                        .unwrap_or_else(|| "~/.local/bin".to_owned());
                    format!(
                        "Copy this program to {where_to}, write its icon into your \
                         icon theme, and add it to the menu? Everything goes under \
                         your home directory — nothing is installed system-wide and \
                         nothing asks for a password."
                    )
                } else if running_installed {
                    "Remove the program, its icons and its menu entry? This window \
                     stays open and keeps working, but geo-ref_mapper will not start \
                     again until it is installed or run from a build."
                        .to_owned()
                } else {
                    "Remove the installed program, its icons and its menu entry? \
                     The copy running now is somewhere else and is not touched."
                        .to_owned()
                };
                let measure = ui.available_width();
                wrapped(
                    ui,
                    &question,
                    theme::label_font(),
                    theme::TEXT_MUTED,
                    measure,
                    0.0,
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let confirm = if install {
                        "Yes, install"
                    } else {
                        "Yes, remove"
                    };
                    if widgets::button(ui, confirm, 130.0, true).clicked() {
                        state.confirming = None;
                        state.message = Some(if install {
                            match crate::install::install() {
                                Ok(report) => (install_summary(&report), true),
                                Err(error) => (format!("Could not install — {error}"), false),
                            }
                        } else {
                            match crate::install::uninstall() {
                                Ok(()) => ("Removed from ~/.local".to_owned(), true),
                                Err(error) => (format!("Could not remove — {error}"), false),
                            }
                        });
                    }
                    if widgets::button(ui, "Cancel", 90.0, true).clicked() {
                        state.confirming = None;
                    }
                });
            }
        }

        if let Some((message, ok)) = &state.message {
            ui.add_space(6.0);
            widgets::status_line(
                ui,
                message,
                if *ok {
                    theme::TEXT_MUTED
                } else {
                    theme::WARNING
                },
            );
        }
    });

    ui.ctx().data_mut(|data| data.insert_temp(id, state));
}

/// What an install did, in one line.
fn install_summary(report: &crate::install::Report) -> String {
    let what = if report.refreshed {
        "Icons and menu entry rewritten"
    } else {
        "Installed"
    };
    let icons = report.icons;
    let path = tilde(&report.exe);
    if report.on_path {
        format!("{what} — {path}, {icons} icon sizes")
    } else {
        format!("{what} — {path}, {icons} icon sizes  ·  not on PATH")
    }
}

/// Registering geo-ref_mapper as the system handler for the formats it decodes.
///
/// This writes outside the application's own directory, so it asks first and
/// names both files it will touch. Nothing happens without the second click.
fn default_application(ui: &mut Ui) {
    let id = ui.id().with("default_application");
    let mut state: DefaultAppState = ui
        .ctx()
        .data_mut(|data| data.get_temp(id))
        .unwrap_or_default();

    let registered = crate::desktop::registered_count();
    let total = crate::desktop::ASSOCIATIONS.len();
    let installed = crate::desktop::installed();

    widgets::collapsing_card(
        ui,
        "DEFAULT IMAGE VIEWER",
        config_open("DEFAULT IMAGE VIEWER"),
        |ui| {
            widgets::kv_row(
                ui,
                "Status",
                &if !crate::desktop::supported() {
                    "Only available on Linux desktops".to_owned()
                } else if installed {
                    format!("Handling {registered} of {total} types")
                } else {
                    "Not registered".to_owned()
                },
            );
            widgets::kv_row(
                ui,
                "Launcher entry",
                &crate::desktop::desktop_file()
                    .map(|path| tilde(&path))
                    .unwrap_or_else(|| "—".to_owned()),
            );
            widgets::kv_row(
                ui,
                "Defaults list",
                &crate::desktop::mimeapps_file()
                    .map(|path| tilde(&path))
                    .unwrap_or_else(|| "—".to_owned()),
            );

            ui.add_space(8.0);
            match state.confirming {
                None => {
                    let label = if installed {
                        "Restore previous handlers"
                    } else {
                        "Set geo-ref_mapper as the default"
                    };
                    if widgets::button(ui, label, 220.0, crate::desktop::supported()).clicked() {
                        state.confirming = Some(!installed);
                        state.message = None;
                    }
                }
                Some(install) => {
                    let question = if install {
                        format!(
                            "Write the launcher entry and claim {total} image types? \
                         Whatever handles them now is remembered in the same file \
                         and comes back if you undo this."
                        )
                    } else {
                        "Delete the launcher entry and give every type back to \
                     whatever handled it before?"
                            .to_owned()
                    };
                    let measure = ui.available_width();
                    wrapped(
                        ui,
                        &question,
                        theme::label_font(),
                        theme::TEXT_MUTED,
                        measure,
                        0.0,
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let confirm = if install {
                            "Yes, register"
                        } else {
                            "Yes, undo"
                        };
                        if widgets::button(ui, confirm, 130.0, true).clicked() {
                            state.confirming = None;
                            state.message = Some(if install {
                                match crate::desktop::install() {
                                    Ok(count) => {
                                        (format!("Registered for {count} image types"), true)
                                    }
                                    Err(error) => (format!("Could not register — {error}"), false),
                                }
                            } else {
                                match crate::desktop::remove() {
                                    Ok(()) => ("Handlers restored".to_owned(), true),
                                    Err(error) => (format!("Could not undo — {error}"), false),
                                }
                            });
                        }
                        if widgets::button(ui, "Cancel", 90.0, true).clicked() {
                            state.confirming = None;
                        }
                    });
                }
            }

            if let Some((message, ok)) = &state.message {
                ui.add_space(6.0);
                widgets::status_line(
                    ui,
                    message,
                    if *ok {
                        theme::TEXT_MUTED
                    } else {
                        theme::WARNING
                    },
                );
            }
        },
    );

    ui.ctx().data_mut(|data| data.insert_temp(id, state));
}

/// `/home/morgan/…` reads as noise in a settings row; `~/…` does not.
fn tilde(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => text
            .strip_prefix(&home)
            .map_or(text.clone(), |rest| format!("~{rest}")),
        _ => text,
    }
}

/// The image formats this build understands, and the ones it does not.
fn supported_formats(ui: &mut Ui) {
    let (decodes, opt_in, planned) = formats::counts();

    widgets::collapsing_card(
        ui,
        "SUPPORTED FORMATS",
        config_open("SUPPORTED FORMATS"),
        |ui| {
            // Allocated rather than painted at a hand-offset cursor, so the line
            // below it starts where this one actually ends.
            let (rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
            ui.painter().text(
                rect.left_center(),
                Align2::LEFT_CENTER,
                format!(
                    "{decodes} decode now · {opt_in} opt-in · {planned} planned · still images only"
                ),
                theme::small_caps_font(),
                theme::TEXT_DIM,
            );

            for group in formats::GROUPS {
                widgets::section_label(ui, group.title);
                for format in group.formats {
                    // Two stacked lines: 6 + 13pt name + 4 + 11pt detail + 6. The
                    // old flat 34 was six points short of that, so the name and the
                    // detail under it were drawn through each other.
                    let height =
                        6.0 + theme::label_font().size + 4.0 + theme::small_caps_font().size + 6.0;
                    let (rect, response) =
                        ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
                    if response.hovered() {
                        ui.painter()
                            .rect_filled(rect, CornerRadius::same(5), theme::SURFACE_HOVER);
                    }

                    let painter = ui.painter();
                    painter.text(
                        pos2(rect.left() + 6.0, rect.top() + 6.0),
                        Align2::LEFT_TOP,
                        format.name,
                        theme::label_font(),
                        theme::TEXT,
                    );
                    painter.text(
                        pos2(rect.left() + 6.0, rect.bottom() - 6.0),
                        Align2::LEFT_BOTTOM,
                        format.detail,
                        theme::small_caps_font(),
                        theme::TEXT_DIM,
                    );
                    // The status dot is painted rather than typed: the filled and
                    // hollow bullet glyphs are not in the bundled font and came out
                    // as empty boxes.
                    let colour = status_colour(format.status);
                    let label = painter.text(
                        pos2(rect.right() - 6.0, rect.top() + 6.0),
                        Align2::RIGHT_TOP,
                        format.status.label(),
                        theme::small_caps_font(),
                        colour,
                    );
                    status_dot(
                        painter,
                        pos2(label.left() - 9.0, label.center().y),
                        format.status,
                        colour,
                    );
                    painter.text(
                        pos2(rect.right() - 6.0, rect.bottom() - 6.0),
                        Align2::RIGHT_BOTTOM,
                        format.extensions,
                        theme::small_caps_font(),
                        theme::TEXT_MUTED,
                    );
                }
            }
        },
    );
}

/// Filled for "decodes", half for "opt-in", hollow for "planned" — the same
/// three states the bullet glyphs used to stand for, drawn instead of typed.
fn status_dot(
    painter: &eframe::egui::Painter,
    centre: eframe::egui::Pos2,
    status: formats::Status,
    colour: Color32,
) {
    const RADIUS: f32 = 3.5;
    match status {
        formats::Status::Decodes => {
            painter.circle_filled(centre, RADIUS, colour);
        }
        formats::Status::NeedsFeature => {
            painter.circle_stroke(centre, RADIUS, Stroke::new(1.0, colour));
            // Left half filled, as a half-full bullet would be.
            painter.rect_filled(
                Rect::from_min_max(
                    pos2(centre.x - RADIUS, centre.y - RADIUS),
                    pos2(centre.x, centre.y + RADIUS),
                ),
                CornerRadius::ZERO,
                colour,
            );
        }
        formats::Status::Planned => {
            painter.circle_stroke(centre, RADIUS, Stroke::new(1.0, colour));
        }
    }
}

fn status_colour(status: formats::Status) -> Color32 {
    match status {
        formats::Status::Decodes => Color32::from_rgb(0x5d, 0xc2, 0x8a),
        formats::Status::NeedsFeature => Color32::from_rgb(0xd8, 0xa5, 0x4a),
        formats::Status::Planned => theme::TEXT_DIM,
    }
}

/// Draws a texture centred in `rect`, scaled to fit without distorting it.
fn draw_contained(ui: &Ui, rect: Rect, texture: eframe::egui::TextureId, size: [usize; 2]) {
    let aspect = size[0] as f32 / size[1].max(1) as f32;
    let fitted = if aspect >= rect.width() / rect.height() {
        vec2(rect.width(), rect.width() / aspect)
    } else {
        vec2(rect.height() * aspect, rect.height())
    };

    ui.painter()
        .rect_filled(rect, CornerRadius::same(4), theme::CANVAS_BG);
    ui.painter().image(
        texture,
        Rect::from_center_size(rect.center(), fitted),
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
        Color32::WHITE,
    );
}

/// Draws the stored thumbnail for an indexed image, requesting it if it is not
/// resident yet. Returns false when there is nothing to draw.
/// Draws one tile's thumbnail. `small` picks the 48 px texture, which is what
/// a tile only a few points across can actually show.
fn draw_thumbnail(
    ui: &Ui,
    app: &mut GeoRefMapperApp,
    rect: Rect,
    db_id: Option<i64>,
    small: bool,
) -> bool {
    let Some(texture) = app.thumbnail(db_id, small) else {
        return false;
    };
    // The aspect ratio comes from whichever cache actually answered, so a tile
    // falling back to the full texture is not stretched to the other's shape.
    let size = db_id
        .and_then(|id| {
            let mini = small
                .then(|| app.mini_textures.get(id).map(|h| h.size()))
                .flatten();
            mini.or_else(|| app.thumb_textures.get(id).map(|h| h.size()))
        })
        .unwrap_or([1, 1]);
    draw_contained(ui, rect, texture, size);
    true
}

/// The count on a collapsed stack.
fn stack_badge(ui: &Ui, tile: Rect, count: usize) {
    let text = count.to_string();
    let font = theme::small_caps_font();
    let width = ui
        .ctx()
        .fonts_mut(|f| f.layout_no_wrap(text.clone(), font.clone(), theme::TEXT))
        .size()
        .x;
    let size = vec2((width + 12.0).max(20.0), 18.0);
    let badge = Rect::from_min_size(pos2(tile.right() - size.x - 4.0, tile.top() + 4.0), size);

    ui.painter()
        .rect_filled(badge, CornerRadius::same(4), Color32::from_black_alpha(190));
    ui.painter().rect_stroke(
        badge,
        CornerRadius::same(4),
        Stroke::new(1.0, theme::TEXT_MUTED),
        StrokeKind::Inside,
    );
    ui.painter().text(
        badge.center(),
        Align2::CENTER_CENTER,
        text,
        font,
        theme::TEXT,
    );
}

/// Blue for a folder in the viewing scope, purple for a picture in the batch.
pub fn mark_colour(mark: crate::files::Mark) -> Color32 {
    match mark {
        crate::files::Mark::Folder => theme::ACCENT,
        crate::files::Mark::Image => theme::MARKED,
    }
}

/// Why a tile has no picture in it yet.
///
/// One reading of the whole app's state rather than a lookup per tile: the
/// answer is the same for every tile on screen, and a database round trip for
/// each of three thousand of them would cost far more than the message is
/// worth.
fn placeholder_note_text(app: &GeoRefMapperApp) -> &'static str {
    if !app.config.background_thumbs {
        "Thumbnails off"
    } else if app.thumbs.as_ref().is_some_and(|thumbs| thumbs.sweeping) {
        "Building thumbnail…"
    } else {
        "No preview"
    }
}

/// A tile with nothing to show yet.
///
/// A flat surface with a word on it, rather than a colour derived from the
/// filename. The coloured squares carried no information — the hue meant
/// nothing — and read as pictures that had loaded wrongly rather than as ones
/// that had not loaded at all.
/// A small green mark in a tile's top-left corner: this picture says where on
/// the earth it is.
///
/// The Mosaic already said so by tinting the filename, which is no use with
/// filenames turned off — and on the rail there was nowhere it was said at all.
/// A corner mark is legible at any tile size and does not touch the picture.
///
/// Drawn with a dark ring so it reads against a pale map as well as a dark one;
/// these are scans of paper, and most of them are pale.
fn geo_badge(painter: &eframe::egui::Painter, rect: Rect) {
    let at = rect.left_top() + vec2(9.0, 9.0);
    painter.circle_filled(at, 5.5, Color32::from_black_alpha(120));
    painter.circle_filled(at, 4.0, theme::GEOREFERENCED);
}

fn placeholder_tile(ui: &Ui, rect: Rect, corner: CornerRadius, message: &str) {
    ui.painter().rect_filled(rect, corner, theme::SURFACE);
    ui.painter().rect_stroke(
        rect,
        corner,
        Stroke::new(1.0, theme::HAIRLINE),
        StrokeKind::Inside,
    );
    // Below this the text would be unreadable anyway, and a grid of tiny tiles
    // full of clipped words is worse than a grid of plain ones.
    if rect.width() < 90.0 || rect.height() < 44.0 {
        return;
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        widgets::elide(ui, message, theme::small_caps_font(), rect.width() - 12.0),
        theme::small_caps_font(),
        theme::TEXT_DIM,
    );
}
