//! App shots: the state of the app under each keyframe, for an agent that wants more than the
//! picture. While a Mac capture records with app state on (`handoff::app_state`), one background
//! walker reads the captured window's Accessibility tree at the start, when the top window or
//! its title changes, 300 ms after a click or a mark, and when the picture changes. Each shot
//! holds the app, the window title, the URL without its query, and a capped text outline of the
//! window (`button "Save"`). What may be read and kept is `crate::privacy`'s to say.
//!
//! Shots are written to `appshots/.spool/` in the bundle folder as they are taken. When the
//! capture finishes they become `appshots/shot-NN.json` and `appshots/timeline.json`, and each
//! keyframe gets the shot of its own window nearest its time (`match_keyframes`).
//! `scrawl appshot <id> --frame N` prints one.
//!
//! macOS only: other systems take nothing.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::model::{CaptureMode, Keyframe, TargetFrame};
use crate::privacy::Tree;
use crate::windows::WindowSnapshot;

/// The folder of a bundle that holds its shots.
pub const DIR: &str = "appshots";
/// Inside `DIR` while the capture records.
pub const SPOOL: &str = ".spool";
pub const TIMELINE: &str = "timeline.json";

/// A click or a mark changes the window; the shot waits this long for it to settle.
pub const SETTLE: Duration = Duration::from_millis(300);
/// How long finishing a capture waits for the walk in flight.
const FINISH_WAIT: Duration = Duration::from_millis(400);
/// A picture change asks for a shot at most this often.
const SCENE_GAP: Duration = Duration::from_secs(2);
/// A capture's shots stop growing past this.
const SPOOL_CAP: u64 = 32 * 1024 * 1024;
/// A keyframe takes a shot at most this far from it.
const MATCH_MS: u64 = 3_000;

/// How far one walk goes.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub depth: usize,
    pub nodes: usize,
    pub bytes: usize,
    pub deadline: Duration,
}

pub const LIMITS: Limits = Limits {
    depth: 12,
    nodes: 1_500,
    bytes: 64 * 1024,
    deadline: Duration::from_millis(1_500),
};

/// A window's text outline.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outline {
    /// One element per line, indented two spaces per level.
    pub lines: Vec<String>,
    pub nodes: usize,
    /// The cap that cut the walk (`depth`, `nodes`, `bytes`) or `timeout`.
    pub truncated: Option<&'static str>,
    /// The `AXURL` of each web area met, in order.
    pub urls: Vec<String>,
}

/// `AXStaticText` → `static text`.
pub fn role_name(role: &str) -> String {
    let mut name = String::new();
    for c in role.strip_prefix("AX").unwrap_or(role).chars() {
        if c.is_uppercase() && !name.is_empty() {
            name.push(' ');
        }
        name.extend(c.to_lowercase());
    }
    if name.is_empty() {
        "element".into()
    } else {
        name
    }
}

/// `button "Save" #save`: one element as a line of the outline.
fn line(node: &crate::privacy::Node, depth: usize) -> String {
    let role = if node.subrole == "AXSecureTextField" {
        "secure text field".to_owned()
    } else {
        role_name(&node.role)
    };
    let mut out = format!("{}{role}", "  ".repeat(depth));
    let label = node
        .value
        .as_ref()
        .or(node.title.as_ref())
        .or(node.description.as_ref());
    if let Some(label) = label {
        out.push_str(&format!(" \"{label}\""));
    }
    if let Some(id) = &node.identifier {
        out.push_str(&format!(" #{id}"));
    }
    out
}

/// Walks the tree under `root` in document order. It reads each element's role and subrole
/// first, then its names, and its value only where `crate::privacy` allows (`privacy::read`);
/// nothing under an input is read. `stop` is checked before every read: when it fires the
/// outline keeps what it has and says `timeout`.
pub fn walk<T: Tree>(
    tree: &T,
    root: &T::Element,
    limits: &Limits,
    stop: &dyn Fn() -> bool,
) -> Outline {
    let mut out = Outline::default();
    let mut bytes = 0;
    let mut visited = HashSet::new();
    let mut stack = vec![(root.clone(), 0_usize)];
    while let Some((element, depth)) = stack.pop() {
        if !visited.insert(tree.key(&element)) {
            continue;
        }
        if out.nodes >= limits.nodes {
            out.truncated = Some("nodes");
            break;
        }
        if stop() {
            out.truncated = Some("timeout");
            break;
        }
        let role = tree.string(&element, "AXRole").unwrap_or_default();
        if stop() {
            out.truncated = Some("timeout");
            break;
        }
        let subrole = tree.string(&element, "AXSubrole").unwrap_or_default();
        let Some((node, descend)) = crate::privacy::read(tree, &element, &role, &subrole, stop)
        else {
            out.truncated = Some("timeout");
            break;
        };
        let text = line(&node, depth);
        if bytes + text.len() + 1 > limits.bytes {
            out.truncated = Some("bytes");
            break;
        }
        bytes += text.len() + 1;
        out.lines.push(text);
        out.nodes += 1;
        if role == "AXWebArea" {
            if stop() {
                out.truncated = Some("timeout");
                break;
            }
            out.urls.extend(tree.url(&element, "AXURL"));
        }
        if !descend {
            continue;
        }
        if stop() {
            out.truncated = Some("timeout");
            break;
        }
        let children = tree.children(&element);
        if depth >= limits.depth {
            if !children.is_empty() {
                out.truncated = Some("depth");
            }
            continue;
        }
        stack.extend(children.into_iter().rev().map(|child| (child, depth + 1)));
    }
    out
}

/// The URL a window shows: the web area's `AXURL` that is the window's `AXDocument`, else
/// `AXDocument`, else the first web area's URL (a browser that names no document).
pub fn window_url(document: Option<&str>, web_areas: &[String]) -> Option<String> {
    let bare = |url: &str| url.split('#').next().unwrap_or(url).to_owned();
    document
        .and_then(|document| web_areas.iter().find(|url| bare(url) == bare(document)))
        .map(String::as_str)
        .or(document)
        .or(web_areas.first().map(String::as_str))
        .map(str::to_owned)
}

/// One state of one window, as stored in `appshots/shot-NN.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shot {
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<u32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Without its query and fragment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub nodes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
    /// Why there is no tree: `sensitive app` or `browser page`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tree: Vec<String>,
}

impl Shot {
    /// The same for the same app, bundle id, window, title, URL and tree: the spool keeps one
    /// file per key. Only compared within one capture.
    fn key(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (
            &self.app,
            &self.bundle_id,
            self.window_id,
            &self.title,
            &self.url,
            &self.withheld,
            &self.tree,
        )
            .hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

/// A shot from what was read of a window. The policy's last word: a password manager keeps
/// only its app, a browser's own page keeps no tree, and the title and URL lose any query.
pub fn shot_of(
    app: &str,
    bundle_id: Option<&str>,
    window_id: Option<u32>,
    title: &str,
    document: Option<&str>,
    outline: Outline,
) -> Shot {
    let mut shot = Shot {
        app: crate::privacy::text(app).unwrap_or_default(),
        bundle_id: bundle_id.map(str::to_owned),
        window_id,
        ..Shot::default()
    };
    if crate::privacy::is_sensitive_app(bundle_id) {
        shot.withheld = Some("sensitive app".into());
        return shot;
    }
    shot.title = crate::privacy::window_title(bundle_id, title);
    let url = window_url(document, &outline.urls);
    shot.url = url.as_deref().and_then(crate::privacy::url);
    let internal = document
        .into_iter()
        .chain(outline.urls.iter().map(String::as_str))
        .any(crate::privacy::is_internal_url);
    if internal {
        shot.withheld = Some("browser page".into());
        return shot;
    }
    shot.nodes = outline.nodes;
    shot.truncated = outline.truncated.map(str::to_owned);
    shot.tree = outline.lines;
    shot
}

/// One time a shot was seen, as stored in `appshots/timeline.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    /// On the capture clock (ms), like a keyframe's `t`.
    pub t: u64,
    /// The shot's file in `appshots/`.
    pub shot: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<u32>,
    pub pid: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    pub app: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// The window's `[x, y, width, height]` in points from the capture's top-left corner.
    pub frame: [i32; 4],
    /// How long reading the window took.
    pub took_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timeline {
    pub observations: Vec<Observation>,
    /// When the picture changed enough to ask for a shot (ms).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scenes: Vec<u64>,
    /// The capture's shots reached `SPOOL_CAP`, so later states are missing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub capped: bool,
}

/// A capture's shots on their way into a bundle: the files in `dir` and when each was seen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shots {
    pub dir: PathBuf,
    pub timeline: Timeline,
}

impl Shots {
    /// The shots of a finished bundle (`scrawl reprocess`, `scrawl appshot`).
    pub fn load(bundle_dir: &Path) -> Option<Self> {
        let dir = bundle_dir.join(DIR);
        let timeline = serde_json::from_slice(&std::fs::read(dir.join(TIMELINE)).ok()?).ok()?;
        Some(Self { dir, timeline })
    }

    pub fn shot(&self, file: &str) -> Option<Shot> {
        serde_json::from_slice(&std::fs::read(self.dir.join(file)).ok()?).ok()
    }

    /// Takes a paused recording's pauses out (`crate::pause`).
    pub fn cut(&mut self, cuts: &crate::pause::Cuts) {
        cuts.cut_moments(&mut self.timeline.observations, |seen| &mut seen.t);
        cuts.cut_moments(&mut self.timeline.scenes, |t| t);
    }

    /// Moves the shots into `bundle_dir/appshots/` as `shot-NN.json` (in the order first seen)
    /// with `timeline.json`, and gives each keyframe its shot. Nothing is written when there
    /// is no shot.
    pub fn store(mut self, bundle_dir: &Path, bundle: &mut crate::model::ScrawlBundle) -> Result<()> {
        let dest = bundle_dir.join(DIR);
        let mut names: HashMap<String, String> = HashMap::new();
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        self.timeline.observations.retain_mut(|seen| {
            if let Some(name) = names.get(&seen.shot) {
                seen.shot.clone_from(name);
                return true;
            }
            let Ok(bytes) = std::fs::read(self.dir.join(&seen.shot)) else {
                return false;
            };
            let name = format!("shot-{:02}.json", files.len() + 1);
            names.insert(std::mem::replace(&mut seen.shot, name.clone()), name.clone());
            files.push((name, bytes));
            true
        });
        // The spool has done its work, whether or not anything is kept.
        let _ = std::fs::remove_dir_all(dest.join(SPOOL));
        if files.is_empty() {
            let _ = std::fs::remove_dir(&dest);
            return Ok(());
        }
        std::fs::create_dir_all(&dest)
            .with_context(|| format!("could not create {}", dest.display()))?;
        for (name, bytes) in &files {
            std::fs::write(dest.join(name), bytes)?;
        }
        std::fs::write(
            dest.join(TIMELINE),
            serde_json::to_vec_pretty(&self.timeline)?,
        )?;
        let picks = match_keyframes(bundle, &self.timeline);
        for (keyframe, pick) in bundle.keyframes.iter_mut().zip(picks) {
            keyframe.appshot = pick.map(|i| self.timeline.observations[i].shot.clone());
        }
        let slowest = self.timeline.observations.iter().map(|seen| seen.took_ms).max();
        crate::capture::scrawl_log(format!(
            "App shots: {} taken, {} states, {} of {} keyframes have one, slowest {} ms{}",
            self.timeline.observations.len(),
            files.len(),
            bundle.keyframes.iter().filter(|k| k.appshot.is_some()).count(),
            bundle.keyframes.len(),
            slowest.unwrap_or(0),
            if self.timeline.capped { ", capped" } else { "" }
        ));
        Ok(())
    }
}

/// The window a keyframe should show, and from when to when the capture stayed on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expected {
    /// `None` only for a Window capture made without a window id.
    pub window_id: Option<u32>,
    pub bundle_id: Option<String>,
    pub app: String,
    pub title: String,
    /// `from..until` on the capture clock.
    pub from: u64,
    pub until: u64,
}

