//! The app-state text of the paste (`paste_xml`): what happened during a capture as event lines
//! (clicks, drags, shortcuts, windows opening and closing, app events), and what each mark is.
//! Marks sharing a keyframe are numbered ①②③ in drawing order for `scrawl zoom --mark` and
//! `scrawl inspect --marks`. Nothing here gives raw coordinates: the images show where, so a
//! place is only written when something names it (an Accessibility label, the dev page's element).

use crate::model::*;
use crate::sketch::{
    describe_event, direction, mark_on_keyframe, marks_on, quote_label, settled_events,
};

/// An unnamed drag shorter than this (normalized) says nothing the images don't.
const MIN_DRAG: f32 = 0.15;

/// How close a drag is to the dev page's report of a video scrub it caused.
const SCRUB_MS: u64 = 1_500;

/// Repeats of one thing closer together than this read as one line (`pressed ⌘T ×3`).
const REPEAT_MS: u64 = 3_000;

/// More on-screen windows than this changing at once (a Space switch) is one line.
const WINDOW_BURST: usize = 4;

/// The circled number for the `n`th mark (1-based) on a shared keyframe.
pub fn circled(n: usize) -> String {
    const DIGITS: [char; 20] = [
        '①', '②', '③', '④', '⑤', '⑥', '⑦', '⑧', '⑨', '⑩', '⑪', '⑫', '⑬', '⑭', '⑮', '⑯', '⑰', '⑱',
        '⑲', '⑳',
    ];
    DIGITS
        .get(n.wrapping_sub(1))
        .map_or_else(|| format!("({n})"), char::to_string)
}

/// A mark as the text names it: on keyframe `#keyframe`, the `number`th of its marks in drawing
/// order (shown as ①②③ only when `numbered`, i.e. the keyframe draws two or more).
#[derive(Clone, Debug)]
pub struct MarkEntry<'a> {
    pub mark: &'a Mark,
    /// 1-based keyframe index, when a keyframe draws the mark.
    pub keyframe: Option<usize>,
    pub number: usize,
    pub numbered: bool,
}

impl MarkEntry<'_> {
    /// `#2 ✎② 0:09 marked (2 strokes) · m2`
    pub fn line(&self) -> String {
        let frame = self.keyframe.map_or("#-".to_owned(), |k| format!("#{k}"));
        let num = self.numbered.then_some(self.number);
        format!("{frame} {} · {}", mark_label(self.mark, num), self.mark.id)
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "keyframe": self.keyframe,
            "number": self.number,
            "numbered": self.numbered,
            "id": self.mark.id,
            "t0": self.mark.t0,
            "t1": self.mark.t1,
            "time": format_clock(self.mark.t0),
            "shape": self.mark.shape,
            "on": self.mark.on,
            "strokes": self.mark.strokes.len(),
            "text": mark_text(self.mark),
            "frame": self.mark.frame,
        })
    }
}

/// Every mark, in time order, numbered the way the summary numbers them.
pub fn mark_entries(bundle: &ScrawlBundle) -> Vec<MarkEntry<'_>> {
    let mut entries: Vec<MarkEntry> = bundle
        .marks
        .iter()
        .map(|mark| {
            let frame = bundle.keyframes.iter().find(|k| mark_on_keyframe(mark, k));
            let shown = frame
                .map(|k| marks_on(k, &bundle.marks))
                .unwrap_or_default();
            let pos = shown.iter().position(|m| std::ptr::eq(*m, mark));
            MarkEntry {
                mark,
                keyframe: frame.map(|k| k.index),
                number: pos.map_or(1, |n| n + 1),
                numbered: pos.is_some() && shown.len() >= 2,
            }
        })
        .collect();
    entries.sort_by_key(|e| e.mark.t0);
    entries
}

