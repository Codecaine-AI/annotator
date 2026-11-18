---
name: scrawl
description: Read and dig into Scrawl screen captures, where someone recorded their screen while talking, clicking and drawing red marks on it. Use whenever a message contains a Scrawl capture (a line like `[scrawl 0:12 · App “Window” · /path/to/….scrawl]`, a `.scrawl` bundle path, or a contact-sheet.png from one), when the user says "look at my scrawl" or refers to something they just recorded or drew, or when you need the `scrawl` CLI to zoom into a moment, pull frames, send a capture to a session, or export one as video.
---

# Scrawl captures

Scrawl records someone's screen, voice, clicks and red ink, then packs the result into a `.scrawl` folder (a bundle). They send it to show you a problem instead of describing it, so the capture is the bug report. Read it closely before you answer, and don't ask them to explain again what the capture already shows. If something is unclear, look closer with `scrawl zoom` first.

## What a paste looks like

```text
[scrawl 0:33 · OpenCode “Settings” · /Users/me/Library/Application Support/scrawl/captures/scrawl-1792432139176.scrawl]
Open contact-sheet.png and frames/ in that folder before changing anything: the marks and words only make sense on the images, which can show more than the text does.
Cropped to: the activity in OpenCode (full frames in frames-full/)
- #1 (0:04) [clicked “New tab”] “Command T makes a new session, but here it should make a tab.”
- #2 (0:07) “No, it doesn't.”
- #3 (0:10) ⚑ “It says command shift B makes a new tab.”
- #4 (0:18) clicked (x 0.51, y 0.93)
```

