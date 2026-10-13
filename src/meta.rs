//! Text metadata that is not EXIF.
//!
//! Four independent containers, each with its own format, each of which can
//! appear in several image formats:
//!
//!   * **XMP** — Adobe's RDF/XML packet. Carries the creating application, a
//!     document id, rating, keywords and an edit history.
//!   * **IPTC** — the older press/agency records, inside a Photoshop `8BIM`
//!     block. Caption, byline, credit, keywords.
//!   * **ICC** — the embedded colour profile's own name, which is far more
//!     specific than EXIF's three-value colour-space enum.
//!   * **Free text** — PNG `tEXt`/`iTXt`/`zTXt` chunks and the JPEG `COM`
//!     marker, where writers put whatever they like.
//!
//! Finding each block differs per image format, so the file is walked once to
//! locate the raw blocks and the parsers below are shared across all of them.
//! Everything here is best-effort: metadata is frequently truncated, mislabelled
//! or written by software that got the spec wrong, and a bad block must cost a
//! row in a panel, never the picture.

use std::collections::BTreeMap;
use std::path::Path;

/// How much of a file to read looking for metadata. All of these blocks live in
/// the header in practice; a whole 250 MB map does not need to be walked to
/// find out which program wrote it.
const PREFIX: usize = 4 * 1024 * 1024;

/// One `key = value` pair to show, in the order the file listed them.
pub type Pairs = Vec<(String, String)>;

#[derive(Clone, Default, PartialEq, Debug)]
pub struct Metadata {
    pub xmp: Pairs,
    pub iptc: Pairs,
    /// The colour profile's description, e.g. `Display P3`.
    pub icc: Option<String>,
    pub text: Pairs,
}

impl Metadata {
    pub fn is_empty(&self) -> bool {
        self.xmp.is_empty() && self.iptc.is_empty() && self.icc.is_none() && self.text.is_empty()
    }
}

/// Reads everything this module understands out of one file.
pub fn from_path(path: &Path) -> Metadata {
    let Some(bytes) = prefix(path, PREFIX) else {
        return Metadata::default();
    };
    from_bytes(&bytes)
}

fn prefix(path: &Path, limit: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit as u64).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

pub fn from_bytes(bytes: &[u8]) -> Metadata {
    let mut out = Metadata::default();

    // Every container is searched for in every file rather than switching on
    // the format first: a PSD carries an 8BIM block, a JPEG carries the same
    // block inside an APP13 segment, and a TIFF carries it in tag 33723. One
    // scan that knows all three costs less than three format-specific paths.
    if let Some(packet) = find_xmp(bytes) {
        out.xmp = parse_xmp(&packet);
    }
    if let Some(block) = find_iptc(bytes) {
        out.iptc = parse_iptc(&block);
    }
    if let Some(profile) = find_icc(bytes) {
        out.icc = profile_description(profile);
    }
    out.text = free_text(bytes);
    out
}

// ------------------------------------------------------------------------ XMP

/// The XMP packet, whichever wrapper it arrived in.
///
/// Every format stores the same XML; only the envelope differs — JPEG puts it
/// in an APP1 segment behind a namespace URI, PNG in an `iTXt` chunk, TIFF in
/// tag 700, HEIF in a `mime` item. Rather than four finders, the packet is
/// located by its own unmistakable delimiters, which are identical everywhere.
fn find_xmp(bytes: &[u8]) -> Option<Vec<u8>> {
    let start = find(bytes, b"<x:xmpmeta")?;
    let end = find(&bytes[start..], b"</x:xmpmeta>")? + start + b"</x:xmpmeta>".len();
    // Guard against a truncated prefix producing a multi-megabyte "packet".
    if end - start > 2 * 1024 * 1024 {
        return None;
    }
    Some(bytes[start..end].to_vec())
}

