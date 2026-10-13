//! geo-ref_mapper — a fast, flexible image viewer.

// The window draws its own title bar, so no console window should tag along on
// Windows either.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod anchor;
mod app;
mod app_icon;
mod bookmarks;
mod cache;
mod carry;
mod chrome;
mod codecs;
mod cog;
mod convert;
mod crossfade;
mod db;
mod desktop;
mod detail;
mod exif;
mod files;
mod formats;
mod geo;
mod geotiff;
mod geowrite;
mod gpu;
mod heif;
mod hevc;
mod icons;
mod imagetiles;
mod install;
mod jobs;
mod jpeg;
mod jpeg_parallel;
mod loader;
mod manifest;
mod map;
mod meta;
mod metrics;
mod oom;
mod ops;
mod pages;
mod panels;
mod pill;
mod regain;
mod renderer;
mod scan;
mod sidecar;
mod stacks;
mod startup;
mod streets;
mod theme;
mod thumbs;
mod tiffwrite;
mod timeline;
mod transfer;
mod version;
mod viewport;
mod vram;
mod widgets;

use eframe::egui;

fn main() -> eframe::Result {
    // First thing of all, while the program is still single-threaded: taking a
    // variable out of the environment is only sound before there is a second
    // thread to race with, and the library opened below spawns them.
    startup::take_token();

    // Geometry has to be known before the window exists, so this reads the
    // library directly rather than waiting for the app to start.
    let saved = db::Library::open_default()
        .ok()
        .and_then(|library| library.setting(app::SETTING_WINDOW).ok().flatten())
        .and_then(|value| app::parse_geometry(&value));

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(app::APP_NAME)
        .with_app_id("geo-ref_mapper")
        .with_icon(app_icon::icon())
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([560.0, 360.0])
        .with_decorations(false)
        .with_resizable(true);

    // Opened straight onto a picture (`geo-ref_mapper photo.jpg`), the app goes full
    // screen instead on its first frame — the saved geometry belongs to the
    // panelled layout, and restoring it here would only flash it up before the
    // window fills the screen.
    let opened_on_picture = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .is_some_and(|path| path.is_file());

    if !opened_on_picture && let Some((x, y, width, height)) = saved {
        viewport = viewport
            .with_position([x, y])
            .with_inner_size([width, height]);
    }

    // Find a graphics adapter that will actually start before opening a window.
    // The discrete card is the first choice, but a game can leave it with no
    // memory to give — in which case the integrated GPU or a software
    // rasteriser still gets the viewer on screen.
    let choice = match renderer::probe() {
        Ok(choice) => choice,
        Err(details) => {
            renderer::report_failure(&details);
            std::process::exit(1);
        }
    };

    for (tier, reason) in &choice.rejected {
        eprintln!("geo-ref_mapper: {} unavailable — {reason}", tier.label());
    }
    if choice.is_fallback() {
        eprintln!("geo-ref_mapper: falling back to {}", choice.summary());
    } else {
        eprintln!("geo-ref_mapper: rendering on {}", choice.summary());
    }

    let tier = choice.tier;
    // Sized from what the driver says is free, not from a fixed guess.
    let (image_mb, thumbnail_mb) = choice.budgets();
    eprintln!("geo-ref_mapper: texture budgets {image_mb} MB images, {thumbnail_mb} MB thumbnails");

    // A backstop, not the handling. An out-of-memory raised on the device the
    // app draws with is caught by `oom`, installed below, and answered by
    // letting go of textures. This catches one raised anywhere else — while
    // the surface is being configured, or on a device eframe built before the
    // app was — where there is nothing to install a handler on. Nothing can be
    // recovered at that point, so all it does is say what happened.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info.to_string();
        if message.contains("Out of Memory") {
            eprintln!(
                "\ngeo-ref_mapper: the {} would not give up any more graphics memory.\n\n\
                 \u{20}   That is usually something else on the card — a game, most often —\n\
                 \u{20}   but it can also be this viewer: maps faded up on the map page each\n\
                 \u{20}   hold squares of their own, outside the texture budgets. The DEBUG\n\
                 \u{20}   tab breaks down what is held.\n\n\
                 \u{20}   Start on another renderer instead:\n\
                 \u{20}   GEO_REF_MAPPER_RENDERER=integrated geo-ref_mapper\n\
                 \u{20}   GEO_REF_MAPPER_RENDERER=software geo-ref_mapper\n",
                tier.label(),
            );
        }
        previous(info);
    }));

    let outcome = eframe::run_native(
        app::APP_NAME,
        eframe::NativeOptions {
            viewport,
            wgpu_options: renderer::configuration(tier),
            ..Default::default()
        },
        Box::new(move |cc| {
            // Takes wgpu's error reporting off its default, which is to panic
            // the moment the card refuses anything. Done before the first
            // frame, so the very first texture is already covered.
            if let Some(state) = &cc.wgpu_render_state {
                oom::install(&state.device);
                oom::exhaust_for_testing(&state.device);
            }
            theme::install(&cc.egui_ctx);
            Ok(Box::new(app::GeoRefMapperApp::with_renderer(
                &cc.egui_ctx,
                choice,
                image_mb,
                thumbnail_mb,
            )))
        }),
    );

    // The probe said this adapter would work, so getting here means it stopped
    // being true in between — a game can claim the rest of the card in that
    // gap. Say so in a window rather than dying with a wgpu error on a terminal
    // nobody is watching.
    if let Err(error) = &outcome {
        renderer::report_failure(&format!(
            "The {} was available a moment ago but failed when the window opened.\n\n{error}",
            tier.label()
        ));
    }
    outcome
}
