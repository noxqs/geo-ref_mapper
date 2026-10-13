//! A slippy map, drawn from OpenStreetMap tiles.
//!
//! Tiles are 256×256 PNGs on a quadtree: at zoom `z` the world is `2^z` tiles
//! across, addressed by `(x, y)` from the top-left in Web Mercator. Everything
//! here follows from that one fact.
//!
//! **This is the first part of geo-ref_mapper that talks to the network on its
//! own.** Everything else either works offline or waits for a click. So it is
//! built to be a good citizen of a service that is donated, not sold:
//!
//!   * every tile is **cached on disk** and re-read from there afterwards, so
//!     panning back over ground already covered costs nothing
//!   * at most two requests are in flight at once — no bulk downloading
//!   * a real `User-Agent` identifies the application, as the tile policy
//!     requires
//!   * attribution is drawn on the map, which the licence requires

use eframe::egui::{Rect, pos2, vec2};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

/// Tiles are this many pixels square, by definition of the scheme.
pub const TILE: f32 = 256.0;

/// Beyond this the tile server has nothing more detailed to give.
pub const MAX_ZOOM: u32 = 19;
pub const MIN_ZOOM: u32 = 2;

/// How many fetches may be in flight. Deliberately small: the tile policy asks
/// for no more than two connections, and a map being panned is exactly the
/// pattern that would otherwise hammer it.
/// Threads that may talk to the tile server.
///
/// Two, and no more: the usage policy names that figure, and being refused
/// costs far more than the second or two saved. Prefetching gets its speed from
/// asking earlier and from the disk, not from opening more sockets.
const NETWORK_WORKERS: usize = 2;

/// How far up the pyramid to look for a stand-in tile. Three steps is an eighth
/// of the resolution — soft, but recognisably the right place, and past that it
/// is too blurry to be worth drawing.
const MAX_ANCESTOR_STEPS: u32 = 3;

/// Threads that only ever read the disk cache.
///
/// These are free of the policy — nothing leaves the machine — so a revisited
/// area comes back at disk speed even while a long prefetch queue is trickling
/// in behind it.
const CACHE_WORKERS: usize = 4;

/// How many requests may be outstanding before the oldest is dropped.
///
/// Generous, because the queue now holds a prefetch ring and three zoom levels
/// as well as the screen. Visible tiles live in their own queue and are never
/// crowded out by it.
const QUEUE_LIMIT: usize = 512;

/// What a tile is for, which decides what it is allowed to displace.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Want {
    /// On screen now. Served first, always.
    Visible,
    /// Somewhere the view is likely to go next.
    Prefetch,
}

/// Requests waiting to be served, kept in two tiers.
///
/// One queue would mean a prefetch of several hundred tiles sitting in front of
/// the one tile under the cursor. Splitting them is what makes it safe to be
/// greedy: the screen is always served first, and the prefetch uses whatever is
/// left over.
#[derive(Default)]
struct Queue {
    visible: Vec<TileId>,
    prefetch: Vec<TileId>,
}

impl Queue {
    fn push(&mut self, id: TileId, want: Want) {
        let queue = match want {
            Want::Visible => &mut self.visible,
            Want::Prefetch => &mut self.prefetch,
        };
        // Taken from the end, so the newest request is served first: a map
        // being panned wants what is on screen now, not what was two gestures
        // ago.
        queue.push(id);
        if queue.len() > QUEUE_LIMIT {
            queue.remove(0);
        }
    }

    fn take(&mut self) -> Option<TileId> {
        self.visible.pop().or_else(|| self.prefetch.pop())
    }

    /// The newest request that is already on disk, for a cache worker. Reading
    /// it costs no network, so it can jump the whole prefetch queue.
    fn take_cached(&mut self) -> Option<TileId> {
        let dir = cache_dir()?;
        for queue in [&mut self.visible, &mut self.prefetch] {
            if let Some(index) = queue
                .iter()
                .rposition(|id| dir.join(id.cache_name()).exists())
            {
                return Some(queue.remove(index));
            }
        }
        None
    }

    fn len(&self) -> usize {
        self.visible.len() + self.prefetch.len()
    }
}

/// Identifies the application, as the tile usage policy requires.
///
/// The policy asks for a User-Agent that names the application **and carries
/// contact information**. An agent with no way to reach anyone is grounds for
/// being refused — which is what serves the "blocked" tile instead of a map.
/// The contact comes from the Config page, because inventing an address that
/// does not reach anyone would be worse than admitting there is none.
pub fn user_agent(contact: &str) -> String {
    let contact = contact.trim();
    if contact.is_empty() {
        format!(
            "geo-ref_mapper/{} (no contact configured)",
            env!("CARGO_PKG_VERSION")
        )
    } else {
        format!("geo-ref_mapper/{} (+{contact})", env!("CARGO_PKG_VERSION"))
    }
}

/// Which tile, at which zoom.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TileId {
    pub zoom: u32,
    pub x: u32,
    pub y: u32,
}

impl TileId {
    fn url(&self) -> String {
        format!(
            "https://tile.openstreetmap.org/{}/{}/{}.png",
            self.zoom, self.x, self.y
        )
    }

    fn cache_name(&self) -> String {
        format!("{}_{}_{}.png", self.zoom, self.x, self.y)
    }
}

// ------------------------------------------------------------- the projection

/// Longitude to tile x. Fractional: the whole part is the tile, the remainder
/// is the position inside it.
pub fn lon_to_tile(lon: f64, zoom: u32) -> f64 {
    let n = f64::from(1_u32 << zoom);
    (lon + 180.0) / 360.0 * n
}

/// Latitude to tile y, through the Web Mercator projection — which is why the
/// map cannot show the poles and why Greenland looks the size of Africa.
pub fn lat_to_tile(lat: f64, zoom: u32) -> f64 {
    let n = f64::from(1_u32 << zoom);
    let radians = lat.clamp(-85.051_128, 85.051_128).to_radians();
    (1.0 - (radians.tan() + 1.0 / radians.cos()).ln() / std::f64::consts::PI) / 2.0 * n
}

pub fn tile_to_lon(x: f64, zoom: u32) -> f64 {
    let n = f64::from(1_u32 << zoom);
    x / n * 360.0 - 180.0
}

pub fn tile_to_lat(y: f64, zoom: u32) -> f64 {
    let n = f64::from(1_u32 << zoom);
    (std::f64::consts::PI * (1.0 - 2.0 * y / n))
        .sinh()
        .atan()
        .to_degrees()
}

// ------------------------------------------------------------- where we are

/// The map's position: a centre in degrees, and a zoom that may sit between
/// whole levels while a wheel gesture is in progress.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub lat: f64,
    pub lon: f64,
    pub zoom: f64,
}

impl Default for View {
    fn default() -> Self {
        // Singapore, which is what this library's maps are of.
        Self {
            lat: 1.3521,
            lon: 103.8198,
            zoom: 11.0,
        }
    }
}

