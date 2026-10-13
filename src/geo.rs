//! Turning a photo's GPS coordinates into a place you recognise.
//!
//! Two tiers, deliberately separated:
//!
//! - **Offline**, always on. The country comes from a boundary lookup and the
//!   nearest town from a GeoNames extract, both compiled into the binary. No
//!   network, nothing leaves the machine, and it answers in microseconds — so
//!   it can run for every photo as you browse.
//! - **Online**, only when asked. A street address needs a dataset far too
//!   large to carry, so [`Address`] fetches one from Nominatim for a single
//!   coordinate at a time, on an explicit click.
//!
//! The split matters because photo locations are personal. The default reveals
//! nothing to anyone; the online path sends exactly one coordinate, only when
//! the button is pressed, and never in bulk.

use std::sync::OnceLock;

/// A place, as far offline data can describe it.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Place {
    /// Nearest populated place from GeoNames.
    pub town: Option<String>,
    /// First-level division: state, prefecture, province.
    pub region: Option<String>,
    pub country: Option<String>,
    /// ISO 3166-1 alpha-2, kept even when the name is unknown.
    pub country_code: Option<String>,
    /// How far the named town is, in kilometres. Large values mean the photo
    /// was taken somewhere empty, and the town is context rather than location.
    pub town_distance_km: f64,
}

impl Place {
    /// One line for the panel: the most specific parts, largest last.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(town) = &self.town {
            parts.push(town.clone());
        }
        // A region that merely repeats the town adds nothing.
        if let Some(region) = &self.region
            && Some(region) != self.town.as_ref()
        {
            parts.push(region.clone());
        }
        if let Some(country) = &self.country
            && Some(country) != self.region.as_ref()
        {
            parts.push(country.clone());
        }
        if parts.is_empty() {
            return self
                .country_code
                .clone()
                .unwrap_or_else(|| "Unknown".to_owned());
        }
        parts.join(", ")
    }

    /// True when the nearest town is far enough that naming it would mislead.
    pub fn town_is_distant(&self) -> bool {
        self.town_distance_km > 25.0
    }
}

/// The GeoNames extract, built once. Around 7 MB of cities parsed into a
/// k-d tree; doing that per photo would be absurd, so it is done on first use.
fn cities() -> &'static reverse_geocoder::ReverseGeocoder {
    static CITIES: OnceLock<reverse_geocoder::ReverseGeocoder> = OnceLock::new();
    CITIES.get_or_init(reverse_geocoder::ReverseGeocoder::new)
}

/// Country polygons, likewise.
///
/// Worth having in addition to the nearest town: near a border, or at sea, the
/// closest settlement can easily be in the wrong country, and the boundary
/// lookup is the one that is actually right.
fn boundaries() -> Option<&'static country_boundaries::CountryBoundaries> {
    static BOUNDARIES: OnceLock<Option<country_boundaries::CountryBoundaries>> = OnceLock::new();
    BOUNDARIES
        .get_or_init(|| {
            country_boundaries::CountryBoundaries::from_reader(
                country_boundaries::BOUNDARIES_ODBL_360X180,
            )
            .ok()
        })
        .as_ref()
}

/// True when a coordinate is a real position rather than a failed fix.
///
/// Exactly zero in both axes is in the Atlantic off Africa, and is what a
/// camera writes when the GPS never locked. Treating it as a location would put
/// a pin in the ocean for every failed fix — and, worse, would offer to send it
/// to a geocoding service.
///
/// Public so the panel applies the same rule the lookup does: one definition,
/// or the two drift and the UI offers an address for a position that has none.
pub fn has_fix(latitude: f64, longitude: f64) -> bool {
    if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
        return false;
    }
    // Not exact equality: some cameras write a rounded near-zero instead.
    !(latitude.abs() < 0.0001 && longitude.abs() < 0.0001)
}

