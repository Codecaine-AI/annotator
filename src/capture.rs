use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::app_identity::Variant;

use crate::model::{
    CaptureMode, InteractionEvent, RgbaFrame, SCALE, Stroke, TargetFrame, TargetInfo,
};

// The operating system side of capture: global shortcuts, input monitoring, the frontmost app,
// displays and windows, and the screen recorder. Each platform module provides the same items.
#[cfg_attr(target_os = "macos", path = "capture/macos.rs")]
#[cfg_attr(target_os = "windows", path = "capture/windows.rs")]
mod platform;
pub use platform::*;

pub const FRAME_SAMPLE_INTERVAL: Duration = Duration::from_millis(220);
/// Candidate frames only feed scene diffs and keyframe selection; the keyframes themselves are
/// decoded from `recording.mp4` at full size afterwards.
pub const CANDIDATE_EDGE: f64 = 480.0;
/// Without a recording to decode from, the candidates are the keyframes, so keep them sharper.
pub(crate) const CANDIDATE_EDGE_NO_VIDEO: f64 = 1280.0;
/// The candidates' memory ceiling, however long the capture runs.
pub const CANDIDATE_BUDGET_BYTES: usize = 384 << 20;

/// The candidate frames of a capture, bounded in memory. When full it drops every other frame
/// (keeping the first and the ones taken for a click, stroke or shortcut) and samples half as
/// often from then on, so a long capture keeps its whole timeline, evenly, at a coarser step.
pub struct CandidateBuffer {
    frames: Vec<(RgbaFrame, bool)>,
    cap: usize,
    budget: usize,
    interval: Duration,
    /// The recording is paused (`crate::pause`): frames are not kept, so a long pause neither
    /// fills the budget nor thins what was recorded.
    paused: bool,
}

impl CandidateBuffer {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            frames: Vec::new(),
            cap: 0,
            budget: budget_bytes,
            interval: FRAME_SAMPLE_INTERVAL,
            paused: false,
        }
    }

    /// How often to sample now.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn push(&mut self, frame: RgbaFrame, urgent: bool) {
        if self.paused {
            return;
        }
        if self.cap == 0 {
            self.cap = (self.budget / frame.rgba.len().max(1)).clamp(60, 1_200);
        }
        self.frames.push((frame, urgent));
        if self.frames.len() >= self.cap {
            self.thin();
        }
    }

    fn thin(&mut self) {
        let mut regular = 0_usize;
        let mut kept: Vec<(RgbaFrame, bool)> = std::mem::take(&mut self.frames)
            .into_iter()
            .enumerate()
            .filter(|(i, (_, urgent))| {
                let keep = *i == 0 || *urgent || regular.is_multiple_of(2);
                if !urgent {
                    regular += 1;
                }
                keep
            })
            .map(|(_, f)| f)
            .collect();
        // Mostly urgent frames (a storm of clicks): thin those too.
        if kept.len() > self.cap * 3 / 4 {
            kept = kept
                .into_iter()
                .enumerate()
                .filter(|(i, _)| i.is_multiple_of(2))
                .map(|(_, f)| f)
                .collect();
        }
        self.frames = kept;
        self.interval *= 2;
    }

    /// Forgets frames taken after `t_ms`, except the first.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn drop_since(&mut self, t_ms: u64) {
        let mut index = 0;
        self.frames.retain(|(frame, _)| {
            index += 1;
            index == 1 || frame.t_ms <= t_ms
        });
    }

    pub fn into_frames(self) -> Vec<RgbaFrame> {
        self.frames.into_iter().map(|(f, _)| f).collect()
    }
}

/// What a finished screen capture leaves behind.
pub struct ScreenRecording {
    /// Small candidate frames for keyframe selection (see `CANDIDATE_EDGE`).
    pub frames: Vec<RgbaFrame>,
    pub video: Option<PathBuf>,
    pub duration_ms: u64,
    /// The capture time (on the frames' clock) of the video's start.
    pub video_offset_ms: u64,
}

impl ScreenRecording {
    /// Takes a paused recording's pauses out of the candidate frames, the movie and the
    /// duration. `cuts` are on this recording's clock (the frames'). A movie that can't be cut
    /// is deleted, so nothing recorded during a pause stays in the bundle; the keyframes then
    /// come from the candidates. When every candidate was taken during a pause (paused before
    /// the first one and never resumed), none is kept and a blank one of the same size stands
    /// in, because a bundle needs a frame (as `ScreenCaptureSession::stop` sees to).
    pub fn cut(&mut self, cuts: &crate::pause::Cuts) {
        if cuts.is_empty() {
            return;
        }
        let size = self.frames.first().map(|frame| (frame.width, frame.height));
        cuts.cut_frames(&mut self.frames);
        if let (true, Some((width, height))) = (self.frames.is_empty(), size) {
            let blank = [0, 0, 0, 255].repeat(width as usize * height as usize);
            self.frames.push(RgbaFrame::new(0, width, height, blank));
        }
        if let Some(movie) = self.video.take() {
            match cuts.on_clock_from(self.video_offset_ms).cut_movie(&movie) {
                Ok(()) => self.video = Some(movie),
                Err(error) => {
                    scrawl_log(format!(
                        "The paused recording's movie could not be cut and is left out: {error:#}"
                    ));
                    let _ = std::fs::remove_file(&movie);
                }
            }
        }
        self.video_offset_ms = cuts.shift(self.video_offset_ms);
        self.duration_ms = cuts.shift(self.duration_ms).max(100);
    }
}

/// Live UI events emitted by the global input monitor to drive Scrawl's HUD & Review windows.
#[derive(Clone, Debug)]
pub enum LiveOverlayEvent {
    /// ⌥⇧S (or the configured shortcut) went down; `shortcut::Shortcut` decides what it means.
    CaptureShortcut,
    PasteLatestShortcut,
    UndoStroke,
    RequestSnapshot,
    /// The iPhone mirror ended on its own (unplugged, never found, or access denied).
    PhoneEnded,
    /// The welcome window: a button, fresh permission states, or the helper exiting.
    Onboarding(crate::onboarding::Event),
    /// The Settings window: an action, fresh permission states, or the helper exiting.
    Settings(crate::settings::Event),
    /// A ScrawlKit recording was ingested from an iPhone and compiled into a `.scrawl` bundle.
    IngestedBundle {
        dir: PathBuf,
        bundle: Box<crate::model::ScrawlBundle>,
        status: Option<String>,
    },
}

// Carbon HIToolbox modifier bits. The shortcut is stored with them on every platform.
pub const CARBON_CMD_KEY: u32 = 1 << 8;
pub const CARBON_SHIFT_KEY: u32 = 1 << 9;
pub const CARBON_OPTION_KEY: u32 = 1 << 11;
pub const CARBON_CONTROL_KEY: u32 = 1 << 12;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShortcutConfig {
    pub key_code: u32,
    pub carbon_modifiers: u32,
    pub label: String,
}

impl Default for ShortcutConfig {
    fn default() -> Self {
        Self::default_record()
    }
}

/// Annotator's key combinations. Record and Paste are global hotkeys (Carbon,
/// `RegisterHotKey`); Undo is matched only by the key tap or hook while a recording runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShortcutKind {
    /// Start, switch or finish a recording (⌥⇧S).
    Record,
    /// Paste the latest capture into the app in front (⌥⇧V).
    Paste,
    /// Take back the last stroke while recording (⌘Z).
    Undo,
}

impl ShortcutKind {
    pub const ALL: [Self; 3] = [Self::Record, Self::Paste, Self::Undo];

    pub fn default_config(self) -> ShortcutConfig {
        match self {
            Self::Record => ShortcutConfig::default_record(),
            Self::Paste => ShortcutConfig::default_paste(),
            Self::Undo => ShortcutConfig::default_undo(),
        }
    }

    /// The letter of the default, on the active layout (Dvorak's S is not ANSI's).
    fn default_char(self) -> char {
        match self {
            Self::Record => 's',
            Self::Paste => 'v',
            Self::Undo => 'z',
        }
    }

    pub(crate) fn ansi_code(self) -> u16 {
        match self {
            Self::Record => KEY_S,
            Self::Paste => KEY_V,
            Self::Undo => KEY_Z,
        }
    }

    /// The default's modifiers: the variant's for Record and Paste (`app_identity`), the
    /// system's undo modifier (⌘, or Ctrl on Windows) for Undo.
    fn default_modifiers_in(self, variant: Variant) -> u32 {
        match self {
            Self::Record | Self::Paste => variant.shortcut_modifiers(),
            Self::Undo if cfg!(target_os = "windows") => CARBON_CONTROL_KEY,
            Self::Undo => CARBON_CMD_KEY,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Record => "Record",
            Self::Paste => "Paste last capture",
            Self::Undo => "Undo stroke",
        }
    }

    /// Whether it can be turned off. Record can't: it's the way into Scrawl.
    pub fn can_unset(self) -> bool {
        self == Self::Paste
    }
}

impl ShortcutConfig {
    /// ⌥⇧S, or ⌃⌥S in Annotator Dev (`app_identity`).
    pub fn default_record() -> Self {
        let modifiers = Variant::current().shortcut_modifiers();
        Self {
            key_code: u32::from(layout_s_keycode()),
            carbon_modifiers: modifiers,
            label: shortcut_label(modifiers, "S"),
        }
    }

    /// ⌥⇧V, or ⌃⌥V in Annotator Dev.
    pub fn default_paste() -> Self {
        let modifiers = Variant::current().shortcut_modifiers();
        Self {
            key_code: u32::from(layout_v_keycode()),
            carbon_modifiers: modifiers,
            label: shortcut_label(modifiers, "V"),
        }
    }

    /// ⌘Z (Ctrl+Z on Windows) in both variants.
    pub fn default_undo() -> Self {
        let modifiers = ShortcutKind::Undo.default_modifiers_in(Variant::current());
        Self {
            key_code: u32::from(key_code_for_char('z').unwrap_or(KEY_Z)),
            carbon_modifiers: modifiers,
            label: shortcut_label(modifiers, "Z"),
        }
    }

    /// No shortcut: nothing is registered and no key press matches. Only the paste shortcut can
    /// be turned off.
    pub fn unset() -> Self {
        Self {
            key_code: u32::MAX,
            carbon_modifiers: 0,
            label: String::new(),
        }
    }

    pub fn is_unset(&self) -> bool {
        self.label.is_empty()
    }

    /// Whether this is `kind`'s default, which in the live app also answers to its aliases
    /// (⌃⌥⇧, ⌘⌥⇧, ⌃⌥⌘ and the ANSI key).
    pub fn is_default_for(&self, kind: ShortcutKind) -> bool {
        self.is_default_in(Variant::current(), kind)
    }

    fn is_default_in(&self, variant: Variant, kind: ShortcutKind) -> bool {
        self.carbon_modifiers == kind.default_modifiers_in(variant)
            && shortcut_parts(&self.label).1 == kind.default_char().to_ascii_uppercase().to_string()
    }

    pub fn key_label(&self) -> &str {
        shortcut_parts(&self.label).1
    }
}

/// `⌃⌥⇧⌘` in the order macOS writes them, for Carbon modifier bits; on Windows the same
/// modifiers as `Ctrl+Win+Alt+Shift+` (ready to take a key).
pub fn modifier_symbols(carbon_modifiers: u32) -> String {
    if cfg!(target_os = "windows") {
        let label = shortcut_label(carbon_modifiers, "");
        return label;
    }
    [
        (CARBON_CONTROL_KEY, '⌃'),
        (CARBON_OPTION_KEY, '⌥'),
        (CARBON_SHIFT_KEY, '⇧'),
        (CARBON_CMD_KEY, '⌘'),
    ]
    .into_iter()
    .filter(|(bit, _)| carbon_modifiers & bit != 0)
    .map(|(_, symbol)| symbol)
    .collect()
}

/// A shortcut as this platform writes it: `⌥⇧S` on macOS, `Alt+Shift+S` on Windows.
pub fn shortcut_label(carbon_modifiers: u32, key: &str) -> String {
    let held = |bit: u32| carbon_modifiers & bit != 0;
    if cfg!(target_os = "windows") {
        let mut parts = Vec::new();
        for (bit, name) in [
            (CARBON_CONTROL_KEY, "Ctrl"),
            (CARBON_CMD_KEY, "Win"),
            (CARBON_OPTION_KEY, "Alt"),
            (CARBON_SHIFT_KEY, "Shift"),
        ] {
            if held(bit) {
                parts.push(name);
            }
        }
        parts.push(key);
        parts.join("+")
    } else {
        let mut label = String::new();
        for (bit, glyph) in [
            (CARBON_CONTROL_KEY, '⌃'),
            (CARBON_OPTION_KEY, '⌥'),
            (CARBON_SHIFT_KEY, '⇧'),
            (CARBON_CMD_KEY, '⌘'),
        ] {
            if held(bit) {
                label.push(glyph);
            }
        }
        label.push_str(key);
        label
    }
}

/// The system paste keystroke: `⌘V` on macOS, `Ctrl+V` on Windows.
pub fn system_paste_label() -> &'static str {
    if cfg!(target_os = "windows") {
        "Ctrl+V"
    } else {
        "⌘V"
    }
}

/// The keys of a shortcut label, one per keycap: `⌥⇧S` → `⌥`, `⇧`, `S`; `Alt+Shift+S` →
/// `Alt`, `Shift`, `S`.
pub fn shortcut_keys(label: &str) -> Vec<String> {
    if label.contains('+') && label.len() > 1 {
        let mut keys: Vec<String> = label
            .split('+')
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
            .collect();
        // `Ctrl++` binds the plus key itself.
        if label.ends_with("++") {
            keys.push("+".into());
        }
        keys
    } else {
        label.chars().map(String::from).collect()
    }
}

pub fn scrawl_log(msg: impl AsRef<str>) {
    use std::io::Write;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let line = format!("[{ts}] {}\n", msg.as_ref());
    eprint!("{line}");
    // Tests log fixture failures on purpose; keep them out of the user's real log.
    if cfg!(test) {
        return;
    }
    let Some(dir) = log_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("scrawl.log"))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// A shortcut label split into its modifiers and key: `⌥⇧S` → (`⌥⇧`, `S`), and on Windows
/// `Alt+Shift+S` → (`Alt+Shift`, `S`).
pub fn shortcut_parts(label: &str) -> (&str, &str) {
    // The key itself may be "+" (`Ctrl++`), or a letter outside ASCII.
    let before_key = label.char_indices().last().map_or("", |(i, _)| &label[..i]);
    if let Some(plus) = before_key.rfind('+') {
        return (&label[..plus], &label[plus + 1..]);
    }
    let split = label
        .char_indices()
        .find(|(_, c)| !matches!(c, '⌃' | '⌥' | '⇧' | '⌘'))
        .map_or(label.len(), |(i, _)| i);
    label.split_at(split)
}

