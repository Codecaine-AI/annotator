//! The `xmlPaste` experiment's summary: the same facts as `sketch::summary_markdown_with`, in
//! nested XML sections an agent and a person can scan. Each `<moment>` holds one keyframe: its
//! image, the action that cut it, the marks it draws and the words said over it. No coordinates:
//! they are in full-capture space, and the frames are often cropped, so the images show where.

use std::fmt::Write as _;

use crate::agent_context::{events as event_line_texts, mark_text, without_positions};
use crate::model::*;
use crate::sketch::{context_value, marks_on};

/// Read first: one wording for the paste shortcut (the sheet is attached above the text) and for
/// `summary.md` and the automatic copy (text only), so all three stay the same text.
pub const INSTRUCTIONS: &str = "Start with contact-sheet.png: it is attached above when the capture was pasted with its image, else it is in the bundle folder. It shows every moment on one image, numbered by the white chip in its corner. Read it before you change anything: the marks and words only make sense on the images. Open a moment's image for full size.";

/// A pin absorbs a tap or a partial mark drawn into or out of it this soon (`PIN_MAX_PRESS_MS`).
const PIN_JOIN_MS: u64 = 1_000;

/// How close (normalized) a neighbour's box must come to a pin to be part of the same ⇧-click.
const PIN_TOUCH: f32 = 0.01;

/// Text wraps at this many characters, indent included.
const WIDTH: usize = 96;

pub fn summary_xml(
    bundle: &ScrawlBundle,
    bundle_dir: &std::path::Path,
    agent_context: bool,
) -> String {
    let mut out = format!(
        "<scrawl_capture id=\"{}\" duration=\"{}\">\n",
        attr(&bundle.id),
        format_clock(bundle.duration_ms)
    );
    section(&mut out, 1, "instructions", INSTRUCTIONS);
    out.push('\n');
    out.push_str(&source(bundle));
    let _ = writeln!(
        out,
        "    <bundle path=\"{}\" />",
        attr(&bundle_dir.display().to_string())
    );
    if let Some(dest) = &bundle.destination {
        let title = if dest.title.is_empty() {
            &dest.session
        } else {
            &dest.title
        };
        let mut tag = format!(
            "    <destination session=\"{}\" title=\"{}\"",
            attr(&dest.session),
            attr(title)
        );
        if let Some(url) = &dest.url {
            let _ = write!(tag, " page=\"{}\"", attr(&url.to_string()));
        }
        let _ = writeln!(out, "{tag} />");
    }
    if bundle.keyframes.iter().any(|k| k.appshot.is_some()) {
        let _ = writeln!(
            out,
            "    <app_shots command=\"{} appshot {} --frame N\" />",
            crate::app_identity::Variant::current().command_name(),
            attr(&bundle.id)
        );
    }
    if !bundle.context.is_empty() {
        out.push_str("    <context>\n");
        for (key, value) in &bundle.context {
            let _ = writeln!(
                out,
                "        - {}: {}",
                text(&context_value(key)),
                text(&context_value(value))
            );
        }
        out.push_str("    </context>\n");
    }
    if let Some(note) = bundle
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        section(&mut out, 1, "note", note);
    }
    if let Some(missing) = &bundle.transcript.missing
        && bundle.transcript.text.trim().is_empty()
    {
        section(&mut out, 1, "no_transcript", &missing.reason);
    }
    out.push('\n');
    if bundle.keyframes.is_empty() {
        out.push_str(&without_moments(bundle));
    } else {
        out.push_str(&moments(bundle, agent_context));
    }
    out.push_str("</scrawl_capture>\n");
    out
}

