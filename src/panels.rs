//! The two slide-in side panels: FILES on the left, INFO on the right.

use eframe::egui::{Align2, CornerRadius, Rect, ScrollArea, Sense, Ui, pos2, vec2};

use crate::app::GeoRefMapperApp;
use crate::files::{FileTree, ImageMeta, Kind, Node, NodePath, format_bytes};
use crate::scan::Mode;
use crate::{icons, theme, widgets};

pub const FILES_WIDTH: f32 = 248.0;
pub const INFO_WIDTH: f32 = 268.0;

const ROW_HEIGHT: f32 = 19.0;
/// Per-level indent in the tree. Deliberately tight: these paths nest deeply —
/// MAPS-NEW alone is 211 folders — and every point spent on the step is a point
/// taken off the filenames in a 248 pt panel.
const INDENT: f32 = 7.0;
/// Room the selection footer needs under the tree.
const FOOTER_HEIGHT: f32 = 76.0;

// ---------------------------------------------------------------- FILES panel

pub fn files_panel(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    widgets::panel_header(ui, "FILES");
    scan_toolbar(ui, app);
    bookmark_row(ui, app);

    // The action footer is drawn after the tree, so the tree has to leave room
    // for it — a full-height scroll area would push it off the bottom.
    let footer = if !app.files.has_marks() {
        0.0
    } else {
        FOOTER_HEIGHT
    };
    let tree_height = (ui.available_height() - footer).max(60.0);

    // Two ways of picking the same set. The tree is where a picture is found by
    // where it sits; tags are where a set is found by what the pictures are.
    ui.add_space(2.0);
    let mut which = app.files_tab;
    let row = Rect::from_min_size(ui.cursor().left_top(), vec2(ui.available_width(), 22.0));
    if crate::pill::segmented(ui, row, &mut which, &["Files", "Tags"]) {
        app.files_tab = which;
    }
    ui.advance_cursor_after_rect(row);
    ui.add_space(4.0);

    if app.files_tab == 1 {
        tags_tab(ui, app, tree_height - 26.0);
        selection_actions(ui, app);
        return;
    }

    ScrollArea::vertical()
        .max_height(tree_height - 26.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Rows are drawn edge to edge; the default item spacing would put
            // 8px of dead air between every entry and halve how much fits.
            ui.spacing_mut().item_spacing.y = 0.0;

            let mut rows = Vec::new();
            // The same switch the Mosaic and the filmstrip follow, so the
            // number beside a folder means what the rest of the app would show
            // you if you opened it.
            let recursive = app.config.mosaic_recursive;
            flatten(&app.files.roots, &[], 0, recursive, &mut rows);
            count_marked_per_folder(&app.files, recursive, &mut rows);

            let mut toggle: Option<NodePath> = None;
            let mut select: Option<NodePath> = None;
            let mut mark: Option<NodePath> = None;
            let mut range: Option<NodePath> = None;
            let mut menu_for: Option<NodePath> = None;
            // A click that only brings the panel forward, changing nothing.
            let mut activate = false;

            let visible: Vec<NodePath> = rows.iter().map(|row| row.path.clone()).collect();
            let (control, shift) =
                ui.input(|i| (i.modifiers.command || i.modifiers.ctrl, i.modifiers.shift));

            // What a right-click on a marked row would act on.
            let batch = app.files.action_paths();
            // The folder currently being indexed. Its contents are appearing a
            // few hundred at a time, so acting on it mid-scan would act on
            // whatever happened to have landed so far.
            let importing = app
                .scan
                .is_running()
                .then(|| app.scan.root.clone())
                .flatten();
            let mut refresh: Vec<std::path::PathBuf> = Vec::new();
            let mut menu = MenuResult::default();
            let stack_menu = app.stack_menu(&batch);
            // Taken once: the scroll happens on the row that matches, and the
            // request must not survive into the next frame and fight a scroll
            // the user has since made by hand.
            let reveal = std::mem::take(&mut app.reveal_selection);
            let selected = app.files.selected.clone();
            // Read once for the whole pass: a click part way down must not be
            // judged against a focus its own earlier neighbour just changed.
            let tree_active = app.focus == crate::app::Focus::Tree;
            for row in &rows {
                // Straight from the row's own id — the tree already carries it,
                // so there is nothing to look up.
                let row_stacked = app
                    .files
                    .get(&row.path)
                    .and_then(|node| match &node.kind {
                        Kind::Image(meta) => meta.db_id,
                        Kind::Folder => None,
                    })
                    .is_some_and(|id| app.stacks.of(id).is_some());
                match tree_row(
                    ui,
                    &app.files,
                    row,
                    &batch,
                    importing.as_deref(),
                    reveal && row.path == selected,
                    tree_active,
                    Some(&stack_menu),
                    row_stacked,
                    app.config.map_support,
                    &mut menu,
                    &mut refresh,
                ) {
                    RowAction::None => {}
                    RowAction::Toggle => toggle = Some(row.path.clone()),
                    RowAction::Select => {
                        // Ctrl adds one, shift takes a run, a plain click moves
                        // the viewer and adds to or removes from the set.
                        //
                        // Except when the tree is not the panel being worked in:
                        // then the first plain click only brings it forward.
                        // Coming back from a picture zoomed right in, a click
                        // aimed at the tree would otherwise change which picture
                        // is shown and throw the zoom away, and the row landed
                        // on is whichever the pointer happened to be over.
                        //
                        // Ctrl and shift are let through regardless: reaching
                        // for a modifier says plainly what was meant, and making
                        // someone click twice to say it again is a nuisance
                        // rather than a safeguard.
                        if control {
                            mark = Some(row.path.clone());
                        } else if shift {
                            range = Some(row.path.clone());
                        } else if tree_active {
                            select = Some(row.path.clone());
                        } else {
                            activate = true;
                        }
                    }
                    RowAction::Menu => menu_for = Some(row.path.clone()),
                }
            }

            if let Some(path) = toggle {
                app.focus = crate::app::Focus::Tree;
                if let Some(node) = app.files.get_mut(&path) {
                    node.expanded = !node.expanded;
                }
                app.files.selected = path;
            }
            if activate {
                app.focus = crate::app::Focus::Tree;
            }
            if let Some(path) = mark {
                app.focus = crate::app::Focus::Tree;
                app.files.toggle_mark(&path);
            }
            if let Some(path) = range {
                app.focus = crate::app::Focus::Tree;
                app.files.mark_range(&visible, &path);
            }
            if let Some(path) = select {
                app.focus = crate::app::Focus::Tree;
                // With a set already being built, a plain click adds to it or
                // takes one back out — it does not throw the set away.
                //
                // Clearing was the old behaviour and it is the wrong risk to
                // take: a set assembled over a hundred rows is gone to a single
                // mis-aimed click, with nothing to undo it, while the cost of
                // the other choice is one more click to start again. The
                // "Unmark all" button is that click.
                //
                // Only counts the kind just clicked: picking a picture looks at
                // the marked pictures, not at the folder range being browsed.
                let is_folder = app.files.get(&path).is_some_and(Node::is_folder);
                let building = if is_folder {
                    !app.files.marked_folders.is_empty()
                } else {
                    !app.files.marked.is_empty()
                };
                if building {
                    app.files.toggle_mark(&path);
                } else {
                    app.files.clear_marks_like(&path);
                }
                app.files.anchor = Some(path.clone());
                app.files.selected = path;
            }
            // A right-click on an unmarked row acts on that row alone, which is
            // what every file manager does. The menu itself is attached to the
            // row inside `tree_row`; this only moves the selection under it.
            if let Some(path) = menu_for
                && !app.files.is_marked(&path)
            {
                app.files.clear_marks();
                app.files.selected = path;
            }
            let now = ui.input(|i| i.time);
            app.apply_menu(menu, &batch, now);
            if !refresh.is_empty() {
                // Every marked folder when the one clicked is among them, so a
                // range picked in the tree refreshes in one go.
                let marked: Vec<std::path::PathBuf> = app
                    .files
                    .marked
                    .iter()
                    .filter_map(|path| app.files.get(path))
                    .filter(|node| node.is_folder())
                    .filter_map(|node| node.path().map(std::path::PathBuf::from))
                    .collect();
                let folders = if refresh.iter().any(|one| marked.contains(one)) {
                    marked
                } else {
                    refresh
                };
                app.refresh_folders(folders, &ui.ctx().clone());
            }
        });

    selection_actions(ui, app);
}

/// The Scan button plus whatever the last or current scan has to say.
fn scan_toolbar(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let running = app.scan.is_running();
    let has_library = app.library.is_some();

    let start = ui.cursor().left_top();
    let row = Rect::from_min_size(start, vec2(ui.available_width(), 24.0));
    let gap = 6.0;
    let half = ((row.width() - gap) * 0.5).max(48.0);
    // Removable only when a real folder is picked out; the tree root is the
    // whole library and "remove everything" is not a button anyone wants next
    // to the one they press all the time.
    let removable = app.removable_folder();

    ui.scope_builder(
        eframe::egui::UiBuilder::new()
            .max_rect(row)
            .layout(eframe::egui::Layout::left_to_right(
                eframe::egui::Align::Center,
            )),
        |ui| {
            // Widths come from `row`, not from a second `available_width()`
            // read inside the scope. Those differ by the layout's own spacing,
            // so a full-width button asked for slightly more than the scope had
            // every frame — the panel grew to fit, which made the next frame's
            // width larger again, and the FILES panel crept wider on its own
            // for as long as it was open.
            ui.spacing_mut().item_spacing.x = 0.0;
            let scan = widgets::button(ui, "Scan", half, has_library && !running)
                .on_hover_text("Index a folder and every subfolder");
            if scan.clicked() {
                app.start_scan(Mode::Hunt, &ui.ctx().clone());
            }
            ui.add_space(gap);
            let remove = widgets::button(ui, "Remove", half, removable.is_some() && !running)
                .on_hover_text(match &removable {
                    Some(folder) => format!(
                        "Take {} out of the tree.\nThe folder and its pictures stay on disk.",
                        folder.display()
                    ),
                    None => "Select a folder to take it out of the tree".to_owned(),
                });
            if remove.clicked()
                && let Some(folder) = removable
            {
                app.forget_folder(&folder);
            }
        },
    );
    ui.advance_cursor_after_rect(row);
    ui.add_space(3.0);
    selection_row(ui, app);
    ui.add_space(4.0);

    if running {
        let mode = app.scan.mode.map(Mode::label).unwrap_or("Scan");
        let current = if app.scan.current.is_empty() {
            "…".to_owned()
        } else {
            app.scan.current.clone()
        };
        widgets::status_line(
            ui,
            &format!("{mode}: {} found · {}", app.scan.found, current),
            theme::TEXT_MUTED,
        );
        // Total is unknown until the walk finishes, so the bar sweeps.
        widgets::progress_bar(ui, None);
    } else if let Some(error) = &app.library_error {
        widgets::status_line(ui, error, theme::CLOSE_HOVER);
    } else if let Some(error) = &app.scan.error {
        widgets::status_line(ui, error, theme::CLOSE_HOVER);
    } else if let Some(summary) = &app.scan.summary {
        // This is the only sign a scan happened at all. Dim made a finished
        // scan look like nothing had run.
        widgets::status_line(ui, summary, theme::TEXT);
    } else if app.files.is_mock {
        widgets::status_line(
            ui,
            "Sample data — scan a folder to index your own",
            theme::TEXT_DIM,
        );
    } else {
        widgets::status_line(
            ui,
            &format!(
                "{} images in {} folders",
                app.stats.images, app.stats.folders
            ),
            theme::TEXT,
        );
    }

    ui.add_space(6.0);
    background_tasks(ui, app);
    ui.add_space(6.0);
}

/// The second rank under Scan: things done to the selection rather than to the
/// library.
///
/// Its own row, in the smaller type, because these are occasional. Laid out
/// from a list so another one is a line added here rather than a rearrangement
/// — the widths come from how many there are.
///
/// Each says whether it applies. "Unmark all" with nothing marked is drawn
/// disabled rather than hidden: a button that comes and goes is one you have to
/// hunt for, and its absence looks like the panel has lost something.
fn selection_row(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let marked = app.files.action_paths().len();

    // (label, enabled, hover, what it does)
    #[allow(clippy::type_complexity)]
    let actions: Vec<(String, bool, String, fn(&mut GeoRefMapperApp))> = vec![
        (
            "Select all".to_owned(),
            true,
            "Marks every picture in the tree. The start of narrowing a set down \
             by tag rather than picking it out by hand."
                .to_owned(),
            (|app: &mut GeoRefMapperApp| app.mark_everything(0.0)) as fn(&mut GeoRefMapperApp),
        ),
        (
            if marked > 0 {
                format!("Unmark all ({marked})")
            } else {
                "Unmark all".to_owned()
            },
            marked > 0,
            "Clears every marked picture and folder. Nothing on disk changes.".to_owned(),
            (|app: &mut GeoRefMapperApp| app.unmark_everything()) as fn(&mut GeoRefMapperApp),
        ),
    ];

    let start = ui.cursor().left_top();
    let row = Rect::from_min_size(start, vec2(ui.available_width(), 19.0));
    let gap = 6.0;
    let each =
        ((row.width() - gap * (actions.len() as f32 - 1.0)) / actions.len() as f32).max(48.0);

    ui.scope_builder(
        eframe::egui::UiBuilder::new()
            .max_rect(row)
            .layout(eframe::egui::Layout::left_to_right(
                eframe::egui::Align::Center,
            )),
        |ui| {
            // As in the row above: widths come from `row` rather than a second
            // reading of the available width, which differs by the layout's own
            // spacing and makes the panel creep wider every frame.
            ui.spacing_mut().item_spacing.x = 0.0;
            for (index, (label, enabled, hover, act)) in actions.iter().enumerate() {
                if index > 0 {
                    ui.add_space(gap);
                }
                if widgets::small_button(ui, label, each, *enabled)
                    .on_hover_text(hover.clone())
                    .clicked()
                {
                    act(app);
                }
            }
        },
    );
    ui.advance_cursor_after_rect(row);
}

/// What is running in the background, and how to stop it.
///
/// A section of its own rather than another branch of the status line above.
/// That line answers one question at a time — the newest thing to have happened
/// — so a finished scan's summary hid the thumbnail sweep behind it, and work
/// that takes twenty minutes on a large library appeared to be doing nothing at
/// all. These run at the same time as each other and as everything else, so
/// they are listed rather than chosen between.
///
/// Shown only while something is running: an empty list every time the panel is
/// open would be a heading earning its space by saying nothing.
fn background_tasks(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let sweeping = app
        .thumbs
        .as_ref()
        .is_some_and(|thumbs| thumbs.sweeping || thumbs.remaining > 0);
    let converting = app.converting.is_some();
    if !sweeping && !converting {
        return;
    }

    widgets::section_label(ui, "BACKGROUND");

    if let Some(thumbs) = &app.thumbs
        && (thumbs.sweeping || thumbs.remaining > 0)
    {
        let (remaining, running) = (thumbs.remaining, thumbs.sweeping);
        let done = app.stats.images.saturating_sub(remaining);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            widgets::status_line(
                ui,
                &if running {
                    format!("Thumbnails — {remaining} left of {}", app.stats.images)
                } else {
                    format!("Thumbnails paused — {remaining} left")
                },
                if running {
                    theme::TEXT_MUTED
                } else {
                    theme::TEXT_DIM
                },
            );
        });
        widgets::progress_bar(
            ui,
            (app.stats.images > 0).then(|| done as f32 / app.stats.images as f32),
        );
        // Pause rather than cancel: the work is picking up where it left off
        // anyway — every thumbnail already built stays built — so stopping is
        // only ever "not now", and calling it Cancel would suggest otherwise.
        let label = if running { "Pause" } else { "Resume" };
        if widgets::button(ui, label, 90.0, true)
            .on_hover_text(
                "Thumbnail building is what makes the machine warm. Pausing \
                 gives the cores back; nothing already built is lost.",
            )
            .clicked()
            && let Some(thumbs) = &mut app.thumbs
        {
            if running {
                thumbs.stop_sweep();
            } else {
                thumbs.start_sweep();
            }
        }
        ui.add_space(4.0);
    }

    if let Some(running) = &app.converting {
        let (done, total) = (running.done, running.total);
        let current = running.current.clone();
        widgets::status_line(
            ui,
            &format!("Converting maps — {done} of {total} · {current}"),
            theme::TEXT_MUTED,
        );
        widgets::progress_bar(ui, (total > 0).then(|| done as f32 / total as f32));
        // Stop, not pause: a half-written map is removed and the queue keeps
        // what it has not reached, so starting again carries on from there.
        if widgets::button(ui, "Stop", 90.0, true)
            .on_hover_text(
                "Finishes the map being converted now, then stops. The rest \
                 stay on the queue.",
            )
            .clicked()
        {
            app.stop_map_conversion();
        }
        ui.add_space(4.0);
    }
}

