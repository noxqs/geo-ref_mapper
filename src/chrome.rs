//! The custom window chrome: title bar, window buttons and edge resize grips.
//!
//! The native decorations are turned off in `main`, so everything the OS would
//! normally draw is painted here instead.

use eframe::egui::{
    Align, Align2, Area, Color32, CornerRadius, CursorIcon, Id, Layout, Order, Rect, Response,
    Sense, Ui, UiBuilder, ViewportCommand, pos2, vec2, viewport::ResizeDirection,
};

use crate::app::{GeoRefMapperApp, PAGE_LABELS};
use crate::{icons, pill, theme};

const BUTTON_SIZE: egui::Vec2 = egui::Vec2::new(34.0, 26.0);
const TOGGLE_SIZE: egui::Vec2 = egui::Vec2::new(28.0, 26.0);

use eframe::egui;

/// Set when the app itself asked the window to close — the title bar's close
/// button, or Escape out of a picture-only launch — so an unexplained shutdown
/// can be told apart from a deliberate one.
pub static CLOSE_REQUESTED_BY_BUTTON: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn title_bar(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let bar = ui.max_rect();
    let painter = ui.painter();
    painter.rect_filled(bar, CornerRadius::ZERO, theme::TITLE_BAR);
    painter.line_segment([bar.left_bottom(), bar.right_bottom()], theme::hairline());

    // Registered before the buttons so that later widgets win the interaction;
    // anything not covered by a control drags or maximizes the window.
    let drag_zone = ui.interact(bar, ui.id().with("title_drag"), Sense::click_and_drag());
    if drag_zone.double_clicked() {
        toggle_maximized(ui.ctx());
    } else if drag_zone.drag_started() {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }

    // Our own texture footprint, so the readout can distinguish what geo-ref_mapper
    // holds from what everything else on the card does.
    let ours = app.textures.used() + app.thumb_textures.used();
    // Decoded once, the first time the badge is drawn.
    if app.vendor_logo.is_none()
        && let Some(bytes) = app.renderer.as_ref().and_then(|r| r.logo())
        && let Ok(decoded) = image::load_from_memory(bytes)
    {
        let rgba = decoded.to_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        app.vendor_logo = Some(ui.ctx().load_texture(
            "vendor_logo",
            egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()),
            egui::TextureOptions::LINEAR,
        ));
    }
    let left_edge = identity(
        ui,
        bar,
        app.renderer.as_ref(),
        app.vram,
        ours,
        app.vendor_logo.clone(),
    );
    let right_edge = window_controls(ui, bar, app);
    page_pill(ui, bar, left_edge, right_edge, app);
}

/// App mark, name and version, pinned to the left.
///
/// Returns the right edge of the whole cluster, so the pill can be centred
/// without running into it. Measured rather than assumed: the version now
/// carries a build date, which made the fixed allowance this used to have too
/// small — and the pill quietly overlapped the renderer badge.
fn identity(
    ui: &mut Ui,
    bar: Rect,
    renderer: Option<&crate::renderer::Choice>,
    vram: Option<crate::vram::Memory>,
    ours: usize,
    logo: Option<egui::TextureHandle>,
) -> f32 {
    let rect = Rect::from_min_max(
        bar.left_top() + vec2(12.0, 0.0),
        bar.left_bottom() + vec2(280.0, 0.0),
    );
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            let (mark, _) = ui.allocate_exact_size(egui::Vec2::splat(18.0), Sense::hover());
            icons::app_mark(ui.painter(), mark, theme::ACCENT);

            ui.add_space(2.0);
            ui.painter().text(
                pos2(ui.cursor().left(), bar.center().y),
                Align2::LEFT_CENTER,
                crate::app::APP_NAME,
                theme::heading_font(),
                theme::TEXT,
            );

            let name_width = ui
                .ctx()
                .fonts_mut(|f| {
                    f.layout_no_wrap(
                        crate::app::APP_NAME.to_owned(),
                        theme::heading_font(),
                        theme::TEXT,
                    )
                })
                .size()
                .x;
            // No version here. Config → INSTALL carries it in full, to the
            // minute, with the installed copy's stamp beside it — which is the
            // only place the two are ever read together. On the title bar it
            // was a string nobody consults, competing for the one row the page
            // pill also needs, and on a scaled-up interface the pill was what
            // lost.
            //
            // A debug build still says so, because that is not a version, it is
            // a warning: this build is 18× slower at the work the program
            // exists to do, and mistaking one for a release reads as the viewer
            // being broken. A release build gets the clean bar.
            let mut right = ui.cursor().left() + name_width;
            if cfg!(debug_assertions) {
                const MARKER: &str = "debug";
                let left = right + 7.0;
                ui.painter().text(
                    pos2(left, bar.center().y + 1.0),
                    Align2::LEFT_CENTER,
                    MARKER,
                    theme::small_caps_font(),
                    theme::CLOSE_HOVER,
                );
                right = left
                    + ui.ctx()
                        .fonts_mut(|f| {
                            f.layout_no_wrap(
                                MARKER.to_owned(),
                                theme::small_caps_font(),
                                theme::TEXT,
                            )
                        })
                        .size()
                        .x;
            }
            if let Some(renderer) = renderer {
                right = renderer_badge(ui, bar, right + 8.0, renderer, vram, ours, logo);
            }
            right
        },
    )
    .inner
}

