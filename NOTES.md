# geo-ref_mapper — working notes

Most recent session first.

---

# 3 September 2026 — opening on a picture

Still **0.6.0**; 535 tests passing, 38 ignored. Every change here is about the
same launch: `geo-ref_mapper photo.jpg`, which is what a double-click in the file
manager runs. Nothing about the panelled app moves, and nothing on disk changes.

---

## 1. The keys were dead until you clicked the picture

Escape, the arrows, everything. Not a binding problem — the window never had the
keyboard at all, so no key ever reached egui. Two things earn a window that
treatment on most desktops: focus-stealing prevention on anything a file manager
starts, and a window that moves and resizes itself on its first frames, which
`apply_launch_window` does.

`claim_keyboard` asks for it: `ViewportCommand::Focus` every frame until
`viewport().focused` comes back true, with `request_repaint_after` so there *is*
a next frame to notice on — an idle picture asks for none. It gives up after two
seconds. A window still unfocused by then was left in the background on purpose,
and a program that goes on asking is one fighting its user for the foreground.

This turned out to be a symptom rather than the disease; see 6.

---

## 2. Stepping stays in the folder it opened in

`mosaic_recursive` is on by default, and `viewer_images` reads it by rooting the
rail on the folder *above* the selection — so the arrows ran out of the picture's
folder into its neighbours. Correct in the panelled app, where the tree shows
where the stepping has got to. Wrong in a window that is nothing but a picture,
where it looks like the arrows wandering off.

It is ignored while `launched_on_picture` is set, and honoured again the moment
Enter opens the panelled app. The rail draws from the same function, so the two
cannot disagree about what comes next.

---

## 3. The arrows do nothing until the folder lands

A file opened by name does not select itself until its folder has been indexed,
and until then the tree still holds *last session's* selection. The arrows walked
that folder — a different set of pictures entirely — and the scan then snapped
the selection back and undid it.

The arrow dispatch returns while `pending_selection` is outstanding. Off a USB
disk that is a real pause rather than a frame, but it is a pause instead of the
wrong pictures.

---

## 4. Both mouse buttons hang off one question

`Viewport::pannable`: is any of the picture off the edge of the canvas? Asked of
the pixels, not the mode — wheeling backwards lands in `Free` with the whole
picture on screen just as Fit does, and the two must behave alike.

* **Zoomed in.** Left-drag pans. Right-click goes back to Fit, which is where the
  wheel that got you there would otherwise have to be undone by a trip to the
  toolbar.
* **Whole picture on screen.** There is nothing to pan, so left-drag sends
  `StartDrag` and moves the *window* — the point being that this window has no
  title bar on screen to take hold of. Right-click keeps its file menu.

Suppressed full screen, where there is no window to move. The trade is that the
canvas menu is not reachable by right-click while zoomed in; it is still on the
filmstrip tile, the tree row and the mosaic tile.

---

## 5. The window is the size of the picture

`apply_launch_window` used to jump to 90 % of the monitor on the first frame that
knew how big the monitor was. It now sizes to the picture instead. Presentation
mode gives the canvas the whole window, so that is one screen pixel per image
pixel with nothing around it.

**From the header, not the decode.** The first cut waited for the decoded
texture, which is the obvious place to learn how big a picture is and the wrong
one: the window sat at its default size for the whole of a slow open and then
jumped, which reads as the picture being slow to arrive even when it is not.
`scan::displayed_size` answers the same question from the header in about a
millisecond, so the window is the right size before the pixels exist and the
decode finishes into a window that already fits. It applies orientation, without
which a phone photograph taken upright would open in a landscape window and be
letterboxed inside it.

Five things it has to get right:

* **Which picture.** Held by name in `launch_picture`, not read from `current` —
  which is still last session's selection until the scan lands, and which
  `sync_selection` does not update until *after* this runs in the frame.
* **Points, not pixels.** Divided by `pixels_per_point`, or a 1200-pixel picture
  on a 150 % display would ask for a window half again too big.
* **One squeeze, not two.** `launch_geometry` takes both axes down by the same
  factor. Clamping them separately fits the window to the screen and leaves the
  picture letterboxed inside it — a portrait scan would get a window 90 % of the
  screen wide and two thirds empty.
* **A header that cannot be read.** Falls back to the decoded texture, for a file
  we could not measure but could open anyway.
