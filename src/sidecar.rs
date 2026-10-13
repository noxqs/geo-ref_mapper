//! Reading the sidecar written beside a source map.
//!
//! Maps arrive with a `<picture>.histomap.json` next to them holding what the
//! archive said — the title, the year, a description of what the map shows,
//! where it came from — and what has been decided about them since: a crop,
//! whether the scan is inverted, the tags the user set.
//!
//! **None of that survives conversion on its own.** The converted map is a new
//! file with a new name, and the sidecar names the old one, so a map that has
//! been through the converter arrives at the other end anonymous. This module
//! reads the sidecar so `convert` can put its contents inside the file, where
//! they cannot be separated from the picture again.
//!
//! Everything here is best-effort. A sidecar that is missing, truncated, or
//! written by some later version must cost the metadata and never the
//! conversion — a map without a title is a map; a map that would not convert is
//! nothing.

use crate::tiffwrite::Value;
use std::path::{Path, PathBuf};

/// The suffix added to the *whole* file name, extension and all:
/// `1857 map.tif` is described by `1857 map.tif.histomap.json`.
const SUFFIX: &str = ".histomap.json";

/// What the sidecar knows, once the archive's blob has been taken apart.
///
/// Everything optional, because every one of these is missing from some real
/// file — including, on 66 of them, the entire source block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sidecar {
    pub title: Option<String>,
    /// The date exactly as written: `c.1873-1881`, `18/06/1825`, `1930s`.
    ///
    /// Kept verbatim on purpose. Roughly 470 of these carry a "circa" and 238
    /// are spans, and no fixed-width date field can hold either without
    /// inventing a precision the archive deliberately refused to claim.
    pub date_text: Option<String>,
    /// The same date reduced to what ISO 8601 can actually express — a year, or
    /// a `1873/1881` interval. For sorting; never for showing.
    pub date_iso: Option<String>,
    /// What the map shows, in the archive's own words.
    pub description: Option<String>,
    /// `Survey Map`, `Chart`, and so on.
    pub kind: Option<String>,
    pub provider: Option<String>,
    /// The record page — a citation that stays valid, not the image file.
    pub url: Option<String>,
    pub accession: Option<String>,
    /// Where the picture itself can be fetched again.
    pub iiif: Option<String>,
    pub rights: Option<String>,
    /// The user's own tags, with the ones that only repeat other fields removed.
    pub keywords: Vec<String>,
    /// Four corners in 0–1 of the picture, clockwise from the top left.
    ///
    /// Normalised rather than in pixels because the converted file holds the
    /// map at six to ten sizes, and a pixel coordinate would not say which one
    /// it meant. A quad rather than a rectangle because a scanned map's border
    /// is not square to its edges.
    pub crop: Option<[[f64; 2]; 4]>,
    pub invert: bool,
}