- The first line has the length, the app and window recorded, and the bundle path.
- The next line asks you to open the images first. Do: the text is only what was said and done, and a problem can show in a frame without being marked or mentioned.
- Each `#N (M:SS)` line is a keyframe: what they did at that moment in brackets, then what they said while it was on screen. A line with no quote has the action alone.
- `⚑` means they flagged the moment on purpose. `⟨Name⟩` means the app reported a new screen. Lines starting `·` are events or values the app logged.
- Optional lines near the top: `For session:` (where Scrawl sent it), `App shots:` (the command that prints a keyframe's app state, see below), `Screen:` with app context, `Note:`, and `Cropped to:`.
- Coordinates are fractions of the captured area, from the top left (x 0.51 is just right of centre).

When you answer, refer to moments the same way, as in "#3 (0:10)", so the person can find them on the contact sheet.

## Reading a bundle

1. Look at `contact-sheet.png` in the bundle first (it's often attached already). It shows every keyframe in time order, left to right and top to bottom, each with its number `N` in a small white chip in a top corner (the `#N` in `summary.md`).
2. Read the transcript lines next to it. Speech recognition mishears names and code terms, so trust the screen when the two disagree.
3. Open `frames/frame-NN.png` for any keyframe you need at full size (`frame-03.png` is #3). When the summary says `Cropped to:`, the uncropped screens are in `frames-full/`.

The person can pause a recording (Esc) and resume it. Paused time is cut out of the video, the audio, the transcript and every time in the bundle, so times run on without a gap. A sudden change between two keyframes can be a pause.

What the drawings mean:

- Red strokes, circles, underlines and arrows are the person's marks, and they point at the problem. Marks drawn on the same screen all show on one keyframe. When there are several, the text numbers them in drawing order (`✎① 0:06 …`, `✎② 0:09 …`); the image shows only the ink, so use `scrawl zoom --mark` to see which ink is which.
- A red disc in a white ring with a white number is a pin: a deliberate "here". Pins are numbered 1, 2, 3 in the order they were dropped in the whole recording, and the text calls them `pin N`. A pin number is not a mark number: `scrawl inspect --marks` lists each pin's mark id (`m3`), and `scrawl zoom --mark m3` zooms to it.
- A ripple around the cursor shows a click or tap, and a finger trail shows a swipe.
- A glowing red trail that tapers off is the laser pointer, used to point without leaving a mark.

Other files: `scrawl.json` is the full manifest (keyframes, `marks` with shape and box, `events`, `transcript.words` with times, app `context`, and `autocrop`). `recording.mp4` or `recording.mov` and `audio.wav` are the source media. A `.media-pending` file means the video is still being copied from another Mac. `appshots/` holds the app state of Mac captures: `shot-NN.json` is one window's app, title, URL and text outline (`tree`), `timeline.json` says when each was seen, and a keyframe's `appshot` in `scrawl.json` names its file. It stays after the video is cleaned.

## The scrawl CLI

`scrawl` is the app's own binary (Annotator.app, or upstream's Scrawl.app). If `command -v scrawl` finds nothing, use `/Applications/Annotator.app/Contents/MacOS/scrawl` (or `/Applications/Scrawl.app/Contents/MacOS/scrawl`), and tell the person they can put it on PATH with Install in the app's Settings › Agents. Annotator Dev.app's command is `scrawl-dev`, with its own captures. It runs only on the Mac with Scrawl installed. Anywhere else, read `contact-sheet.png`, `frames/` and `scrawl.json` directly. Every command takes a bundle path, and most default to `latest`, the newest capture. Quote the path, because it usually contains `Application Support`.

### Look closer at a moment

Use `zoom` when the problem is motion (an animation, a jump, a flicker) or happens between keyframes, or when you can't tell what a mark points at:

```sh
scrawl zoom <bundle> --at 0:21                 # a time, M:SS or seconds
scrawl zoom <bundle> --at "the width changes"  # or a phrase they said
scrawl zoom <bundle> --at 0:21 --span 1 --frames 16 --full
scrawl zoom <bundle> --mark 2                  # the mark shown as ✎② in the text
scrawl zoom <bundle> --mark 1 --frame 4        # ✎① of keyframe #4, when several have a ✎①
```

It prints the path of a PNG sheet of frames around that moment, the time of each cell, and what was said. Open the PNG and look at it before you act on the feedback. `--span` is the seconds covered (default 2, a quarter of it before the moment), `--frames` the number of cells (default 12), and `--full` shows the whole screen instead of the crop. The cells are numbered from #1 within the sheet, so those numbers aren't keyframe numbers.

`--mark` takes a mark's number from the text or its id (`m2`) and centres the sheet on the time it was drawn, with only that mark's ink, drawn whole from the moment it starts. `scrawl inspect <bundle> --marks` lists every mark (`#2 ✎② 0:09 marked (2 strokes) · m2`: keyframe, number, time, shape, what it was drawn on, id), and `--marks --json` gives the same as JSON.

### Measure sizes and spacing

When the person talks about size, width, height, spacing, a gap, padding, alignment, or something that looks too big, too small or off, measure it before you answer or change code. Never estimate a size from the image.

1. Find the keyframe that shows the component, from their words and their marks.
2. Pick a point on the component's surface, or a region around the parts to compare. Give each as fractions of that keyframe's image (`frames/frame-NN.png`), from 0 to 1. On a flat fill, a point a few pixels off finds the same box.
3. Run `scrawl measure`:

```sh
scrawl measure <bundle> --frame 3 --at 0.42,0.31                # the box that holds the point (no gaps)
scrawl measure <bundle> --frame 3 --region 0.1,0.2,0.9,0.6      # every box in the region and the gaps between them
scrawl measure <bundle> --frame 3 --at 0.42,0.31 --json
```

A point gives one box and no gaps. For a gap, a padding or the distance between two parts, draw a region around both and read `gap`. A region over text gives one box per word (a one-letter word can join the next), so draw the region tight around the parts you compare.

Put a point on the component's fill, not on its text. Every `note:` line says what the box is, so read them before you use the size:

- `the point was on text, a glyph or a thin line: this is the surface around it`: the point was on a label, in the hole of a letter such as "o", on a divider or on a 1 px border. The box is the surface next to it.
- `this is a small part under the point`: a part under 12 pt on a side, such as a 3 to 8 pt status dot, a count badge, an icon or a pill, measures itself. For the surface it sits on, move the point off it.
- `the point is on text, an icon or a line inside this box`: the box is the component. The point was not on its fill.
- `the box ends at lines`: a list row between dividers, a table cell or an outlined card. The box is the fill between the lines. A faint line lets it run on, so check it against the image.
- `more than one color borders this box`: a panel between other panels, or a gradient fill, which gives only a part of the component. Check it with a tight region.
- `the box includes a border, a shadow or a part against its fill`: the box has them, and the note gives the size of the fill alone.
- `no box: the point is on the page background`: use a region for the parts on it.
- `box N touches the region edge and may be cut off there`: the size is only what the region holds. Draw a wider region.

How to read the output:

- A point and a region give the same box, within 1 px, with and without the movie. With a border, both give the box with its border when the page has another color.
- A region joins parts up to 1.5 pt apart into one box, so two boxes that close show as one. A point on each gives each box.
- Rows split by dividers: a point in a row gives that row. A region over the rows gives the whole list as one box, because the dividers join them.
- A region shows at most 30 boxes and says how many it left out. A shown box next to a left-out one gets no gap, so draw a tighter region.
- `no boxes found` names the cause: one plain color, or only parts under 6 pt on a side (a thin line, small text). Neither mode measures a line under 6 pt thick: read its width in the source code.
- A faint 1 px border can give the area past it, up to the whole window. The note says the box ends at lines or has more than one color round it: check the box against the image, and try a second point or a tight region.
- Sizes come in pt, which match SwiftUI points and CSS px at 100% browser zoom, and in px of the recording (not the screen's pixels). Positions are in the full recording frame, which the keyframe image is a crop of, so read sizes and gaps rather than positions. An iPhone capture without a screen size gives px only. Compare each pt value with the project's design tokens, such as its spacing scale and max widths, and name the token that fits or the one it misses. When the source code sets the value, read it there too: the measure shows what rendered on screen.

### Other commands

```sh
scrawl inspect <bundle>               # summary.md, the text above
scrawl inspect <bundle> --json        # scrawl.json
scrawl inspect <bundle> --marks       # the marks, numbered as in the text [--json]
scrawl appshot <bundle>               # each keyframe's app, window, URL and tree size [--json]
scrawl appshot <bundle> --frame 2     # keyframe #2's window as a text outline [--json]
scrawl frames <bundle> --from 0:04 --to 0:06 [--fps 4]   # plain PNGs of a range; prints their paths
scrawl send <bundle> --session ses_…  # queue the capture into an OpenCode session
scrawl send <bundle> --clipboard      # copy the summary text to the clipboard
scrawl export <bundle> [--out demo.mp4] [--zoom] [--crop auto|full|x,y,w,h] [--size 1920]
```

- `appshot` prints what the app itself knew about the window a keyframe shows: use it when the picture leaves a name, a label or a URL unclear, or to find the element to change. `<bundle>` can be the capture's id from the `App shots:` line. The outline is one element per line, indented by depth, such as `button "Save" #save` (role, label, identifier). A text field shows its name, never what was typed in it, and a URL has no query. A header's `cut: nodes` (or `depth`, `bytes`, `timeout`) means the outline stops early. `no app shots in this capture` means it was made with app state off or on Windows; a keyframe without a shot says which window it expected.
- `zoom` and `frames` write into `<bundle>/zoom/`. Leave the rest of the bundle as it is, because Scrawl manages those files and cleans them up.
- `send` without `--session` goes to the session the capture was addressed to (the `For session:` line), and otherwise to the most recently active session. With a recorded destination it refuses a different `--session` unless you pass `--reroute`, so check with the person before rerouting.
- `export` writes one `.mp4` with the ink and clicks burned in and the audio in sync (default `~/Movies/Scrawl/<bundle>.mp4`). `--zoom` eases in toward the activity. `--no-ink`, `--no-clicks`, `--hevc` and `--fps` are also available.
- Captures are kept forever unless the person set a limit in Settings › Storage. Once video and audio are cleaned, `zoom`, `frames` and `export` can't read the video, but `frames/`, the contact sheet, `scrawl.json` and `appshots/` remain.
- `scrawl <command> --help` lists every flag, and `scrawl skill` prints this guide as the installed version knows it.