* **Giving up.** Ten seconds, then the old 90 % rule. A file nothing here decodes
  must not leave the window at its default size for ever.

The zoom is pinned to 1:1 when the window really did get the picture's size.
Identical to Fit in the exact case; it matters below the 560×360 window minimum,
where Fit would blow a 200-pixel thumbnail up to fill the window.

---

## 6. The busy cursor that outlived the program

The pointer kept the launch spinner after the app had exited. It had been
spinning the whole time — the app's own cursor covers it while the pointer is
over the window, so it only shows once there is no window left.

Our entry says `StartupNotify=true`, so Cinnamon puts an id in
`DESKTOP_STARTUP_ID` and waits to be told the launch is over. Nothing was telling
it. winit can — its X11 backend builds exactly that message — but only for an app
that hands it a token at window creation, and eframe has no way to pass one
through. So the sequence ran to muffin's timeout every time.

`startup.rs` sends it by hand: `remove: ID=…` broadcast to the root window in
20-byte client messages, the first tagged `_NET_STARTUP_INFO_BEGIN` and the rest
`_NET_STARTUP_INFO`, from an unmapped 1×1 window the spec wants for the sender.
`x11rb` was already in the tree under winit, so it is a direct dependency that
costs no compilation.

Two details worth keeping:

* **The token is taken out of the environment in `main`, before any thread
  exists.** Removing a variable is only sound while the program is
  single-threaded, and the app runs `xdg-open` and `gio` as children — one of
  them inheriting the id would claim our startup sequence as its own.
* **The id is always quoted.** The first attempt quoted only values containing a
  space, and it did not work. Nemo builds its id out of the `Exec` line:

      nemo-235936-K11-"/home/morgan/.local/bin/geo-ref_mapper"-24_TIME22723154

  Quotes, no spaces. The specification recognises `\\` and `\"` *inside* quotes
  alone, so the bare escapes that came out were read literally, the id did not
  match, and the sequence stayed open. Wrapping unconditionally is also exactly
  what wrote the id in the first place — GDK's `append_quoted` — so what goes
  back is the shape that came in.

The entry also gained `StartupWMClass=geo-ref_mapper`, which is the other way a
sequence ends: the window manager matching the new window to it. egui-winit
applies `app_id` on Wayland only, so on X11 the class falls back to `argv[0]`.

Two things made this findable. A launch from the file manager has no console, but
**stderr from one lands in `~/.xsession-errors`** — which is where the id read
and the removal sent are now both reported. And `startup_traffic`, an ignored
test, watches the root window and prints every startup message that goes by:

```text
GEO_REF_MAPPER_WATCH_SECONDS=120 cargo test --release startup_traffic -- --ignored --nocapture
```

It is what produced the id above, in the form the launcher actually writes it,
and it is the tool to reach for the next time a launch misbehaves.

Honest about what was proved: the spinner is gone, and the id Nemo sends matches
what is now sent back. The quoting and `StartupWMClass` landed together and the
watcher never caught our own `remove:` alongside a launch, so which of the two
did it is not established.

---

# 9 August 2026 — one program, two shapes

Still **0.6.0**; 524 tests passing, 36 ignored. Nothing here changes what a
converted map contains, so the `tiffwrite::software()` stamp is untouched and
maps written today are indistinguishable from those written on the 4th.

**A build now says when it was made.** `build.rs` stamps the Unix second into
the binary and `version.rs` turns it into a date. No `rerun-if-changed` is
printed, deliberately: with none, Cargo re-runs the script whenever anything in
the package changes, which is exactly the rule wanted — the stamp moves when the
binary is really rebuilt and stays put when Cargo has nothing to do. Naming a
file would freeze it; naming a nonexistent path would rebuild every time.
Verified both ways (a no-op build takes 0.12 s and keeps its stamp).

The date arithmetic is Hinnant's `civil_from_days`, transcribed rather than
invented, tested on the 2000-versus-2100 century rules and by walking all 366
days of 2024. No new dependency.

---

## 1. Image map support is one switch

Config → **IMAGE MAPS**. Off, everything about maps goes together: the Map and
Map Images pages, the INFO panel's MAP and IMAGES tabs, *Add to map images* in
both right-click menus, the MAP TILES settings, the `G` and `L` keys, the green
georeferenced rows in the tree, and the background sweep that opens every TIFF
looking for a position. On by default, so an existing library is unchanged.

Three things make it a switch rather than a list of places to remember:

