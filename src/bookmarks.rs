//! Pinned folders, shown as a row of small squares above the file tree.
//!
//! Two characters is very little to identify a folder by, so which two matters
//! more than it sounds. Taking the first two of the name is the obvious choice
//! and the wrong one for a library organised by year: `1600`, `1604`, `1606`
//! and `1607` all become `16`. Taking the *last* two digits of a numeric name
//! gives `00`, `04`, `06`, `07` instead, which is what the eye needs.

use std::path::{Path, PathBuf};

/// How many will fit across the panel without becoming a wall of squares.
pub const MAX: usize = 24;

/// The two characters shown on a bookmark button.
///
/// A name with words in it is identified by its first word — `2015-01 ICELAND`
/// reads better as `IC` than as `20`, since every folder that year starts the
/// same way. A purely numeric name falls back to its last two digits, which is
/// the part that actually varies.
pub fn label(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return "··".to_owned();
    }

    // The first run of letters anywhere in the name.
    let word: String = name
        .split(|c: char| !c.is_alphabetic())
        .find(|part| !part.is_empty())
        .unwrap_or_default()
        .chars()
        .take(2)
        .collect();
    if word.chars().count() == 2 {
        return word.to_uppercase();
    }

    // No word worth using, so fall back to digits — the trailing ones, which
    // are what distinguishes 1600 from 1607.
    let digits: Vec<char> = name.chars().filter(char::is_ascii_digit).collect();
    if digits.len() >= 2 {
        return digits[digits.len() - 2..].iter().collect();
    }

    // A single letter or digit, padded so every button is the same width.
    let mut short: String = name.chars().take(2).collect::<String>().to_uppercase();
    while short.chars().count() < 2 {
        short.push('·');
    }
    short
}

/// Labels for a whole row, none of them repeated.
///
/// Two characters collide easily on a real library — `MAPS` and `MAPS-NEW` both
/// reduce to `MA` — and two identical squares are worse than a slightly odd
/// one. Later entries fall back to other pairs drawn from the same name until
/// something unused is found, so the first folder pinned keeps the obvious
/// label and the clash is pushed onto the newcomer.
pub fn labels(names: &[String]) -> Vec<String> {
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(names.len());

    for name in names {
        let chosen = candidates(name)
            .into_iter()
            .find(|candidate| !used.contains(candidate))
            // Everything collided, which needs 30-odd similar names; take the
            // base label and accept the repeat rather than inventing noise.
            .unwrap_or_else(|| label(name));
        used.insert(chosen.clone());
        out.push(chosen);
    }
    out
}

/// Two-character labels for one name, best first.
fn candidates(name: &str) -> Vec<String> {
    let mut out = vec![label(name)];

    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect();

    // First letters of the first two words: MAPS-NEW becomes MN.
    if let (Some(first), Some(second)) = (words.first(), words.get(1))
        && let (Some(a), Some(b)) = (first.chars().next(), second.chars().next())
    {
        out.push(format!("{a}{b}").to_uppercase());
    }

    // First and last character of the first word: MAPS becomes MS.
    if let Some(first) = words.first()
        && let (Some(a), Some(z)) = (first.chars().next(), first.chars().last())
        && first.chars().count() > 1
    {
        out.push(format!("{a}{z}").to_uppercase());
    }

    // Then simply walk the name's own characters in pairs.
    let letters: Vec<char> = name.chars().filter(|c| c.is_alphanumeric()).collect();
    for window in letters.windows(2) {
        out.push(window.iter().collect::<String>().to_uppercase());
    }

    out.dedup();
    out
}

/// One pinned entry: a single folder, or a set of them pinned together.
///
/// A set exists so a multi-folder view can be got back in one click — mark
/// three folders, pin them, and the Mosaic shows all three again later.
#[derive(Clone, PartialEq, Debug)]
pub struct Bookmark {
    pub folders: Vec<PathBuf>,
}

impl Bookmark {
    pub fn is_group(&self) -> bool {
        self.folders.len() > 1
    }

    /// The folder a single bookmark points at.
    pub fn only(&self) -> Option<&Path> {
        match self.folders.as_slice() {
            [one] => Some(one),
            _ => None,
        }
    }