/// The crop that means "nothing is cropped".
pub const WHOLE: [[f64; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];

/// Where the sidecar for a picture would be.
pub fn path_for(source: &Path) -> Option<PathBuf> {
    let name = source.file_name()?.to_str()?;
    Some(source.with_file_name(format!("{name}{SUFFIX}")))
}

/// Reads the sidecar beside a picture, if there is one worth reading.
pub fn beside(source: &Path) -> Option<Sidecar> {
    let text = std::fs::read_to_string(path_for(source)?).ok()?;
    parse(&text)
}

/// Takes one apart. `None` only when the text is not JSON at all.
pub fn parse(text: &str) -> Option<Sidecar> {
    let root: serde_json::Value = serde_json::from_str(text).ok()?;
    let string = |key: &str| -> Option<String> { trimmed(root.get(key)?.as_str()?) };

    let source = root.get("source");
    let from_source = |key: &str| -> Option<String> { trimmed(source?.get(key)?.as_str()?) };

    // The archive's own fields arrive as one flat blob, so the interesting
    // parts have to be dug back out of it.
    let blob = from_source("provider_info").unwrap_or_default();
    let info = split_provider_info(&blob);
    let field = |want: &str| -> Option<String> {
        info.iter()
            .find(|(key, _)| *key == want)
            .and_then(|(_, value)| trimmed(value))
    };

    let date_text = string("date");
    let tags: Vec<String> = root
        .get("tags")
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|tag| trimmed(tag.as_str()?))
                .collect()
        })
        .unwrap_or_default();

    Some(Sidecar {
        // The archive's title is the better one when both exist: the sidecar's
        // own `name` is empty on plenty of files.
        title: field("Title").or_else(|| string("name")),
        date_iso: iso_date(date_text.as_deref(), &tags),
        date_text,
        description: field("Scope and Content"),
        kind: field("Type of Map"),
        provider: from_source("provider"),
        url: from_source("url"),
        accession: from_source("accession_number"),
        iiif: from_source("iiif"),
        rights: field("Conditions Governing Access"),
        keywords: tags.into_iter().filter(|tag| descriptive(tag)).collect(),
        crop: crop_of(&root),
        invert: root
            .get("invert")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

fn trimmed(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn crop_of(root: &serde_json::Value) -> Option<[[f64; 2]; 4]> {
    let corners = root.get("crop")?.as_array()?;
    if corners.len() != 4 {
        return None;
    }
    let mut out = [[0.0_f64; 2]; 4];
    for (index, corner) in corners.iter().enumerate() {
        let pair = corner.as_array()?;
        out[index] = [pair.first()?.as_f64()?, pair.get(1)?.as_f64()?];
    }
    // An uncropped map says so by carrying the whole of itself. Storing that as
    // "there is a crop" would make every map look edited.
    (out != WHOLE).then_some(out)
}

/// A tag is worth keeping only if it says something no other field does.
///
/// Measured across 7,699 real sidecars: `nas` appears on 7,611 of them and only
/// repeats the provider, and the rest are overwhelmingly years, which only
/// repeat the date. What is left — `SG`, `full island`, `NUS libmaps`,
/// `aerial` — is the part a keyword field is for.
fn descriptive(tag: &str) -> bool {
    !tag.eq_ignore_ascii_case("nas") && !is_year(tag)
}

fn is_year(text: &str) -> bool {
    text.len() == 4 && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// Every maximal run of exactly four digits, in order.
///
/// Maximal on purpose: an accession number like `20080000141` holds `2008` as a
/// substring and is not a year, so only runs that are four digits *and stop*
/// count.
fn years(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if !bytes[at].is_ascii_digit() {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        if at - start == 4 {
            found.push(&text[start..at]);
        }
    }
    found
}

/// The sortable form of a date, or nothing when there is no year to be had.
///
/// A span becomes an ISO 8601 interval — `c.1873-1881` is `1873/1881` — which
/// keeps both ends rather than collapsing to one. The circa is simply absent,
/// because ISO 8601 has no way to say it; that is what `date_text` is for.
///
/// The first year in the text is the start of the span. Worth stating because
/// the year *tag* is not: on all 238 real spans the tag holds the **end** year,
/// so a map surveyed 1830–1880 would sort as 1880 if the tag were trusted. The
/// tag is used only when the date says nothing at all.
fn iso_date(date: Option<&str>, tags: &[String]) -> Option<String> {
    if let Some(text) = date {
        let found = years(text);
        if let (Some(first), Some(last)) = (found.first(), found.last()) {
            return Some(if first == last {
                (*first).to_owned()
            } else {
                format!("{first}/{last}")
            });
        }
    }
    tags.iter().find(|tag| is_year(tag)).cloned()
}

/// The archive's fields, dug out of the one string they arrive in.
///
/// `provider_info` reads as `Key: value; Key: value`, but **splitting on `"; "`
/// does not work**: the descriptions contain semicolons of their own — "Two
/// Insets; …" — and a plain split shreds those records, inventing about forty
/// spurious keys across the real collection. So a separator only starts a new
/// field when a *known* key follows it.
fn split_provider_info(blob: &str) -> Vec<(&'static str, &str)> {
    /// The keys seen across the collection, most on better than 7,000 files.
    const KEYS: [&str; 16] = [
        "Title",
        "Collection",
        "Covering Date",
        "Accession Number",
        "Source Reference Number",
        "Source",
        "Type of Item",
        "Type of Map",
        "Media - Image Number",
        "Microfilm Number",
        "Scope and Content",
        "Publication Notes",
        "Conditions Governing Access",
        "Language/ Script of material",
        "Other information about copies",
        "Creator",
    ];

    // `Source Reference Number` is listed before `Source` above and matched in
    // that order, so the shorter key cannot claim the longer one's text.
    let starts_here = |at: usize| -> Option<&'static str> {
        KEYS.into_iter()
            .find(|key| blob[at..].starts_with(key) && blob[at + key.len()..].starts_with(": "))
    };

    let mut marks: Vec<(usize, &'static str)> = Vec::new();
    if let Some(key) = starts_here(0) {
        marks.push((0, key));
    }
    let mut from = 0;
    while let Some(offset) = blob[from..].find("; ") {
        // `"; "` is ASCII, so what follows it is always a character boundary.
        let at = from + offset + 2;
        if let Some(key) = starts_here(at) {
            marks.push((at, key));
        }
        from = at;
    }

    let mut out = Vec::with_capacity(marks.len());
    for (index, (at, key)) in marks.iter().enumerate() {
        let from = at + key.len() + 2;
        // The value runs to the separator before the next field, not into it.
        let to = marks
            .get(index + 1)
            .map_or(blob.len(), |(next, _)| next.saturating_sub(2));
        if from <= to {
            out.push((*key, &blob[from..to]));
        }
    }
    out
}

// ------------------------------------------------------------------ into a TIFF

/// Tags private to this program, from the range TIFF reserves for exactly that.
/// Every conformant reader skips tags it does not know, so nothing else in the
/// world is disturbed by these.
pub mod tag {
    /// Eight doubles: four `x, y` corners in 0–1 of the picture.
    pub const CROP: u16 = 65000;
    /// 1 when the scan is a negative of the map.
    pub const INVERT: u16 = 65001;

    /// What the map shows — the tag every other program surfaces unasked.
    pub const IMAGE_DESCRIPTION: u16 = 270;
    //
    // The title is deliberately *not* written to `DocumentName` (269). That tag
    // is read back as EXIF and shown in the panel's SOFTWARE group under the
    // label "Document", which then wins over the XMP row saying "Title" — so a
    // map's name appeared as a document name under a heading about software.
    // `dc:title` says it properly, and exiftool and GDAL read that too.
    //
    /// The conditions the archive puts on using it.
    pub const COPYRIGHT: u16 = 33432;
    /// The XMP packet.
    pub const XMP: u16 = 700;
}

/// What to write into a converted map, split by where in the file it goes.
#[derive(Debug, Default)]
pub struct Embedded {
    /// Written near the front of the file: large, and rewritten later.
    pub early: Vec<(u16, Value)>,
    /// Written beside the directory with everything else: small and fixed.
    pub extra: Vec<(u16, Value)>,
}

/// How much room the XMP packet gets, whatever it currently holds.
///
/// The crop, the inversion and the user's tags all change *after* the file is
/// written, and a TIFF value can only be overwritten in place by one of exactly
/// its own size. XMP is built for this — the standard has writers pad the packet
/// with blanks precisely so it can be rewritten without moving — so the room is
/// taken once, here, and every later edit is an overwrite rather than a rebuild
/// of the file.
const XMP_ROOM: usize = 4096;

/// Room left over *beyond* whatever the packet already holds.
///
/// A fixed size alone is not enough: the longest archive description runs to
/// 3,446 characters, which with the other fields overflows 4 KB on its own — and
/// a packet that exactly fills its space has nowhere to put a keyword the user
/// adds later, quietly costing the in-place rewrite it was padded for. The room
/// is therefore the larger of the two.
const XMP_SLACK: usize = 2048;

/// Everything a converted map should carry, sidecar or no sidecar.
///
/// Called even when there is nothing beside the picture, because the crop, the
/// inversion and the packet are written at their full size either way. A file
/// that leaves out the fields it has no value for yet has nowhere to put them
/// later without being rewritten end to end.
pub fn embed(source: &Path) -> Embedded {
    of(beside(source).as_ref())
}

/// The same, from a sidecar already in hand — which is what the tests use.
pub fn of(sidecar: Option<&Sidecar>) -> Embedded {
    let mut extra = Vec::new();

    let corners = sidecar.and_then(|side| side.crop).unwrap_or(WHOLE);
    extra.push((
        tag::CROP,
        Value::Doubles(corners.iter().flatten().copied().collect()),
    ));
    extra.push((
        tag::INVERT,
        Value::Shorts(vec![u16::from(sidecar.is_some_and(|side| side.invert))]),
    ));

    if let Some(side) = sidecar {
        if let Some(text) = &side.description {
            extra.push((tag::IMAGE_DESCRIPTION, Value::Ascii(text.clone())));
        }
        if let Some(text) = &side.rights {
            extra.push((tag::COPYRIGHT, Value::Ascii(text.clone())));
        }
    }

    Embedded {
        early: vec![(tag::XMP, Value::Bytes(xmp_packet(sidecar)))],
        extra,
    }
}

/// Builds the XMP packet.
///
/// Dublin Core throughout, because these are archive records and every one of
/// them has a term that already means the right thing — a map's publisher is
/// the institution holding it, and the record page is literally "the resource
/// from which this one is derived". Only the verbatim date needs a term of our
/// own, since no standard date field can hold `c.1873-1881`.
fn xmp_packet(sidecar: Option<&Sidecar>) -> Vec<u8> {
    let mut body = String::new();
    if let Some(side) = sidecar {
        if let Some(text) = &side.title {
            body.push_str(&alt("dc:title", text));
        }
        if let Some(text) = &side.description {
            body.push_str(&alt("dc:description", text));
        }
        // The date as written is only worth its own field when it says
        // something the sortable form cannot. For a plain `1857` the two are
        // the same sentence twice; for `c.1873-1881` the circa is the part no
        // standard date field can carry.
        let written = side
            .date_text
            .clone()
            .filter(|text| Some(text) != side.date_iso.as_ref());
        for (term, value) in [
            ("dc:date", &side.date_iso),
            ("dc:type", &side.kind),
            ("dc:publisher", &side.provider),
            ("dc:source", &side.url),
            ("dc:identifier", &side.accession),
            ("dc:relation", &side.iiif),
            ("imgv:dateText", &written),
        ] {
            if let Some(text) = value {
                body.push_str(&element(term, text));
            }
        }
        if let Some(text) = &side.rights {
            body.push_str(&alt("dc:rights", text));
        }
        if !side.keywords.is_empty() {
            body.push_str(&bag("dc:subject", &side.keywords));
        }
    }

    let packet = format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"{}\">\n\
         \x20<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
         \x20 <rdf:Description rdf:about=\"\"\n\
         \x20   xmlns:dc=\"http://purl.org/dc/elements/1.1/\"\n\
         \x20   xmlns:imgv=\"https://imgviewer.local/ns/1.0/\">\n\
         {body}\
         \x20 </rdf:Description>\n\
         \x20</rdf:RDF>\n\
         </x:xmpmeta>\n",
        escape(&crate::tiffwrite::software()),
    );

    // The padding sits outside the packet's own end, so a reader takes the same
    // bytes whether or not it is there.
    let mut out = packet.into_bytes();
    let tail = b"<?xpacket end=\"w\"?>";
    let room = XMP_ROOM.max(out.len() + tail.len() + XMP_SLACK);
    let padding = room.saturating_sub(out.len() + tail.len());
    for index in 0..padding {
        // Broken into lines rather than one long run of blanks, which is what
        // the standard suggests and what makes the file readable in an editor.
        out.push(if index % 80 == 79 { b'\n' } else { b' ' });
    }
    out.extend_from_slice(tail);
    out
}

fn element(term: &str, text: &str) -> String {
    format!("\x20  <{term}>{}</{term}>\n", escape(text))
}

/// A language-alternative field, which is how Dublin Core spells anything a
/// human reads.
fn alt(term: &str, text: &str) -> String {
    format!(
        "\x20  <{term}><rdf:Alt><rdf:li xml:lang=\"x-default\">{}</rdf:li></rdf:Alt></{term}>\n",
        escape(text),
    )
}

/// An unordered list, which is how keywords are spelled.
fn bag(term: &str, items: &[String]) -> String {
    let mut out = format!("\x20  <{term}><rdf:Bag>");
    for item in items {
        out.push_str(&format!("<rdf:li>{}</rdf:li>", escape(item)));
    }
    out.push_str(&format!("</rdf:Bag></{term}>\n"));
    out
}

/// The five characters XML cannot carry as themselves.
///
/// These descriptions are archive prose and routinely contain `&` and quotes;
/// one unescaped ampersand turns the whole packet into something a strict
/// reader throws away.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(character),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sidecar in the shape the real ones take.
    fn real() -> &'static str {
        r#"{
          "version": 1,
          "name": "",
          "date": "c.1873-1881",
          "source": {
            "url": "https://www.nas.gov.sg/record/49598c64",
            "provider": "National Archives of Singapore — Maps & Building Plans",
            "provider_info": "Title: A General Plan of the Town; Covering Date: 1857; Accession Number: SP004422_1; Type of Map: Survey Map; Scope and Content: Map shows contours, roads & jetties. Two Insets; one of the harbour; Conditions Governing Access: Viewing permitted.",
            "accession_number": "SP004422_1",
            "iiif": "https://iiif.micr.io/abc",
            "downloaded": "2026-07-29T19:33:39Z"
          },
          "tags": ["nas", "1881", "SG", "full island"],
          "crop": [[0.05, 0.05], [0.95, 0.05], [0.95, 0.9], [0.05, 0.9]],
          "invert": true,
          "pins": [],
          "placement": null
        }"#
    }

    #[test]
    fn the_archives_fields_come_out_of_the_blob() {
        let side = parse(real()).expect("it parses");
        assert_eq!(side.title.as_deref(), Some("A General Plan of the Town"));
        assert_eq!(side.kind.as_deref(), Some("Survey Map"));
        assert_eq!(side.accession.as_deref(), Some("SP004422_1"));
        assert_eq!(side.rights.as_deref(), Some("Viewing permitted."));
    }

    #[test]
    fn a_description_keeps_its_own_semicolons() {
        // The failure this guards: splitting on "; " cuts this description in
        // two and invents "one of the harbour" as a field name.
        let side = parse(real()).unwrap();
        assert_eq!(
            side.description.as_deref(),
            Some("Map shows contours, roads & jetties. Two Insets; one of the harbour"),
        );
    }

    #[test]
    fn the_written_date_survives_and_a_sortable_one_is_derived() {
        let side = parse(real()).unwrap();
        assert_eq!(side.date_text.as_deref(), Some("c.1873-1881"));
        // Both ends kept, and the *start* leads — the year tag says 1881.
        assert_eq!(side.date_iso.as_deref(), Some("1873/1881"));
    }

    #[test]
    fn a_span_is_never_collapsed_to_its_end() {
        for (written, expected) in [
            ("1857", "1857"),
            ("c.1820", "1820"),
            ("18/06/1825", "1825"),
            ("1930s", "1930"),
            ("c.1550-1600", "1550/1600"),
            ("01/01/1789 - 21/12/1868", "1789/1868"),
            // Both ends the same year is not a span.
            ("01/01/1789-31/12/1789", "1789"),
        ] {
            assert_eq!(
                iso_date(Some(written), &[]).as_deref(),
                Some(expected),
                "{written}",
            );
        }
    }

    #[test]
    fn an_empty_date_falls_back_to_the_year_tag() {
        let tags = vec!["nas".to_owned(), "1893".to_owned()];
        assert_eq!(iso_date(Some(""), &tags).as_deref(), Some("1893"));
        assert_eq!(iso_date(None, &tags).as_deref(), Some("1893"));
    }

    #[test]
    fn a_long_number_holding_a_year_is_not_a_year() {
        // `20080000141` is an image number that starts with 2008. Only a run of
        // four digits that *stops* counts, or half the collection would be
        // dated by its filing reference.
        assert!(years("20080000141").is_empty());
        assert!(years("SP004422_1").is_empty());
        assert_eq!(years("01/01/1789 - 21/12/1868"), vec!["1789", "1868"]);
    }

    #[test]
    fn only_the_tags_that_say_something_new_are_kept() {
        let side = parse(real()).unwrap();
        assert_eq!(side.keywords, vec!["SG", "full island"]);
    }

    #[test]
    fn an_uncropped_map_carries_no_crop() {
        let text = r#"{"crop": [[0,0],[1,0],[1,1],[0,1]], "tags": []}"#;
        assert_eq!(parse(text).unwrap().crop, None);
        let side = parse(real()).unwrap();
        assert_eq!(side.crop.unwrap()[0], [0.05, 0.05]);
        assert!(side.invert);
    }

    #[test]
    fn a_sidecar_that_is_not_json_costs_only_the_metadata() {
        assert_eq!(parse("{ this is not json"), None);
        assert_eq!(parse("null"), Some(Sidecar::default()));
    }

    #[test]
    fn the_packet_escapes_what_xml_cannot_carry() {
        let side = parse(real()).unwrap();
        let packet = String::from_utf8(xmp_packet(Some(&side))).unwrap();
        assert!(packet.contains("roads &amp; jetties"), "{packet}");
        assert!(!packet.contains("roads & jetties"));
        assert!(packet.contains("<dc:date>1873/1881</dc:date>"));
        assert!(packet.contains("<imgv:dateText>c.1873-1881</imgv:dateText>"));
        assert!(packet.contains("<rdf:li>full island</rdf:li>"));
    }

    #[test]
    fn the_packet_is_padded_so_it_can_be_rewritten_in_place() {
        let side = parse(real()).unwrap();
        let full = xmp_packet(Some(&side));
        let empty = xmp_packet(None);
        assert_eq!(
            full.len(),
            XMP_ROOM,
            "an ordinary packet takes the fixed room"
        );
        assert_eq!(empty.len(), XMP_ROOM);
        assert!(full.ends_with(b"<?xpacket end=\"w\"?>"));

        // A description longer than the fixed room must still leave slack, or
        // the very maps with the most to say are the ones that cannot be
        // edited in place afterwards.
        let mut wordy = side.clone();
        wordy.description = Some("Map shows ".repeat(400));
        let packet = xmp_packet(Some(&wordy));
        assert!(packet.len() > XMP_ROOM, "it grew past the fixed room");
        let text = String::from_utf8(packet.clone()).unwrap();
        let body_end = text.find("</x:xmpmeta>").expect("a whole packet") + "</x:xmpmeta>".len();
        assert!(
            packet.len() - body_end >= XMP_SLACK,
            "only {} bytes of slack left",
            packet.len() - body_end,
        );
    }

    #[test]
    fn a_map_with_no_sidecar_still_gets_room_for_one() {
        let embedded = of(None);
        let crop = embedded
            .extra
            .iter()
            .find(|(tag, _)| *tag == tag::CROP)
            .expect("the crop is always there");
        assert_eq!(
            crop.1,
            Value::Doubles(vec![0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0]),
        );
        assert!(embedded.extra.iter().any(|(tag, _)| *tag == tag::INVERT));
        assert_eq!(embedded.early.len(), 1);
    }

    #[test]
    fn the_sidecar_sits_beside_the_picture_extension_and_all() {
        let path = path_for(Path::new("/maps/1857 plan.tif")).unwrap();
        assert_eq!(path, Path::new("/maps/1857 plan.tif.histomap.json"));
    }
}