* **`enter_page` is the single gate.** Every way onto a map page goes through
  it — the pill, Space, placing, a pin in the panel. `PAGE_MAP` and friends keep
  their numbers and the pill translates between the segment clicked and the page
  meant; dropping two entries from the array would have renumbered Formats,
  Export and Config, every one of which is compared against `app.page` somewhere.
* **`info_tab` now numbers `INFO_TABS`, not the tabs on screen.** MAP and IMAGES
  come out of the *middle*, unlike DEBUG, so a saved position would have meant a
  different tab from one run to the next. The panel looks its tab up by name.
* **Turning it off stops work already in flight.** `map_support_changed` *stops*
  the conversion rather than killing it — it finishes the map in hand and leaves
  the rest queued — cancels placing and drops the overlay. Nothing on disk is
  touched; the queue, the anchors and every converted map are there if it goes
  back on.

---

## 2. Interface scale, 80 % to 140 %

Config → APPEARANCE. It is egui's **zoom factor**, which is the whole interface
and not the glyphs alone. It has to be: this window paints its own chrome to
fixed point metrics — a 28 pt tab strip, a 40 pt title bar, a 32 pt pill — and
text scaled on its own would be spilling out of all three well before the top of
the range. The picture is unaffected; the canvas keeps its physical pixels and
there are simply fewer points across it.

The ceiling started at 2.5 and came down to **1.4** the same afternoon, once it
was run on the machine rather than reasoned about — see §7. This scale
multiplies whatever the desktop already applies, and a 4K screen at 200 % is 2×
before the program says a word.

* **Applied on release, not during the drag.** The slider is *inside* the thing
  being scaled: rescaling mid-drag lengthens the track and slides the handle out
  from under the pointer, so the reading chases a value that is moving because it
  is being read. The slider writes `pending_ui_scale`; the setting is taken when
  the **pointer comes up** — not when the slider reports a change, because a
  release off the track, off the window, or on a frame where Config is not drawn
  would otherwise be missed entirely.
* **The slider and Ctrl+plus take turns.** egui's own zoom shortcuts were
  already live. Pushing the config every frame would undo them, so a zoom factor
  that moved on its own is adopted as the setting and saved. `resolve_ui_scale`
  is the whole of that decision, split out and tested.
* **A bug this would have introduced.** egui reports the window rect with the
  zoom already divided out, but `main` restores it through winit, which knows
  only the display's factor. Saved as it arrived, a window left at 200 % would
  reopen at half size — and half that again next time. `geometry_to_save`
  multiplies it back.

---

## 3. Installing into `~/.local`, no root

Config → **INSTALL**, above the default-viewer group, because the two are read
together and confused: this one is "the menu knows geo-ref_mapper exists", that one is
"double-clicking a JPEG opens it". Neither needs the other.

| what | where |
|---|---|
| the binary | `~/.local/bin/geo-ref_mapper` |
| the mark, five sizes | `~/.local/share/icons/hicolor/<n>x<n>/apps/geo-ref_mapper.png` |
| the launcher entry | `~/.local/share/applications/geo-ref_mapper.desktop`, now with `Icon=` |

Icons are **drawn** at each size rather than downscaled from one bitmap — the
frame outline is a single pixel at 48 px and a downscale loses it. `app_icon`
gained `rgba(size)` for that.

Four sharp edges, each of which cost something:

* **Copying onto itself.** Reinstalling while running the installed copy would
  have had `fs::copy` truncate the running program to nothing. Both paths are
  canonicalised — `~/.local/bin` is very often a symlink — and the copy skipped.
* **`ETXTBSY`.** Writing over a running executable fails on Linux. Staged beside
  the target and renamed on top, the same trick `convert` uses for maps.
* **The two buttons share the desktop entry.** *Restore previous handlers* used
  to delete it, which would have un-listed a deliberate install. With an install
  present it rewrites the entry and drops only the `mimeapps.list` claims.
* **Uninstall order is the counter-intuitive one.** Binary and icons first,
  launcher entry last — because of the line above, calling `desktop::remove`
  while still installed would preserve the entry and leave the menu pointing at
  nothing.

**A startup dialog** offers to install, or to update when the running build is
newer. The comparison is the installed file's **mtime** against this build's
stamp: no subprocess, nothing to hang on, and sound because an install cannot
have copied a binary built after itself. The reverse — an installed copy newer
than the running one — says nothing, because that is an ordinary old build out of
a stale `target/`.