/// Fields worth a row, as (XMP property, label). Order is the display order.
///
/// A deliberately short list. XMP files routinely carry a hundred properties,
/// most of them internal bookkeeping, and a panel showing all of them answers
/// no question anyone actually has.
const XMP_FIELDS: &[(&str, &str)] = &[
    ("CreatorTool", "Created with"),
    ("Software", "Software"),
    ("creator", "Author"),
    ("title", "Title"),
    ("description", "Description"),
    // What an archive says about a map. `date` is the sortable form and
    // `dateText` the one the archive actually wrote — the second is only
    // present when it says something the first cannot, so an ordinary `1857`
    // produces one row and `c.1873-1881` produces both.
    ("date", "Date"),
    ("dateText", "Date as written"),
    ("type", "Type"),
    ("publisher", "Held by"),
    ("source", "Record"),
    ("identifier", "Accession"),
    ("relation", "Image source"),
    ("Rating", "Rating"),
    ("Label", "Label"),
    ("subject", "Keywords"),
    ("Lens", "Lens"),
    ("SerialNumber", "Serial number"),
    ("DocumentID", "Document ID"),
    ("rights", "Rights"),
];

/// Pulls the interesting properties out of an XMP packet.
///
/// This reads the XML rather than parsing it. RDF allows each property as
/// either an attribute or an element, and real files use both — often in the
/// same packet — so both spellings are tried. A conforming XML parser would be
/// stricter and would reject the malformed packets that plenty of software
/// emits; here a broken packet should cost one row, not all of them.
pub fn parse_xmp(packet: &[u8]) -> Pairs {
    let text = String::from_utf8_lossy(packet);
    let mut out = Pairs::new();

    for (property, label) in XMP_FIELDS {
        let value = attribute_value(&text, property)
            .or_else(|| element_value(&text, property))
            .map(|value| clean(&unescape(&value)))
            .filter(|value| !value.is_empty());
        if let Some(value) = value {
            out.push(((*label).to_owned(), value));
        }
    }

    // The edit history is a list, and its interesting part is which programs
    // touched the file — not the timestamps or the internal action names.
    let mut agents: Vec<String> = Vec::new();
    for chunk in text.split("softwareAgent").skip(1) {
        if let Some(value) = first_quoted(chunk).or_else(|| first_element_body(chunk)) {
            let value = clean(&value);
            if !value.is_empty() && !agents.contains(&value) {
                agents.push(value);
            }
        }
    }
    if !agents.is_empty() {
        out.push(("Edited with".to_owned(), agents.join(" · ")));
    }

    out
}