impl View {
    /// The whole zoom level whose tiles are drawn. Tiles exist only at integer
    /// zooms; anything between is those tiles scaled.
    pub fn tile_zoom(&self) -> u32 {
        (self.zoom.round() as u32).clamp(MIN_ZOOM, MAX_ZOOM)
    }

    /// How much a tile is stretched by, to cover a fractional zoom.
    pub fn scale(&self) -> f32 {
        (2.0_f64).powf(self.zoom - f64::from(self.tile_zoom())) as f32
    }

    /// Moves the view by a drag, in screen points.
    pub fn pan(&mut self, delta: eframe::egui::Vec2) {
        let zoom = self.tile_zoom();
        let scale = f64::from(self.scale());
        let x = lon_to_tile(self.lon, zoom) - f64::from(delta.x) / (f64::from(TILE) * scale);
        let y = lat_to_tile(self.lat, zoom) - f64::from(delta.y) / (f64::from(TILE) * scale);
        self.lon = tile_to_lon(x, zoom).clamp(-180.0, 180.0);
        self.lat = tile_to_lat(y, zoom);
    }

    /// Puts a place in the middle, leaving the zoom alone.
    ///
    /// The zoom is deliberately untouched: choosing an anchor from the list is a
    /// request to *look* at it, and changing how far in the map is zoomed would
    /// throw away the scale the user had settled on to compare it with the
    /// picture beside it.
    pub fn centre_on(&mut self, lat: f64, lon: f64) {
        self.lon = lon.clamp(-180.0, 180.0);
        // Past the Mercator limit there is no tile to show, and a view that
        // cannot be drawn is worse than one nudged to the edge of what can.
        self.lat = lat.clamp(-85.051_128, 85.051_128);
    }

    /// Zooms about a point, keeping the ground under it fixed — the same rule
    /// the image canvas uses, and what makes a map feel right rather than
    /// merely correct.
    pub fn zoom_at(&mut self, cursor: eframe::egui::Vec2, by: f64) {
        let before = self.at_offset(cursor);
        self.zoom = (self.zoom + by).clamp(f64::from(MIN_ZOOM), f64::from(MAX_ZOOM));
        let after = self.at_offset(cursor);
        self.lat = (self.lat + before.0 - after.0).clamp(-85.0, 85.0);
        self.lon = (self.lon + before.1 - after.1).clamp(-180.0, 180.0);
    }

    /// The coordinates at a point this many screen points from the centre.
    pub fn at_offset(&self, offset: eframe::egui::Vec2) -> (f64, f64) {
        let zoom = self.tile_zoom();
        let scale = f64::from(self.scale());
        let x = lon_to_tile(self.lon, zoom) + f64::from(offset.x) / (f64::from(TILE) * scale);
        let y = lat_to_tile(self.lat, zoom) + f64::from(offset.y) / (f64::from(TILE) * scale);
        (tile_to_lat(y, zoom), tile_to_lon(x, zoom))
    }
}

// ---------------------------------------------------------------- the tiles

/// Half the width of the Web Mercator world, in its own metres.
pub const EDGE: f64 = 20_037_508.342_789_244;

/// Spacings for a lat/lon grid, coarse first.
///
/// Whole degrees, then minutes, then seconds — the units these are read and
/// written in. A decimal ladder would put lines at 0.02°, which is nothing
/// anybody has ever marked on a chart.
const GRATICULE_STEPS: [f64; 15] = [
    30.0,
    10.0,
    5.0,
    2.0,
    1.0,
    30.0 / 60.0,
    15.0 / 60.0,
    5.0 / 60.0,
    2.0 / 60.0,
    1.0 / 60.0,
    30.0 / 3600.0,
    15.0 / 3600.0,
    5.0 / 3600.0,
    2.0 / 3600.0,
    1.0 / 3600.0,
];

/// The spacing to draw across a span of this many degrees.
///
/// The coarsest that still puts a few lines on screen. Erring coarse is
/// deliberate: a grid dense enough to read positions off is dense enough to
/// hide the map under it.
pub fn graticule_step(span: f64) -> f64 {
    const WANTED: f64 = 4.0;
    let span = span.abs();
    GRATICULE_STEPS
        .into_iter()
        .find(|step| span / step >= WANTED)
        .unwrap_or(GRATICULE_STEPS[GRATICULE_STEPS.len() - 1])
}

/// Degrees, minutes and seconds, shown only as finely as the spacing warrants.
///
/// A line every minute labelled `103°37′00″E` is three characters of noise on
/// every one of them; the seconds appear when there are seconds to tell apart.
///
/// Anything finer than the spacing is dropped rather than rounded, so a label
/// never disagrees with the line it is written against.
pub fn dms(value: f64, hemispheres: (char, char), step: f64) -> String {
    let hemisphere = if value < 0.0 {
        hemispheres.1
    } else {
        hemispheres.0
    };
    // Rounded to whole seconds first, so 37 minutes never prints as 36′60″.
    let total = (value.abs() * 3600.0).round();
    let degrees = (total / 3600.0).floor();
    let minutes = ((total - degrees * 3600.0) / 60.0).floor();
    let seconds = total - degrees * 3600.0 - minutes * 60.0;

    if step >= 1.0 {
        format!("{degrees:.0}°{hemisphere}")
    } else if step >= 1.0 / 60.0 {
        format!("{degrees:.0}°{minutes:02.0}′{hemisphere}")
    } else {
        format!("{degrees:.0}°{minutes:02.0}′{seconds:02.0}″{hemisphere}")
    }
}

/// Holds a zoom inside the levels tiles exist at.
pub fn clamp_zoom(zoom: f64) -> f64 {
    zoom.clamp(f64::from(MIN_ZOOM), f64::from(MAX_ZOOM))
}

/// Mercator metres covered by one screen point at a zoom.
///
/// Mercator metres, not ground metres: they run large away from the equator by
/// a factor of one over the cosine of the latitude. That is the right unit here
/// because a placement's scale is in the same ones, so the two cancel and the
/// picture and the map end up drawn at genuinely the same size.
pub fn metres_per_point(zoom: f64) -> f64 {
    EDGE * 2.0 / (f64::from(TILE) * (2.0_f64).powf(zoom))
}

/// The zoom at which one screen point covers this many Mercator metres — the
/// way back through [`metres_per_point`].
pub fn zoom_for_metres_per_point(metres: f64) -> f64 {
    (EDGE * 2.0 / (f64::from(TILE) * metres)).log2()
}

/// Latitude and longitude to Web Mercator metres — the numbers a GeoTIFF
/// stores, and the ones the placement is worked out in.
pub fn to_mercator(lat: f64, lon: f64) -> (f64, f64) {
    let clamped = lat.clamp(-85.051_129, 85.051_129).to_radians();
    (
        lon / 180.0 * EDGE,
        ((clamped / 2.0 + std::f64::consts::FRAC_PI_4).tan()).ln() * EDGE / std::f64::consts::PI,
    )
}