/// Looks up a coordinate offline. Fast enough to call while browsing.
pub fn lookup(latitude: f64, longitude: f64) -> Option<Place> {
    if !has_fix(latitude, longitude) {
        return None;
    }

    // The boundary lookup is authoritative for the country; the nearest town
    // supplies the rest. Its ids are ISO codes, subdivisions first, so the
    // two-letter entry is the country.
    let country_code = boundaries().and_then(|boundaries| {
        let position = country_boundaries::LatLon::new(latitude, longitude).ok()?;
        boundaries
            .ids(position)
            .into_iter()
            .find(|id| id.len() == 2)
            .map(str::to_owned)
    });

    let nearest = cities().search((latitude, longitude));
    let record = nearest.record;

    // `search` returns squared great-circle distance on the unit sphere; the
    // earth's radius turns that into kilometres.
    let town_distance_km = nearest.distance.max(0.0).sqrt() * 6371.0;

    let country_code = country_code.or_else(|| {
        let code = record.cc.trim();
        (!code.is_empty()).then(|| code.to_owned())
    });
    let country = country_code
        .as_deref()
        .and_then(|code| isocountry::CountryCode::for_alpha2(code).ok())
        .map(|country| country.name().to_owned());

    let blank = |value: &str| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    };

    Some(Place {
        town: blank(&record.name),
        region: blank(&record.admin1),
        country,
        country_code,
        town_distance_km,
    })
}

// ------------------------------------------------------------------- online

/// Where a street address is fetched from, and who is asking.
///
/// Nominatim requires an identifying User-Agent and allows at most one request
/// a second. Both are honoured here rather than left to the caller, because a
/// polite client is the condition of using the service at all.
const NOMINATIM: &str = "https://nominatim.openstreetmap.org/reverse";
const USER_AGENT: &str = concat!(
    "geo-ref_mapper/",
    env!("CARGO_PKG_VERSION"),
    " (personal photo viewer; one lookup per user action)"
);
const MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1100);

/// A street-level lookup, run on a worker thread.
///
/// One coordinate at a time and only on request. There is deliberately no bulk
/// mode: running a library through this would breach Nominatim's usage policy
/// and send thousands of personal locations to a third party.
pub struct Address {
    results: std::sync::mpsc::Receiver<(f64, f64, Result<String, String>)>,
    sender: std::sync::mpsc::Sender<(f64, f64, Result<String, String>)>,
    /// The coordinate currently being fetched, so a second click is ignored.
    pending: Option<(f64, f64)>,
    last_request: Option<std::time::Instant>,
    /// Latest answer, with the coordinate it belongs to.
    pub result: Option<(f64, f64, Result<String, String>)>,
}

impl Default for Address {
    fn default() -> Self {
        let (sender, results) = std::sync::mpsc::channel();
        Self {
            results,
            sender,
            pending: None,
            last_request: None,
            result: None,
        }
    }
}

impl Address {
    pub fn is_busy(&self) -> bool {
        self.pending.is_some()
    }

    /// True when a request now would come too soon after the last.
    pub fn is_rate_limited(&self) -> bool {
        self.last_request
            .is_some_and(|at| at.elapsed() < MIN_INTERVAL)
    }

    /// The address already fetched for this coordinate, if any.
    pub fn cached(&self, latitude: f64, longitude: f64) -> Option<&Result<String, String>> {
        self.result
            .as_ref()
            .filter(|(lat, lon, _)| same_place(*lat, latitude) && same_place(*lon, longitude))
            .map(|(_, _, result)| result)
    }

    /// Starts one lookup. Does nothing if one is already running, if the rate
    /// limit has not elapsed, if this coordinate has already been fetched, or
    /// if the coordinate is not a real fix — sending `0,0` to a geocoder would
    /// be both pointless and a needless disclosure.
    pub fn request(&mut self, latitude: f64, longitude: f64, ctx: eframe::egui::Context) {
        if !has_fix(latitude, longitude)
            || self.is_busy()
            || self.is_rate_limited()
            || self.cached(latitude, longitude).is_some()
        {
            return;
        }
        self.pending = Some((latitude, longitude));
        self.last_request = Some(std::time::Instant::now());

        let sender = self.sender.clone();
        let _ = std::thread::Builder::new()
            .name("geo-ref_mapper-geocode".to_owned())
            .spawn(move || {
                let outcome = fetch(latitude, longitude);
                let _ = sender.send((latitude, longitude, outcome));
                ctx.request_repaint();
            });
    }