/// Size of one bookmark square. Small enough that a couple of dozen fit across
/// the panel, which is the point of two-character labels.
const BOOKMARK_SIZE: f32 = 22.0;

/// Pinned folders, plus the buttons that add and remove them.
///
/// Sits directly above the tree so a jump between distant folders is one click
/// rather than a scroll through everything in between.
fn bookmark_row(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    // What the `+` would pin: every marked folder when there are some, else the
    // one folder selected. Marking three and pinning them gives one square that
    // brings all three back.
    let target = app.bookmarkable_folders();
    let pinned = !target.is_empty() && app.bookmarks.contains_set(&target);

    let mut jump_to: Option<crate::bookmarks::Bookmark> = None;
    let mut drop_bookmark: Option<Vec<std::path::PathBuf>> = None;
    let mut changed = false;

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(3.0, 3.0);

        // Add: disabled once this exact set is pinned, so the button's state
        // answers "is this one saved?" without a second indicator.
        if square_button(ui, "+", theme::TEXT, !target.is_empty() && !pinned)
            .on_hover_text(match target.as_slice() {
                [] => "Select a folder — or mark several — to bookmark it".to_owned(),
                [one] => format!("Bookmark {}", one.display()),
                many => format!("Bookmark these {} folders together", many.len()),
            })
            .clicked()
        {
            changed |= app.bookmarks.add_set(target.clone());
        }

        let remove = square_button(ui, "−", theme::TEXT, pinned).on_hover_text(
            "Remove this bookmark  ·  right-click to drop any whose folders have gone",
        );
        if remove.clicked() {
            changed |= app.bookmarks.remove_set(&target);
        }
        // Bookmarks on an unplugged drive are kept, not silently dropped — but
        // there has to be some way to clear out ones that are truly gone.
        if remove.secondary_clicked() {
            let before = app.bookmarks.entries().len();
            app.bookmarks.forget_missing();
            changed |= app.bookmarks.entries().len() != before;
        }

        if !app.bookmarks.is_empty() {
            ui.add_space(4.0);
        }
        // Single folders are labelled as a set so no two squares read the same;
        // a group is labelled by its count, which two characters cannot do for
        // three different names anyway.
        let singles: Vec<String> = app
            .bookmarks
            .entries()
            .iter()
            .filter_map(|entry| entry.only())
            .map(|folder| {
                folder
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| folder.to_string_lossy().into_owned())
            })
            .collect();
        let mut single_labels = crate::bookmarks::labels(&singles).into_iter();

        for entry in app.bookmarks.entries() {
            let label = match entry.only() {
                Some(_) => single_labels.next().unwrap_or_else(|| entry.label()),
                None => entry.label(),
            };
            // A group is the folder-scope colour, because that is what pressing
            // it restores.
            let is_current = entry.folders == target;
            let tint = match (is_current, entry.is_group()) {
                (true, _) => theme::ACCENT,
                (false, true) => theme::MARKED,
                (false, false) => theme::TEXT,
            };

            // Right-click removes *this* square.
            //
            // The `−` above can only remove the bookmark matching what is
            // currently selected, which is no help at all for the case that
            // needs it: a bookmark whose folder has been taken out of the
            // library cannot be selected, so it could never be matched and
            // never be removed.
            let square = square_button(ui, &label, tint, true).on_hover_text(format!(
                "{}\n\nRight-click to remove this bookmark",
                entry.tooltip(),
            ));
            if square.clicked() {
                jump_to = Some(entry.clone());
            }
            if square.secondary_clicked() {
                drop_bookmark = Some(entry.folders.clone());
            }
        }
    });

    if let Some(folders) = drop_bookmark {
        changed |= app.bookmarks.remove_set(&folders);
    }

    if changed {
        app.persist_bookmarks();
    }
    if let Some(entry) = jump_to {
        app.open_bookmark_set(&entry);
    }
    ui.add_space(6.0);
}

/// One small square button. Its own widget rather than `widgets::button` so the
/// label is centred in a fixed square instead of a text-width pill.
fn square_button(
    ui: &mut Ui,
    label: &str,
    tint: eframe::egui::Color32,
    enabled: bool,
) -> eframe::egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        vec2(BOOKMARK_SIZE, BOOKMARK_SIZE),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );

    let (fill, ink) = if !enabled {
        (theme::SURFACE.gamma_multiply(0.5), theme::TEXT_DIM)
    } else if response.hovered() {
        (theme::SURFACE_HOVER, theme::TEXT)
    } else {
        (theme::SURFACE, tint)
    };

    ui.painter().rect_filled(rect, CornerRadius::same(4), fill);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        theme::small_caps_font(),
        ink,
    );
    response
}

struct Row {
    path: NodePath,
    depth: usize,
    name: String,
    is_folder: bool,
    expanded: bool,
    child_count: usize,
    /// Marked pictures inside this folder, when any are marked at all.
    selected_count: usize,
    /// The file carries GeoTIFF position tags, so it can go on the map.
    georeferenced: bool,
}

#[cfg(test)]
mod counting {
    use super::*;

    fn tree() -> FileTree {
        // /photos holds one picture and a subfolder with two more.
        let records = ["/photos/a.png", "/photos/trip/b.png", "/photos/trip/c.png"]
            .into_iter()
            .map(|path| crate::db::ImageRecord {
                path: path.to_owned(),
                folder: std::path::Path::new(path)
                    .parent()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                filename: std::path::Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        FileTree::from_records(&records)
    }

    /// The count the tree would print beside the root folder.
    fn root_count(tree: &FileTree, recursive: bool) -> usize {
        let mut rows = Vec::new();
        flatten(&tree.roots, &[], 0, recursive, &mut rows);
        rows[0].child_count
    }

    #[test]
    fn a_folder_shows_how_much_of_it_is_marked() {
        let tree = tree();
        let mut rows = Vec::new();
        flatten(&tree.roots, &[], 0, true, &mut rows);
        assert_eq!(rows[0].selected_count, 0, "nothing marked, nothing shown");

        // Mark the two pictures inside the subfolder. Taken from the tree
        // rather than from the rows: a collapsed folder emits no rows, so the
        // pictures under it are not in that list at all.
        let mut tree = tree;
        let nested: Vec<NodePath> = tree
            .images_in(&[0], true)
            .into_iter()
            .map(|(path, _)| path)
            .filter(|path| path.len() > 2)
            .collect();
        assert_eq!(nested.len(), 2, "the subfolder holds two: {nested:?}");
        for path in nested {
            tree.marked.insert(path);
        }
        let mut rows = Vec::new();
        flatten(&tree.roots, &[], 0, true, &mut rows);
        count_marked_per_folder(&tree, true, &mut rows);
        assert_eq!(rows[0].selected_count, 2, "the root sees them recursively");

        // Without recursion the root sees only what is directly inside it.
        let mut rows = Vec::new();
        flatten(&tree.roots, &[], 0, false, &mut rows);
        count_marked_per_folder(&tree, false, &mut rows);
        assert_eq!(rows[0].selected_count, 0);
    }

    #[test]
    fn a_folder_counts_its_pictures_not_its_subfolders() {
        let tree = tree();
        assert_eq!(
            root_count(&tree, false),
            1,
            "one picture sits directly in /photos; the subfolder is not a picture",
        );
        assert_eq!(
            root_count(&tree, true),
            3,
            "recursively, the subfolder's two count as well",
        );
    }

    #[test]
    fn an_empty_parent_reads_as_empty_rather_than_as_its_subfolder_count() {
        // The case this exists for: a folder of nothing but folders used to
        // report how many folders it held, which reads as a picture count.
        let records = ["/maps/1930/a.tif", "/maps/1931/b.tif"]
            .into_iter()
            .map(|path| crate::db::ImageRecord {
                path: path.to_owned(),
                folder: std::path::Path::new(path)
                    .parent()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                filename: "x.tif".to_owned(),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let tree = FileTree::from_records(&records);
        assert_eq!(root_count(&tree, false), 0);
        assert_eq!(root_count(&tree, true), 2);
    }
}

/// What the menu was asked to do, collected rather than acted on: the menu is
/// drawn deep inside a borrow of the tree or the grid, and the actions all need
/// the whole app.
#[derive(Default)]
pub struct MenuResult {
    pub delete: Vec<std::path::PathBuf>,
    pub transfer: Option<(crate::transfer::Mode, Vec<std::path::PathBuf>)>,
    pub copy_image: Option<std::path::PathBuf>,
    pub copied_path: bool,
    /// The action, and the picture it was asked on.
    pub stack: Option<(StackAction, std::path::PathBuf)>,
    /// Pictures to add to the map conversion queue.
    pub queue_maps: Vec<std::path::PathBuf>,
    /// A folder to take out of the library. Nothing on disk is touched.
    pub forget_folder: Option<std::path::PathBuf>,
    /// A folder whose pictures — and every picture beneath it — go on the queue.
    ///
    /// The folder rather than the files: the tree knows how many there are, but
    /// working out *which* is a question for the library, and a menu drawn every
    /// frame for every visible row is the wrong place to ask it.
    pub queue_map_folder: Option<std::path::PathBuf>,
}

/// What the stack part of the menu should offer for this selection.
pub struct StackMenu {
    /// The label for "make a stack of these", or why it cannot be done.
    pub create: Result<String, String>,
    /// A stack is open, so its members can be reordered and removed.
    pub open: bool,
}

/// Which stack action the menu asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StackAction {
    Create,
    Unstack,
    SetCover,
    Remove,
}

/// The right-click menu for a folder.
///
/// Refresh re-reads it from disk: pictures added outside geo-ref_mapper appear, and
/// ones deleted outside it stop being offered. Nothing here changes a file.
///
/// `nested` is how many pictures lie anywhere beneath the folder — already
/// counted and cached by the tree, so the menu can say the number rather than
/// leaving it to be discovered from the toast afterwards. Queuing 7,000 maps
/// and queuing 3 should not look like the same click.
pub fn folder_context_menu(
    _ui: &mut Ui,
    response: &eframe::egui::Response,
    folder: &std::path::Path,
    nested: usize,
    map_support: bool,
    refresh: &mut Vec<std::path::PathBuf>,
    out: &mut MenuResult,
) {
    response.context_menu(|ui| {
        // First, and the only thing here that leads anywhere. Disabled with its
        // reason rather than hidden when the folder holds nothing, so it is
        // findable on the folder where it does not apply — but gone entirely
        // with map support off, where the queue it adds to has no page.
        if map_support {
            if nested == 0 {
                ui.add_enabled(false, eframe::egui::Button::new("Add to map images"))
                    .on_disabled_hover_text("No pictures in this folder or beneath it");
            } else {
                let label = format!("Add {} to map images", crate::app::plural_images(nested),);
                if ui
                    .button(label)
                    .on_hover_text(format!(
                        "Everything under {}, however deep",
                        folder.display(),
                    ))
                    .clicked()
                {
                    out.queue_map_folder = Some(folder.to_path_buf());
                    ui.close();
                }
            }
            ui.separator();
        }

        if ui.button("Refresh").clicked() {
            refresh.push(folder.to_path_buf());
            ui.close();
        }
        // The other half of Scan, and until now only reachable through the
        // button in the toolbar — which acts on the *selected* folder, not the
        // one under the pointer.
        if ui
            .button("Remove from the tree")
            .on_hover_text(format!(
                "Takes {} out of the library.\nThe folder and its pictures stay on disk.",
                folder.display()
            ))
            .clicked()
        {
            out.forget_folder = Some(folder.to_path_buf());
            ui.close();
        }
        if ui.button("Open location").clicked() {
            crate::desktop::show_in_file_manager(folder);
            ui.close();
        }
    });
}

/// The right-click menu for one image, shared by the FILES tree and the Mosaic
/// so the two cannot offer different things for the same file.
///
/// Deleting is here as well as on the Delete key, but it only ever *asks* — the
/// menu sets the confirmation, it does not remove anything.
pub fn file_context_menu(
    _ui: &mut Ui,
    response: &eframe::egui::Response,
    file: &std::path::Path,
    batch: &[std::path::PathBuf],
    stack_menu: Option<&StackMenu>,
    in_stack: bool,
    map_support: bool,
    out: &mut MenuResult,
) {
    // Right-clicking one of the marked images acts on the whole set; right
    // clicking an unmarked one acts on it alone. That is what every file
    // manager does, and it is the difference between deleting one picture and
    // deleting twelve.
    let targets: Vec<std::path::PathBuf> = if batch.iter().any(|marked| marked == file) {
        batch.to_vec()
    } else {
        vec![file.to_path_buf()]
    };
    let many = targets.len() > 1;
    let count = crate::app::plural_images(targets.len());
    let (trash, move_to, copy_to) = if many {
        (
            format!("Move {count} to Trash…"),
            format!("Move {count} to folder…"),
            format!("Copy {count} to folder…"),
        )
    } else {
        (
            "Move to Trash…".to_owned(),
            "Move to folder…".to_owned(),
            "Copy to folder…".to_owned(),
        )
    };

    response.context_menu(|ui| {
        // Stacking first: it is the action that changes what the grid shows,
        // and it is disabled with its reason rather than hidden, so it is
        // findable when it does not apply.
        if let Some(stack) = stack_menu.as_ref() {
            match &stack.create {
                Ok(label) => {
                    if ui.button(label).clicked() {
                        out.stack = Some((StackAction::Create, file.to_path_buf()));
                        ui.close();
                    }
                }
                Err(why) => {
                    ui.add_enabled(false, eframe::egui::Button::new("Stack"))
                        .on_disabled_hover_text(why.clone());
                }
            }
            if in_stack {
                if ui.button("Unstack").clicked() {
                    out.stack = Some((StackAction::Unstack, file.to_path_buf()));
                    ui.close();
                }
                if stack.open {
                    if ui.button("Set as stack cover").clicked() {
                        out.stack = Some((StackAction::SetCover, file.to_path_buf()));
                        ui.close();
                    }
                    if ui.button("Remove from stack").clicked() {
                        out.stack = Some((StackAction::Remove, file.to_path_buf()));
                        ui.close();
                    }
                }
            }
            ui.separator();
        }

        // Above the copying and moving, because it is the one that leads
        // somewhere rather than acting here. Absent with map support off:
        // there is nowhere for it to lead.
        if map_support {
            let queue_label = if many {
                format!("Add {count} to map images")
            } else {
                "Add to map images".to_owned()
            };
            if ui.button(queue_label).clicked() {
                out.queue_maps = targets.clone();
                ui.close();
            }
            ui.separator();
        }

        if ui.button("Copy image").clicked() {
            out.copy_image = Some(file.to_path_buf());
            ui.close();
        }
        if ui.button("Copy path").clicked() {
            ui.ctx().copy_text(file.to_string_lossy().into_owned());
            out.copied_path = true;
            ui.close();
        }
        if ui.button("Open file location").clicked() {
            crate::desktop::show_in_file_manager(file);
            ui.close();
        }
        ui.separator();
        if ui.button(copy_to).clicked() {
            out.transfer = Some((crate::transfer::Mode::Copy, targets.clone()));
            ui.close();
        }
        if ui.button(move_to).clicked() {
            out.transfer = Some((crate::transfer::Mode::Move, targets.clone()));
            ui.close();
        }
        ui.separator();
        if ui.button(trash).clicked() {
            out.delete = targets;
            ui.close();
        }
    });
}

enum RowAction {
    None,
    Toggle,
    Select,
    Menu,
}

/// Footer under the tree: what is selected, and the way in to acting on it.
///
/// Deliberately always visible rather than hidden behind the right-click menu —
/// a batch that is about to rewrite a hundred files should say so on screen
/// before anyone goes looking for it.
/// Picking a set by what the pictures *are* rather than where they sit.
///
/// The whole point is set arithmetic. Select everything, then narrow: keep only
/// the survey maps, drop the low-contrast ones, and what is left is the batch —
/// which on eight thousand maps is the difference between a minute and an
/// afternoon.
///
/// Three buttons per tag rather than one, because "and", "not" and "or" are
/// three different questions and guessing which was meant would be wrong a
/// third of the time.
fn tags_tab(ui: &mut Ui, app: &mut GeoRefMapperApp, height: f32) {
    let tags = app.tag_counts();
    if tags.is_empty() {
        ui.add_space(6.0);
        hint(
            ui,
            "No tags in the library. They come from the sidecar beside each \
             picture and are read as it is indexed — scan a folder that has \
             them and they appear here.",
        );
        return;
    }

    let marked = app.files.action_paths().len();
    let filtering = !app.tag_filter.is_empty();
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        // The button is allocated *first*. `status_line` takes the whole of
        // whatever is left, so anything placed after it lands past the panel's
        // edge — and a panel that sizes to its contents then grows to fit,
        // which is the width jumping the moment a filter becomes active.
        if filtering && widgets::small_button(ui, "Clear filter", 96.0, true).clicked() {
            let now = ui.input(|i| i.time);
            app.clear_tag_filter(now);
        }
        let room = ui.available_width();
        widgets::status_line_sized(
            ui,
            &if filtering {
                format!("{marked} selected")
            } else {
                format!("{} tags", tags.len())
            },
            if filtering {
                theme::TEXT
            } else {
                theme::TEXT_MUTED
            },
            room,
        );
    });
    ui.add_space(2.0);
    hint(
        ui,
        "Click a tag to require it, again to refuse it, again to stop asking. \
         The selection is worked out afresh each time, so nothing has to be \
         undone by hand.",
    );
    ui.add_space(4.0);

    let mut clicked: Option<String> = None;
    ScrollArea::vertical()
        .max_height(height.max(60.0))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (tag, count) in &tags {
                let state = app.tag_state(tag);
                let (mark, ink) = match state {
                    crate::app::TagState::Include => ("+", theme::GEOREFERENCED),
                    crate::app::TagState::Exclude => ("\u{2212}", theme::CLOSE_HOVER),
                    crate::app::TagState::Off => (" ", theme::TEXT_DIM),
                };

                let (rect, response) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::click());
                if state != crate::app::TagState::Off {
                    ui.painter()
                        .rect_filled(rect, CornerRadius::same(4), ink.gamma_multiply(0.16));
                }
                if response.hovered() {
                    ui.ctx()
                        .set_cursor_icon(eframe::egui::CursorIcon::PointingHand);
                }

                ui.painter().text(
                    pos2(rect.left() + 6.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    mark,
                    theme::label_font(),
                    ink,
                );
                let room = rect.width() - 76.0;
                ui.painter().text(
                    pos2(rect.left() + 20.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    widgets::elide(ui, tag, theme::label_font(), room.max(20.0)),
                    theme::label_font(),
                    if state == crate::app::TagState::Off {
                        theme::TEXT_MUTED
                    } else {
                        theme::TEXT
                    },
                );
                ui.painter().text(
                    pos2(rect.right() - 6.0, rect.center().y),
                    Align2::RIGHT_CENTER,
                    count.to_string(),
                    theme::small_caps_font(),
                    theme::TEXT_DIM,
                );

                if response
                    .on_hover_text(match state {
                        crate::app::TagState::Off => "Click to require this tag",
                        crate::app::TagState::Include => {
                            "Required \u{2014} click to refuse it instead"
                        }
                        crate::app::TagState::Exclude => "Refused \u{2014} click to stop asking",
                    })
                    .clicked()
                {
                    clicked = Some(tag.clone());
                }
            }
        });