/// Web Mercator metres back to latitude and longitude.
pub fn from_mercator(x: f64, y: f64) -> (f64, f64) {
    (
        (y / EDGE * std::f64::consts::PI).sinh().atan().to_degrees(),
        x / EDGE * 180.0,
    )
}

/// A georeferenced picture laid over the map.
pub struct Overlay {
    pub path: PathBuf,
    pub geo: crate::geotiff::GeoRef,
    /// 0 is the map alone, 1 is the picture alone.
    pub opacity: f32,
    /// Squares of the map, drawn at the level the zoom asks for — the same way
    /// the viewer draws it.
    pub squares: crate::imagetiles::Squares,
    /// One stretched copy, for a picture with no squares to read. A last
    /// resort: it goes soft as soon as you zoom in.
    pub whole: Option<eframe::egui::TextureHandle>,
    /// A placement worked out by hand, used instead of anything in the file.
    ///
    /// Lets a map be looked at on the basemap *before* deciding whether the
    /// anchors were good enough to write down — which is the only way to judge
    /// that honestly.
    pub placement: Option<crate::anchor::Placement>,
    /// Full-size dimensions, for turning a pixel into a place.
    pub size: (u32, u32),
}

impl View {
    /// Moves the view to hold a whole georeferenced picture.
    ///
    /// Zoom is chosen so the picture's longest edge covers about four fifths of
    /// the window — enough margin to see it sitting in its surroundings, which
    /// is the point of putting it on a map at all.
    pub fn fit(
        &mut self,
        north_west: (f64, f64),
        south_east: (f64, f64),
        window: eframe::egui::Vec2,
    ) {
        let (north, west) = north_west;
        let (south, east) = south_east;
        self.lat = (north + south) / 2.0;
        self.lon = (west + east) / 2.0;

        // Solve at one zoom and scale from there: tile span is proportional to
        // 2^zoom, so the correction is a single logarithm rather than a search.
        const REFERENCE: u32 = 10;
        let span_x = (lon_to_tile(east, REFERENCE) - lon_to_tile(west, REFERENCE)).abs();
        let span_y = (lat_to_tile(south, REFERENCE) - lat_to_tile(north, REFERENCE)).abs();
        let wanted_x = f64::from(window.x) * 0.8 / f64::from(TILE);
        let wanted_y = f64::from(window.y) * 0.8 / f64::from(TILE);

        let by = match (span_x > 0.0, span_y > 0.0) {
            (true, true) => (wanted_x / span_x).min(wanted_y / span_y),
            (true, false) => wanted_x / span_x,
            (false, true) => wanted_y / span_y,
            // A picture with no extent has no scale to fit; leave the zoom be.
            (false, false) => return,
        };
        self.zoom =
            (f64::from(REFERENCE) + by.log2()).clamp(f64::from(MIN_ZOOM), f64::from(MAX_ZOOM));
    }
}

impl View {
    /// Screen pixels one image pixel covers, for a picture placed on the map.
    ///
    /// The map draws the picture across `Δlongitude/360 · TILE · 2^zoom`
    /// screen pixels — the tile grid's own scale, since a tile is `TILE` points
    /// wide and the world is `2^zoom` tiles across. Dividing by the raster's
    /// width gives the same quantity the Viewer calls zoom, which is what makes
    /// the two comparable at all.
    pub fn scale_against(&self, geo: &crate::geotiff::GeoRef) -> Option<f64> {
        let (_, west) = geo.top_left()?;
        let (_, east) = geo.bottom_right()?;
        if geo.width == 0 {
            return None;
        }
        let across = (east - west).abs() / 360.0 * f64::from(TILE) * self.zoom.exp2();
        Some(across / f64::from(geo.width))
    }

    /// Sets the zoom so one image pixel covers `per_pixel` screen pixels.
    ///
    /// The inverse of [`scale_against`](Self::scale_against), so a picture
    /// handed back and forth between the map and the Viewer keeps the size it
    /// had — the toggle changes what is around the picture, not the picture.
    pub fn match_scale(&mut self, geo: &crate::geotiff::GeoRef, per_pixel: f64) {
        let (Some((_, west)), Some((_, east))) = (geo.top_left(), geo.bottom_right()) else {
            return;
        };
        let span = (east - west).abs() / 360.0 * f64::from(TILE);
        if span <= 0.0 || per_pixel <= 0.0 || geo.width == 0 {
            return;
        }
        let wanted = per_pixel * f64::from(geo.width) / span;
        self.zoom = wanted
            .log2()
            .clamp(f64::from(MIN_ZOOM), f64::from(MAX_ZOOM));
    }

    /// Where in the raster the middle of the map is sitting, as a fraction of
    /// its width and height. Outside 0..1 when the map has been panned off it.
    pub fn fraction_within(&self, geo: &crate::geotiff::GeoRef) -> Option<(f64, f64)> {
        let (north, west) = geo.top_left()?;
        let (south, east) = geo.bottom_right()?;
        if (east - west).abs() <= 0.0 || (south - north).abs() <= 0.0 {
            return None;
        }
        Some((
            (self.lon - west) / (east - west),
            (self.lat - north) / (south - north),
        ))
    }

    /// Points the map at a place in the raster, given as a fraction of its
    /// width and height.
    pub fn look_at_fraction(&mut self, geo: &crate::geotiff::GeoRef, across: f64, down: f64) {
        let (Some((north, west)), Some((south, east))) = (geo.top_left(), geo.bottom_right())
        else {
            return;
        };
        self.lon = west + (east - west) * across;
        self.lat = north + (south - north) * down;
    }
}

/// What the tile source is doing, for the corner of the map.
#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub pending: usize,
    pub queued: usize,
    pub resident: usize,
    pub refused: u64,
}

/// A tile that has arrived, ready to upload.
pub struct Fetched {
    pub id: TileId,
    pub image: eframe::egui::ColorImage,
}

/// Fetches tiles, caches them on disk, and hands them to the interface.
pub struct Tiles {
    wanted: Arc<Mutex<Queue>>,
    inflight: std::collections::HashSet<TileId>,
    results: Receiver<Result<Fetched, TileId>>,
    textures: HashMap<TileId, eframe::egui::TextureHandle>,
    /// Tiles that would not load. Kept so a failure is not asked for again on
    /// the very next frame, which would turn one refusal into a flood.
    failed: std::collections::HashSet<TileId>,
    pub loaded: u64,
    pub refused: u64,
}