/// `…:Property="value"` — the attribute spelling.
fn attribute_value(text: &str, property: &str) -> Option<String> {
    let needle = format!("{property}=\"");
    let start = find_str_after_separator(text, &needle)? + needle.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// `<ns:Property>value</ns:Property>` — the element spelling, including the
/// `rdf:Alt` / `rdf:Seq` wrappers that language-alternative fields use.
///
/// Everything is bounded to the property's own element. Searching the rest of
/// the document for an `<rdf:li>` found one belonging to some unrelated
/// property further down — in a Photoshop file that meant reading a colour
/// table and reporting `Created with: 1`.
fn element_value(text: &str, property: &str) -> Option<String> {
    let needle = format!("{property}>");
    let start = find_str_after_separator(text, &needle)? + needle.len();
    let rest = &text[start..];

    // This element ends at its own closing tag, `</ns:Property>`.
    let close_name = find_str_after_separator(rest, &needle)?;
    let body = &rest[..rest[..close_name].rfind('<')?];

    // A wrapped value: the text lives in the <rdf:li> items. Every one of them
    // is taken, not just the first — keywords are a list, and showing only the
    // first of five is worse than showing none, because nothing says the rest
    // are there.
    if body.contains("<rdf:li") {
        let mut items = Vec::new();
        for chunk in body.split("<rdf:li").skip(1) {
            let Some(open) = chunk.find('>') else {
                continue;
            };
            let Some(close) = chunk.find("</rdf:li>") else {
                continue;
            };
            if open + 1 <= close {
                items.push(chunk[open + 1..close].to_owned());
            }
        }
        if !items.is_empty() {
            return Some(items.join(" · "));
        }
    }
    Some(body.to_owned())
}

/// Puts back the characters XML cannot carry as themselves.
///
/// Without this a panel shows `Maps &amp; Building Plans`, which is the escape
/// rather than the text. It matters more than it looks: an ampersand is ordinary
/// in a title and every one of them arrives escaped.
///
/// One pass, left to right, so an escaped escape — `&amp;lt;` — correctly comes
/// back as the text `&lt;` rather than being unescaped twice.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let Some(end) = tail.find(';').filter(|end| *end <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let name = &tail[1..end];
        let character = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            // A numeric reference, which is how anything outside the five is
            // spelled — including the em dash in half these provider names.
            _ => name
                .strip_prefix('#')
                .and_then(|digits| match digits.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => digits.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        match character {
            Some(character) => {
                out.push(character);
                rest = &tail[end + 1..];
            }
            // Not an escape at all, just an ampersand followed by words.
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Finds `needle` only where it starts a property name, so `Rating` does not
/// match inside `xmp:RatingPercent` and `creator` does not match `CreatorTool`.
fn find_str_after_separator(text: &str, needle: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(offset) = text[from..].find(needle) {
        let at = from + offset;
        let preceding = text[..at].chars().next_back();
        if matches!(preceding, Some(':') | Some('<') | Some(' ') | None) {
            return Some(at);
        }
        from = at + needle.len();
    }
    None
}

fn first_quoted(text: &str) -> Option<String> {
    let start = text.find('"')? + 1;
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

fn first_element_body(text: &str) -> Option<String> {
    let open = text.find('>')? + 1;
    let rest = &text[open..];
    let end = rest.find('<')?;
    Some(rest[..end].to_owned())
}

// ----------------------------------------------------------------------- IPTC

/// The IPTC-IIM block, which always lives inside a Photoshop `8BIM` resource
/// with id 0x0404 — in a JPEG APP13 segment, a TIFF tag, or a PSD's resource
/// section. Searching for the resource directly covers all three.
fn find_iptc(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut from = 0;
    while let Some(offset) = find(&bytes[from..], b"8BIM") {
        let at = from + offset;
        from = at + 4;

        let Some(id) = be16(bytes, at + 4) else {
            continue;
        };
        if id != 0x0404 {
            continue;
        }
        // A Pascal-style name, padded to an even length, then a 4-byte size.
        let name_len = *bytes.get(at + 6)? as usize;
        let name_field = (name_len + 2) & !1;
        let size_at = at + 6 + name_field;
        let size = be32(bytes, size_at)? as usize;
        let start = size_at + 4;
        if size == 0 || size > 512 * 1024 {
            return None;
        }
        return bytes
            .get(start..start.checked_add(size)?)
            .map(<[u8]>::to_vec);
    }
    None
}

/// IIM datasets worth showing, as (record, dataset, label).
const IPTC_FIELDS: &[(u8, u8, &str)] = &[
    (2, 5, "Title"),
    (2, 105, "Headline"),
    (2, 120, "Caption"),
    (2, 80, "By-line"),
    (2, 85, "By-line title"),
    (2, 110, "Credit"),
    (2, 115, "Source"),
    (2, 116, "Copyright"),
    (2, 25, "Keywords"),
    (2, 55, "Date created"),
    (2, 90, "City"),
    (2, 101, "Country"),
];

/// Walks the IIM dataset list.
///
/// Each entry is `0x1C`, a record number, a dataset number, then a 16-bit
/// length — or, when the top bit of that length is set, a long-form size whose
/// own width the field gives. The long form is vanishingly rare in image files
/// but skipping it wrongly would desynchronise the whole walk, so it is handled
/// rather than assumed away.
pub fn parse_iptc(block: &[u8]) -> Pairs {
    let mut found: BTreeMap<(u8, u8), Vec<String>> = BTreeMap::new();
    let mut at = 0;

    while at + 5 <= block.len() {
        if block[at] != 0x1C {
            at += 1;
            continue;
        }
        let record = block[at + 1];
        let dataset = block[at + 2];
        let Some(raw_len) = be16(block, at + 3) else {
            break;
        };

        let (value_at, length) = if raw_len & 0x8000 == 0 {
            (at + 5, raw_len as usize)
        } else {
            let width = (raw_len & 0x7FFF) as usize;
            if width == 0 || width > 4 || at + 5 + width > block.len() {
                break;
            }
            let mut length = 0_usize;
            for byte in &block[at + 5..at + 5 + width] {
                length = (length << 8) | *byte as usize;
            }
            (at + 5 + width, length)
        };

        let Some(value) = block.get(value_at..value_at.saturating_add(length)) else {
            break;
        };
        at = value_at + length;

        // IIM is Latin-1 unless dataset 1:90 says otherwise; UTF-8 in practice.
        let text = clean(&String::from_utf8_lossy(value));
        if !text.is_empty() {
            found.entry((record, dataset)).or_default().push(text);
        }
    }

    IPTC_FIELDS
        .iter()
        .filter_map(|(record, dataset, label)| {
            // Keywords repeat: one dataset per keyword, joined for display.
            let values = found.get(&(*record, *dataset))?;
            Some(((*label).to_owned(), values.join(", ")))
        })
        .collect()
}

// ------------------------------------------------------------------------ ICC

/// The embedded colour profile. Its header ends with the `acsp` signature at a
/// fixed offset, which is what makes it findable without knowing the wrapper.
fn find_icc(bytes: &[u8]) -> Option<&[u8]> {
    let mut from = 0;
    while let Some(offset) = find(&bytes[from..], b"acsp") {
        let at = from + offset;
        from = at + 4;
        // `acsp` sits 36 bytes into the profile, and the first four bytes are
        // the profile's own size — which has to agree, or this is a chance
        // match on four bytes of pixel data.
        let start = at.checked_sub(36)?;
        let size = be32(bytes, start)? as usize;
        if !(132..=64 * 1024 * 1024).contains(&size) {
            continue;
        }
        // A truncated prefix is fine: the description is near the front.
        let end = (start + size).min(bytes.len());
        return bytes.get(start..end);
    }
    None
}

/// The profile's human-readable name, from its `desc` tag.
///
/// Two encodings, because ICC changed it: v2 stores `desc` as an ASCII string
/// with a length prefix, v4 as `mluc`, a table of language-tagged UTF-16BE
/// strings. Reading only one of them is how you end up printing the wrong tag's
/// contents, so both are handled.
pub fn profile_description(profile: &[u8]) -> Option<String> {
    let count = be32(profile, 128)? as usize;
    if count > 1024 {
        return None;
    }
    for index in 0..count {
        let entry = 132 + index * 12;
        let signature = profile.get(entry..entry + 4)?;
        if signature != b"desc" {
            continue;
        }
        let offset = be32(profile, entry + 4)? as usize;
        let size = be32(profile, entry + 8)? as usize;
        let tag = profile.get(offset..offset.checked_add(size)?)?;

        return match tag.get(..4)? {
            // v2: `desc`, a 4-byte length, then NUL-terminated ASCII.
            b"desc" => {
                let length = be32(tag, 8)? as usize;
                let text = tag.get(12..12 + length.min(tag.len().saturating_sub(12)))?;
                Some(clean(&String::from_utf8_lossy(text)))
            }
            // v4: `mluc`, a record count, a record size, then the records.
            b"mluc" => {
                let records = be32(tag, 8)? as usize;
                if records == 0 {
                    return None;
                }
                let length = be32(tag, 20)? as usize;
                let offset = be32(tag, 24)? as usize;
                let raw = tag.get(offset..offset.checked_add(length)?)?;
                let utf16: Vec<u16> = raw
                    .chunks_exact(2)
                    .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                    .collect();
                Some(clean(&String::from_utf16_lossy(&utf16)))
            }
            _ => None,
        }
        .filter(|text| !text.is_empty());
    }
    None
}

// ----------------------------------------------------------------- free text

/// PNG text chunks and the JPEG comment marker.
fn free_text(bytes: &[u8]) -> Pairs {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png_text(bytes);
    }
    if bytes.starts_with(b"\xFF\xD8") {
        return jpeg_comments(bytes);
    }
    Pairs::new()
}

/// Walks the PNG chunk list for `tEXt`, `iTXt` and `zTXt`.
///
/// The chunk list is walked properly rather than searched for, because the
/// four-letter type codes occur by chance inside compressed image data all the
/// time — a scan finds dozens of "chunks" in the middle of `IDAT`.
fn png_text(bytes: &[u8]) -> Pairs {
    let mut out = Pairs::new();
    let mut at = 8;

    while at + 8 <= bytes.len() {
        let Some(length) = be32(bytes, at) else { break };
        let length = length as usize;
        let Some(kind) = bytes.get(at + 4..at + 8) else {
            break;
        };
        let body_at = at + 8;
        let Some(body) = bytes.get(body_at..body_at.saturating_add(length)) else {
            break;
        };

        match kind {
            b"IEND" => break,
            // Compressed payloads are skipped rather than inflated: the key is
            // readable but the value is not, and half a row is not worth a
            // decompressor here.
            b"tEXt" | b"iTXt" => {
                if let Some((key, value)) = png_pair(body, kind == b"iTXt") {
                    // The XMP packet arrives as an iTXt chunk and is already
                    // handled properly above; repeating it raw helps no one.
                    if !key.starts_with("XML:com.adobe.xmp") {
                        out.push((key, value));
                    }
                }
            }
            _ => {}
        }
        at = body_at + length + 4; // 4 for the CRC.
    }
    out
}

/// `keyword\0text` for `tEXt`; `iTXt` inserts a compression flag, a compression
/// method and two language fields before the text.
fn png_pair(body: &[u8], international: bool) -> Option<(String, String)> {
    let split = body.iter().position(|byte| *byte == 0)?;
    let key = clean(&String::from_utf8_lossy(&body[..split]));
    let mut rest = body.get(split + 1..)?;

    if international {
        let compressed = *rest.first()?;
        if compressed != 0 {
            return None;
        }
        rest = rest.get(2..)?;
        // Language tag, then translated keyword, both NUL-terminated.
        for _ in 0..2 {
            let end = rest.iter().position(|byte| *byte == 0)?;
            rest = rest.get(end + 1..)?;
        }
    }

    let value = clean(&String::from_utf8_lossy(rest));
    (!key.is_empty() && !value.is_empty()).then_some((key, value))
}

/// The `COM` marker, walked through the segment list for the same reason the
/// PNG chunks are: `FF FE` appears inside entropy-coded scan data constantly.
fn jpeg_comments(bytes: &[u8]) -> Pairs {
    let mut out = Pairs::new();
    let mut at = 2;

    while at + 4 <= bytes.len() {
        if bytes[at] != 0xFF {
            break;
        }
        let marker = bytes[at + 1];
        // Start of scan: everything after this is compressed pixels.
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        if (0xD0..=0xD8).contains(&marker) {
            at += 2;
            continue;
        }
        let Some(length) = be16(bytes, at + 2) else {
            break;
        };
        let length = length as usize;
        if length < 2 {
            break;
        }
        if marker == 0xFE
            && let Some(body) = bytes.get(at + 4..at + 2 + length)
        {
            let text = clean(&String::from_utf8_lossy(body));
            if !text.is_empty() {
                out.push(("Comment".to_owned(), text));
            }
        }
        at += 2 + length;
    }
    out
}

// -------------------------------------------------------------------- helpers

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Trims, drops control characters and collapses runs of whitespace.
///
/// Metadata strings are padded with NULs, wrapped across lines by the writer,
/// and occasionally left with a stray tab. A panel row is one line, so this is
/// what "one line" means.
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for character in text.chars() {
        if character.is_control() || character.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(character);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xmp_reads_both_spellings() {
        let packet = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
            <rdf:RDF><rdf:Description
                xmp:CreatorTool="Adobe Photoshop 24.0"
                xmp:Rating="4"
                xmpMM:DocumentID="adobe:docid:photoshop:abc">
                <dc:title><rdf:Alt><rdf:li xml:lang="x-default">A map</rdf:li></rdf:Alt></dc:title>
            </rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let pairs = parse_xmp(packet);

        let get = |label: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == label)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("Created with"), Some("Adobe Photoshop 24.0"));
        assert_eq!(get("Rating"), Some("4"));
        assert_eq!(get("Document ID"), Some("adobe:docid:photoshop:abc"));
        assert_eq!(get("Title"), Some("A map"));
    }

    #[test]
    fn xmp_property_names_must_match_whole() {
        // `CreatorTool` contains `creator`; matching loosely would report the
        // application as the author of the photograph.
        let packet = br#"<x:xmpmeta><rdf:Description xmp:CreatorTool="Lightroom"/></x:xmpmeta>"#;
        let pairs = parse_xmp(packet);
        assert_eq!(
            pairs.iter().find(|(key, _)| key == "Author"),
            None,
            "CreatorTool must not be read as dc:creator: {pairs:?}",
        );
    }

    #[test]
    fn a_property_does_not_borrow_a_list_from_further_down() {
        // Straight from `heijdeman.jpg`: the properties are in element form,
        // and further down the packet sits an unrelated `1 2 3 0` array. The
        // wrapper search used to run past the end of the property and return
        // that array's first item, so the panel read `Created with: 1`.
        let packet = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF><rdf:Description>
            <xap:CreatorTool>Adobe Photoshop CS2 Windows</xap:CreatorTool>
            <xapMM:DocumentID>adobe:docid:photoshop:d8b623e0-cbd9-11db</xapMM:DocumentID>
            <crs:ToneCurve><rdf:Seq>
                <rdf:li>1</rdf:li><rdf:li>2</rdf:li><rdf:li>3</rdf:li><rdf:li>0</rdf:li>
            </rdf:Seq></crs:ToneCurve>
        </rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let pairs = parse_xmp(packet);
        let get = |label: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == label)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("Created with"), Some("Adobe Photoshop CS2 Windows"));
        assert_eq!(
            get("Document ID"),
            Some("adobe:docid:photoshop:d8b623e0-cbd9-11db")
        );
    }

    #[test]
    fn xmp_collects_the_edit_history() {
        let packet = br#"<x:xmpmeta><rdf:Seq>
            <rdf:li stEvt:softwareAgent="Adobe Photoshop 24.0"/>
            <rdf:li stEvt:softwareAgent="Adobe Photoshop 24.0"/>
            <rdf:li stEvt:softwareAgent="Affinity Photo 2"/>
        </rdf:Seq></x:xmpmeta>"#;
        let pairs = parse_xmp(packet);
        assert_eq!(
            pairs.iter().find(|(key, _)| key == "Edited with"),
            Some(&(
                "Edited with".to_owned(),
                "Adobe Photoshop 24.0 · Affinity Photo 2".to_owned()
            )),
            "repeats should collapse: {pairs:?}",
        );
    }

    fn iim(record: u8, dataset: u8, value: &str) -> Vec<u8> {
        let mut out = vec![0x1C, record, dataset];
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
        out
    }

    #[test]
    fn iptc_reads_datasets_and_joins_keywords() {
        let mut block = iim(2, 120, "A caption");
        block.extend(iim(2, 80, "M. Photographer"));
        block.extend(iim(2, 25, "maps"));
        block.extend(iim(2, 25, "singapore"));

        let pairs = parse_iptc(&block);
        assert!(pairs.contains(&("Caption".to_owned(), "A caption".to_owned())));
        assert!(pairs.contains(&("By-line".to_owned(), "M. Photographer".to_owned())));
        assert!(pairs.contains(&("Keywords".to_owned(), "maps, singapore".to_owned())));
    }

    #[test]
    fn iptc_survives_a_truncated_block() {
        // Claims 4000 bytes and provides three. Metadata is truncated in the
        // wild constantly; this must not panic or loop.
        let block = [0x1C, 2, 120, 0x0F, 0xA0, b'a', b'b', b'c'];
        assert_eq!(parse_iptc(&block), Pairs::new());
    }

    /// A minimal v2 profile whose only tag is a `desc`.
    fn icc_v2(name: &str) -> Vec<u8> {
        let tag_offset = 132 + 12;
        let tag = {
            let mut tag = b"desc".to_vec();
            tag.extend_from_slice(&[0; 4]);
            tag.extend_from_slice(&((name.len() + 1) as u32).to_be_bytes());
            tag.extend_from_slice(name.as_bytes());
            tag.push(0);
            tag
        };
        let mut profile = vec![0_u8; tag_offset];
        profile[36..40].copy_from_slice(b"acsp");
        profile[128..132].copy_from_slice(&1_u32.to_be_bytes());
        profile[132..136].copy_from_slice(b"desc");
        profile[136..140].copy_from_slice(&(tag_offset as u32).to_be_bytes());
        profile[140..144].copy_from_slice(&(tag.len() as u32).to_be_bytes());
        profile.extend_from_slice(&tag);
        let size = profile.len() as u32;
        profile[0..4].copy_from_slice(&size.to_be_bytes());
        profile
    }

    #[test]
    fn icc_v2_description() {
        assert_eq!(
            profile_description(&icc_v2("sRGB IEC61966-2.1")),
            Some("sRGB IEC61966-2.1".to_owned())
        );
    }

    #[test]
    fn icc_is_found_inside_a_larger_file() {
        // As it arrives: buried in a JPEG APP2 segment, not at offset zero.
        let mut file = vec![0xAB; 900];
        file.extend(icc_v2("Display P3"));
        file.extend(vec![0xCD; 400]);
        assert_eq!(
            find_icc(&file).and_then(profile_description),
            Some("Display P3".to_owned())
        );
    }

    #[test]
    fn png_text_ignores_matches_inside_pixel_data() {
        // `tEXt` as a literal in the compressed stream must not be mistaken for
        // a chunk header — walking the list is what makes that impossible.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let idat = b"junk tEXt Software\0Adobe ImageReady more junk";
        png.extend_from_slice(&(idat.len() as u32).to_be_bytes());
        png.extend_from_slice(b"IDAT");
        png.extend_from_slice(idat);
        png.extend_from_slice(&[0; 4]);

        let body = b"Software\0mapDownloader 0.1.0";
        png.extend_from_slice(&(body.len() as u32).to_be_bytes());
        png.extend_from_slice(b"tEXt");
        png.extend_from_slice(body);
        png.extend_from_slice(&[0; 4]);

        assert_eq!(
            png_text(&png),
            vec![("Software".to_owned(), "mapDownloader 0.1.0".to_owned())],
        );
    }

    #[test]
    fn jpeg_comment_is_read_from_the_segment_list() {
        let mut jpeg = vec![0xFF, 0xD8];
        let comment = b"ACD Systems Digital Imaging";
        jpeg.extend_from_slice(&[0xFF, 0xFE]);
        jpeg.extend_from_slice(&((comment.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(comment);
        jpeg.extend_from_slice(&[0xFF, 0xDA]);
        // Scan data containing the comment marker, which must not be read.
        jpeg.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x08, b'n', b'o', b'p', b'e']);

        assert_eq!(
            jpeg_comments(&jpeg),
            vec![(
                "Comment".to_owned(),
                "ACD Systems Digital Imaging".to_owned()
            )],
        );
    }

    #[test]
    fn cleaning_collapses_the_padding_writers_leave() {
        assert_eq!(
            clean("  Adobe\0\0  Photoshop \n CS  "),
            "Adobe Photoshop CS"
        );
        assert_eq!(clean("\0\0\0"), "");
    }

    #[test]
    fn escapes_come_back_as_the_characters_they_stand_for() {
        assert_eq!(
            unescape("Maps &amp; Building Plans"),
            "Maps & Building Plans"
        );
        assert_eq!(
            unescape("&lt;a&gt; &quot;b&quot; &apos;c&apos;"),
            "<a> \"b\" 'c'"
        );
        assert_eq!(unescape("Singapore &#8212; NAS"), "Singapore — NAS");
        assert_eq!(unescape("&#x2014;"), "—");
        // An escaped escape is text, and must not be unescaped twice.
        assert_eq!(unescape("&amp;lt;"), "&lt;");
        // A bare ampersand is left where it is rather than eating what follows.
        assert_eq!(unescape("Jardine & Matheson"), "Jardine & Matheson");
        assert_eq!(unescape("R&D; and more"), "R&D; and more");
    }

    #[test]
    fn a_converted_maps_keywords_come_back_as_a_list() {
        // What the TAGS group is built from: the tags mapSelector put in the
        // sidecar, written into `dc:subject` at conversion.
        let packet = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF><rdf:Description>
            <dc:subject><rdf:Bag>
              <rdf:li>full-colour</rdf:li><rdf:li>mukim</rdf:li>
              <rdf:li>photo-cd</rdf:li><rdf:li>postwar</rdf:li>
              <rdf:li>survey</rdf:li><rdf:li>text-dense</rdf:li>
            </rdf:Bag></dc:subject>
        </rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let pairs = parse_xmp(packet);
        let keywords = pairs
            .iter()
            .find(|(key, _)| key == "Keywords")
            .map(|(_, value)| value.as_str())
            .expect("keywords are read");
        let tags: Vec<&str> = keywords.split('·').map(str::trim).collect();
        assert_eq!(
            tags,
            vec![
                "full-colour",
                "mukim",
                "photo-cd",
                "postwar",
                "survey",
                "text-dense"
            ],
        );
    }

    #[test]
    fn every_keyword_is_shown_not_just_the_first() {
        let packet = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF><rdf:Description>
            <dc:subject><rdf:Bag>
              <rdf:li>SG</rdf:li><rdf:li>full island</rdf:li><rdf:li>aerial</rdf:li>
            </rdf:Bag></dc:subject>
        </rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let pairs = parse_xmp(packet);
        let keywords = pairs.iter().find(|(key, _)| key == "Keywords");
        assert_eq!(
            keywords.map(|(_, value)| value.as_str()),
            Some("SG · full island · aerial"),
        );
    }

    #[test]
    fn nothing_found_is_empty_rather_than_wrong() {
        assert!(from_bytes(&[0_u8; 4096]).is_empty());
    }
}