    if let Some(tag) = clicked {
        let now = ui.input(|i| i.time);
        app.cycle_tag(&tag, now);
    }
}

fn selection_actions(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let paths = app.files.action_paths();
    if paths.is_empty() {
        return;
    }

    ui.add_space(4.0);
    ui.separator();

    let marked = app.files.marked.len();
    let summary = if marked > 1 {
        format!("{marked} rows selected · {} image(s)", paths.len())
    } else {
        format!("{} image(s)", paths.len())
    };
    widgets::status_line(ui, &summary, theme::TEXT_MUTED);

    let gap = 6.0;
    let start = ui.cursor().left_top();
    let row = Rect::from_min_size(start, vec2(ui.available_width(), 24.0));
    let width = ((row.width() - gap) * 0.5).max(48.0);
    ui.scope_builder(
        eframe::egui::UiBuilder::new()
            .max_rect(row)
            .layout(eframe::egui::Layout::left_to_right(
                eframe::egui::Align::Center,
            )),
        |ui| {
            // Widths below are computed to fill the row exactly, with the gap
            // added by hand. egui also inserts `item_spacing.x` between items,
            // so leaving it on made the content 8 pt wider than the row every
            // single frame — the panel grew to fit, and kept growing, 8 pt at a
            // time, until it hit its maximum width. That was the FILES panel
            // widening by itself on startup.
            ui.spacing_mut().item_spacing.x = 0.0;
            if widgets::button(ui, "Export…", width, true)
                .on_hover_text("Convert, resize or crop these into another folder")
                .clicked()
            {
                app.export.sources = paths.clone();
                app.go_to_page(crate::pages::PAGE_EXPORT); // Escape comes back
            }
            ui.add_space(gap);
            if widgets::button(ui, "Clear", width, marked > 0)
                .on_hover_text("Deselect everything")
                .clicked()
            {
                app.files.clear_marks();
            }
        },
    );
    ui.advance_cursor_after_rect(row);
    ui.add_space(4.0);
}

/// Fills in how many marked pictures sit inside each visible folder.
///
/// Walked from the marked set upwards rather than from each folder downwards:
/// there are a few dozen rows on screen but there can be thousands of marks,
/// and asking every row to search the whole set costs far more than asking
/// every mark which of the rows it belongs to. Nothing happens at all when
/// nothing is marked, which is the usual case.
fn count_marked_per_folder(tree: &FileTree, recursive: bool, rows: &mut [Row]) {
    if tree.marked.is_empty() {
        return;
    }
    let mut counts: std::collections::HashMap<NodePath, usize> = std::collections::HashMap::new();
    let folders: std::collections::HashSet<NodePath> = rows
        .iter()
        .filter(|row| row.is_folder)
        .map(|row| row.path.clone())
        .collect();

    for marked in &tree.marked {
        // A picture counts towards its own folder always, and towards every
        // folder above it only when the counts are recursive — so the two
        // numbers in `44 / 123` are always measuring the same thing.
        let depths: Box<dyn Iterator<Item = usize>> = if recursive {
            Box::new(1..marked.len())
        } else {
            Box::new(marked.len().checked_sub(1).into_iter())
        };
        for depth in depths {
            if depth == 0 {
                continue;
            }
            let ancestor = &marked[..depth];
            if folders.contains(ancestor) {
                *counts.entry(ancestor.to_vec()).or_default() += 1;
            }
        }
    }

    for row in rows.iter_mut() {
        if let Some(count) = counts.get(&row.path) {
            row.selected_count = *count;
        }
    }
}

/// Builds the visible rows. Counts come from the cached totals on each node,
/// so this walks only what is on screen rather than the whole library.
fn flatten(nodes: &[Node], prefix: &[usize], depth: usize, recursive: bool, out: &mut Vec<Row>) {
    for (index, node) in nodes.iter().enumerate() {
        let mut path = prefix.to_vec();
        path.push(index);

        out.push(Row {
            path: path.clone(),
            depth,
            name: node.name.clone(),
            is_folder: node.is_folder(),
            expanded: node.expanded,
            child_count: if recursive {
                node.nested_images
            } else {
                node.direct_images
            },
            selected_count: 0,
            georeferenced: match &node.kind {
                Kind::Image(meta) => meta.georeferenced,
                Kind::Folder => false,
            },
        });

        if node.is_folder() && node.expanded {
            flatten(&node.children, &path, depth + 1, recursive, out);
        }
    }
}

fn tree_row(
    ui: &mut Ui,
    tree: &FileTree,
    row: &Row,
    batch: &[std::path::PathBuf],
    importing: Option<&std::path::Path>,
    reveal: bool,
    // The tree is the panel the keyboard and plain clicks belong to.
    active: bool,
    stack_menu: Option<&StackMenu>,
    in_stack: bool,
    // Whether the right-click menus carry their *Add to map images* entry.
    map_support: bool,
    out: &mut MenuResult,
    refresh: &mut Vec<std::path::PathBuf>,
) -> RowAction {
    // Rows under the folder being imported are inert until it finishes: what is
    // in them changes several hundred times a second, so anything acted on now
    // would be whatever had happened to land by the time of the click.
    let busy = importing.is_some_and(|root| {
        tree.get(&row.path)
            .and_then(Node::path)
            .is_some_and(|path| std::path::Path::new(path).starts_with(root))
    });
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), ROW_HEIGHT),
        if busy {
            Sense::hover()
        } else {
            Sense::click_and_drag()
        },
    );

    // A bookmark can land anywhere in a tree of four hundred folders, so the
    // panel has to travel to it — otherwise the jump looks like nothing at all
    // happened.
    if reveal {
        ui.scroll_to_rect(rect, Some(eframe::egui::Align::Center));
    }

    let selected = tree.selected == row.path;
    let marked = tree.mark_of(&row.path);
    if selected {
        // Dimmer while the arrows and clicks belong to another panel. Without
        // some sign of it, the first click on an inactive tree — which only
        // brings it forward — looks like a click that did nothing at all.
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(5),
            theme::ACCENT.gamma_multiply(if active { 0.22 } else { 0.09 }),
        );
    } else if let Some(mark) = marked {
        // Its own colour, not a dimmer accent: marked rows are queued for
        // something, which is a different thing from the row being viewed —
        // and folders and pictures are queued for different things again.
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(5),
            crate::pages::mark_colour(mark).gamma_multiply(0.20),
        );
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(5), theme::SURFACE);
    }
    if let Some(mark) = marked {
        // A stripe down the left edge, so the state survives a light theme or a
        // colour-blind eye rather than resting on fill alone.
        ui.painter().rect_filled(
            Rect::from_min_size(rect.left_top(), vec2(2.0, rect.height())),
            CornerRadius::ZERO,
            crate::pages::mark_colour(mark),
        );
    }

    let indent = rect.left() + 4.0 + row.depth as f32 * INDENT;
    let mut cursor = indent;

    // The chevron opens and closes the folder on a single click, the way the
    // disclosure triangle does in every file tree. It is registered after the
    // row, so it takes the click rather than the row's select — which is what
    // lets a single click on the name mean "select" without also making the
    // chevron useless.
    let mut chevron_clicked = false;
    if row.is_folder {
        let chevron = Rect::from_center_size(pos2(cursor + 7.0, rect.center().y), vec2(14.0, 14.0));
        // A little wider than it is drawn: 14 points is a small target, and
        // missing it selects the row instead, which is the wrong thing.
        let target = chevron.expand2(vec2(3.0, 2.0));
        let hit = ui.interact(
            target,
            ui.id().with(("chevron_hit", &row.path)),
            Sense::click(),
        );
        chevron_clicked = hit.clicked();
        if hit.hovered() {
            ui.ctx()
                .set_cursor_icon(eframe::egui::CursorIcon::PointingHand);
        }

        let openness = ui
            .ctx()
            .animate_bool(ui.id().with(("chevron", &row.path)), row.expanded);
        icons::chevron(
            ui.painter(),
            chevron,
            if hit.hovered() {
                theme::TEXT
            } else {
                theme::TEXT_MUTED
            },
            openness,
        );
    }
    cursor += 16.0;

    let icon_rect = Rect::from_center_size(pos2(cursor + 8.0, rect.center().y), vec2(16.0, 16.0));
    if row.is_folder {
        icons::folder(ui.painter(), icon_rect, theme::ACCENT, row.expanded);
    } else {
        icons::image_file(ui.painter(), icon_rect, theme::TEXT_MUTED);
    }
    cursor += 22.0;

    // Leave room for the child count before eliding the name. With pictures
    // marked inside, it reads `44 / 123` — how much of this folder is in the
    // batch, and how much there is.
    let count = (row.is_folder && row.child_count > 0).then(|| {
        if row.selected_count > 0 {
            format!("{} / {}", row.selected_count, row.child_count)
        } else {
            row.child_count.to_string()
        }
    });
    let reserved = count.as_ref().map_or(6.0, |count| {
        ui.ctx()
            .fonts_mut(|f| {
                f.layout_no_wrap(count.clone(), theme::small_caps_font(), theme::TEXT_DIM)
            })
            .size()
            .x
            + 14.0
    });

    let text_color = if busy {
        // Greyed while its contents are still arriving, so it looks as inert
        // as it behaves.
        theme::TEXT_DIM
    } else if row.georeferenced && map_support {
        // Brighter when selected, so being georeferenced never costs the row
        // its usual "this is the one" contrast. Only while there is a map at
        // all: with the switch off nothing else on screen mentions positions,
        // so a scattering of green rows is a code nothing explains — and the
        // sweep that sets the flag does not run either, which would make it a
        // stale one.
        if selected {
            theme::GEOREFERENCED
        } else {
            theme::GEOREFERENCED.gamma_multiply(0.85)
        }
    } else if row.is_folder || selected {
        theme::TEXT
    } else {
        theme::TEXT_MUTED
    };
    ui.painter().text(
        pos2(cursor, rect.center().y),
        Align2::LEFT_CENTER,
        widgets::elide(
            ui,
            &row.name,
            theme::label_font(),
            rect.right() - cursor - reserved,
        ),
        theme::label_font(),
        text_color,
    );

    if let Some(count) = count {
        ui.painter().text(
            pos2(rect.right() - 6.0, rect.center().y),
            Align2::RIGHT_CENTER,
            count,
            theme::small_caps_font(),
            theme::TEXT_DIM,
        );
    }

    // Images get the same menu here as in the Mosaic. Folders get their own,
    // with nothing destructive on it: "Move to Trash" on a folder full of
    // pictures is not something this application is going to offer by accident.
    match tree.get(&row.path).map(|node| &node.kind) {
        Some(Kind::Image(meta)) => {
            if let Some(file) = meta.path.as_deref().map(std::path::PathBuf::from) {
                file_context_menu(
                    ui,
                    &response,
                    &file,
                    batch,
                    stack_menu,
                    in_stack,
                    map_support,
                    out,
                );
            }
        }
        Some(Kind::Folder) => {
            let node = tree.get(&row.path);
            if let Some(folder) = node.and_then(Node::path).map(std::path::PathBuf::from) {
                let nested = node.map_or(0, |node| node.nested_images);
                folder_context_menu(ui, &response, &folder, nested, map_support, refresh, out);
            }
        }
        None => {}
    }

    if response.secondary_clicked() {
        return RowAction::Menu;
    }
    // Opening a folder is a double-click, the chevron, or the right arrow. A
    // single click on the name only selects — which is what makes
    // click-then-shift-click pick a range of folders without unfolding every
    // one of them on the way.
    if chevron_clicked || (response.double_clicked() && row.is_folder) {
        return RowAction::Toggle;
    }
    if response.clicked() {
        return RowAction::Select;
    }
    RowAction::None
}