/// Reads every sidecar under a directory and reports what came out.
///
/// The tests above are written against the shapes these files take; this is how
/// that claim is checked against the files themselves rather than against my
/// memory of them.
///
/// `GEO_REF_MAPPER_SIDECAR_DIR=~/Pictures cargo test sidecar_survey -- --ignored --nocapture`
#[cfg(test)]
mod real_files {
    use super::*;

    #[test]
    #[ignore = "needs GEO_REF_MAPPER_SIDECAR_DIR"]
    fn sidecar_survey() {
        let Ok(root) = std::env::var("GEO_REF_MAPPER_SIDECAR_DIR") else {
            return;
        };

        let (mut seen, mut unreadable) = (0_usize, 0_usize);
        let mut counts = std::collections::BTreeMap::<&str, usize>::new();
        let mut odd_dates: Vec<String> = Vec::new();

        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_name().to_string_lossy().ends_with(SUFFIX) {
                continue;
            }
            seen += 1;
            let Some(side) = std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|text| parse(&text))
            else {
                unreadable += 1;
                println!("  unreadable: {}", entry.path().display());
                continue;
            };

            for (label, present) in [
                ("title", side.title.is_some()),
                ("date", side.date_text.is_some()),
                ("date sortable", side.date_iso.is_some()),
                ("description", side.description.is_some()),
                ("type", side.kind.is_some()),
                ("provider", side.provider.is_some()),
                ("url", side.url.is_some()),
                ("accession", side.accession.is_some()),
                ("iiif", side.iiif.is_some()),
                ("rights", side.rights.is_some()),
                ("keywords", !side.keywords.is_empty()),
                ("crop", side.crop.is_some()),
                ("inverted", side.invert),
            ] {
                if present {
                    *counts.entry(label).or_default() += 1;
                }
            }

            // A date that produced no sortable year is the interesting failure:
            // it means a form nothing here recognises.
            if let (Some(text), None, true) =
                (&side.date_text, &side.date_iso, odd_dates.len() < 20)
            {
                odd_dates.push(text.clone());
            }
        }

        println!("\n  {seen} sidecars, {unreadable} unreadable");
        for (label, count) in &counts {
            println!("  {count:6}  {label}");
        }
        if !odd_dates.is_empty() {
            println!("  dates with no year in them: {odd_dates:?}");
        }
        assert_eq!(unreadable, 0, "every real sidecar must read");
    }
}