*Not now* records **which build** was declined, not a permanent flag. The next
build asks again. A permanent flag would mean one dismissal during a morning's
work hid every update afterwards.

`~/.local/bin` not being on `PATH` is **reported, not fixed**: the entry carries
an absolute path and works regardless, and editing `.profile` is touching
something the user owns.

---

## 4. Opening a file from outside

Three separate faults, one of them the cause of another.

* **The arrows were dead.** `focus` starts as `Focus::Tree` and `enter_page`
  aims it at the rail on the way into the Viewer — but a file opened from the
  file browser never goes through it, since the app is *built* already on the
  Viewer. So the arrows walked tree rows, and with the folder still shut the
  selected picture was not among the visible ones, so `step_tree_row` fell to its
  `None` branch and jumped to the top of the tree. Fixed where the opened file
  lands. **This may also be the "alt-tab then arrows" item below** — same
  mechanism, no click having set the focus.
* **A deliberately opened picture arrives fitted.** `open_path` already reset the
  view, but at the moment the open was *asked for* — the picture does not arrive
  until a scan and a decode later, and `carry` brings it in keeping whatever mode
  is current by then. The reset now also happens where the picture lands.
* **`bronte.enc` opened a different picture entirely.** The scan indexes by
  extension, `find_by_path` then failed, and the whole block was skipped —
  leaving the selection wherever the scan had left it. A *different* picture came
  up as though it were the one asked for.

That last one is now two things. It says so when it cannot open a file; and on a
**deliberate** open — file browser, command line, drop, dialog — a name the scan
would skip gets a second chance on its bytes. `bronte.enc` is a plain baseline
JPEG (`FF D8 FF E0`) with a misleading extension, and refusing it over four
letters after the dot is being wrong on purpose about a file the user named.

The gate is deliberately **"a probe came back with a size"**, not "we recognise
the header". `describe` names anything it knows, AVIF included, which this build
cannot decode. Without the probe gate, taking `.avif` out of `IMAGE_EXTENSIONS`
would have achieved nothing — every AVIF opened by hand would be let straight
back in. Only on a deliberate open, never during a sweep: opening 8,000 files to
read their headers would be absurd, and a name is a fine guess at scale.

---

## 5. What the decoders actually do — 975 conformance files

`loader::decode_survey`, an ignored harness that walks a folder and reports what
decoded, what refused and what **panicked**, each decode wrapped in
`catch_unwind` so a suite of deliberately-broken files cannot stop at the first.

```text
GEO_REF_MAPPER_SURVEY_DIR=…/testdata cargo test --release decode_survey -- --ignored --nocapture
```

| format | ok | refused | note |
|---|---:|---:|---|
| PNG | 260 | 14 | PngSuite: **all 162 valid decoded, all 14 corrupt refused** |
| JPEG | 99 | 0 | |
| JPEG XL | 6 | 0 | |
| WebP | 5 | 0 | |
| QOI | 8 | 0 | |
| SVG | 1 | 0 | |
| OpenEXR | 74 | 23 | deep data 12, channel subsampling 6, no non-deep RGB 5 |
| HEIC | 44 | 19 | the familiar libheif refusal rate |
| **AVIF** | **0** | **172** | every one |

**Zero panics across all 969 files**, including the suites' explicit
`heap_buffer_overflow`, `memory_allocation_error`, `null_deref` and `.fuzz`
cases.

### AVIF was claimed in two places it could not honour

`formats.rs` was honest — `Status::NeedsFeature`, "decoding needs dav1d". But
`db::IMAGE_EXTENSIONS` held `avif`, so the sweep filled the tree and the Mosaic
with pictures that could not be shown; and `desktop::ASSOCIATIONS` held
`image/avif`, so *Set geo-ref_mapper as the default* **took AVIF off whatever could
actually open it**. Both removed. The limits blurb was wrong in the other
direction too — it said HEIC and JPEG XL were "recognised but not decoded", and
the survey decoded 44 and 6 of them.

`describe` still *identifies* AVIF, which is useful and is not a promise.
`avif_is_not_let_in_through_the_back_door` checks both halves — the extension
list and the sniffing path — against the real 172 files, so neither can quietly
reopen it.