// ----------------------------------------------------------------- INFO panel

/// Every tab the INFO panel can carry, in order.
///
/// This is the numbering `app.info_tab` is in, and the one the saved tab index
/// means — MAP and IMAGES come out of the middle when map support is off, so a
/// position in the shown list is not a tab name from one run to the next.
pub const INFO_TABS: &[&str] = &["METADATA", "MAP", "IMAGES", "STATS", "DEBUG"];

/// The tabs actually shown. MAP and IMAGES are both about the basemap: one is
/// where the picture sits on it, the other what else covers the same view.
pub fn info_tabs(show_debug: bool, map_support: bool) -> Vec<&'static str> {
    INFO_TABS
        .iter()
        .copied()
        .filter(|tab| match *tab {
            "DEBUG" => show_debug,
            "MAP" | "IMAGES" => map_support,
            _ => true,
        })
        .collect()
}

/// Every collapsible group, so their state can be saved and restored.
pub const DEBUG_SECTIONS: &[(&str, bool)] = &[
    ("LOADING · THIS IMAGE", true),
    ("RENDERER", false),
    ("PROCESS MEMORY", false),
    ("FRAME", false),
    ("DECODER", false),
    ("THUMBNAILS", false),
    ("LIBRARY", false),
];

pub fn info_panel(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    widgets::panel_header(ui, "INFO");

    let node = app.files.selected_node().cloned();

    // No filename heading: the Viewer's own header carries it now, across the
    // full window rather than elided into 268 pt. `title` is still used below,
    // where STATS wants the name for the placeholder values it derives.
    let tabs = info_tabs(app.config.show_debug_tab, app.config.map_support);
    // Cleared unless the IMAGES tab is up to set it again below: a map left
    // outlined on the basemap by a row nobody can see any more is a mark with
    // nothing to trace it back to.
    app.map_row_hovered = None;
    // `info_tab` numbers `INFO_TABS`; the strip numbers what is on screen. A
    // tab that has just been taken away falls back to the first rather than to
    // whichever tab inherited its position.
    let name = INFO_TABS.get(app.info_tab).copied().unwrap_or(INFO_TABS[0]);
    let mut shown = tabs.iter().position(|tab| *tab == name).unwrap_or_default();
    widgets::tab_strip(ui, &mut shown, &tabs);

    let tab = tabs[shown];
    app.info_tab = INFO_TABS
        .iter()
        .position(|other| *other == tab)
        .unwrap_or_default();
    let image = node.as_ref().and_then(|node| match &node.kind {
        Kind::Image(meta) => Some(meta.clone()),
        Kind::Folder => None,
    });
    // The DEBUG tab reports the process, so it is useful with nothing selected.
    // Read before the tabs borrow `app`; the STATS tab reports it rather than
    // guessing at one.
    let profile = app.current_meta.icc.clone();
    let here = app.files.images_for_mosaic(false).len();
    let below = app.files.images_for_mosaic(true).len();
    let folder_counts = node.as_ref().filter(|node| node.is_folder()).map(|node| {
        (
            here,
            below,
            node.children
                .iter()
                .filter(|child| child.is_folder())
                .count(),
        )
    });

    ScrollArea::vertical()
        .auto_shrink([false, false])
        // Each tab answers for itself when there is no image. Folding them
        // together made EXIF and STATS print the same paragraph, which read as
        // the tabs being broken rather than as nothing being selected.
        .show(ui, |ui| match tab {
            "DEBUG" => debug_tab(ui, app, image.as_ref()),
            "MAP" => map_tab(ui, app),
            "IMAGES" => images_tab(ui, app),
            "METADATA" => match &image {
                Some(_) => metadata_tab(ui, app),
                None => exif_empty(ui),
            },
            // One tab rather than two. A separate STACK tab was empty whenever
            // nothing was stacked, and STATS was the tab that went quiet when
            // something was — so each was blank exactly when the other had
            // something to say.
            _ => {
                let stacked = stack_sections(ui, app);
                match (&image, folder_counts) {
                    (Some(meta), _) => {
                        if stacked {
                            widgets::section_label(ui, "SELECTED PICTURE");
                        }
                        stats_tab(ui, meta, profile.as_deref());
                    }
                    (None, Some((direct, recursive, subfolders))) => {
                        folder_summary(ui, direct, recursive, subfolders)
                    }
                    (None, None) => empty_note(ui, "Select an image in the FILES panel"),
                }
            }
        });
}

/// The maps covering what the basemap is showing, oldest at the top.
///
/// The list the timeline counts, written out by name — the strip says *how
/// many* maps there are for a stretch of years, which is the wrong shape of
/// answer once you want to know *which*.
///
/// Each row is its own fader: dragging sideways across it sets how opaque that
/// map is drawn over the basemap, and the filled part of the row is that
/// amount. A row at nothing is a map not drawn at all, so the list doubles as
/// what is on the canvas — there is no separate switch to keep in step with it.
fn images_tab(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    if app.maps_in_view.is_empty() {
        empty_note(
            ui,
            "Open the MAP page — this lists the maps covering what it shows",
        );
        return;
    }

    let rows = maps_for_list(&app.maps_in_view, app.timeline_span, app.timeline);

    if rows.is_empty() {
        empty_note(ui, "No maps in the years the timeline is showing");
        return;
    }

    widgets::section_label(ui, "OVER THIS VIEW");
    // The two ways of working, and it is one or the other: the strip setting
    // every row and a hand setting one row cannot both be the last word, so
    // taking hold of a row below switches the strip off rather than fighting it
    // for a frame and winning.
    widgets::toggle(ui, &mut app.timeline_fades, "Fade with the timeline");
    hint(
        ui,
        if app.timeline_fades {
            "Scroll the timeline to walk the fade through the years.              Ctrl+wheel over it spreads the years out, so fewer maps              are mixed at once. Drag a row to take it over by hand."
        } else {
            "Drag a row sideways to fade its map in and out."
        },
    );
    ui.add_space(4.0);

    // Cleared here and refilled by the rows below, so a row the pointer has
    // left stops marking its map on the very next frame.
    let mut hovered: Option<std::path::PathBuf> = None;

    // Collected rather than applied inside the loop: acting on a row needs the
    // whole app, and the rows are being read off it at the same time.
    let mut acted: Option<(std::path::PathBuf, MapRowAction)> = None;

    for (year, path) in &rows {
        let opacity = app.map_opacity_of(path);
        let marked = app.maps_under_pointer.contains(path);
        let action = images_row(ui, path, *year, opacity, marked);
        if action.hovered {
            hovered = Some(path.clone());
        }
        if action.acted() {
            acted = Some((path.clone(), action));
        }
    }

    if let Some((path, action)) = acted {
        let now = ui.input(|input| input.time);
        if let Some(opacity) = action.opacity {
            // A hand on a control beats a rule about it.
            app.timeline_fades = false;
            app.set_map_opacity(&path, opacity);
        }
        // Switched off and back on, the same as double-clicking the sheet
        // itself out on the tiles.
        if action.double_clicked {
            app.toggle_map_mute(&path);
        } else if action.clicked {
            // Held back a moment: the first click of a double looks exactly
            // like this one until the moment for the second has passed.
            app.click_on_map(&path, now);
        }
    }
    app.map_row_hovered = hovered;
    ui.add_space(8.0);
}

/// Which maps the IMAGES list shows, oldest first.
///
/// Cut to the stretch of years the timeline is showing, but only while the
/// timeline is up: the strip is the control for this list, so a row for a year
/// scrolled off it is a map the user has just said they are not looking for.
/// With the strip hidden nothing has been said, so nothing is cut.
///
/// An undated map goes last rather than being read as year zero, which would
/// file every one of them above the oldest real sheet.
fn maps_for_list(
    in_view: &[(Option<i32>, std::path::PathBuf)],
    (first, last): (i32, i32),
    timeline_shown: bool,
) -> Vec<(Option<i32>, std::path::PathBuf)> {
    let mut rows: Vec<(Option<i32>, std::path::PathBuf)> = in_view
        .iter()
        .filter(|(year, _)| {
            !timeline_shown || year.is_none_or(|year| year >= first && year <= last)
        })
        .cloned()
        .collect();
    rows.sort_by(|left, right| match (left.0, right.0) {
        (Some(a), Some(b)) => a.cmp(&b).then_with(|| left.1.cmp(&right.1)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left.1.cmp(&right.1),
    });
    rows
}

/// What a row of the IMAGES list was asked to do this frame.
#[derive(Default)]
struct MapRowAction {
    hovered: bool,
    /// A sideways drag, and where it left the fader.
    opacity: Option<f32>,
    /// Select the file this map came from.
    clicked: bool,
    /// Switch the map off, or back on at what it was.
    double_clicked: bool,
}

impl MapRowAction {
    /// Whether anything happened worth carrying out of the loop.
    fn acted(&self) -> bool {
        self.opacity.is_some() || self.clicked || self.double_clicked
    }
}

/// One row of the IMAGES list.
///
/// The background is painted into a slot reserved *before* the text is laid
/// out: painted afterwards it would cover the name, and painted into the
/// enclosing panel it would escape the scrolling area and sit over whatever
/// happened to be beneath.
fn images_row(
    ui: &mut Ui,
    path: &std::path::Path,
    year: Option<i32>,
    opacity: f32,
    marked: bool,
) -> MapRowAction {
    let name = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let width = ui.available_width();

    // Wrapped rather than cut short: these names run to eighty characters and
    // what tells two sheets of the same survey apart is the end of them.
    let galley = ui.ctx().fonts_mut(|fonts| {
        fonts.layout(
            name.clone(),
            theme::label_font(),
            if opacity > 0.0 {
                theme::TEXT
            } else {
                theme::TEXT_MUTED
            },
            width - 46.0,
        )
    });
    let height = galley.size().y + 8.0;

    let background = ui.painter().add(eframe::egui::Shape::Noop);
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click_and_drag());

    let mut action = MapRowAction {
        hovered: response.hovered(),
        clicked: response.clicked(),
        double_clicked: response.double_clicked(),
        ..Default::default()
    };
    // One row's width is the whole range, so the pointer moves the level
    // one-for-one and a full sweep takes it end to end.
    if response.dragged() {
        action.opacity = Some(opacity + response.drag_delta().x / rect.width().max(1.0));
    }
    if response.hovered() || response.dragged() {
        ui.ctx()
            .set_cursor_icon(eframe::egui::CursorIcon::ResizeHorizontal);
    }

    let mut behind = vec![eframe::egui::Shape::rect_filled(
        rect,
        CornerRadius::same(4),
        if response.hovered() {
            theme::SURFACE
        } else {
            theme::PANEL_BG
        },
    )];
    let filled = rect.width() * opacity.clamp(0.0, 1.0);
    if filled > 0.5 {
        behind.push(eframe::egui::Shape::rect_filled(
            Rect::from_min_size(rect.min, vec2(filled, rect.height())),
            CornerRadius::same(4),
            theme::OPACITY_FILL,
        ));
    }
    ui.painter()
        .set(background, eframe::egui::Shape::Vec(behind));

    // The pointer is over this map out on the basemap. Outlined rather than
    // filled: the fill is already saying something else, and the two must stay
    // readable at once.
    if marked {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(4),
            eframe::egui::Stroke::new(1.5, theme::ACCENT),
            eframe::egui::StrokeKind::Inside,
        );
    }

    ui.painter()
        .galley(rect.min + vec2(6.0, 4.0), galley, theme::TEXT);
    if let Some(year) = year {
        ui.painter().text(
            pos2(rect.right() - 6.0, rect.top() + 4.0),
            Align2::RIGHT_TOP,
            year.to_string(),
            theme::small_caps_font(),
            theme::TEXT_DIM,
        );
    }
    ui.add_space(3.0);

    if response.hovered() {
        response.on_hover_text(format!(
            "{name}\n{:.0}% opaque\nClick to select the file  ·               double-click to switch it off  ·  drag to fade",
            opacity * 100.0
        ));
    }
    action
}

/// Flash tag 0x9209: bit 0 is whether it fired, bits 5-6 whether the camera
/// has one at all.
fn flash_label(value: u16) -> String {
    if value & 0b0010_0000 != 0 {
        return "No flash fitted".to_owned();
    }
    let fired = value & 1 != 0;
    let forced = (value >> 3) & 0b11;
    match (fired, forced) {
        (true, 1) => "Fired, forced".to_owned(),
        (false, 2) => "Suppressed".to_owned(),
        (true, _) => "Fired".to_owned(),
        (false, _) => "Did not fire".to_owned(),
    }
}

/// A row that disappears when the camera did not record the field, which reads
/// better than a column of dashes.
fn optional_row(ui: &mut Ui, label: &str, value: Option<String>) {
    if let Some(value) = value.filter(|text| !text.is_empty()) {
        widgets::kv_row(ui, label, &value);
    }
}

/// Where the photo was taken, and the way in to a street address.
///
/// The place name is resolved offline and shown without being asked for. The
/// address is not: it needs a request to a third party carrying this photo's
/// coordinates, so it happens on a click and never on its own.
fn location_section(
    ui: &mut Ui,
    app: &mut GeoRefMapperApp,
    latitude: f64,
    longitude: f64,
    altitude_m: Option<f64>,
) {
    widgets::section_label(ui, "LOCATION");

    // A camera that never got a lock writes zeroes, which is a real place in
    // the Atlantic. Say so rather than showing coordinates and offering to send
    // them somewhere — there is nothing there to look up.
    if !crate::geo::has_fix(latitude, longitude) {
        widgets::status_line(ui, "GPS recorded no fix for this photo.", theme::TEXT_DIM);
        // The altitude belongs to the same non-existent fix, and "0 m" reads
        // like sea level rather than like missing data.
        return;
    }

    if let Some(place) = &app.current_place {
        widgets::kv_row(ui, "Place", &place.summary());
        if place.town_is_distant() {
            widgets::status_line(
                ui,
                &format!("nearest town is {:.0} km away", place.town_distance_km),
                theme::TEXT_DIM,
            );
        }
    }

    optional_row(
        ui,
        "Latitude",
        Some(format!(
            "{:.5}° {}",
            latitude.abs(),
            if latitude < 0.0 { "S" } else { "N" }
        )),
    );
    optional_row(
        ui,
        "Longitude",
        Some(format!(
            "{:.5}° {}",
            longitude.abs(),
            if longitude < 0.0 { "W" } else { "E" }
        )),
    );

    optional_row(ui, "Altitude", altitude_m.map(|m| format!("{m:.0} m")));

    ui.add_space(4.0);
    match app.address.cached(latitude, longitude) {
        Some(Ok(address)) => widgets::kv_row(ui, "Address", address),
        Some(Err(error)) => widgets::status_line(ui, error, theme::CLOSE_HOVER),
        None => {
            let busy = app.address.is_busy();
            let label = if busy {
                "Looking up…"
            } else {
                "Look up address"
            };
            if widgets::button(ui, label, 190.0, !busy)
                .on_hover_text(
                    "Sends just this photo's coordinates to OpenStreetMap's Nominatim \
                     service to get a street address. Nothing is sent until you click, \
                     and never for more than one photo at a time.",
                )
                .clicked()
            {
                app.address.request(latitude, longitude, ui.ctx().clone());
            }
        }
    }
}