    /// Collects a finished lookup. Call once a frame.
    pub fn poll(&mut self) {
        while let Ok(answer) = self.results.try_recv() {
            self.pending = None;
            self.result = Some(answer);
        }
    }
}

/// Two coordinates that came from the same photo, allowing for float noise.
fn same_place(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// One blocking HTTP request. Runs on a worker thread, never the UI thread.
fn fetch(latitude: f64, longitude: f64) -> Result<String, String> {
    // `accept-language=en` is not a preference, it is a requirement: Nominatim
    // returns names in the local script by default, and a Japanese address
    // renders as a row of empty boxes in the bundled font.
    let url = format!(
        "{NOMINATIM}?format=json&zoom=18&addressdetails=0\
         &accept-language=en&lat={latitude:.7}&lon={longitude:.7}"
    );

    let mut response = ureq::get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .call()
        .map_err(|error| format!("{error}"))?;

    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("{error}"))?;

    let address = json_string_field(&body, "display_name")
        .or_else(|| json_string_field(&body, "error"))
        .ok_or_else(|| "no address for this position".to_owned())?;

    // Even asking for English, some places have no Latin name. Saying so beats
    // showing a line of boxes the font cannot draw.
    if !is_renderable(&address) {
        return Err(format!(
            "address is in a script this build has no font for ({} characters)",
            address.chars().count()
        ));
    }
    Ok(address)
}

/// True when the bundled font can be expected to draw this text.
///
/// egui ships Latin, Greek and Cyrillic coverage plus punctuation. Anything
/// substantially outside that — CJK, Arabic, Devanagari — comes out as boxes,
/// which looks like a bug rather than a missing font.
fn is_renderable(text: &str) -> bool {
    let drawable = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .filter(|c| (*c as u32) < 0x0500 || matches!(*c as u32, 0x2000..=0x206F))
        .count();
    let total = text.chars().filter(|c| !c.is_whitespace()).count();
    total == 0 || drawable * 100 / total.max(1) >= 80
}

