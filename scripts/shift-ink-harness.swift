// Proves that ⇧ ink and ⇧-click pins work in Annotator Dev.app, end to end, with real system
// input events.
//
//   swiftc -O scripts/shift-ink-harness.swift -o /tmp/shift-ink-harness
//   /tmp/shift-ink-harness [--scenario <name>] [--screenshot <png>]
//
// It opens a small window of its own (so the test clicks land on nothing of yours), starts a
// recording with Annotator Dev's ⌃⌥S, posts ⇧, moves, drags and clicks at the HID event tap,
// stops with ⌃⌥S, and counts the strokes, pins and marks in the new bundle's `scrawl.json`.
//
// Exit codes: 0 the scenario left what it should; 1 it did not; 2 it could not run (no
// Accessibility for this terminal, Dev not running, or the live Annotator is recording, which
// the test's ⇧ would draw into); 3 real input kept interrupting the posted ⇧ (see `disturbed`).
//
// Scenarios (one recording each):
//   all          move, drag and click (the default)
//   move         hold ⇧, move the pointer: 1 stroke
//   drag         hold ⇧, press the left button, drag, let go: 1 stroke
//   click        hold ⇧, click, let go of ⇧: pin 1
//   pins         three ⇧-clicks, ⇧ let go between them: pins 1, 2 and 3
//   held-pins    hold ⇧ once and click three places: pins 1, 2 and 3, no stroke
//   button-first press the left button, then hold ⇧ and drag: 1 stroke
//   late-shift   press the left button, tap ⇧ without moving, let go: nothing (by design)
//
// --screenshot <png> saves the main display (screencapture) 0.4 s after the last ⇧-click, to
// show the live overlay's pins.
//
// The terminal that runs it needs Accessibility (to post events). Annotator Dev must be running.

import AppKit
import CoreGraphics
import Foundation

func say(_ message: String) {
  FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
}

func stop(_ code: Int32, _ message: String) -> Never {
  say(message)
  exit(code)
}

/// What a scenario must leave: ink strokes and pin numbers.
let wanted: [String: (ink: Int, pins: [Int])] = [
  "all": (2, [1]), "move": (1, []), "drag": (1, []), "click": (0, [1]),
  "pins": (0, [1, 2, 3]), "held-pins": (0, [1, 2, 3]), "button-first": (1, []),
  "late-shift": (0, []),
]
let usage = "usage: shift-ink-harness [--scenario \(wanted.keys.sorted().joined(separator: "|"))] [--screenshot <png>]"

var scenario = "all"
var screenshot: String?
var args = CommandLine.arguments.dropFirst().makeIterator()
while let arg = args.next() {
  switch arg {
  case "--scenario": scenario = args.next() ?? ""
  case "--screenshot": screenshot = args.next()
  default: stop(2, usage)
  }
}
guard let want = wanted[scenario] else { stop(2, usage) }

let home = FileManager.default.homeDirectoryForCurrentUser
let devLog = home.appendingPathComponent("Library/Logs/Scrawl Dev/scrawl.log")
let liveLog = home.appendingPathComponent("Library/Logs/Scrawl/scrawl.log")
let captures = home.appendingPathComponent("Library/Application Support/scrawl-dev/captures")

/// The log's lines from byte `offset` on.
func logLines(_ url: URL, from offset: UInt64 = 0) -> [String] {
  guard let handle = try? FileHandle(forReadingFrom: url) else { return [] }
  defer { try? handle.close() }
  try? handle.seek(toOffset: offset)
  let data = (try? handle.readToEnd()) ?? Data()
  return String(decoding: data, as: UTF8.self).split(separator: "\n").map(String.init)
}

func logSize(_ url: URL) -> UInt64 {
  ((try? FileManager.default.attributesOfItem(atPath: url.path))?[.size] as? UInt64) ?? 0
}

/// Whether the app that writes `url` is in the middle of a recording.
func recording(_ url: URL) -> Bool {
  let last = logLines(url).last {
    $0.contains("start_recording_with_target") || $0.contains("finish_recording invoked")
      || $0.contains("cancel_recording invoked") || $0.contains("run_app started")
  }
  return last?.contains("start_recording_with_target") ?? false
}

func waitFor(_ url: URL, from offset: UInt64, seconds: Double, _ needle: String) -> Bool {
  let deadline = Date().addingTimeInterval(seconds)
  while Date() < deadline {
    if logLines(url, from: offset).contains(where: { $0.contains(needle) }) { return true }
    Thread.sleep(forTimeInterval: 0.05)
  }
  return false
}

func bundles() -> Set<String> {
  Set((try? FileManager.default.contentsOfDirectory(atPath: captures.path)) ?? [])
}

guard AXIsProcessTrusted() else {
  stop(2, "this terminal has no Accessibility, so it cannot post input events")
}
let devRunning = NSWorkspace.shared.runningApplications.contains {
  $0.bundleIdentifier == "ai.codecaine.annotator.dev"
}
guard devRunning else { stop(2, "Annotator Dev.app is not running") }