/// The EXIF tab with nothing selected: the same shape, with the values blank.
///
/// Keeping the layout means the panel does not jump when an image is picked,
/// and it says what this tab is for without a paragraph of explanation.
fn exif_empty(ui: &mut Ui) {
    widgets::section_label(ui, "CAPTURE");
    for key in [
        "Camera",
        "Lens",
        "Focal length",
        "Aperture",
        "Shutter",
        "ISO",
    ] {
        widgets::kv_row(ui, key, "—");
    }
    widgets::section_label(ui, "IMAGE");
    for key in ["Date taken", "Flash", "Rotation", "Colour space"] {
        widgets::kv_row(ui, key, "—");
    }
    ui.add_space(10.0);
    hint(ui, "Select an image to fill this in.");
}

/// Everything the file says about itself: EXIF first, then the other four
/// containers, each shown only when it holds something.
fn metadata_tab(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let exif = app.current_exif.clone();
    let meta = app.current_meta.clone();

    if let Some(exif) = exif.as_ref() {
        exif_sections(ui, app, exif);
    }
    provenance(ui, exif.as_ref(), &meta, app.current_sidecar.as_ref());

    if exif.is_none() && meta.is_empty() {
        ui.add_space(6.0);
        hint(ui, "This file carries no metadata.");
    }
}

/// The EXIF text tags plus the four non-EXIF containers.
///
/// Each is its own group and each is skipped when empty, so a screenshot shows
/// one row and an edited photograph shows twenty — rather than both showing the
/// same page of dashes.
fn provenance(
    ui: &mut Ui,
    exif: Option<&crate::exif::Exif>,
    meta: &crate::meta::Metadata,
    sidecar: Option<&crate::sidecar::Sidecar>,
) {
    let mut already: Vec<String> = Vec::new();
    if let Some(exif) = exif {
        let rows = [
            ("Software", &exif.software),
            ("Description", &exif.description),
            ("Author", &exif.artist),
            ("Copyright", &exif.copyright),
            ("Computer", &exif.host_computer),
            ("Document", &exif.document_name),
        ];
        if rows.iter().any(|(_, value)| value.is_some()) {
            widgets::section_label(ui, "SOFTWARE");
            for (label, value) in rows {
                if let Some(value) = value {
                    widgets::kv_block(ui, label, value);
                    already.push(squashed(value));
                }
            }
        }
    }

    // The same sentence reaching the panel twice. A converted map carries its
    // description in both the plain TIFF tag that every other program reads and
    // the XMP packet, which is right in the file and wrong on screen — the
    // reader wants the description once, not a lesson in where it is kept.
    // Above the containers rather than inside one: which packet a keyword came
    // out of is a fact about the file, not about the map.
    tags_section(ui, meta, sidecar);

    pair_section(
        ui,
        "XMP",
        &without_keywords(&not_already_shown(&meta.xmp, &already)),
    );
    pair_section(ui, "IPTC", &without_keywords(&meta.iptc));
    if let Some(profile) = &meta.icc {
        widgets::section_label(ui, "COLOUR PROFILE");
        widgets::kv_row(ui, "Profile", profile);
    }
    pair_section(ui, "EMBEDDED TEXT", &meta.text);
}

/// The rows whose text has not already appeared higher up the panel.
///
/// Only the value is compared, not the label: the two containers give the same
/// sentence different names — `Document` and `Title` for one map's name — and
/// it is the sentence the reader recognises as a repeat.
fn not_already_shown(pairs: &crate::meta::Pairs, already: &[String]) -> crate::meta::Pairs {
    pairs
        .iter()
        .filter(|(_, value)| !already.contains(&squashed(value)))
        .cloned()
        .collect()
}

/// One spelling of a value, for deciding whether two rows say the same thing.
///
/// The two containers hold the same sentence with different padding — XMP is
/// cleaned of the line breaks the XML needs, the TIFF tag is not — so comparing
/// them as they arrive would call them different and show both.
fn squashed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Everything about where the selected picture is, and putting it on the map.
///
/// A tab of its own rather than a group inside METADATA: georeferencing is a
/// different question from what camera took a photograph, and there is a lot
/// more of it to come.
fn map_tab(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    let geo = app.current_geo.clone();
    let path = app.current.clone();

    // While placing, the panel is the controls for it and nothing else — the
    // rest would be about a position the picture does not have yet.
    if app.placing.is_some() {
        placing_controls(ui, app);
        ui.add_space(10.0);
        overlay_toggles(ui, app);
        return;
    }

    overlay_toggles(ui, app);
    anchors_table(ui, app);
    ui.add_space(10.0);
    placement_group(ui, app);
    ui.add_space(10.0);

    let Some(geo) = geo else {
        // A layered TIFF is already in the right shape for the map — squares
        // and smaller copies — so the only thing stopping it is that nobody has
        // said where it goes. Worth saying plainly, because "no position" and
        // "wrong kind of file" are different problems with different fixes.
        let layers = app.squares.shapes().len();
        if layers > 1 {
            let (width, height) = app
                .squares
                .shapes()
                .first()
                .map(|shape| (shape.width, shape.height))
                .unwrap_or((0, 0));
            widgets::section_label(ui, "READY, BUT UNPLACED");
            widgets::kv_row(ui, "Size", &format!("{width} × {height}"));
            widgets::kv_row(ui, "Copies", &format!("{layers} sizes inside the file"));
            ui.add_space(6.0);
            hint(
                ui,
                "This map is stored the way the map view wants it — in squares, \
                 with smaller copies for zooming out. It just does not say where \
                 on the earth it sits, so it cannot be placed yet.",
            );
            return;
        }

        empty_note(
            ui,
            "The selected picture does not say where it is.\n\nA georeferenced \
             TIFF — a scanned map, an aerial survey — carries its position, and \
             it can be laid over the map here.",
        );
        return;
    };

    // Only offer it when the projection is one the map agrees with; placing an
    // unknown projection would put the picture confidently in the wrong place,
    // which is worse than not offering.
    let placeable = geo.top_left().is_some() && geo.bottom_right().is_some();
    if !placeable {
        hint(
            ui,
            "This projection cannot be laid over the map — only Web Mercator and \
             plain latitude/longitude line up with the tiles.",
        );
        ui.add_space(8.0);
        georeference(ui, &geo);
        return;
    }

    // One toggle rather than two buttons: showing and hiding is one decision,
    // and a pair of buttons made it look like two.
    let shown = app
        .map_overlay
        .as_ref()
        .is_some_and(|overlay| Some(&overlay.path) == path.as_ref());

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        if widgets::button(ui, if shown { "HIDE" } else { "SHOW" }, 74.0, true).clicked() {
            // The same path the space bar takes, so the two cannot drift.
            app.toggle_on_map(ui.ctx());
        }
        ui.add_space(8.0);

        // Beside the toggle, and only live while there is something to fade.
        let remaining = ui.available_width();
        match (&mut app.map_overlay, shown) {
            (Some(overlay), true) => {
                // Slid in whole percent rather than over 0..1. `max_decimals`
                // rounds the *stored* value, so asking for no decimals on a
                // 0..1 range left the opacity able to hold only 0 or 1 — a
                // two-position switch wearing a slider's clothes. Percent gives
                // a hundred and one stops and reads the way the number is said.
                opacity_slider(ui, &mut overlay.opacity, remaining);
            }
            _ => {
                ui.add_space(remaining.max(0.0));
            }
        }
    });

    georeference(ui, &geo);

    ui.add_space(6.0);
    if widgets::button(ui, "Remove the position", 190.0, true)
        .on_hover_text("Takes the position tags out. The picture itself is untouched.")
        .clicked()
    {
        let now = ui.input(|i| i.time);
        app.clear_placement(now);
    }
}

/// The two sets of lines that can be drawn over the basemap.
///
/// Kept apart because they are different things, and the maps in this library
/// carry both. A **graticule** is latitude and longitude — tied to the earth,
/// the same lines on every map ever made. A **grid** is ruled in linear units
/// on a projection: these sheets carry a thousand-yard military survey grid,
/// whose lines mean nothing to any other projection.
///
/// Shown whether or not a picture is being placed, because the question "where
/// is this on the earth" is worth asking of the basemap on its own.
fn overlay_toggles(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    widgets::section_label(ui, "LINES OVER THE MAP");

    ui.checkbox(&mut app.graticule, "Graticule  (G)")
        .on_hover_text(
            "Meridians and parallels, labelled in degrees and minutes. The \
             spacing follows the zoom — degrees over the region, minutes over \
             the island, seconds over a street.",
        );

    // The switches first, and always both of them. The row below comes and
    // goes with them, so whichever way they are set they stay under the same
    // part of the panel — a control that moves when you use it is one you have
    // to find again every time.
    let mut hidden = app.anchors_shown.hidden();
    if widgets::toggle(ui, &mut hidden, "Hide anchors").changed() {
        app.anchors_shown = if hidden {
            crate::app::AnchorsShown::Hidden
        } else {
            app.anchors_shown_before_hiding
        };
        // Hiding locks. Nothing can be dragged that is not on screen anyway, so
        // an unlocked switch alongside hidden pins would be stating a freedom
        // that does not exist — and leaving it unlocked for whenever they come
        // back is the state this is here to prevent.
        if hidden {
            app.config.anchors_locked = true;
        }
    }
    ui.add_enabled_ui(!hidden, |ui| {
        widgets::toggle(ui, &mut app.config.anchors_locked, "Locked  ·  L")
            .on_disabled_hover_text("Hidden anchors cannot be dragged. Show them to unlock.");
    });

    // Which of them, while they are showing at all: a subset of nothing is not
    // a choice worth offering.
    if !hidden {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label("Anchors");
            for choice in [
                crate::app::AnchorsShown::All,
                crate::app::AnchorsShown::Used,
            ] {
                if ui
                    .selectable_value(&mut app.anchors_shown, choice, choice.label())
                    .on_hover_text(choice.why())
                    .clicked()
                {
                    app.anchors_shown_before_hiding = choice;
                }
            }
        });
    }
    hint(
        ui,
        if app.config.anchors_locked {
            "Pins cannot be dragged. An anchor several maps are placed by moves \
             all of them at once, and a nudge meant for the map is not noticed \
             until a sheet is in the wrong place."
        } else {
            "Pins can be dragged. Moving one moves every map placed by it."
        },
    );
}

/// Choosing which anchor to put down, while the two halves are up.
fn placing_controls(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    widgets::section_label(ui, "COPYING ANCHORS ONTO THIS PICTURE");
    let name = app
        .placing
        .as_ref()
        .and_then(|session| session.path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    widgets::kv_block(ui, "Picture", &name);
    let (down, total) = app
        .placing
        .as_ref()
        .map(|session| (session.placed.len(), app.anchors.len()))
        .unwrap_or((0, 0));
    widgets::kv_row(ui, "Marked", &format!("{down} of {total} anchors"));
    ui.add_space(4.0);
    hint(
        ui,
        "Each anchor you use is copied onto this picture with its own spot on \
         it. Pick one — from the list or by clicking it on the map — then click \
         the same place on the picture. Two is enough; more is better. To move \
         a mark afterwards, drag it. The shared anchors on the map are fixed \
         while this is open — one made just for this picture can be dragged to \
         where it belongs.",
    );
    // Offered from the start but only bites once two anchors are down, which is
    // when the pair of them first implies where every other pixel belongs.
    let ready = app
        .placing
        .as_ref()
        .is_some_and(|session| session.placed.len() >= 2);
    let mut locked = app.placing.as_ref().is_some_and(|session| session.locked);
    ui.add_space(6.0);
    if ui
        .checkbox(&mut locked, "Keep the two halves together")
        .on_hover_text(if ready {
            "Moving either half moves the other to the same ground, at the same \
             size. The turn is not matched — the basemap is always north up."
        } else {
            "Needs two anchors down. Until then there is no scale or turn to \
             hold them together with."
        })
        .changed()
        && let Some(session) = &mut app.placing
    {
        session.locked = locked;
    }
    let mut upright = app.placing.as_ref().is_some_and(|session| session.upright);
    if ui
        .checkbox(&mut upright, "Turn the scan to north")
        .on_hover_text(
            "Draws the picture turned so its north is up, like the basemap \
             beside it. The angle is solved from every anchor placed so far. \
             Turn it off to read the sheet's own writing.",
        )
        .changed()
        && let Some(session) = &mut app.placing
    {
        session.upright = upright;
    }

    ui.add_space(6.0);
    if widgets::button(ui, "Add one just for this picture", 210.0, true)
        .on_hover_text(
            "At the middle of the map. For a spot that only means something on \
             this scan — a printed cross, a building since gone. It is not \
             added to the shared list.",
        )
        .clicked()
    {
        let (lat, lon) = (app.map_view.lat, app.map_view.lon);
        let now = ui.input(|i| i.time);
        app.add_anchor_here_only(lat, lon, now);
    }
    ui.add_space(8.0);

    // Only the ones this picture is actually using, plus whichever is waiting to
    // be put down. The full list is every place ever marked across the island —
    // a page of names to scroll past to reach the three that matter here. The
    // way to bring another in is to click its pin on the basemap, which is
    // where recognising it happens anyway.
    let session_ids: std::collections::HashSet<i64> = app
        .placing
        .as_ref()
        .map(|session| session.placed.keys().copied().collect())
        .unwrap_or_default();
    let chosen_now = app.placing.as_ref().and_then(|session| session.choosing);
    let anchors: Vec<crate::db::Anchor> = app
        .anchors
        .iter()
        .filter(|held| session_ids.contains(&held.id) || chosen_now == Some(held.id))
        .cloned()
        .collect();
    if anchors.is_empty() {
        hint(
            ui,
            "None used yet. Click an anchor on the map to bring it in, or add \
             one just for this picture.",
        );
    }
    let mut choose: Option<i64> = None;
    let mut clear: Option<i64> = None;
    // Colours come from each anchor's place in the *full* list, not this
    // filtered one. The colour is the only thing tying a row to its pin on the
    // map and its mark on the scan, and it must not change as the list is
    // filtered — two anchors swapping colours mid-session is a placement
    // silently ruined.
    let order: Vec<i64> = app.anchors.iter().map(|held| held.id).collect();
    for anchor in &anchors {
        let index = order
            .iter()
            .position(|held| *held == anchor.id)
            .unwrap_or(0);
        let placed = app
            .placing
            .as_ref()
            .and_then(|session| session.placed.get(&anchor.id).copied());
        let chosen = app.placing.as_ref().and_then(|session| session.choosing) == Some(anchor.id);

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let colour = theme::anchor_colour(index);

            // The colour swatch is the whole point of the row: it is what ties
            // this line to the two marks on screen.
            let (swatch, _) = ui.allocate_exact_size(vec2(14.0, 20.0), Sense::hover());
            ui.painter().circle_filled(swatch.center(), 5.0, colour);

            let label = if chosen {
                format!("▸ {}", anchor.name)
            } else {
                anchor.name.clone()
            };
            let width = (ui.available_width() - 120.0).max(60.0);
            let (rect, response) = ui.allocate_exact_size(vec2(width, 20.0), Sense::click());
            ui.painter().text(
                pos2(rect.left(), rect.center().y),
                Align2::LEFT_CENTER,
                widgets::elide(ui, &label, theme::label_font(), width - 4.0),
                theme::label_font(),
                if chosen {
                    theme::TEXT
                } else {
                    theme::TEXT_MUTED
                },
            );
            if response.on_hover_text("Place this one").clicked() {
                choose = Some(anchor.id);
            }

            let (state, ink) = match placed {
                Some(_) => ("copied", colour),
                None => ("not yet", theme::TEXT_DIM),
            };
            let (rect, _) = ui.allocate_exact_size(vec2(88.0, 20.0), Sense::hover());
            ui.painter().text(
                pos2(rect.left(), rect.center().y),
                Align2::LEFT_CENTER,
                state,
                theme::small_caps_font(),
                ink,
            );

            if placed.is_some()
                && widgets::button(ui, "×", 24.0, true)
                    .on_hover_text("Remove this copy from the picture")
                    .clicked()
            {
                clear = Some(anchor.id);
            }
        });
    }
    if let Some(id) = choose {
        if let Some(session) = &mut app.placing {
            session.choosing = Some(id);
        }
        // And bring it into view. Choosing one from the list is otherwise a
        // guessing game whenever it sits outside the map's current corner —
        // which, on a list built up across a whole island, is most of them.
        if let Some(anchor) = anchors.iter().find(|held| held.id == id) {
            app.map_view.centre_on(anchor.lat, anchor.lon);
            // And the picture with it. The half-following only runs when that
            // half was the one touched, which is right for gestures and wrong
            // here — this moved the basemap without anybody dragging it.
            app.picture_follows_map();
        }
    }
    if let Some(id) = clear {
        app.unmark_anchor(id);
    }

    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if widgets::button(ui, "Done", 90.0, down >= 2)
            .on_disabled_hover_text("At least two anchors have to be on the picture")
            .clicked()
        {
            let ctx = ui.ctx().clone();
            app.finish_placing(&ctx);
        }
        if widgets::button(ui, "Cancel", 90.0, true).clicked() {
            app.cancel_placing();
        }
    });
}