/// Pulls one string field out of a flat JSON object.
///
/// A whole JSON parser would be a dependency for a single field. This reads the
/// value the same way one would by hand: find the key, take the quoted string
/// after it, and honour backslash escapes so a name containing a quote does not
/// truncate the result.
fn json_string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)? + needle.len();
    let rest = json.get(start..)?;

    // Skip the colon and any whitespace, then require an opening quote.
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let mut chars = rest.strip_prefix('"')?.chars();

    let mut out = String::new();
    while let Some(character) = chars.next() {
        match character {
            '"' => return Some(out),
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    // Four hex digits; anything malformed is dropped rather
                    // than allowed to derail the rest of the string.
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Ok(code) = u32::from_str_radix(&hex, 16)
                        && let Some(decoded) = char::from_u32(code)
                    {
                        out.push(decoded);
                    }
                }
                Some(other) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    // Unterminated string: better to say nothing than to show a fragment.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_city_resolves_to_the_right_country() {
        // Singapore.
        let place = lookup(1.3521, 103.8198).expect("should resolve");
        assert_eq!(place.country_code.as_deref(), Some("SG"));
        assert_eq!(place.country.as_deref(), Some("Singapore"));
        assert!(!place.town_is_distant(), "a city should have a town nearby");

        // Fukuoka, from one of morgan's photo folders.
        let place = lookup(33.5904, 130.4106).expect("should resolve");
        assert_eq!(place.country_code.as_deref(), Some("JP"));
        assert_eq!(place.country.as_deref(), Some("Japan"));
        assert!(place.summary().contains("Japan"), "{}", place.summary());
    }

    #[test]
    fn the_southern_and_western_hemispheres_work() {
        // Sydney.
        let place = lookup(-33.8688, 151.2093).expect("should resolve");
        assert_eq!(place.country_code.as_deref(), Some("AU"));

        // Reykjavik, matching the Iceland folder.
        let place = lookup(64.1466, -21.9426).expect("should resolve");
        assert_eq!(place.country_code.as_deref(), Some("IS"));
        assert_eq!(place.country.as_deref(), Some("Iceland"));
    }

    #[test]
    fn a_failed_gps_fix_is_not_reported_as_the_atlantic() {
        // Exactly zero is what a camera writes when it never got a lock.
        assert!(!has_fix(0.0, 0.0));
        assert!(!has_fix(0.00001, -0.00001));
        assert!(lookup(0.0, 0.0).is_none());
        assert!(lookup(0.00001, -0.00001).is_none());

        // A real position near the equator is still fine — only both axes
        // being zero means no fix, not either one alone.
        assert!(has_fix(1.3521, 103.8198));
        assert!(lookup(1.3521, 103.8198).is_some());
        assert!(has_fix(0.0, 103.8198), "on the equator is a real place");
        assert!(has_fix(1.3521, 0.0), "on the prime meridian is too");
    }

    /// The online path must refuse a non-fix as well: `0,0` should never be
    /// sent to a geocoding service.
    #[test]
    fn a_non_fix_is_never_sent_to_the_network() {
        let ctx = eframe::egui::Context::default();
        let mut address = Address::default();

        address.request(0.0, 0.0, ctx.clone());
        assert!(!address.is_busy(), "0,0 must not start a request");
        assert!(
            !address.is_rate_limited(),
            "a refused request should not consume the rate limit either"
        );

        // A real coordinate still goes through.
        address.request(51.5007, -0.1246, ctx);
        assert!(address.is_busy());
    }

    #[test]
    fn impossible_coordinates_are_refused() {
        assert!(lookup(91.0, 0.0).is_none());
        assert!(lookup(-91.0, 0.0).is_none());
        assert!(lookup(0.0, 181.0).is_none());
        assert!(lookup(f64::NAN, 0.0).is_none());
    }

    #[test]
    fn the_middle_of_an_ocean_still_names_a_country_or_nothing_silly() {
        // Deep Pacific: no boundary contains it, and the nearest town is far.
        let place = lookup(-30.0, -140.0).expect("a nearest town always exists");
        assert!(
            place.town_is_distant(),
            "{} km should count as distant",
            place.town_distance_km
        );
    }

    #[test]
    fn the_summary_does_not_repeat_itself() {
        let place = Place {
            town: Some("Singapore".to_owned()),
            region: Some("Singapore".to_owned()),
            country: Some("Singapore".to_owned()),
            country_code: Some("SG".to_owned()),
            town_distance_km: 1.0,
        };
        assert_eq!(place.summary(), "Singapore");

        let place = Place {
            town: Some("Fukuoka".to_owned()),
            region: Some("Fukuoka".to_owned()),
            country: Some("Japan".to_owned()),
            country_code: Some("JP".to_owned()),
            town_distance_km: 2.0,
        };
        assert_eq!(place.summary(), "Fukuoka, Japan");
    }

    #[test]
    fn an_empty_place_still_says_something() {
        assert_eq!(Place::default().summary(), "Unknown");
        let code_only = Place {
            country_code: Some("JP".to_owned()),
            ..Default::default()
        };
        assert_eq!(code_only.summary(), "JP");
    }

    #[test]
    fn json_fields_are_read_including_escapes_and_unicode() {
        let json = r#"{"place_id":1,"display_name":"12 Main St, Springfield","lat":"1.0"}"#;
        assert_eq!(
            json_string_field(json, "display_name").as_deref(),
            Some("12 Main St, Springfield")
        );

        // An escaped quote must not end the value early.
        let json = r#"{"display_name":"The \"Old\" Mill, Kent"}"#;
        assert_eq!(
            json_string_field(json, "display_name").as_deref(),
            Some("The \"Old\" Mill, Kent")
        );

        // A \u escape, as Nominatim returns for non-Latin names.
        let json = r#"{"display_name":"福岡, Japan"}"#;
        assert_eq!(
            json_string_field(json, "display_name").as_deref(),
            Some("福岡, Japan")
        );

        // Whitespace around the colon is legal JSON.
        assert_eq!(
            json_string_field(r#"{"a" :  "b"}"#, "a").as_deref(),
            Some("b")
        );
    }

    #[test]
    fn malformed_json_yields_nothing_rather_than_a_fragment() {
        assert!(json_string_field("", "display_name").is_none());
        assert!(json_string_field("{}", "display_name").is_none());
        // Missing the closing quote — a truncated response.
        assert!(json_string_field(r#"{"display_name":"half"#, "display_name").is_none());
        // The key is present but the value is not a string.
        assert!(json_string_field(r#"{"display_name":42}"#, "display_name").is_none());
    }

    #[test]
    fn text_the_font_cannot_draw_is_recognised() {
        assert!(is_renderable("12 Main St, Springfield, England"));
        assert!(is_renderable("Rue de l'Église, Québec"));
        // Cyrillic and Greek are covered by the bundled font.
        assert!(is_renderable("Тверская улица, Москва"));

        // These would come out as boxes.
        assert!(!is_renderable("福岡市中央区天神"));
        assert!(!is_renderable("東京都渋谷区"));

        // A mostly-Latin address with one foreign word is still fine.
        assert!(is_renderable("Tenjin 天神, Fukuoka, Japan"));
        assert!(is_renderable(""));
    }

    #[test]
    fn the_online_lookup_rate_limits_and_deduplicates() {
        let ctx = eframe::egui::Context::default();
        let mut address = Address::default();
        assert!(!address.is_busy());
        assert!(!address.is_rate_limited());

        address.request(1.0, 2.0, ctx.clone());
        assert!(address.is_busy(), "the first request should start");
        // A second one must not pile on.
        assert!(address.is_rate_limited());

        // A cached answer is returned without another request.
        address.pending = None;
        address.result = Some((1.0, 2.0, Ok("somewhere".to_owned())));
        assert_eq!(
            address
                .cached(1.0, 2.0)
                .and_then(|r| r.as_ref().ok())
                .map(String::as_str),
            Some("somewhere")
        );
        assert!(
            address.cached(9.0, 9.0).is_none(),
            "a different photo is not cached"
        );
    }

    /// Checks the online path against Nominatim.
    ///
    /// Deliberately uses a public landmark rather than a photo's coordinates:
    /// verifying the code works should not require sending anyone's personal
    /// location to a third party.
    ///
    /// `cargo test --release bench_address -- --ignored --nocapture`
    #[test]
    #[ignore = "makes a network request"]
    fn bench_address_against_nominatim() {
        // Big Ben, London.
        match fetch(51.5007, -0.1246) {
            Ok(address) => {
                println!("  51.5007,-0.1246 -> {address}");
                assert!(!address.is_empty());
                assert!(
                    is_renderable(&address),
                    "an English result should be drawable"
                );
            }
            Err(error) => println!("  lookup failed (offline?): {error}"),
        }
    }

    /// Resolves the coordinates of morgan's actual photo folders, which are
    /// named after the position their photo manager recorded.
    ///
    /// `cargo test --release bench_geo -- --ignored --nocapture`
    #[test]
    #[ignore = "reports against a real library"]
    fn bench_geo_over_real_folders() {
        let folders = std::fs::read_dir("/media/morgan/T5 EVO/Library/Events");
        let Ok(folders) = folders else {
            eprintln!("library not mounted");
            return;
        };

        let mut resolved = 0;
        for entry in folders.filter_map(Result::ok).take(400) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(coords) = name.split("Place @ ").nth(1) else {
                continue;
            };
            let mut parts = coords.split(',');
            let (Some(lat), Some(lon)) = (
                parts.next().and_then(|v| v.trim().parse::<f64>().ok()),
                parts.next().and_then(|v| v.trim().parse::<f64>().ok()),
            ) else {
                continue;
            };
            if let Some(place) = lookup(lat, lon) {
                resolved += 1;
                if resolved <= 20 {
                    println!("  {lat:>9.4},{lon:>10.4}  {}", place.summary());
                }
            } else {
                println!("  {lat:>9.4},{lon:>10.4}  (no fix)");
            }
        }
        println!("\n  {resolved} folders resolved to a place\n");
    }
}