/// The window the capture was on at `t`: for a Screen or Area capture the top window
/// (`windows::top_window`) of the last window-log snapshot at or before `t` (the first one for
/// an earlier `t`), for a Window capture its window. `None` when no app window was on top.
pub fn expected_window(bundle: &crate::model::ScrawlBundle, t: u64) -> Option<Expected> {
    let mode = bundle.target.mode;
    match mode {
        CaptureMode::Device => None,
        CaptureMode::Window => Some(Expected {
            window_id: bundle.target.window_id,
            bundle_id: bundle.target.bundle_id.clone(),
            app: bundle.target.app.clone().unwrap_or_default(),
            title: bundle.target.window_title.clone().unwrap_or_default(),
            from: 0,
            until: u64::MAX,
        }),
        CaptureMode::Display | CaptureMode::Region => {
            let snapshots: &[WindowSnapshot] = &bundle.windows;
            let at = snapshots
                .iter()
                .rposition(|snapshot| snapshot.t <= t)
                .or((!snapshots.is_empty()).then_some(0))?;
            let top = |i: usize| crate::windows::top_window(&snapshots[i], mode).map(|w| w.id);
            let window = crate::windows::top_window(&snapshots[at], mode)?;
            let mut first = at;
            while first > 0 && top(first - 1) == Some(window.id) {
                first -= 1;
            }
            let until = (at + 1..snapshots.len())
                .find(|&i| top(i) != Some(window.id))
                .map_or(u64::MAX, |i| snapshots[i].t);
            Some(Expected {
                window_id: Some(window.id),
                bundle_id: window.bundle_id.clone(),
                app: window.app.clone(),
                title: window.title.clone(),
                from: if first == 0 { 0 } else { snapshots[first].t },
                until,
            })
        }
    }
}

impl Expected {
    /// Whether `seen` is a shot of this window. A shot without a window id goes by the bundle
    /// id when both have one, else by the app's name.
    fn is(&self, seen: &Observation) -> bool {
        match (seen.window_id, self.window_id) {
            (Some(seen), Some(expected)) => seen == expected,
            _ => match (&seen.bundle_id, &self.bundle_id) {
                (Some(seen), Some(expected)) => seen == expected,
                _ => seen.app == self.app,
            },
        }
    }
}

/// When the window last changed under the user's hand or eye: clicks, the ends of drags and
/// marks, and picture changes, sorted.
fn triggers(bundle: &crate::model::ScrawlBundle, timeline: &Timeline) -> Vec<u64> {
    use crate::model::InteractionEvent;
    let mut out: Vec<u64> = bundle
        .events
        .iter()
        .filter_map(|event| match event {
            InteractionEvent::Click { t, .. } => Some(*t),
            InteractionEvent::Drag { t1, .. } => Some(*t1),
            _ => None,
        })
        .chain(bundle.strokes.iter().filter(|s| !s.laser).map(|s| s.t1))
        .chain(timeline.scenes.iter().copied())
        .collect();
    out.sort_unstable();
    out
}

/// For each keyframe, the observation it shows (an index into `timeline.observations`).
///
/// A keyframe takes a shot only of the window the capture was on at its time
/// (`expected_window`), taken while the capture stayed on that window and within `MATCH_MS`:
/// the nearest one in time. But a shot from before the last click, mark or picture change
/// before the keyframe shows the window as it was, so it loses to any shot from after that.
pub fn match_keyframes(
    bundle: &crate::model::ScrawlBundle,
    timeline: &Timeline,
) -> Vec<Option<usize>> {
    let triggers = triggers(bundle, timeline);
    bundle
        .keyframes
        .iter()
        .map(|keyframe: &Keyframe| {
            let expected = expected_window(bundle, keyframe.t)?;
            let changed = triggers.iter().rev().find(|&&t| t <= keyframe.t).copied();
            timeline
                .observations
                .iter()
                .enumerate()
                .filter(|(_, seen)| {
                    expected.is(seen)
                        && (expected.from..expected.until).contains(&seen.t)
                        && seen.t.abs_diff(keyframe.t) <= MATCH_MS
                })
                .min_by_key(|(_, seen)| {
                    let stale = changed.is_some_and(|changed| seen.t < changed);
                    // Nearest first; of two as near, the later one.
                    (stale, seen.t.abs_diff(keyframe.t), u64::MAX - seen.t)
                })
                .map(|(i, _)| i)
        })
        .collect()
}

/// `…/appshot-secret.html`: a URL short enough for one line of the list.
fn short_url(url: &str) -> String {
    let bare = url.split_once("://").map_or(url, |(_, rest)| rest);
    let bare = bare.trim_matches('/');
    match bare.rsplit_once('/') {
        Some((_, last)) if bare.chars().count() > 32 => format!("…/{last}"),
        _ => bare.to_owned(),
    }
}

/// `1,500`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One keyframe's row of `scrawl appshot`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    frame: usize,
    /// The keyframe's time (ms).
    t: u64,
    app: String,
    window: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    /// The shot's file in `appshots/`, when the keyframe has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    shot: Option<String>,
    /// How far from the keyframe the shot was taken.
    #[serde(skip_serializing_if = "Option::is_none")]
    age_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    took_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nodes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    withheld: Option<String>,
}

/// A keyframe's row and its shot. A keyframe without a shot names the window it expected.
fn row(
    bundle: &crate::model::ScrawlBundle,
    shots: &Shots,
    keyframe: &Keyframe,
) -> (Row, Option<Shot>) {
    let seen = keyframe.appshot.as_ref().and_then(|file| {
        shots
            .timeline
            .observations
            .iter()
            .filter(|seen| &seen.shot == file)
            .min_by_key(|seen| seen.t.abs_diff(keyframe.t))
    });
    let shot = seen.and_then(|seen| shots.shot(&seen.shot));
    let expected = expected_window(bundle, keyframe.t);
    let (app, window) = match (&shot, &expected) {
        (Some(shot), _) => (shot.app.clone(), shot.title.clone()),
        (None, Some(expected)) => (expected.app.clone(), expected.title.clone()),
        (None, None) => Default::default(),
    };
    let row = Row {
        frame: keyframe.index,
        t: keyframe.t,
        app,
        window,
        url: shot.as_ref().and_then(|shot| shot.url.clone()),
        shot: shot.as_ref().and(keyframe.appshot.clone()),
        age_ms: shot.as_ref().and(seen).map(|seen| seen.t.abs_diff(keyframe.t)),
        took_ms: shot.as_ref().and(seen).map(|seen| seen.took_ms),
        nodes: shot.as_ref().map(|shot| shot.nodes),
        truncated: shot.as_ref().and_then(|shot| shot.truncated.clone()),
        withheld: shot.as_ref().and_then(|shot| shot.withheld.clone()),
    };
    (row, shot)
}

/// `scrawl appshot [BUNDLE] [--frame N] [--json]`: what to print, and the exit code.
///
/// Without `frame` it lists each keyframe's app, window, URL, how far from the keyframe its shot
/// was taken, and the size of its tree: never the tree, also with `json`. With `frame` (the
/// keyframe's number, from 1) it prints that keyframe's header and outline. A capture without
/// shots says so and exits 0; a frame out of range names the range and exits 2.
pub fn report(
    bundle_dir: &Path,
    bundle: &crate::model::ScrawlBundle,
    frame: Option<usize>,
    json: bool,
) -> (String, i32) {
    use crate::model::format_clock;
    let Some(shots) = Shots::load(bundle_dir) else {
        return ("no app shots in this capture\n".into(), 0);
    };
    let dash = |text: &str| {
        if text.is_empty() {
            "-".to_owned()
        } else {
            text.to_owned()
        }
    };
    let Some(n) = frame else {
        let rows: Vec<Row> = bundle
            .keyframes
            .iter()
            .map(|keyframe| row(bundle, &shots, keyframe).0)
            .collect();
        if json {
            let value = serde_json::json!({
                "capture": bundle.id,
                "capped": shots.timeline.capped,
                "keyframes": rows,
            });
            return (format!("{value:#}\n"), 0);
        }
        let mut table = vec![["#", "time", "app", "window", "url", "age", "nodes"].map(String::from)];
        for row in &rows {
            table.push([
                row.frame.to_string(),
                format_clock(row.t),
                dash(&row.app),
                dash(&row.window),
                dash(&row.url.as_deref().map(short_url).unwrap_or_default()),
                row.age_ms
                    .map_or("-".into(), |ms| format!("{:.1} s", ms as f64 / 1_000.0)),
                row.nodes.map_or("-".into(), thousands),
            ]);
        }
        let width =
            |column: usize| table.iter().map(|r| r[column].chars().count()).max().unwrap_or(0);
        let widths: Vec<usize> = (0..7).map(width).collect();
        let mut out = String::new();
        for (i, cells) in table.iter().enumerate() {
            let line: Vec<String> = cells
                .iter()
                .enumerate()
                .map(|(column, cell)| {
                    let pad = " ".repeat(widths[column] - cell.chars().count());
                    // The age and the node count line up on the right.
                    if column >= 5 {
                        format!("{pad}{cell}")
                    } else {
                        format!("{cell}{pad}")
                    }
                })
                .collect();
            out.push_str(line.join("  ").trim_end());
            out.push('\n');
            let Some(row) = i.checked_sub(1).map(|i| &rows[i]) else {
                continue;
            };
            let indent = " ".repeat(widths[0] + 2);
            if row.shot.is_none() {
                out.push_str(&format!("{indent}no app shot of this window\n"));
            }
            if let Some(why) = &row.withheld {
                out.push_str(&format!("{indent}no tree: {why}\n"));
            }
            if let Some(cap) = &row.truncated {
                out.push_str(&format!("{indent}cut: {cap}\n"));
            }
        }
        if shots.timeline.capped {
            out.push_str("App shots stopped at 32 MB for this capture: later states are missing.\n");
        }
        return (out, 0);
    };
    let total = bundle.keyframes.len();
    let Some(keyframe) = n.checked_sub(1).and_then(|i| bundle.keyframes.get(i)) else {
        let range = match total {
            0 => "this capture has no keyframes".to_owned(),
            1 => "this capture has keyframe 1 only".to_owned(),
            _ => format!("this capture has keyframes 1 to {total}"),
        };
        return (format!("--frame {n} is out of range: {range}\n"), 2);
    };
    let (row, shot) = row(bundle, &shots, keyframe);
    let Some(shot) = shot else {
        let expected = match (row.app.is_empty(), row.window.is_empty()) {
            (true, _) => "no app window was in front then".to_owned(),
            (false, true) => format!("expected {}", row.app),
            (false, false) => format!("expected {} “{}”", row.app, row.window),
        };
        let text = format!(
            "no app shot for keyframe {n} ({}): {expected}\n",
            format_clock(keyframe.t)
        );
        if json {
            let value = serde_json::json!({"capture": bundle.id, "keyframe": row});
            return (format!("{value:#}\n"), 0);
        }
        return (text, 0);
    };
    if json {
        let value = serde_json::json!({"capture": bundle.id, "keyframe": row, "tree": shot.tree});
        return (format!("{value:#}\n"), 0);
    }
    let mut header = vec![shot.app.clone()];
    if !shot.title.is_empty() {
        header.push(format!("“{}”", shot.title));
    }
    header.extend(shot.url.as_deref().map(short_url));
    header.push(format_clock(keyframe.t));
    header.push(format!("{} nodes", thousands(shot.nodes)));
    header.extend(shot.truncated.as_ref().map(|cap| format!("cut: {cap}")));
    let mut out = header.join(" · ");
    out.push('\n');
    if let Some(why) = &shot.withheld {
        out.push_str(&format!("no tree: {why}\n"));
    }
    for line in &shot.tree {
        out.push_str(line);
        out.push('\n');
    }
    (out, 0)
}

