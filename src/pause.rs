//! Pausing a recording. One press of the cancel key (Esc unless Settings › Shortcuts set
//! another) pauses a recording or resumes it; two presses within `DOUBLE_PRESS_MS` throw it
//! away.
//!
//! Nothing stops while a recording is paused: the screen, the microphone and the window log go
//! on recording on the one capture clock, and `Session` only notes when each pause began and
//! ended. At finish `Cuts` takes those spans out of every timeline (video, audio, candidate
//! frames, clicks, ink, the window log), so they all lose the same milliseconds and stay in
//! sync. `docs/capture.md` says why.

use crate::model::{InteractionEvent, RgbaFrame, Stroke};

pub use crate::shortcut::DOUBLE_PRESS_MS;

/// Two reports this close are one press seen twice (the key tap and the capsule's own key
/// handler, or Windows' hook and poller), never a double press: fingers aren't that fast.
pub const SAME_PRESS_MS: u64 = 40;

/// What a press of the cancel key is, given the press before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// On its own: pauses or resumes.
    Single,
    /// The second of two within `DOUBLE_PRESS_MS`: cancels.
    Double,
    /// The press before, reported again: nothing.
    Echo,
}

/// The timing rule, on any clock that only moves forward: `previous_ms` is the last press that
/// counted (not an echo), `now_ms` this one.
pub fn classify(previous_ms: Option<u64>, now_ms: u64) -> Press {
    let Some(previous) = previous_ms else {
        return Press::Single;
    };
    let gap = now_ms.saturating_sub(previous);
    if gap < SAME_PRESS_MS {
        Press::Echo
    } else if gap < DOUBLE_PRESS_MS {
        Press::Double
    } else {
        Press::Single
    }
}

/// Where a recording is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Recording,
    /// Paused since `since_ms` on the capture clock.
    Paused {
        since_ms: u64,
    },
    /// Finish was asked for: the bundle is being built.
    Finishing,
    /// Thrown away.
    Cancelled,
}

/// What a press or a button did, for the coordinator to act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    None,
    Paused,
    Resumed,
    Cancelled,
}

/// One recording's state machine. Every time is milliseconds on the capture clock.
#[derive(Debug)]
pub struct Session {
    state: State,
    /// The pauses that have ended.
    spans: Vec<Span>,
    /// The last press of the cancel key that counted.
    last_press_ms: Option<u64>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            state: State::Recording,
            spans: Vec::new(),
            last_press_ms: None,
        }
    }
}

impl Session {
    pub fn state(&self) -> State {
        self.state
    }

    pub fn is_paused(&self) -> bool {
        matches!(self.state, State::Paused { .. })
    }

    /// The cancel key went down at `now_ms`: a single press pauses or resumes, the second of a
    /// double press cancels (the first one's pause or resume has happened by then, which a
    /// cancelled recording doesn't mind).
    pub fn press(&mut self, now_ms: u64) -> Change {
        if !matches!(self.state, State::Recording | State::Paused { .. }) {
            return Change::None;
        }
        match classify(self.last_press_ms, now_ms) {
            Press::Echo => Change::None,
            Press::Double => {
                self.last_press_ms = Some(now_ms);
                self.cancel()
            }
            Press::Single => {
                self.last_press_ms = Some(now_ms);
                self.toggle(now_ms)
            }
        }
    }

    /// The capsule's Pause or Resume button, or a single press.
    pub fn toggle(&mut self, now_ms: u64) -> Change {
        match self.state {
            State::Recording => {
                self.state = State::Paused { since_ms: now_ms };
                Change::Paused
            }
            State::Paused { since_ms } => {
                self.close(since_ms, now_ms);
                self.state = State::Recording;
                Change::Resumed
            }
            State::Finishing | State::Cancelled => Change::None,
        }
    }

    /// The capsule's Cancel button, or a double press.
    pub fn cancel(&mut self) -> Change {
        match self.state {
            State::Recording | State::Paused { .. } => {
                self.state = State::Cancelled;
                Change::Cancelled
            }
            State::Finishing | State::Cancelled => Change::None,
        }
    }