/// `<source …/>`: the app or device, the window title after the privacy policy, and the crop.
fn source(bundle: &ScrawlBundle) -> String {
    let mut attrs: Vec<String> = Vec::new();
    if let Some(device) = &bundle.device {
        attrs.push(format!("device=\"{}\"", attr(&device.name)));
        if let Some(app) = &device.app {
            attrs.push(format!("app=\"{}\"", attr(app)));
        }
        if let Some(v) = device
            .app_short_version
            .as_deref()
            .filter(|v| !v.is_empty())
        {
            attrs.push(format!("version=\"{}\"", attr(v)));
        }
        if let Some(b) = device.app_version.as_deref().filter(|v| !v.is_empty()) {
            attrs.push(format!("build=\"{}\"", attr(b)));
        }
    } else {
        let app = bundle
            .target
            .app
            .clone()
            .unwrap_or_else(|| "Display".into());
        attrs.push(format!("app=\"{}\"", attr(&app)));
        if let Some(title) = &bundle.target.window_title {
            let title = crate::privacy::window_title(bundle.target.bundle_id.as_deref(), title);
            if !title.is_empty() && title != app {
                attrs.push(format!("window=\"{}\"", attr(&title)));
            }
        }
    }
    if let Some(screen) = bundle.screen_at(0) {
        attrs.push(format!("screen=\"{}\"", attr(&context_value(screen))));
    }
    if let Some(crop) = &bundle.autocrop {
        attrs.push(format!("cropped_to=\"{}\"", attr(&crop.label)));
        attrs.push(format!("uncropped=\"{}/\"", attr(&crop.full_frames)));
    }
    let mut line = String::from("    <source");
    let mut width = line.len();
    for a in attrs {
        if width + 1 + a.len() > WIDTH && width > "    <source".len() {
            line.push_str("\n           ");
            width = 11;
        }
        line.push(' ');
        line.push_str(&a);
        width += 1 + a.len();
    }
    line.push_str(" />\n");
    line
}

fn moments(bundle: &ScrawlBundle, agent_context: bool) -> String {
    let frames = &bundle.keyframes;
    let mut events: Vec<Vec<(u64, String)>> = vec![Vec::new(); frames.len()];
    for (t, line) in event_lines(bundle, agent_context) {
        let i = frames.iter().rposition(|k| k.t <= t).unwrap_or(0);
        events[i].push((t, line));
    }
    let mut out = format!("    <moments count=\"{}\">\n", frames.len());
    let mut shown_screen = bundle.screen_at(0);
    for (i, frame) in frames.iter().enumerate() {
        let screen = bundle.screen_at(frame.t);
        let mut head = format!(
            "        <moment n=\"{}\" t=\"{}\"",
            frame.index,
            format_clock(frame.t)
        );
        if let Some(name) = screen.filter(|_| screen != shown_screen) {
            let _ = write!(head, " screen=\"{}\"", attr(&context_value(name)));
        }
        shown_screen = screen;
        let _ = writeln!(out, "{head}>");
        let _ = writeln!(out, "            <image>{}</image>", text(&frame.file));

        let marks = mark_lines(frame, &bundle.marks);
        let said = frame
            .said
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let moment_events = std::mem::take(&mut events[i]);
        let action = action(
            bundle,
            frame,
            said.is_none() && marks.is_empty() && moment_events.is_empty(),
        );
        if let Some(action) = &action {
            let _ = writeln!(out, "            <action>{}</action>", text(action));
        }
        if !marks.is_empty() {
            out.push_str("            <marks>\n");
            for line in marks {
                let _ = writeln!(out, "                - {}", text(&line));
            }
            out.push_str("            </marks>\n");
        }
        if !moment_events.is_empty() {
            out.push_str("            <events>\n");
            for (t, line) in moment_events {
                let _ = writeln!(out, "                - {} {}", format_clock(t), text(&line));
            }
            out.push_str("            </events>\n");
        }
        if let Some(said) = said {
            section(&mut out, 3, "said", said);
        }
        out.push_str("        </moment>\n");
    }
    out.push_str("    </moments>\n");
    out
}