Camera RAW was not downloaded: `.dng` and friends are in no list, and
`formats.rs` says `Planned`. Consistent already. One consequence though — a DNG
is TIFF-based, so an **explicitly opened** one now gets past `describe` and, if
the probe reads its size, is indexed and then hits the honest decode error.

---

## 6. Smaller things

* **Config group titles** are 14 pt and lighter (`TEXT_MUTED`, `TEXT` on hover)
  where they were 11 pt dim. `collapsing_header` took a `Heading` weight rather
  than changing the shared style: `collapsing_card` is Config-only and
  `collapsing_section` is the eleven panel groups, so the split falls exactly on
  that boundary. `Section`'s numbers are spelled out as the literals it always
  had — a derived chevron centre would have nudged all eleven by 0.2 pt.
* **The page pill was measuring nothing.** It reserved a hard-coded 300 pt for
  the identity cluster and centred itself, so it could overlap either side. It
  was changed to measure the cluster's real right edge and take twice the
  *smaller* gap — which was still wrong, and §7 has the rest of the story.
* **A flaky test was a real bug.** `geowrite`'s two "turned" tests both called
  `a_map("turned")`. The scratch directory is per-*process*, so both wrote
  `<tmp>/geo-ref_mapper-geowrite-<pid>/turned.tif` and the harness ran them
  concurrently — one read the map while the other was still converting it. Failed
  about one run in three, passed every time alone. The constraint is now in
  `a_map`'s doc comment.

---

## 7. Afterwards — what running it found

Everything above was written before the program was actually looked at. Then it
was launched and screenshotted at 200 %, and four things fell out at once. Two
of them were introduced by §2 that same afternoon.

**A GTK3/4 port was weighed and turned down.** It is feasible, and it is a
rewrite of about 40 % of the code — `app`, `pages`, `panels`, `widgets`,
`chrome`, `pill`, `icons`, `theme`, `viewport`, `crossfade`, `timeline`,
`detail` are all immediate-mode painting and would go. The decoders, `db`,
`scan`, `thumbs`, `cog`, `heif`, `exif`, `geo*`, `convert` and `install` would
survive. The cost is not the line count but what lives in those lines: the
crossfade that never blinks through black, zoom carrying as a *mode*,
`anchored_offset`, the VRAM budgets and OOM recovery — every one of them tuned
by hand and documented here. And the rendering strategy is wgpu top to bottom,
where GTK4 renders through GSK; the interop is real work on the exact axis this
program already needs a three-tier fallback for.

The deciding argument was that the bugs below are not egui's. They are all the
same fault: **this chrome hand-computes layout with fixed point metrics instead
of using layout containers.** A different toolkit is not required to fix that.
A copy of the tree was taken first regardless —
`RustroverProjects/imgViewer-before-gtk.tgz`, verified by diff.

### The rail hung over the INFO panel

`if x > strip.right() { break; }` stops at the first tile whose **left** edge is
past the end — so the one before it is drawn in full and hangs up to 88 pt over,
across a panel it knows nothing about. Nothing clipped it.

Always wrong, invisible at a comfortable width where the overhang landed in the
gap. At 200 % the canvas was barely three tiles across and the last one
straddled the divider.

Clipped to the strip rather than stopped a tile early: a rail that scrolls
*should* cut its end tiles off — that is what says there are more — and stopping
short would leave up to 88 pt of empty strip, which reads as the rail failing to
fill its width. `Ui::interact` takes its hit rectangle as `clip_rect ∩ rect`, so
the same line also stops the hidden half of a tile answering to a pointer that
is really over the INFO panel.

### The pill vanished, and the reason was not the one assumed

Twice-the-smaller-gap (§6) was still wrong, and reasoning about it produced the
wrong answer twice. Instrumenting it took a minute and settled it:

```text
bar=[0..768] centre=384 left_edge=347 right_edge=513 gap=25 available=51 floor=80
```

Two surprises. **The title bar is 768 points, not 1536** — the display is
already 2× native, so an interface scale of 2.0 is 4× device pixels. And there
were **166 pt free** between the clusters, of which centring could claim 51,
because the free span is not centred on the bar. It fell under its own floor and
disappeared, taking every way between pages with it.

The pill now takes the whole span and slides off centre only when it must —
symmetry is worth having and worth losing in that order. Labels are elided to
their segments (they used to overrun their neighbours at any squeezed width),
and a label with no room left shows its first letter rather than a bare `…`,
which names nothing. The floor scales with the number of segments.