impl Tiles {
    pub fn new(ctx: eframe::egui::Context, agent: String) -> Self {
        if let Some(dir) = cache_dir() {
            discard_stale_cache(&dir);
        }
        let wanted = Arc::new(Mutex::new(Queue::default()));
        let (sender, results) = channel();

        for index in 0..NETWORK_WORKERS + CACHE_WORKERS {
            let disk_only = index >= NETWORK_WORKERS;
            let wanted = Arc::clone(&wanted);
            let sender: Sender<Result<Fetched, TileId>> = sender.clone();
            let ctx = ctx.clone();
            let agent = agent.clone();
            std::thread::spawn(move || worker(&wanted, &sender, &ctx, &agent, disk_only));
        }

        Self {
            wanted,
            inflight: std::collections::HashSet::new(),
            results,
            textures: HashMap::new(),
            failed: std::collections::HashSet::new(),
            loaded: 0,
            refused: 0,
        }
    }

    /// Asks for a tile, unless it is already here or on its way.
    pub fn request(&mut self, id: TileId, want: Want) {
        if self.textures.contains_key(&id)
            || self.inflight.contains(&id)
            || self.failed.contains(&id)
        {
            return;
        }
        self.inflight.insert(id);
        if let Ok(mut wanted) = self.wanted.lock() {
            wanted.push(id, want);
        }
    }

    /// The nearest coarser tile that covers this one, and which part of it to
    /// draw.
    ///
    /// A zoomed-out tile has the same ground on it, just softer. Drawing it
    /// stretched is what keeps the map from ever being blank: pan or zoom into
    /// new ground and there is a blurry version there immediately, replaced the
    /// moment the sharp one lands. Coarse tiles are also a quarter as numerous
    /// per step, so the ones this needs are usually already in hand.
    pub fn ancestor(&self, id: &TileId) -> Option<(&eframe::egui::TextureHandle, Rect)> {
        for step in 1..=MAX_ANCESTOR_STEPS {
            if step > id.zoom {
                break;
            }
            let coarser = TileId {
                zoom: id.zoom - step,
                x: id.x >> step,
                y: id.y >> step,
            };
            if let Some(texture) = self.textures.get(&coarser) {
                return Some((texture, ancestor_uv(id, step)));
            }
        }
        None
    }

    /// Uploads whatever arrived since the last frame.
    pub fn poll(&mut self, ctx: &eframe::egui::Context) {
        while let Ok(result) = self.results.try_recv() {
            match result {
                Ok(tile) => {
                    self.inflight.remove(&tile.id);
                    let texture = ctx.load_texture(
                        format!("tile{}_{}_{}", tile.id.zoom, tile.id.x, tile.id.y),
                        tile.image,
                        eframe::egui::TextureOptions::LINEAR,
                    );
                    self.textures.insert(tile.id, texture);
                    self.loaded += 1;
                }
                // Reported rather than dropped: a failure that never came back
                // left the tile "in flight" for ever, so it was never drawn and
                // never retried, and the pending count only ever grew.
                Err(id) => {
                    self.inflight.remove(&id);
                    self.failed.insert(id);
                    self.refused += 1;
                }
            }
        }
    }

    /// Forgets past failures, so a map that was blocked can be tried again
    /// once the contact details are filled in.
    pub fn retry_failed(&mut self) {
        self.failed.clear();
    }

    pub fn get(&self, id: &TileId) -> Option<&eframe::egui::TextureHandle> {
        self.textures.get(id)
    }

    pub fn pending(&self) -> usize {
        self.inflight.len()
    }

    /// How many are queued but not yet started, so the overlay can distinguish
    /// "working on it" from "a long way to go".
    pub fn queued(&self) -> usize {
        self.wanted.lock().map(|queue| queue.len()).unwrap_or(0)
    }

    /// Tiles held as textures. Bounded by [`trim`], not by chance.
    ///
    /// [`trim`]: Self::trim
    pub fn resident(&self) -> usize {
        self.textures.len()
    }

    /// Video memory the tiles in hand are holding.
    pub fn bytes(&self) -> usize {
        self.textures
            .values()
            .map(|texture| {
                let [width, height] = texture.size();
                width * height * 4
            })
            .sum()
    }

    /// Lets go of every tile, for a card that has run out of room.
    ///
    /// The disk cache is the real store, so the ones still being looked at
    /// come back without touching the network. Past failures are left alone —
    /// a tile the server refused was not refused for want of video memory, and
    /// asking again would be asking a server that already said no.
    pub fn shed(&mut self) {
        self.textures.clear();
    }

    /// Drops textures far from where the map is looking.
    ///
    /// Prefetching without this would grow the texture set without limit — the
    /// disk cache is meant to be the big store, video memory is not. Coarse
    /// tiles are kept regardless: there are few of them and they are what
    /// [`ancestor`](Self::ancestor) draws while the sharp ones arrive.
    pub fn trim(&mut self, centre: (f64, f64), zoom: u32, keep: usize) {
        if self.textures.len() <= keep {
            return;
        }
        for id in expendable(self.textures.keys().copied(), centre, zoom, keep) {
            self.textures.remove(&id);
        }
    }
}

/// Which held tiles to let go of, worst first.
///
/// Ranked by distance from where the map is looking, in tiles at that tile's
/// own zoom, so levels compare fairly. Coarse tiles sort to the front of the
/// keep list unconditionally: there are few of them and they are what
/// [`Tiles::ancestor`] draws while the sharp ones arrive, so dropping them
/// would make every pan blink grey.
fn expendable(
    held: impl Iterator<Item = TileId>,
    centre: (f64, f64),
    zoom: u32,
    keep: usize,
) -> Vec<TileId> {
    let (cx, cy) = centre;
    let mut ranked: Vec<(i64, TileId)> = held
        .map(|id| {
            if id.zoom + MAX_ANCESTOR_STEPS < zoom {
                return (i64::MIN, id); // a stand-in, always worth keeping
            }
            // The view centre expressed at this tile's own zoom.
            let scale = 2_f64.powi(id.zoom as i32 - zoom as i32);
            let dx = f64::from(id.x) - cx * scale;
            let dy = f64::from(id.y) - cy * scale;
            (((dx * dx + dy * dy) / scale.max(1e-9)) as i64, id)
        })
        .collect();
    if ranked.len() <= keep {
        return Vec::new();
    }
    ranked.sort_unstable_by_key(|(distance, _)| *distance);
    ranked.drain(keep..).map(|(_, id)| id).collect()
}

/// Which part of a tile `step` levels coarser holds this tile's ground.
///
/// Each step up halves the span and the tile's own low bits say which quarter
/// it fell into, so the offset is just those bits scaled.
fn ancestor_uv(id: &TileId, step: u32) -> Rect {
    let span = 1.0 / (1_u32 << step) as f32;
    let left = (id.x & ((1 << step) - 1)) as f32 * span;
    let top = (id.y & ((1 << step) - 1)) as f32 * span;
    Rect::from_min_size(pos2(left, top), vec2(span, span))
}

/// Longest edge to decode when a picture has no squares to read.
const WHOLE_EDGE: u32 = 4096;