/// The mark `query` names: its id (`m2`), or its number as the text shows it (`2` or `②`), on
/// keyframe `keyframe` when given. A number on several keyframes needs the keyframe; numbers
/// shown as ①②③ win over single, unnumbered marks.
pub fn find_mark<'a>(
    bundle: &'a ScrawlBundle,
    query: &str,
    keyframe: Option<usize>,
) -> Result<MarkEntry<'a>, String> {
    let entries = mark_entries(bundle);
    let query = query.trim();
    if let Some(entry) = entries.iter().find(|e| e.mark.id == query) {
        return Ok(entry.clone());
    }
    let number = query
        .parse::<usize>()
        .ok()
        .or_else(|| (1..=20).find(|&n| circled(n) == query))
        .ok_or_else(|| format!("“{query}” is neither a mark number nor a mark id"))?;
    let on_frame: Vec<&MarkEntry> = entries
        .iter()
        .filter(|e| e.number == number && keyframe.is_none_or(|k| e.keyframe == Some(k)))
        .collect();
    let shown: Vec<&MarkEntry> = on_frame.iter().copied().filter(|e| e.numbered).collect();
    let candidates = if shown.is_empty() { on_frame } else { shown };
    match candidates.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(match keyframe {
            Some(k) => format!("Keyframe #{k} has no mark {}", circled(number)),
            None => format!("No mark {} in this bundle", circled(number)),
        }),
        many => Err(format!(
            "Mark {} is on several keyframes ({}); add --frame <N> or use the mark id",
            circled(number),
            many.iter().map(|e| e.line()).collect::<Vec<_>>().join("; ")
        )),
    }
}

/// Everything besides marks that the summary shows inline, in time order, with repeats of one
/// thing (a shortcut, a click on one control) folded into `×N` and a run of drags into one line.
pub(crate) fn events(bundle: &ScrawlBundle) -> Vec<(u64, String)> {
    let settled = settled_events(&bundle.events);
    // On a Mac capture these come from the dev page (`crate::page`), not an app.
    let page_scrubs: Vec<u64> = settled
        .iter()
        .filter_map(|e| match e {
            InteractionEvent::AppEvent { t, label }
                if bundle.device.is_none() && label.contains(" scrubbed to ") =>
            {
                Some(*t)
            }
            _ => None,
        })
        .collect();
    let mut out: Vec<(u64, String)> = Vec::new();
    let mut drags: Option<DragRun> = None;
    for event in &settled {
        if let InteractionEvent::Drag { t, t1, .. } = event {
            let reported = page_scrubs
                .iter()
                .any(|&s| s + SCRUB_MS >= *t && s <= t1 + SCRUB_MS);
            if let Some(drag) = drag_text(event).filter(|_| !reported) {
                match &mut drags {
                    Some(run) if run.joins(&drag, *t) => run.add(drag, *t1),
                    _ => {
                        out.extend(drags.take().map(DragRun::line));
                        drags = Some(DragRun::new(drag, *t, *t1));
                    }
                }
            }
            continue;
        }
        let line = match event {
            InteractionEvent::AppEvent { t, label } if bundle.device.is_none() => {
                Some((*t, label.clone()))
            }
            // Window titles stay in scrawl.json; the summary names apps only.
            InteractionEvent::AppChange { t, app, .. } => Some((*t, format!("switched to {app}"))),
            other => event_text(other).map(|text| (other.timestamp_ms(), text)),
        };
        out.extend(line);
    }
    out.extend(drags.map(DragRun::line));
    // Area captures log windows too (for their apps list), but the summary keeps its
    // Screen-only window lines.
    if bundle.target.mode == crate::model::CaptureMode::Display {
        out.extend(window_changes(&bundle.windows));
    }
    out.extend(bundle.context_changes.iter().map(|change| {
        let text = match &change.value {
            Some(value) => format!("{} → {}", one_line(&change.key), one_line(value)),
            None => format!("{} cleared", one_line(&change.key)),
        };
        (change.t, text)
    }));
    out.sort_by_key(|(t, _)| *t);
    fold_repeats(out)
}

fn fold_repeats(items: Vec<(u64, String)>) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String, u64, usize)> = Vec::new();
    for (t, text) in items {
        match out.last_mut() {
            Some((_, prev, last, count)) if *prev == text && t - *last < REPEAT_MS => {
                *last = t;
                *count += 1;
            }
            _ => out.push((t, text, t, 1)),
        }
    }
    out.into_iter()
        .map(|(t, text, _, count)| {
            if count > 1 {
                (t, format!("{text} ×{count}"))
            } else {
                (t, text)
            }
        })
        .collect()
}

/// A drag worth a line: one that names what it moved, or an unnamed one that went far.
#[derive(Clone, Debug, PartialEq)]
struct Drag {
    verb: &'static str,
    name: Option<String>,
    direction: String,
}