**The lesson is the method, not the fix**: three wrong answers from reading the
code, one right answer from printing six numbers.

### The version came off the title bar

Config → INSTALL carries it in full, beside the installed copy's stamp — the
only place the two are read together. On the bar it was a string nobody consults
competing for the one row the pill also needs. A debug build still says `debug`
in red, because that is not a version but a warning: 18× slower, and mistaking
one for a release reads as the viewer being broken.

### The scale ceiling came down to 1.4

It multiplies whatever the desktop already applies. On a 4K screen at 200 %,
1.4 is 2.8× device pixels and a 3072 px title bar is down to 550 points. Past
that the picture has nowhere left to go. Saved values above the ceiling clamp on
the way in, so lowering it strands nobody.

### `D` stopped working, and that one was mine

`tile_debug` serves **two** places: the Viewer's image-tile overlay
(`imagetiles::draw` and `tiling_hud`) and the Map page's overlay debug. §1
gated `D`, `G` and `L` together as "the map keys" — grouping three letters by
where they were *assumed* to apply rather than by what each one reads. With map
support off, the Viewer lost its tiling overlay, which is exactly where it earns
its keep: a gigapixel scan loading the wrong level.

`D` is unconditional again. `G` and `L` stay gated, because a graticule is drawn
on the map page alone and locking anchors is meaningless without them.

`grep tile_debug` would have shown two call sites in the Viewer, in five
seconds. Searching for the *feature* found the key handler; nobody checked what
the flag was read by.

### The `.enc` fix was right — the screenshot was early

Reported here as half-working on the strength of one screenshot: the file was in
the tree, and the Viewer was showing a different picture. Wrong. The selection
lands when the **scan completes**, and the shot was taken while it was still
running.

Three printed lines settled it in one run — `index_by_content` upserting the row
as a JPEG 640 wide, `pending_selection` set, and `reload_library` consuming it
with `found=true` — with the INFO panel then reading *640 × 640, JPEG, baseline,
8-bit, 4:2:0* off a file called `.enc`.

**Twice in a day a screenshot read at the wrong moment sent the diagnosis the
wrong way**, and both times printing four or five values settled it immediately.
The pill (above) was the same shape of mistake. A screenshot says what is on
screen *now*; it does not say what the program is in the middle of.

### Closing was measured, not fixed

Reported as slow; measured at **0.12–0.35 s** across five closes, teardown
69–130 ms. Not reproduced, so nothing was changed on a guess.

What *is* unbounded is joining the worker pools: a worker is asked to stop
between tasks, and nothing can interrupt a decode already under way — a 2 GB map
takes as long as it takes. Each pool now names itself if it makes the user wait
over 200 ms (`metrics::report_slow_join`), silent otherwise. The next slow close
will say which one. The export job is the one genuinely worth waiting for; it is
writing the user's files.

---

## Open after this session

* **Space still means two things** by page — though with map support off it now
  marks from anywhere, so the collision only exists with maps on.
* `tree_row` reached **12 arguments**; `file_context_menu` is at 8 and clippy has
  started saying so. The struct that was promised last time is overdue.
* **AVIF has three ways forward** if it is ever wanted: enable `image`'s
  `avif-native` (pulls in dav1d as a system dependency), decode through the
  existing libheif path if that build has an AV1 decoder, or leave it declared.
* `tiffwrite::software()` still stamps a bare `geo-ref_mapper 0.6.0`. The build stamp
  would sharpen provenance, but it changes a string these notes treat as an
  invariant.
* **A slow close has not been reproduced.** Reported, measured at a third of a
  second, and left instrumented rather than guessed at. Watch for
  `waited … ms at exit for …` in the log; my money is on the thumbnail sweep or
  the decode pool mid-map, both of which are safe to stop waiting for.
* **The chrome still hand-computes its layout.** Every GUI bug this session was
  that same fault — fixed metrics and painter calls where a layout container
  belongs. §7 turned down a toolkit change on the grounds that it is not the
  toolkit; the corollary is that the title bar and the rail are still arithmetic,
  and the next thing added to either will meet this again.
* The interface scale is **applied on top of the desktop's own**. Nothing in the
  program says so except the Config paragraph, and the ceiling is set for a 2×
  display. On a 1× screen 1.4 is conservative.