    /// What the square says.
    ///
    /// A set of folders that all sit under one parent is labelled from that
    /// parent — pinning every year under `MAPS-NEW` gives `MA`, which says
    /// where you are going. Only when they come from different places does it
    /// fall back to the count, since no two characters can stand for three
    /// unrelated names.
    pub fn label(&self) -> String {
        if let Some(path) = self.only() {
            return label(&display_name(path));
        }
        match self.common_parent() {
            Some(parent) => label(&display_name(&parent)),
            None => self.folders.len().to_string(),
        }
    }

    /// The folder every member sits directly under, if there is one.
    fn common_parent(&self) -> Option<PathBuf> {
        let mut parents = self.folders.iter().map(|folder| folder.parent());
        let first = parents.next().flatten()?;
        parents
            .all(|parent| parent == Some(first))
            .then(|| first.to_path_buf())
            .filter(|parent| parent.file_name().is_some())
    }

    /// Every folder, one per line, for the button's tooltip.
    pub fn tooltip(&self) -> String {
        match self.only() {
            Some(path) => path.display().to_string(),
            None => self
                .folders
                .iter()
                .map(|folder| folder.display().to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// The pinned entries, in the order they were added.
#[derive(Clone, Debug, Default)]
pub struct Bookmarks {
    entries: Vec<Bookmark>,
}

impl Bookmarks {
    pub fn entries(&self) -> &[Bookmark] {
        &self.entries
    }

    /// True when exactly this set is already pinned, so the `+` can go dim
    /// rather than making a duplicate.
    pub fn contains_set(&self, folders: &[PathBuf]) -> bool {
        self.entries.iter().any(|entry| entry.folders == folders)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Pins a set of folders as one entry. Returns false when it is already
    /// pinned or the row is full, so the caller can leave the settings alone.
    pub fn add_set(&mut self, folders: Vec<PathBuf>) -> bool {
        if folders.is_empty() || self.contains_set(&folders) || self.entries.len() >= MAX {
            return false;
        }
        self.entries.push(Bookmark { folders });
        true
    }

    /// Unpins the entry that is exactly this set.
    pub fn remove_set(&mut self, folders: &[PathBuf]) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.folders != folders);
        self.entries.len() != before
    }

    /// Serialised for the settings table: one entry per line, and within a
    /// line one tab between folders.
    ///
    /// Newlines cannot appear in a path on any platform we run on. A tab
    /// legally can, but a folder named with one is rare enough that the cost
    /// of getting it wrong — one bookmark reading as two — is smaller than the
    /// cost of a format nobody can read.
    pub fn encode(&self) -> String {
        self.entries
            .iter()
            .map(|entry| {
                entry
                    .folders
                    .iter()
                    .map(|folder| folder.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Reads back what [`Self::encode`] wrote, dropping anything unusable so a
    /// stale entry cannot break startup. Rows written before groups existed
    /// have no tabs in them and read as single folders, unchanged.
    pub fn decode(value: &str) -> Self {
        let entries = value
            .lines()
            .filter_map(|line| {
                let folders: Vec<PathBuf> = line
                    .split('\t')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(PathBuf::from)
                    .collect();
                (!folders.is_empty()).then_some(Bookmark { folders })
            })
            .take(MAX)
            .collect();
        Self { entries }
    }

    /// Drops entries whose folders are all gone. A set that has lost only some
    /// of its folders keeps the ones that are still there.
    ///
    /// Deliberately not called at startup: an external drive that happens to be
    /// unplugged should not silently lose its pins.
    pub fn forget_missing(&mut self) {
        for entry in &mut self.entries {
            entry.folders.retain(|folder| folder.exists());
        }
        self.entries.retain(|entry| !entry.folders.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_survives_the_round_trip_and_old_rows_still_read() {
        let mut bookmarks = Bookmarks::default();
        assert!(bookmarks.add_set(vec![PathBuf::from("/photos/one")]));
        assert!(bookmarks.add_set(vec![
            PathBuf::from("/photos/a"),
            PathBuf::from("/photos/b"),
            PathBuf::from("/photos/c"),
        ]));
        assert!(
            !bookmarks.add_set(vec![
                PathBuf::from("/photos/a"),
                PathBuf::from("/photos/b"),
                PathBuf::from("/photos/c")
            ]),
            "the same set must not pin twice",
        );

        let restored = Bookmarks::decode(&bookmarks.encode());
        assert_eq!(restored.entries().len(), 2);
        assert_eq!(restored.entries()[0].only(), Some(Path::new("/photos/one")));
        assert_eq!(restored.entries()[1].folders.len(), 3);
        assert!(restored.entries()[1].is_group());
        // Those three share `/photos`, so the square borrows its name. What
        // this test is about is the round trip; labelling has its own.
        assert_eq!(restored.entries()[1].label(), "PH");

        // Rows written before groups existed have no tabs and must still read.
        let old = Bookmarks::decode("/photos/x\n/photos/y");
        assert_eq!(old.entries().len(), 2);
        assert!(old.entries().iter().all(|entry| entry.only().is_some()));
    }

    #[test]
    fn a_group_under_one_parent_is_named_after_it() {
        // Pinning every year folder under MAPS-NEW should say where it goes,
        // not just how many there are.
        let mut bookmarks = Bookmarks::default();
        bookmarks.add_set(vec![
            PathBuf::from("/photos/MAPS-NEW/1930"),
            PathBuf::from("/photos/MAPS-NEW/1931"),
            PathBuf::from("/photos/MAPS-NEW/1932"),
        ]);
        assert_eq!(bookmarks.entries()[0].label(), "MA");

        // Folders from different places have no shared name to borrow, so the
        // count is all that is left to say.
        let mut mixed = Bookmarks::default();
        mixed.add_set(vec![
            PathBuf::from("/photos/MAPS-NEW/1930"),
            PathBuf::from("/photos/Pince/scans"),
        ]);
        assert_eq!(mixed.entries()[0].label(), "2");
    }

    #[test]
    fn a_group_keeps_the_folders_that_are_still_there() {
        let mut bookmarks = Bookmarks::default();
        let here = std::env::temp_dir();
        bookmarks.add_set(vec![here.clone(), PathBuf::from("/no/such/folder/at/all")]);
        bookmarks.forget_missing();
        assert_eq!(
            bookmarks.entries()[0].folders,
            vec![here],
            "losing one folder must not lose the whole pin",
        );
    }

    /// The case that decided the rule: morgan's map folders are named by year.
    #[test]
    fn numeric_folders_are_told_apart_by_their_last_digits() {
        assert_eq!(label("1600"), "00");
        assert_eq!(label("1604"), "04");
        assert_eq!(label("1606"), "06");
        assert_eq!(label("1607"), "07");
        assert_eq!(label("1628"), "28");
        assert_eq!(label("1935"), "35");

        // Which is the whole point — taking the front would collide.
        let names = [
            "1600", "1604", "1606", "1607", "1628", "1630", "1631", "1633",
        ];
        let labels: std::collections::HashSet<String> = names.iter().map(|n| label(n)).collect();
        assert_eq!(labels.len(), names.len(), "every year should be distinct");
    }

    #[test]
    fn names_with_words_use_the_first_word() {
        assert_eq!(label("MAPS"), "MA");
        assert_eq!(label("NUS libmaps"), "NU");
        assert_eq!(label("bk-pano"), "BK");
        // A leading date is skipped in favour of the part that means something.
        assert_eq!(label("2015-01 2015 ICELAND"), "IC");
        assert_eq!(label("2010-05 handphone"), "HA");
    }

    #[test]
    fn awkward_names_still_produce_two_characters() {
        for name in ["", " ", "7", "x", "-", "日本語", "1"] {
            let label = label(name);
            assert_eq!(
                label.chars().count(),
                2,
                "{name:?} produced {label:?}, which would not fit the button"
            );
        }
    }

    /// The clash that showed up the moment real folders were pinned.
    #[test]
    fn folders_that_would_share_a_label_are_disambiguated() {
        let names: Vec<String> = ["MAPS", "MAPS-NEW", "Pictures", "Photos"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let labels = labels(&names);

        assert_eq!(labels[0], "MA", "the first pinned keeps the obvious label");
        assert_ne!(labels[1], "MA", "the second must not repeat it");
        assert_ne!(labels[2], labels[3], "Pictures and Photos both start Ph/Pi");

        let unique: std::collections::HashSet<&String> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?} contains a repeat");
    }

    #[test]
    fn a_whole_year_of_folders_stays_unique() {
        let names: Vec<String> = (1600..1640).map(|year| year.to_string()).collect();
        let labels = labels(&names);
        let unique: std::collections::HashSet<&String> = labels.iter().collect();
        assert_eq!(
            unique.len(),
            labels.len(),
            "forty year folders should all be distinguishable"
        );
    }

    #[test]
    fn every_disambiguated_label_is_still_two_characters() {
        let names: Vec<String> = ["MAPS", "MAPS-NEW", "M", "1", "", "日本", "a-b-c"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        for label in labels(&names) {
            assert_eq!(
                label.chars().count(),
                2,
                "{label:?} would not fit the button"
            );
        }
    }

    #[test]
    fn adding_is_idempotent_and_bounded() {
        let mut bookmarks = Bookmarks::default();
        assert!(bookmarks.is_empty());

        assert!(bookmarks.add_set(vec![PathBuf::from("/photos/a")]));
        assert!(
            !bookmarks.add_set(vec![PathBuf::from("/photos/a")]),
            "already pinned"
        );
        assert_eq!(bookmarks.entries().len(), 1);
        assert!(bookmarks.contains_set(&[PathBuf::from("/photos/a")]));

        for index in 0..MAX * 2 {
            bookmarks.add_set(vec![PathBuf::from(&format!("/photos/{index}"))]);
        }
        assert_eq!(
            bookmarks.entries().len(),
            MAX,
            "the row has to stay bounded"
        );
    }

    #[test]
    fn removing_reports_whether_anything_changed() {
        let mut bookmarks = Bookmarks::default();
        bookmarks.add_set(vec![PathBuf::from("/photos/a")]);

        assert!(bookmarks.remove_set(&[PathBuf::from("/photos/a")]));
        assert!(
            !bookmarks.remove_set(&[PathBuf::from("/photos/a")]),
            "already gone"
        );
        assert!(bookmarks.is_empty());
    }

    #[test]
    fn the_order_they_were_added_is_kept() {
        let mut bookmarks = Bookmarks::default();
        for name in ["c", "a", "b"] {
            bookmarks.add_set(vec![PathBuf::from(&format!("/photos/{name}"))]);
        }
        let names: Vec<String> = bookmarks
            .entries()
            .iter()
            .filter_map(|entry| entry.only())
            .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["c", "a", "b"], "not re-sorted behind the user");
    }

    #[test]
    fn encoding_round_trips_including_awkward_paths() {
        let mut bookmarks = Bookmarks::default();
        // A comma and spaces, which is why the separator is a newline.
        bookmarks.add_set(vec![PathBuf::from("/photos/2015-01, Iceland trip")]);
        bookmarks.add_set(vec![PathBuf::from("/media/T5 EVO/Library/Events")]);

        let decoded = Bookmarks::decode(&bookmarks.encode());
        assert_eq!(decoded.entries(), bookmarks.entries());
    }

    #[test]
    fn decoding_survives_a_stale_or_empty_setting() {
        assert!(Bookmarks::decode("").is_empty());
        assert!(Bookmarks::decode("\n\n  \n").is_empty());

        // More entries than the cap are truncated rather than accepted.
        let many = (0..MAX * 2)
            .map(|index| format!("/photos/{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(Bookmarks::decode(&many).entries().len(), MAX);
    }

    #[test]
    fn missing_folders_are_only_dropped_when_asked() {
        let mut bookmarks = Bookmarks::default();
        bookmarks.add_set(vec![PathBuf::from("/definitely/not/here")]);
        // Present until explicitly pruned — an unplugged drive keeps its pins.
        assert_eq!(bookmarks.entries().len(), 1);

        bookmarks.forget_missing();
        assert!(bookmarks.is_empty());
    }
}