/// Deletes the spools of captures that never finished: at launch, when nothing records, every
/// bundle folder in `captures` without a `scrawl.json` loses its `appshots/.spool`.
pub fn sweep_spools(captures: &Path) {
    let Ok(entries) = std::fs::read_dir(captures) else {
        return;
    };
    for dir in entries.flatten().map(|entry| entry.path()) {
        if dir.join("scrawl.json").exists() {
            continue;
        }
        drop_spool(&dir);
    }
}

/// Removes `bundle_dir`'s spool, and `appshots/` with it when nothing else is there.
pub fn drop_spool(bundle_dir: &Path) {
    let shots = bundle_dir.join(DIR);
    if std::fs::remove_dir_all(shots.join(SPOOL)).is_ok() {
        let _ = std::fs::remove_dir(&shots);
    }
}

/// What a capture's shots are taken of.
#[derive(Clone, Debug)]
pub struct Session {
    pub mode: CaptureMode,
    /// A Window capture's window.
    pub window_id: Option<u32>,
    /// The captured rectangle in global points.
    pub region: TargetFrame,
    /// The capture clock's zero, as the window log has it.
    pub started_at: Instant,
    pub bundle_dir: PathBuf,
}

/// One read of the captured window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Taken {
    pub shot: Shot,
    pub pid: i32,
    pub frame: [i32; 4],
}

/// Reads the captured window. The real one talks to the Accessibility API (`MacTaker`); tests
/// use a fake.
pub trait Taker: Send + 'static {
    /// `dropped` says the capture no longer wants this shot (it finished, was cancelled or
    /// paused): stop reading.
    fn take(&mut self, session: &Session, dropped: &dyn Fn() -> bool) -> Option<Taken>;
    /// The capture is over: undo what taking changed in other apps.
    fn release(&mut self) {}
}

enum Message {
    Shoot { generation: u64, due: Instant },
    Release,
}

struct Live {
    generation: u64,
    session: Session,
    spool: Spool,
    last_scene: Option<Instant>,
}

struct Shared {
    /// Counts captures: a request or a walk of another generation is dropped.
    generation: AtomicU64,
    paused: AtomicBool,
    live: Mutex<Option<Live>>,
    walking: Mutex<bool>,
    walked: Condvar,
}

/// Takes app shots for one capture at a time on a background thread. Its queue holds one
/// request (the latest wins), a walk never lands in a capture it wasn't started for, and
/// nothing is taken while the recording is paused.
pub struct Walker {
    messages: flume::Sender<Message>,
    shared: Arc<Shared>,
}

/// The app's walker, on systems that take app shots.
pub fn walker() -> Option<&'static Walker> {
    cfg!(target_os = "macos").then(Walker::shared)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Walker {
    fn shared() -> &'static Walker {
        static WALKER: OnceLock<Walker> = OnceLock::new();
        WALKER.get_or_init(|| Walker::start(platform_taker()))
    }

    fn start(mut taker: impl Taker) -> Self {
        let (messages, queue) = flume::unbounded::<Message>();
        let shared = Arc::new(Shared {
            generation: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            live: Mutex::new(None),
            walking: Mutex::new(false),
            walked: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let _ = std::thread::Builder::new()
            .name("scrawl-appshot".into())
            .spawn(move || {
                // The one request that waits: `(generation, due)`.
                let mut waiting: Option<(u64, Instant)> = None;
                loop {
                    let message = match waiting {
                        Some((_, due)) => match queue.recv_deadline(due) {
                            Ok(message) => Some(message),
                            Err(flume::RecvTimeoutError::Timeout) => None,
                            Err(flume::RecvTimeoutError::Disconnected) => return,
                        },
                        None => match queue.recv() {
                            Ok(message) => Some(message),
                            Err(_) => return,
                        },
                    };
                    match message {
                        Some(Message::Shoot { generation, due }) => {
                            waiting = Some((generation, due));
                        }
                        Some(Message::Release) => taker.release(),
                        None => {
                            if let Some((generation, _)) = waiting.take() {
                                worker.walk(generation, &mut taker);
                            }
                        }
                    }
                }
            });
        Self { messages, shared }
    }

    /// A capture starts: its first shot is taken at once.
    pub fn begin(&self, session: Session) {
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.shared.paused.store(false, Ordering::Release);
        let spool = Spool::new(&session.bundle_dir);
        *lock(&self.shared.live) = Some(Live {
            generation,
            session,
            spool,
            last_scene: None,
        });
        self.request(Duration::ZERO);
    }

    /// Asks for a shot `after` from now. Nothing happens without a capture or while paused.
    pub fn request(&self, after: Duration) {
        if self.shared.paused.load(Ordering::Acquire) || lock(&self.shared.live).is_none() {
            return;
        }
        let _ = self.messages.send(Message::Shoot {
            generation: self.shared.generation.load(Ordering::Acquire),
            due: Instant::now() + after,
        });
    }

    /// The picture changed: notes when, and asks for a shot, at most once per `SCENE_GAP`.
    pub fn scene(&self) {
        if self.shared.paused.load(Ordering::Acquire) {
            return;
        }
        {
            let mut live = lock(&self.shared.live);
            let Some(live) = live.as_mut() else {
                return;
            };
            if live
                .last_scene
                .is_some_and(|last| last.elapsed() < SCENE_GAP)
            {
                return;
            }
            live.last_scene = Some(Instant::now());
            let t = live.session.started_at.elapsed().as_millis() as u64;
            live.spool.scenes.push(t);
        }
        self.request(Duration::ZERO);
    }

    /// Whether a capture is taking shots.
    pub fn is_live(&self) -> bool {
        lock(&self.shared.live).is_some()
    }

    /// A paused recording takes no shot: the one waiting and the one in flight are dropped.
    /// Resuming takes one at once.
    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
        if !paused {
            self.request(Duration::ZERO);
        }
    }

    /// The capture finished: waits at most `FINISH_WAIT` for the walk in flight, drops it if
    /// it is still going, and hands over what was taken.
    pub fn finish(&self) -> Option<Shots> {
        let walking = lock(&self.shared.walking);
        let (walking, _) = self
            .shared
            .walked
            .wait_timeout_while(walking, FINISH_WAIT, |walking| *walking)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(walking);
        let live = self.end()?;
        Some(live.spool.into_shots())
    }

    /// The capture was thrown away: the walk in flight is dropped at once, and the spool goes.
    pub fn cancel(&self) {
        if let Some(live) = self.end() {
            drop_spool(&live.session.bundle_dir);
        }
    }

    fn end(&self) -> Option<Live> {
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        self.shared.paused.store(false, Ordering::Release);
        let live = lock(&self.shared.live).take();
        if live.is_some() {
            let _ = self.messages.send(Message::Release);
        }
        live
    }
}

impl Shared {
    fn dropped(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) != generation
            || self.paused.load(Ordering::Acquire)
    }

    fn walk(&self, generation: u64, taker: &mut impl Taker) {
        if self.dropped(generation) {
            return;
        }
        let Some(session) = lock(&self.live)
            .as_ref()
            .filter(|live| live.generation == generation)
            .map(|live| live.session.clone())
        else {
            return;
        };
        *lock(&self.walking) = true;
        let t = session.started_at.elapsed().as_millis() as u64;
        let began = Instant::now();
        let taken = taker.take(&session, &|| self.dropped(generation));
        let took_ms = began.elapsed().as_millis() as u64;
        if let Some(staged) = taken.and_then(Staged::new) {
            self.land(generation, t, took_ms, staged);
        }
        *lock(&self.walking) = false;
        self.walked.notify_all();
    }

    /// The capture a walk of `generation` was started for, while it still wants the shot.
    fn current<'a>(&self, live: &'a mut Option<Live>, generation: u64) -> Option<&'a mut Live> {
        live.as_mut()
            .filter(|live| live.generation == generation && !self.dropped(generation))
    }

    /// Puts a shot into its capture's spool. `finish` and `cancel` take the capture under
    /// `live`, so the shot is noted under that lock and a walk that ends late never lands in
    /// the next capture. The file is written between two short holds of the lock, never under
    /// it: a write that stalls holds up neither `finish` nor `cancel`. A shot whose capture
    /// ended while its file was written is dropped, and so is the file.
    fn land(&self, generation: u64, t: u64, took_ms: u64, staged: Staged) {
        let plan = {
            let mut live = lock(&self.live);
            let Some(live) = self.current(&mut live, generation) else {
                return;
            };
            live.spool.plan(&staged)
        };
        let wrote = match &plan {
            Plan::Capped => return,
            Plan::Seen => false,
            Plan::Write(path) => {
                if !staged.write(path) {
                    return;
                }
                true
            }
        };
        let mut live = lock(&self.live);
        match self.current(&mut live, generation) {
            Some(live) => live.spool.note(t, took_ms, staged, wrote),
            None => {
                if let Plan::Write(path) = &plan {
                    Staged::unwrite(path);
                }
            }
        }
    }
}

/// A shot ready for the spool: its file's name and bytes, made before any lock is taken.
struct Staged {
    file: String,
    bytes: Vec<u8>,
    taken: Taken,
}

impl Staged {
    fn new(taken: Taken) -> Option<Self> {
        Some(Self {
            file: format!("{}.json", taken.shot.key()),
            bytes: serde_json::to_vec_pretty(&taken.shot).ok()?,
            taken,
        })
    }

    /// Writes the file at `path` in the spool. As slow as the disk: never call it under
    /// `Shared::live`. It makes `appshots/` and the spool, never the bundle's own folder: a
    /// capture that was thrown away stays gone.
    fn write(&self, path: &Path) -> bool {
        let Some((spool, shots)) = path.parent().and_then(|spool| Some((spool, spool.parent()?)))
        else {
            return false;
        };
        for dir in [shots, spool] {
            match std::fs::create_dir(dir) {
                Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return false,
                _ => {}
            }
        }
        std::fs::write(path, &self.bytes).is_ok()
    }