let source = CGEventSource(stateID: .hidSystemState)
let shiftKey: CGKeyCode = 56
var held: CGEventFlags = []
/// How many times this run pressed ⇧.
var shiftPresses = 0

func post(_ event: CGEvent?) {
  guard let event else { stop(2, "could not make an input event") }
  event.flags = held
  event.post(tap: .cghidEventTap)
}

func holdShift() {
  let event = CGEvent(keyboardEventSource: source, virtualKey: shiftKey, keyDown: !held.isEmpty)
  event?.type = .flagsChanged
  post(event)
}

func shift(_ down: Bool) {
  held = down ? .maskShift : []
  if down { shiftPresses += 1 }
  holdShift()
  Thread.sleep(forTimeInterval: 0.12)
}

func mouse(_ type: CGEventType, _ at: CGPoint) {
  post(CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: at, mouseButton: .left))
}

/// Moves (or drags) along a line in 8 ms steps, like a hand does. A plain move warps the
/// pointer and posts ⇧ again, to keep the posted ⇧ in the modifier state.
func glide(_ type: CGEventType, from: CGPoint, to: CGPoint, seconds: Double = 0.5) {
  let steps = max(2, Int(seconds / 0.008))
  for step in 0...steps {
    let t = CGFloat(step) / CGFloat(steps)
    let at = CGPoint(x: from.x + (to.x - from.x) * t, y: from.y + (to.y - from.y) * t)
    if type == .mouseMoved {
      CGWarpMouseCursorPosition(at)
      if held.contains(.maskShift) { holdShift() }
    } else {
      mouse(type, at)
    }
    Thread.sleep(forTimeInterval: 0.008)
  }
}

func click(_ at: CGPoint) {
  mouse(.leftMouseDown, at)
  Thread.sleep(forTimeInterval: 0.06)
  mouse(.leftMouseUp, at)
  Thread.sleep(forTimeInterval: 0.06)
}

func capture() {
  guard let screenshot else { return }
  Thread.sleep(forTimeInterval: 0.4)
  let task = Process()
  task.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
  task.arguments = ["-x", "-m", screenshot]
  try? task.run()
  task.waitUntilExit()
}

/// ⌃⌥S, Annotator Dev's record shortcut. The modifiers go down and come up as keys of their
/// own: Annotator starts recording only once it sees them let go.
func recordShortcut() {
  let control: CGKeyCode = 59
  let option: CGKeyCode = 58
  let s: CGKeyCode = 1
  let steps: [(CGKeyCode, Bool, CGEventFlags)] = [
    (control, true, [.maskControl]),
    (option, true, [.maskControl, .maskAlternate]),
    (s, true, [.maskControl, .maskAlternate]),
    (s, false, [.maskControl, .maskAlternate]),
    (option, false, [.maskControl]),
    (control, false, []),
  ]
  for (key, down, flags) in steps {
    let event = CGEvent(keyboardEventSource: source, virtualKey: key, keyDown: down)
    if key != s { event?.type = .flagsChanged }
    event?.flags = flags
    event?.post(tap: .cghidEventTap)
    Thread.sleep(forTimeInterval: 0.04)
  }
}

// A window of our own on the main display, so the clicks and drags touch nothing else.
let app = NSApplication.shared
app.setActivationPolicy(.accessory)
guard let screen = NSScreen.screens.first else { stop(2, "no display") }
let size = CGSize(width: 640, height: 420)
let window = NSWindow(
  contentRect: CGRect(
    x: screen.frame.midX - size.width / 2, y: screen.frame.midY - size.height / 2,
    width: size.width, height: size.height),
  styleMask: [.titled], backing: .buffered, defer: false)
window.title = "Shift ink harness (closes by itself)"
window.level = .floating
window.isReleasedWhenClosed = false

// Global (top-left origin) points inside the window.
let left = Double(screen.frame.midX - size.width / 2) + 60
let top = Double(screen.frame.height - (screen.frame.midY + size.height / 2)) + 80
func point(_ x: Double, _ y: Double) -> CGPoint { CGPoint(x: left + x, y: top + y) }