/// Prepares a georeferenced picture for laying over the map.
///
/// Nothing is decoded for a picture stored in squares — the squares are read as
/// they are needed, at whatever level the zoom asks for, so placing a gigapixel
/// map is instant and stays sharp however far in you go.
///
/// A picture with no squares has no such option: one copy is decoded and
/// stretched, which is what the map used to do for everything.
pub fn load_overlay(
    ctx: &eframe::egui::Context,
    path: &std::path::Path,
    geo: crate::geotiff::GeoRef,
) -> Result<Overlay, String> {
    let mut squares = crate::imagetiles::Squares::new(ctx);
    squares.open(path);

    let whole = if squares.shapes().is_empty() {
        let mut tiff = crate::cog::TiledTiff::open(path).map_err(|error| error.to_string())?;
        let level = tiff.level_for(WHOLE_EDGE);
        let image = tiff
            .decode_level(level)
            .map_err(|error| format!("could not decode level {level}: {error}"))?;
        let size = [image.width() as usize, image.height() as usize];
        Some(ctx.load_texture(
            format!("overlay:{}", path.display()),
            eframe::egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()),
            eframe::egui::TextureOptions::LINEAR,
        ))
    } else {
        None
    };

    let size = squares
        .shapes()
        .first()
        .map(|shape| (shape.width, shape.height))
        .unwrap_or_default();

    // The turn the file records, if it records one.
    //
    // `GeoRef` reduces a `ModelTransformation` to a corner and a pixel size,
    // which is everything the simple case needs and throws the rotation away —
    // so the overlay drew a plain upright rectangle and every hand-turned map
    // came out square to north. Correctly *positioned*, and visibly not lining
    // up with anything, which reads as the saved placement being ignored.
    //
    // Only a placement takes the turned drawing path, and until now only one
    // worked out from anchors in this session ever set it. A placement read
    // back from the file is the same thing written down.
    let placement = crate::geotiff::placement_of(path).map(|(placement, _, _)| placement);

    Ok(Overlay {
        path: path.to_path_buf(),
        geo,
        opacity: 0.75,
        squares,
        whole,
        placement,
        size,
    })
}

/// Where tiles are kept between runs.
/// Throws away a cache written by an older build.
///
/// Tiles are cached by coordinate and never expire, which is right for map
/// tiles and wrong for the refusal images an older build stored as though they
/// were map tiles. Those decode and draw like any other tile, so nothing would
/// ever replace them. Wiping on a version change costs a re-fetch of one
/// screenful and removes a whole class of "it is still broken after you fixed
/// it".
fn discard_stale_cache(dir: &std::path::Path) {
    let stamp = dir.join(".version");
    if std::fs::read_to_string(&stamp).is_ok_and(|held| held.trim() == env!("CARGO_PKG_VERSION")) {
        return;
    }
    if dir.exists() {
        let _ = std::fs::remove_dir_all(dir);
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&stamp, env!("CARGO_PKG_VERSION"));
}

pub fn cache_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(cache) if !cache.is_empty() => PathBuf::from(cache),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
    };
    crate::db::adopt_legacy_dir(&base.join("imgViewer"), &base.join("geo-ref_mapper"));
    Some(base.join("geo-ref_mapper/tiles"))
}

fn worker(
    wanted: &Arc<Mutex<Queue>>,
    sender: &Sender<Result<Fetched, TileId>>,
    ctx: &eframe::egui::Context,
    agent: &str,
    disk_only: bool,
) {
    loop {
        let next = wanted.lock().ok().and_then(|mut wanted| {
            if disk_only {
                wanted.take_cached()
            } else {
                wanted.take()
            }
        });
        let Some(id) = next else {
            // Sleeping beats spinning a core on an idle map.
            std::thread::sleep(std::time::Duration::from_millis(40));
            continue;
        };
        let message = match load_tile(id, agent) {
            Some(image) => Ok(Fetched { id, image }),
            None => Err(id),
        };
        if sender.send(message).is_err() {
            return; // The app has gone.
        }
        ctx.request_repaint();
    }
}

/// Disk first, then the network. A tile never changes, so a cached one is
/// always as good as a fresh one.
fn load_tile(id: TileId, agent: &str) -> Option<eframe::egui::ColorImage> {
    let cached = cache_dir().map(|dir| dir.join(id.cache_name()));
    if let Some(path) = &cached
        && let Ok(bytes) = std::fs::read(path)
        && let Some(image) = decode(&bytes)
    {
        return Some(image);
    }

    let bytes = fetch(id, agent)?;
    if let Some(path) = &cached {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, &bytes);
    }
    decode(&bytes)
}