    /// Takes back a file written for a capture that ended meanwhile, and the spool with it
    /// when nothing else is there.
    fn unwrite(path: &Path) {
        let _ = std::fs::remove_file(path);
        if let Some(spool) = path.parent() {
            let _ = std::fs::remove_dir(spool);
            if let Some(shots) = spool.parent() {
                let _ = std::fs::remove_dir(shots);
            }
        }
    }
}

/// What the spool wants done with a shot's file.
enum Plan {
    /// This state is in the spool already.
    Seen,
    /// The spool is full: the shot is not kept.
    Capped,
    /// Write it here, then `Spool::note` it.
    Write(PathBuf),
}

/// A capture's shots while it records: each new one is written to `appshots/.spool/` at once,
/// so none is held in memory; only when each was seen is. The spool itself never writes: it
/// says what to do with a shot's file (`plan`) and notes the shot once that is done (`note`).
struct Spool {
    dir: PathBuf,
    /// Shot key to its file.
    seen: HashSet<String>,
    bytes: u64,
    observations: Vec<Observation>,
    scenes: Vec<u64>,
    capped: bool,
}

impl Spool {
    fn new(bundle_dir: &Path) -> Self {
        Self {
            dir: bundle_dir.join(DIR).join(SPOOL),
            seen: HashSet::new(),
            bytes: 0,
            observations: Vec::new(),
            scenes: Vec::new(),
            capped: false,
        }
    }

    /// What to do with `staged`'s file. Past the cap the spool stays capped.
    fn plan(&mut self, staged: &Staged) -> Plan {
        if self.seen.contains(&staged.file) {
            return Plan::Seen;
        }
        if self.capped || self.bytes + staged.bytes.len() as u64 > SPOOL_CAP {
            self.capped = true;
            return Plan::Capped;
        }
        Plan::Write(self.dir.join(&staged.file))
    }

    /// Notes that `staged` was seen at `t`; `wrote` when its file was written for this.
    fn note(&mut self, t: u64, took_ms: u64, staged: Staged, wrote: bool) {
        let Staged { file, bytes, taken } = staged;
        let Taken { shot, pid, frame } = taken;
        if wrote {
            self.bytes += bytes.len() as u64;
            self.seen.insert(file.clone());
        }
        self.observations.push(Observation {
            t,
            shot: file,
            window_id: shot.window_id,
            pid,
            bundle_id: shot.bundle_id,
            app: shot.app,
            title: shot.title,
            frame,
            took_ms,
        });
    }

    /// `plan`, the write and `note` in one go, for tests that fill a spool by hand.
    #[cfg(test)]
    fn add(&mut self, t: u64, took_ms: u64, taken: Taken) {
        let Some(staged) = Staged::new(taken) else {
            return;
        };
        let wrote = match self.plan(&staged) {
            Plan::Capped => return,
            Plan::Seen => false,
            Plan::Write(path) => {
                if !staged.write(&path) {
                    return;
                }
                true
            }
        };
        self.note(t, took_ms, staged, wrote);
    }

    fn into_shots(self) -> Shots {
        Shots {
            dir: self.dir,
            timeline: Timeline {
                observations: self.observations,
                scenes: self.scenes,
                capped: self.capped,
            },
        }
    }
}

/// Takes nothing: systems without app shots.
#[cfg(not(target_os = "macos"))]
struct NoTaker;

#[cfg(not(target_os = "macos"))]
impl Taker for NoTaker {
    fn take(&mut self, _: &Session, _: &dyn Fn() -> bool) -> Option<Taken> {
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn platform_taker() -> impl Taker {
    NoTaker
}

#[cfg(target_os = "macos")]
fn platform_taker() -> impl Taker {
    mac::MacTaker::default()
}

#[cfg(target_os = "macos")]
mod mac {
    use std::collections::HashMap;
    use std::time::Instant;

    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::string::{CFString, CFStringRef};

    use super::{LIMITS, Session, Taken, Taker, shot_of, walk};
    use crate::ax::{AXUIElementCreateApplication, AxElement, MacTree, Owned};
    use crate::model::CaptureMode;
    use crate::privacy::Tree;
    use crate::windows::{AppMeta, RawWindow};

    /// Each Accessibility message of a walk gives up after this.
    const MESSAGE_TIMEOUT_S: f32 = 0.25;
    /// An Accessibility window is the captured one when its position and size are this close.
    const WINDOW_SLOP_PT: f64 = 2.0;
    /// A tree this small right after Electron's accessibility was turned on isn't built yet.
    const EMPTY_TREE_NODES: usize = 3;

    const AX_VALUE_CG_POINT: u32 = 1;
    const AX_VALUE_CG_SIZE: u32 = 2;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementSetAttributeValue(
            element: AxElement,
            attribute: CFStringRef,
            value: CFTypeRef,
        ) -> i32;
        fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut std::ffi::c_void) -> bool;
        fn AXValueGetTypeID() -> usize;
    }