    /// Finish, recording or paused: the paused spans to take out. `None` once the recording is
    /// over.
    pub fn finish(&mut self, now_ms: u64) -> Option<Cuts> {
        match self.state {
            State::Recording => {}
            State::Paused { since_ms } => self.close(since_ms, now_ms),
            State::Finishing | State::Cancelled => return None,
        }
        self.state = State::Finishing;
        Some(Cuts::new(std::mem::take(&mut self.spans)))
    }

    /// How much has been recorded at `now_ms`, which stands still while paused: what the
    /// capsule's timer shows.
    pub fn recorded_ms(&self, now_ms: u64) -> u64 {
        let paused: u64 = self.spans.iter().map(Span::len).sum();
        let until = match self.state {
            State::Paused { since_ms } => since_ms.min(now_ms),
            _ => now_ms,
        };
        until.saturating_sub(paused)
    }

    fn close(&mut self, since_ms: u64, now_ms: u64) {
        self.spans.push(Span {
            start_ms: since_ms,
            end_ms: now_ms.max(since_ms),
        });
    }
}

/// A pause: `start_ms..end_ms` on the capture clock, the end not included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start_ms: u64,
    pub end_ms: u64,
}

impl Span {
    fn len(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }

    fn holds(&self, t: u64) -> bool {
        (self.start_ms..self.end_ms).contains(&t)
    }
}

/// A recording's pauses, in order and apart, and the shift they give every time after them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cuts {
    spans: Vec<Span>,
}

impl Cuts {
    pub fn new(mut spans: Vec<Span>) -> Self {
        spans.retain(|span| span.end_ms > span.start_ms);
        spans.sort_by_key(|span| span.start_ms);
        let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
        for span in spans {
            match merged.last_mut() {
                Some(last) if span.start_ms <= last.end_ms => {
                    last.end_ms = last.end_ms.max(span.end_ms);
                }
                _ => merged.push(span),
            }
        }
        Self { spans: merged }
    }

    /// Nothing was paused: every timeline stays as it is.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// The paused time in all.
    pub fn total_ms(&self) -> u64 {
        self.spans.iter().map(Span::len).sum()
    }

    /// Where the time `t` lands once the pauses are gone; a time inside a pause lands where
    /// that pause began. Use it for ends and lengths (a duration, a drag's `t1`).
    pub fn shift(&self, t: u64) -> u64 {
        let mut cut = 0;
        for span in &self.spans {
            if t >= span.end_ms {
                cut += span.len();
            } else {
                cut += t.saturating_sub(span.start_ms);
                break;
            }
        }
        t - cut
    }

    /// `shift` for something that happened at `t`: `None` if that was during a pause, so it is
    /// left out.
    pub fn map(&self, t: u64) -> Option<u64> {
        (!self.spans.iter().any(|span| span.holds(t))).then(|| self.shift(t))
    }

    /// What is left of `from_ms..to_ms`, as ranges on the same clock: what to keep of a movie
    /// or of the audio.
    pub fn kept(&self, from_ms: u64, to_ms: u64) -> Vec<(u64, u64)> {
        let mut kept = Vec::with_capacity(self.spans.len() + 1);
        let mut at = from_ms;
        for span in &self.spans {
            if span.end_ms <= at {
                continue;
            }
            if span.start_ms >= to_ms {
                break;
            }
            if span.start_ms > at {
                kept.push((at, span.start_ms));
            }
            at = span.end_ms;
        }
        if at < to_ms {
            kept.push((at, to_ms));
        }
        kept
    }

    /// The same pauses on a clock that started `offset_ms` after the capture's (a movie's own
    /// clock: `videoOffsetMs`, the iPhone movie's `offset_ms`).
    pub fn on_clock_from(&self, offset_ms: u64) -> Self {
        Self::new(
            self.spans
                .iter()
                .map(|span| Span {
                    start_ms: span.start_ms.saturating_sub(offset_ms),
                    end_ms: span.end_ms.saturating_sub(offset_ms),
                })
                .collect(),
        )
    }