func play(_ name: String) {
  switch name {
  case "all":
    for part in ["move", "drag", "click"] {
      play(part)
      Thread.sleep(forTimeInterval: 0.5)
    }
  case "move":
    mouse(.mouseMoved, point(0, 0))
    shift(true)
    glide(.mouseMoved, from: point(0, 0), to: point(300, 60))
    shift(false)
  case "drag":
    mouse(.mouseMoved, point(0, 120))
    shift(true)
    mouse(.leftMouseDown, point(0, 120))
    glide(.leftMouseDragged, from: point(0, 120), to: point(300, 180))
    mouse(.leftMouseUp, point(300, 180))
    shift(false)
  case "click":
    mouse(.mouseMoved, point(150, 260))
    Thread.sleep(forTimeInterval: 0.1)
    shift(true)
    click(point(150, 260))
    shift(false)
    capture()
  case "pins":
    for x in [100.0, 220.0, 340.0] {
      mouse(.mouseMoved, point(x, 260))
      Thread.sleep(forTimeInterval: 0.1)
      shift(true)
      click(point(x, 260))
      shift(false)
    }
    capture()
  case "held-pins":
    mouse(.mouseMoved, point(100, 260))
    Thread.sleep(forTimeInterval: 0.1)
    shift(true)
    click(point(100, 260))
    glide(.mouseMoved, from: point(100, 260), to: point(220, 260), seconds: 0.25)
    click(point(220, 260))
    glide(.mouseMoved, from: point(220, 260), to: point(340, 260), seconds: 0.25)
    click(point(340, 260))
    shift(false)
    capture()
  case "button-first":
    mouse(.mouseMoved, point(0, 120))
    mouse(.leftMouseDown, point(0, 120))
    Thread.sleep(forTimeInterval: 0.3)
    shift(true)
    glide(.leftMouseDragged, from: point(0, 120), to: point(300, 180))
    shift(false)
    mouse(.leftMouseUp, point(300, 180))
  case "late-shift":
    mouse(.mouseMoved, point(150, 120))
    mouse(.leftMouseDown, point(150, 120))
    Thread.sleep(forTimeInterval: 0.3)
    shift(true)
    shift(false)
    mouse(.leftMouseUp, point(150, 120))
  default: break
  }
}

struct Outcome {
  let bundle: URL
  let ink: Int
  let pins: [Int]
  let marks: Int
  /// The poller saw ⇧ go down more often than this run pressed it. The posted ⇧ lasts only
  /// until the next real input event (the real ⇧ key is up), so on a Mac in use it can drop out
  /// for a few milliseconds, which ends the stroke or the hold early. A real ⇧ key never does
  /// that, so such a run says nothing about Annotator and is made again.
  let disturbed: Bool
}

/// One recording of the scenario.
func record() -> Outcome {
  guard !recording(liveLog) else {
    stop(2, "the live Annotator is recording; the test's ⇧ would draw into it. Try again later")
  }
  guard !recording(devLog) else { stop(2, "Annotator Dev is already recording") }
  window.orderFrontRegardless()
  RunLoop.current.run(until: Date().addingTimeInterval(0.4))
  let pointerBefore = CGEvent(source: nil)?.location
  let before = bundles()
  let start = logSize(devLog)
  shiftPresses = 0

  // The record shortcut takes the display under the pointer.
  mouse(.mouseMoved, point(0, 0))
  Thread.sleep(forTimeInterval: 0.2)
  recordShortcut()
  guard waitFor(devLog, from: start, seconds: 8, "start_recording_with_target") else {
    stop(2, "Annotator Dev did not start recording on ⌃⌥S (see \(devLog.path))")
  }
  Thread.sleep(forTimeInterval: 1.0)
  play(scenario)
  Thread.sleep(forTimeInterval: 0.5)
  let shiftSeen = logLines(devLog, from: start).filter {
    $0.contains("hid_poller") && $0.contains("shift=true")
  }.count
  let stopAt = logSize(devLog)
  recordShortcut()
  let finished = waitFor(devLog, from: stopAt, seconds: 30, "finish timings")
  window.orderOut(nil)
  if let pointerBefore { CGWarpMouseCursorPosition(pointerBefore) }
  guard finished else { stop(2, "Annotator Dev did not finish the recording (see \(devLog.path))") }

  guard let name = bundles().subtracting(before).sorted().last else {
    stop(2, "no new bundle in \(captures.path)")
  }
  let bundle = captures.appendingPathComponent(name)
  guard let data = try? Data(contentsOf: bundle.appendingPathComponent("scrawl.json")),
    let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
  else { stop(2, "could not read \(bundle.path)/scrawl.json") }
  let strokes = root["strokes"] as? [[String: Any]] ?? []
  let pins = strokes.compactMap { ($0["pin"] as? NSNumber)?.intValue }
  return Outcome(
    bundle: bundle, ink: strokes.count - pins.count, pins: pins,
    marks: (root["marks"] as? [Any])?.count ?? 0, disturbed: shiftSeen > shiftPresses)
}

let attempts = 6
for attempt in 1...attempts {
  let outcome = record()
  if outcome.disturbed && attempt < attempts {
    say("attempt \(attempt): real input interrupted the posted ⇧; recording again")
    Thread.sleep(forTimeInterval: 2)
    continue
  }
  let ok = outcome.ink == want.ink && outcome.pins == want.pins
  print("bundle: \(outcome.bundle.path)")
  print("scenario: \(scenario) (attempt \(attempt))")
  print("ink strokes: \(outcome.ink) (want \(want.ink))")
  print("pins: \(outcome.pins) (want \(want.pins))")
  print("marks: \(outcome.marks)")
  if outcome.disturbed && !ok {
    print("DISTURBED: real input interrupted the posted ⇧ in every attempt")
    exit(3)
  }
  print(ok ? "PASS" : "FAIL")
  exit(ok ? 0 : 1)
}