/// What the anchors add up to for this picture, and how well each one matched.
fn placement_group(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    widgets::collapsing_section(ui, "ANCHORS ON THIS PICTURE", true, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;

        let started = widgets::button(ui, "Copy anchors onto it", 180.0, app.current.is_some())
            .on_hover_text("Opens the basemap and the picture side by side")
            .clicked();
        if started {
            let ctx = ui.ctx().clone();
            app.start_placing(&ctx);
            return;
        }

        let Some(outcome) = app.placement.clone() else {
            ui.add_space(4.0);
            hint(
                ui,
                "No anchors have been copied onto this picture yet, so there is \
                 nothing to work a position out from.",
            );
            return;
        };
        let fit = match outcome {
            Ok(fit) => fit,
            Err(refused) => {
                ui.add_space(4.0);
                hint(ui, refused.why());
                return;
            }
        };

        ui.add_space(6.0);
        widgets::kv_row(
            ui,
            "Ground per pixel",
            &format!("{:.3} m", fit.placement.scale),
        );
        widgets::kv_row(
            ui,
            "Turned by",
            &format!("{:.2}°", fit.placement.rotation_degrees()),
        );
        // The numbers that would go into the file. Shown because a placement is
        // only useful once it can be written down, and a turn of any size means
        // the simple form cannot hold it.
        if fit.placement.rotation_degrees().abs() > 0.01 {
            widgets::kv_row(
                ui,
                "Stored as",
                "a full transformation, not a corner and a scale",
            );
        } else {
            widgets::kv_row(ui, "Stored as", "a corner and a scale");
        }
        widgets::kv_row(
            ui,
            "Typical error",
            &format!(
                "{:.1} m  ·  worst {:.1} m",
                fit.average_metres, fit.worst_metres
            ),
        );

        ui.add_space(8.0);
        let showing = app
            .map_overlay
            .as_ref()
            .is_some_and(|overlay| overlay.placement.is_some());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if widgets::button(ui, if showing { "HIDE" } else { "SHOW" }, 74.0, true)
                .on_hover_text("Lay it on the basemap using these anchors")
                .clicked()
            {
                let ctx = ui.ctx().clone();
                app.show_placement_on_map(&ctx);
            }
            ui.add_space(8.0);

            let remaining = ui.available_width();
            match (&mut app.map_overlay, showing) {
                (Some(overlay), true) => {
                    opacity_slider(ui, &mut overlay.opacity, remaining);
                }
                _ => ui.add_space(remaining.max(0.0)),
            }
        });

        ui.add_space(6.0);
        if widgets::button(ui, "Write it into the file", 190.0, true)
            .on_hover_text(
                "Adds the position tags. No pixel is touched and nothing is re-compressed.",
            )
            .clicked()
        {
            let now = ui.input(|i| i.time);
            app.save_placement(now);
        }
        ui.add_space(4.0);
        // Separate from "Remove the position", which takes the tags back out of
        // the file. This throws away the *working out* — the anchors this
        // picture was matched by — so it can be done again from nothing.
        if widgets::button(ui, "Undo the whole mapping", 190.0, true)
            .on_hover_text(
                "Takes every anchor back off this picture so it can be mapped \
                 again. The file's own position, if one was written, stays until \
                 it is removed separately.",
            )
            .clicked()
        {
            let now = ui.input(|i| i.time);
            app.clear_mapping(now);
            return;
        }
        ui.add_space(4.0);

        // Judged against the size of the map: the same error means something
        // different on a street plan and on a map of a whole island.
        let extent = app
            .squares
            .shapes()
            .first()
            .map(|shape| f64::from(shape.width) * fit.placement.scale)
            .unwrap_or(0.0);

        ui.add_space(8.0);
        widgets::section_label(ui, "HOW WELL EACH ONE MATCHED");
        let placed = app.placed_anchors.clone();
        let order: Vec<i64> = app.anchors.iter().map(|anchor| anchor.id).collect();
        // Collected rather than acted on inside the loop, which is borrowing the
        // list being drawn.
        let mut drop_anchor: Option<i64> = None;
        for residual in &fit.residuals {
            let Some(anchor) = placed.iter().find(|held| held.id == residual.id) else {
                continue;
            };
            let index = order
                .iter()
                .position(|held| *held == residual.id)
                .unwrap_or(0);
            let quality = crate::anchor::quality(residual.metres, extent);

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let (swatch, _) = ui.allocate_exact_size(vec2(14.0, 18.0), Sense::hover());
                ui.painter()
                    .circle_filled(swatch.center(), 5.0, theme::anchor_colour(index));

                // What follows on this row: the distance in a cell 86 wide, the
                // × button at 24, and the spacing between the three. Reserve
                // too little and the row overruns — and in a panel that sizes
                // to its contents, the overrun becomes the panel's new width
                // and it grows every frame.
                let width = (ui.available_width() - 120.0).max(60.0);
                let (rect, _) = ui.allocate_exact_size(vec2(width, 18.0), Sense::hover());
                ui.painter().text(
                    pos2(rect.left(), rect.center().y),
                    Align2::LEFT_CENTER,
                    widgets::elide(ui, &anchor.name, theme::label_font(), width - 4.0),
                    theme::label_font(),
                    theme::TEXT_MUTED,
                );

                let (rect, response) = ui.allocate_exact_size(vec2(86.0, 18.0), Sense::hover());
                ui.painter().text(
                    pos2(rect.left(), rect.center().y),
                    Align2::LEFT_CENTER,
                    format!("{:.0} m", residual.metres),
                    theme::small_caps_font(),
                    theme::accuracy_colour(quality),
                );
                response.on_hover_text(
                    "How far this anchor lands from where it was said to be. \
                     A large one usually means it was clicked in the wrong spot.",
                );

                // Right beside the number that condemns it: the row says this
                // one is wrong, and this is how to take it off.
                if widgets::button(ui, "×", 24.0, true)
                    .on_hover_text(
                        "Take this anchor off the picture and work the position out again",
                    )
                    .clicked()
                {
                    drop_anchor = Some(residual.id);
                }
            });
        }

        if let Some(id) = drop_anchor {
            app.unplace_anchor(id);
        }
    });
}

/// The transparency slider, with the number beside it.
///
/// `show_value` is deliberately off. Its box is sized to whatever is in it, so
/// it is wider at "100%" than at "0%" — and in a panel that sizes to its
/// contents, that extra width feeds straight back into the width the slider is
/// given, and the panel grows as the slider is dragged right. The number is
/// drawn here instead, in a cell of a fixed size, so nothing on this row
/// changes width with the value.
fn opacity_slider(ui: &mut Ui, opacity: &mut f32, room: f32) {
    const NUMBER: f32 = 46.0;
    let mut percent = (*opacity * 100.0).round();

    ui.spacing_mut().slider_width = (room - NUMBER - 6.0).clamp(60.0, 400.0);
    if ui
        .add(
            eframe::egui::Slider::new(&mut percent, 0.0..=100.0)
                .step_by(1.0)
                .show_value(false),
        )
        .changed()
    {
        *opacity = percent as f32 / 100.0;
    }

    ui.add_space(6.0);
    let (rect, _) = ui.allocate_exact_size(vec2(NUMBER, 20.0), Sense::hover());
    ui.painter().text(
        pos2(rect.right(), rect.center().y),
        Align2::RIGHT_CENTER,
        format!("{percent:.0}%"),
        theme::small_caps_font(),
        theme::TEXT_MUTED,
    );
}

/// Marked places, as a compact table.
///
/// One row each: the name, which can be typed over, where it is, and a way to
/// remove it. Names are written as they are typed rather than on some separate
/// confirmation — there is nothing to get wrong and nothing to lose.
fn anchors_table(ui: &mut Ui, app: &mut GeoRefMapperApp) {
    // Folded to begin with. The list is for when an anchor needs renaming or
    // taking out, which is occasional — open by default it pushed the switches
    // and the placement controls off the bottom of the panel on any library
    // with a few marked places in it.
    widgets::collapsing_section(ui, "ANCHORS", false, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;

        if widgets::button(ui, "Add one here", 130.0, true)
            .on_hover_text("At the middle of the map. Right-click the map to place one exactly.")
            .clicked()
        {
            let (lat, lon) = (app.map_view.lat, app.map_view.lon);
            app.add_anchor(lat, lon);
        }

        if app.anchors.is_empty() {
            ui.add_space(4.0);
            hint(
                ui,
                "None yet. Right-click anywhere on the map to mark a place, then                  drag it to move it.",
            );
            return;
        }

        ui.add_space(6.0);
        let pins = app.anchors.clone();
        // Collected rather than applied inside the loop: the rows borrow the
        // list they are drawn from.
        let mut renamed: Vec<(i64, String)> = Vec::new();
        let mut go_to: Option<(f64, f64)> = None;
        let mut remove: Option<i64> = None;

        for pin in &pins {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;

                let mut name = pin.name.clone();
                let width = (ui.available_width() - 190.0).max(70.0);
                if ui
                    .add(
                        eframe::egui::TextEdit::singleline(&mut name)
                            .desired_width(width)
                            .font(theme::label_font()),
                    )
                    .changed()
                {
                    renamed.push((pin.id, name));
                }

                let place = format!("{:.4}, {:.4}", pin.lat, pin.lon);
                let (rect, response) = ui.allocate_exact_size(vec2(120.0, 20.0), Sense::click());
                ui.painter().text(
                    pos2(rect.left(), rect.center().y),
                    Align2::LEFT_CENTER,
                    &place,
                    theme::small_caps_font(),
                    theme::TEXT_MUTED,
                );
                if response
                    .on_hover_text("Click to bring the map here")
                    .clicked()
                {
                    go_to = Some((pin.lat, pin.lon));
                }

                if widgets::button(ui, "×", 26.0, true)
                    .on_hover_text("Remove this anchor")
                    .clicked()
                {
                    remove = Some(pin.id);
                }
            });
        }

        for (id, name) in renamed {
            app.rename_anchor(id, &name);
        }
        if let Some((lat, lon)) = go_to {
            app.map_view.lat = lat;
            app.map_view.lon = lon;
            app.go_to_page(crate::pages::PAGE_MAP);
        }
        if let Some(id) = remove {
            app.delete_anchor(id);
        }
    });
}

/// Where a scanned map sits on the earth, for a TIFF that says so.
fn georeference(ui: &mut Ui, geo: &crate::geotiff::GeoRef) {
    widgets::collapsing_section(ui, "GEOREFERENCE", false, |ui| {
        // Tighter than the default: this group is a list of short pairs, and
        // the standard row spacing made it twice as tall as it needed to be.
        ui.spacing_mut().item_spacing.y = 1.0;
        // One row each. The file states these separately, and which is the
        // projection and which the datum it sits on is not something this
        // reader establishes — running them together invented a relationship
        // the file never stated.
        match geo.citations.split_first() {
            Some((first, rest)) => {
                widgets::kv_block(ui, "Coordinate system", first);
                for other in rest {
                    widgets::kv_block(ui, "", other);
                }
            }
            None => widgets::kv_block(ui, "Coordinate system", "unstated"),
        }
        widgets::kv_row(
            ui,
            "EPSG",
            &geo.epsg
                .map(|code| code.to_string())
                .unwrap_or_else(|| "—".to_owned()),
        );

        // Degrees when the projection is one we can undo; otherwise the projected
        // numbers as stored, which is honest rather than wrong.
        match (geo.top_left(), geo.bottom_right()) {
            (Some((north, west)), Some((south, east))) => {
                widgets::kv_block(ui, "North-west", &format!("{north:.6}, {west:.6}"));
                widgets::kv_block(ui, "South-east", &format!("{south:.6}, {east:.6}"));
                if let Some((lat, lon)) = geo.centre() {
                    widgets::kv_block(ui, "Centre", &format!("{lat:.6}, {lon:.6}"));
                }
            }
            _ => {
                widgets::kv_block(
                    ui,
                    "Origin",
                    &format!("{:.3}, {:.3}", geo.origin.0, geo.origin.1),
                );
            }
        }

        match geo.metres_per_pixel() {
            Some(metres) if metres < 1.0 => {
                widgets::kv_row(ui, "Ground per pixel", &format!("{:.1} cm", metres * 100.0));
            }
            Some(metres) => widgets::kv_row(ui, "Ground per pixel", &format!("{metres:.2} m")),
            None => widgets::kv_row(ui, "Pixel scale", &format!("{:.6}°", geo.scale.0)),
        }
        if let Some((across, down)) = geo.extent_metres() {
            widgets::kv_row(
                ui,
                "Covers",
                &format!("{:.2} × {:.2} km", across / 1000.0, down / 1000.0),
            );
        }
        if geo.transformed {
            // The simple model cannot describe this one, and pretending otherwise
            // would put the map in the wrong place.
            ui.add_space(4.0);
            hint(
                ui,
                "This raster carries a full transformation matrix — it may be \
             rotated or skewed, which the tiepoint alone does not describe.",
            );
        }
    });
}