/// What cut the keyframe, without positions: a click (`clicked`, `clicked button “Files”`), a
/// flag, a screen change. A mark's keyframe lists the mark instead, and a speech keyframe's words
/// say it, unless the moment would otherwise be empty.
fn action(bundle: &ScrawlBundle, frame: &Keyframe, empty: bool) -> Option<String> {
    let flagged = frame
        .mark_id
        .as_ref()
        .and_then(|id| bundle.marks.iter().find(|m| &m.id == id))
        .is_some_and(|m| m.shape == Shape::Flag);
    if flagged {
        return Some("flagged this moment".into());
    }
    let shown = match frame.reason {
        KeyframeReason::Mark | KeyframeReason::Speech | KeyframeReason::Initial => empty,
        _ => true,
    };
    let event = without_positions(&frame.event);
    let event = event.trim();
    (shown && !event.is_empty()).then(|| event.to_owned())
}

/// The marks a keyframe draws, one line each, with a ⇧-click's pin and the ink drawn into or out
/// of it as one line. `capture::end_press` cuts the ⇧-hold's ink at the click, and
/// `sketch::marks_of` keeps the pin apart, so one ⇧-click can leave a tap, a pin and a mark.
fn mark_lines(frame: &Keyframe, marks: &[Mark]) -> Vec<String> {
    let shown: Vec<&Mark> = marks_on(frame, marks)
        .into_iter()
        .filter(|m| m.shape != Shape::Flag)
        .collect();
    let mut absorbed = vec![false; shown.len()];
    let mut ink_from = vec![false; shown.len()];
    for (i, pin) in shown.iter().enumerate() {
        if pin.shape != Shape::Pin {
            continue;
        }
        for j in [i.wrapping_sub(1), i + 1] {
            let Some(other) = shown.get(j) else { continue };
            if absorbed[j] || !matches!(other.shape, Shape::Tap | Shape::Mark) {
                continue;
            }
            let gap = if j < i {
                pin.t0.saturating_sub(other.t1)
            } else {
                other.t0.saturating_sub(pin.t1)
            };
            if gap <= PIN_JOIN_MS && touches(other.box_norm, pin.box_norm) {
                absorbed[j] = true;
                ink_from[i] |= other.shape == Shape::Mark;
            }
        }
    }
    shown
        .iter()
        .enumerate()
        .filter(|(i, _)| !absorbed[*i])
        .map(|(i, mark)| {
            let mut line = format!("{} {}", format_clock(mark.t0), mark_text(mark));
            if ink_from[i] {
                line.push_str(", with ink drawn from it");
            }
            line
        })
        .collect()
}

/// Whether box `a`, grown by `PIN_TOUCH`, overlaps box `b`.
fn touches(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] - PIN_TOUCH <= b[2]
        && b[0] <= a[2] + PIN_TOUCH
        && a[1] - PIN_TOUCH <= b[3]
        && b[1] <= a[3] + PIN_TOUCH
}

/// App events and context changes; with app state on, also clicks, keys, drags and windows.
fn event_lines(bundle: &ScrawlBundle, agent_context: bool) -> Vec<(u64, String)> {
    if agent_context {
        return event_line_texts(bundle);
    }
    let mut lines: Vec<(u64, String)> = bundle
        .events
        .iter()
        .filter_map(|event| match event {
            InteractionEvent::AppEvent { t, label } => Some((*t, format!("app: {label}"))),
            _ => None,
        })
        .collect();
    lines.extend(bundle.context_changes.iter().map(|change| {
        let key = context_value(&change.key);
        let line = match &change.value {
            Some(value) => format!("{key} → {}", context_value(value)),
            None => format!("{key} cleared"),
        };
        (change.t, line)
    }));
    lines.sort_by_key(|(t, _)| *t);
    lines
}

/// A capture with no keyframes: its app events, else the whole transcript.
fn without_moments(bundle: &ScrawlBundle) -> String {
    let events = event_lines(bundle, false);
    let mut out = String::new();
    if !events.is_empty() {
        out.push_str("    <events>\n");
        for (t, line) in events {
            let _ = writeln!(out, "        - {} {}", format_clock(t), text(&line));
        }
        out.push_str("    </events>\n");
    } else if !bundle.transcript.text.trim().is_empty() {
        section(&mut out, 1, "said", bundle.transcript.text.trim());
    }
    out
}