/// Names the adapter the viewer is drawing on, next to the version.
///
/// Amber when it is not the machine's fastest option: falling back to the
/// integrated GPU or to software is the difference between smooth and sluggish,
/// and it should not be a mystery why.
/// Returns its own right edge, so the caller knows how far the left cluster
/// reaches.
fn renderer_badge(
    ui: &mut Ui,
    bar: eframe::egui::Rect,
    left: f32,
    renderer: &crate::renderer::Choice,
    vram: Option<crate::vram::Memory>,
    ours: usize,
    logo: Option<egui::TextureHandle>,
) -> f32 {
    use eframe::egui::{Rect, vec2};

    // The maker's logo says who it is on its own, so the name beside it would
    // only repeat the picture. The drawn chip does not — nothing about it says
    // "CPU" — so the label stays wherever there is no logo to carry it.
    let text = logo.is_none().then(|| renderer.badge());
    let font = theme::small_caps_font();
    let width = text.map_or(0.0, |text| {
        ui.ctx()
            .fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font.clone(), theme::TEXT))
            .size()
            .x
            + 4.0
    });

    let padding = 6.0;
    let height = font.size + 6.0;
    let glyph = height - 4.0;
    let badge = Rect::from_min_size(
        pos2(left, bar.center().y + 1.0 - height / 2.0),
        vec2(glyph + width + padding * 2.0, height),
    );

    // No plate behind it: the logo is already a solid block of the maker's
    // colour, and tinting the background the same way only muddies it.
    let ink = renderer.badge_colour();

    let mark = Rect::from_min_size(
        pos2(badge.left() + padding * 0.7, badge.center().y - glyph / 2.0),
        vec2(glyph, glyph),
    );
    match &logo {
        // The maker's own artwork when we have it. Drawn untinted so the brand
        // colours are the ones they chose, not ours.
        Some(texture) => {
            ui.painter().image(
                texture.id(),
                mark,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        None if renderer.is_software() => icons::cpu_chip(ui.painter(), mark, ink),
        None => icons::gpu_chip(ui.painter(), mark, ink),
    }

    if let Some(text) = text {
        ui.painter().text(
            pos2(mark.right() + 4.0, badge.center().y),
            Align2::LEFT_CENTER,
            text,
            font.clone(),
            ink,
        );
    }

    // How much of the card is spoken for, refreshed as textures come and go and
    // on a slow tick so a game filling it shows up too. Only NVIDIA reports
    // this, so the figure is simply absent elsewhere rather than guessed.
    let mut right = badge.right();
    if let Some(memory) = vram {
        // Free rather than used: the question this answers is "is there room
        // for the next image?", and used tells you that only by subtraction.
        let usage = format!("free: {:.1} GB", memory.free_mb() as f32 / 1024.0);
        let usage_width = ui
            .ctx()
            .fonts_mut(|f| f.layout_no_wrap(usage.clone(), font.clone(), theme::TEXT))
            .size()
            .x;

        // A hair of headroom left is worth flagging: that is when the next
        // large texture fails.
        // Bright enough to read at a glance, and it warms as the card fills.
        //
        // Both a fraction and an absolute floor, taking whichever is more
        // alarming: a fifth of a 24 GB card is still 5 GB and fine, while a
        // fifth of a 6 GB card will not hold the next full-resolution texture.
        let free_mb = memory.free_mb();
        let fraction = memory.free_fraction();
        let colour = if fraction < 0.10 || free_mb < 512 {
            theme::CLOSE_HOVER
        } else if fraction < 0.25 || free_mb < 1536 {
            theme::WARNING
        } else {
            theme::TEXT
        };
        ui.painter().text(
            pos2(badge.right() + 7.0, badge.center().y),
            Align2::LEFT_CENTER,
            &usage,
            font,
            colour,
        );
        right = badge.right() + 7.0 + usage_width;
    }

    // The full story — adapter, class and free memory — on hover, so the badge
    // itself can stay to one word.
    let hover = Rect::from_min_max(badge.left_top(), pos2(right, badge.bottom()));
    let response = ui.interact(
        hover,
        ui.id().with("renderer_badge"),
        eframe::egui::Sense::hover(),
    );
    // Spell out whose memory the figure is: the card is shared, and a number
    // that looks like ours but is not would be worse than none.
    let mut tooltip = format!("Rendering on {}", renderer.summary());
    if let Some(memory) = vram {
        tooltip.push_str(&if renderer.shares_system_memory() {
            // No dedicated memory to report — this adapter takes what it needs
            // from the machine's own RAM, so that is the figure that means
            // anything.
            format!(
                "\n\nThis adapter has no memory of its own; it draws on system \
                 memory.\n{:.1} GB free of {:.1} GB.\n\
                 geo-ref_mapper's own textures account for {} MB.",
                memory.free_mb() as f32 / 1024.0,
                memory.total_mb() as f32 / 1024.0,
                ours / (1024 * 1024),
            )
        } else {
            format!(
                "\n\n{} MB in use on the card by all applications, of {} MB.\n\
                 geo-ref_mapper's own textures account for {} MB of that.",
                memory.used_mb(),
                memory.total_mb(),
                ours / (1024 * 1024),
            )
        });
    }
    response.on_hover_text(tooltip);
    right
}

/// Panel toggles plus minimize / maximize / close. Returns the left edge of the
/// whole cluster so the pill can be centred without overlapping it.
fn window_controls(ui: &mut Ui, bar: Rect, app: &mut GeoRefMapperApp) -> f32 {
    let rect = Rect::from_min_max(
        bar.left_top() + vec2(bar.width() * 0.5, 0.0),
        bar.right_bottom(),
    )
    .shrink2(vec2(8.0, 0.0));

    let mut left_edge = bar.right();
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            let close = chrome_button(ui, BUTTON_SIZE, "close", theme::CLOSE_HOVER);
            let tint = if close.hovered() {
                Color32::WHITE
            } else {
                theme::TEXT_MUTED
            };
            icons::close(ui.painter(), close.rect, tint);
            if close.clicked() {
                CLOSE_REQUESTED_BY_BUTTON.store(true, std::sync::atomic::Ordering::Relaxed);
                ui.ctx().send_viewport_cmd(ViewportCommand::Close);
            }

            let maximized = is_maximized(ui.ctx());
            let max_btn = chrome_button(ui, BUTTON_SIZE, "maximize", theme::SURFACE_HOVER);
            let tint = hover_tint(&max_btn);
            if maximized {
                icons::restore(ui.painter(), max_btn.rect, tint);
            } else {
                icons::maximize(ui.painter(), max_btn.rect, tint);
            }
            if max_btn.clicked() {
                toggle_maximized(ui.ctx());
            }

            let min_btn = chrome_button(ui, BUTTON_SIZE, "minimize", theme::SURFACE_HOVER);
            icons::minimize(ui.painter(), min_btn.rect, hover_tint(&min_btn));
            if min_btn.clicked() {
                ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true));
            }

            ui.add_space(6.0);
            separator(ui, bar);
            ui.add_space(6.0);

            let info = chrome_button(ui, TOGGLE_SIZE, "info_toggle", theme::SURFACE_HOVER);
            icons::panel_toggle(
                ui.painter(),
                info.rect,
                hover_tint(&info),
                icons::PanelSide::Right,
                app.info_open,
            );
            if info.clicked() {
                app.info_open = !app.info_open;
            }

            // The filmstrip, between its two side panels — the button order
            // matches where the panels sit on screen.
            let strip = chrome_button(ui, TOGGLE_SIZE, "filmstrip_toggle", theme::SURFACE_HOVER);
            icons::panel_toggle(
                ui.painter(),
                strip.rect,
                hover_tint(&strip),
                icons::PanelSide::Bottom,
                app.config.show_filmstrip,
            );
            if strip.clicked() {
                app.config.show_filmstrip = !app.config.show_filmstrip;
            }

            let files = chrome_button(ui, TOGGLE_SIZE, "files_toggle", theme::SURFACE_HOVER);
            icons::panel_toggle(
                ui.painter(),
                files.rect,
                hover_tint(&files),
                icons::PanelSide::Left,
                app.files_open,
            );
            if files.clicked() {
                app.files_open = !app.files_open;
            }

            left_edge = ui.min_rect().left();
        },
    );

    left_edge
}