* **Gating a feature means finding what reads it, not what it sounds like.**
  Both regressions this session — `D`, and the pill's floor — came from grouping
  things by their apparent subject rather than by their call sites. `map_support`
  now guards eight places; each was checked by grep except the one that was not.
* **`open_path` does not switch to the Viewer.** Opening a file while another
  page is up indexes and selects it without showing it. Harmless today because
  the app starts on the Viewer and the page is not persisted, but it is a
  latent surprise if either ever changes.

---

# 3–4 August 2026 — maps, metadata and placement

Version 0.5.0 → **0.6.0**; 436 tests passing.

The version matters: `tiffwrite::software()` stamps `geo-ref_mapper 0.6.0` into tag
305 of every file it writes, so that string distinguishes maps converted before
and after this work.

---

## 1. Source metadata now lives inside the converted TIFF

Source maps arrive with a `<picture>.histomap.json` sidecar beside them. The
converter writes a **new** file under a new name, so the sidecar named the old
one and every converted map came out anonymous. 7,633 of 7,699 sidecars hold
provenance recoverable no other way.

`sidecar.rs` reads them and `convert.rs` writes the contents into the file:

| where | what |
|---|---|
| XMP packet, tag 700, **at the front of the file** | `dc:title`, `dc:description`, `dc:date`, `dc:type`, `dc:publisher`, `dc:source`, `dc:identifier`, `dc:relation`, `dc:rights`, `dc:subject` |
| `ImageDescription` 270 | what the map shows — the tag exiftool and GDAL surface unasked |
| `Copyright` 33432 | the archive's access conditions |
| private 65000 | crop, 8 doubles, four corners in 0–1 |
| private 65001 | invert flag |

**Front of the file** because `meta::from_path` reads the first 4 MB and stops;
the back of a 2 GB map is nowhere near that. It also means a value at a fixed
offset can be rewritten without moving the directory. The packet is padded to
`max(4 KB, packet + 2 KB)` so later edits are same-size overwrites — the fixed
4 KB alone overflowed on the longest descriptions, which were exactly the ones
that would have needed a rewrite first.

`DocumentName` (269) is deliberately **not** written: it reads back as EXIF and
the panel showed a map's title as "Document" under a heading about software.

### Things learned from the real data

* `provider_info` is one flat `Key: value; Key: value` blob. **Splitting on
  `"; "` does not work** — descriptions contain their own semicolons and a plain
  split invented ~40 spurious keys. Anchor on known key names.
* Dates are free text: 5,499 plain years, 1,557 `DD/MM/YYYY`, 285 `c.YYYY`,
  205 spans, 64 other spellings. No fixed-width date field can hold them, so the
  written form is kept verbatim and a sortable form derived beside it.
* On all 238 real spans the year *tag* is the **end** year. The sortable date is
  taken from the date text (which gives the start) and only falls back to the tag.
* Tags are mostly noise: `nas` on 7,611 and years on most of the rest. Only ~160
  files carry anything descriptive.
* Verified over **7,895 real sidecars, 0 parse failures**
  (`GEO_REF_MAPPER_SIDECAR_DIR=~/Pictures cargo test sidecar_survey -- --ignored`).

### Bugs found in `meta.rs` on the way

* XMP values were never unescaped, so `Maps &amp; Building Plans` reached the
  panel. Ampersands are ordinary in a title and every one arrives escaped.
* `element_value` took only the first `<rdf:li>`, so a five-keyword list showed
  as one keyword with nothing saying the rest existed.
* The panel showed description/rights twice, once from the plain TIFF tag and
  once from XMP. It now drops an XMP row whose text already appeared above it.

---

## 2. The conversion pipeline

* **Output is always `.tif`.** It used to mirror the source name, so a PNG map
  became a `.png` holding TIFF bytes. Caught after 13 files.
* **An existing target is replaced**, because there is no telling a finished
  conversion from an abandoned one. It writes `.name.tif.part` beside the target
  and **renames on top at the end** — a plain overwrite would spend the whole
  conversion with the old file destroyed and the new one incomplete. The report
  says `· replaced` so a silent overwrite is never silent.
* **Failure cleanup only removes a file this conversion created.** It previously
  deleted the target unconditionally, so a refused conversion destroyed *another
  map's* finished output. That cost one real file.
* **One progress bar per map**, layers weighted by area (each is a quarter of
  the one before). Counting them equally made a seven-layer map crawl through
  the first and leap through six.
