//! The History window's rows: every capture in the captures folder, newest first.

use std::path::{Path, PathBuf};

use crate::model::{ScrawlBundle, format_clock};

/// Apps the Apps column shows before it folds the rest into "+N".
const SHOWN_APPS: usize = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    pub dir: PathBuf,
    pub created_at_ms: u64,
    /// Local time, such as "Nov 13, 2:41 PM".
    pub date: String,
    /// `ScrawlBundle::apps`, in first-seen order.
    pub apps: Vec<String>,
    /// `M:SS`.
    pub length: String,
    /// The video file `scrawl.json` names is gone (`retention` cleaned it).
    pub cleaned: bool,
}

impl HistoryEntry {
    pub fn new(dir: &Path, bundle: &ScrawlBundle, now_ms: u64) -> Self {
        Self {
            dir: dir.to_path_buf(),
            created_at_ms: bundle.created_at_ms,
            date: local_date(bundle.created_at_ms, now_ms),
            apps: bundle.apps(),
            length: format_clock(bundle.duration_ms),
            cleaned: playable_video(dir, bundle).is_none(),
        }
    }

    /// Up to three apps joined by arrows, then "+N" for the rest.
    pub fn apps_label(&self) -> String {
        let mut label = self.apps[..self.apps.len().min(SHOWN_APPS)].join(" → ");
        if self.apps.len() > SHOWN_APPS {
            label.push_str(&format!(" +{}", self.apps.len() - SHOWN_APPS));
        }
        label
    }

    /// Every app, for the tooltip and the Review header.
    pub fn apps_tooltip(&self) -> String {
        self.apps.join(" → ")
    }
}

/// The bundle's video when it is still on disk: the Review window shows its Video tab only then.
pub fn playable_video(dir: &Path, bundle: &ScrawlBundle) -> Option<PathBuf> {
    Some(crate::handoff::bundle_video(dir, Some(bundle))).filter(|video| video.is_file())
}

/// The captures in `dir`, newest first. A bundle without a readable `scrawl.json` is skipped.
pub fn scan(dir: &Path, now_ms: u64) -> Vec<HistoryEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<HistoryEntry> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "scrawl") && path.is_dir())
        .filter_map(|path| {
            let bytes = std::fs::read(path.join("scrawl.json")).ok()?;
            let bundle: ScrawlBundle = serde_json::from_slice(&bytes).ok()?;
            Some(HistoryEntry::new(&path, &bundle, now_ms))
        })
        .collect();
    rows.sort_by(|a, b| {
        b.created_at_ms
            .cmp(&a.created_at_ms)
            .then_with(|| b.dir.cmp(&a.dir))
    });
    rows
}

/// A calendar moment, as `localtime_r` gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Moment {
    year: i32,
    /// `0..12`.
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
}

/// "Nov 13, 2:41 PM" in local time, with the year when it isn't this year.
pub fn local_date(epoch_ms: u64, now_ms: u64) -> String {
    match (moment(epoch_ms), moment(now_ms)) {
        (Some(at), Some(now)) => format_moment(at, now.year),
        _ => String::new(),
    }
}

fn format_moment(at: Moment, this_year: i32) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS[at.month as usize % 12];
    let hour12 = match at.hour % 12 {
        0 => 12,
        h => h,
    };
    let half = if at.hour < 12 { "AM" } else { "PM" };
    let year = if at.year == this_year {
        String::new()
    } else {
        format!(" {},", at.year)
    };
    format!("{month} {},{year} {hour12}:{:02} {half}", at.day, at.minute)
}

#[cfg(unix)]
fn moment(epoch_ms: u64) -> Option<Moment> {
    let secs = libc::time_t::try_from(epoch_ms / 1_000).ok()?;
    // SAFETY: `localtime_r` writes only into `tm`, which outlives the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let filled = unsafe { libc::localtime_r(&secs, &mut tm) };
    (!filled.is_null()).then(|| Moment {
        year: tm.tm_year + 1900,
        month: tm.tm_mon as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
    })
}