/// The Viewer / Mosaic / Config switcher, centred in the bar.
fn page_pill(ui: &mut Ui, bar: Rect, left_edge: f32, right_edge: f32, app: &mut GeoRefMapperApp) {
    // With map support off the two map pages are not in the pill at all, so
    // the segment a click lands on is not the page number. Both are carried:
    // `pages` says what each segment means, `labels` what it reads as.
    let pages = crate::pages::visible_pages(app.config.map_support);
    let labels: Vec<&str> = pages.iter().map(|page| PAGE_LABELS[*page]).collect();

    let wanted = pill::desired_width(ui, &labels);

    // All the room between the two clusters, not the part of it that happens
    // to be centred.
    //
    // Centring on the bar and measuring outwards was throwing most of the gap
    // away: on a 768 pt title bar with the identity ending at 347 and the
    // window buttons starting at 513, there are 166 pt free — but only 25 of
    // them lie on the *left* of centre, so a centred pill could claim 51. It
    // fell under its own floor and vanished, taking every way between pages
    // with it.
    let free_left = left_edge + 12.0;
    let free_right = right_edge - 12.0;
    let width = wanted.min((free_right - free_left).max(0.0));

    // Shrunk rather than hidden. The pill is the only way between pages, so it
    // gives up its comfortable width long before it gives up existing — the
    // labels are cut to their segments to suit. It goes only when there is
    // genuinely nothing left to divide up.
    if width < pill::min_width(&labels) {
        return;
    }

    // Centred on the bar while that fits, and slid just far enough into the
    // free span when it does not. Symmetry is worth having and worth losing in
    // that order: a pill a little off-centre still reads as the navigation, and
    // an absent one does not.
    //
    // The clamp is safe because `width` was capped at the span above, so the
    // low bound can never overtake the high one.
    let half = width / 2.0;
    let centre_x = bar.center().x.clamp(free_left + half, free_right - half);

    let rect = Rect::from_center_size(pos2(centre_x, bar.center().y), vec2(width, pill::HEIGHT));
    ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
        // The page showing, as a segment. Acted on only when the pill says the
        // selection changed, so a page that is not in the list — which nothing
        // should be able to reach — lights the first segment without silently
        // navigating anywhere.
        let mut segment = pages
            .iter()
            .position(|page| *page == app.page)
            .unwrap_or_default();
        if pill::segmented(ui, rect, &mut segment, &labels)
            && let Some(page) = pages.get(segment).copied()
        {
            // The pill is navigation, not drilling in, so it leaves no return
            // point for Escape to follow.
            app.switch_page(page);
        }
    });

    tile_slider(ui, bar, rect.right() + 14.0, right_edge, app);
}