/// The keywords a picture carries, as their own group of chips.
///
/// They arrive joined into one line — `survey · mukim · photo-cd · postwar` —
/// which is how every other metadata row is shaped and the wrong shape for
/// these. A tag is a thing in its own right, and eight of them run past the
/// width of the panel and get elided away to nothing.
///
/// Taken from XMP and IPTC together and folded into one set: a file written by
/// two programs can carry the same word twice, and nobody wants to read it
/// twice.
fn tags_section(
    ui: &mut Ui,
    meta: &crate::meta::Metadata,
    sidecar: Option<&crate::sidecar::Sidecar>,
) {
    let mut tags: Vec<String> = Vec::new();
    // The sidecar first, because on an original it is the only place they are:
    // the tags do not move into the picture until it is converted, and a map is
    // being judged by them long before that.
    if let Some(sidecar) = sidecar {
        for tag in &sidecar.keywords {
            if !tags.iter().any(|held| held == tag) {
                tags.push(tag.clone());
            }
        }
    }
    for pairs in [&meta.xmp, &meta.iptc] {
        for (label, value) in pairs {
            if label != "Keywords" {
                continue;
            }
            // Both separators: ours joins with a middle dot, IPTC writers
            // usually use commas.
            for tag in value.split(['·', ',']) {
                let tag = tag.trim();
                if !tag.is_empty() && !tags.iter().any(|held| held == tag) {
                    tags.push(tag.to_owned());
                }
            }
        }
    }
    if tags.is_empty() {
        return;
    }

    widgets::collapsing_section(ui, "TAGS", true, |ui| {
        ui.add_space(2.0);
        let font = theme::small_caps_font();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            for tag in &tags {
                let galley = ui
                    .painter()
                    .layout_no_wrap(tag.clone(), font.clone(), theme::TEXT);
                let (rect, response) =
                    ui.allocate_exact_size(galley.size() + vec2(14.0, 6.0), Sense::hover());
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(9), theme::SURFACE);
                ui.painter()
                    .galley(rect.min + vec2(7.0, 3.0), galley, theme::TEXT);
                response.on_hover_text("From the picture's own keywords");
            }
        });
        ui.add_space(4.0);
    });
}

/// One `Keywords` row is shown as chips above, so it is dropped from the list
/// it came in — the same sentence twice in two shapes is worse than either.
fn without_keywords(pairs: &crate::meta::Pairs) -> crate::meta::Pairs {
    pairs
        .iter()
        .filter(|(label, _)| label != "Keywords")
        .cloned()
        .collect()
}

fn pair_section(ui: &mut Ui, title: &str, pairs: &crate::meta::Pairs) {
    if pairs.is_empty() {
        return;
    }
    widgets::section_label(ui, title);
    for (label, value) in pairs {
        widgets::kv_block(ui, label, value);
    }
}

fn exif_sections(ui: &mut Ui, app: &mut GeoRefMapperApp, exif: &crate::exif::Exif) {
    let has_capture = exif.camera().is_some()
        || exif.lens.is_some()
        || exif.focal_length_mm.is_some()
        || exif.aperture.is_some()
        || exif.shutter_label().is_some()
        || exif.iso.is_some();
    if !has_capture {
        return exif_image_section(ui, app, exif);
    }

    widgets::section_label(ui, "CAPTURE");
    optional_row(ui, "Camera", exif.camera());
    optional_row(ui, "Lens", exif.lens.clone());
    optional_row(
        ui,
        "Focal length",
        // Phone lenses are a couple of millimetres, where rounding to whole
        // numbers turns 1.9 mm into 2 mm and loses the only interesting digit.
        exif.focal_length_mm.map(|mm| {
            if mm < 10.0 {
                format!("{mm:.1} mm")
            } else {
                format!("{mm:.0} mm")
            }
        }),
    );
    optional_row(ui, "Aperture", exif.aperture.map(|f| format!("f/{f:.1}")));
    optional_row(ui, "Shutter", exif.shutter_label());
    optional_row(ui, "ISO", exif.iso.map(|iso| iso.to_string()));
    optional_row(
        ui,
        "Exposure bias",
        exif.exposure_bias_ev
            .filter(|ev| *ev != 0.0)
            .map(|ev| format!("{ev:+.1} EV")),
    );

    exif_image_section(ui, app, exif);
}

/// The IMAGE rows and, when the file knows where it was taken, LOCATION.
///
/// Split out because a file can have these without any CAPTURE fields at all —
/// a scan or an export keeps its orientation and colour space long after every
/// trace of a camera has gone.
fn exif_image_section(ui: &mut Ui, app: &mut GeoRefMapperApp, exif: &crate::exif::Exif) {
    widgets::section_label(ui, "IMAGE");
    optional_row(ui, "Date taken", exif.taken_at.clone());
    optional_row(
        ui,
        "White balance",
        exif.white_balance.map(|value| {
            match value {
                0 => "Auto",
                1 => "Manual",
                _ => "Unknown",
            }
            .to_owned()
        }),
    );
    optional_row(ui, "Flash", exif.flash.map(flash_label));
    // Always shown: it is the one field that changes what appears on screen.
    // Called "Rotation" rather than "Orientation" because STATS already has an
    // Orientation row meaning portrait-or-landscape, and two rows of the same
    // name meaning different things is worse than a slightly loose word.
    widgets::kv_block(ui, "Rotation", exif.orientation.label());
    optional_row(
        ui,
        "Colour space",
        exif.colour_space.map(|value| {
            match value {
                1 => "sRGB",
                2 => "Adobe RGB",
                0xFFFF => "Uncalibrated",
                _ => "Unknown",
            }
            .to_owned()
        }),
    );

    if let Some(gps) = &exif.gps
        && let (Some(latitude), Some(longitude)) = (gps.latitude, gps.longitude)
    {
        let altitude = gps.altitude_m;
        location_section(ui, app, latitude, longitude, altitude);
    }
}

/// What is in the stack the selection belongs to, above the picture's own
/// figures. Returns whether anything was drawn.
///
/// A stack is a different subject from the picture in front of it — reporting
/// the cover's dimensions as though they were the stack's was the confusion
/// that started this — but they are different *sections*, not different tabs.
fn stack_sections(ui: &mut Ui, app: &mut GeoRefMapperApp) -> bool {
    let Some((stack, members)) = app.selected_stack() else {
        return false;
    };

    let bytes: u64 = members.iter().map(|(_, meta)| meta.bytes).sum();
    let mut formats: Vec<String> = members
        .iter()
        .map(|(_, meta)| meta.format.clone())
        .collect();
    formats.sort();
    formats.dedup();
    let pixels: u64 = members
        .iter()
        .map(|(_, meta)| meta.width as u64 * meta.height as u64)
        .sum();

    widgets::section_label(ui, "STACK");
    widgets::kv_row(ui, "Pictures", &members.len().to_string());
    widgets::kv_row(ui, "Combined size", &format_bytes(bytes));
    widgets::kv_row(
        ui,
        "Total pixels",
        &format!("{:.1} MP", pixels as f64 / 1_000_000.0),
    );
    widgets::kv_block(ui, "Formats", &formats.join(", "));

    let open = app.open_stack == Some(stack);
    ui.add_space(6.0);
    if widgets::button(
        ui,
        if open { "Close stack" } else { "Open stack" },
        150.0,
        true,
    )
    .clicked()
    {
        app.open_stack = if open { None } else { Some(stack) };
        app.go_to_page(crate::pages::PAGE_MOSAIC);
    }

    widgets::section_label(ui, "MEMBERS");
    let current = app.selected_image_path();
    let mut pick: Option<std::path::PathBuf> = None;
    for (index, (path, meta)) in members.iter().enumerate() {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // The cover is named as such, since which one it is is the whole point
        // of the ordering.
        let label = if index == 0 {
            format!("{name}  ·  cover")
        } else {
            name
        };

        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
        let is_current = current.as_deref() == Some(path.as_path());
        if is_current || response.hovered() {
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(4),
                if is_current {
                    theme::ACCENT.gamma_multiply(0.22)
                } else {
                    theme::SURFACE_HOVER
                },
            );
        }
        ui.painter().text(
            pos2(rect.left() + 4.0, rect.center().y),
            Align2::LEFT_CENTER,
            widgets::elide(ui, &label, theme::label_font(), rect.width() - 70.0),
            theme::label_font(),
            if index == 0 {
                theme::TEXT
            } else {
                theme::TEXT_MUTED
            },
        );
        ui.painter().text(
            pos2(rect.right() - 4.0, rect.center().y),
            Align2::RIGHT_CENTER,
            format_bytes(meta.bytes),
            theme::small_caps_font(),
            theme::TEXT_DIM,
        );
        if response.clicked() {
            pick = Some(path.clone());
        }
    }
    if let Some(path) = pick
        && let Some(node) = app.files.find_by_path(&path)
    {
        app.files.selected = node;
        app.reveal_selection = true;
    }
    true
}

fn debug_tab(ui: &mut Ui, app: &GeoRefMapperApp, meta: Option<&ImageMeta>) {
    // The image currently on the canvas comes first and is open by default:
    // it is the group anyone opens this tab to look at.
    loading_group(ui, app, meta);

    widgets::collapsing_section(ui, "RENDERER", false, |ui| match &app.renderer {
        Some(renderer) => {
            widgets::kv_row(ui, "Adapter", &renderer.adapter_name);
            widgets::kv_row(ui, "Type", renderer.tier.label());
            // Live reading, not the one taken at startup.
            if let Some(memory) = app.vram.or(renderer.memory) {
                widgets::meter(
                    ui,
                    "Card memory free",
                    &format!("{} of {} MB", memory.free_mb(), memory.total_mb()),
                    memory.free_fraction(),
                    if memory.free_fraction() < 0.2 {
                        theme::CLOSE_HOVER
                    } else {
                        theme::ACCENT
                    },
                );
                widgets::kv_row(ui, "In use, all apps", &format!("{} MB", memory.used_mb()));
            }
            // The card is shared, so say plainly which part is ours — and all
            // of it. This used to add up the two budgeted caches alone, which
            // on the map page is a small fraction of what is actually held and
            // reads as the viewer being frugal while the card fills up.
            let held = app.textures_held();
            widgets::kv_row(
                ui,
                "In use, geo-ref_mapper",
                &format!("{} of textures", format_bytes(held.total() as u64)),
            );
            widgets::kv_row(ui, "  Pictures", &format_bytes(held.images as u64));
            widgets::kv_row(ui, "  Thumbnails", &format_bytes(held.thumbnails as u64));
            widgets::kv_row(ui, "  Map squares", &format_bytes(held.squares as u64));
            widgets::kv_row(ui, "  Basemap tiles", &format_bytes(held.basemap as u64));
            widgets::kv_row(ui, "  Whole overlays", &format_bytes(held.overlays as u64));
            widgets::kv_row(ui, "  Detail patch", &format_bytes(held.detail as u64));
            let (images, thumbs) = renderer.budgets();
            widgets::kv_row(ui, "Texture budget", &format!("{images} + {thumbs} MB"));
            // The budgets cover the first two rows and nothing else, so the
            // rest is worth naming rather than leaving to be worked out.
            widgets::kv_row(
                ui,
                "Outside the budget",
                &format_bytes(held.unbudgeted() as u64),
            );

            // Only ever non-zero on a card that has actually run out. Kept
            // visible because the recovery is quiet by design: textures come
            // back, budgets do not, and this is what says why the viewer is
            // suddenly holding less than it was told it could.
            let refusals = crate::oom::refusals();
            if refusals > 0 {
                widgets::status_line(
                    ui,
                    &format!(
                        "{refusals} texture{} refused — budgets were cut to fit",
                        if refusals == 1 { "" } else { "s" }
                    ),
                    theme::CLOSE_HOVER,
                );
            }
            let faults = crate::oom::faults();
            if faults > 0 {
                widgets::status_line(
                    ui,
                    &format!("{faults} wgpu errors — see the terminal"),
                    theme::CLOSE_HOVER,
                );
            }
            if renderer.is_fallback() {
                widgets::status_line(
                    ui,
                    "Not on the fastest adapter — see below for why.",
                    theme::CLOSE_HOVER,
                );
            }
            for (tier, reason) in &renderer.rejected {
                widgets::status_line(ui, &format!("{}: {reason}", tier.label()), theme::TEXT_DIM);
            }
        }
        None => widgets::status_line(ui, "no adapter recorded", theme::TEXT_DIM),
    });

    widgets::collapsing_section(ui, "PROCESS MEMORY", false, |ui| match app.metrics.memory {
        Some(memory) => {
            // Scale against the peak so the bar stays readable as the process
            // grows, with a 128 MB floor so a small app is not pegged.
            let ceiling = memory.peak_resident.max(128 * 1024 * 1024) as f32;
            widgets::meter(
                ui,
                "Resident (RSS)",
                &format_bytes(memory.resident),
                memory.resident as f32 / ceiling,
                theme::ACCENT,
            );
            widgets::kv_row(ui, "Peak resident", &format_bytes(memory.peak_resident));
            widgets::kv_row(ui, "Virtual", &format_bytes(memory.virtual_size));
        }
        None => {
            widgets::kv_row(ui, "Resident (RSS)", "unavailable");
            hint(ui, "Process memory is read from /proc on Linux.");
        }
    });

    widgets::collapsing_section(ui, "FRAME", false, |ui| {
        // The UI is only rebuilt when something changes, so the gap between
        // repaints says nothing about speed — the build cost does.
        widgets::meter(
            ui,
            "UI build",
            &format!(
                "{:.2} ms of {:.1}",
                app.metrics.ui_ms,
                crate::metrics::FRAME_BUDGET_MS
            ),
            app.metrics.budget_fraction(),
            theme::ACCENT,
        );
        widgets::kv_row(
            ui,
            "Worst build",
            &format!("{:.1} ms", app.metrics.worst_ui_ms),
        );
        widgets::kv_row(
            ui,
            "Repaint gap",
            &format!("{:.0} ms (idle)", app.metrics.repaint_interval_ms),
        );
        widgets::kv_row(ui, "Frames drawn", &app.metrics.frames.to_string());
        widgets::kv_row(ui, "Uptime", &format_duration(app.metrics.uptime()));
    });

    widgets::collapsing_section(ui, "DECODER", false, |ui| {
        widgets::kv_row(ui, "Worker threads", &app.loader.worker_count().to_string());
        widgets::kv_row(ui, "Jobs in flight", &app.loader.queued().to_string());
        widgets::kv_row(ui, "Textures cached", &app.textures.len().to_string());
        let budget = app.textures.budget().max(1);
        widgets::meter(
            ui,
            "Texture memory",
            &format!(
                "{} / {}",
                format_bytes(app.textures.used() as u64),
                format_bytes(budget as u64)
            ),
            app.textures.used() as f32 / budget as f32,
            theme::ACCENT,
        );
        widgets::kv_row(
            ui,
            "Cache hit / miss",
            &format!("{} / {}", app.textures.hits, app.textures.misses),
        );
        widgets::kv_row(ui, "Evictions", &app.textures.evictions.to_string());
        widgets::kv_row(
            ui,
            "GPU decoding",
            if app.config.use_gpu_decode {
                "on"
            } else {
                "off"
            },
        );
    });

    widgets::collapsing_section(ui, "THUMBNAILS", false, |ui| match &app.thumbs {
        Some(thumbs) => {
            widgets::kv_row(ui, "Worker threads", &thumbs.worker_count().to_string());
            widgets::kv_row(ui, "Loads in flight", &thumbs.queued().to_string());
            widgets::kv_row(
                ui,
                "Sweep",
                if thumbs.sweeping { "running" } else { "idle" },
            );
            widgets::kv_row(ui, "Built this session", &thumbs.generated.to_string());
            if thumbs.sweeping && app.stats.images > 0 {
                let done = app.stats.images.saturating_sub(thumbs.remaining);
                widgets::meter(
                    ui,
                    "Coverage",
                    &format!("{done} / {}", app.stats.images),
                    done as f32 / app.stats.images as f32,
                    theme::ACCENT,
                );
            } else {
                widgets::kv_row(ui, "Remaining", &thumbs.remaining.to_string());
            }
            widgets::kv_row(
                ui,
                "Textures resident",
                &app.thumb_textures.len().to_string(),
            );
            let budget = app.thumb_textures.budget().max(1);
            widgets::meter(
                ui,
                "Thumbnail VRAM",
                &format!(
                    "{} / {}",
                    format_bytes(app.thumb_textures.used() as u64),
                    format_bytes(budget as u64)
                ),
                app.thumb_textures.used() as f32 / budget as f32,
                theme::ACCENT,
            );
            widgets::kv_row(
                ui,
                "Stored size",
                &format!("{} px", crate::thumbs::THUMB_EDGE),
            );
        }
        None => widgets::kv_row(ui, "Sweep", "no library"),
    });

    widgets::collapsing_section(ui, "LIBRARY", false, |ui| {
        widgets::kv_row(ui, "Indexed images", &app.stats.images.to_string());
        widgets::kv_row(ui, "Folders", &app.stats.folders.to_string());
        widgets::kv_row(
            ui,
            "Indexed on disk",
            &format_bytes(app.stats.indexed_bytes),
        );
        widgets::kv_row(ui, "Thumbnails", &app.stats.thumbnails.to_string());
        widgets::kv_row(
            ui,
            "Thumbnail bytes",
            &format_bytes(app.stats.thumbnail_bytes),
        );
        widgets::kv_row(ui, "Database size", &format_bytes(app.stats.db_bytes));
        if let Some(renderer) = &app.renderer {
            widgets::kv_row(ui, "Renderer", &renderer.adapter_name);
            widgets::kv_row(ui, "Adapter type", renderer.tier.label());
            // Only worth the row when something went wrong; on a normal start
            // there is nothing to explain.
            if renderer.is_fallback() {
                widgets::status_line(
                    ui,
                    &format!(
                        "Fell back — {} was unavailable",
                        renderer
                            .rejected
                            .first()
                            .map(|(tier, _)| tier.label())
                            .unwrap_or("the preferred adapter")
                    ),
                    theme::CLOSE_HOVER,
                );
            }
        }
        widgets::kv_row(ui, "Decode threads", &app.config.decode_threads.to_string());
    });

    ui.add_space(10.0);
}