/// UTC on platforms without `localtime_r`.
#[cfg(not(unix))]
fn moment(epoch_ms: u64) -> Option<Moment> {
    let secs = epoch_ms / 1_000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    Some(Moment {
        year,
        month: month - 1,
        day,
        hour: (rem / 3_600) as u32,
        minute: (rem % 3_600 / 60) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder removed when the test ends.
    struct Root(PathBuf);

    impl Root {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_bundle(root: &Path, name: &str, created_at_ms: u64, app: &str, video: Option<&str>) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let json = serde_json::json!({
            "version": 1, "id": name, "createdAtMs": created_at_ms, "durationMs": 42_000,
            "target": {"mode": "window", "app": app, "displayId": 0,
                "frame": {"x": 0.0, "y": 0.0, "width": 10.0, "height": 10.0, "scale": 1.0}},
            "transcript": {"source": "test", "text": "", "segments": []},
            "strokes": [], "marks": [], "events": [], "keyframes": [], "contactSheet": "c.png",
            "video": video
        });
        std::fs::write(dir.join("scrawl.json"), json.to_string()).unwrap();
    }

    #[test]
    fn history_lists_readable_captures_newest_first_and_marks_cleaned_videos() {
        let root =
            std::env::temp_dir().join(format!("annotator-history-{}", crate::make_bundle_id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = Root(root);
        write_bundle(
            root.path(),
            "old.scrawl",
            1_000,
            "Xcode",
            Some("recording.mp4"),
        );
        write_bundle(
            root.path(),
            "new.scrawl",
            3_000,
            "Safari",
            Some("recording.mov"),
        );
        write_bundle(root.path(), "mid.scrawl", 2_000, "Figma", None);
        std::fs::write(root.path().join("new.scrawl/recording.mov"), b"movie").unwrap();
        std::fs::write(root.path().join("mid.scrawl/recording.mp4"), b"movie").unwrap();
        // Skipped: no manifest, a broken one, and something that isn't a bundle.
        std::fs::create_dir_all(root.path().join("empty.scrawl")).unwrap();
        std::fs::create_dir_all(root.path().join("broken.scrawl")).unwrap();
        std::fs::write(root.path().join("broken.scrawl/scrawl.json"), b"{").unwrap();
        write_bundle(root.path(), "notes", 4_000, "Notes", None);

        let rows = scan(root.path(), 3_000);
        let summary: Vec<(&str, &str, bool)> = rows
            .iter()
            .map(|r| (r.apps[0].as_str(), r.length.as_str(), r.cleaned))
            .collect();
        assert_eq!(
            summary,
            [
                ("Safari", "0:42", false),
                ("Figma", "0:42", false),
                ("Xcode", "0:42", true)
            ]
        );
        assert!(rows[0].dir.ends_with("new.scrawl"));

        // The Review window's Video tab follows the same file: gone once retention cleans it.
        let (dir, bundle) = crate::handoff::load_bundle(&rows[0].dir).unwrap();
        assert_eq!(
            playable_video(&dir, &bundle),
            Some(dir.join("recording.mov"))
        );
        std::fs::remove_file(dir.join("recording.mov")).unwrap();
        assert_eq!(playable_video(&dir, &bundle), None);
        assert!(scan(&root.path().join("missing"), 0).is_empty());
    }

    #[test]
    fn the_apps_column_shows_three_apps_and_folds_the_rest_into_plus_n() {
        let row = |apps: &[&str]| HistoryEntry {
            dir: PathBuf::new(),
            created_at_ms: 0,
            date: String::new(),
            apps: apps.iter().map(|a| a.to_string()).collect(),
            length: String::new(),
            cleaned: false,
        };
        let four = row(&["Safari", "T3 Code", "Finder", "Xcode"]);
        assert_eq!(four.apps_label(), "Safari → T3 Code → Finder +1");
        assert_eq!(four.apps_tooltip(), "Safari → T3 Code → Finder → Xcode");
        assert_eq!(row(&["Figma", "Safari"]).apps_label(), "Figma → Safari");
        assert_eq!(row(&["T3 Code"]).apps_label(), "T3 Code");
        assert_eq!(row(&[]).apps_label(), "");
        let six = row(&["A", "B", "C", "D", "E", "F"]);
        assert_eq!(six.apps_label(), "A → B → C +3");
    }

    #[test]
    fn dates_read_like_the_menu_bar_clock() {
        let at = |year, month, day, hour, minute| Moment {
            year,
            month,
            day,
            hour,
            minute,
        };
        assert_eq!(
            format_moment(at(2026, 10, 13, 14, 41), 2026),
            "Nov 13, 2:41 PM"
        );
        assert_eq!(format_moment(at(2026, 0, 2, 0, 5), 2026), "Jan 2, 12:05 AM");
        assert_eq!(
            format_moment(at(2026, 11, 31, 12, 0), 2026),
            "Dec 31, 12:00 PM"
        );
        assert_eq!(
            format_moment(at(2025, 9, 28, 15, 55), 2026),
            "Oct 28, 2025, 3:55 PM"
        );
        // A real timestamp gets a date whatever the time zone.
        assert!(local_date(1_794_610_038_922, 1_794_610_038_922).contains(", "));
    }
}