fn drag_text(event: &InteractionEvent) -> Option<Drag> {
    let InteractionEvent::Drag {
        x0,
        y0,
        x1,
        y1,
        label,
        button,
        ..
    } = event
    else {
        return None;
    };
    let label = label.as_deref().map(str::trim).filter(|l| !l.is_empty());
    if let Some(rest) = label.and_then(|l| l.strip_prefix("long press")) {
        let name = rest.trim_start_matches(':').trim();
        return Some(Drag {
            verb: "long-pressed",
            name: (!name.is_empty()).then(|| name.to_owned()),
            direction: String::new(),
        });
    }
    let touch = button.as_deref() == Some("touch");
    let (dx, dy) = (x1 - x0, y1 - y0);
    match label {
        Some(name) => {
            let lower = name.to_lowercase();
            let scrub = ["slider", "scroll bar", "scrubber", "timeline", "progress"]
                .iter()
                .any(|word| lower.contains(word));
            Some(Drag {
                verb: if scrub {
                    "scrubbed"
                } else if touch {
                    "swiped"
                } else {
                    "dragged"
                },
                name: Some(name.to_owned()),
                direction: direction(dx, dy, 0.04),
            })
        }
        None if dx.hypot(dy) >= MIN_DRAG => Some(Drag {
            verb: if touch { "swiped" } else { "dragged" },
            name: None,
            direction: direction(dx, dy, 0.04),
        }),
        None => None,
    }
}

/// Drags of one thing in quick succession, as one line.
struct DragRun {
    t: u64,
    end: u64,
    drags: Vec<Drag>,
}

impl DragRun {
    fn new(drag: Drag, t: u64, end: u64) -> Self {
        Self {
            t,
            end,
            drags: vec![drag],
        }
    }

    fn joins(&self, drag: &Drag, t: u64) -> bool {
        let first = &self.drags[0];
        first.verb == drag.verb && first.name == drag.name && t <= self.end + REPEAT_MS
    }

    fn add(&mut self, drag: Drag, end: u64) {
        self.end = self.end.max(end);
        self.drags.push(drag);
    }

    fn line(self) -> (u64, String) {
        let first = &self.drags[0];
        let mut text = first.verb.to_owned();
        if let Some(name) = &first.name {
            text.push(' ');
            text.push_str(&quote_label(name));
        }
        let count = self.drags.len();
        let same_way = self.drags.iter().all(|d| d.direction == first.direction);
        // A scrub's direction and count say nothing; the dev page reports where it landed.
        if first.verb != "scrubbed" && first.verb != "long-pressed" {
            if same_way && first.direction != "a little" {
                text.push(' ');
                text.push_str(&first.direction);
            } else if !same_way {
                text.push_str(" back and forth");
            }
        }
        if count > 1 && first.verb != "scrubbed" {
            text.push_str(&format!(" ×{count}"));
        }
        (self.t, text)
    }
}

/// A click, scroll, point or key press without coordinates; `None` for a point nothing names.
fn event_text(event: &InteractionEvent) -> Option<String> {
    match event {
        InteractionEvent::Click {
            label,
            button,
            role,
            ..
        } => {
            let mut text = if button.as_deref() == Some("touch") {
                "tapped"
            } else {
                "clicked"
            }
            .to_owned();
            if let Some(role) = role.as_deref().filter(|r| !r.is_empty()) {
                text.push(' ');
                text.push_str(role);
            }
            if let Some(label) = label.as_deref().filter(|l| !l.is_empty()) {
                text.push(' ');
                text.push_str(&quote_label(label));
            }
            Some(text)
        }
        InteractionEvent::Point { label, .. } => label
            .as_deref()
            .filter(|l| !l.is_empty())
            .map(|l| format!("pointed at {}", quote_label(l))),
        other => Some(describe_event(other)),
    }
}

/// What a mark is: its shape, what it was drawn on when something names that, and its stroke
/// count when more than one.
pub(crate) fn mark_text(mark: &Mark) -> String {
    let mut text = match mark.shape {
        Shape::Circle => "circled".to_owned(),
        Shape::Underline => "underlined".to_owned(),
        Shape::Line => "drew a line".to_owned(),
        Shape::Arrow => "drew an arrow".to_owned(),
        Shape::Pin => format!("pin {}", mark.pin.unwrap_or(1)),
        Shape::Tap => "tapped".to_owned(),
        Shape::Mark => "marked".to_owned(),
        Shape::Flag => "flagged this moment".to_owned(),
    };
    if let Some(on) = mark.on.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        text.push_str(match mark.shape {
            Shape::Arrow => " pointing to ",
            Shape::Pin | Shape::Line => " on ",
            _ => " ",
        });
        text.push_str(&quote_label(on));
    }
    if mark.strokes.len() > 1 {
        text.push_str(&format!(" ({} strokes)", mark.strokes.len()));
    }
    text
}