/// The Mosaic's tile-size slider, parked beside the page pill.
///
/// It lives up here rather than on the page because it is the Mosaic's only
/// persistent control, and a whole toolbar row for one slider was a row the
/// pictures could have had. Shown only while the Mosaic is open — on any other
/// page it would be a control for something you cannot see.
fn tile_slider(ui: &mut Ui, bar: Rect, left: f32, right_edge: f32, app: &mut GeoRefMapperApp) {
    const WIDTH: f32 = 128.0;
    if app.page != crate::pages::PAGE_MOSAIC || left + WIDTH > right_edge - 24.0 {
        return;
    }

    let rect = Rect::from_min_size(
        pos2(left, bar.center().y - pill::HEIGHT / 2.0),
        vec2(WIDTH, pill::HEIGHT),
    );
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            ui.spacing_mut().slider_width = WIDTH;
            let range =
                crate::pages::MIN_TILE..=app.mosaic_max_tile.max(crate::pages::MIN_TILE + 1.0);
            ui.add(egui::Slider::new(&mut app.mosaic_tile_size, range).show_value(false))
                .on_hover_text("Tile size  ·  Ctrl+wheel over the grid does this too");
        },
    );
}

fn separator(ui: &mut Ui, bar: Rect) {
    let (rect, _) = ui.allocate_exact_size(vec2(1.0, bar.height() * 0.4), Sense::hover());
    ui.painter().line_segment(
        [
            pos2(rect.center().x, rect.top()),
            pos2(rect.center().x, rect.bottom()),
        ],
        theme::hairline(),
    );
}

