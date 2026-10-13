//! What this build is, and when it was made.
//!
//! The crate version alone does not say which build you are running: several
//! days of work can happen at one version number, and "0.6" on the title bar is
//! the same string before and after. The build time is what actually
//! distinguishes two binaries, so it is stamped in at compile time by
//! `build.rs` and carried here.
//!
//! It is also what [`crate::install`] compares: the copy in `~/.local/bin` was
//! put there at a known moment, and a build made after that moment is one the
//! installed copy does not have.

/// When this binary was built, in seconds since the Unix epoch.
///
/// Zero if the build machine's clock was before 1970, which is reported as
/// "unknown" rather than as 1970.
pub fn build_unix() -> u64 {
    env!("GEO_REF_MAPPER_BUILD_UNIX").parse().unwrap_or(0)
}

/// `2026-08-09 12:34`, in UTC.
///
/// UTC rather than local time: it is compared against a file's timestamp and
/// read across a reinstall, and a machine that changes zone — or crosses a
/// daylight-saving boundary — would otherwise make one build look older than
/// another it was plainly newer than.
pub fn build_stamp() -> String {
    match build_unix() {
        0 => "unknown".to_owned(),
        seconds => stamp(seconds),
    }
}

/// `YYYY-MM-DD HH:MM` for any Unix second, in UTC.
///
/// Used for file timestamps as well as for this build's own, so the two read
/// the same way when they are shown next to each other and compared.
pub fn stamp_of(seconds: u64) -> String {
    stamp(seconds)
}

/// `YYYY-MM-DD HH:MM` for a Unix second.
fn stamp(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rest / 3600,
        (rest % 3600) / 60,
    )
}

/// Days since 1970-01-01 to a calendar date.
///
/// Howard Hinnant's `civil_from_days`, which is the standard way to do this
/// without a date library. It shifts the epoch to March 1st so that the leap
/// day lands at the end of the year and the month-length pattern becomes
/// regular enough to invert arithmetically — which is why the constants look
/// arbitrary and why this is transcribed rather than invented.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    // March is 0 in the shifted year, so months 0..=9 are March..December and
    // 10..=11 are the January and February that belong to the *next* year.
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (year + i64::from(month <= 2), month, day)
}

/// `0.1` rather than the full `0.1.0`: the patch digit has never been what
/// distinguishes two builds here, and the date does.
fn number() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let mut parts = version.split('.');
    match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) => format!("{major}.{minor}"),
        _ => version.to_owned(),
    }
}

/// `0.6 (release) · 2026-08-09 12:34` — the whole of it, to the minute.
///
/// For the Config page and the install dialog, where two builds made the same
/// day have to be told apart and there is room to do it.
pub fn full_version() -> String {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    format!("{} ({profile})  ·  {}", number(), build_stamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants_format_correctly() {
        assert_eq!(stamp(0), "1970-01-01 00:00");
        assert_eq!(stamp(86_399), "1970-01-01 23:59");
        assert_eq!(stamp(86_400), "1970-01-02 00:00");
        // 2000-03-01, the day after the leap day of a century that *is* a leap
        // year — the case the 100/400 rules disagree about.
        assert_eq!(stamp(951_868_800), "2000-03-01 00:00");
        assert_eq!(stamp(951_782_400), "2000-02-29 00:00");
        // 2100 is not a leap year, so this is the 1st rather than the 29th.
        assert_eq!(stamp(4_107_542_400), "2100-03-01 00:00");
        // A date in the range this will actually be used in.
        assert_eq!(stamp(1_786_233_600), "2026-08-09 00:00");
        assert_eq!(stamp(1_786_233_600 + 45_296), "2026-08-09 12:34");
    }

    #[test]
    fn every_day_of_a_leap_year_round_trips() {
        // Walks 2024 day by day, checking the date only ever moves forward by
        // one and the month lengths come out right. A transcribed algorithm is
        // exactly the kind that is subtly wrong in one month.
        let lengths = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let start = 19_723_i64; // 2024-01-01
        let mut expected = (2024_i64, 1_u32, 1_u32);
        for offset in 0..366 {
            assert_eq!(civil_from_days(start + offset), expected, "day {offset}");
            let (year, month, day) = expected;
            expected = if day < lengths[month as usize - 1] {
                (year, month, day + 1)
            } else if month < 12 {
                (year, month + 1, 1)
            } else {
                (year + 1, 1, 1)
            };
        }
        assert_eq!(expected, (2025, 1, 1), "366 days lands on new year");
    }

    #[test]
    fn the_build_stamp_is_this_build() {
        // Not a fixed value — it changes every build, which is the point. What
        // can be asserted is that the stamp exists and is not the epoch, which
        // is what a missing or unparsed env var would give.
        assert_ne!(build_unix(), 0, "build.rs did not set the stamp");
        let stamp = build_stamp();
        assert_eq!(stamp.len(), "2026-08-09 12:34".len(), "{stamp}");
        assert!(full_version().contains(&stamp), "{}", full_version());
        // The date is in there, and so is the time it is told apart by.
        let date = stamp.split(' ').next().expect("a date");
        assert!(full_version().contains(date), "{}", full_version());
    }

    /// The title bar shares one row with the page pill, which is the only way
    /// between pages — so anything that grows on the left is spending the
    /// program's navigation. Measured with real font metrics, at the narrowest
    /// the bar gets in practice, because that is the failure being guarded
    /// against: the pill did vanish, on this machine, and no test knew.
    #[test]
    fn the_title_bar_leaves_room_for_the_page_pill() {
        let ctx = eframe::egui::Context::default();
        let _ = ctx.run_ui(Default::default(), |ui| {
            let ctx = ui.ctx();
            let width = |text: String, font| {
                ctx.fonts_mut(|fonts| fonts.layout_no_wrap(text, font, crate::theme::TEXT))
                    .size()
                    .x
            };

            // The identity cluster as it is actually drawn: margin, mark, gap,
            // name, the debug marker when there is one, and the widest the
            // renderer badge gets — logo plus the VRAM readout. No version;
            // that is what taking it off the bar bought.
            let name = width(
                crate::app::APP_NAME.to_owned(),
                crate::theme::heading_font(),
            );
            let marker = if cfg!(debug_assertions) {
                7.0 + width("debug".to_owned(), crate::theme::small_caps_font())
            } else {
                0.0
            };
            let badge = 26.0 + width("free: 10.7 GB".to_owned(), crate::theme::small_caps_font());
            let cluster = 12.0 + 18.0 + 2.0 + name + marker + 8.0 + badge;

            // The narrowest bar this can meet: a 3072 px screen whose desktop
            // is already at 2×, with the interface scale at its ceiling on top
            // of that. Points, not pixels — which is the whole trap, and why
            // 200 % on a HiDPI display left 768 pt to divide up.
            let bar = 3072.0 / (2.0 * crate::app::MAX_UI_SCALE);
            // What the panel toggles and the window buttons take, measured
            // from a running window rather than guessed.
            let controls = 255.0_f32;

            // The pill takes the whole span between the clusters now, and only
            // slides off centre when it has to — so the span is what has to
            // clear the floor.
            let free = (bar - controls - 12.0) - (cluster + 12.0);
            let floor = crate::pill::min_width(crate::app::PAGE_LABELS);
            assert!(
                free >= floor,
                "a {bar:.0} pt bar with a {cluster:.0} pt cluster leaves {free:.0} pt, \
                 under the pill's {floor:.0} pt floor",
            );
        });
    }
}