/// Fetches one tile, refusing to believe a 200 that is really a refusal.
///
/// A blocked tile is *not* an HTTP error — the server answers 200 with a PNG
/// that reads "access denied", which decodes perfectly and caches perfectly and
/// then sits on the map forever. It is only distinguishable by the `x-blocked`
/// header, which says so in as many words, and by `cache-control: no-cache`.
/// Both are checked, so a change to one still leaves the other.
fn fetch(id: TileId, agent: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut response = ureq::get(&id.url())
        .header("User-Agent", agent)
        .call()
        .ok()?;

    if let Some(reason) = response.headers().get("x-blocked") {
        eprintln!(
            "geo-ref_mapper: the tile server refused {} — {}",
            id.url(),
            reason.to_str().unwrap_or("no reason given"),
        );
        return None;
    }
    // Anything the server tells us not to keep is not a tile worth keeping.
    if response
        .headers()
        .get("cache-control")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("no-cache"))
    {
        return None;
    }

    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(1024 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

fn decode(bytes: &[u8]) -> Option<eframe::egui::ColorImage> {
    let image = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Some(eframe::egui::ColorImage::from_rgba_unmultiplied(
        size,
        image.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singapores_own_lines_come_out_as_they_are_written() {
        // The west edge of the island and a parallel through the middle of it —
        // the two morgan named, and the ones to check a printed chart against.
        assert_eq!(dms(103.0 + 37.0 / 60.0, ('E', 'W'), 1.0 / 60.0), "103°37′E");
        assert_eq!(dms(1.0 + 22.0 / 60.0, ('N', 'S'), 1.0 / 60.0), "1°22′N");
    }

    #[test]
    fn the_two_hemispheres_are_named_not_signed() {
        assert_eq!(dms(-(7.0 / 60.0), ('E', 'W'), 1.0 / 60.0), "0°07′W");
        assert_eq!(dms(-34.0, ('N', 'S'), 1.0), "34°S");
        assert_eq!(dms(-(7.5 / 60.0), ('E', 'W'), 1.0 / 3600.0), "0°07′30″W");
    }

    #[test]
    fn a_value_between_two_lines_reads_as_the_one_below_it() {
        // Grid lines always sit on exact multiples, so this only arises if the
        // function is used for something else. Truncating is the predictable
        // answer: 7′40″ shown to the minute is 7′, not a rounded 8′ that would
        // then disagree with the line drawn beside it.
        assert_eq!(dms(0.1278, ('E', 'W'), 1.0 / 60.0), "0°07′E");
        assert_eq!(dms(0.1278, ('E', 'W'), 1.0 / 3600.0), "0°07′40″E");
    }

    #[test]
    fn a_line_is_never_labelled_sixty_minutes() {
        // 103.99999° is 103°59′59.996″, and rounding the parts separately turns
        // that into `103°60′00″` — a reading that does not exist.
        let text = dms(103.999_999, ('E', 'W'), 1.0 / 3600.0);
        assert!(!text.contains("60′"), "got {text}");
        assert_eq!(text, "104°00′00″E");
    }

    #[test]
    fn the_spacing_shown_is_the_one_that_can_be_read() {
        // Degrees when the window holds a continent, minutes over a city,
        // seconds over a street.
        assert_eq!(graticule_step(40.0), 10.0);
        assert!((graticule_step(0.4) - 5.0 / 60.0).abs() < 1e-12);
        assert!((graticule_step(0.004) - 2.0 / 3600.0).abs() < 1e-12);
        // And the finest is a floor, not something to fall through.
        assert!(graticule_step(1e-9) > 0.0);
    }

    #[test]
    fn every_spacing_puts_some_lines_on_screen_and_not_a_thicket() {
        for span in [60.0, 12.0, 3.0, 0.5, 0.08, 0.01, 0.002] {
            let lines = span / graticule_step(span);
            assert!(
                (4.0..40.0).contains(&lines),
                "a {span}° window would draw {lines:.0} lines",
            );
        }
    }

    #[test]
    fn a_zoom_and_the_size_it_draws_the_world_at_are_the_way_back_from_each_other() {
        for zoom in [2.0, 11.0, 14.5, 19.0] {
            let metres = metres_per_point(zoom);
            assert!(
                (zoom_for_metres_per_point(metres) - zoom).abs() < 1e-9,
                "zoom {zoom} came back as {}",
                zoom_for_metres_per_point(metres),
            );
        }
    }

    #[test]
    fn one_tile_covers_the_whole_world_at_zoom_zero() {
        // The anchor for the whole scale relationship: 256 points across the
        // world at zoom 0, and half that ground for each level after.
        assert!((metres_per_point(0.0) * f64::from(TILE) - EDGE * 2.0).abs() < 1e-6);
        assert!((metres_per_point(1.0) * 2.0 - metres_per_point(0.0)).abs() < 1e-9);
    }

    #[test]
    fn centring_moves_the_view_without_changing_the_zoom() {
        let mut view = View {
            lat: 1.3521,
            lon: 103.8198,
            zoom: 17.0,
        };
        view.centre_on(51.5074, -0.1278);
        assert!((view.lat - 51.5074).abs() < 1e-9);
        assert!((view.lon + 0.1278).abs() < 1e-9);
        assert_eq!(view.zoom, 17.0, "the scale the user chose must survive");
    }

    #[test]
    fn centring_past_the_projection_stops_at_the_edge_it_can_draw() {
        let mut view = View::default();
        view.centre_on(89.0, 200.0);
        assert!(view.lat <= 85.051_129, "got {}", view.lat);
        assert_eq!(view.lon, 180.0);
        // And the result is still somewhere the tile maths can express.
        assert!(lat_to_tile(view.lat, 11).is_finite());
    }

    #[test]
    fn the_projection_round_trips() {
        for (lat, lon) in [(1.3521, 103.8198), (51.5074, -0.1278), (-33.8688, 151.2093)] {
            for zoom in [2, 11, 19] {
                let x = lon_to_tile(lon, zoom);
                let y = lat_to_tile(lat, zoom);
                assert!(
                    (tile_to_lon(x, zoom) - lon).abs() < 1e-6,
                    "lon {lon} at zoom {zoom}",
                );
                assert!(
                    (tile_to_lat(y, zoom) - lat).abs() < 1e-6,
                    "lat {lat} at zoom {zoom}",
                );
            }
        }
    }

    #[test]
    fn zero_zero_is_the_middle_of_the_world() {
        // At zoom 1 the world is two tiles across, so the origin sits at 1,1.
        assert!((lon_to_tile(0.0, 1) - 1.0).abs() < 1e-9);
        assert!((lat_to_tile(0.0, 1) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_poles_are_clamped_rather_than_producing_infinity() {
        // Mercator sends the poles to infinity; the scheme cuts off at ±85.05.
        let y = lat_to_tile(90.0, 4);
        assert!(y.is_finite(), "a pole must not produce {y}");
        assert!((0.0..=16.0).contains(&y), "and must stay on the map: {y}");
    }

    #[test]
    fn panning_moves_the_ground_the_way_the_hand_went() {
        // Dragging right shows what was to the west, so longitude decreases.
        let mut view = View {
            lat: 0.0,
            lon: 0.0,
            zoom: 4.0,
        };
        view.pan(eframe::egui::vec2(100.0, 0.0));
        assert!(view.lon < 0.0, "lon went to {}", view.lon);

        // Dragging down shows what was to the north.
        let mut view = View {
            lat: 0.0,
            lon: 0.0,
            zoom: 4.0,
        };
        view.pan(eframe::egui::vec2(0.0, 100.0));
        assert!(view.lat > 0.0, "lat went to {}", view.lat);
    }

    #[test]
    fn zooming_about_the_centre_keeps_the_centre() {
        let mut view = View {
            lat: 1.3521,
            lon: 103.8198,
            zoom: 11.0,
        };
        let (lat, lon) = (view.lat, view.lon);
        view.zoom_at(eframe::egui::Vec2::ZERO, 1.0);
        assert!((view.lat - lat).abs() < 1e-6 && (view.lon - lon).abs() < 1e-6);
        assert!((view.zoom - 12.0).abs() < 1e-9);
    }

    #[test]
    fn zooming_about_a_point_keeps_the_ground_under_it() {
        let mut view = View {
            lat: 1.3521,
            lon: 103.8198,
            zoom: 11.0,
        };
        let cursor = eframe::egui::vec2(200.0, -120.0);
        let before = view.at_offset(cursor);
        view.zoom_at(cursor, 1.0);
        let after = view.at_offset(cursor);
        assert!(
            (before.0 - after.0).abs() < 1e-4 && (before.1 - after.1).abs() < 1e-4,
            "the ground moved from {before:?} to {after:?}",
        );
    }

    #[test]
    fn zoom_stays_within_what_the_server_has() {
        let mut view = View::default();
        view.zoom_at(eframe::egui::Vec2::ZERO, 100.0);
        assert!(view.zoom <= f64::from(MAX_ZOOM));
        view.zoom_at(eframe::egui::Vec2::ZERO, -100.0);
        assert!(view.zoom >= f64::from(MIN_ZOOM));
    }
}

#[cfg(test)]
mod prefetch_tests {
    use super::*;

    #[test]
    fn a_tile_reads_the_matching_quarter_of_its_parent() {
        // (2,3) at any zoom is the odd column, odd row of parent (1,1).
        let part = ancestor_uv(
            &TileId {
                zoom: 4,
                x: 3,
                y: 3,
            },
            1,
        );
        assert_eq!(part.min, pos2(0.5, 0.5));
        assert_eq!(part.size(), vec2(0.5, 0.5));

        let part = ancestor_uv(
            &TileId {
                zoom: 4,
                x: 2,
                y: 3,
            },
            1,
        );
        assert_eq!(part.min, pos2(0.0, 0.5), "even x is the left half");
    }

    #[test]
    fn three_steps_up_reads_one_sixty_fourth() {
        let part = ancestor_uv(
            &TileId {
                zoom: 10,
                x: 805,
                y: 507,
            },
            3,
        );
        assert_eq!(part.size(), vec2(0.125, 0.125));
        // 805 mod 8 = 5, 507 mod 8 = 3.
        assert!((part.min.x - 5.0 / 8.0).abs() < 1e-6);
        assert!((part.min.y - 3.0 / 8.0).abs() < 1e-6);
    }

    #[test]
    fn the_screen_is_always_served_before_a_prefetch() {
        let mut queue = Queue::default();
        let far = TileId {
            zoom: 5,
            x: 1,
            y: 1,
        };
        let near = TileId {
            zoom: 5,
            x: 2,
            y: 2,
        };
        queue.push(far, Want::Prefetch);
        queue.push(near, Want::Visible);
        assert_eq!(
            queue.take(),
            Some(near),
            "a long prefetch must never queue in front of the screen"
        );
        assert_eq!(queue.take(), Some(far));
    }

    #[test]
    fn the_queue_drops_the_oldest_rather_than_growing_without_end() {
        let mut queue = Queue::default();
        for x in 0..QUEUE_LIMIT as u32 + 10 {
            queue.push(TileId { zoom: 5, x, y: 0 }, Want::Prefetch);
        }
        assert_eq!(queue.len(), QUEUE_LIMIT);
        // The newest survived and is served first.
        assert_eq!(
            queue.take(),
            Some(TileId {
                zoom: 5,
                x: QUEUE_LIMIT as u32 + 9,
                y: 0
            }),
        );
    }

    #[test]
    fn a_window_with_no_size_yet_does_not_throw_the_view_away() {
        // The map page sets its own size as it draws, so putting a picture on
        // the map before ever visiting that page fits against whatever the
        // default is. Whatever comes out, the centre must still be the picture
        // — a view centred elsewhere is what "the position was ignored" looks
        // like from the outside.
        let mut view = View {
            lat: 0.0,
            lon: 0.0,
            zoom: 3.0,
        };
        view.fit(
            (1.3333, 103.7884),
            (1.2465, 103.8969),
            eframe::egui::vec2(0.0, 0.0),
        );
        assert!((view.lat - 1.29).abs() < 0.01, "{}", view.lat);
        assert!((view.lon - 103.84).abs() < 0.01, "{}", view.lon);
        assert!(view.zoom.is_finite(), "zoom came out at {}", view.zoom);
        assert!(
            view.zoom >= f64::from(MIN_ZOOM) && view.zoom <= f64::from(MAX_ZOOM),
            "zoom came out at {}",
            view.zoom
        );
    }

    #[test]
    fn fitting_a_picture_centres_it_and_zooms_to_hold_it() {
        // The Singapore map: about 0.11° across, near the equator.
        let mut view = View {
            lat: 0.0,
            lon: 0.0,
            zoom: 3.0,
        };
        view.fit(
            (1.3333, 103.7884),
            (1.2465, 103.8969),
            eframe::egui::vec2(900.0, 600.0),
        );

        assert!(
            (view.lat - 1.29).abs() < 0.01,
            "latitude came out at {}",
            view.lat
        );
        assert!(
            (view.lon - 103.84).abs() < 0.01,
            "longitude came out at {}",
            view.lon
        );

        // At this zoom the picture must fit the window with room to spare, and
        // not be so small that it is a speck.
        let span = (lon_to_tile(103.8969, view.tile_zoom())
            - lon_to_tile(103.7884, view.tile_zoom()))
        .abs()
            * f64::from(TILE)
            * f64::from(view.scale());
        assert!(span <= 900.0, "the picture must fit across: {span} px");
        assert!(span > 300.0, "and fill a fair part of it: {span} px");
    }

    #[test]
    fn opacity_holds_every_step_between_clear_and_solid() {
        // The slider works in whole percent and stores a fraction. Rounding the
        // stored value to no decimals — which is what `Slider::max_decimals(0)`
        // does on a 0..=1 range — left this able to hold only 0 or 1.
        let mut held = std::collections::BTreeSet::new();
        for percent in 0..=100 {
            let opacity = percent as f32 / 100.0;
            // The tint the map draws with, which is where a coarse value shows.
            held.insert((opacity.clamp(0.0, 1.0) * 255.0) as u8);
        }
        assert!(
            held.len() > 90,
            "a hundred steps must give nearly a hundred distinct alphas, got {}",
            held.len(),
        );
        assert!(held.contains(&0) && held.contains(&255));
    }

    #[test]
    fn a_scale_survives_the_trip_to_the_viewer_and_back() {
        let geo = crate::geotiff::GeoRef {
            origin: (11_554_603.38, 148_440.71),
            scale: (0.298_582, 0.298_582),
            epsg: Some(3857),
            citations: Vec::new(),
            width: 40448,
            height: 32256,
            transformed: false,
        };

        let mut view = View {
            lat: 1.29,
            lon: 103.85,
            zoom: 12.93,
        };
        let seen_on_the_map = view.scale_against(&geo).expect("it is placeable");

        // Off to the Viewer at that size, and straight back.
        view.match_scale(&geo, seen_on_the_map);
        let after = view.scale_against(&geo).unwrap();
        assert!(
            (after - seen_on_the_map).abs() < 1e-9,
            "the picture changed size on the way: {seen_on_the_map} then {after}",
        );
        assert!((view.zoom - 12.93).abs() < 1e-9, "and so did the zoom");
    }

    #[test]
    fn doubling_the_zoom_doubles_how_big_the_picture_is_drawn() {
        let geo = crate::geotiff::GeoRef {
            origin: (0.0, 0.0),
            scale: (1.0, 1.0),
            epsg: Some(3857),
            citations: Vec::new(),
            width: 1024,
            height: 1024,
            transformed: false,
        };
        let at_ten = View {
            lat: 0.0,
            lon: 0.0,
            zoom: 10.0,
        }
        .scale_against(&geo)
        .unwrap();
        let at_eleven = View {
            lat: 0.0,
            lon: 0.0,
            zoom: 11.0,
        }
        .scale_against(&geo)
        .unwrap();
        assert!((at_eleven / at_ten - 2.0).abs() < 1e-9);
    }

    #[test]
    fn the_middle_of_the_raster_round_trips_through_a_fraction() {
        let geo = crate::geotiff::GeoRef {
            origin: (11_554_603.38, 148_440.71),
            scale: (0.298_582, 0.298_582),
            epsg: Some(3857),
            citations: Vec::new(),
            width: 40448,
            height: 32256,
            transformed: false,
        };
        let mut view = View {
            lat: 0.0,
            lon: 0.0,
            zoom: 12.0,
        };
        view.look_at_fraction(&geo, 0.25, 0.75);
        let (across, down) = view.fraction_within(&geo).unwrap();
        assert!((across - 0.25).abs() < 1e-9, "across came back as {across}");
        assert!((down - 0.75).abs() < 1e-9, "down came back as {down}");
    }

    #[test]
    fn a_picture_with_no_extent_leaves_the_zoom_alone() {
        let mut view = View {
            lat: 0.0,
            lon: 0.0,
            zoom: 7.5,
        };
        view.fit((1.0, 103.0), (1.0, 103.0), eframe::egui::vec2(900.0, 600.0));
        assert_eq!(view.zoom, 7.5, "no size means no scale to fit");
        assert_eq!(view.lat, 1.0, "but it is still somewhere");
    }

    #[test]
    fn trimming_drops_the_far_ones_and_spares_what_is_under_the_eye() {
        let near = TileId {
            zoom: 10,
            x: 800,
            y: 500,
        };
        let far = TileId {
            zoom: 10,
            x: 900,
            y: 500,
        };
        let dropped = expendable([near, far].into_iter(), (800.0, 500.0), 10, 1);
        assert_eq!(dropped, vec![far]);
    }

    #[test]
    fn trimming_never_drops_a_stand_in() {
        // Coarse tiles are what the map draws while sharp ones arrive; losing
        // them would make every pan blink grey.
        let coarse = TileId {
            zoom: 5,
            x: 25,
            y: 15,
        };
        let near = TileId {
            zoom: 10,
            x: 800,
            y: 500,
        };
        let far = TileId {
            zoom: 10,
            x: 940,
            y: 560,
        };
        // Room for two: the coarse tile takes one of the places, ahead of the
        // sharp tile that is further away.
        let dropped = expendable([coarse, near, far].into_iter(), (800.0, 500.0), 10, 2);
        assert_eq!(dropped, vec![far]);

        // Room for one: the coarse tile keeps it, even over the tile in view.
        let dropped = expendable([coarse, near, far].into_iter(), (800.0, 500.0), 10, 1);
        assert_eq!(dropped, vec![near, far]);
    }

    #[test]
    fn trimming_holding_less_than_the_limit_drops_nothing() {
        let ids = (0..5).map(|x| TileId { zoom: 10, x, y: 0 });
        assert!(expendable(ids, (800.0, 500.0), 10, 600).is_empty());
    }
}

#[cfg(test)]
mod overlay_tests {
    /// Places a real NUS map and checks it lands where the map says it should.
    /// `GEO_REF_MAPPER_GEOTIFF=/path/to.tif cargo test --release places_a_real_map -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_GEOTIFF"]
    fn places_a_real_map() {
        let Ok(path) = std::env::var("GEO_REF_MAPPER_GEOTIFF") else {
            return;
        };
        let path = std::path::Path::new(&path);
        let geo = crate::geotiff::from_path(path).expect("the file is georeferenced");
        let (north, west) = geo.top_left().expect("and in a projection we know");
        let (south, east) = geo.bottom_right().unwrap();
        println!("  NW {north:.5}, {west:.5}   SE {south:.5}, {east:.5}");

        let mut tiff = crate::cog::TiledTiff::open(path).expect("it opens");
        let level = tiff.level_for(super::WHOLE_EDGE);
        let image = tiff.decode_level(level).expect("the level decodes");
        println!(
            "  level {level} of {} decoded at {}x{}",
            tiff.level_sizes().len(),
            image.width(),
            image.height(),
        );
        assert!(image.width() > 0 && image.height() > 0);

        let mut view = super::View {
            lat: 0.0,
            lon: 0.0,
            zoom: 3.0,
        };
        view.fit(
            (north, west),
            (south, east),
            eframe::egui::vec2(900.0, 600.0),
        );
        println!(
            "  fitted to {:.5}, {:.5} at zoom {:.2}",
            view.lat, view.lon, view.zoom
        );

        // The fitted view must actually contain the picture.
        let zoom = view.tile_zoom();
        let size = f64::from(super::TILE) * f64::from(view.scale());
        let width = (super::lon_to_tile(east, zoom) - super::lon_to_tile(west, zoom)).abs() * size;
        let height =
            (super::lat_to_tile(south, zoom) - super::lat_to_tile(north, zoom)).abs() * size;
        println!("  drawn at {width:.0} x {height:.0} px in a 900 x 600 window");
        assert!(width <= 900.0 && height <= 600.0, "it must fit the window");
        assert!(width > 200.0 || height > 200.0, "and not be a speck");
    }
}

#[cfg(test)]
mod headers {
    /// A screenful of real tiles, end to end, checking that none of what lands
    /// in the cache is a refusal wearing a tile's clothes.
    ///
    /// Empties the tile cache first, so it starts from nothing — the tiles are
    /// re-fetched, which is the whole point.
    /// `cargo test --release live_fetch -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the real tile server and empties the tile cache"]
    fn live_fetch() {
        let dir = super::cache_dir().unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        // A screenful, asked for the way the map asks for it.
        let ctx = eframe::egui::Context::default();
        let ctx2 = ctx.clone();
        let mut tiles = super::Tiles::new(ctx, super::user_agent(""));
        let wanted: Vec<_> = (804..812)
            .flat_map(|x| (506..512).map(move |y| super::TileId { zoom: 10, x, y }))
            .collect();
        for id in &wanted {
            tiles.request(*id, super::Want::Visible);
        }
        for _ in 0..120 {
            tiles.poll(&ctx2);
            if tiles.pending() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }

        let held: Vec<u64> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "png"))
            .map(|entry| entry.metadata().unwrap().len())
            .collect();
        let blocked = held.iter().filter(|size| **size == 6987).count();
        println!(
            "  {} of {} still pending, {} cached, {blocked} of them refusals, refused counter {}",
            tiles.pending(),
            wanted.len(),
            held.len(),
            tiles.refused,
        );
        assert_eq!(blocked, 0, "a refusal must never reach the cache");
    }

    /// What we actually put on the wire, checked against a local server rather
    /// than assumed. `cargo test --release sent_headers -- --ignored --nocapture`
    #[test]
    #[ignore = "starts a local server"]
    fn sent_headers() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                headers.push(line.trim().to_owned());
            }
            let mut stream = &stream;
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            headers
        });

        let _ = ureq::get(&format!("http://127.0.0.1:{port}/0/0/0.png"))
            .header("User-Agent", &super::user_agent("mail@example.com"))
            .call();

        for header in handle.join().unwrap() {
            println!("  {header}");
        }
    }
}