fn chrome_button(ui: &mut Ui, size: egui::Vec2, id_source: &str, hover_fill: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    let _ = id_source;
    if response.hovered() {
        let fill = if response.is_pointer_button_down_on() {
            hover_fill.gamma_multiply(0.8)
        } else {
            hover_fill
        };
        ui.painter().rect_filled(rect, CornerRadius::same(5), fill);
    }
    response
}

fn hover_tint(response: &Response) -> Color32 {
    if response.hovered() {
        theme::TEXT
    } else {
        theme::TEXT_MUTED
    }
}

fn is_maximized(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.viewport().maximized.unwrap_or(false))
}

fn toggle_maximized(ctx: &egui::Context) {
    let maximized = is_maximized(ctx);
    ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
}

/// Invisible grips along the window edges. Without decorations the compositor
/// no longer offers resize handles, so we hand the gesture back to it.
pub fn resize_grips(ctx: &egui::Context) {
    let locked = ctx.input(|i| {
        i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false)
    });
    if locked {
        return;
    }

    let screen = ctx.input(|i| i.viewport_rect());
    let b = theme::RESIZE_BORDER;

    // Edges first, corners after, so corners win where they overlap.
    let grips: [(&str, Rect, ResizeDirection, CursorIcon); 8] = [
        (
            "n",
            Rect::from_min_max(screen.left_top(), pos2(screen.right(), screen.top() + b)),
            ResizeDirection::North,
            CursorIcon::ResizeNorth,
        ),
        (
            "s",
            Rect::from_min_max(
                pos2(screen.left(), screen.bottom() - b),
                screen.right_bottom(),
            ),
            ResizeDirection::South,
            CursorIcon::ResizeSouth,
        ),
        (
            "w",
            Rect::from_min_max(screen.left_top(), pos2(screen.left() + b, screen.bottom())),
            ResizeDirection::West,
            CursorIcon::ResizeWest,
        ),
        (
            "e",
            Rect::from_min_max(
                pos2(screen.right() - b, screen.top()),
                screen.right_bottom(),
            ),
            ResizeDirection::East,
            CursorIcon::ResizeEast,
        ),
        (
            "nw",
            Rect::from_min_size(screen.left_top(), egui::Vec2::splat(b * 2.0)),
            ResizeDirection::NorthWest,
            CursorIcon::ResizeNorthWest,
        ),
        (
            "ne",
            Rect::from_min_size(
                pos2(screen.right() - b * 2.0, screen.top()),
                egui::Vec2::splat(b * 2.0),
            ),
            ResizeDirection::NorthEast,
            CursorIcon::ResizeNorthEast,
        ),
        (
            "sw",
            Rect::from_min_size(
                pos2(screen.left(), screen.bottom() - b * 2.0),
                egui::Vec2::splat(b * 2.0),
            ),
            ResizeDirection::SouthWest,
            CursorIcon::ResizeSouthWest,
        ),
        (
            "se",
            Rect::from_min_size(
                screen.right_bottom() - egui::Vec2::splat(b * 2.0),
                egui::Vec2::splat(b * 2.0),
            ),
            ResizeDirection::SouthEast,
            CursorIcon::ResizeSouthEast,
        ),
    ];

    for (name, rect, direction, cursor) in grips {
        Area::new(Id::new(("resize_grip", name)))
            .order(Order::Foreground)
            .fixed_pos(rect.min)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| {
                let (_, response) = ui.allocate_exact_size(rect.size(), Sense::drag());
                if response.hovered() || response.dragged() {
                    ui.ctx().set_cursor_icon(cursor);
                }
                if response.drag_started() {
                    ui.ctx()
                        .send_viewport_cmd(ViewportCommand::BeginResize(direction));
                }
            });
    }
}