    /// Clicks, drags, scrolls and keys: one that began during a pause is dropped, the rest
    /// move earlier.
    pub fn cut_events(&self, events: &mut Vec<InteractionEvent>) {
        if self.is_empty() {
            return;
        }
        events.retain_mut(|event| match event {
            InteractionEvent::Drag { t, t1, .. } => {
                let Some(start) = self.map(*t) else {
                    return false;
                };
                *t1 = self.shift(*t1).max(start);
                *t = start;
                true
            }
            InteractionEvent::Click { t, .. }
            | InteractionEvent::Scroll { t, .. }
            | InteractionEvent::Point { t, .. }
            | InteractionEvent::Key { t, .. }
            | InteractionEvent::AppChange { t, .. }
            | InteractionEvent::AppEvent { t, .. } => self.map(*t).map(|at| *t = at).is_some(),
        });
    }

    /// Ink, pins and laser trails: a stroke that began during a pause is dropped, the rest move
    /// earlier with their points' own timing untouched.
    pub fn cut_strokes(&self, strokes: &mut Vec<Stroke>) {
        if self.is_empty() {
            return;
        }
        strokes.retain_mut(|stroke| {
            let Some(start) = self.map(stroke.t0) else {
                return false;
            };
            stroke.t1 = self.shift(stroke.t1).max(start);
            stroke.t0 = start;
            true
        });
    }

    /// Candidate frames: those taken during a pause are dropped (never the only one).
    pub fn cut_frames(&self, frames: &mut Vec<RgbaFrame>) {
        if self.is_empty() {
            return;
        }
        let only = (!frames.iter().any(|frame| self.map(frame.t_ms).is_some()))
            .then(|| frames.first().map(|frame| frame.t_ms))
            .flatten();
        frames.retain_mut(|frame| {
            let at = self.map(frame.t_ms);
            let kept = at.is_some() || only == Some(frame.t_ms);
            frame.t_ms = at.unwrap_or_else(|| self.shift(frame.t_ms));
            kept
        });
        frames.dedup_by_key(|frame| frame.t_ms);
    }

    /// Moments with a time (cursor samples): those during a pause are dropped.
    pub fn cut_moments<T>(&self, items: &mut Vec<T>, time: impl Fn(&mut T) -> &mut u64) {
        if self.is_empty() {
            return;
        }
        items.retain_mut(|item| {
            let t = time(item);
            self.map(*t).map(|at| *t = at).is_some()
        });
    }

    /// States that hold until the next one (window snapshots): the last one of a pause is what
    /// the recording resumes on, so it moves to where the pause began; the ones before it in
    /// that pause are dropped.
    pub fn cut_states<T>(&self, items: &mut Vec<T>, time: impl Fn(&mut T) -> &mut u64) {
        if self.is_empty() {
            return;
        }
        let pause_of = |t: u64| self.spans.iter().position(|span| span.holds(t));
        let pauses: Vec<Option<usize>> = items.iter_mut().map(|item| pause_of(*time(item))).collect();
        let mut index = 0;
        items.retain_mut(|item| {
            let pause = pauses[index];
            let last_of_pause = pauses.get(index + 1).copied() != Some(pause);
            index += 1;
            let t = time(item);
            *t = self.shift(*t);
            pause.is_none() || last_of_pause
        });
    }