    /// Whether Annotator turned an Electron app's accessibility tree on.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Electron {
        /// Not an Electron app, its tree was on already, or it doesn't say.
        Leave,
        /// `AXManualAccessibility` read false and Annotator set it: reset it when the capture
        /// ends.
        SetByUs,
    }

    #[derive(Default)]
    pub struct MacTaker {
        apps: HashMap<i32, AppMeta>,
        electron: HashMap<i32, Electron>,
    }

    /// A point or size attribute (`AXPosition`, `AXSize`) as two numbers.
    fn pair(element: &Owned, name: &'static str, kind: u32) -> Option<[f64; 2]> {
        let value = crate::ax::attribute(element.0, name)?;
        if unsafe { core_foundation::base::CFGetTypeID(value.0) } != unsafe { AXValueGetTypeID() } {
            return None;
        }
        let mut out = [0.0_f64; 2];
        unsafe { AXValueGetValue(value.0, kind, out.as_mut_ptr().cast()) }.then_some(out)
    }

    /// `pid`'s Accessibility window at `bounds` (global points), which is how a window-server
    /// window id finds its Accessibility element.
    fn window_at(tree: &MacTree, app: &Owned, bounds: [f64; 4]) -> Option<Owned> {
        tree.elements(app, "AXWindows").into_iter().find(|window| {
            let near = |a: f64, b: f64| (a - b).abs() <= WINDOW_SLOP_PT;
            matches!(
                (
                    pair(window, "AXPosition", AX_VALUE_CG_POINT),
                    pair(window, "AXSize", AX_VALUE_CG_SIZE),
                ),
                (Some([x, y]), Some([w, h]))
                    if near(x, bounds[0]) && near(y, bounds[1]) && near(w, bounds[2]) && near(h, bounds[3])
            )
        })
    }

    /// The app's focused window. Some apps answer `AXFocusedWindow` with the application
    /// itself when no window has the focus (Chrome does): that is not a window to read.
    fn focused_window(tree: &MacTree, app: &Owned) -> Option<Owned> {
        tree.element(app, "AXFocusedWindow")
            .filter(|window| tree.string(window, "AXRole").as_deref() == Some("AXWindow"))
    }

    fn is_electron(pid: i32) -> bool {
        use objc2_app_kit::NSRunningApplication;
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .and_then(|app| app.bundleURL())
            .and_then(|url| url.path())
            .is_some_and(|path| {
                std::path::Path::new(&path.to_string())
                    .join("Contents/Frameworks/Electron Framework.framework")
                    .exists()
            })
    }

    fn set_manual_accessibility(app: AxElement, on: bool) {
        let key = CFString::from_static_string("AXManualAccessibility");
        let value = if on {
            CFBoolean::true_value()
        } else {
            CFBoolean::false_value()
        };
        unsafe {
            AXUIElementSetAttributeValue(app, key.as_concrete_TypeRef(), value.as_CFTypeRef())
        };
    }

    impl MacTaker {
        /// An Electron app builds no Accessibility tree until asked. When its
        /// `AXManualAccessibility` reads false, sets it and returns true: the tree isn't there
        /// yet. An app that can't be asked is left alone. `AXEnhancedUserInterface` is never
        /// set: it changes how apps lay out and animate.
        fn wake_electron(&mut self, pid: i32, app: &Owned) -> bool {
            if self.electron.contains_key(&pid) {
                return false;
            }
            let mut state = Electron::Leave;
            if is_electron(pid)
                && let Some(value) = crate::ax::attribute(app.0, "AXManualAccessibility")
                && let Some(on) = unsafe { CFType::wrap_under_get_rule(value.0) }
                    .downcast::<CFBoolean>()
                    .map(bool::from)
                && !on
            {
                set_manual_accessibility(app.0, true);
                state = Electron::SetByUs;
            }
            self.electron.insert(pid, state);
            state == Electron::SetByUs
        }
    }

    impl Taker for MacTaker {
        fn take(&mut self, session: &Session, dropped: &dyn Fn() -> bool) -> Option<Taken> {
            let began = Instant::now();
            let own = std::process::id() as i32;
            let (raw, front) = objc2::rc::autoreleasepool(|_| {
                (
                    crate::windows::raw_windows(),
                    crate::capture::frontmost_external_pid(),
                )
            });
            let window: &RawWindow = match session.mode {
                CaptureMode::Display | CaptureMode::Region => {
                    let apps = &mut self.apps;
                    let top = objc2::rc::autoreleasepool(|_| {
                        crate::windows::top_raw_window(
                            &raw,
                            session.region,
                            session.mode,
                            own,
                            front,
                            &mut |pid| {
                                apps.entry(pid)
                                    .or_insert_with(|| crate::windows::app_meta_of(pid))
                                    .clone()
                            },
                        )
                    })?;
                    raw.iter().find(|w| w.id == top.id)?
                }
                // A Window capture's window is gone when the window server no longer lists it.
                CaptureMode::Window => raw
                    .iter()
                    .find(|w| Some(w.id) == session.window_id && w.pid != own)?,
                CaptureMode::Device => return None,
            };
            let pid = window.pid;
            let bundle_id = objc2::rc::autoreleasepool(|_| {
                self.apps
                    .entry(pid)
                    .or_insert_with(|| crate::windows::app_meta_of(pid))
                    .bundle_id
                    .clone()
            });
            let [x, y, w, h] = window.bounds;
            let frame = [
                (x - session.region.x).round() as i32,
                (y - session.region.y).round() as i32,
                w.round() as i32,
                h.round() as i32,
            ];
            let taken = |shot| Some(Taken { shot, pid, frame });
            // A password manager is never asked anything.
            if crate::privacy::is_sensitive_app(bundle_id.as_deref()) {
                return taken(shot_of(
                    &window.owner,
                    bundle_id.as_deref(),
                    Some(window.id),
                    "",
                    None,
                    Default::default(),
                ));
            }
            let tree = MacTree {
                timeout_s: MESSAGE_TIMEOUT_S,
            };
            let app = tree.adopt(Owned(unsafe { AXUIElementCreateApplication(pid) }));
            let woken = objc2::rc::autoreleasepool(|_| self.wake_electron(pid, &app));
            // The window by its place on screen, else the app's focused window, which then has
            // no window id to its name.
            let (element, window_id) = match window_at(&tree, &app, window.bounds) {
                Some(element) => (element, Some(window.id)),
                None => (focused_window(&tree, &app)?, None),
            };
            let stop = || began.elapsed() > LIMITS.deadline || dropped();
            let document = tree.url(&element, "AXDocument");
            let title = if window.title.trim().is_empty() {
                tree.string(&element, "AXTitle").unwrap_or_default()
            } else {
                window.title.clone()
            };
            let outline = if document
                .as_deref()
                .is_some_and(crate::privacy::is_internal_url)
            {
                Default::default()
            } else {
                walk(&tree, &element, &LIMITS, &stop)
            };
            if dropped() {
                return None;
            }
            // Electron builds its tree after it is asked for: the next shot gets it.
            if woken && outline.nodes <= EMPTY_TREE_NODES {
                return None;
            }
            taken(shot_of(
                &window.owner,
                bundle_id.as_deref(),
                window_id,
                &title,
                document.as_deref(),
                outline,
            ))
        }

        fn release(&mut self) {
            for (pid, state) in self.electron.drain() {
                if state == Electron::SetByUs {
                    let app = Owned(unsafe { AXUIElementCreateApplication(pid) });
                    unsafe {
                        crate::ax::AXUIElementSetMessagingTimeout(app.0, MESSAGE_TIMEOUT_S);
                    }
                    set_manual_accessibility(app.0, false);
                }
            }
            self.apps.clear();
        }
    }

    /// `cargo test appshot_live -- --ignored --nocapture` prints the app shot of the frontmost
    /// app's focused window (or of `SCRAWL_APPSHOT_PID`'s), read through the real Accessibility
    /// API, and what a Screen capture of the main display would take right now. The terminal
    /// needs Accessibility. It only reads: it never turns an Electron app's tree on.
    #[cfg(test)]
    mod live {
        use super::*;

        #[test]
        #[ignore = "reads the frontmost app through the Accessibility API"]
        fn appshot_live_reads_the_frontmost_window() {
            use objc2_app_kit::{NSRunningApplication, NSWorkspace};
            // `SCRAWL_APPSHOT_PID=<pid>` reads that app instead of the frontmost one.
            let front = std::env::var("SCRAWL_APPSHOT_PID")
                .ok()
                .and_then(|pid| pid.parse().ok())
                .and_then(NSRunningApplication::runningApplicationWithProcessIdentifier)
                .or_else(|| NSWorkspace::sharedWorkspace().frontmostApplication())
                .expect("a frontmost app");
            let pid = front.processIdentifier();
            let bundle_id = front.bundleIdentifier().map(|id| id.to_string());
            let name = front
                .localizedName()
                .map(|name| name.to_string())
                .unwrap_or_default();
            let tree = MacTree {
                timeout_s: MESSAGE_TIMEOUT_S,
            };
            let app = tree.adopt(Owned(unsafe { AXUIElementCreateApplication(pid) }));
            let window = focused_window(&tree, &app)
                .or_else(|| tree.elements(&app, "AXWindows").into_iter().next())
                .expect("the app has a window that answers");
            let began = Instant::now();
            let outline = walk(&tree, &window, &LIMITS, &|| began.elapsed() > LIMITS.deadline);
            let took = began.elapsed();
            println!(
                "window at {:?} size {:?}",
                pair(&window, "AXPosition", AX_VALUE_CG_POINT),
                pair(&window, "AXSize", AX_VALUE_CG_SIZE)
            );
            let title = tree.string(&window, "AXTitle").unwrap_or_default();
            let document = tree.url(&window, "AXDocument");
            let shot = shot_of(
                &name,
                bundle_id.as_deref(),
                None,
                &title,
                document.as_deref(),
                outline,
            );
            println!(
                "{} ({}) · “{}” · {} · {} nodes · cut: {} · tookMs {}",
                shot.app,
                bundle_id.as_deref().unwrap_or("-"),
                shot.title,
                shot.url.as_deref().unwrap_or("-"),
                shot.nodes,
                shot.truncated.as_deref().unwrap_or("-"),
                took.as_millis()
            );
            for line in &shot.tree {
                println!("{line}");
            }
            let screen = crate::screens::displays()
                .into_iter()
                .next()
                .expect("a display");
            let taken = MacTaker::default().take(
                &Session {
                    mode: CaptureMode::Display,
                    window_id: None,
                    region: crate::screens::with_scale(screen.frame, screen.scale),
                    started_at: Instant::now(),
                    bundle_dir: std::env::temp_dir(),
                },
                &|| false,
            );
            match taken {
                Some(taken) => println!(
                    "a Screen capture takes: {} “{}” window {:?} at {:?}, {} nodes",
                    taken.shot.app,
                    taken.shot.title,
                    taken.shot.window_id,
                    taken.frame,
                    taken.shot.nodes
                ),
                None => println!("a Screen capture takes nothing now (no app window is on top)"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ScrawlBundle;
    use crate::privacy::fake::FakeTree;

    const MARKER: &str = "SECRET-MARKER";

    fn never() -> bool {
        false
    }

    /// A browser window on the fixture page: its URL and title hold a marker, and so does
    /// what was typed into its text field and its password field.
    fn fixture() -> (FakeTree, usize) {
        let mut tree = FakeTree::default();
        let window = tree.add(None, &[("AXRole", "AXWindow"), ("AXTitle", "Fixture")]);
        let web = tree.add(
            Some(window),
            &[
                ("AXRole", "AXWebArea"),
                ("AXURL", "file:///f/appshot-secret.html?token=SECRET-MARKER-URL#SECRET-MARKER-FRAG"),
            ],
        );
        tree.add(Some(web), &[("AXRole", "AXHeading"), ("AXTitle", "Settings")]);
        tree.add(
            Some(web),
            &[
                ("AXRole", "AXStaticText"),
                ("AXValue", "Open the settings to change your keys."),
            ],
        );
        let field = tree.add(
            Some(web),
            &[
                ("AXRole", "AXTextField"),
                ("AXTitle", "Name"),
                ("AXValue", "SECRET-MARKER-INPUT"),
            ],
        );
        tree.add(
            Some(field),
            &[("AXRole", "AXStaticText"), ("AXValue", "SECRET-MARKER-INPUT")],
        );
        let password = tree.add(
            Some(web),
            &[
                ("AXRole", "AXTextField"),
                ("AXSubrole", "AXSecureTextField"),
                ("AXDescription", "Password"),
                ("AXValue", "SECRET-MARKER-PASSWORD"),
            ],
        );
        tree.add(
            Some(password),
            &[("AXRole", "AXStaticText"), ("AXValue", "SECRET-MARKER-PASSWORD")],
        );
        tree.add(
            Some(web),
            &[
                ("AXRole", "AXLink"),
                ("AXTitle", "Reset https://a.dev/reset?token=SECRET-MARKER-URL"),
                ("AXValue", "SECRET-MARKER-LINK-VALUE"),
            ],
        );
        tree.add(
            Some(web),
            &[
                ("AXRole", "AXButton"),
                ("AXTitle", "Save"),
                ("AXIdentifier", "save"),
            ],
        );
        (tree, window)
    }

    #[test]
    fn appshot_outline_names_a_field_and_never_holds_what_was_typed() {
        let (tree, window) = fixture();
        let outline = walk(&tree, &window, &LIMITS, &never);
        assert_eq!(
            outline.lines,
            [
                "window \"Fixture\"",
                "  web area",
                "    heading \"Settings\"",
                "    static text \"Open the settings to change your keys.\"",
                "    text field \"Name\"",
                "    secure text field \"Password\"",
                "    link \"Reset https://a.dev/reset\"",
                "    button \"Save\" #save",
            ]
        );
        assert_eq!((outline.nodes, outline.truncated), (8, None));
        let shot = shot_of(
            "Safari",
            Some("com.apple.Safari"),
            Some(7),
            "Fixture file:///f/appshot-secret.html?token=SECRET-MARKER-URL",
            Some("file:///f/appshot-secret.html?token=SECRET-MARKER-URL"),
            outline,
        );
        assert_eq!(shot.url.as_deref(), Some("file:///f/appshot-secret.html"));
        assert_eq!(shot.title, "Fixture file:///f/appshot-secret.html");
        let stored = serde_json::to_string(&shot).unwrap();
        assert!(!stored.contains(MARKER), "{stored}");
        // The typed values were never asked for, nor anything under the fields.
        let asked_for_a_secret = tree.reads.borrow().iter().any(|(element, attribute)| {
            let under_a_field = [5, 7].contains(element);
            let a_fields_value = [4, 6].contains(element) && *attribute == "AXValue";
            let a_links_value = *element == 8 && *attribute == "AXValue";
            under_a_field || a_fields_value || a_links_value
        });
        assert!(!asked_for_a_secret, "{:?}", tree.reads.borrow());
    }

    #[test]
    fn appshot_of_a_password_manager_or_a_browsers_own_page_has_no_tree() {
        let (tree, window) = fixture();
        let outline = || walk(&tree, &window, &LIMITS, &never);
        let vault = shot_of(
            "1Password",
            Some("com.1password.1password"),
            Some(7),
            "Login SECRET-MARKER",
            Some("file:///vault?SECRET-MARKER"),
            outline(),
        );
        assert_eq!(
            vault,
            Shot {
                app: "1Password".into(),
                bundle_id: Some("com.1password.1password".into()),
                window_id: Some(7),
                withheld: Some("sensitive app".into()),
                ..Shot::default()
            }
        );
        let settings = shot_of(
            "Google Chrome",
            Some("com.google.Chrome"),
            Some(8),
            "Settings",
            Some("chrome://settings/passwords"),
            outline(),
        );
        assert!(settings.tree.is_empty());
        assert_eq!(settings.withheld.as_deref(), Some("browser page"));
        // A browser that names no document: its web area's URL decides.
        let mut inner = outline();
        inner.urls = vec!["about:blank".into()];
        let blank = shot_of("Arc", None, None, "New Tab", None, inner);
        assert!(blank.tree.is_empty());
        assert_eq!(blank.url.as_deref(), Some("about:blank"));
    }

    #[test]
    fn appshot_url_is_the_web_area_that_is_the_windows_document() {
        let areas = vec![
            "https://ads.dev/frame?x=1".to_owned(),
            "https://a.dev/page#top".to_owned(),
        ];
        assert_eq!(
            window_url(Some("https://a.dev/page"), &areas).as_deref(),
            Some("https://a.dev/page#top")
        );
        assert_eq!(
            window_url(Some("file:///notes.md"), &areas).as_deref(),
            Some("file:///notes.md"),
            "a document no web area shows"
        );
        assert_eq!(
            window_url(None, &areas).as_deref(),
            Some("https://ads.dev/frame?x=1")
        );
        assert_eq!(window_url(None, &[]), None);
    }

    /// `width` children under each of `depth` levels below a window.
    fn deep(depth: usize, width: usize) -> (FakeTree, usize) {
        let mut tree = FakeTree::default();
        let window = tree.add(None, &[("AXRole", "AXWindow")]);
        let mut level = vec![window];
        for _ in 0..depth {
            let mut next = Vec::new();
            for parent in &level {
                for _ in 0..width {
                    next.push(tree.add(Some(*parent), &[("AXRole", "AXGroup"), ("AXTitle", "Row")]));
                }
            }
            level = next;
        }
        (tree, window)
    }

    #[test]
    fn appshot_walker_stops_at_each_cap_and_names_it() {
        // Depth: 20 levels of one child each stop at level 12.
        let (tree, window) = deep(20, 1);
        let outline = walk(&tree, &window, &LIMITS, &never);
        assert_eq!(outline.truncated, Some("depth"));
        assert_eq!(outline.nodes, 13, "the window and 12 levels under it");
        // Exactly 12 levels is the whole tree.
        let (tree, window) = deep(12, 1);
        assert_eq!(walk(&tree, &window, &LIMITS, &never).truncated, None);

        // Nodes: 2,000 rows stop at 1,500.
        let (tree, window) = deep(1, 2_000);
        let outline = walk(&tree, &window, &LIMITS, &never);
        assert_eq!((outline.nodes, outline.truncated), (1_500, Some("nodes")));

        // Bytes: long rows fill 64 KB first.
        let mut tree = FakeTree::default();
        let window = tree.add(None, &[("AXRole", "AXWindow")]);
        let long = "x".repeat(190);
        for _ in 0..1_000 {
            tree.add(Some(window), &[("AXRole", "AXStaticText"), ("AXValue", &long)]);
        }
        let outline = walk(&tree, &window, &LIMITS, &never);
        assert_eq!(outline.truncated, Some("bytes"));
        let size: usize = outline.lines.iter().map(|line| line.len() + 1).sum();
        assert!(size <= LIMITS.bytes && size > LIMITS.bytes - 300, "{size}");

        // A tree that loops is walked once.
        let (mut tree, window) = deep(3, 1);
        tree.link(3, window);
        let outline = walk(&tree, &window, &LIMITS, &never);
        assert_eq!((outline.nodes, outline.truncated), (4, None));
    }

    #[test]
    fn appshot_walker_gives_up_on_a_slow_tree_at_its_deadline() {
        // Every read takes 5 ms: the 400 rows would take over 10 s.
        let (mut tree, window) = deep(1, 400);
        tree.delay = Duration::from_millis(5);
        let limits = Limits {
            deadline: Duration::from_millis(1_000),
            ..LIMITS
        };
        let began = Instant::now();
        let outline = walk(&tree, &window, &limits, &|| began.elapsed() > limits.deadline);
        let took = began.elapsed();
        assert_eq!(outline.truncated, Some("timeout"));
        assert!(outline.nodes > 0 && outline.nodes < 400, "{}", outline.nodes);
        assert!(took < Duration::from_secs(5), "{took:?}");
        // What it read before the deadline is kept.
        assert_eq!(outline.lines[0], "window");

        // The deadline is checked before every read: once it passes, nothing more is asked.
        let (tree, window) = deep(1, 400);
        let outline = walk(&tree, &window, &LIMITS, &|| tree.reads.borrow().len() >= 40);
        assert_eq!(outline.truncated, Some("timeout"));
        assert_eq!(tree.reads.borrow().len(), 40);
    }

    fn seen(t: u64, shot: &str, window_id: Option<u32>, app: &str) -> Observation {
        Observation {
            t,
            shot: shot.into(),
            window_id,
            pid: 1,
            bundle_id: Some(format!("dev.{}", app.to_lowercase().replace(' ', ""))),
            app: app.into(),
            ..Observation::default()
        }
    }

    fn entry(id: u32, app: &str, frontmost: bool) -> serde_json::Value {
        serde_json::json!({
            "id": id, "app": app, "bundleId": format!("dev.{}", app.to_lowercase().replace(' ', "")),
            "title": format!("{app} {id}"), "bounds": [0, 0, 800, 600], "layer": 0,
            "frontmost": frontmost
        })
    }

    /// A capture with keyframes at `keyframes`, clicks at `clicks` and the window log `windows`.
    fn bundle(
        mode: &str,
        keyframes: &[u64],
        clicks: &[u64],
        windows: serde_json::Value,
    ) -> ScrawlBundle {
        serde_json::from_value(serde_json::json!({
            "version": 1, "id": "scrawl-1-test", "createdAtMs": 0, "durationMs": 10_000,
            "target": {
                "mode": mode, "app": "Safari", "bundleId": "dev.safari", "windowId": 1,
                "displayId": 1,
                "frame": {"x": 0.0, "y": 0.0, "width": 1000.0, "height": 800.0, "scale": 2.0}
            },
            "transcript": {"source": "test", "text": "", "segments": []},
            "strokes": [], "marks": [],
            "events": clicks.iter().map(|t| serde_json::json!(
                {"kind": "click", "t": t, "x": 0.5, "y": 0.5}
            )).collect::<Vec<_>>(),
            "keyframes": keyframes.iter().enumerate().map(|(i, t)| serde_json::json!({
                "index": i + 1, "t": t, "reason": "scene_change",
                "file": format!("frames/frame-{:02}.png", i + 1), "event": "scene changed"
            })).collect::<Vec<_>>(),
            "contactSheet": "contact-sheet.png",
            "windows": windows
        }))
        .unwrap()
    }

    fn picks(bundle: &ScrawlBundle, observations: Vec<Observation>) -> Vec<Option<String>> {
        let timeline = Timeline {
            observations,
            ..Timeline::default()
        };
        match_keyframes(bundle, &timeline)
            .into_iter()
            .map(|pick| pick.map(|i| timeline.observations[i].shot.clone()))
            .collect()
    }

    #[test]
    fn appshot_match_never_gives_a_keyframe_another_windows_shot() {
        // Safari until 1,000 ms, then T3 Code.
        let log = serde_json::json!([
            {"t": 0, "windows": [entry(1, "Safari", true), entry(2, "T3 Code", false)]},
            {"t": 1_000, "windows": [entry(2, "T3 Code", true), entry(1, "Safari", false)]},
        ]);
        let b = bundle("display", &[1_100], &[], log.clone());
        assert_eq!(
            picks(&b, vec![seen(0, "safari", Some(1), "Safari")]),
            [None],
            "a T3 Code frame never gets Safari's shot"
        );
        assert_eq!(
            picks(
                &b,
                vec![
                    seen(0, "safari", Some(1), "Safari"),
                    seen(1_050, "t3", Some(2), "T3 Code")
                ]
            ),
            [Some("t3".into())]
        );
        // Two Safari windows: A in front until 1,050 ms, then B.
        let log = serde_json::json!([
            {"t": 0, "windows": [entry(1, "Safari", true), entry(3, "Safari", false)]},
            {"t": 1_050, "windows": [entry(3, "Safari", true), entry(1, "Safari", false)]},
        ]);
        let b = bundle("display", &[1_000], &[], log);
        assert_eq!(
            picks(&b, vec![seen(1_100, "window-b", Some(3), "Safari")]),
            [None],
            "window A's frame never gets window B's shot"
        );
        assert_eq!(
            picks(
                &b,
                vec![
                    seen(200, "window-a", Some(1), "Safari"),
                    seen(1_100, "window-b", Some(3), "Safari")
                ]
            ),
            [Some("window-a".into())]
        );
        // A shot of window A taken after the capture left it is not of this stay.
        let log = serde_json::json!([
            {"t": 0, "windows": [entry(1, "Safari", true)]},
            {"t": 1_050, "windows": [entry(3, "Safari", true)]},
            {"t": 2_000, "windows": [entry(1, "Safari", true)]},
        ]);
        let b = bundle("display", &[1_000], &[], log);
        assert_eq!(picks(&b, vec![seen(2_100, "later", Some(1), "Safari")]), [None]);
    }

    #[test]
    fn appshot_match_prefers_the_shot_taken_after_the_last_click() {
        let log = serde_json::json!([{"t": 0, "windows": [entry(1, "Safari", true)]}]);
        let shots = || {
            vec![
                seen(0, "before", Some(1), "Safari"),
                seen(1_300, "after", Some(1), "Safari"),
            ]
        };
        let b = bundle("display", &[1_250], &[1_000], log.clone());
        assert_eq!(picks(&b, shots()), [Some("after".into())]);
        // The shot before the click is nearer, and still loses.
        let b = bundle("display", &[1_100], &[1_000], log.clone());
        assert_eq!(
            picks(
                &b,
                vec![
                    seen(950, "before", Some(1), "Safari"),
                    seen(1_400, "after", Some(1), "Safari")
                ]
            ),
            [Some("after".into())]
        );
        // With no shot after the click, the one before it is still the window's.
        assert_eq!(
            picks(&b, vec![seen(950, "before", Some(1), "Safari")]),
            [Some("before".into())]
        );
        // Without a click the nearest wins.
        let b = bundle("display", &[400], &[], log.clone());
        assert_eq!(picks(&b, shots()), [Some("before".into())]);
        // A picture change counts like a click.
        let b = bundle("display", &[1_100], &[], log);
        let timeline = Timeline {
            observations: vec![
                seen(950, "before", Some(1), "Safari"),
                seen(1_400, "after", Some(1), "Safari"),
            ],
            scenes: vec![1_000],
            capped: false,
        };
        assert_eq!(match_keyframes(&b, &timeline), [Some(1)]);
    }

    #[test]
    fn appshot_match_follows_an_area_captures_top_window() {
        // An Area capture: the first normal window in the region, whatever is frontmost.
        let log = serde_json::json!([
            {"t": 0, "windows": [entry(1, "Safari", false), entry(2, "Xcode", false)]},
            {"t": 2_000, "windows": [entry(2, "Xcode", false), entry(1, "Safari", false)]},
        ]);
        let b = bundle("region", &[500, 2_500], &[], log);
        assert_eq!(
            picks(
                &b,
                vec![
                    seen(100, "safari", Some(1), "Safari"),
                    seen(2_100, "xcode", Some(2), "Xcode")
                ]
            ),
            [Some("safari".into()), Some("xcode".into())]
        );
        assert_eq!(expected_window(&b, 500).unwrap().title, "Safari 1");
        let xcode = expected_window(&b, 2_500).unwrap();
        assert_eq!((xcode.app.as_str(), xcode.from, xcode.until), ("Xcode", 2_000, u64::MAX));
    }

    #[test]
    fn appshot_match_leaves_a_keyframe_empty_rather_than_stale() {
        let log = serde_json::json!([{"t": 0, "windows": [entry(1, "Safari", true)]}]);
        let b = bundle("display", &[8_000], &[], log);
        assert_eq!(
            picks(&b, vec![seen(4_900, "old", Some(1), "Safari")]),
            [None],
            "a shot more than 3 s away is not this keyframe's"
        );
        assert_eq!(
            picks(&b, vec![seen(5_000, "near", Some(1), "Safari")]),
            [Some("near".into())]
        );
        // No window was on top (the desktop): nothing is expected.
        let empty = bundle(
            "display",
            &[100],
            &[],
            serde_json::json!([{"t": 0, "windows": [entry(1, "Safari", false)]}]),
        );
        assert_eq!(picks(&empty, vec![seen(0, "x", Some(1), "Safari")]), [None]);
        assert!(expected_window(&empty, 100).is_none());
    }

    #[test]
    fn appshot_match_uses_each_time_a_repeated_state_was_seen() {
        // The same state at 0 and at 6,000 ms is one shot seen twice.
        let log = serde_json::json!([{"t": 0, "windows": [entry(1, "Safari", true)]}]);
        let b = bundle("display", &[100, 6_100], &[], log);
        let observations = vec![
            seen(0, "home", Some(1), "Safari"),
            seen(3_000, "detail", Some(1), "Safari"),
            seen(6_000, "home", Some(1), "Safari"),
        ];
        assert_eq!(
            picks(&b, observations),
            [Some("home".into()), Some("home".into())]
        );
    }

    #[test]
    fn appshot_match_goes_by_app_for_a_shot_without_a_window_id() {
        let log = serde_json::json!([{"t": 0, "windows": [entry(1, "Safari", true)]}]);
        let b = bundle("display", &[100], &[], log);
        assert_eq!(
            picks(&b, vec![seen(0, "safari", None, "Safari")]),
            [Some("safari".into())]
        );
        assert_eq!(picks(&b, vec![seen(0, "other", None, "Xcode")]), [None]);
        // Neither side has a bundle id: the app's name decides.
        let mut nameless = seen(0, "safari", None, "Safari");
        nameless.bundle_id = None;
        assert_eq!(picks(&b, vec![nameless]), [Some("safari".into())]);
        // A Window capture expects its own window from start to end.
        let window = bundle("window", &[100, 9_000], &[], serde_json::json!([]));
        assert_eq!(
            picks(
                &window,
                vec![
                    seen(0, "mine", Some(1), "Safari"),
                    seen(8_900, "other", Some(2), "Safari")
                ]
            ),
            [Some("mine".into()), None]
        );
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "scrawl-appshot-{name}-{}-{}",
            std::process::id(),
            crate::now_epoch_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn session(dir: &Path) -> Session {
        Session {
            mode: CaptureMode::Display,
            window_id: None,
            region: TargetFrame {
                x: 0.0,
                y: 0.0,
                width: 1000.0,
                height: 800.0,
                scale: 2.0,
            },
            started_at: Instant::now(),
            bundle_dir: dir.to_path_buf(),
        }
    }

    /// Takes a shot named by a counter after `delay`, checking `dropped` every millisecond.
    struct SlowTaker {
        delay: Duration,
        /// Reads on to the end even when the capture no longer wants the shot, like a walk
        /// stuck in one long Accessibility message.
        heedless: bool,
        taken: Arc<AtomicU64>,
        released: Arc<AtomicU64>,
        /// Shots with the same state give the same tree.
        states: fn(u64) -> u64,
    }

    impl Taker for SlowTaker {
        fn take(&mut self, _: &Session, dropped: &dyn Fn() -> bool) -> Option<Taken> {
            let began = Instant::now();
            while began.elapsed() < self.delay {
                if dropped() && !self.heedless {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let n = self.taken.fetch_add(1, Ordering::AcqRel);
            Some(Taken {
                shot: Shot {
                    app: "Safari".into(),
                    window_id: Some(1),
                    nodes: 1,
                    tree: vec![format!("state {}", (self.states)(n))],
                    ..Shot::default()
                },
                pid: 42,
                frame: [0, 0, 800, 600],
            })
        }

        fn release(&mut self) {
            self.released.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn walker(delay: Duration, states: fn(u64) -> u64) -> (Walker, Arc<AtomicU64>, Arc<AtomicU64>) {
        walker_with(delay, false, states)
    }

    fn walker_with(
        delay: Duration,
        heedless: bool,
        states: fn(u64) -> u64,
    ) -> (Walker, Arc<AtomicU64>, Arc<AtomicU64>) {
        let taken = Arc::new(AtomicU64::new(0));
        let released = Arc::new(AtomicU64::new(0));
        let walker = Walker::start(SlowTaker {
            delay,
            heedless,
            taken: Arc::clone(&taken),
            released: Arc::clone(&released),
            states,
        });
        (walker, taken, released)
    }

    fn wait_for(what: impl Fn() -> bool) {
        let began = Instant::now();
        while !what() {
            assert!(began.elapsed() < Duration::from_secs(5), "timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn appshot_walker_spools_each_state_once_with_every_time_it_was_seen() {
        let dir = temp("spool");
        // States 0, 1, 0: the third shot is the first state again.
        let (walker, taken, released) = walker(Duration::ZERO, |n| n % 2);
        walker.begin(session(&dir));
        wait_for(|| taken.load(Ordering::Acquire) == 1);
        for n in 2..=3 {
            walker.request(Duration::ZERO);
            wait_for(|| taken.load(Ordering::Acquire) == n);
        }
        let spool = dir.join(DIR).join(SPOOL);
        wait_for(|| std::fs::read_dir(&spool).is_ok_and(|files| files.count() == 2));
        let shots = walker.finish().unwrap();
        assert_eq!(shots.timeline.observations.len(), 3);
        assert_eq!(
            shots.timeline.observations[0].shot,
            shots.timeline.observations[2].shot
        );
        assert_ne!(
            shots.timeline.observations[0].shot,
            shots.timeline.observations[1].shot
        );
        assert_eq!(shots.timeline.observations[0].pid, 42);
        wait_for(|| released.load(Ordering::Acquire) == 1);
        assert!(walker.finish().is_none(), "the capture is over");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appshot_walker_keeps_only_the_latest_request_and_waits_for_a_click_to_settle() {
        let dir = temp("latest");
        let (walker, taken, _) = walker(Duration::ZERO, |n| n);
        walker.begin(session(&dir));
        wait_for(|| taken.load(Ordering::Acquire) == 1);
        // Five clicks in a row are one shot, 300 ms after the last.
        let clicked = Instant::now();
        for _ in 0..5 {
            walker.request(SETTLE);
        }
        wait_for(|| taken.load(Ordering::Acquire) == 2);
        assert!(clicked.elapsed() >= SETTLE, "{:?}", clicked.elapsed());
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(taken.load(Ordering::Acquire), 2);
        // A picture change asks at most once per 2 s.
        walker.scene();
        walker.scene();
        wait_for(|| taken.load(Ordering::Acquire) == 3);
        std::thread::sleep(Duration::from_millis(50));
        let shots = walker.finish().unwrap();
        assert_eq!(taken.load(Ordering::Acquire), 3);
        assert_eq!(shots.timeline.scenes.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appshot_finish_waits_briefly_for_a_walk_and_drops_a_long_one() {
        // A 100 ms walk in flight lands.
        let dir = temp("finish-short");
        let (walker, _, _) = walker(Duration::from_millis(100), |n| n);
        walker.begin(session(&dir));
        std::thread::sleep(Duration::from_millis(30));
        let began = Instant::now();
        let shots = walker.finish().unwrap();
        assert_eq!(shots.timeline.observations.len(), 1);
        assert!(began.elapsed() < FINISH_WAIT);
        let _ = std::fs::remove_dir_all(&dir);

        // A 3 s walk is dropped after 400 ms.
        let dir = temp("finish-long");
        let (walker, _, _) = self::walker(Duration::from_secs(3), |n| n);
        walker.begin(session(&dir));
        std::thread::sleep(Duration::from_millis(30));
        let began = Instant::now();
        let shots = walker.finish().unwrap();
        let waited = began.elapsed();
        assert!(shots.timeline.observations.is_empty());
        assert!(
            waited >= FINISH_WAIT && waited < FINISH_WAIT + Duration::from_millis(250),
            "{waited:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appshot_a_walk_that_ends_late_never_lands_in_the_next_capture() {
        // The first capture's walk reads on for 900 ms, past its finish and into the next
        // capture, and comes back with a shot.
        let first = temp("late-first");
        let (walker, taken, _) = walker_with(Duration::from_millis(900), true, |n| n);
        walker.begin(session(&first));
        std::thread::sleep(Duration::from_millis(50));
        assert!(walker.finish().unwrap().timeline.observations.is_empty());
        let next = temp("late-next");
        walker.begin(session(&next));
        wait_for(|| taken.load(Ordering::Acquire) == 1);
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !next.join(DIR).exists() && !first.join(DIR).exists(),
            "the late shot was dropped"
        );
        // The next capture's own first shot is the second one taken.
        wait_for(|| taken.load(Ordering::Acquire) == 2);
        let shots = walker.finish().unwrap();
        assert_eq!(shots.timeline.observations.len(), 1);
        let shot = shots.shot(&shots.timeline.observations[0].shot).unwrap();
        assert_eq!(shot.tree, ["state 1"]);
        for dir in [first, next] {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn appshot_cancel_drops_the_walk_at_once_and_deletes_the_spool() {
        let dir = temp("cancel");
        let (walker, taken, released) = walker(Duration::ZERO, |n| n);
        walker.begin(session(&dir));
        wait_for(|| taken.load(Ordering::Acquire) == 1);
        wait_for(|| dir.join(DIR).join(SPOOL).exists());
        walker.cancel();
        assert!(!dir.join(DIR).exists(), "the spool goes with the capture");
        wait_for(|| released.load(Ordering::Acquire) == 1);
        // A walk in flight costs a cancel no wait.
        let (walker, _, _) = self::walker(Duration::from_secs(3), |n| n);
        walker.begin(session(&dir));
        std::thread::sleep(Duration::from_millis(30));
        let began = Instant::now();
        walker.cancel();
        assert!(began.elapsed() < Duration::from_millis(100));
        walker.request(Duration::ZERO);
        assert!(!walker.is_live());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A shot whose file is far larger than a pipe holds (64 KB).
    fn big_shot() -> Shot {
        Shot {
            app: "Safari".into(),
            window_id: Some(1),
            nodes: 1,
            tree: vec!["x".repeat(1024 * 1024)],
            ..Shot::default()
        }
    }

    struct BigTaker(Arc<AtomicU64>);

    impl Taker for BigTaker {
        fn take(&mut self, _: &Session, _: &dyn Fn() -> bool) -> Option<Taken> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Some(Taken {
                shot: big_shot(),
                pid: 42,
                frame: [0, 0, 800, 600],
            })
        }
    }

    /// A disk that stalls: the spool file of `big_shot` is a named pipe that nothing reads for
    /// `stall`, so the write of the shot blocks that long. Returns the pipe's path and the
    /// thread that then reads it to its end.
    #[cfg(unix)]
    fn stalled_spool_file(dir: &Path, stall: Duration) -> (PathBuf, std::thread::JoinHandle<()>) {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        use std::os::unix::io::AsRawFd as _;
        let spool = dir.join(DIR).join(SPOOL);
        std::fs::create_dir_all(&spool).unwrap();
        let path = spool.join(format!("{}.json", big_shot().key()));
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        // Open for reading now, so the writer's open succeeds and its write is what blocks.
        let mut pipe = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        let reader = std::thread::spawn(move || {
            use std::io::Read as _;
            std::thread::sleep(stall);
            let fd = pipe.as_raw_fd();
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
            let _ = pipe.read_to_end(&mut Vec::new());
        });
        (path, reader)
    }

    #[cfg(unix)]
    #[test]
    fn appshot_a_stalled_spool_write_holds_up_neither_finish_nor_cancel() {
        let stall = Duration::from_millis(900);
        for cancel in [false, true] {
            let dir = temp("stalled");
            let (file, reader) = stalled_spool_file(&dir, stall);
            let taken = Arc::new(AtomicU64::new(0));
            let walker = Walker::start(BigTaker(Arc::clone(&taken)));
            walker.begin(session(&dir));
            wait_for(|| taken.load(Ordering::Acquire) == 1);
            // The walker is now writing the shot, and the write is stalled.
            std::thread::sleep(Duration::from_millis(50));
            let began = Instant::now();
            if cancel {
                walker.cancel();
                let waited = began.elapsed();
                assert!(waited < Duration::from_millis(100), "cancel took {waited:?}");
            } else {
                let shots = walker.finish().unwrap();
                let waited = began.elapsed();
                assert!(
                    waited < FINISH_WAIT + Duration::from_millis(250),
                    "finish took {waited:?}"
                );
                assert!(shots.timeline.observations.is_empty());
            }
            // The write ends after the capture did: its shot lands nowhere and leaves no file.
            reader.join().unwrap();
            wait_for(|| !file.exists() && !dir.join(DIR).exists());
            assert!(!walker.is_live());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn appshot_takes_nothing_while_the_recording_is_paused() {
        let dir = temp("paused");
        let (walker, taken, _) = walker(Duration::from_millis(400), |n| n);
        walker.begin(session(&dir));
        wait_for(|| taken.load(Ordering::Acquire) == 1);
        // A walk in flight when the pause begins is dropped.
        walker.request(Duration::ZERO);
        std::thread::sleep(Duration::from_millis(20));
        walker.set_paused(true);
        walker.request(Duration::ZERO);
        walker.scene();
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(taken.load(Ordering::Acquire), 1);
        // Resuming takes one at once.
        walker.set_paused(false);
        wait_for(|| taken.load(Ordering::Acquire) == 2);
        let shots = walker.finish().unwrap();
        assert_eq!(shots.timeline.observations.len(), 2);
        assert!(shots.timeline.scenes.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appshot_pauses_are_cut_out_of_the_shot_times() {
        let mut shots = Shots {
            dir: PathBuf::new(),
            timeline: Timeline {
                observations: vec![
                    seen(500, "a", Some(1), "Safari"),
                    seen(1_500, "paused", Some(1), "Safari"),
                    seen(3_200, "b", Some(1), "Safari"),
                ],
                scenes: vec![400, 1_200, 3_100],
                capped: false,
            },
        };
        shots.cut(&crate::pause::Cuts::new(vec![crate::pause::Span {
            start_ms: 1_000,
            end_ms: 3_000,
        }]));
        let times: Vec<(u64, &str)> = shots
            .timeline
            .observations
            .iter()
            .map(|seen| (seen.t, seen.shot.as_str()))
            .collect();
        assert_eq!(times, [(500, "a"), (1_200, "b")]);
        assert_eq!(shots.timeline.scenes, [400, 1_100]);
    }

    #[test]
    fn appshot_spool_stops_at_its_cap_and_says_so() {
        let dir = temp("cap");
        let mut spool = Spool::new(&dir);
        let big = |n: usize| Taken {
            shot: Shot {
                app: "Safari".into(),
                tree: vec![format!("{n} {}", "x".repeat(1024 * 1024))],
                ..Shot::default()
            },
            pid: 1,
            frame: [0; 4],
        };
        for n in 0..40 {
            spool.add(n as u64, 1, big(n));
        }
        assert!(spool.capped);
        assert!(spool.bytes <= SPOOL_CAP);
        let kept = spool.observations.len();
        assert!((30..32).contains(&kept), "{kept}");
        // A state seen before the cap is still noted when it comes back.
        spool.add(99, 1, big(0));
        assert_eq!(spool.observations.len(), kept + 1);
        assert!(spool.into_shots().timeline.capped);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appshot_store_numbers_the_shots_and_gives_each_keyframe_its_file() {
        let dir = temp("store");
        let mut spool = Spool::new(&dir);
        let state = |name: &str| Taken {
            shot: Shot {
                app: "Safari".into(),
                bundle_id: Some("dev.safari".into()),
                window_id: Some(1),
                nodes: 1,
                tree: vec![format!("window \"{name}\"")],
                ..Shot::default()
            },
            pid: 1,
            frame: [0, 0, 800, 600],
        };
        spool.add(0, 5, state("home"));
        spool.add(3_000, 5, state("detail"));
        spool.add(6_000, 5, state("home"));
        let log = serde_json::json!([{"t": 0, "windows": [entry(1, "Safari", true)]}]);
        let mut b = bundle("display", &[100, 3_100, 9_900], &[], log);
        spool.into_shots().store(&dir, &mut b).unwrap();
        let files: Vec<Option<&str>> = b.keyframes.iter().map(|k| k.appshot.as_deref()).collect();
        assert_eq!(files, [Some("shot-01.json"), Some("shot-02.json"), None]);
        let shots = Shots::load(&dir).unwrap();
        assert_eq!(shots.timeline.observations.len(), 3);
        assert_eq!(shots.timeline.observations[2].shot, "shot-01.json");
        assert_eq!(shots.shot("shot-02.json").unwrap().tree, ["window \"detail\""]);
        let mut names: Vec<String> = std::fs::read_dir(dir.join(DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["shot-01.json", "shot-02.json", "timeline.json"]);

        // Stored shots go into another bundle unchanged (`scrawl reprocess`).
        let copy = temp("store-copy");
        let mut again = b.clone();
        for keyframe in &mut again.keyframes {
            keyframe.appshot = None;
        }
        Shots::load(&dir).unwrap().store(&copy, &mut again).unwrap();
        assert_eq!(again.keyframes, b.keyframes);
        assert_eq!(Shots::load(&copy).unwrap().timeline, shots.timeline);

        // A capture with no shot gets no folder.
        let empty = temp("store-empty");
        Spool::new(&empty).into_shots().store(&empty, &mut b).unwrap();
        assert!(!empty.join(DIR).exists());
        for dir in [dir, copy, empty] {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A stored capture: Safari on the fixture for keyframe 1, T3 Code for keyframe 2 (its tree
    /// cut at the node cap), and a keyframe 3 on Xcode with no shot.
    fn stored() -> (PathBuf, ScrawlBundle) {
        let dir = temp("cli");
        let mut spool = Spool::new(&dir);
        let (tree, window) = fixture();
        let safari = shot_of(
            "Safari",
            Some("dev.safari"),
            Some(1),
            "Fixture",
            Some("file:///Users/kit/annotator/tests/fixtures/appshot-secret.html?token=SECRET-MARKER-URL"),
            walk(&tree, &window, &LIMITS, &never),
        );
        spool.add(3_700, 12, Taken { shot: safari, pid: 1, frame: [0, 0, 800, 600] });
        let t3 = Shot {
            app: "T3 Code".into(),
            bundle_id: Some("dev.t3code".into()),
            window_id: Some(2),
            title: "Annotator plan".into(),
            nodes: 1_500,
            truncated: Some("nodes".into()),
            tree: vec!["window \"Annotator plan\"".into()],
            ..Shot::default()
        };
        spool.add(19_800, 310, Taken { shot: t3, pid: 2, frame: [0, 0, 800, 600] });
        let log = serde_json::json!([
            {"t": 0, "windows": [entry(1, "Safari", true)]},
            {"t": 15_000, "windows": [entry(2, "T3 Code", true)]},
            {"t": 30_000, "windows": [entry(3, "Xcode", true)]},
        ]);
        let mut b = bundle("display", &[4_000, 19_000, 31_000], &[], log);
        spool.into_shots().store(&dir, &mut b).unwrap();
        (dir, b)
    }

    #[test]
    fn appshot_cli_lists_each_keyframes_shot_without_its_tree() {
        let (dir, b) = stored();
        let (out, code) = report(&dir, &b, None, false);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "\
#  time  app      window          url                      age  nodes
1  0:04  Safari   Fixture         …/appshot-secret.html  0.3 s      8
2  0:19  T3 Code  Annotator plan  -                      0.8 s  1,500
   cut: nodes
3  0:31  Xcode    Xcode 3         -                          -      -
   no app shot of this window
"
        );
        assert!(!out.contains("button"), "the list holds no tree");
        let (out, code) = report(&dir, &b, None, true);
        assert_eq!(code, 0);
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json["capture"], "scrawl-1-test");
        let rows = json["keyframes"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["url"], "file:///Users/kit/annotator/tests/fixtures/appshot-secret.html");
        assert_eq!(rows[0]["ageMs"], 300);
        assert_eq!(rows[0]["tookMs"], 12);
        assert_eq!(rows[1]["truncated"], "nodes");
        assert_eq!(rows[1]["nodes"], 1_500);
        assert_eq!(rows[2]["app"], "Xcode");
        assert!(rows[2].get("shot").is_none());
        assert!(!out.contains("tree") && !out.contains("button"), "metadata only: {out}");
        assert!(!out.contains(MARKER));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appshot_cli_prints_one_keyframes_outline() {
        let (dir, b) = stored();
        let (out, code) = report(&dir, &b, Some(1), false);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "\
Safari · “Fixture” · …/appshot-secret.html · 0:04 · 8 nodes
window \"Fixture\"
  web area
    heading \"Settings\"
    static text \"Open the settings to change your keys.\"
    text field \"Name\"
    secure text field \"Password\"
    link \"Reset https://a.dev/reset\"
    button \"Save\" #save
"
        );
        assert!(!out.contains(MARKER));
        let (out, _) = report(&dir, &b, Some(2), false);
        assert!(
            out.starts_with("T3 Code · “Annotator plan” · 0:19 · 1,500 nodes · cut: nodes\n"),
            "{out}"
        );
        let (out, code) = report(&dir, &b, Some(1), true);
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(code, 0);
        assert_eq!(json["keyframe"]["app"], "Safari");
        assert_eq!(json["tree"].as_array().unwrap().len(), 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appshot_cli_says_what_is_missing_and_exits_2_only_for_a_bad_frame() {
        let (dir, b) = stored();
        // A keyframe with no shot names the window it expected.
        let (out, code) = report(&dir, &b, Some(3), false);
        assert_eq!(
            (out.as_str(), code),
            ("no app shot for keyframe 3 (0:31): expected Xcode “Xcode 3”\n", 0)
        );
        // Out of range, at both ends.
        for n in [0, 4, 99] {
            let (out, code) = report(&dir, &b, Some(n), false);
            assert_eq!(code, 2);
            assert_eq!(
                out,
                format!("--frame {n} is out of range: this capture has keyframes 1 to 3\n")
            );
        }
        // A capture made with app state off, or on another system.
        let bare = temp("cli-bare");
        for frame in [None, Some(1), Some(99)] {
            assert_eq!(
                report(&bare, &b, frame, false),
                ("no app shots in this capture\n".to_owned(), 0)
            );
        }
        for dir in [dir, bare] {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn appshot_spools_of_unfinished_captures_are_swept_at_launch() {
        let captures = temp("sweep");
        let spool_of = |name: &str| {
            let dir = captures.join(name);
            std::fs::create_dir_all(dir.join(DIR).join(SPOOL)).unwrap();
            std::fs::write(dir.join(DIR).join(SPOOL).join("a.json"), "{}").unwrap();
            dir
        };
        let crashed = spool_of("crashed.scrawl");
        let finished = spool_of("finished.scrawl");
        std::fs::write(finished.join("scrawl.json"), "{}").unwrap();
        std::fs::write(finished.join(DIR).join(TIMELINE), "{}").unwrap();
        sweep_spools(&captures);
        assert!(!crashed.join(DIR).exists());
        assert!(crashed.exists(), "the capture's folder and media stay");
        assert!(finished.join(DIR).join(TIMELINE).exists());
        let _ = std::fs::remove_dir_all(&captures);
    }
}
