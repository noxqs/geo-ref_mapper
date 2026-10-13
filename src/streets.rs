//! Looking up a street by name.
//!
//! **Only ever on an explicit request.** Place names shown on the map come from
//! the offline dataset and cost nothing; this asks a public service run on
//! donated hardware, so it happens when somebody types a name and presses the
//! button, and at no other time. Nothing here runs on a pan, a zoom or a
//! selection.
//!
//! The search is biased to what is on screen, which is both better — you are
//! looking for a street in the place you are looking at — and cheaper, since it
//! narrows the work the service has to do.

use std::sync::mpsc::{Receiver, channel};

/// One answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    /// What the service calls it, trimmed to something readable.
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// Its extent, when the service gives one — a street is a line, not a
    /// point, and centring on its midpoint at the wrong zoom shows nothing.
    pub bounds: Option<(f64, f64, f64, f64)>,
}

/// A search in flight, or its answers.
#[derive(Default)]
pub struct Search {
    pub query: String,
    pub results: Vec<Place>,
    pub error: Option<String>,
    pub running: bool,
    incoming: Option<Receiver<Result<Vec<Place>, String>>>,
}

impl Search {
    /// Asks, on its own thread. The window must not wait for a network call.
    pub fn start(&mut self, query: &str, near: (f64, f64, f64, f64), agent: String) {
        let query = query.trim().to_owned();
        if query.is_empty() {
            return;
        }
        self.results.clear();
        self.error = None;
        self.running = true;

        let (sender, receiver) = channel();
        self.incoming = Some(receiver);
        std::thread::Builder::new()
            .name("geo-ref_mapper-street-search".to_owned())
            .spawn(move || {
                let _ = sender.send(lookup(&query, near, &agent));
            })
            .ok();
    }

    /// Picks up an answer, if one has arrived. Cheap to call every frame.
    pub fn poll(&mut self) -> bool {
        let Some(incoming) = &self.incoming else {
            return false;
        };
        match incoming.try_recv() {
            Ok(Ok(places)) => {
                self.results = places;
                self.running = false;
                self.incoming = None;
                true
            }
            Ok(Err(why)) => {
                self.error = Some(why);
                self.running = false;
                self.incoming = None;
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.running = false;
                self.incoming = None;
                true
            }
        }
    }
}

/// The request itself, on a worker thread.
fn lookup(
    query: &str,
    (west, south, east, north): (f64, f64, f64, f64),
    agent: &str,
) -> Result<Vec<Place>, String> {
    // `bounded=0`: the view is a preference, not a fence. A street just off the
    // edge of the window is still the street being looked for, and refusing it
    // would be a puzzling way to answer.
    let url = format!(
        "https://nominatim.openstreetmap.org/search\
         ?q={}&format=jsonv2&limit=8&addressdetails=0\
         &viewbox={west:.5},{north:.5},{east:.5},{south:.5}&bounded=0",
        urlencode(query),
    );

    let mut response = ureq::get(&url)
        .header("User-Agent", agent)
        .call()
        .map_err(|error| format!("{error}"))?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("{error}"))?;
    parse(&body)
}

/// Takes the answers apart.
pub fn parse(body: &str) -> Result<Vec<Place>, String> {
    let root: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "the reply was not JSON".to_owned())?;
    let list = root.as_array().ok_or("the reply was not a list")?;

    let places: Vec<Place> = list
        .iter()
        .filter_map(|item| {
            let lat = item.get("lat")?.as_str()?.parse().ok()?;
            let lon = item.get("lon")?.as_str()?.parse().ok()?;
            // The full display name runs to "Orchard Road, Somerset, Singapore,
            // 238841, Singapore" — the tail repeats for every result and pushes
            // the part that distinguishes them off the edge of a narrow panel.
            let full = item.get("display_name")?.as_str()?;
            let name = shorten(full);
            let bounds = item.get("boundingbox").and_then(|raw| {
                let numbers: Vec<f64> = raw
                    .as_array()?
                    .iter()
                    .filter_map(|v| v.as_str()?.parse().ok())
                    .collect();
                match numbers.as_slice() {
                    // Nominatim gives south, north, west, east.
                    [s, n, w, e] => Some((*w, *s, *e, *n)),
                    _ => None,
                }
            });
            Some(Place {
                name,
                lat,
                lon,
                bounds,
            })
        })
        .collect();
    Ok(places)
}

/// The first two parts of a display name — enough to tell results apart.
fn shorten(full: &str) -> String {
    let parts: Vec<&str> = full.split(',').map(str::trim).collect();
    match parts.as_slice() {
        [] => full.to_owned(),
        [one] => (*one).to_owned(),
        [one, two, ..] => format!("{one}, {two}"),
    }
}

/// Percent-encoding, for the query only.
fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = r#"[
      {"lat":"1.3048","lon":"103.8318",
       "display_name":"Orchard Road, Somerset, Singapore, 238841, Singapore",
       "boundingbox":["1.3000","1.3100","103.8300","103.8400"]},
      {"lat":"1.2966","lon":"103.8547","display_name":"North Bridge Road, Singapore"}
    ]"#;

    #[test]
    fn answers_come_back_with_their_position_and_extent() {
        let places = parse(REPLY).expect("it parses");
        assert_eq!(places.len(), 2);
        assert_eq!(places[0].name, "Orchard Road, Somerset");
        assert!((places[0].lat - 1.3048).abs() < 1e-9);
        // south, north, west, east on the way in; west, south, east, north out.
        assert_eq!(places[0].bounds, Some((103.83, 1.30, 103.84, 1.31)));
    }

    #[test]
    fn a_result_without_an_extent_is_still_a_result() {
        // A point of interest has no bounding box, and dropping it would lose
        // an answer for the sake of a field that is only an optimisation.
        let places = parse(REPLY).unwrap();
        assert_eq!(places[1].name, "North Bridge Road, Singapore");
        assert_eq!(places[1].bounds, None);
    }

    #[test]
    fn a_name_is_shortened_to_the_part_that_distinguishes_it() {
        // Every result in Singapore ends "…, Singapore, 2388xx, Singapore",
        // which is identical on all of them and pushes the useful part off a
        // narrow panel.
        assert_eq!(
            shorten("Orchard Road, Somerset, Singapore, 238841, Singapore"),
            "Orchard Road, Somerset"
        );
        assert_eq!(shorten("Bukit Timah"), "Bukit Timah");
    }

    #[test]
    fn a_query_survives_being_put_in_a_url() {
        assert_eq!(urlencode("north bridge road"), "north+bridge+road");
        assert_eq!(urlencode("Jalan Besar"), "Jalan+Besar");
        // The characters that would otherwise end the query or start another.
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert!(urlencode("café").contains('%'));
    }

    #[test]
    fn nonsense_is_refused_rather_than_returned_empty() {
        assert!(parse("not json at all").is_err());
        assert!(parse(r#"{"error":"whatever"}"#).is_err());
        // An empty list is a valid answer meaning "nothing found".
        assert_eq!(parse("[]").unwrap().len(), 0);
    }
}