/// Whether a key press is the record shortcut. A custom one matches exactly; the default ⌥⇧S
/// also answers with ⌃ or ⌘ added and as ⌃⌥⌘S, on the active layout's S key (`s_code`) or the
/// ANSI one.
pub fn is_capture_shortcut(
    cfg: &ShortcutConfig,
    s_code: u16,
    keycode: u16,
    cmd: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> bool {
    shortcut_matches(
        ShortcutKind::Record,
        cfg,
        s_code,
        keycode,
        cmd,
        ctrl,
        alt,
        shift,
    )
}

/// `is_capture_shortcut` for any of Scrawl's shortcuts; `layout_code` is the default key on the
/// active layout.
#[allow(clippy::too_many_arguments)]
pub fn shortcut_matches(
    kind: ShortcutKind,
    cfg: &ShortcutConfig,
    layout_code: u16,
    keycode: u16,
    cmd: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> bool {
    if cfg.is_unset() {
        return false;
    }
    let want_cmd = (cfg.carbon_modifiers & CARBON_CMD_KEY) != 0;
    let want_ctrl = (cfg.carbon_modifiers & CARBON_CONTROL_KEY) != 0;
    let want_alt = (cfg.carbon_modifiers & CARBON_OPTION_KEY) != 0;
    let want_shift = (cfg.carbon_modifiers & CARBON_SHIFT_KEY) != 0;
    let exact = u32::from(keycode) == cfg.key_code
        && cmd == want_cmd
        && ctrl == want_ctrl
        && alt == want_alt
        && shift == want_shift;
    if exact {
        return true;
    }
    let is_key = keycode == layout_code || keycode == kind.ansi_code();
    if !cfg.is_default_for(kind) || !is_key {
        return false;
    }
    if kind == ShortcutKind::Undo {
        return carbon_modifiers(cmd, ctrl, alt, shift) == cfg.carbon_modifiers;
    }
    Variant::current().shortcut_aliases() && alt && (shift || (ctrl && cmd))
}

/// Carbon modifier bits for the modifiers held.
pub fn carbon_modifiers(cmd: bool, ctrl: bool, alt: bool, shift: bool) -> u32 {
    [
        (cmd, CARBON_CMD_KEY),
        (ctrl, CARBON_CONTROL_KEY),
        (alt, CARBON_OPTION_KEY),
        (shift, CARBON_SHIFT_KEY),
    ]
    .into_iter()
    .filter(|(held, _)| *held)
    .fold(0, |bits, (_, bit)| bits | bit)
}

/// Whether the modifiers `held` (Carbon bits) are the draw modifier `draw` and nothing else:
/// held alone it shows the laser, with a drag it inks, with a click it drops a pin.
pub fn draws(draw: u32, held: u32) -> bool {
    held == draw
}

/// Whether the draw modifier is down together with another one (as in ⌥⇧S): ink waits until
/// the draw modifier comes up, so a chord's leftover modifier never draws.
pub fn draw_in_chord(draw: u32, held: u32) -> bool {
    held & draw != 0 && held & !draw != 0
}

/// What decides whether a left press is kept from the app under the pointer.
#[derive(Clone, Copy)]
pub struct DrawPress {
    /// `blockDrawClicks` was on when the recording started.
    pub setting: bool,
    /// A recording takes input (not paused, not finished).
    pub active: bool,
    /// The draw modifier alone is held (`draws`).
    pub draws: bool,
    /// The draw modifier is left over from a chord, so it doesn't draw yet (the chord latch).
    pub latched: bool,
    /// The press is inside the capture target's rectangle.
    pub inside: bool,
}

/// Whether a left press stops at Annotator: it is one that drops a pin or draws ink, so the app
/// under the pointer never gets it. `elsewhere` (a window list, so asked last) says the press
/// lands on something that is not being recorded, and it then goes there: one of Annotator's own
/// windows such as the capsule, a macOS prompt, or a window in front of the recorded one
/// (`SharedCaptureTelemetry::press_misses_target`).
pub fn blocks_draw_press(press: DrawPress, elsewhere: impl FnOnce() -> bool) -> bool {
    press.setting && press.active && press.draws && !press.latched && press.inside && !elsewhere()
}

/// Bit 0 of the chord latch word (`SharedCaptureTelemetry::draw_latch`): the draw modifier is
/// latched. The bits above it count the key tap's modifier events.
const DRAW_LATCHED: u64 = 1;

/// The chord latch after the modifiers `held` (Carbon bits) are seen: off once the draw
/// modifier `draw` is up, on while it is down with another one (`draw_in_chord`), and as it was
/// while it is down alone.
fn latch_after(latched: bool, draw: u32, held: u32) -> bool {
    if held & draw == 0 {
        false
    } else if draw_in_chord(draw, held) {
        true
    } else {
        latched
    }
}

/// The chord latch as the poller found it before it read the modifier flags
/// (`SharedCaptureTelemetry::draw_latch_mark`): its sample counts only if the key tap has seen
/// no modifier change since.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LatchMark(u64);

/// A draw-press waiting for its release: when, where (global points) and on which frame.
#[derive(Clone, Copy, Debug)]
struct PinPress {
    t_ms: u64,
    x: f64,
    y: f64,
    frame: TargetFrame,
    /// The key tap kept this press from the app and ends it (`release_press`). Otherwise the
    /// mouse monitor or the polled buttons saw it and end it (`shift_up`).
    by_tap: bool,
}

/// What is under a global point, from one read of the window list (`window_hit_in`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowHit {
    /// One of Annotator's own windows (`scrawl_window_in`).
    scrawl: bool,
    /// A macOS prompt (`windows::system_prompt_at`): a press there answers macOS.
    prompt: bool,
    /// A press there goes to what is being recorded. Always so for a Screen or Area capture;
    /// for a Window or iPhone capture only when it goes to that window (`windows::press_goes_to`).
    target: bool,
}

impl WindowHit {
    /// A press there lands on something that is not being recorded, so it is no draw-press.
    fn misses_target(self) -> bool {
        self.scrawl || self.prompt || !self.target
    }
}

/// The active layout's key for `kind`'s default letter (Windows' hooks and poller match on it).
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn layout_default_keycode(kind: ShortcutKind) -> u16 {
    key_code_for_char(kind.default_char()).unwrap_or(kind.ansi_code())
}

/// The key codes and Carbon modifiers `cfg` registers: itself, plus for a default its aliases
/// (⌃⌥⇧, ⌘⌥⇧, ⌃⌥⌘) on the active layout's key and the ANSI one.
pub(crate) fn hotkey_combos(kind: ShortcutKind, cfg: &ShortcutConfig) -> Vec<(u32, u32)> {
    hotkey_combos_in(Variant::current(), kind, cfg)
}

pub(crate) fn hotkey_combos_in(
    variant: Variant,
    kind: ShortcutKind,
    cfg: &ShortcutConfig,
) -> Vec<(u32, u32)> {
    if cfg.is_unset() {
        return Vec::new();
    }
    let mut combos = vec![(cfg.key_code, cfg.carbon_modifiers)];
    if !cfg.is_default_in(variant, kind) {
        return combos;
    }
    let mut codes = vec![cfg.key_code];
    if cfg.key_code != u32::from(kind.ansi_code()) {
        codes.push(u32::from(kind.ansi_code()));
    }
    if kind == ShortcutKind::Undo {
        return codes
            .into_iter()
            .map(|code| (code, cfg.carbon_modifiers))
            .collect();
    }
    if !variant.shortcut_aliases() {
        return combos;
    }
    for code in codes {
        for mods in [
            CARBON_OPTION_KEY | CARBON_SHIFT_KEY,
            CARBON_CONTROL_KEY | CARBON_OPTION_KEY | CARBON_SHIFT_KEY,
            CARBON_CMD_KEY | CARBON_OPTION_KEY | CARBON_SHIFT_KEY,
            CARBON_CONTROL_KEY | CARBON_OPTION_KEY | CARBON_CMD_KEY,
        ] {
            if !combos.contains(&(code, mods)) {
                combos.push((code, mods));
            }
        }
    }
    combos
}

/// Undo's combos as one atomic word: two key codes (the layout's and the ANSI key, the same
/// for a custom one) and the modifiers. `u64::MAX` matches nothing.
fn pack_undo(cfg: &ShortcutConfig) -> u64 {
    let combos = hotkey_combos(ShortcutKind::Undo, cfg);
    let Some(&(a, mods)) = combos.first() else {
        return u64::MAX;
    };
    let b = combos.get(1).map_or(a, |&(b, _)| b);
    (u64::from(a & 0xFFFF) << 32) | (u64::from(b & 0xFFFF) << 16) | u64::from(mods & 0xFFFF)
}

fn undo_matches(packed: u64, keycode: u16, held: u32) -> bool {
    if packed == u64::MAX {
        return false;
    }
    let (a, b, mods) = (
        packed >> 32 & 0xFFFF,
        packed >> 16 & 0xFFFF,
        packed & 0xFFFF,
    );
    u64::from(held) == mods && (u64::from(keycode) == a || u64::from(keycode) == b)
}

fn clean_label(input: &str) -> String {
    let squashed = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = squashed.trim();
    if trimmed.chars().count() > 44 {
        let prefix: String = trimmed.chars().take(43).collect();
        format!("{}…", prefix.trim_end())
    } else {
        trimmed.to_string()
    }
}

/// Which display to record: the one under `point`, else the one holding the focused window's
/// centre, else the first. Frames and points are global, top-left origin.
pub fn pick_display(
    displays: &[[f64; 4]],
    point: Option<[f64; 2]>,
    focused: Option<[f64; 4]>,
) -> usize {
    let inside = |p: [f64; 2]| {
        displays
            .iter()
            .position(|&[x, y, w, h]| p[0] >= x && p[0] < x + w && p[1] >= y && p[1] < y + h)
    };
    point
        .and_then(inside)
        .or_else(|| focused.and_then(|[x, y, w, h]| inside([x + w / 2.0, y + h / 2.0])))
        .unwrap_or(0)
}

pub fn selectable_window_at(windows: &[TargetInfo], x: f64, y: f64) -> Option<&TargetInfo> {
    windows.iter().find(|window| {
        let f = window.frame;
        x >= f.x && x <= f.x + f.width && y >= f.y && y <= f.y + f.height
    })
}

/// Resolves region metadata from the picker's snapshot, without another SCK query.
pub fn region_target(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    windows: &[TargetInfo],
) -> TargetInfo {
    let mid_x = x + width * 0.5;
    let mid_y = y + height * 0.5;
    let hit = selectable_window_at(windows, mid_x, mid_y);
    let frontmost = frontmost_app_info();
    // One display per recording: the area's (by its centre), trimmed to that display and at
    // least `MIN_AREA` a side inside it, so this frame is exactly the recorded `sourceRect`.
    let screens = crate::screens::displays();
    let (display_id, scale, [x, y, width, height]) =
        crate::screens::area_on_display(&screens, [x, y, width, height]).map_or_else(
            || {
                let min = crate::screens::MIN_AREA;
                let rect = [x, y, width.max(min), height.max(min)];
                (main_display_id(), MAX_CAPTURE_SCALE, rect)
            },
            |(i, rect)| (screens[i].id, screens[i].scale, rect),
        );
    TargetInfo {
        mode: CaptureMode::Region,
        app: hit
            .and_then(|window| window.app.clone())
            .or_else(|| frontmost.as_ref().map(|(_, name, _)| name.clone())),
        bundle_id: hit
            .and_then(|window| window.bundle_id.clone())
            .or_else(|| frontmost.and_then(|(_, _, bid)| bid)),
        window_title: hit.and_then(|window| window.window_title.clone()),
        window_id: None,
        display_id,
        frame: TargetFrame {
            x,
            y,
            width,
            height,
            scale,
        },
    }
}

/// The capture size for `target` with its long edge at most `long_edge` px (and at most its
/// display's pixels: a 1x display isn't upscaled, and on Windows frames are pixels already,
/// `MAX_CAPTURE_SCALE`).
pub fn capture_size(target: &TargetInfo, long_edge: f64) -> (u32, u32) {
    let max_dim = target.frame.width.max(target.frame.height).max(1.0);
    let native = target.frame.scale.clamp(1.0, MAX_CAPTURE_SCALE);
    let scale = (long_edge / max_dim).min(native);
    (
        ((target.frame.width * scale).round() as u32).max(48) & !1,
        ((target.frame.height * scale).round() as u32).max(48) & !1,
    )
}

/// What the platform's sampler thread records into. It runs until `stopping`, pushes candidate
/// frames (at once when `urgent`), stores when the movie started into `video_offset_ms`, and
/// returns the movie it wrote.
pub(crate) struct Sampler {
    pub target: TargetInfo,
    pub video_output_path: Option<PathBuf>,
    /// Scrawl's own windows to leave out of the recording.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub excluded_window_ids: Arc<Mutex<Vec<u32>>>,
    pub started_at: Instant,
    pub stopping: Arc<AtomicBool>,
    pub urgent: Arc<AtomicBool>,
    pub frames: Arc<Mutex<CandidateBuffer>>,
    pub video_offset_ms: Arc<AtomicU64>,
}

/// Active screen + optional MP4 recorder (ScreenCaptureKit on macOS, Windows.Graphics.Capture
/// and Media Foundation on Windows).
pub struct ScreenCaptureSession {
    started_at: Instant,
    stopping: Arc<AtomicBool>,
    urgent_snapshot: Arc<AtomicBool>,
    frames: Arc<Mutex<CandidateBuffer>>,
    video_offset_ms: Arc<AtomicU64>,
    sampler_thread: Option<JoinHandle<Option<PathBuf>>>,
    _recording: crate::retention::Recording,
}

impl ScreenCaptureSession {
    pub fn start(
        target: TargetInfo,
        video_output_path: Option<PathBuf>,
        excluded_window_ids: Arc<Mutex<Vec<u32>>>,
    ) -> Self {
        let started_at = Instant::now();
        let stopping = Arc::new(AtomicBool::new(false));
        let urgent_snapshot = Arc::new(AtomicBool::new(true));
        let frames = Arc::new(Mutex::new(CandidateBuffer::new(CANDIDATE_BUDGET_BYTES)));
        let video_offset_ms = Arc::new(AtomicU64::new(0));

        let thread_stopping = Arc::clone(&stopping);
        let thread_urgent = Arc::clone(&urgent_snapshot);
        let thread_frames = Arc::clone(&frames);
        let thread_offset = Arc::clone(&video_offset_ms);
        let sampler_target = target;
        let sampler_thread = thread::Builder::new()
            .name("scrawl-frame-sampler".into())
            .spawn(move || {
                platform::run_sampler(Sampler {
                    target: sampler_target,
                    video_output_path,
                    excluded_window_ids,
                    started_at,
                    stopping: thread_stopping,
                    urgent: thread_urgent,
                    frames: thread_frames,
                    video_offset_ms: thread_offset,
                })
            })
            .ok();

        Self {
            started_at,
            stopping,
            urgent_snapshot,
            frames,
            video_offset_ms,
            sampler_thread,
            _recording: crate::retention::Recording::start(),
        }
    }

    pub fn request_immediate_frame(&self) {
        self.urgent_snapshot.store(true, Ordering::Release);
        if let Some(ref handle) = self.sampler_thread {
            handle.thread().unpark();
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// While paused no candidate frame is kept. The movie goes on; `pause::Cuts` takes the
    /// pause out of it at finish.
    pub fn set_paused(&self, paused: bool) {
        self.frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_paused(paused);
    }

    pub fn stop(mut self) -> ScreenRecording {
        let duration_ms = self.elapsed_ms();
        self.stopping.store(true, Ordering::Release);
        let video_path = self
            .sampler_thread
            .take()
            .and_then(|handle| {
                handle.thread().unpark();
                handle.join().ok()
            })
            .flatten();
        let buffer = std::mem::replace(
            &mut *self
                .frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            CandidateBuffer::new(0),
        );
        let mut collected = buffer.into_frames();
        if collected.is_empty() {
            collected.push(crate::render::render_synthetic_demo_frame(1280, 800, 0, 0));
        }
        let valid_video =
            video_path.filter(|path| path.exists() && path.metadata().is_ok_and(|m| m.len() > 0));
        ScreenRecording {
            frames: collected,
            video: valid_video,
            duration_ms: duration_ms.max(100),
            video_offset_ms: self.video_offset_ms.load(Ordering::Acquire),
        }
    }
}

impl Drop for ScreenCaptureSession {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(handle) = self.sampler_thread.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

/// Shared state collected by the global input monitor during an active capture session.
pub struct SharedCaptureTelemetry {
    pub active: AtomicBool,
    pub selecting: AtomicBool,
    pub shift_held: AtomicBool,
    pub capture_mode: AtomicU64,
    /// The record shortcut (`crate::settings` loads and saves it).
    pub shortcut: Mutex<ShortcutConfig>,
    pub paste_shortcut: Mutex<ShortcutConfig>,
    /// Undo, the draw modifier and the cancel key, as Settings shows them.
    controls: Mutex<crate::settings::Shortcuts>,
    /// The same three as the key tap and the poller match them, swapped whole on a change:
    /// Undo's combos (`pack_undo`), the draw modifier's Carbon bit, the cancel key's code.
    undo_combo: AtomicU64,
    draw_mask: AtomicU32,
    cancel_key: AtomicU32,
    /// Settings is recording a new shortcut: none of Scrawl's fire (`pause_shortcuts`).
    pub shortcuts_paused: AtomicBool,
    pub started_at: Mutex<Instant>,
    pub picker_started_at: Mutex<Instant>,
    pub picker_rect: Mutex<Option<([f32; 4], u8)>>,
    pub target_frame: Mutex<TargetFrame>,
    pub pointer_screen: Mutex<[f32; 2]>,
    pub events: Mutex<Vec<InteractionEvent>>,
    pub strokes: Mutex<Vec<Stroke>>,
    pub active_laser: Mutex<Option<Stroke>>,
    pub live_ink: Mutex<Option<Stroke>>,
    pub excluded_window_ids: Arc<Mutex<Vec<u32>>>,
    /// Raised when a click, ink stroke or shortcut starts, so the Screen-mode window log takes
    /// a snapshot right away.
    pub window_log_request: Arc<AtomicBool>,
    pub ui_sender: flume::Sender<LiveOverlayEvent>,
    /// Scrawl's windows that let clicks through (the ink overlay), skipped when deciding whether
    /// a click landed on Scrawl itself.
    pub passthrough_window_ids: Mutex<Vec<u32>>,
    /// The `scrawl-helper` whose window is what's being recorded (the iPhone mirror): its clicks
    /// count, unlike those on other helper windows (Settings, onboarding). 0 when none.
    capture_source_pid: AtomicI32,
    /// The window a Window capture records (`TargetInfo::window_id`), 0 when there is none.
    target_window_id: AtomicU32,
    /// The last `on_scrawl_window` answer (when, where, what), reused for a burst of scrolls.
    /// The mouse monitor, the polled buttons and scrolls ask it. The key tap never reads or
    /// writes it: each press it judges gets an answer of its own (`press_misses_target`).
    scrawl_hit: Mutex<Option<(Instant, f64, f64, bool)>>,
    /// macOS's key event tap (`CFMachPortRef`), made once Accessibility is on (`check_key_tap`).
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    tap_port: AtomicU64,
    /// The key event tap thread's run loop (retained), so `check_key_tap` can stop it.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    tap_run_loop: AtomicU64,
    /// A key tap thread is being started.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    tap_starting: AtomicBool,
    /// When the last key tap was tried (ms since `UNIX_EPOCH`), to space out retries.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    tap_tried_ms: AtomicU64,
    /// Whether key presses are arriving (macOS's key tap, Windows' keyboard hook), as the
    /// poller last found it (every 500 ms): for the log and `keys_live`, never for who records
    /// a press (`tap_takes_presses`).
    tap_live: AtomicBool,
    /// macOS's key tap may be delivering left presses: set before it is made or enabled and
    /// cleared once it is off (`tap_presses_on`, `tap_presses_off`), so it is never off while
    /// the tap delivers.
    tap_takes_presses: AtomicBool,
    /// Held while the key tap is turned on or off, so `tap_takes_presses` and the tap change
    /// together, and while a tap that macOS turned off is brought back (`recover_tap`), so a
    /// look at the tap, the forgetting of its kept press and its enabling are one step.
    tap_gate: Mutex<()>,
    /// When the cancel key went down, for each press since the UI loop last looked
    /// (`take_escape`, `take_cancel_presses`).
    cancel_presses: Mutex<Vec<Instant>>,
    /// The recording is paused (`crate::pause`): `active` is off so no click, ink or key is
    /// recorded, and the overlay keeps drawing what is already there.
    paused: AtomicBool,
    /// Whether input was being recorded when the pause began, to put back at resume.
    active_before_pause: AtomicBool,
    /// A Screen recording started straight from the shortcut: the overlay flashes an edge glow.
    pub start_glow: AtomicBool,
    /// Clicks are arriving (macOS's global mouse monitor, Windows' mouse hook). Until then the
    /// poller records clicks from polled button state.
    mouse_live: AtomicBool,
    last_shortcut_ms: AtomicU64,
    drag_start: Mutex<Option<(u64, f32, f32, Option<String>, TargetFrame)>>,
    /// App state was on when this capture started (`handoff::app_state`): clicks and strokes
    /// are looked up in the Accessibility tree (`crate::ax`).
    agent_context: AtomicBool,
    /// A ⇧-press inside the target, and who ends it. Whoever starts one (the key tap, or the
    /// mouse monitor and the polled buttons) decides under this lock, so a press has one owner.
    shift_press: Mutex<Option<PinPress>>,
    /// `blockDrawClicks` as the recording started (`handoff::block_draw_clicks`).
    block_draw_clicks: AtomicBool,
    /// The chord latch (bit `DRAW_LATCHED`): the draw modifier came down with another one and
    /// doesn't draw until it is let go, so a press then is not a draw-press either. The bits
    /// above count the key tap's modifier events, so a polled sample taken before the tap's
    /// last one never replaces it, and while the tap is on only its own events clear the
    /// latch (`note_tap_modifiers`, `note_polled_modifiers`).
    draw_latch: AtomicU64,
    /// The left button is down on a press the key tap kept from the app: its drags and its
    /// release are kept too, even if the recording pauses or finishes before the release.
    /// Set and forgotten under `shift_press`'s lock, with the press it belongs to.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    blocked_press: AtomicBool,
    /// Counts the events the key tap was handed (`note_tap_event`), notices aside.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    tap_events: AtomicU64,
    /// `tap_events` when the kept press came down. While they are equal, the kept press is
    /// the last event the tap took, so it may be the one macOS timed the tap out on.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    kept_at_event: AtomicU64,
    /// The poller found the key tap off and has not seen it on since (`recover_tap`). Each
    /// tap starts with it clear (`tap_presses_on` making a tap, `tap_presses_off`).
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    tap_found_off: AtomicBool,
    /// The event time of the last left press the key tap judged (`note_left_down_at`), so the
    /// kept one's when it is kept.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    kept_down_at: AtomicU64,
}

/// Why the key tap is being brought back (`SharedCaptureTelemetry::recover_tap`).
#[cfg_attr(target_os = "windows", allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TapOff {
    /// The tap's notice that macOS timed it out on a slow callback.
    Timeout,
    /// The tap's notice that macOS turned it off for the user (Secure Input, say). No callback
    /// was cut short, so every press the tap kept was kept in time.
    UserInput,
    /// The poller's look every 500 ms. It knows no reason.
    Found,
}

impl SharedCaptureTelemetry {
    pub fn new(ui_sender: flume::Sender<LiveOverlayEvent>) -> Arc<Self> {
        let shortcuts = crate::settings::Shortcuts::load();
        crate::settings::note_paste_label(&shortcuts.paste.label);
        Arc::new(Self {
            active: AtomicBool::new(false),
            selecting: AtomicBool::new(false),
            shift_held: AtomicBool::new(false),
            capture_mode: AtomicU64::new(0),
            undo_combo: AtomicU64::new(pack_undo(&shortcuts.undo)),
            draw_mask: AtomicU32::new(shortcuts.draw.carbon()),
            cancel_key: AtomicU32::new(shortcuts.cancel.key_code()),
            shortcut: Mutex::new(shortcuts.record.clone()),
            paste_shortcut: Mutex::new(shortcuts.paste.clone()),
            controls: Mutex::new(shortcuts),
            shortcuts_paused: AtomicBool::new(false),
            started_at: Mutex::new(Instant::now()),
            picker_started_at: Mutex::new(Instant::now()),
            picker_rect: Mutex::new(None),
            target_frame: Mutex::new(TargetFrame {
                x: 0.0,
                y: 0.0,
                width: 1440.0,
                height: 900.0,
                scale: 2.0,
            }),
            pointer_screen: Mutex::new([0.0, 0.0]),
            events: Mutex::new(Vec::new()),
            strokes: Mutex::new(Vec::new()),
            active_laser: Mutex::new(None),
            live_ink: Mutex::new(None),
            excluded_window_ids: Arc::new(Mutex::new(Vec::new())),
            window_log_request: Arc::new(AtomicBool::new(false)),
            ui_sender,
            passthrough_window_ids: Mutex::new(Vec::new()),
            capture_source_pid: AtomicI32::new(0),
            target_window_id: AtomicU32::new(0),
            scrawl_hit: Mutex::new(None),
            tap_port: AtomicU64::new(0),
            tap_run_loop: AtomicU64::new(0),
            tap_starting: AtomicBool::new(false),
            tap_tried_ms: AtomicU64::new(0),
            tap_live: AtomicBool::new(false),
            tap_takes_presses: AtomicBool::new(false),
            tap_gate: Mutex::new(()),
            cancel_presses: Mutex::new(Vec::new()),
            paused: AtomicBool::new(false),
            active_before_pause: AtomicBool::new(false),
            start_glow: AtomicBool::new(false),
            mouse_live: AtomicBool::new(false),
            last_shortcut_ms: AtomicU64::new(0),
            drag_start: Mutex::new(None),
            agent_context: AtomicBool::new(false),
            shift_press: Mutex::new(None),
            block_draw_clicks: AtomicBool::new(true),
            draw_latch: AtomicU64::new(0),
            blocked_press: AtomicBool::new(false),
            tap_events: AtomicU64::new(0),
            kept_at_event: AtomicU64::new(0),
            tap_found_off: AtomicBool::new(false),
            kept_down_at: AtomicU64::new(0),
        })
    }

    /// Each source (Carbon or `RegisterHotKey`, the key tap or hook) reports the same press within
    /// a few ms; a press after that is a new one, as when S is pressed again to switch modes.
    pub fn emit_capture_shortcut(&self) {
        if self.shortcuts_paused.load(Ordering::Acquire) {
            return;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let prev = self.last_shortcut_ms.load(Ordering::Acquire);
        if now_ms.saturating_sub(prev) >= 90
            && self
                .last_shortcut_ms
                .compare_exchange(prev, now_ms, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            scrawl_log("emit_capture_shortcut");
            let _ = self.ui_sender.try_send(LiveOverlayEvent::CaptureShortcut);
        }
    }

    pub fn emit_paste_shortcut(&self) {
        if self.shortcuts_paused.load(Ordering::Acquire) {
            return;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let prev = self.last_shortcut_ms.load(Ordering::Acquire);
        if now_ms.saturating_sub(prev) >= 400
            && self
                .last_shortcut_ms
                .compare_exchange(prev, now_ms, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            scrawl_log("emit_paste_shortcut -> sending PasteLatestShortcut");
            let _ = self
                .ui_sender
                .try_send(LiveOverlayEvent::PasteLatestShortcut);
        }
    }

    pub fn matches_shortcut(
        &self,
        keycode: u16,
        cmd: bool,
        ctrl: bool,
        alt: bool,
        shift: bool,
    ) -> bool {
        let cfg = self
            .shortcut
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        is_capture_shortcut(&cfg, layout_s_keycode(), keycode, cmd, ctrl, alt, shift)
    }

    /// The record and paste shortcuts.
    pub fn shortcuts(&self) -> (ShortcutConfig, ShortcutConfig) {
        let lock = |m: &Mutex<ShortcutConfig>| {
            m.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        };
        (lock(&self.shortcut), lock(&self.paste_shortcut))
    }

    /// All five controls: Record and Paste as registered, the other three as last set.
    pub fn controls(&self) -> crate::settings::Shortcuts {
        let (record, paste) = self.shortcuts();
        crate::settings::Shortcuts {
            record,
            paste,
            ..self
                .controls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    /// Takes Undo, the draw modifier and the cancel key from `shortcuts`; the next key press
    /// and poll use them.
    pub fn set_recording_keys(&self, shortcuts: &crate::settings::Shortcuts) {
        self.undo_combo
            .store(pack_undo(&shortcuts.undo), Ordering::Release);
        self.draw_mask
            .store(shortcuts.draw.carbon(), Ordering::Release);
        self.cancel_key
            .store(shortcuts.cancel.key_code(), Ordering::Release);
        *self
            .controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = shortcuts.clone();
    }

    /// The draw modifier's Carbon bit.
    pub fn draw_mask(&self) -> u32 {
        self.draw_mask.load(Ordering::Acquire)
    }

    /// Whether a key press (`held`: Carbon bits) is the undo shortcut.
    pub fn is_undo(&self, keycode: u16, held: u32) -> bool {
        undo_matches(self.undo_combo.load(Ordering::Acquire), keycode, held)
    }

    /// Whether a key press is the cancel key. ⌥ and ⇧ may still be down from the record
    /// shortcut, and ⇧ is down while drawing; with ⌘ or ⌃ it's some other app's shortcut.
    /// Settings lets no shortcut share the key, whatever its modifiers (`settings::clash`), so
    /// a press is never Undo as well.
    pub fn is_cancel(&self, keycode: u16, held: u32) -> bool {
        !self.shortcuts_paused.load(Ordering::Acquire)
            && u32::from(keycode) == self.cancel_key.load(Ordering::Acquire)
            && held & (CARBON_CMD_KEY | CARBON_CONTROL_KEY) == 0
    }

    /// Undo's key codes and modifiers, for a poller that reads key state (Windows).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn undo_keys(&self) -> Option<([u16; 2], u32)> {
        let packed = self.undo_combo.load(Ordering::Acquire);
        (packed != u64::MAX).then(|| {
            let code = |shift: u32| (packed >> shift & 0xFFFF) as u16;
            ([code(32), code(16)], (packed & 0xFFFF) as u32)
        })
    }

    /// The cancel key's code.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn cancel_key(&self) -> u16 {
        self.cancel_key.load(Ordering::Acquire) as u16
    }

    /// The cancel key went down (in any app): `take_escape` and `take_cancel_presses` report it.
    pub fn note_cancel(&self) {
        let mut presses = self
            .cancel_presses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The UI loop takes them every few ms; this only bounds a stalled one.
        if presses.len() < 32 {
            presses.push(Instant::now());
        }
    }

    /// What the next recording is of: its mode and, for a Window capture, that window's id
    /// (an iPhone capture's window is known by its process, `set_capture_source_pid`).
    pub fn set_capture_target(&self, mode: CaptureMode, window_id: Option<u32>) {
        let code = match mode {
            CaptureMode::Display => 0,
            CaptureMode::Region => 1,
            CaptureMode::Window => 2,
            CaptureMode::Device => 3,
        };
        self.capture_mode.store(code, Ordering::Release);
        self.target_window_id
            .store(window_id.unwrap_or(0), Ordering::Release);
    }

    pub fn begin_picker_session(&self) {
        *self
            .picker_started_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
        *self
            .picker_rect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.active.store(false, Ordering::Release);
        self.selecting.store(true, Ordering::Release);
    }

    pub fn set_picker_rect(&self, rect: Option<([f32; 4], u8)>) {
        *self
            .picker_rect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = rect;
    }

    pub fn reset_for_session(&self, frame: TargetFrame) {
        self.selecting.store(false, Ordering::Release);
        *self
            .picker_rect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .started_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
        *self
            .target_frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = frame;
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.strokes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        *self
            .active_laser
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .live_ink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .drag_start
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .shift_press
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.excluded_window_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.shift_held.store(false, Ordering::Release);
        self.paused.store(false, Ordering::Release);
        self.active_before_pause.store(false, Ordering::Release);
        self.window_log_request.store(false, Ordering::Release);
        let agent_context = crate::handoff::app_state();
        self.agent_context.store(agent_context, Ordering::Release);
        self.block_draw_clicks
            .store(crate::handoff::block_draw_clicks(), Ordering::Release);
        if agent_context {
            crate::ax::Prober::shared().begin();
        }
        self.active.store(true, Ordering::Release);
        let clicks = if self.mouse_live.load(Ordering::Acquire) {
            platform::MOUSE_SOURCE
        } else {
            "polled mouse buttons"
        };
        let keys = if self.tap_live.load(Ordering::Acquire) {
            platform::KEY_SOURCE
        } else {
            platform::NO_KEY_SOURCE
        };
        scrawl_log(format!("session clicks from {clicks}, keys from {keys}"));
    }

    /// Whether the cancel key went down since the last call (a pending shortcut and an open
    /// picker cancel on it).
    pub fn take_escape(&self) -> bool {
        !self.take_cancel_presses().is_empty()
    }

    /// The cancel key's presses since the last call, each as milliseconds on the capture clock
    /// (`elapsed_ms`), oldest first: a recording pauses on one and cancels on two close together
    /// (`crate::pause`).
    pub fn take_cancel_presses(&self) -> Vec<u64> {
        let presses = std::mem::take(
            &mut *self
                .cancel_presses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if presses.is_empty() {
            return Vec::new();
        }
        let started_at = *self
            .started_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        presses
            .into_iter()
            .map(|at| at.saturating_duration_since(started_at).as_millis() as u64)
            .collect()
    }

    /// Whether key presses reach Annotator from every app (the key tap or hook is on).
    pub fn keys_live(&self) -> bool {
        self.tap_live.load(Ordering::Acquire)
    }

    /// The recording pauses: what is being drawn is finished as it stands, and no click, ink or
    /// key is recorded until `resume_input`.
    pub fn pause_input(&self) {
        if self.paused.swap(true, Ordering::AcqRel) {
            return;
        }
        let was_active = self.active.swap(false, Ordering::AcqRel);
        self.active_before_pause
            .store(was_active, Ordering::Release);
        self.end_gestures();
    }

    /// The recording resumes.
    pub fn resume_input(&self) {
        if self.paused.swap(false, Ordering::AcqRel)
            && self.active_before_pause.swap(false, Ordering::AcqRel)
        {
            self.active.store(true, Ordering::Release);
        }
    }

    /// Whether the overlay has something to draw: a recording, paused or not.
    pub fn overlay_live(&self) -> bool {
        self.active.load(Ordering::Acquire) || self.paused.load(Ordering::Acquire)
    }

    /// Ends a press, drag, laser trail or ink stroke in progress, keeping what was drawn.
    fn end_gestures(&self) {
        *self
            .drag_start
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .shift_press
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.shift_held.store(false, Ordering::Release);
        self.finish_active_laser(None);
        if let Some(ink) = self
            .live_ink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            && ink.pts.len() >= 3
        {
            self.strokes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(ink);
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            .as_millis() as u64
    }

    pub fn contains_screen_point(&self, screen_x: f64, screen_y: f64) -> bool {
        let frame = *self
            .target_frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        frame.width >= 16.0
            && frame.height >= 16.0
            && frame.x > -10_000.0
            && screen_x >= frame.x
            && screen_x <= frame.x + frame.width
            && screen_y >= frame.y
            && screen_y <= frame.y + frame.height
    }

    /// Whether to look up what clicks and strokes land on, and to take app shots: app state is
    /// on, the target is this Mac's screen (not the iPhone mirror) and Accessibility is granted.
    pub fn probing(&self) -> bool {
        self.agent_context.load(Ordering::Acquire)
            && self.capture_mode.load(Ordering::Acquire) != 3
            && accessibility_granted()
    }

    /// A mouse button went down at a global point.
    fn pointer_down(&self, t_ms: u64, screen_x: f64, screen_y: f64) {
        if !self.contains_screen_point(screen_x, screen_y) {
            return;
        }
        #[cfg(target_os = "macos")]
        {
            let raw = objc2::rc::autoreleasepool(|_| crate::windows::raw_windows());
            if crate::windows::system_prompt_at(&raw, screen_x, screen_y) {
                return;
            }
        }
        let (nx, ny, _, _) = self.normalize_point(screen_x, screen_y);
        self.window_log_request.store(true, Ordering::Release);
        let start_frame = *self
            .target_frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *self
            .drag_start
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((t_ms, nx, ny, None, start_frame));
        if self.probing() {
            crate::ax::Prober::shared().probe(crate::ax::Target::Pointer(t_ms), screen_x, screen_y);
            // The click changes the window: an app shot once it has settled.
            if let Some(walker) = crate::appshot::walker() {
                walker.request(crate::appshot::SETTLE);
            }
        }
    }

    /// A mouse button came up: records the click or drag that `pointer_down` began.
    fn pointer_up(&self, t_ms: u64, screen_x: f64, screen_y: f64, right: bool) {
        let Some((t0, x0, y0, start_label, start_frame)) = self
            .drag_start
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let end_frame = *self
            .target_frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !same_target_frame(start_frame, end_frame) {
            return;
        }
        let (nx, ny, fw, fh) = self.normalize_point(screen_x, screen_y);
        let dist_px = ((nx - x0) * fw as f32).hypot((ny - y0) * fh as f32);
        let _ = self.ui_sender.try_send(LiveOverlayEvent::RequestSnapshot);
        let event = if dist_px < CLICK_SLOP_PT {
            InteractionEvent::Click {
                t: t0,
                x: x0,
                y: y0,
                button: Some(if right { "right" } else { "left" }.into()),
                label: start_label,
                role: None,
            }
        } else {
            InteractionEvent::Drag {
                t: t0,
                t1: t_ms,
                x0,
                y0,
                x1: nx,
                y1: ny,
                label: start_label,
                button: None,
                path: Vec::new(),
            }
        };
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }

    /// The key tap saw the modifiers change to `held` (Carbon bits). Returns whether the draw
    /// modifier is latched: it came down with another one (`draw_in_chord`, as in ⌥⇧S) and
    /// draws nothing until it is let go. The tap's events come in the order the keys moved, the
    /// same order as the presses it judges, so each one counts, and it is counted so that an
    /// older polled sample can't replace it (`note_polled_modifiers`). One latch decides ink,
    /// pins and kept presses alike.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn note_tap_modifiers(&self, held: u32) -> bool {
        let draw = self.draw_mask();
        let mut word = self.draw_latch.load(Ordering::Acquire);
        loop {
            let latched = latch_after(word & DRAW_LATCHED != 0, draw, held);
            let next = (word & !DRAW_LATCHED).wrapping_add(2) | u64::from(latched);
            match self.draw_latch.compare_exchange_weak(
                word,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return latched,
                Err(now) => word = now,
            }
        }
    }

    /// The chord latch as it is now, for the poller to take before it reads the modifier flags
    /// and hand back with what it read (`note_polled_modifiers`).
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn draw_latch_mark(&self) -> LatchMark {
        LatchMark(self.draw_latch.load(Ordering::Acquire))
    }

    /// The poller read the modifiers `held` (Carbon bits) after taking `mark`. Returns whether
    /// the draw modifier is latched. The sample counts only if the key tap has seen no modifier
    /// change since `mark`: otherwise the sample may be from before that change, and the tap's
    /// answer stands. While the tap is on (`tap_presses_on`) a sample may set the latch but
    /// never clears it: the flags are read as the keys are now, which can be later than a
    /// press still waiting at the tap, and that press must be judged by the keys as they were.
    /// Only the tap's own event, in the order of its presses, says the draw modifier is up
    /// (`note_tap_modifiers`). A change the tap never sees leaves the latch on until it sees
    /// the next one, and a latched press passes. Without a tap every sample counts, as there
    /// is no other writer.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn note_polled_modifiers(&self, mark: LatchMark, held: u32) -> bool {
        let LatchMark(mark) = mark;
        let was = mark & DRAW_LATCHED != 0;
        let latched = latch_after(was, self.draw_mask(), held);
        if was && !latched && self.tap_takes_presses.load(Ordering::Acquire) {
            return self.draw_latch.load(Ordering::Acquire) & DRAW_LATCHED != 0;
        }
        let next = (mark & !DRAW_LATCHED) | u64::from(latched);
        match self
            .draw_latch
            .compare_exchange(mark, next, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => latched,
            Err(now) => now & DRAW_LATCHED != 0,
        }
    }

    /// Whether the draw modifier alone is held and draws: not left over from a chord.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn draws_now(&self, held: u32) -> bool {
        draws(self.draw_mask(), held) && self.draw_latch.load(Ordering::Acquire) & DRAW_LATCHED == 0
    }

    /// The key tap takes every draw-press: it may be delivering left presses
    /// (`tap_presses_on`) and `blockDrawClicks` is on. The mouse monitor and the polled buttons
    /// then start none (`shift_down`), so a press is counted once.
    pub(crate) fn tap_owns_draw_presses(&self) -> bool {
        self.tap_takes_presses.load(Ordering::Acquire)
            && self.block_draw_clicks.load(Ordering::Acquire)
    }

    /// Turns the key tap on: `turn_on` makes or enables it and says whether it is on. The tap's
    /// ownership of draw-presses is published before `turn_on` runs and taken back if the tap
    /// did not come on, under one lock with `tap_presses_off`. So the tap never delivers a
    /// press while the mouse monitor and the polled buttons still start draw-presses, and no
    /// late answer from an earlier look at the tap can undo it.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn tap_presses_on(&self, turn_on: impl FnOnce() -> bool) -> bool {
        let _gate = self
            .tap_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.publish_tap_presses(turn_on)
    }

    /// `tap_presses_on` for a caller that holds `tap_gate`.
    fn publish_tap_presses(&self, turn_on: impl FnOnce() -> bool) -> bool {
        self.tap_takes_presses.store(true, Ordering::Release);
        let on = turn_on();
        if !on {
            self.tap_takes_presses.store(false, Ordering::Release);
        }
        on
    }

    /// Brings the key tap back after macOS may have turned it off, as one step under the lock
    /// every enabling of the tap takes: a look at the tap (`is_on`), the forgetting of a kept
    /// press whose release may have gone by (`forget_press`), and `turn_on`, which enables the
    /// tap as `tap_presses_on` would. Returns whether the tap is on.
    ///
    /// Only a timeout can cut a callback short, so only then is a kept press forgotten, and
    /// only one that is the last event the tap took (`kept_press_is_last_event`): the tap may
    /// have timed out on it, and the app got it. A kept press with a later event after it (the
    /// draw modifier let go while the button stays down, and that event timed out) was kept in
    /// time, so its drags and release stay kept. If its release, or a new press, went by while
    /// the tap was off, the mouse monitor saw it reach the app and forgets the kept press
    /// (`left_edge_reached_app`); the next press through the tap does too (`block_press`).
    ///
    /// The tap's notices (`TapOff::Timeout`, `TapOff::UserInput`) come in the order of its
    /// events and say why. The poller's look (`TapOff::Found`) has neither: it can come long
    /// after the tap was brought back, and it cannot tell a timeout from the user's. A tap it
    /// finds on is left as it is. One it finds off is left for the notice that is on its way;
    /// only a tap still off at its next look, with no notice between, is taken as timed out and
    /// brought back.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn recover_tap(
        &self,
        off: TapOff,
        is_on: impl FnOnce() -> bool,
        turn_on: impl FnOnce() -> bool,
    ) -> bool {
        let _gate = self
            .tap_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let timed_out = match off {
            TapOff::Timeout => true,
            TapOff::UserInput => false,
            TapOff::Found if is_on() => false,
            TapOff::Found => {
                if !self.tap_found_off.swap(true, Ordering::AcqRel) {
                    return false;
                }
                true
            }
        };
        self.tap_found_off.store(false, Ordering::Release);
        if timed_out && self.kept_press_is_last_event() {
            self.forget_press();
        }
        self.publish_tap_presses(turn_on)
    }

    /// Turns the key tap off with `turn_off`, and only then hands draw-presses back to the mouse
    /// monitor and the polled buttons.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn tap_presses_off(&self, turn_off: impl FnOnce()) {
        let _gate = self
            .tap_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        turn_off();
        self.tap_found_off.store(false, Ordering::Release);
        self.tap_takes_presses.store(false, Ordering::Release);
    }

    /// A new key tap is being made (call it inside `tap_presses_on`): no look at an earlier
    /// tap counts for it, so its first time found off waits for its own notice.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn tap_made(&self) {
        self.tap_found_off.store(false, Ordering::Release);
    }

    /// The key tap is about to judge a left press made at `at` (its event time). Call it
    /// before `block_press`.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn note_left_down_at(&self, at: u64) {
        self.kept_down_at.store(at, Ordering::Release);
    }

    /// The mouse monitor saw a left press or release at `at` (event time) reach an app. The
    /// monitor sees no event the tap kept, so one after the kept press went down means that
    /// press is over: its release went by, or a new press went down, while the tap was off.
    /// The kept press is forgotten, so the new press's drags and release reach the app with
    /// it. Without this, a press that ended during an outage would keep the next one's.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn left_edge_reached_app(&self, at: u64) {
        if self.blocked_press.load(Ordering::Acquire)
            && at > self.kept_down_at.load(Ordering::Acquire)
        {
            self.forget_press();
        }
    }

    /// A left press at the key tap: when it is a draw-press (`blocks_draw_press`) it starts as
    /// `shift_down` would, and the tap keeps it, its drags and its release from the app.
    /// `held` is the event's modifiers (Carbon bits). `elsewhere` says the press lands on
    /// something that is not being recorded (`press_misses_target`). A press while an earlier
    /// kept one never saw its release (the tap was off for a moment) starts afresh.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn block_press(
        &self,
        screen_x: f64,
        screen_y: f64,
        held: u32,
        elsewhere: impl FnOnce() -> bool,
    ) -> bool {
        if self.blocked_press.load(Ordering::Acquire) {
            self.forget_press();
        }
        let setting = self.block_draw_clicks.load(Ordering::Acquire);
        let active = self.active.load(Ordering::Acquire);
        let draws = draws(self.draw_mask(), held);
        // Every left press on the Mac passes here: no lock until it could be a draw-press.
        if !(setting && active && draws) {
            return false;
        }
        let press = DrawPress {
            setting,
            active,
            draws,
            latched: self.draw_latch.load(Ordering::Acquire) & DRAW_LATCHED != 0,
            inside: self.contains_screen_point(screen_x, screen_y),
        };
        // Kept only once its pin is waiting: a press with nowhere to drop one passes.
        blocks_draw_press(press, elsewhere)
            && self.start_press(self.elapsed_ms(), screen_x, screen_y, true)
    }

    /// A kept press whose release the tap may not see is over: a press Annotator posted itself,
    /// or the tap was disabled, stopped or made again. Its drags and release pass, since a
    /// release the app never pressed for does no harm and a press it never sees released does.
    /// Its pin goes with it, so no later release is taken for it. A press the mouse monitor or
    /// the polled buttons started is theirs and stays.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn forget_press(&self) {
        let mut press = self
            .shift_press
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.blocked_press.store(false, Ordering::Release);
        if press.is_some_and(|press| press.by_tap) {
            *press = None;
        }
    }

    /// Whether the left button is down on a kept press, so a drag is kept too.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn press_blocked(&self) -> bool {
        self.blocked_press.load(Ordering::Acquire)
    }

    /// The left button came up: when its press was kept, the release is kept too, and it ends
    /// the press as `shift_up` would while the recording still takes input (a pause or finish
    /// during the press drops it). Returns whether the release is kept.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn release_press(&self, screen_x: f64, screen_y: f64) -> bool {
        if !self.blocked_press.swap(false, Ordering::AcqRel) {
            return false;
        }
        if self.active.load(Ordering::Acquire) {
            self.end_press(self.elapsed_ms(), screen_x, screen_y, true);
        } else {
            // Paused or finished during the press (a pause may have raced `block_press`).
            self.take_press(true);
        }
        true
    }

    /// The left button went down with only ⇧ held, as the mouse monitor or the polled buttons
    /// saw it: a pin if it comes up as a click (`shift_up`). Nothing while the key tap takes
    /// draw-presses: it has kept this one and records it, or it let it through to something
    /// that is not being recorded.
    fn shift_down(&self, t_ms: u64, screen_x: f64, screen_y: f64) {
        self.start_press(t_ms, screen_x, screen_y, false);
    }

    /// Starts a draw-press inside the target for the key tap (`by_tap`) or for the mouse
    /// monitor and the polled buttons, and says whether it did. Who owns draw-presses is read
    /// under the lock the press is stored under, and the tap owns them before it can deliver
    /// one (`tap_presses_on`), so a press never has two owners: however late a monitor's or a
    /// poll's press gets here, it starts nothing once the tap has taken over.
    fn start_press(&self, t_ms: u64, screen_x: f64, screen_y: f64, by_tap: bool) -> bool {
        if !self.contains_screen_point(screen_x, screen_y) {
            return false;
        }
        let frame = *self
            .target_frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut press = self
            .shift_press
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !by_tap && self.tap_owns_draw_presses() {
            return false;
        }
        *press = Some(PinPress {
            t_ms,
            x: screen_x,
            y: screen_y,
            frame,
            by_tap,
        });
        if by_tap {
            self.kept_at_event
                .store(self.tap_events.load(Ordering::Acquire), Ordering::Release);
            self.blocked_press.store(true, Ordering::Release);
        }
        true
    }

    /// The key tap was handed an event (not a notice that it was disabled). Call it first in
    /// the tap's callback, before the event is judged.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn note_tap_event(&self) {
        self.tap_events.fetch_add(1, Ordering::AcqRel);
    }

    /// Whether the kept press is the last event the key tap took. macOS times the tap out on
    /// the event its callback is busy with and hands no other event on until the tap is on
    /// again, so only that press can be the one the app got when the tap went off. A press
    /// with a later event after it was kept in time: the app never saw it go down.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    fn kept_press_is_last_event(&self) -> bool {
        self.kept_at_event.load(Ordering::Acquire) == self.tap_events.load(Ordering::Acquire)
    }

    /// Takes the waiting draw-press if `by_tap` says it is the caller's: the key tap ends the
    /// presses it kept, the mouse monitor and the polled buttons end theirs.
    fn take_press(&self, by_tap: bool) -> Option<PinPress> {
        let mut press = self
            .shift_press
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if press.is_some_and(|press| press.by_tap == by_tap) {
            press.take()
        } else {
            None
        }
    }

    /// The button of a ⇧-press came up, as the mouse monitor or the polled buttons saw it (with
    /// or without ⇧ still held; ⇧ is the draw modifier, unless Settings changed it). Returns
    /// whether a ⇧-press of theirs was waiting, so the release isn't also taken as a click's.
    fn shift_up(&self, t_ms: u64, screen_x: f64, screen_y: f64) -> bool {
        self.end_press(t_ms, screen_x, screen_y, false)
    }

    /// Ends the waiting draw-press of the key tap (`by_tap`) or of the mouse monitor and the
    /// polled buttons. Every ⇧-click drops the next numbered pin where it went down. Ink the
    /// hold drew before the press stays: it becomes a stroke that ends at the press, and ink
    /// after the click starts a new one. A ⇧-drag is left to the ink the poller is drawing.
    /// Returns whether a press was waiting.
    fn end_press(&self, t_ms: u64, screen_x: f64, screen_y: f64, by_tap: bool) -> bool {
        let Some(PinPress {
            t_ms: t0,
            x: x0,
            y: y0,
            frame: start_frame,
            ..
        }) = self.take_press(by_tap)
        else {
            return false;
        };
        let frame = *self
            .target_frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !same_target_frame(start_frame, frame)
            || !is_pin_click((t0, x0, y0), (t_ms, screen_x, screen_y))
        {
            return true;
        }
        // ⇧ draws as it's held. A line drawn before the press is kept without the click's
        // jitter; with nothing drawn first, the live ink is only that jitter and goes.
        let drawn = self
            .live_ink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .and_then(|ink| ink_before(ink, t0));
        if let Some(ink) = drawn {
            self.commit_ink(ink);
        }
        let (nx, ny, fw, fh) = self.normalize_point(x0, y0);
        let mut strokes = self
            .strokes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let number = strokes.iter().filter(|s| s.pin.is_some()).count() as u32 + 1;
        strokes.push(Stroke {
            t0,
            t1: t_ms,
            fw,
            fh,
            on: None,
            pin: Some(number),
            laser: false,
            pts: vec![
                (nx * SCALE as f32).round() as i32,
                (ny * SCALE as f32).round() as i32,
                0,
            ],
        });
        drop(strokes);
        self.window_log_request.store(true, Ordering::Release);
        let _ = self.ui_sender.try_send(LiveOverlayEvent::RequestSnapshot);
        true
    }

    /// Keeps finished ink as a stroke, and with app state on looks up what it was drawn on.
    fn commit_ink(&self, stroke: Stroke) {
        if self.probing() {
            let frame = *self
                .target_frame
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((x, y)) = crate::ax::stroke_probe_point(&stroke, frame) {
                crate::ax::Prober::shared().probe(crate::ax::Target::Stroke(stroke.t0), x, y);
            }
            // An app shot of what the mark is on.
            if let Some(walker) = crate::appshot::walker() {
                walker.request(crate::appshot::SETTLE);
            }
        }
        self.strokes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(stroke);
        let _ = self.ui_sender.try_send(LiveOverlayEvent::RequestSnapshot);
    }

    /// The helper process whose window is the capture target (the iPhone mirror).
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub fn set_capture_source_pid(&self, pid: u32) {
        self.capture_source_pid
            .store(i32::try_from(pid).unwrap_or(0), Ordering::Release);
    }

    /// Whether the topmost window at a global point is Scrawl's own or a `scrawl-helper`'s other
    /// than the capture source (ignoring the click-through ink overlay). The mouse monitor and
    /// the polled buttons ask once per press, and for scrolls, which come in bursts at one
    /// point, at most every 250 ms.
    fn on_scrawl_window(&self, x: f64, y: f64) -> bool {
        self.on_scrawl_window_in(x, y, platform::raw_windows_now, platform::is_helper_process)
    }

    /// `on_scrawl_window` with `windows` to read the window list (front to back). The answer
    /// is kept for 250 ms for the same point. Nothing but this reads it: it decides what is
    /// recorded, never what is kept from an app.
    fn on_scrawl_window_in(
        &self,
        x: f64,
        y: f64,
        windows: impl FnOnce() -> Vec<crate::windows::RawWindow>,
        is_helper: impl Fn(i32) -> bool,
    ) -> bool {
        let last = *self
            .scrawl_hit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, hx, hy, on)) = last
            && at.elapsed() < Duration::from_millis(250)
            && (hx - x).abs() <= 2.0
            && (hy - y).abs() <= 2.0
        {
            return on;
        }
        // The window list is read with no lock held: the main thread and the poller both ask.
        let on = self.scrawl_window_in(&windows(), x, y, is_helper);
        *self
            .scrawl_hit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((Instant::now(), x, y, on));
        on
    }

    /// Whether a press at a global point inside the target's rectangle lands on something that
    /// is not being recorded, so it goes there and is no draw-press: one of Annotator's own
    /// windows, a macOS prompt, or for a Window or iPhone capture any window but the recorded
    /// one. The key tap asks only for a press that is otherwise a draw-press, and reads the
    /// window list for each one: an answer for another press, however near in time and place,
    /// says nothing about this one (another window starts a point away, a prompt or a menu
    /// has just opened under the pointer).
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn press_misses_target(&self, x: f64, y: f64) -> bool {
        #[cfg(target_os = "macos")]
        let pressed = || platform::window_taking_press(x, y);
        #[cfg(not(target_os = "macos"))]
        let pressed = || None;
        self.press_misses_target_in(
            x,
            y,
            platform::raw_windows_now,
            platform::is_helper_process,
            pressed,
        )
    }

    /// `press_misses_target` with `windows` to read the window list (front to back) and
    /// `pressed` to ask the window server which window a press at the point goes to.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    fn press_misses_target_in(
        &self,
        x: f64,
        y: f64,
        windows: impl FnOnce() -> Vec<crate::windows::RawWindow>,
        is_helper: impl Fn(i32) -> bool,
        pressed: impl FnOnce() -> Option<u32>,
    ) -> bool {
        self.window_hit_in(&windows(), x, y, is_helper, pressed)
            .misses_target()
    }

    /// What is under a global point (`WindowHit`), going by a window list (front to back).
    /// `pressed` asks the window server which window a press at the point goes to
    /// (`windows::press_goes_to`).
    fn window_hit_in(
        &self,
        windows: &[crate::windows::RawWindow],
        x: f64,
        y: f64,
        is_helper: impl Fn(i32) -> bool,
        pressed: impl FnOnce() -> Option<u32>,
    ) -> WindowHit {
        let passthrough = self
            .passthrough_window_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let goes_to = |recorded: &dyn Fn(&crate::windows::RawWindow) -> bool| {
            crate::windows::press_goes_to(windows, x, y, &passthrough, recorded, pressed)
        };
        // Without the window or the process to look for, nothing says the press is on it.
        let target = match self.capture_mode.load(Ordering::Acquire) {
            2 => {
                let id = self.target_window_id.load(Ordering::Acquire);
                id != 0 && goes_to(&|w| w.id == id)
            }
            3 => {
                let pid = self.capture_source_pid.load(Ordering::Acquire);
                pid != 0 && goes_to(&|w| w.pid == pid)
            }
            _ => true,
        };
        WindowHit {
            scrawl: self.scrawl_window_in(windows, x, y, is_helper),
            prompt: crate::windows::system_prompt_at(windows, x, y),
            target,
        }
    }

    fn scrawl_window_in(
        &self,
        windows: &[crate::windows::RawWindow],
        x: f64,
        y: f64,
        is_helper: impl Fn(i32) -> bool,
    ) -> bool {
        let passthrough = self
            .passthrough_window_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let own = std::process::id() as i32;
        let source = self.capture_source_pid.load(Ordering::Acquire);
        crate::windows::owned_window_at(windows, x, y, &passthrough, |pid| {
            pid == own || (pid != source && is_helper(pid))
        })
    }

    pub fn normalize_point(&self, screen_x: f64, screen_y: f64) -> (f32, f32, u32, u32) {
        let frame = *self
            .target_frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let nx = ((screen_x - frame.x) / frame.width.max(1.0)).clamp(0.0, 1.0) as f32;
        let ny = ((screen_y - frame.y) / frame.height.max(1.0)).clamp(0.0, 1.0) as f32;
        (
            nx,
            ny,
            frame.width.round().max(1.0) as u32,
            frame.height.round().max(1.0) as u32,
        )
    }

    pub fn finish_active_laser(&self, _ax_point: Option<(f64, f64)>) {
        if let Some(mut laser) = self
            .active_laser
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            && !laser.pts.is_empty()
        {
            if let Some(last) = laser.pts.chunks_exact(3).last() {
                let lx = last[0] as f32 / SCALE as f32;
                let ly = last[1] as f32 / SCALE as f32;
                laser.t1 = laser.t0.saturating_add(last[2].max(0) as u64);
                self.events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(InteractionEvent::Point {
                        t: laser.t1,
                        x: lx,
                        y: ly,
                        label: None,
                    });
            }
            self.strokes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(laser);
        }
    }

    pub fn finish_session(&self) -> (Vec<InteractionEvent>, Vec<Stroke>) {
        self.active.store(false, Ordering::Release);
        self.selecting.store(false, Ordering::Release);
        self.paused.store(false, Ordering::Release);
        self.active_before_pause.store(false, Ordering::Release);
        self.end_gestures();
        let mut events = std::mem::take(
            &mut *self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut strokes = std::mem::take(
            &mut *self
                .strokes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if self.agent_context.swap(false, Ordering::AcqRel) {
            let hits = crate::ax::Prober::shared().finish();
            crate::ax::apply(&hits, &mut events, &mut strokes);
            if self.capture_mode.load(Ordering::Acquire) != 3 {
                let end = crate::now_epoch_ms();
                let start = end.saturating_sub(self.elapsed_ms());
                let page = crate::page::between(start, end);
                crate::page::apply(&page, start, &mut events, &mut strokes);
            }
        }
        (events, strokes)
    }
}

/// Clicks from the mouse buttons' polled state, while the mouse monitor (or hook) is missing: a
/// press starts a click (unless it lands on Scrawl's own window), a release ends it. A ⇧-press may
/// be a pin, as in the mouse monitor's handler.
fn poll_clicks(
    telemetry: &SharedCaptureTelemetry,
    prev: &mut [bool; 2],
    now: [bool; 2],
    (x, y): (f64, f64),
    pure_shift: bool,
    on_scrawl: impl Fn(f64, f64) -> bool,
) {
    let was = std::mem::replace(prev, now);
    if telemetry.mouse_live.load(Ordering::Acquire) {
        return;
    }
    for (button, (&down, &before)) in now.iter().zip(&was).enumerate() {
        let left = button == 0;
        if down && !before && !on_scrawl(x, y) {
            if !pure_shift {
                telemetry.pointer_down(telemetry.elapsed_ms(), x, y);
            } else if left {
                // Nothing while the key tap takes draw-presses (`shift_down`).
                telemetry.shift_down(telemetry.elapsed_ms(), x, y);
            }
        } else if !down && before {
            let shift_press = left && telemetry.shift_up(telemetry.elapsed_ms(), x, y);
            if !shift_press && !pure_shift {
                telemetry.pointer_up(telemetry.elapsed_ms(), x, y, !left);
            }
        }
    }
}

/// A press that moves less than this (points) before it's let go is a click, not a drag.
const CLICK_SLOP_PT: f32 = 12.0;
/// A ⇧-press held still for longer than this drops no pin.
const PIN_MAX_PRESS_MS: u64 = 1_000;
/// Ink spanning less than this (points) is a stationary ⇧ tap, not a stroke.
const MIN_INK_SPAN_PT: f32 = 6.0;

/// Whether a ⇧-press (`t`, global `x`, `y`) let go at `up` is a ⇧-click, which drops a pin.
fn is_pin_click(down: (u64, f64, f64), up: (u64, f64, f64)) -> bool {
    up.0.saturating_sub(down.0) <= PIN_MAX_PRESS_MS
        && ((up.1 - down.1).hypot(up.2 - down.2) as f32) < CLICK_SLOP_PT
}

/// Whether `stroke`'s points before `before_ms` reach at least `MIN_INK_SPAN_PT` from its start.
fn drew_before(stroke: &Stroke, before_ms: u64) -> bool {
    let Some(first) = stroke.pts.get(0..2) else {
        return false;
    };
    let mut points = 0_usize;
    let mut span = 0.0_f32;
    for p in stroke
        .pts
        .chunks_exact(3)
        .take_while(|p| stroke.t0.saturating_add(p[2].max(0) as u64) < before_ms)
    {
        points += 1;
        let dx = (p[0] - first[0]) as f32 / SCALE as f32 * stroke.fw as f32;
        let dy = (p[1] - first[1]) as f32 / SCALE as f32 * stroke.fh as f32;
        span = span.max(dx.hypot(dy));
    }
    points >= 2 && span >= MIN_INK_SPAN_PT
}

/// Whether finished ink is worth keeping as a stroke.
fn is_real_ink(stroke: &Stroke) -> bool {
    drew_before(stroke, u64::MAX)
}

/// The part of `stroke` drawn before `before_ms`, when that much is ink worth keeping.
fn ink_before(mut stroke: Stroke, before_ms: u64) -> Option<Stroke> {
    if !drew_before(&stroke, before_ms) {
        return None;
    }
    let at = |p: &[i32]| stroke.t0.saturating_add(p[2].max(0) as u64);
    let kept = stroke
        .pts
        .chunks_exact(3)
        .take_while(|p| at(p) < before_ms)
        .count();
    let end = at(&stroke.pts[(kept - 1) * 3..kept * 3]);
    stroke.pts.truncate(kept * 3);
    stroke.t1 = end;
    Some(stroke)
}

fn same_target_frame(a: TargetFrame, b: TargetFrame) -> bool {
    (a.x - b.x).abs() <= 0.5
        && (a.y - b.y).abs() <= 0.5
        && (a.width - b.width).abs() <= 0.5
        && (a.height - b.height).abs() <= 0.5
}

/// One poll of the Shift ink: holding Shift alone and moving the cursor inside the capture
/// target draws a persistent stroke; letting go of Shift commits it. `pure_shift` is Shift
/// without other modifiers (and not left over from a chord such as the capture shortcut).
fn track_shift_ink(
    telemetry: &SharedCaptureTelemetry,
    (x, y): (f64, f64),
    pure_shift: bool,
    prev_shift_ink: bool,
) {
    let inside_target = telemetry.contains_screen_point(x, y);
    if pure_shift && inside_target {
        telemetry.shift_held.store(true, Ordering::Release);
        let t_ms = telemetry.elapsed_ms();
        let (nx, ny, fw, fh) = telemetry.normalize_point(x, y);
        let sx = (nx * SCALE as f32).round() as i32;
        let sy = (ny * SCALE as f32).round() as i32;
        let mut guard = telemetry
            .live_ink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_none() {
            telemetry.window_log_request.store(true, Ordering::Release);
        }
        let stroke = guard.get_or_insert_with(|| Stroke {
            t0: t_ms,
            t1: t_ms,
            fw,
            fh,
            on: None,
            pin: None,
            laser: false,
            pts: Vec::new(),
        });
        let dt = t_ms.saturating_sub(stroke.t0) as i32;
        let should_push = stroke.pts.chunks_exact(3).last().is_none_or(|prev| {
            let dx = (sx - prev[0]) as f32 / SCALE as f32 * fw as f32;
            let dy = (sy - prev[1]) as f32 / SCALE as f32 * fh as f32;
            dx.hypot(dy) >= 1.5
        });
        if should_push && stroke.pts.len() / 3 < 2_400 {
            stroke.pts.extend_from_slice(&[sx, sy, dt]);
            stroke.t1 = t_ms;
        }
    } else if !pure_shift && prev_shift_ink {
        telemetry.shift_held.store(false, Ordering::Release);
        let finished = telemetry
            .live_ink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            // Ignore stationary Shift taps (e.g. typing a capital letter without moving the mouse).
            .filter(is_real_ink);
        if let Some(stroke) = finished {
            telemetry.commit_ink(stroke);
        }
    }
}

pub fn is_frontmost_terminal() -> bool {
    let Some((_pid, app, bundle_id)) = frontmost_app_info() else {
        return false;
    };
    let a = app.to_ascii_lowercase();
    let b = bundle_id.unwrap_or_default().to_ascii_lowercase();
    [
        "ghostty",
        "iterm",
        "terminal",
        "kitty",
        "wezterm",
        "alacritty",
        "warp",
        // Windows Terminal, the console host and the shells it runs.
        "windowsterminal",
        "conhost",
        "powershell",
        "pwsh",
        "cmd.exe",
        "mintty",
    ]
    .iter()
    .any(|needle| a.contains(needle) || b.contains(needle))
}

/// Writes all bundle artifacts (`scrawl.json`, `summary.md`, `contact-sheet.png`, `frames/frame-XX.png`)
/// into a new or specified `.scrawl` bundle directory.
pub fn write_bundle_artifacts(
    out_dir: &Path,
    bundle: &crate::model::ScrawlBundle,
    annotated_frames: &[(crate::model::Keyframe, RgbaFrame)],
    contact_sheet: &RgbaFrame,
    summary_md: &str,
) -> Result<()> {
    std::fs::create_dir_all(out_dir.join("frames"))
        .with_context(|| format!("could not create {}", out_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(out_dir, std::fs::Permissions::from_mode(0o700));
    }

    thread::scope(|scope| -> Result<()> {
        let workers: Vec<_> = annotated_frames
            .iter()
            .map(|(kf, frame)| {
                scope.spawn(move || -> Result<()> {
                    crate::render::save_png(frame, &out_dir.join(&kf.file))
                })
            })
            .collect();
        crate::render::save_png(contact_sheet, &out_dir.join(&bundle.contact_sheet))?;
        for worker in workers {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("PNG encoding worker panicked"))??;
        }
        Ok(())
    })?;

    let manifest_bytes = serde_json::to_vec_pretty(bundle)?;
    std::fs::write(out_dir.join("scrawl.json"), manifest_bytes)?;
    std::fs::write(out_dir.join("summary.md"), summary_md.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_stop_joins_worker_before_collecting_its_final_media() {
        let stopping = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(Mutex::new(CandidateBuffer::new(1 << 20)));
        let worker_stopping = Arc::clone(&stopping);
        let worker_frames = Arc::clone(&frames);
        let path = std::env::temp_dir().join(crate::make_bundle_id());
        let worker_path = path.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !worker_stopping.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::yield_now();
            }
            assert!(worker_stopping.load(Ordering::Acquire));
            worker_frames
                .lock()
                .unwrap()
                .push(RgbaFrame::new(42, 1, 1, vec![1, 2, 3, 255]), false);
            std::fs::write(&worker_path, b"finalized media").unwrap();
            Some(worker_path)
        });
        let session = ScreenCaptureSession {
            started_at: Instant::now(),
            stopping,
            urgent_snapshot: Arc::new(AtomicBool::new(false)),
            frames,
            video_offset_ms: Arc::new(AtomicU64::new(120)),
            sampler_thread: Some(worker),
            _recording: crate::retention::Recording::start(),
        };
        let recording = session.stop();
        let frames = recording.frames;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].t_ms, 42);
        assert_eq!(frames[0].rgba, vec![1, 2, 3, 255]);
        assert_eq!(recording.video.as_ref(), Some(&path));
        assert_eq!(recording.video_offset_ms, 120);
        std::fs::remove_file(path).unwrap();
    }

    /// Clicks on `scrawl-helper`'s windows (the SwiftUI capsule, Settings, onboarding) are
    /// Scrawl's own, except on the iPhone mirror, which is what's being recorded.
    #[test]
    fn helper_windows_are_scrawl_s_except_the_capture_source() {
        let (sender, _receiver) = flume::unbounded();
        let telemetry = SharedCaptureTelemetry::new(sender);
        let window = |id, pid, bounds| crate::windows::RawWindow {
            id,
            pid,
            owner: String::new(),
            title: String::new(),
            bounds,
            layer: 0,
            alpha: 1.0,
        };
        let own = std::process::id() as i32;
        let (settings, mirror, app) = (own + 1, own + 2, own + 3);
        let windows = [
            window(1, own, [0.0, 0.0, 1728.0, 1117.0]), // the ink overlay, click-through
            window(2, settings, [100.0, 100.0, 600.0, 400.0]),
            window(3, mirror, [1300.0, 100.0, 300.0, 650.0]),
            window(4, app, [0.0, 0.0, 1728.0, 1117.0]),
        ];
        telemetry.passthrough_window_ids.lock().unwrap().push(1);
        telemetry.set_capture_source_pid(mirror as u32);
        let helper = |pid| pid == settings || pid == mirror;
        let at = |x, y| telemetry.scrawl_window_in(&windows, x, y, helper);
        assert!(at(300.0, 300.0), "Settings");
        assert!(!at(1400.0, 400.0), "the iPhone mirror");
        assert!(!at(1000.0, 900.0), "the app under the ink overlay");
        telemetry.set_capture_source_pid(0);
        assert!(at(1400.0, 400.0), "a helper that isn't the source");
    }

    /// Kit's Screen capture on the Pro: the old listen-only event tap was off (no Input
    /// Monitoring), so his clicks on OpenCode's composer never reached the bundle. Clicks now
    /// come from the mouse monitor; polled button state stands in if it couldn't be installed.
    #[test]
    fn clicks_are_recorded_from_polled_buttons_while_the_mouse_monitor_is_missing() {
        let (sender, _receiver) = flume::unbounded();
        let telemetry = SharedCaptureTelemetry::new(sender);
        telemetry.reset_for_session(TargetFrame {
            x: 0.0,
            y: 0.0,
            width: 1728.0,
            height: 1117.0,
            scale: 2.0,
        });
        assert!(!telemetry.mouse_live.load(Ordering::Acquire));
        let mut prev = [false; 2];
        let not_scrawl = |_: f64, _: f64| false;
        let composer = (778.0, 1050.0);
        // A ⇧-click is a pin, not a click.
        poll_clicks(
            &telemetry,
            &mut prev,
            [true, false],
            composer,
            true,
            not_scrawl,
        );
        poll_clicks(
            &telemetry,
            &mut prev,
            [false, false],
            composer,
            true,
            not_scrawl,
        );
        // A click on the composer.
        poll_clicks(
            &telemetry,
            &mut prev,
            [true, false],
            composer,
            false,
            not_scrawl,
        );
        poll_clicks(
            &telemetry,
            &mut prev,
            [true, false],
            composer,
            false,
            not_scrawl,
        );
        poll_clicks(
            &telemetry,
            &mut prev,
            [false, false],
            composer,
            false,
            not_scrawl,
        );
        // Then the stop button, which is Scrawl's own.
        let stop = (860.0, 20.0);
        poll_clicks(&telemetry, &mut prev, [true, false], stop, false, |_, _| {
            true
        });
        poll_clicks(
            &telemetry,
            &mut prev,
            [false, false],
            stop,
            false,
            |_, _| true,
        );
        // While the mouse monitor is installed it reports clicks itself.
        telemetry.mouse_live.store(true, Ordering::Release);
        poll_clicks(
            &telemetry,
            &mut prev,
            [true, false],
            composer,
            false,
            not_scrawl,
        );
        poll_clicks(
            &telemetry,
            &mut prev,
            [false, false],
            composer,
            false,
            not_scrawl,
        );

        let (events, _) = telemetry.finish_session();
        assert_eq!(events.len(), 1, "{events:?}");
        let InteractionEvent::Click { x, y, button, .. } = &events[0] else {
            panic!("{events:?}");
        };
        assert!(
            (x - 0.45).abs() < 0.01 && (y - 0.94).abs() < 0.01,
            "{x}, {y}"
        );
        assert_eq!(button.as_deref(), Some("left"));
    }

    /// Two quick presses of the cancel key cancel a recording and one pauses it, so the UI
    /// loop must get each press, however late it looks.
    #[test]
    fn every_press_of_the_cancel_key_reaches_the_ui_loop() {
        let (sender, _receiver) = flume::unbounded();
        let telemetry = SharedCaptureTelemetry::new(sender);
        assert!(telemetry.take_cancel_presses().is_empty());
        telemetry.note_cancel();
        telemetry.note_cancel();
        let presses = telemetry.take_cancel_presses();
        assert_eq!(presses.len(), 2);
        assert!(presses[0] <= presses[1], "{presses:?}");
        assert!(telemetry.take_cancel_presses().is_empty());
        // The picker and a pending shortcut only ask whether it went down.
        telemetry.note_cancel();
        assert!(telemetry.take_escape());
        assert!(!telemetry.take_escape());
    }

    /// Pausing keeps the ink being drawn and records nothing more until the recording resumes.
    #[test]
    fn a_pause_keeps_the_stroke_in_progress_and_stops_input_until_resume() {
        let (sender, _receiver) = flume::unbounded();
        let telemetry = SharedCaptureTelemetry::new(sender);
        let frame = TargetFrame {
            x: 0.0,
            y: 0.0,
            width: 1728.0,
            height: 1117.0,
            scale: 2.0,
        };
        telemetry.reset_for_session(frame);
        track_shift_ink(&telemetry, (100.0, 100.0), true, false);
        track_shift_ink(&telemetry, (300.0, 240.0), true, true);
        telemetry.pause_input();
        assert!(!telemetry.active.load(Ordering::Acquire));
        assert!(telemetry.overlay_live(), "the ink stays on screen");
        assert!(telemetry.live_ink.lock().unwrap().is_none());
        assert_eq!(telemetry.strokes.lock().unwrap().len(), 1);
        telemetry.resume_input();
        assert!(telemetry.active.load(Ordering::Acquire));
        let (_, strokes) = telemetry.finish_session();
        assert_eq!(strokes.len(), 1);
        assert_eq!(strokes[0].pts.len(), 6);

        // An iPhone recording waits for its movie before it takes input; a pause and resume
        // before then don't start it early.
        telemetry.reset_for_session(frame);
        telemetry.active.store(false, Ordering::Release);
        telemetry.pause_input();
        telemetry.resume_input();
        assert!(!telemetry.active.load(Ordering::Acquire));
        assert!(!telemetry.overlay_live());
    }

    #[test]
    fn dev_defaults_register_none_of_the_live_apps_keys() {
        let default_in = |variant: Variant, kind: ShortcutKind| {
            let modifiers = variant.shortcut_modifiers();
            let key = if kind == ShortcutKind::Record {
                "S"
            } else {
                "V"
            };
            ShortcutConfig {
                key_code: u32::from(kind.ansi_code()),
                carbon_modifiers: modifiers,
                label: shortcut_label(modifiers, key),
            }
        };
        for kind in ShortcutKind::ALL {
            let live = hotkey_combos_in(Variant::Live, kind, &default_in(Variant::Live, kind));
            let dev = hotkey_combos_in(Variant::Dev, kind, &default_in(Variant::Dev, kind));
            assert_eq!(dev.len(), 1, "{kind:?}: no aliases in Dev");
            assert!(
                !live.contains(&dev[0]),
                "{kind:?}: {dev:?} is one of {live:?}"
            );
        }
        if cfg!(target_os = "macos") {
            assert_eq!(default_in(Variant::Dev, ShortcutKind::Record).label, "⌃⌥S");
            assert_eq!(default_in(Variant::Dev, ShortcutKind::Paste).label, "⌃⌥V");
        }
    }

    #[test]
    fn the_capture_shortcut_matches_its_aliases_and_a_custom_one() {
        let s = 1_u16; // the S key on this layout
        let default = ShortcutConfig {
            key_code: u32::from(s),
            carbon_modifiers: CARBON_OPTION_KEY | CARBON_SHIFT_KEY,
            label: "⌥⇧S".into(),
        };
        // (cmd, ctrl, alt, shift)
        let hit = |cfg: &ShortcutConfig, key, (cmd, ctrl, alt, shift)| {
            is_capture_shortcut(cfg, s, key, cmd, ctrl, alt, shift)
        };
        assert!(hit(&default, s, (false, false, true, true)), "⌥⇧S");
        assert!(hit(&default, s, (true, false, true, true)), "⌥⇧⌘S");
        assert!(hit(&default, s, (false, true, true, true)), "⌃⌥⇧S");
        assert!(hit(&default, s, (true, true, true, false)), "⌃⌥⌘S");
        assert!(hit(&default, s, (true, true, true, true)), "hyper S");
        assert!(hit(&default, KEY_S, (false, false, true, true)), "ANSI S");
        assert!(!hit(&default, s, (false, false, false, true)), "⇧S");
        assert!(!hit(&default, s, (true, false, false, false)), "⌘S");
        assert!(!hit(&default, 2, (false, false, true, true)), "⌥⇧D");

        let custom = ShortcutConfig {
            key_code: 15,
            carbon_modifiers: CARBON_CONTROL_KEY | CARBON_OPTION_KEY,
            label: "⌃⌥R".into(),
        };
        assert!(hit(&custom, 15, (false, true, true, false)));
        assert!(!hit(&custom, 15, (true, true, true, false)));
        assert!(!hit(&custom, 15, (false, false, true, false)));
        // Once changed, ⌥⇧S and its aliases are free for other apps.
        assert!(!hit(&custom, s, (false, false, true, true)), "⌥⇧S");
        assert!(!hit(&custom, KEY_S, (true, true, true, true)), "hyper S");
        assert_eq!(
            hotkey_combos(ShortcutKind::Record, &custom),
            [(15, custom.carbon_modifiers)]
        );
        assert_eq!(
            hotkey_combos(ShortcutKind::Record, &default).len(),
            if u32::from(KEY_S) == default.key_code {
                4
            } else {
                8
            },
            "⌥⇧S and three aliases, on the layout's key and the ANSI one"
        );

        // The paste shortcut works the same way.
        let v = 9_u16;
        let paste = ShortcutConfig {
            key_code: u32::from(v),
            carbon_modifiers: CARBON_OPTION_KEY | CARBON_SHIFT_KEY,
            label: "⌥⇧V".into(),
        };
        let paste_hit = |cfg: &ShortcutConfig, key, (cmd, ctrl, alt, shift)| {
            shortcut_matches(ShortcutKind::Paste, cfg, v, key, cmd, ctrl, alt, shift)
        };
        assert!(paste_hit(&paste, v, (false, false, true, true)), "⌥⇧V");
        assert!(paste_hit(&paste, v, (true, true, true, false)), "⌃⌥⌘V");
        assert!(
            !paste_hit(&paste, s, (false, false, true, true)),
            "⌥⇧S isn't paste"
        );
        let paste_custom = ShortcutConfig {
            key_code: 35,
            carbon_modifiers: CARBON_CONTROL_KEY | CARBON_OPTION_KEY,
            label: "⌃⌥P".into(),
        };
        assert!(paste_hit(&paste_custom, 35, (false, true, true, false)));
        assert!(!paste_hit(&paste_custom, v, (false, false, true, true)));
        assert!(
            paste.is_default_for(ShortcutKind::Paste)
                && !paste.is_default_for(ShortcutKind::Record)
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            modifier_symbols(CARBON_CMD_KEY | CARBON_CONTROL_KEY | CARBON_SHIFT_KEY),
            "⌃⇧⌘"
        );
        #[cfg(target_os = "windows")]
        assert_eq!(
            modifier_symbols(CARBON_CMD_KEY | CARBON_CONTROL_KEY | CARBON_SHIFT_KEY),
            "Ctrl+Win+Shift+"
        );

        assert_eq!(shortcut_parts("⌥⇧S"), ("⌥⇧", "S"));
        assert_eq!(shortcut_parts("⌃⌥⌘R"), ("⌃⌥⌘", "R"));
        assert_eq!(shortcut_parts("⌥⇧É"), ("⌥⇧", "É"));
        assert_eq!(shortcut_parts("Alt+Shift+S"), ("Alt+Shift", "S"));
        assert_eq!(shortcut_parts("Ctrl+Alt+Ä"), ("Ctrl+Alt", "Ä"));
        assert_eq!(shortcut_parts("Ctrl++"), ("Ctrl", "+"));
        assert_eq!(shortcut_parts(""), ("", ""));
    }

    #[test]
    fn recordings_use_the_display_under_the_cursor() {
        let displays = [[0.0, 0.0, 1728.0, 1117.0], [1728.0, -200.0, 2560.0, 1440.0]];
        assert_eq!(pick_display(&displays, Some([2000.0, 300.0]), None), 1);
        assert_eq!(pick_display(&displays, Some([100.0, 100.0]), None), 0);
        // No cursor: the display holding the focused window's centre.
        let focused = Some([1900.0, 0.0, 800.0, 600.0]);
        assert_eq!(pick_display(&displays, None, focused), 1);
        assert_eq!(pick_display(&displays, Some([-50.0, 0.0]), focused), 1);
        assert_eq!(pick_display(&displays, None, None), 0);
        // A display left of the laptop, at negative x.
        let left = [[0.0, 0.0, 1728.0, 1117.0], [-1920.0, 37.0, 1920.0, 1080.0]];
        assert_eq!(pick_display(&left, Some([-5.0, 600.0]), None), 1);
        // One above it, at negative y.
        let above = [[0.0, 0.0, 1728.0, 1117.0], [0.0, -1440.0, 2560.0, 1440.0]];
        assert_eq!(pick_display(&above, Some([10.0, -1.0]), None), 1);
        assert_eq!(pick_display(&above, Some([10.0, 0.0]), None), 0);
    }

    /// Clicks and ink on a display left of, right of or above the laptop normalize into that
    /// display, and land on the recording's pixels at its own scale.
    #[test]
    fn input_on_an_external_display_normalizes_into_it() {
        let (sender, _receiver) = flume::unbounded();
        let telemetry = SharedCaptureTelemetry::new(sender);
        let left = [-1920.0, 37.0, 1920.0, 1080.0];
        let right = [1728.0, 0.0, 1920.0, 1080.0];
        let above = [0.0, -1440.0, 2560.0, 1440.0];
        let small_left = [-1280.0, -100.0, 1280.0, 800.0];
        let below = [0.0, 1117.0, 1920.0, 1080.0];
        // (display, scale, global point, expected normalized point, recording size)
        let cases = [
            // 1x displays are recorded at their own pixels
            (left, 1.0, [-960.0, 577.0], (0.5, 0.5), (1920, 1080)),
            (right, 1.0, [2208.0, 270.0], (0.25, 0.25), (1920, 1080)),
            (below, 1.0, [480.0, 1387.0], (0.25, 0.25), (1920, 1080)),
            // 2x ones at the 2560 px long edge
            (above, 2.0, [640.0, -360.0], (0.25, 0.75), (2560, 1440)),
            (small_left, 2.0, [-320.0, 500.0], (0.75, 0.75), (2560, 1600)),
        ];
        for (rect, scale, point, expected, size) in cases {
            let frame = crate::screens::with_scale(rect, scale);
            let target = TargetInfo {
                mode: CaptureMode::Display,
                app: None,
                bundle_id: None,
                window_title: None,
                window_id: None,
                display_id: 2,
                frame,
            };
            // Windows frames are pixels already and never upscaled.
            if cfg!(target_os = "macos") {
                assert_eq!(capture_size(&target, 2560.0), size, "{rect:?}");
            }
            telemetry.reset_for_session(frame);
            assert!(telemetry.contains_screen_point(point[0], point[1]));
            // The laptop's centre is outside this target.
            assert!(!telemetry.contains_screen_point(864.0, 558.0));
            let (nx, ny, fw, fh) = telemetry.normalize_point(point[0], point[1]);
            assert_eq!((nx, ny), expected, "{rect:?}");
            assert_eq!((fw, fh), (rect[2] as u32, rect[3] as u32));
            // A click there is recorded at those coordinates.
            telemetry.pointer_down(10, point[0], point[1]);
            telemetry.pointer_up(20, point[0], point[1], false);
            let (events, _) = telemetry.finish_session();
            assert!(
                matches!(events[..], [InteractionEvent::Click { x, y, .. }] if (x, y) == (nx, ny)),
                "{events:?}"
            );
        }
        // In recording pixels: the left 1x display's centre is (960, 540).
        let frame = crate::screens::with_scale([-1920.0, 37.0, 1920.0, 1080.0], 1.0);
        telemetry.reset_for_session(frame);
        let (nx, ny, _, _) = telemetry.normalize_point(-960.0, 577.0);
        assert_eq!((nx * 1920.0, ny * 1080.0), (960.0, 540.0));
        // Above 2x: (640, -360) is a quarter in → (640, 1080) px of 2560×1440.
        let frame = crate::screens::with_scale([0.0, -1440.0, 2560.0, 1440.0], 2.0);
        telemetry.reset_for_session(frame);
        let (nx, ny, _, _) = telemetry.normalize_point(640.0, -360.0);
        assert_eq!((nx * 2560.0, ny * 1440.0), (640.0, 1080.0));
        telemetry.finish_session();
    }

    fn session(width: f64, height: f64) -> Arc<SharedCaptureTelemetry> {
        let (sender, _receiver) = flume::unbounded();
        let telemetry = SharedCaptureTelemetry::new(sender);
        telemetry.reset_for_session(TargetFrame {
            x: 0.0,
            y: 0.0,
            width,
            height,
            scale: 2.0,
        });
        telemetry
    }

    /// Settings' changes to Undo, the draw modifier and the cancel key reach the next key press
    /// without a restart.
    #[test]
    fn undo_cancel_and_draw_follow_settings_at_once() {
        use crate::settings::{CancelKey, DrawModifier, Shortcuts};
        let telemetry = session(1000.0, 1000.0);
        let menu = if cfg!(target_os = "windows") {
            CARBON_CONTROL_KEY
        } else {
            CARBON_CMD_KEY
        };
        let mut shortcuts = Shortcuts::default();
        telemetry.set_recording_keys(&shortcuts);
        let z = shortcuts.undo.key_code as u16;
        assert!(telemetry.is_undo(z, menu));
        assert!(telemetry.is_undo(KEY_Z, menu), "the ANSI Z too");
        assert!(!telemetry.is_undo(z, menu | CARBON_SHIFT_KEY));
        let esc = CancelKey::ESCAPE.key_code() as u16;
        assert!(telemetry.is_cancel(esc, 0));
        assert!(
            telemetry.is_cancel(esc, CARBON_OPTION_KEY | CARBON_SHIFT_KEY),
            "with the record shortcut's ⌥⇧ still down"
        );
        assert!(!telemetry.is_cancel(esc, CARBON_CMD_KEY));
        assert_eq!(telemetry.draw_mask(), CARBON_SHIFT_KEY);

        // A custom undo matches only itself; F5 cancels instead of Esc; ⌥ draws.
        let u = shortcuts.undo.key_code + 1;
        shortcuts.undo = ShortcutConfig {
            key_code: u,
            carbon_modifiers: CARBON_CONTROL_KEY | CARBON_OPTION_KEY,
            label: "⌃⌥U".into(),
        };
        shortcuts.cancel = CancelKey::parse("f5").unwrap();
        shortcuts.draw = DrawModifier::Option;
        telemetry.set_recording_keys(&shortcuts);
        assert!(telemetry.is_undo(u as u16, CARBON_CONTROL_KEY | CARBON_OPTION_KEY));
        assert!(!telemetry.is_undo(z, menu));
        assert!(!telemetry.is_undo(KEY_Z, menu));
        assert!(!telemetry.is_cancel(esc, 0));
        assert!(telemetry.is_cancel(shortcuts.cancel.key_code() as u16, 0));
        assert_eq!(telemetry.draw_mask(), CARBON_OPTION_KEY);
        assert_eq!(telemetry.controls().draw, DrawModifier::Option);

        // While Settings records a shortcut, the cancel key is the recorder's.
        telemetry.shortcuts_paused.store(true, Ordering::Release);
        assert!(!telemetry.is_cancel(shortcuts.cancel.key_code() as u16, 0));
    }

    /// Paused before the first candidate frame and finished without resuming: the candidates at
    /// 300 and 500 ms hold pixels recorded entirely during the pause, so neither is kept. The
    /// bundle still needs a frame, and gets a blank one of the same size.
    #[test]
    fn a_recording_paused_before_its_first_frame_keeps_no_paused_image() {
        let shown = |t_ms| RgbaFrame::new(t_ms, 2, 1, vec![200, 30, 30, 255, 30, 200, 30, 255]);
        let mut recording = ScreenRecording {
            frames: vec![shown(300), shown(500)],
            video: None,
            duration_ms: 900,
            video_offset_ms: 0,
        };
        recording.cut(&crate::pause::Cuts::new(vec![crate::pause::Span {
            start_ms: 100,
            end_ms: crate::pause::Span::OPEN,
        }]));
        assert_eq!(recording.frames.len(), 1);
        let frame = &recording.frames[0];
        assert_eq!(
            frame.rgba,
            [0, 0, 0, 255, 0, 0, 0, 255],
            "pixels from the pause stayed"
        );
        assert_eq!((frame.t_ms, frame.width, frame.height), (0, 2, 1));
        assert_eq!(recording.duration_ms, 100);

        // A frame from before the pause is kept as it is, with no blank beside it.
        let mut recording = ScreenRecording {
            frames: vec![shown(50), shown(300)],
            video: None,
            duration_ms: 900,
            video_offset_ms: 0,
        };
        recording.cut(&crate::pause::Cuts::new(vec![crate::pause::Span {
            start_ms: 100,
            end_ms: crate::pause::Span::OPEN,
        }]));
        assert_eq!(recording.frames.len(), 1);
        assert_eq!(recording.frames[0].rgba, shown(50).rgba);
    }

    /// With any shortcuts Settings accepts, no key press is both Undo and the cancel key (Undo
    /// on ⌥F5 with F5 as the cancel key would pause the recording), and the cancel key goes on
    /// working with ⌥ or ⇧ held: ⇧ is down while drawing.
    #[test]
    fn no_undo_settings_accepts_is_also_the_cancel_key() {
        use crate::settings::{CancelKey, Control, Shortcuts, check};
        let telemetry = session(1000.0, 1000.0);
        let all = [
            CARBON_CMD_KEY,
            CARBON_SHIFT_KEY,
            CARBON_OPTION_KEY,
            CARBON_CONTROL_KEY,
        ];
        let masks = || {
            (0..16_u32).map(|bits| {
                (0..4)
                    .filter(|bit| bits >> bit & 1 == 1)
                    .fold(0, |mask, bit| mask | all[bit])
            })
        };
        for cancel in CancelKey::all() {
            let key = cancel.key_code() as u16;
            for undo_modifiers in masks() {
                let mut shortcuts = Shortcuts::default();
                shortcuts.cancel = cancel;
                shortcuts.undo = ShortcutConfig {
                    key_code: cancel.key_code(),
                    carbon_modifiers: undo_modifiers,
                    label: shortcut_label(undo_modifiers, &cancel.label()),
                };
                telemetry.set_recording_keys(&shortcuts);
                for held in [0, CARBON_SHIFT_KEY, CARBON_OPTION_KEY] {
                    assert!(
                        telemetry.is_cancel(key, held),
                        "{} with {held:#x}",
                        cancel.label()
                    );
                }
                assert!(!telemetry.is_cancel(key, CARBON_CMD_KEY));
                assert!(!telemetry.is_cancel(key, CARBON_CONTROL_KEY));
                if check(&shortcuts, Control::Shortcut(ShortcutKind::Undo)).is_err() {
                    continue;
                }
                for held in masks() {
                    assert!(
                        !(telemetry.is_undo(key, held) && telemetry.is_cancel(key, held)),
                        "Settings accepts Undo {} with cancel key {}, and that press is both",
                        shortcuts.undo.label,
                        cancel.label()
                    );
                }
            }
        }
    }

    /// The draw modifier alone draws (laser, ink, pins); with another modifier it is part of a
    /// chord, and ink waits for it to come up.
    #[test]
    fn only_the_draw_modifier_alone_draws() {
        let opt = CARBON_OPTION_KEY;
        assert!(draws(opt, opt));
        assert!(!draws(opt, opt | CARBON_SHIFT_KEY));
        assert!(!draws(opt, CARBON_SHIFT_KEY));
        assert!(!draws(opt, 0));
        assert!(draw_in_chord(opt, opt | CARBON_SHIFT_KEY));
        assert!(!draw_in_chord(opt, opt));
        assert!(!draw_in_chord(opt, CARBON_SHIFT_KEY | CARBON_CMD_KEY));
        assert_eq!(
            carbon_modifiers(true, false, true, false),
            CARBON_CMD_KEY | CARBON_OPTION_KEY
        );
    }

    #[test]
    fn a_long_still_shift_press_drops_no_pin() {
        let down = (1_000, 10.0, 10.0);
        let slop = f64::from(CLICK_SLOP_PT);
        assert!(is_pin_click(down, (1_150, 14.0, 13.0)));
        assert!(is_pin_click(down, (1_000 + PIN_MAX_PRESS_MS, 10.0, 10.0)));
        assert!(!is_pin_click(down, (2_500, 10.0, 10.0)));
        assert!(!is_pin_click(down, (1_100, 10.0, 10.0 + slop)));
    }

    /// The poller draws ink while ⇧ is held. A ⇧-click after ink was drawn in the same hold
    /// drops a pin and keeps the line: the stroke ends where the button went down, without the
    /// click's own jitter, and it comes before its pin. A ⇧-click with nothing drawn first (only
    /// its jitter) drops a pin and leaves no stroke.
    #[test]
    fn a_shift_click_after_drawing_drops_a_pin_and_keeps_the_line() {
        let telemetry = session(1000.0, 1000.0);
        let ink = |pts: &[(i32, i32, i32)]| Stroke {
            t0: 0,
            t1: 0,
            fw: 1000,
            fh: 1000,
            on: None,
            pin: None,
            laser: false,
            pts: pts.iter().flat_map(|&(x, y, t)| [x, y, t]).collect(),
        };
        let press = telemetry.elapsed_ms() + 50;
        let p = press as i32;
        let s = SCALE;
        // ⇧ first, a line to the target, then a click at its end. The last point is the
        // click's jitter.
        let line = ink(&[
            (s / 10, s / 2, 0),
            (s / 5, s / 2, p - 20),
            (s / 5 + s / 500, s / 2, p + 5),
        ]);
        *telemetry.live_ink.lock().unwrap() = Some(line);
        telemetry.shift_down(press, 200.0, 500.0);
        assert!(telemetry.shift_up(press + 80, 201.0, 500.0));
        // Ink after the click starts a new stroke.
        assert!(telemetry.live_ink.lock().unwrap().is_none());

        // A line drawn long before the press is kept the same way.
        *telemetry.live_ink.lock().unwrap() = Some(ink(&[(s / 10, s / 10, 0), (s / 5, s / 10, 1)]));
        let t = telemetry.elapsed_ms() + 5_000;
        telemetry.shift_down(t, 200.0, 100.0);
        assert!(telemetry.shift_up(t + 60, 200.0, 100.0));

        // A ⇧-click with only its own few points of jitter is a pin alone.
        let jitter = ink(&[(s / 2, s / 2, 0), (s / 2 + 2, s / 2, 30)]);
        *telemetry.live_ink.lock().unwrap() = Some(jitter);
        let t = telemetry.elapsed_ms() + 6_000;
        telemetry.shift_down(t, 500.0, 500.0);
        assert!(telemetry.shift_up(t + 60, 500.0, 500.0));
        assert!(telemetry.live_ink.lock().unwrap().is_none());

        let (events, strokes) = telemetry.finish_session();
        assert!(events.is_empty(), "{events:?}");
        let kinds: Vec<_> = strokes.iter().map(|s| (s.pin, s.pts.len() / 3)).collect();
        assert_eq!(
            kinds,
            vec![
                (None, 2),
                (Some(1), 1),
                (None, 2),
                (Some(2), 1),
                (Some(3), 1)
            ]
        );
        // The first line ends at its last point before the press, where the pin is.
        assert_eq!(strokes[0].t1, press - 20);
        assert_eq!(
            strokes[0].normalized_points()[1],
            strokes[1].normalized_points()[0]
        );
    }

    /// A left press stops at Annotator only when it would drop a pin or draw ink: the setting
    /// is on, the recording takes input, the draw modifier alone is held and not left over from
    /// a chord, the press is inside the target and not on Annotator's own window.
    #[test]
    fn only_a_draw_press_inside_the_target_is_kept_from_the_app() {
        let yes = DrawPress {
            setting: true,
            active: true,
            draws: true,
            latched: false,
            inside: true,
        };
        assert!(blocks_draw_press(yes, || false));
        assert!(
            !blocks_draw_press(yes, || true),
            "on the capsule, a macOS prompt or another window"
        );
        for no in [
            DrawPress {
                setting: false,
                ..yes
            },
            DrawPress {
                active: false,
                ..yes
            },
            DrawPress {
                draws: false,
                ..yes
            },
            DrawPress {
                latched: true,
                ..yes
            },
            DrawPress {
                inside: false,
                ..yes
            },
        ] {
            let mut asked = false;
            assert!(!blocks_draw_press(no, || {
                asked = true;
                false
            }));
            assert!(!asked, "the window list is asked only for a draw-press");
        }
    }

    /// A kept press drops its pin at the release, once. A press kept when the recording pauses
    /// keeps its drags and release from the app until the button comes up, and drops nothing.
    /// Presses outside the target, during a chord or with the setting off pass.
    #[test]
    fn a_kept_press_drops_its_pin_and_keeps_its_release_past_a_pause() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        assert!(telemetry.block_press(200.0, 300.0, draw, || false));
        assert!(telemetry.press_blocked());
        assert!(telemetry.release_press(201.0, 300.0));
        assert!(!telemetry.press_blocked());
        assert!(
            !telemetry.release_press(201.0, 300.0),
            "a release passes after"
        );

        // Not the draw modifier alone, outside the target, or on Annotator: passes.
        assert!(!telemetry.block_press(200.0, 300.0, 0, || false));
        assert!(!telemetry.block_press(2000.0, 300.0, draw, || false));
        assert!(!telemetry.block_press(200.0, 300.0, draw, || true));
        // Left over from a chord (⌘ came down with it, then went up): passes.
        let chord = draw | CARBON_CMD_KEY;
        assert!(telemetry.note_tap_modifiers(chord));
        assert!(telemetry.note_tap_modifiers(draw));
        assert!(!telemetry.draws_now(draw));
        assert!(!telemetry.block_press(200.0, 300.0, draw, || false));
        // Let go and pressed again alone: draws and is kept.
        assert!(!telemetry.note_tap_modifiers(0));
        assert!(!telemetry.note_tap_modifiers(draw));
        assert!(telemetry.draws_now(draw));
        telemetry.block_draw_clicks.store(false, Ordering::Release);
        assert!(!telemetry.block_press(200.0, 300.0, draw, || false));
        telemetry.block_draw_clicks.store(true, Ordering::Release);

        // Paused mid-press: the release is still kept, and no pin drops.
        assert!(telemetry.block_press(400.0, 400.0, draw, || false));
        telemetry.pause_input();
        assert!(telemetry.press_blocked());
        assert!(telemetry.release_press(400.0, 400.0));
        // While paused nothing is kept.
        assert!(!telemetry.block_press(400.0, 400.0, draw, || false));
        telemetry.resume_input();

        // A kept press whose release never came is over at the next press, and when the tap
        // was disabled in between.
        assert!(telemetry.block_press(500.0, 500.0, draw, || false));
        assert!(!telemetry.block_press(500.0, 500.0, 0, || false));
        assert!(!telemetry.press_blocked());
        assert!(telemetry.block_press(500.0, 500.0, draw, || false));
        telemetry.forget_press();
        assert!(!telemetry.press_blocked());
        assert!(!telemetry.release_press(500.0, 500.0));

        let (_, strokes) = telemetry.finish_session();
        let pins: Vec<_> = strokes.iter().filter_map(|s| s.pin).collect();
        assert_eq!(pins, vec![1]);
    }

    /// With the key tap taking draw-presses, the polled buttons leave them alone, so one click
    /// is one pin even when both see it.
    #[test]
    fn polled_buttons_leave_draw_presses_to_the_tap() {
        let telemetry = session(1000.0, 1000.0);
        assert!(telemetry.tap_presses_on(|| true));
        let draw = telemetry.draw_mask();
        let mut prev = [false; 2];
        assert!(telemetry.block_press(300.0, 300.0, draw, || false));
        poll_clicks(
            &telemetry,
            &mut prev,
            [true, false],
            (300.0, 300.0),
            true,
            |_, _| false,
        );
        assert!(telemetry.release_press(300.0, 300.0));
        poll_clicks(
            &telemetry,
            &mut prev,
            [false, false],
            (300.0, 300.0),
            true,
            |_, _| false,
        );
        poll_clicks(
            &telemetry,
            &mut prev,
            [true, false],
            (300.0, 300.0),
            true,
            |_, _| false,
        );
        poll_clicks(
            &telemetry,
            &mut prev,
            [false, false],
            (300.0, 300.0),
            true,
            |_, _| false,
        );
        assert_eq!(pins(&telemetry), vec![1]);
    }

    /// The pins of a finished session, in order.
    fn pins(telemetry: &SharedCaptureTelemetry) -> Vec<u32> {
        let (_, strokes) = telemetry.finish_session();
        strokes.iter().filter_map(|s| s.pin).collect()
    }

    /// The key tap owns draw-presses from before it is made or enabled until after it is off,
    /// so it never delivers a press while the mouse monitor and the polled buttons still start
    /// draw-presses. A tap that did not come on owns nothing.
    #[test]
    fn the_tap_owns_draw_presses_before_it_can_deliver_one() {
        let telemetry = session(1000.0, 1000.0);
        assert!(!telemetry.tap_owns_draw_presses(), "no tap yet");
        let on = telemetry.tap_presses_on(|| {
            assert!(telemetry.tap_owns_draw_presses(), "before the tap is on");
            true
        });
        assert!(on && telemetry.tap_owns_draw_presses());
        telemetry.tap_presses_off(|| {
            assert!(telemetry.tap_owns_draw_presses(), "until the tap is off");
        });
        assert!(!telemetry.tap_owns_draw_presses());
        // Secure Input, or no tap to enable: the others keep recording.
        assert!(!telemetry.tap_presses_on(|| false));
        assert!(!telemetry.tap_owns_draw_presses());
        // `blockDrawClicks` off: the tap keeps nothing, so it owns nothing.
        assert!(telemetry.tap_presses_on(|| true));
        telemetry.block_draw_clicks.store(false, Ordering::Release);
        assert!(!telemetry.tap_owns_draw_presses());
    }

    /// Review r2-s4-2: a tap just made kept a ⇧-click while the polled buttons, not yet told
    /// (`tap_live` came 500 ms later), counted the same click: the poll saw the button down,
    /// waited on the window list while the tap dropped pin 1 at the release, then started the
    /// press again and dropped pin 2 at its next sample.
    #[test]
    fn a_tap_just_made_and_the_polled_buttons_count_one_click_once() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        let at = (300.0, 300.0);
        // As `spawn_key_tap` makes it, and nothing else has run since.
        assert!(telemetry.tap_presses_on(|| true));
        assert!(telemetry.block_press(at.0, at.1, draw, || false));
        let mut prev = [false; 2];
        poll_clicks(&telemetry, &mut prev, [true, false], at, true, |x, y| {
            // The poll is here, in the window list, when the tap sees the release.
            assert!(telemetry.release_press(x, y));
            false
        });
        poll_clicks(&telemetry, &mut prev, [false, false], at, true, |_, _| {
            false
        });
        assert_eq!(pins(&telemetry), vec![1]);
    }

    /// The same click with the poll one step earlier: it saw the button down before the tap was
    /// made. Whatever it decided then, it starts no press once the tap owns them, and leaves
    /// nothing behind that a later release would be taken for.
    #[test]
    fn a_poll_that_falls_behind_a_new_tap_starts_no_press() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        let at = (300.0, 300.0);
        let mut prev = [false; 2];
        poll_clicks(&telemetry, &mut prev, [true, false], at, true, |x, y| {
            assert!(telemetry.tap_presses_on(|| true));
            assert!(telemetry.block_press(x, y, draw, || false));
            assert!(telemetry.release_press(x, y));
            false
        });
        poll_clicks(&telemetry, &mut prev, [false, false], at, true, |_, _| {
            false
        });
        // The tap goes (Accessibility off): the next plain click is a click.
        telemetry.tap_presses_off(|| {});
        poll_clicks(&telemetry, &mut prev, [true, false], at, false, |_, _| {
            false
        });
        poll_clicks(&telemetry, &mut prev, [false, false], at, false, |_, _| {
            false
        });
        let (events, strokes) = telemetry.finish_session();
        assert!(
            matches!(events[..], [InteractionEvent::Click { .. }]),
            "{events:?}"
        );
        let pins: Vec<_> = strokes.iter().filter_map(|s| s.pin).collect();
        assert_eq!(pins, vec![1]);
    }

    /// A press has one owner from its start to its end. One the polled buttons started before
    /// the tap came on still drops its pin (the tap never saw it go down), and the tap's own
    /// press is not ended by a polled release.
    #[test]
    fn a_press_is_ended_by_whoever_started_it() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        let mut prev = [false; 2];
        poll_clicks(
            &telemetry,
            &mut prev,
            [true, false],
            (300.0, 300.0),
            true,
            |_, _| false,
        );
        assert!(telemetry.tap_presses_on(|| true));
        assert!(
            !telemetry.release_press(300.0, 300.0),
            "not the tap's: it passes"
        );
        poll_clicks(
            &telemetry,
            &mut prev,
            [false, false],
            (300.0, 300.0),
            true,
            |_, _| false,
        );

        assert!(telemetry.block_press(600.0, 600.0, draw, || false));
        assert!(!telemetry.shift_up(telemetry.elapsed_ms(), 600.0, 600.0));
        assert!(telemetry.release_press(600.0, 600.0));
        assert_eq!(pins(&telemetry), vec![1, 2]);
    }

    /// Review r2-s3 #2: a kept press that was forgotten (the tap timed out on it) left its pin
    /// waiting. With the tap gone later, the next plain release took it: that click was not
    /// recorded and dropped a pin. A forgotten press takes its pin with it, and a press the
    /// polled buttons started is not the tap's to forget.
    #[test]
    fn a_forgotten_kept_press_leaves_no_pin_waiting() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        let at = (500.0, 500.0);
        assert!(telemetry.tap_presses_on(|| true));
        assert!(telemetry.block_press(at.0, at.1, draw, || false));
        telemetry.forget_press();
        assert!(!telemetry.release_press(at.0, at.1), "its release passes");
        telemetry.tap_presses_off(|| {});
        let mut prev = [false; 2];
        poll_clicks(&telemetry, &mut prev, [true, false], at, false, |_, _| {
            false
        });
        poll_clicks(&telemetry, &mut prev, [false, false], at, false, |_, _| {
            false
        });
        // A ⇧-press of the polled buttons outlives a forget.
        poll_clicks(&telemetry, &mut prev, [true, false], at, true, |_, _| false);
        telemetry.forget_press();
        poll_clicks(&telemetry, &mut prev, [false, false], at, true, |_, _| {
            false
        });
        let (events, strokes) = telemetry.finish_session();
        assert!(
            matches!(events[..], [InteractionEvent::Click { .. }]),
            "{events:?}"
        );
        let pins: Vec<_> = strokes.iter().filter_map(|s| s.pin).collect();
        assert_eq!(pins, vec![1]);
    }

    /// Review r2-s4-1: the poller read ⌘⇧, was held up, and wrote its old sample over what the
    /// key tap had seen since (both let go, then ⇧ alone), so the whole fresh ⇧ hold drew
    /// nothing and its click reached the app. A sample taken before the tap's last modifier
    /// event does not count.
    #[test]
    fn a_late_polled_sample_does_not_undo_the_taps_modifier_events() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        let chord = draw | CARBON_CMD_KEY;

        // A fresh hold, and a poll that still holds the chord it read before.
        assert!(telemetry.note_polled_modifiers(telemetry.draw_latch_mark(), chord));
        let mark = telemetry.draw_latch_mark();
        telemetry.note_tap_modifiers(draw);
        telemetry.note_tap_modifiers(0);
        assert!(!telemetry.note_tap_modifiers(draw));
        assert!(
            !telemetry.note_polled_modifiers(mark, chord),
            "the tap's answer"
        );
        assert!(telemetry.draws_now(draw));
        assert!(telemetry.block_press(200.0, 300.0, draw, || false));
        assert!(telemetry.release_press(200.0, 300.0));

        // The other way round: a poll that read no modifier, then ⌘⇧ and ⌘ let go at the tap.
        telemetry.note_tap_modifiers(0);
        let mark = telemetry.draw_latch_mark();
        telemetry.note_tap_modifiers(chord);
        assert!(telemetry.note_tap_modifiers(draw));
        assert!(telemetry.note_polled_modifiers(mark, 0), "the tap's answer");
        assert!(!telemetry.draws_now(draw), "left over from the chord");
        assert!(!telemetry.block_press(200.0, 300.0, draw, || false));

        // A sample taken after the tap's last event counts, and so does every sample when
        // there is no tap: the chord's leftover ends when the poller sees the modifier up.
        assert!(telemetry.note_polled_modifiers(telemetry.draw_latch_mark(), draw));
        assert!(!telemetry.note_polled_modifiers(telemetry.draw_latch_mark(), 0));
        assert!(!telemetry.note_polled_modifiers(telemetry.draw_latch_mark(), draw));
        assert!(telemetry.draws_now(draw));
        assert!(telemetry.note_polled_modifiers(telemetry.draw_latch_mark(), chord));
        assert!(telemetry.note_polled_modifiers(telemetry.draw_latch_mark(), draw));
        assert!(!telemetry.draws_now(draw));
        assert_eq!(pins(&telemetry), vec![1]);
    }

    /// Review r3-s4-2: the poller reads the keys as they are now, which can be later than a
    /// press still waiting at the key tap. ⌘⇧, ⌘ let go, a click, ⇧ let go: the poller saw no
    /// modifier before the tap got to the click and cleared the latch, and the tap then kept a
    /// click that was left over from the chord. While the tap is on a polled sample sets the
    /// latch and never clears it. The tap's own ⇧-up does, after the click.
    #[test]
    fn a_polled_sample_never_clears_the_latch_while_the_tap_is_on() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        let chord = draw | CARBON_CMD_KEY;
        let polled = |held| telemetry.note_polled_modifiers(telemetry.draw_latch_mark(), held);
        assert!(telemetry.tap_presses_on(|| true));

        // The tap has seen ⌘⇧ and ⌘ let go. The click and the ⇧-up still wait at it.
        telemetry.note_tap_modifiers(chord);
        assert!(telemetry.note_tap_modifiers(draw));
        assert!(polled(0), "the keys are up by now, the latch stays");
        assert!(!telemetry.draws_now(draw));
        assert!(
            !telemetry.block_press(200.0, 300.0, draw, || false),
            "left over from the chord"
        );
        // The tap gets to the ⇧-up: the next hold draws, and its click is kept.
        assert!(!telemetry.note_tap_modifiers(0));
        assert!(!telemetry.note_tap_modifiers(draw));
        assert!(telemetry.block_press(200.0, 300.0, draw, || false));
        assert!(telemetry.release_press(200.0, 300.0));
        telemetry.note_tap_modifiers(0);

        // A chord the tap never saw (Secure Input, a tap that was off for a moment) latches
        // from a poll, and stays latched until the tap sees the modifier up: presses pass.
        assert!(polled(chord));
        assert!(polled(0));
        assert!(polled(draw), "a new hold, and the tap saw none of it");
        assert!(!telemetry.block_press(200.0, 300.0, draw, || false));
        assert!(!telemetry.note_tap_modifiers(0));

        // Without a tap the poller is the only writer, and its sample clears the latch.
        assert!(polled(chord));
        telemetry.tap_presses_off(|| {});
        assert!(!polled(0));
        assert_eq!(pins(&telemetry), vec![1]);
    }

    /// Review r3-s4-3: the poller found the tap disabled and was held up before it forgot the
    /// kept press. The tap's own notice brought the tap back meanwhile, and the tap kept a new
    /// press. The poller then forgot that one: its drags and release reached the app, and no
    /// pin dropped. The poller forgets only for a tap that is still off when it looks, in one
    /// step with enabling it. The tap's notice always forgets: a press kept before it may be
    /// the one the tap timed out on, which the app got.
    #[test]
    fn a_late_look_at_the_tap_does_not_forget_a_press_kept_since_it_came_back() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        assert!(telemetry.tap_presses_on(|| true));

        // A kept press, and macOS disables the tap. Its notice brings it back.
        assert!(telemetry.block_press(200.0, 200.0, draw, || false));
        assert!(telemetry.recover_tap(
            TapOff::Timeout,
            || false,
            || {
                assert!(!telemetry.press_blocked(), "forgotten before the tap is on");
                true
            }
        ));
        assert!(!telemetry.release_press(200.0, 200.0), "its release passes");
        // The tap keeps a new press. The poller's look comes now and finds the tap on.
        assert!(telemetry.block_press(600.0, 600.0, draw, || false));
        assert!(telemetry.recover_tap(TapOff::Found, || true, || true));
        assert!(telemetry.press_blocked(), "its drags are still kept");
        assert!(telemetry.release_press(600.0, 600.0));

        // The poller finds the tap off and leaves it for the notice. Still off at its next
        // look, with no notice: the kept press is over.
        assert!(telemetry.block_press(400.0, 400.0, draw, || false));
        assert!(!telemetry.recover_tap(TapOff::Found, || false, || true));
        assert!(telemetry.press_blocked(), "kept until the next look");
        assert!(telemetry.recover_tap(TapOff::Found, || false, || true));
        assert!(!telemetry.release_press(400.0, 400.0));
        // The tap timed out on a press and the app got it. The poller brought the tap back,
        // and only then did the slow callback keep the press. The notice comes next.
        assert!(telemetry.block_press(400.0, 400.0, draw, || false));
        assert!(telemetry.recover_tap(TapOff::Timeout, || true, || true));
        assert!(!telemetry.release_press(400.0, 400.0), "it passes too");

        // A tap that does not come back owns no press.
        assert!(!telemetry.recover_tap(TapOff::Found, || false, || false));
        assert!(!telemetry.recover_tap(TapOff::Found, || false, || false));
        assert!(!telemetry.tap_owns_draw_presses());
        assert_eq!(pins(&telemetry), vec![1]);
    }

    /// Review r4-s4-1: the tap kept a draw-press. With the button still down the draw modifier
    /// was let go, and macOS timed the tap out on that event. Its notice forgot the kept press,
    /// so the drags and the release after it reached the app, which never saw the press go
    /// down. The tap timed out on a later event, so that press stays kept. One the tap timed
    /// out on itself is still forgotten, by the notice or by the poller.
    #[test]
    fn a_press_kept_before_the_event_the_tap_timed_out_on_stays_kept() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        assert!(telemetry.tap_presses_on(|| true));

        telemetry.note_tap_event();
        assert!(telemetry.block_press(200.0, 200.0, draw, || false));
        telemetry.note_tap_event(); // the draw modifier let go: the tap times out on it
        telemetry.note_tap_modifiers(0);
        assert!(telemetry.recover_tap(TapOff::Timeout, || false, || true));
        assert!(telemetry.press_blocked(), "its drags are kept");
        assert!(telemetry.release_press(200.0, 200.0), "its release is kept");
        // The poller finds the tap off after the same kind of outage.
        telemetry.note_tap_event();
        assert!(telemetry.block_press(300.0, 300.0, draw, || false));
        telemetry.note_tap_event();
        assert!(!telemetry.recover_tap(TapOff::Found, || false, || true));
        assert!(telemetry.recover_tap(TapOff::Found, || false, || true));
        assert!(telemetry.release_press(300.0, 300.0));

        // The tap timed out on the press itself: the app got it, so its release passes.
        telemetry.note_tap_event();
        assert!(telemetry.block_press(400.0, 400.0, draw, || false));
        assert!(telemetry.recover_tap(TapOff::Timeout, || false, || true));
        assert!(!telemetry.press_blocked());
        telemetry.note_tap_event();
        assert!(telemetry.block_press(500.0, 500.0, draw, || false));
        assert!(!telemetry.recover_tap(TapOff::Found, || false, || true));
        assert!(telemetry.recover_tap(TapOff::Found, || false, || true));
        assert!(!telemetry.press_blocked());
        assert_eq!(pins(&telemetry), vec![1, 2]);
    }

    /// Review r6-s4-1: the tap kept a draw-press and timed out on a later event. While it was
    /// off, that press came up and a new one went down, both reaching the app. The tap came
    /// back still keeping the old press, so it kept the new press's drags and release, and the
    /// app was left with a press that never came up. The mouse monitor saw the new press reach
    /// the app, so the kept press is over. A release or press from before it changes nothing.
    #[test]
    fn a_press_that_reached_the_app_during_an_outage_ends_the_kept_one() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        assert!(telemetry.tap_presses_on(|| true));

        telemetry.note_tap_event();
        telemetry.note_left_down_at(100);
        assert!(telemetry.block_press(200.0, 200.0, draw, || false));
        telemetry.left_edge_reached_app(90); // an older click, seen late
        assert!(telemetry.press_blocked());
        telemetry.note_tap_event(); // the tap times out on a later event
        // The tap is off: the kept press comes up and a new one goes down, past the tap.
        telemetry.left_edge_reached_app(150);
        telemetry.left_edge_reached_app(160);
        assert!(telemetry.recover_tap(TapOff::Timeout, || false, || true));
        assert!(
            !telemetry.press_blocked(),
            "the new press's drags reach the app"
        );
        assert!(!telemetry.release_press(300.0, 300.0), "and its release");
        assert_eq!(pins(&telemetry), Vec::<u32>::new());
    }

    /// Review r6-s4-2: the poller found a tap off, and that tap was stopped and a new one made
    /// before it looked again. The new tap's first time found off counted as the old tap's
    /// second, so it was taken as timed out before its own notice came. Each tap starts afresh.
    #[test]
    fn a_new_tap_waits_for_its_own_notice() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        assert!(telemetry.tap_presses_on(|| true));
        assert!(!telemetry.recover_tap(TapOff::Found, || false, || true));
        telemetry.tap_presses_off(|| {});
        assert!(telemetry.tap_presses_on(|| {
            telemetry.tap_made();
            true
        }));

        telemetry.note_tap_event();
        assert!(telemetry.block_press(200.0, 200.0, draw, || false));
        assert!(!telemetry.recover_tap(TapOff::Found, || false, || true));
        assert!(telemetry.recover_tap(TapOff::UserInput, || false, || true));
        assert!(telemetry.release_press(200.0, 200.0), "its release is kept");
        assert_eq!(pins(&telemetry), vec![1]);
    }

    /// Review r5-s4-1: the tap kept a draw-press, and with the button still down macOS turned
    /// the tap off for the user, not for a slow callback. Recovery forgot the press, so its
    /// drags and release reached the app without their press. No callback was cut short, so
    /// the press stays kept, also when the poller finds the tap off before the notice comes.
    #[test]
    fn a_tap_turned_off_for_the_user_keeps_its_press() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        assert!(telemetry.tap_presses_on(|| true));

        telemetry.note_tap_event();
        assert!(telemetry.block_press(200.0, 200.0, draw, || false));
        assert!(telemetry.recover_tap(TapOff::UserInput, || false, || true));
        assert!(telemetry.press_blocked(), "its drags are kept");
        assert!(telemetry.release_press(200.0, 200.0), "its release is kept");

        // The poller looks first, then the notice comes and brings the tap back.
        telemetry.note_tap_event();
        assert!(telemetry.block_press(300.0, 300.0, draw, || false));
        assert!(!telemetry.recover_tap(TapOff::Found, || false, || true));
        assert!(telemetry.recover_tap(TapOff::UserInput, || false, || true));
        assert!(telemetry.recover_tap(TapOff::Found, || true, || true));
        assert!(telemetry.press_blocked());
        assert!(telemetry.release_press(300.0, 300.0));
        assert_eq!(pins(&telemetry), vec![1, 2]);
    }

    /// Review r2-s3 #1: "inside the target" was its rectangle only, so a ⇧-click on a macOS
    /// prompt, or on another app's window floating over a recorded window, was kept from it and
    /// dropped a pin on content nobody pointed at. Such a press passes. One on the recorded
    /// window is kept, also under another app's overlay that lets presses through.
    #[test]
    fn a_draw_press_on_a_prompt_or_on_a_window_over_the_recorded_one_passes() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        let window = |id, pid, owner: &str, bounds, layer| crate::windows::RawWindow {
            id,
            pid,
            owner: owner.into(),
            title: String::new(),
            bounds,
            layer,
            alpha: 1.0,
        };
        let own = std::process::id() as i32;
        let (safari, slack, mirror) = (own + 1, own + 2, own + 3);
        let windows = [
            window(1, own, "Annotator", [0.0, 0.0, 1000.0, 1000.0], 30), // the ink overlay
            window(
                2,
                own + 4,
                "SecurityAgent",
                [400.0, 400.0, 200.0, 100.0],
                1000,
            ),
            window(3, own + 5, "Wispr Flow", [0.0, 800.0, 300.0, 200.0], 1000), // lets presses through
            window(4, slack, "Slack", [600.0, 0.0, 400.0, 300.0], 0),
            window(6, mirror, "scrawl-helper", [700.0, 500.0, 300.0, 500.0], 0),
            window(5, safari, "Safari", [0.0, 0.0, 1000.0, 1000.0], 0),
        ];
        telemetry.passthrough_window_ids.lock().unwrap().push(1);
        // What the window server answers: the overlay lets a press through to Safari.
        let pressed = |x: f64, y: f64| {
            windows
                .iter()
                .filter(|w| w.id != 1 && w.id != 3)
                .find(|w| {
                    let [wx, wy, ww, wh] = w.bounds;
                    x >= wx && x <= wx + ww && y >= wy && y <= wy + wh
                })
                .map(|w| w.id)
        };
        let kept = |x: f64, y: f64| {
            let asked = std::cell::Cell::new(false);
            let kept = telemetry.block_press(x, y, draw, || {
                asked.set(true);
                telemetry
                    .window_hit_in(&windows, x, y, |_| false, || pressed(x, y))
                    .misses_target()
            });
            assert!(asked.get(), "a draw-press inside the rectangle asks");
            assert_eq!(telemetry.release_press(x, y), kept);
            kept
        };
        let (on_safari, on_prompt, on_slack, under_overlay, on_mirror) = (
            (100.0, 100.0),
            (500.0, 450.0),
            (800.0, 100.0),
            (100.0, 900.0),
            (800.0, 700.0),
        );

        // A Screen capture: every window is part of it, a macOS prompt is not.
        telemetry.set_capture_target(CaptureMode::Display, None);
        assert!(kept(on_safari.0, on_safari.1));
        assert!(kept(on_slack.0, on_slack.1));
        assert!(!kept(on_prompt.0, on_prompt.1), "answering macOS");

        // Safari's window is recorded.
        telemetry.set_capture_target(CaptureMode::Window, Some(5));
        assert!(kept(on_safari.0, on_safari.1));
        assert!(!kept(on_slack.0, on_slack.1), "Slack floats over it");
        assert!(!kept(on_prompt.0, on_prompt.1));
        assert!(
            kept(under_overlay.0, under_overlay.1),
            "the overlay lets it through"
        );
        assert!(!kept(on_mirror.0, on_mirror.1), "another window");
        // The recorded window is gone (minimized, another Space): its old rectangle is not it.
        telemetry.set_capture_target(CaptureMode::Window, Some(99));
        assert!(!kept(on_safari.0, on_safari.1));
        // A Window capture that never learned its window keeps nothing.
        telemetry.set_capture_target(CaptureMode::Window, None);
        assert!(!kept(on_safari.0, on_safari.1));

        // The iPhone mirror is recorded: it is known by its process.
        telemetry.set_capture_target(CaptureMode::Device, None);
        assert!(!kept(on_mirror.0, on_mirror.1), "no mirror process yet");
        telemetry.set_capture_source_pid(mirror as u32);
        assert!(kept(on_mirror.0, on_mirror.1));
        assert!(!kept(on_safari.0, on_safari.1), "beside the mirror");

        assert_eq!(pins(&telemetry), vec![1, 2, 3, 4, 5]);
    }

    /// Review r3-s4-1: what a press lands on was kept for 250 ms and 2 pt, so a ⇧-click got
    /// the answer of another press. One a point away, on Slack floating over the recorded
    /// Safari, was kept from Slack. So was one on a prompt that had just opened there, and one
    /// on a menu that a plain click (asked about by the mouse monitor) had opened under the
    /// pointer. The key tap reads the windows for each press it judges.
    #[test]
    fn each_draw_press_is_judged_by_the_windows_as_they_are_then() {
        let telemetry = session(1000.0, 1000.0);
        let draw = telemetry.draw_mask();
        let window = |id, pid, owner: &str, bounds, layer| crate::windows::RawWindow {
            id,
            pid,
            owner: owner.into(),
            title: String::new(),
            bounds,
            layer,
            alpha: 1.0,
        };
        let own = std::process::id() as i32;
        let safari = window(5, own + 1, "Safari", [0.0, 0.0, 1000.0, 1000.0], 0);
        let slack = window(4, own + 2, "Slack", [600.0, 0.0, 400.0, 300.0], 0);
        let prompt = window(
            2,
            own + 3,
            "SecurityAgent",
            [400.0, 400.0, 200.0, 100.0],
            1000,
        );
        let menu = window(7, own + 1, "Safari", [100.0, 700.0, 200.0, 200.0], 101);
        telemetry.set_capture_target(CaptureMode::Window, Some(5));
        let reads = std::cell::Cell::new(0);
        let kept = |windows: &[crate::windows::RawWindow], x: f64, y: f64| {
            // The window server's answer: the frontmost window at the point.
            let pressed = || {
                windows
                    .iter()
                    .find(|w| {
                        let [wx, wy, ww, wh] = w.bounds;
                        x >= wx && x <= wx + ww && y >= wy && y <= wy + wh
                    })
                    .map(|w| w.id)
            };
            let read = || {
                reads.set(reads.get() + 1);
                windows.to_vec()
            };
            let kept = telemetry.block_press(x, y, draw, || {
                telemetry.press_misses_target_in(x, y, read, |_| false, pressed)
            });
            assert_eq!(telemetry.release_press(x, y), kept);
            kept
        };

        // Slack's left edge is at x = 600.
        let over_safari = [slack.clone(), safari.clone()];
        assert!(kept(&over_safari, 599.0, 200.0));
        assert!(!kept(&over_safari, 601.0, 200.0), "on Slack, 2 pt on");
        // A prompt opens where the last press went.
        assert!(kept(std::slice::from_ref(&safari), 500.0, 450.0));
        assert!(
            !kept(&[prompt, safari.clone()], 500.0, 450.0),
            "answering macOS"
        );
        // A plain click reaches the mouse monitor and opens a menu under the pointer.
        assert!(!telemetry.on_scrawl_window_in(150.0, 750.0, || vec![safari.clone()], |_| false));
        assert!(!kept(&[menu, safari.clone()], 150.0, 750.0), "on the menu");

        assert_eq!(reads.get(), 5, "one read of the windows for each press");
        assert_eq!(pins(&telemetry), vec![1, 2]);
    }

    /// Holding ⇧ and clicking three places drops three pins, and the lines the poller draws on
    /// the way from one pin to the next stay, each before the pin it ends at. The next hold
    /// carries on the numbering.
    #[test]
    fn shift_clicks_in_one_hold_each_drop_a_pin() {
        let telemetry = session(1000.0, 1000.0);
        let hold = |from: f64, to: f64| {
            let mut x = from;
            while x <= to {
                track_shift_ink(&telemetry, (x, 500.0), true, true);
                x += 10.0;
            }
        };
        let click = |x: f64| {
            // Later than every point the poller has drawn so far.
            let t = telemetry.elapsed_ms() + 1_000;
            telemetry.shift_down(t, x, 500.0);
            assert!(telemetry.shift_up(t + 60, x, 500.0));
        };
        hold(100.0, 100.0);
        click(100.0);
        hold(100.0, 300.0);
        click(300.0);
        hold(300.0, 500.0);
        click(500.0);
        // Still holding ⇧: a line after the last pin.
        hold(500.0, 700.0);
        track_shift_ink(&telemetry, (700.0, 500.0), false, true);

        // The next hold: ⇧ first, a line, then a click.
        hold(100.0, 300.0);
        click(300.0);
        track_shift_ink(&telemetry, (300.0, 500.0), false, true);

        let (events, strokes) = telemetry.finish_session();
        assert!(events.is_empty(), "{events:?}");
        let kinds: Vec<_> = strokes
            .iter()
            .map(|s| (s.pin, s.normalized_points()[0][0]))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (Some(1), 0.1),
                (None, 0.1),
                (Some(2), 0.3),
                (None, 0.3),
                (Some(3), 0.5),
                (None, 0.5),
                (None, 0.1),
                (Some(4), 0.3)
            ]
        );
    }

    #[test]
    fn shift_clicks_drop_pins_from_polled_buttons_too() {
        let telemetry = session(1000.0, 1000.0);
        let mut prev = [false; 2];
        // ⇧-clicks at a point, the second on Scrawl's own window.
        for (at, on_scrawl) in [((250.0, 750.0), false), ((400.0, 400.0), true)] {
            for buttons in [[true, false], [false, false]] {
                poll_clicks(&telemetry, &mut prev, buttons, at, true, |_, _| on_scrawl);
            }
        }
        let (events, strokes) = telemetry.finish_session();
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(strokes.len(), 1);
        assert_eq!(strokes[0].pin, Some(1));
        assert_eq!(strokes[0].normalized_points(), vec![[0.25, 0.75]]);
    }

    #[test]
    fn candidate_buffer_stays_bounded_and_keeps_the_whole_timeline() {
        // A 1 MB budget of 4 KB frames: a 256-frame cap.
        let mut buffer = CandidateBuffer::new(1 << 20);
        let frame = |t| RgbaFrame::new(t, 32, 32, vec![0; 32 * 32 * 4]);
        let mut t = 0;
        let mut peak = 0;
        // Three hours at the sampler's pace, with a click frame every 10 s.
        while t < 3 * 3_600_000 {
            buffer.push(frame(t), t % 10_000 == 0);
            peak = peak.max(buffer.len());
            t += buffer.interval().as_millis() as u64;
        }
        assert!(peak <= 256, "peak {peak}");
        assert!(buffer.interval() >= FRAME_SAMPLE_INTERVAL * 64);
        let frames = buffer.into_frames();
        assert_eq!(frames[0].t_ms, 0, "the start is kept");
        assert!(frames.windows(2).all(|w| w[0].t_ms < w[1].t_ms));
        // Spread over the whole capture, not bunched at the end.
        let first_half = frames.iter().filter(|f| f.t_ms < 3 * 1_800_000).count();
        assert!(
            first_half * 3 > frames.len(),
            "{first_half} of {}",
            frames.len()
        );
    }

    #[test]
    fn candidate_buffer_keeps_urgent_frames_when_thinning() {
        let mut buffer = CandidateBuffer::new(40 * 4096);
        let frame = |t| RgbaFrame::new(t, 32, 32, vec![0; 32 * 32 * 4]);
        for t in 0..60 {
            buffer.push(frame(t * 220), t == 33);
        }
        assert!(buffer.len() < 60);
        assert_eq!(buffer.interval(), FRAME_SAMPLE_INTERVAL * 2);
        assert!(buffer.into_frames().iter().any(|f| f.t_ms == 33 * 220));
    }

    #[test]
    fn frames_that_may_show_a_system_prompt_are_dropped_but_never_the_first() {
        let mut buffer = CandidateBuffer::new(1 << 20);
        for t in [0, 220, 440, 660, 880] {
            buffer.push(RgbaFrame::new(t, 4, 4, vec![0; 64]), false);
        }
        buffer.drop_since(440);
        let times: Vec<u64> = buffer.into_frames().iter().map(|f| f.t_ms).collect();
        assert_eq!(times, [0, 220, 440]);
        let mut buffer = CandidateBuffer::new(1 << 20);
        buffer.push(RgbaFrame::new(500, 4, 4, vec![0; 64]), false);
        buffer.drop_since(0);
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn candidates_are_small_and_recordings_keep_full_size() {
        let target = |mode, width, height| TargetInfo {
            mode,
            app: None,
            bundle_id: None,
            window_title: None,
            window_id: None,
            display_id: 1,
            frame: TargetFrame {
                x: 0.0,
                y: 0.0,
                width,
                height,
                scale: 2.0,
            },
        };
        let display = target(CaptureMode::Display, 1728.0, 1117.0);
        assert_eq!(capture_size(&display, CANDIDATE_EDGE), (480, 310));
        let region = target(CaptureMode::Region, 480.0, 320.0);
        assert_eq!(capture_size(&region, CANDIDATE_EDGE), (480, 320));
        if cfg!(target_os = "windows") {
            // Frames are in pixels: never upscaled past the screen's own.
            assert_eq!(capture_size(&display, 2560.0), (1728, 1116));
            assert_eq!(capture_size(&region, 1280.0), (480, 320));
            let wide = target(CaptureMode::Display, 3440.0, 1440.0);
            assert_eq!(capture_size(&wide, 2560.0), (2560, 1072));
        } else {
            assert_eq!(capture_size(&display, 2560.0), (2560, 1654));
            assert_eq!(capture_size(&region, 1280.0), (960, 640));
        }

        // A 1x external display is recorded at its own pixels, never upscaled.
        let external = |mode, width, height| TargetInfo {
            frame: crate::screens::with_scale([-1920.0, 37.0, width, height], 1.0),
            ..target(mode, width, height)
        };
        let screen = external(CaptureMode::Display, 1920.0, 1080.0);
        assert_eq!(capture_size(&screen, 2560.0), (1920, 1080));
        assert_eq!(capture_size(&screen, CANDIDATE_EDGE), (480, 270));
        let region = external(CaptureMode::Region, 480.0, 320.0);
        assert_eq!(capture_size(&region, 1280.0), (480, 320));
        // A 2x one above the laptop keeps the 2560 px long edge.
        let above = TargetInfo {
            frame: crate::screens::with_scale([0.0, -1440.0, 2560.0, 1440.0], 2.0),
            ..target(CaptureMode::Display, 2560.0, 1440.0)
        };
        assert_eq!(capture_size(&above, 2560.0), (2560, 1440));
    }

    #[test]
    fn cached_window_hit_preserves_front_to_back_order_and_screen_coordinates() {
        let window = |id, x, y| TargetInfo {
            mode: CaptureMode::Window,
            app: Some(format!("App {id}")),
            bundle_id: None,
            window_title: Some(format!("Window {id}")),
            window_id: Some(id),
            display_id: 1,
            frame: TargetFrame {
                x,
                y,
                width: 200.0,
                height: 100.0,
                scale: 2.0,
            },
        };
        let windows = [window(1, -100.0, 20.0), window(2, 0.0, 0.0)];
        assert_eq!(
            selectable_window_at(&windows, 50.0, 50.0)
                .unwrap()
                .window_id,
            Some(1)
        );
        assert_eq!(
            selectable_window_at(&windows, 150.0, 50.0)
                .unwrap()
                .window_id,
            Some(2)
        );
        assert_eq!(
            selectable_window_at(&windows, -100.0, 20.0)
                .unwrap()
                .window_id,
            Some(1)
        );
        assert!(selectable_window_at(&windows, -101.0, 50.0).is_none());
        assert!(selectable_window_at(&[], 0.0, 0.0).is_none());
    }
}