/// A keyframe's own description without its `(x 0.50, y 0.70)` positions.
pub(crate) fn without_positions(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(" (x ") {
        out.push_str(&rest[..start]);
        match rest[start..].find(')') {
            Some(end) => rest = &rest[start + end + 1..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn one_line(raw: &str) -> String {
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= 60 {
        compact
    } else {
        let cut: String = compact.chars().take(59).collect();
        format!("{}…", cut.trim_end())
    }
}

/// Ordinary app windows appearing and disappearing between window-log snapshots, by app name.
fn window_changes(snapshots: &[WindowSnapshot]) -> Vec<(u64, String)> {
    let normal = |snapshot: &WindowSnapshot| -> Vec<(u32, String)> {
        snapshot
            .windows
            .iter()
            .filter(|w| w.layer == 0 && w.regular && w.app != "Annotator")
            .map(|w| (w.id, w.app.clone()))
            .collect()
    };
    let mut out = Vec::new();
    for pair in snapshots.windows(2) {
        let (before, after) = (normal(&pair[0]), normal(&pair[1]));
        let opened: Vec<&String> = after
            .iter()
            .filter(|(id, _)| !before.iter().any(|(b, _)| b == id))
            .map(|(_, app)| app)
            .collect();
        let closed: Vec<&String> = before
            .iter()
            .filter(|(id, _)| !after.iter().any(|(a, _)| a == id))
            .map(|(_, app)| app)
            .collect();
        let t = pair[1].t;
        if opened.len() + closed.len() > WINDOW_BURST {
            out.push((
                t,
                format!(
                    "on-screen windows changed ({} opened, {} closed)",
                    opened.len(),
                    closed.len()
                ),
            ));
            continue;
        }
        for app in closed {
            out.push((t, format!("{app} window closed")));
        }
        for app in opened {
            out.push((t, format!("{app} window opened")));
        }
    }
    out
}

fn mark_label(mark: &Mark, number: Option<usize>) -> String {
    let n = number.map(circled).unwrap_or_default();
    format!("✎{n} {} {}", format_clock(mark.t0), mark_text(mark))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::WindowEntry;

    fn stroke(t0: u64, x: f32, y: f32) -> Stroke {
        let p = |v: f32| (v * SCALE as f32) as i32;
        Stroke {
            t0,
            t1: t0 + 400,
            fw: 1000,
            fh: 1000,
            on: None,
            pin: None,
            laser: false,
            pts: vec![p(x), p(y), 0, p(x + 0.05), p(y), 200, p(x + 0.1), p(y), 400],
        }
    }

    fn words(list: &[(u64, &str)]) -> Vec<WordTiming> {
        list.iter()
            .map(|&(t0, text)| WordTiming {
                t0,
                t1: t0 + 250,
                text: text.into(),
            })
            .collect()
    }

    fn keyframe(index: usize, t: u64, reason: KeyframeReason, said: &str) -> Keyframe {
        Keyframe {
            index,
            t,
            reason,
            file: format!("frames/frame-{index:02}.png"),
            event: "scene changed".into(),
            said: (!said.is_empty()).then(|| said.into()),
            mark_id: None,
            cursor: None,
            appshot: None,
        }
    }

    fn bundle(words: Vec<WordTiming>, keyframes: Vec<Keyframe>) -> ScrawlBundle {
        ScrawlBundle {
            version: 1,
            id: "t".into(),
            created_at_ms: 0,
            duration_ms: 20_000,
            target: TargetInfo {
                mode: CaptureMode::Display,
                app: None,
                bundle_id: None,
                window_title: None,
                window_id: None,
                display_id: 1,
                frame: TargetFrame {
                    x: 0.0,
                    y: 0.0,
                    width: 1000.0,
                    height: 1000.0,
                    scale: 1.0,
                },
            },
            transcript: TranscriptData {
                source: "test".into(),
                words,
                ..Default::default()
            },
            note: None,
            strokes: Vec::new(),
            marks: Vec::new(),
            events: Vec::new(),
            keyframes,
            contact_sheet: "contact-sheet.png".into(),
            video: None,
            video_offset_ms: 0,
            audio: None,
            source: None,
            device: None,
            context: Default::default(),
            context_changes: Vec::new(),
            screens: Vec::new(),
            windows: Vec::new(),
            cursor: Vec::new(),
            autocrop: None,
            destination: None,
        }
    }

    #[test]
    fn marks_are_found_by_their_number_in_the_text_or_their_id() {
        let mut b = bundle(
            Vec::new(),
            vec![
                keyframe(1, 3_000, KeyframeReason::Mark, ""),
                keyframe(2, 9_000, KeyframeReason::Mark, ""),
                keyframe(3, 15_000, KeyframeReason::Mark, ""),
            ],
        );
        b.strokes = vec![
            stroke(1_000, 0.1, 0.2),
            stroke(3_000, 0.4, 0.5),
            stroke(7_000, 0.6, 0.1),
            stroke(9_000, 0.2, 0.8),
            stroke(14_000, 0.5, 0.5),
        ];
        b.marks = crate::sketch::marks_of(&b.strokes);
        assert_eq!(b.marks.len(), 5);
        for (mark, frame) in b.marks.iter_mut().zip([1, 1, 2, 2, 3]) {
            mark.frame = Some(format!("frames/frame-{frame:02}.png"));
        }
        for (k, first) in [(0, 0), (1, 2), (2, 4)] {
            b.keyframes[k].mark_id = Some(b.marks[first].id.clone());
        }
        let found = |q: &str, k: Option<usize>| find_mark(&b, q, k).map(|e| e.mark.t0);
        // ② is on #1 and #2: ambiguous without a keyframe.
        assert!(found("2", None).unwrap_err().contains("--frame"));
        assert_eq!(found("2", Some(2)), Ok(9_000));
        assert_eq!(found("②", Some(1)), Ok(3_000));
        // #3's single mark is unnumbered; --mark 1 --frame 3 still reaches it.
        assert_eq!(found("1", Some(3)), Ok(14_000));
        assert!(found("3", None).is_err());
        let id = b.marks[3].id.clone();
        assert_eq!(found(&id, None), Ok(9_000));
        let entry = find_mark(&b, "2", Some(2)).unwrap();
        assert!(entry.line().starts_with("#2 ✎② 0:09 "), "{}", entry.line());
        assert_eq!(entry.json()["keyframe"], 2);
    }

    #[test]
    fn shortcuts_drags_and_windows_are_events_without_titles_or_jitter() {
        let said = "Okay, I hit command shift B. The whole window closes.";
        let mut b = bundle(
            words(&[
                (500, "Okay,"),
                (900, "I"),
                (1_100, "hit"),
                (1_300, "command"),
                (1_600, "shift"),
                (1_900, "B."),
                (5_000, "The"),
                (5_200, "whole"),
                (5_400, "window"),
                (5_600, "closes."),
            ]),
            vec![keyframe(1, 1_000, KeyframeReason::Click, said)],
        );
        let key = |t| InteractionEvent::Key {
            t,
            shortcut: "⇧⌘B".into(),
        };
        let drag = |t, x1: f32| InteractionEvent::Drag {
            t,
            t1: t + 300,
            x0: 0.5,
            y0: 0.5,
            x1,
            y1: 0.5,
            label: None,
            button: None,
            path: Vec::new(),
        };
        b.events = vec![
            key(2_000),
            key(2_400),
            key(2_900),
            drag(3_000, 0.503),
            drag(3_200, 0.56),
            drag(3_500, 0.8),
            drag(4_000, 0.9),
        ];
        let window = |id, title: &str| WindowEntry {
            id,
            app: "OpenCode".into(),
            bundle_id: None,
            regular: true,
            title: title.into(),
            bounds: [0, 0, 100, 100],
            layer: 0,
            frontmost: true,
        };
        let overlay = WindowEntry {
            layer: 25,
            ..window(9, "Hex")
        };
        b.windows = vec![
            WindowSnapshot {
                t: 0,
                windows: vec![window(1, "Secret plans"), overlay],
            },
            WindowSnapshot {
                t: 4_800,
                windows: Vec::new(),
            },
        ];
        let all = paste(&b);
        assert!(all.contains("- 0:02 pressed ⇧⌘B ×3"), "{all}");
        assert!(all.contains("- 0:03 dragged right ×2"), "{all}");
        assert_eq!(
            all.matches("dragged").count(),
            1,
            "short drags are dropped and a run is one line: {all}"
        );
        assert!(!all.contains("x 0."), "no coordinates: {all}");
        assert!(all.contains("- 0:04 OpenCode window closed"), "{all}");
        assert!(!all.contains("Secret") && !all.contains("Hex"), "{all}");
    }

    #[test]
    fn places_appear_only_with_a_name() {
        let mut b = bundle(
            Vec::new(),
            vec![keyframe(1, 9_000, KeyframeReason::Mark, "")],
        );
        b.strokes = vec![
            Stroke {
                on: Some("button “Window”".into()),
                ..stroke(1_000, 0.1, 0.2)
            },
            stroke(3_000, 0.4, 0.5),
            stroke(3_100, 0.4, 0.52),
        ];
        b.marks = crate::sketch::marks_of(&b.strokes);
        for mark in &mut b.marks {
            mark.frame = Some("frames/frame-01.png".into());
        }
        let slider = |t, x1| InteractionEvent::Drag {
            t,
            t1: t + 300,
            x0: 0.5,
            y0: 0.9,
            x1,
            y1: 0.9,
            label: Some("slider “Playback”".into()),
            button: None,
            path: Vec::new(),
        };
        b.events = vec![
            InteractionEvent::Click {
                t: 5_000,
                x: 0.3,
                y: 0.3,
                button: None,
                label: Some("“Area”".into()),
                role: Some("button".into()),
            },
            slider(6_000, 0.6),
            slider(6_800, 0.4),
            slider(7_500, 0.45),
            InteractionEvent::Point {
                t: 8_500,
                x: 0.2,
                y: 0.2,
                label: None,
            },
        ];
        let all = paste(&b);
        assert!(
            all.contains(
                "            <marks>\n                - 0:01 underlined button “Window”\n                - 0:03 underlined (2 strokes)\n            </marks>\n"
            ),
            "{all}"
        );
        assert!(
            all.contains(
                "            <events>\n                - 0:05 clicked button “Area”\n                - 0:06 scrubbed slider “Playback”\n            </events>\n"
            ),
            "an unnamed point is left out: {all}"
        );
    }

    #[test]
    fn the_paste_lists_inputs_only_with_app_state_on() {
        let mut b = bundle(
            words(&[(500, "Look"), (800, "here.")]),
            vec![keyframe(1, 1_000, KeyframeReason::Speech, "Look here.")],
        );
        b.events = vec![InteractionEvent::Key {
            t: 400,
            shortcut: "⌘K".into(),
        }];
        let dir = std::path::Path::new("/b");
        let off = crate::sketch::summary_markdown_with(&b, dir, false);
        let on = crate::sketch::summary_markdown_with(&b, dir, true);
        assert!(!off.contains("⌘K"), "{off}");
        assert!(on.contains("- 0:00 pressed ⌘K"), "{on}");
    }

    #[test]
    fn repeated_clicks_fold_and_a_click_keyframe_owns_its_click() {
        let click = |t, x| InteractionEvent::Click {
            t,
            x,
            y: 0.5,
            button: None,
            label: None,
            role: None,
        };
        let mut b = bundle(
            Vec::new(),
            vec![
                keyframe(1, 500, KeyframeReason::SceneChange, ""),
                keyframe(2, 9_300, KeyframeReason::Click, ""),
            ],
        );
        b.keyframes[1].event = "clicked (x 0.20, y 0.50)".into();
        b.events = vec![
            click(1_000, 0.63),
            click(2_500, 0.63),
            click(4_000, 0.63),
            click(9_000, 0.2),
        ];
        let all = paste(&b);
        let second = &all[all.find("<moment n=\"2\"").expect("moment 2")..];
        assert!(
            all[..all.len() - second.len()].contains("- 0:01 clicked ×3"),
            "{all}"
        );
        assert!(
            second.starts_with(
                "<moment n=\"2\" t=\"0:09\">\n            <image>frames/frame-02.png</image>\n            <action>clicked</action>\n        </moment>"
            ),
            "the click that cut #2 is its action, not an event of #1: {all}"
        );
    }

    fn paste(b: &ScrawlBundle) -> String {
        crate::paste_xml::summary_xml(b, std::path::Path::new("/b"), true)
    }
}