/// `<tag>`, the body wrapped and indented one level in, `</tag>`, at nesting `level`.
fn section(out: &mut String, level: usize, tag: &str, body: &str) {
    let pad = "    ".repeat(level);
    let inner = "    ".repeat(level + 1);
    let _ = writeln!(out, "{pad}<{tag}>");
    out.push_str(&wrap(&text(body), &inner));
    let _ = writeln!(out, "{pad}</{tag}>");
}

fn wrap(body: &str, indent: &str) -> String {
    let mut out = String::new();
    let mut line = String::new();
    for word in body.split_whitespace() {
        if !line.is_empty()
            && indent.len() + line.chars().count() + 1 + word.chars().count() > WIDTH
        {
            let _ = writeln!(out, "{indent}{line}");
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        let _ = writeln!(out, "{indent}{line}");
    }
    out
}

/// Text inside an element: `&`, `<` and `>` escaped so a spoken or app-given `<` can't close a tag.
fn text(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn attr(raw: &str) -> String {
    text(raw).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(marks: serde_json::Value, keyframes: serde_json::Value) -> ScrawlBundle {
        serde_json::from_value(serde_json::json!({
            "version": 1, "id": "scrawl-1", "createdAtMs": 0, "durationMs": 30000,
            "target": {
                "mode": "display", "app": "Safari", "displayId": 1,
                "frame": {"x": 0.0, "y": 0.0, "width": 1000.0, "height": 1000.0, "scale": 1.0}
            },
            "transcript": {"source": "test", "text": "", "segments": []},
            "strokes": [], "marks": marks, "events": [],
            "keyframes": keyframes,
            "contactSheet": "contact-sheet.png"
        }))
        .unwrap()
    }

    fn mark(
        id: &str,
        shape: &str,
        t: u64,
        box_norm: [f32; 4],
        pin: Option<u32>,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": id, "strokes": [0], "t0": t, "t1": t + 100, "boxNorm": box_norm,
            "shape": shape, "pin": pin, "description": "", "frame": "frames/frame-01.png"
        })
    }

    #[test]
    fn a_shift_click_shows_as_one_pin_and_other_marks_stay_apart() {
        let marks = serde_json::json!([
            mark("m1", "circle", 2000, [0.1, 0.1, 0.3, 0.3], None),
            mark("m2", "tap", 17000, [0.379, 0.339, 0.381, 0.341], None),
            mark("m3", "pin", 17150, [0.38, 0.34, 0.38, 0.34], Some(1)),
            mark("m4", "mark", 17260, [0.34, 0.32, 0.38, 0.36], None),
        ]);
        let frames = serde_json::json!([{
            "index": 1, "t": 18000, "reason": "mark", "file": "frames/frame-01.png",
            "event": "circled region", "markId": "m1"
        }]);
        let out = summary_xml(
            &bundle(marks, frames),
            std::path::Path::new("/b.scrawl"),
            false,
        );
        let expected = "            <marks>\n                - 0:02 circled\n                - 0:17 pin 1, with ink drawn from it\n            </marks>\n";
        assert!(out.contains(expected), "{out}");
    }

    #[test]
    fn a_click_moment_has_no_coordinates_and_spoken_markup_is_escaped() {
        let frames = serde_json::json!([{
            "index": 1, "t": 9000, "reason": "click", "file": "frames/frame-01.png",
            "event": "clicked (x 0.36, y 0.20)", "said": "Make the <div> wider & taller."
        }]);
        let out = summary_xml(
            &bundle(serde_json::json!([]), frames),
            std::path::Path::new("/b.scrawl"),
            false,
        );
        assert!(
            out.contains("            <action>clicked</action>\n"),
            "{out}"
        );
        assert!(
            out.contains("Make the &lt;div&gt; wider &amp; taller."),
            "{out}"
        );
        assert!(!out.contains("x 0.36"), "{out}");
    }
}