#[cfg(test)]
mod real_files {
    use super::*;

    /// Runs the parsers over a real folder and prints what they find, so the
    /// output can be checked against exiftool rather than against my own
    /// assumptions about what the files contain.
    ///
    /// `GEO_REF_MAPPER_META_DIR=~/Pictures cargo test --release meta_survey -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_META_DIR"]
    fn meta_survey() {
        let Ok(root) = std::env::var("GEO_REF_MAPPER_META_DIR") else {
            return;
        };
        let limit: usize = std::env::var("GEO_REF_MAPPER_META_N")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(400);

        let mut files = Vec::new();
        collect(std::path::Path::new(&root), &mut files, limit * 8);
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        let mut shown = 0;

        for path in files.iter().take(limit * 8) {
            let meta = from_path(path);
            if meta.is_empty() {
                continue;
            }
            *counts.entry("any").or_default() += 1;
            if !meta.xmp.is_empty() {
                *counts.entry("xmp").or_default() += 1;
            }
            if !meta.iptc.is_empty() {
                *counts.entry("iptc").or_default() += 1;
            }
            if meta.icc.is_some() {
                *counts.entry("icc").or_default() += 1;
            }
            if !meta.text.is_empty() {
                *counts.entry("text").or_default() += 1;
            }
            if shown < 14 {
                shown += 1;
                println!("\n{}", path.display());
                for (label, value) in meta.xmp.iter().chain(&meta.iptc).chain(&meta.text) {
                    let value: String = value.chars().take(78).collect();
                    println!("    {label:16} {value}");
                }
                if let Some(icc) = &meta.icc {
                    println!("    {:16} {icc}", "ICC");
                }
            }
        }
        println!("\n{} files scanned, counts: {counts:?}", files.len());
    }

    fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>, limit: usize) {
        if out.len() >= limit {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, out, limit);
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(crate::db::is_image_extension)
            {
                out.push(path);
            }
            if out.len() >= limit {
                return;
            }
        }
    }
}

#[cfg(test)]
mod one_file {
    /// `GEO_REF_MAPPER_META_FILE=/path/to.jpg cargo test --release meta_one -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_META_FILE"]
    fn meta_one() {
        let Ok(path) = std::env::var("GEO_REF_MAPPER_META_FILE") else {
            return;
        };
        let path = std::path::Path::new(&path);
        let meta = super::from_path(path);
        // `&v[..100]` panics mid-character on any value containing a
        // multi-byte character, which XMP history strings routinely do.
        let cut = |v: &str| v.chars().take(100).collect::<String>();
        println!("XMP:");
        for (k, v) in &meta.xmp {
            println!("   {k:16} {}", cut(v));
        }
        println!("IPTC:");
        for (k, v) in &meta.iptc {
            println!("   {k:16} {v}");
        }
        println!("ICC:  {:?}", meta.icc);
        println!("TEXT:");
        for (k, v) in &meta.text {
            println!("   {k:16} {}", cut(v));
        }
        if let Some(exif) = crate::exif::from_path(path) {
            println!("EXIF software:    {:?}", exif.software);
            println!("EXIF description: {:?}", exif.description);
            println!("EXIF artist:      {:?}", exif.artist);
            println!("EXIF copyright:   {:?}", exif.copyright);
        }
    }
}