    /// Audio or any other evenly sampled track that began with the capture: the samples of the
    /// pauses are taken out.
    pub fn cut_samples<T: Copy>(&self, samples: &[T], per_second: u64) -> Vec<T> {
        if self.is_empty() {
            return samples.to_vec();
        }
        let index = |ms: u64| ((ms * per_second / 1_000) as usize).min(samples.len());
        let length_ms = (samples.len() as u64 * 1_000).div_ceil(per_second.max(1));
        let mut kept = Vec::with_capacity(samples.len());
        for (from, to) in self.kept(0, length_ms) {
            kept.extend_from_slice(&samples[index(from)..index(to)]);
        }
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click(t: u64) -> InteractionEvent {
        InteractionEvent::Click {
            t,
            x: 0.5,
            y: 0.5,
            button: None,
            label: None,
            role: None,
        }
    }

    fn times(events: &[InteractionEvent]) -> Vec<u64> {
        events.iter().map(InteractionEvent::timestamp_ms).collect()
    }

    fn cuts(spans: &[(u64, u64)]) -> Cuts {
        Cuts::new(
            spans
                .iter()
                .map(|&(start_ms, end_ms)| Span { start_ms, end_ms })
                .collect(),
        )
    }

    // ── The press-timing rule ───────────────────────────────────────────────────────────────

    #[test]
    fn a_press_alone_is_single_and_a_second_within_400_ms_is_double() {
        assert_eq!(classify(None, 0), Press::Single);
        assert_eq!(classify(None, 90_000), Press::Single);
        assert_eq!(classify(Some(1_000), 1_150), Press::Double);
        assert_eq!(classify(Some(1_000), 1_399), Press::Double);
        // 400 ms apart are two single presses.
        assert_eq!(classify(Some(1_000), 1_400), Press::Single);
        assert_eq!(classify(Some(1_000), 9_000), Press::Single);
    }

    #[test]
    fn one_press_reported_by_two_sources_is_not_a_double_press() {
        assert_eq!(classify(Some(1_000), 1_000), Press::Echo);
        assert_eq!(classify(Some(1_000), 1_039), Press::Echo);
        assert_eq!(classify(Some(1_000), 1_040), Press::Double);
        // A report that was stamped a little earlier than the one before it.
        assert_eq!(classify(Some(1_000), 998), Press::Echo);
    }

    // ── The state machine ───────────────────────────────────────────────────────────────────

    #[test]
    fn a_single_press_pauses_and_the_next_single_press_resumes() {
        let mut s = Session::default();
        assert_eq!(s.state(), State::Recording);
        assert_eq!(s.press(2_000), Change::Paused);
        assert_eq!(s.state(), State::Paused { since_ms: 2_000 });
        assert!(s.is_paused());
        assert_eq!(s.press(5_000), Change::Resumed);
        assert_eq!(s.state(), State::Recording);
        assert_eq!(s.finish(8_000), Some(cuts(&[(2_000, 5_000)])));
        assert_eq!(s.state(), State::Finishing);
    }

    #[test]
    fn a_double_press_cancels_while_recording_and_while_paused() {
        let mut recording = Session::default();
        assert_eq!(recording.press(2_000), Change::Paused);
        assert_eq!(recording.press(2_250), Change::Cancelled);
        assert_eq!(recording.state(), State::Cancelled);

        let mut paused = Session::default();
        paused.press(2_000);
        // Long after: the first press resumes, the second within 400 ms cancels.
        assert_eq!(paused.press(60_000), Change::Resumed);
        assert_eq!(paused.press(60_399), Change::Cancelled);
        assert_eq!(paused.state(), State::Cancelled);

        // A cancelled recording takes no more presses and has nothing to finish.
        assert_eq!(paused.press(61_000), Change::None);
        assert_eq!(paused.toggle(61_000), Change::None);
        assert_eq!(paused.finish(62_000), None);
        assert_eq!(paused.state(), State::Cancelled);
    }

    #[test]
    fn the_same_press_seen_twice_pauses_once() {
        let mut s = Session::default();
        assert_eq!(s.press(2_000), Change::Paused);
        assert_eq!(s.press(2_006), Change::None);
        assert!(s.is_paused());
        // The echo doesn't restart the double-press window: 2 000 and 2 390 are a double press.
        assert_eq!(s.press(2_390), Change::Cancelled);
    }

    #[test]
    fn finish_while_paused_finishes_and_ends_the_pause_there() {
        let mut s = Session::default();
        s.press(1_000);
        s.press(2_000);
        s.press(7_000);
        assert_eq!(s.finish(9_500), Some(cuts(&[(1_000, 2_000), (7_000, 9_500)])));
        assert_eq!(s.state(), State::Finishing);
        // Nothing moves a finishing recording.
        assert_eq!(s.press(9_600), Change::None);
        assert_eq!(s.cancel(), Change::None);
        assert_eq!(s.finish(9_700), None);
    }

    #[test]
    fn the_buttons_pause_resume_and_cancel_without_the_key() {
        let mut s = Session::default();
        assert_eq!(s.toggle(1_000), Change::Paused);
        assert_eq!(s.toggle(1_100), Change::Resumed);
        // Two clicks on the button are not a double press of the key.
        assert_eq!(s.state(), State::Recording);
        assert_eq!(s.toggle(4_000), Change::Paused);
        assert_eq!(s.cancel(), Change::Cancelled);
    }

    #[test]
    fn the_timer_stands_still_while_paused() {
        let mut s = Session::default();
        assert_eq!(s.recorded_ms(1_500), 1_500);
        s.press(2_000);
        assert_eq!(s.recorded_ms(2_000), 2_000);
        assert_eq!(s.recorded_ms(4_900), 2_000);
        s.press(5_000);
        assert_eq!(s.recorded_ms(5_000), 2_000);
        assert_eq!(s.recorded_ms(6_000), 3_000);
        s.press(6_500);
        assert_eq!(s.recorded_ms(60_000), 3_500);
    }

    // ── The timeline shift ──────────────────────────────────────────────────────────────────

    /// The brief's case: events at 1 s, a pause from 2 s to 5 s, an event at 6 s.
    #[test]
    fn an_event_after_a_pause_moves_earlier_by_the_pause() {
        let cuts = cuts(&[(2_000, 5_000)]);
        let mut events = vec![click(1_000), click(6_000)];
        cuts.cut_events(&mut events);
        assert_eq!(times(&events), [1_000, 3_000]);
        assert_eq!(cuts.total_ms(), 3_000);
        assert_eq!(cuts.shift(8_000), 5_000, "the duration");
    }

    #[test]
    fn times_shift_by_every_pause_before_them() {
        let cuts = cuts(&[(2_000, 5_000), (10_000, 11_000)]);
        assert_eq!(cuts.map(0), Some(0));
        assert_eq!(cuts.map(1_999), Some(1_999));
        assert_eq!(cuts.map(2_000), None, "a pause includes its start");
        assert_eq!(cuts.map(4_999), None);
        assert_eq!(cuts.map(5_000), Some(2_000), "and not its end");
        assert_eq!(cuts.map(9_999), Some(6_999));
        assert_eq!(cuts.map(10_500), None);
        assert_eq!(cuts.map(12_000), Some(8_000));
        // Ends land where their pause began.
        assert_eq!(cuts.shift(3_000), 2_000);
        assert_eq!(cuts.shift(10_500), 7_000);
        assert_eq!(cuts.kept(0, 12_000), [(0, 2_000), (5_000, 10_000), (11_000, 12_000)]);
        assert_eq!(cuts.kept(3_000, 10_500), [(5_000, 10_000)]);
        assert_eq!(Cuts::default().kept(0, 900), [(0, 900)]);
    }

    #[test]
    fn pauses_are_kept_in_order_and_apart() {
        let cuts = cuts(&[(7_000, 8_000), (1_000, 3_000), (2_500, 4_000), (5_000, 5_000)]);
        assert_eq!(
            cuts.spans(),
            [
                Span {
                    start_ms: 1_000,
                    end_ms: 4_000
                },
                Span {
                    start_ms: 7_000,
                    end_ms: 8_000
                }
            ]
        );
        assert!(Cuts::new(vec![Span {
            start_ms: 5,
            end_ms: 5
        }])
        .is_empty());
    }

    #[test]
    fn what_happened_during_a_pause_is_left_out() {
        let cuts = cuts(&[(2_000, 5_000)]);
        let mut events = vec![
            click(1_000),
            click(3_000),
            InteractionEvent::Drag {
                t: 1_500,
                t1: 2_400,
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
                label: None,
                button: None,
                path: Vec::new(),
            },
            InteractionEvent::Key {
                t: 4_999,
                shortcut: "⌘W".into(),
            },
            InteractionEvent::Scroll {
                t: 5_000,
                x: 0.0,
                y: 0.0,
                dx: 0.0,
                dy: 1.0,
                label: None,
            },
        ];
        cuts.cut_events(&mut events);
        assert_eq!(times(&events), [1_000, 1_500, 2_000]);
        // A drag still going when the pause began ends there.
        assert!(matches!(events[1], InteractionEvent::Drag { t1: 2_000, .. }));
    }

    #[test]
    fn ink_moves_with_its_points_and_ink_from_a_pause_is_dropped() {
        let stroke = |t0, t1| Stroke {
            t0,
            t1,
            fw: 100,
            fh: 100,
            on: None,
            pin: None,
            laser: false,
            pts: vec![0, 0, 0, 10, 10, (t1 - t0) as i32],
        };
        let mut strokes = vec![stroke(500, 900), stroke(2_500, 2_700), stroke(6_000, 6_400)];
        cuts(&[(2_000, 5_000)]).cut_strokes(&mut strokes);
        assert_eq!(strokes.len(), 2);
        assert_eq!((strokes[0].t0, strokes[0].t1), (500, 900));
        assert_eq!((strokes[1].t0, strokes[1].t1), (3_000, 3_400));
        assert_eq!(strokes[1].pts, [0, 0, 0, 10, 10, 400]);
    }

    #[test]
    fn frames_from_a_pause_are_dropped_but_never_the_only_one() {
        let frame = |t_ms| RgbaFrame::new(t_ms, 1, 1, vec![0, 0, 0, 255]);
        let cuts = cuts(&[(2_000, 5_000)]);
        let mut frames: Vec<RgbaFrame> = [0, 1_000, 2_200, 4_800, 5_000, 6_000].map(frame).into();
        cuts.cut_frames(&mut frames);
        let at: Vec<u64> = frames.iter().map(|f| f.t_ms).collect();
        assert_eq!(at, [0, 1_000, 2_000, 3_000]);

        let mut paused_throughout: Vec<RgbaFrame> = [2_200, 3_000].map(frame).into();
        cuts.cut_frames(&mut paused_throughout);
        let at: Vec<u64> = paused_throughout.iter().map(|f| f.t_ms).collect();
        assert_eq!(at, [2_000]);
    }

    #[test]
    fn the_window_log_resumes_on_the_last_state_of_a_pause() {
        let cuts = cuts(&[(2_000, 5_000)]);
        // (time, which windows were up)
        let mut states = vec![(0_u64, 'a'), (1_000, 'b'), (2_500, 'c'), (4_000, 'd'), (7_000, 'e')];
        cuts.cut_states(&mut states, |state| &mut state.0);
        assert_eq!(states, [(0, 'a'), (1_000, 'b'), (2_000, 'd'), (4_000, 'e')]);

        let mut cursor = vec![(900_u64, 1), (2_500, 2), (4_000, 3), (5_100, 4)];
        cuts.cut_moments(&mut cursor, |sample| &mut sample.0);
        assert_eq!(cursor, [(900, 1), (2_100, 4)]);
    }

    #[test]
    fn audio_loses_exactly_the_paused_samples() {
        // 16 samples a millisecond; each sample holds the millisecond it was heard in.
        let samples: Vec<u32> = (0..8_000 * 16).map(|i| i / 16).collect();
        let cut = cuts(&[(2_000, 5_000)]).cut_samples(&samples, 16_000);
        assert_eq!(cut.len(), 5_000 * 16);
        assert_eq!(cut[1_999 * 16 + 15], 1_999);
        assert_eq!(cut[2_000 * 16], 5_000, "the sound at 5 s follows the sound at 2 s");
        // The event at 6 s is heard at 3 s, where `cut_events` puts it.
        assert_eq!(cut[3_000 * 16], 6_000);
        // A pause that outlasts the audio takes its tail and nothing more.
        let tail = cuts(&[(7_000, 60_000)]).cut_samples(&samples, 16_000);
        assert_eq!(tail.len(), 7_000 * 16);
        assert_eq!(Cuts::default().cut_samples(&samples, 16_000), samples);
    }

    #[test]
    fn a_movie_that_started_late_is_cut_on_its_own_clock() {
        // The movie's first frame is 300 ms into the capture.
        let cuts = cuts(&[(2_000, 5_000)]);
        let movie = cuts.on_clock_from(300);
        assert_eq!(movie.kept(0, 7_700), [(0, 1_700), (4_700, 7_700)]);
        // Its start is still 300 ms into the cut capture.
        assert_eq!(cuts.shift(300), 300);
        // A pause that began before the movie did: the movie starts where the pause began.
        let early = super::Cuts::new(vec![Span {
            start_ms: 100,
            end_ms: 1_000,
        }]);
        assert_eq!(early.on_clock_from(300).kept(0, 5_000), [(700, 5_000)]);
        assert_eq!(early.shift(300), 100);
    }
}