* **Output layout is a choice**: whole mirrored path, or one folder per source
  folder (`1932/…` stays `1932/…`).
* **Optional auto-start** when maps are added; picks up anything queued during a
  run when that run finishes.
* Folder right-click → *Add N images to map images*, recursive. The library
  resolves the files, matched on the separator so `MAPS` cannot swallow
  `MAPS-NEW`.

Regenerated the whole collection on 3 August: **287 maps, 7.6 GB**, all `.tif`,
all carrying an XMP packet at offset 62, 222 titles present with **zero lost**.

---

## 3. Georeferencing by hand

* `anchor::pixel_of` — the inverse of `apply`, with the row-flip and rotation
  round-tripped across five angles in tests.
* **The two halves stay in step**: pan and zoom sync, leader decided per gesture
  so they never chase each other. Scale is matched in *Mercator* metres so the
  units cancel.
* **The scan is drawn turned to north**, solved from every placed anchor via a
  centroid least-squares fit — no anchor is the pivot. Togglable, since a title
  block at 40° is hard to read.
* Shared anchors are frozen during placing; anchors made *for one picture* can
  be dragged. Per-picture anchors live in `anchors.owner_image_id` (schema 8) and
  go with the picture.
* Clicking a pin on the basemap chooses it; clicking a name in the list centres
  the map on it.
* Residuals get an `×` to drop one anchor and refit, plus *Undo the whole
  mapping*.
* Writing a position now turns the tree row green immediately — the flag lived in
  four places and the tree's own copy was never updated.

### The yard-grid experiment — built, then removed

The 1:25,000 GSGS sheets carry a 1,000-yard War Office Cassini grid. From the
sheet-corner diagram we solved the false origin against the documented Bukit
Timah origin (EPSG:24500) and reproduced all four corners to **1.3 m**, with the
Kertau→WGS84 shift (EPSG:1158) accounting for a real **192 m** offset.

**It did not work in practice — the scale was off — and the whole thing was
removed at morgan's instruction.** Placement is by distinctive keypoints. Do not
rebuild it without new evidence: `grid.rs`, `datum.rs`, the overlay, the
snap-to-crossing mode and the DMS parser are all gone.

---

## 4. Selection and navigation

* **Space** in the Viewer marks/unmarks the picture on screen. (Still puts the
  picture on the map elsewhere — a split meaning, flagged but not resolved.)
* Clicking in the tree **adds to** a set being built instead of wiping it. A set
  assembled over a hundred rows was previously gone to one mis-aimed click.
* Clicking the **rail** no longer touches the marked set at all.
* Marks survive a library rebuild, carried **by path** — by index they would have
  silently pointed at different files. A copy keeps the set; a move drops the
  files that moved and keeps the rest.
* **Click-to-focus**: the first plain click on an inactive tree only brings it
  forward. The selection dims when the tree is not active, which is the only
  sign anywhere of which panel owns the keyboard.
* Arrow keys scroll the tree to keep up (`step_tree_row` never asked for it;
  `step_image` always had).
* **Zoom carries between images** — the mode, not a scale in points. Opening a
  file deliberately still resets.
* A **shut** folder row shows nothing rather than a picture from inside it, and
  names the folder instead. That blank is the signal you have left one folder.
* Presentation mode shows `● MARKED` and the folder name as badges.
* *Unmark all* in a second row under Scan; `Anchors: All · Used here · None`.
* **BACKGROUND** section listing the thumbnail sweep (Pause/Resume) and the map
  conversion (Stop). These run concurrently, so they are listed rather than
  chosen between — the old status line hid one behind the other.
* *Open file location* no longer freezes the window: `gdbus` blocks until the
  file manager answers, and starting one takes seconds.

---

## Open as of 4 August

* **Alt-tab then arrows**: reported dead. Ruled out the typing guard and event
  consumption (`handle_input` runs before any panel draws). Remaining suspect is
  window-level keyboard focus. Diagnostic: does one click anywhere restore them?
  → **Probably answered on the 9th.** The arrows are dispatched on `focus`, and
  nothing had set it, so they were walking tree rows rather than the rail. Worth
  retesting the alt-tab case specifically.
* Space means two different things by page. → still true, see above.
* `tree_row` is at 11 arguments; a struct next time it grows. → it grew, to 12.
* Basemap POI icons: the OSM raster tiles bake them in. A styled tile source
  (CARTO Positron) is the only lever. Not pursued. → unchanged.