/// Per-stage breakdown for the image on the canvas.
fn loading_group(ui: &mut Ui, app: &GeoRefMapperApp, meta: Option<&ImageMeta>) {
    widgets::collapsing_section(ui, "LOADING · THIS IMAGE", true, |ui| {
        let Some(meta) = meta else {
            hint(ui, "Select an image to see its load breakdown.");
            return;
        };
        let entry = app
            .current
            .as_ref()
            .and_then(|path| app.textures.peek(path));
        let Some(entry) = entry else {
            widgets::kv_row(ui, "On disk", &format_bytes(meta.bytes));
            widgets::kv_row(ui, "State", "not decoded yet");
            return;
        };

        let timings = &entry.timings;
        let total = timings.total_ms().max(0.001);

        // Every stage the pixels pass through, in the order they happen. Each
        // bar is that stage's share, so the expensive one is obvious at a glance.
        let stages: [(&str, f32); 5] = [
            ("Read from disk", timings.read_ms),
            ("Decode", timings.decode_ms),
            ("Downscale", timings.resize_ms),
            ("To texture format", timings.convert_ms),
            ("Upload to GPU", timings.upload_ms),
        ];
        for (index, (label, milliseconds)) in stages.iter().enumerate() {
            if *milliseconds <= 0.0 {
                continue; // A stage that did not run for this image.
            }
            widgets::meter(
                ui,
                label,
                &format!(
                    "{milliseconds:.1} ms · {:.0}%",
                    milliseconds / total * 100.0
                ),
                milliseconds / total,
                theme::ACCENT.gamma_multiply(1.0 - index as f32 * 0.13),
            );
        }
        widgets::kv_row(ui, "Total", &format!("{:.1} ms", timings.total_ms()));

        widgets::kv_row(ui, "On disk", &format_bytes(timings.file_bytes));
        widgets::kv_row(ui, "Decoded", &format_bytes(timings.decoded_bytes as u64));
        widgets::kv_row(
            ui,
            "Texture",
            &format!("{} × {}", entry.size[0], entry.size[1]),
        );
        if let Some([width, height]) = timings.downscaled_from {
            widgets::kv_row(ui, "Downscaled from", &format!("{width} × {height}"));
        }
        if let Some(threads) = timings.downscale_threads {
            widgets::kv_row(ui, "Downscale threads", &threads.to_string());
        }
        if let Some((level, levels, tiles)) = timings.pyramid {
            widgets::kv_row(ui, "Pyramid level", &format!("{} of {levels}", level + 1));
            widgets::kv_row(ui, "Tiles decoded", &tiles.to_string());
        }
        if let Some(patch) = &app.detail_patch {
            let region = patch.request.region;
            widgets::kv_row(
                ui,
                "Sharpened from",
                &format!("level {}", patch.request.level + 1),
            );
            widgets::kv_row(
                ui,
                "Sharpened region",
                &format!("{} × {}", region.width, region.height),
            );
            widgets::kv_row(ui, "Sharpen time", &format!("{:.1} ms", patch.decode_ms));
        }
        widgets::kv_row(
            ui,
            "Decode threads",
            &match timings.parallel_strips {
                Some(threads) => format!("{threads} (restart markers)"),
                None => "1".to_owned(),
            },
        );
        widgets::kv_row(ui, "Decoded by", &format!("worker #{}", timings.worker));
        // Meaningless when the file was never read whole, as on the tiled path.
        if timings.file_bytes > 0 {
            let ratio = timings.decoded_bytes as f64 / timings.file_bytes as f64;
            widgets::kv_row(ui, "Expansion", &format!("{ratio:.1}× on disk"));
        }
        let throughput = timings.decoded_bytes as f64 / 1e6 / (total as f64 / 1000.0);
        widgets::kv_row(ui, "Throughput", &format!("{throughput:.0} MB/s"));
    });
}

fn format_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn stats_tab(ui: &mut Ui, meta: &ImageMeta, colour_profile: Option<&str>) {
    let pixels = (meta.width as u64 * meta.height as u64).max(1);

    widgets::section_label(ui, "DIMENSIONS");
    widgets::kv_row(
        ui,
        "Resolution",
        &format!("{} × {}", meta.width, meta.height),
    );
    widgets::kv_row(
        ui,
        "Megapixels",
        &format!("{:.1} MP", pixels as f64 / 1_000_000.0),
    );
    widgets::kv_row(ui, "Aspect ratio", &aspect_ratio(meta.width, meta.height));
    widgets::kv_row(
        ui,
        "Orientation",
        if meta.width >= meta.height {
            "Landscape"
        } else {
            "Portrait"
        },
    );

    widgets::section_label(ui, "FORMAT");
    widgets::kv_row(ui, "Type", &meta.format);
    widgets::kv_row(ui, "File size", &format_bytes(meta.bytes));
    widgets::kv_row(
        ui,
        "Bits / pixel",
        &format!("{:.2}", (meta.bytes * 8) as f64 / pixels as f64),
    );

    // Real structure, read from the file's own markers.
    if let Some(info) = jpeg_info(ui, meta) {
        widgets::section_label(ui, "JPEG STRUCTURE");
        widgets::kv_row(
            ui,
            "Mode",
            if info.progressive {
                "Progressive"
            } else {
                "Baseline"
            },
        );
        widgets::kv_row(ui, "Precision", &format!("{} bit", info.precision));
        widgets::kv_row(ui, "Components", &info.components.to_string());
        if let Some(subsampling) = info.subsampling {
            widgets::kv_row(ui, "Subsampling", subsampling);
        }
        match info.restart_interval {
            Some(interval) if interval > 0 => {
                widgets::kv_row(ui, "Restart interval", &format!("{interval} MCU"));
                widgets::kv_row(ui, "Splittable scan", "yes");
            }
            _ => {
                widgets::kv_row(ui, "Restart interval", "none");
                widgets::kv_row(ui, "Splittable scan", "no");
            }
        }
        // Be explicit that the capability is not currently used.
        widgets::kv_row(
            ui,
            "Decoded using",
            if info.is_parallel_decodable() {
                "multiple threads"
            } else {
                "1 thread"
            },
        );
        ui.add_space(4.0);
        hint(
            ui,
            if info.is_parallel_decodable() {
                "Restart markers let the scan be split across threads."
            } else {
                "Without restart markers a JPEG scan cannot be split across threads."
            },
        );
    } else if let Some(profile) = colour_profile {
        // The real profile, read out of the file. This section used to invent
        // one — picking from a list of plausible names — which was written
        // before anything could actually read a profile, and stayed long after
        // it could.
        widgets::section_label(ui, "COLOUR");
        widgets::kv_block(ui, "Profile", profile);
    }
}

/// Reads JPEG markers once per file and remembers the answer, so the panel does
/// not re-read the header every frame.
fn jpeg_info(ui: &Ui, meta: &ImageMeta) -> Option<crate::jpeg::JpegInfo> {
    let path = meta.path.as_ref()?;
    if !meta.format.eq_ignore_ascii_case("jpeg") {
        return None;
    }
    let id = ui.id().with(("jpeg_info", path));
    if let Some(cached) = ui
        .ctx()
        .data_mut(|data| data.get_temp::<Option<crate::jpeg::JpegInfo>>(id))
    {
        return cached;
    }
    let info = crate::jpeg::probe(std::path::Path::new(path));
    ui.ctx().data_mut(|data| data.insert_temp(id, info.clone()));
    info
}

/// How much is around the thing you are looking at. Spelled out rather than
/// labelled "here" and "recursive", which needed a moment's thought every time.
fn folder_counts_rows(ui: &mut Ui, here: usize, below: usize) {
    widgets::section_label(ui, "FOLDER");
    widgets::kv_row(ui, "In this folder", &format!("{here} images"));
    widgets::kv_row(ui, "With subfolders", &format!("{below} images"));
}

fn folder_summary(ui: &mut Ui, direct: usize, recursive: usize, subfolders: usize) {
    folder_counts_rows(ui, direct, recursive);
    widgets::kv_row(ui, "Subfolders", &subfolders.to_string());

    ui.add_space(10.0);
    hint(ui, "Select an image to see EXIF, debug and stats.");
}

fn empty_note(ui: &mut Ui, text: &str) {
    let rect = ui.available_rect_before_wrap();
    widgets::placeholder_note(ui, rect, text);
}

/// A dim explanatory line, wrapped to the panel.
///
/// It used to be painted as a single unbroken run at a fixed 32 pt height, so
/// anything longer than the panel is wide ran out past the window edge.
fn hint(ui: &mut Ui, text: &str) {
    let width = ui.available_width();
    let galley = ui.ctx().fonts_mut(|fonts| {
        fonts.layout(
            text.to_owned(),
            theme::small_caps_font(),
            theme::TEXT_DIM,
            width,
        )
    });
    let (rect, _) = ui.allocate_exact_size(vec2(width, galley.size().y + 14.0), Sense::hover());
    ui.painter()
        .galley(pos2(rect.left(), rect.top() + 7.0), galley, theme::TEXT_DIM);
}

fn aspect_ratio(width: u32, height: u32) -> String {
    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let divisor = gcd(width, height).max(1);
    let (w, h) = (width / divisor, height / divisor);
    if w <= 40 && h <= 40 {
        format!("{w}:{h}")
    } else {
        format!("{:.2}:1", width as f32 / height as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sentence_already_shown_is_not_repeated_lower_down() {
        // What a converted map looks like: the description sits in the plain
        // TIFF tag other programs read *and* in the XMP packet, and the two
        // arrive with different line breaks.
        let already = vec![squashed("Map shows contours, roads and jetties")];
        let xmp: crate::meta::Pairs = [
            ("Title", "West Coast of the Malay Peninsula"),
            ("Description", "Map shows contours,\n  roads and jetties"),
            ("Date", "1871"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();

        let kept = not_already_shown(&xmp, &already);
        let labels: Vec<&str> = kept.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, vec!["Title", "Date"]);
    }

    #[test]
    fn two_rows_that_merely_look_alike_both_survive() {
        let already = vec![squashed("1871")];
        let xmp: crate::meta::Pairs = vec![("Date".to_owned(), "1871-1880".to_owned())];
        assert_eq!(not_already_shown(&xmp, &already).len(), 1);
    }

    // ------------------------------------------------------- the IMAGES list

    fn sheet(year: Option<i32>, name: &str) -> (Option<i32>, std::path::PathBuf) {
        (year, std::path::PathBuf::from(format!("/maps/{name}.tif")))
    }

    #[test]
    fn the_images_list_runs_oldest_first() {
        let in_view = vec![
            sheet(Some(1936), "c"),
            sheet(Some(1825), "a"),
            sheet(Some(1890), "b"),
        ];
        let years: Vec<Option<i32>> = maps_for_list(&in_view, (1500, 2030), true)
            .into_iter()
            .map(|(year, _)| year)
            .collect();
        assert_eq!(years, vec![Some(1825), Some(1890), Some(1936)]);
    }

    #[test]
    fn only_the_years_the_timeline_is_showing() {
        let in_view = vec![
            sheet(Some(1825), "a"),
            sheet(Some(1890), "b"),
            sheet(Some(1936), "c"),
        ];
        let shown: Vec<Option<i32>> = maps_for_list(&in_view, (1880, 1900), true)
            .into_iter()
            .map(|(year, _)| year)
            .collect();
        assert_eq!(shown, vec![Some(1890)], "the strip is the control here");

        // With the strip hidden the user has said nothing about years, so
        // cutting the list to a stretch they cannot see would be inventing a
        // filter they never asked for.
        assert_eq!(maps_for_list(&in_view, (1880, 1900), false).len(), 3);
    }

    #[test]
    fn an_undated_map_goes_last_rather_than_to_the_top() {
        // Reading a missing year as zero would file every undated sheet above
        // the oldest real one, which is where the eye least expects them.
        let in_view = vec![sheet(None, "undated"), sheet(Some(1825), "dated")];
        let order: Vec<Option<i32>> = maps_for_list(&in_view, (1500, 2030), true)
            .into_iter()
            .map(|(year, _)| year)
            .collect();
        assert_eq!(order, vec![Some(1825), None]);

        // And it survives the year filter: a map with no year is not outside
        // any stretch of years, so the strip cannot hide it.
        assert_eq!(maps_for_list(&in_view, (1900, 1910), true).len(), 1);
    }

    #[test]
    fn the_same_year_twice_still_has_one_settled_order() {
        // Two sheets of one survey. Sorted by name after the year so the list
        // does not reshuffle itself between frames.
        let in_view = vec![sheet(Some(1840), "second"), sheet(Some(1840), "first")];
        let names: Vec<String> = maps_for_list(&in_view, (1500, 2030), true)
            .into_iter()
            .map(|(_, path)| path.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["first", "second"]);
    }

    // ---------------------------------------------------------- the tab strip

    #[test]
    fn only_debug_and_the_two_map_tabs_can_be_taken_away() {
        let all = info_tabs(true, true);
        assert_eq!(all, INFO_TABS.to_vec());

        assert_eq!(
            info_tabs(false, true),
            vec!["METADATA", "MAP", "IMAGES", "STATS"]
        );
        assert_eq!(info_tabs(true, false), vec!["METADATA", "STATS", "DEBUG"]);
        // The plain image viewer: two tabs, and neither of them about maps.
        assert_eq!(info_tabs(false, false), vec!["METADATA", "STATS"]);
    }

    #[test]
    fn the_shown_tabs_keep_the_order_they_are_declared_in() {
        // The panel finds the selected tab by name in this list, so a filter
        // that reordered it would light one tab and draw another.
        for show_debug in [true, false] {
            for map_support in [true, false] {
                let shown = info_tabs(show_debug, map_support);
                let places: Vec<usize> = shown
                    .iter()
                    .map(|tab| INFO_TABS.iter().position(|other| other == tab).unwrap())
                    .collect();
                assert!(places.windows(2).all(|pair| pair[0] < pair[1]), "{shown:?}");
            }
        }
    }
}
