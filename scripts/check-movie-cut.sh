#!/bin/bash
# Proves `scrawl-helper cut` (a paused recording without its pauses) on a movie made for it.
#
#   scripts/check-movie-cut.sh [path/to/scrawl-helper]
#
# ffmpeg writes a 10 s, 30 fps H.264 movie (a keyframe every 2 s) with a tone, in which every
# frame shows its own number as ten black or white blocks. The helper keeps 0 to 2.3 s and
# 5.7 to 10 s, both cuts between keyframes. The check then decodes frames around the cut with
# the helper (AVFoundation) and with ffmpeg and reads their numbers: the frame after number 68
# must be number 171, and the movie and its audio must be 6.6 s long. Needs ffmpeg and python3.
set -euo pipefail
helper="${1:-$(dirname "$0")/../target/debug/scrawl-helper}"
[ -x "$helper" ] || { echo "no scrawl-helper at $helper (cargo build first)" >&2; exit 1; }
command -v ffmpeg >/dev/null || { echo "ffmpeg is needed" >&2; exit 1; }
dir="$(mktemp -d "${TMPDIR:-/tmp}/scrawl-cut.XXXXXX")"
ffmpeg -v error -y \
  -f lavfi -i "nullsrc=s=320x240:r=30,format=yuv420p,geq=lum='255*mod(floor(N/pow(2,floor(X/32))),2)':cb=128:cr=128" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -t 10 -c:v libx264 -g 60 -bf 0 -pix_fmt yuv420p -c:a aac "$dir/source.mp4"
"$helper" cut "$dir/source.mp4" --out "$dir/cut.mp4" --keep 0-2300,5700-10000 > "$dir/cut.json"
mkdir "$dir/frames"
"$helper" frames "$dir/cut.mp4" --out-dir "$dir/frames" --at 0,2000,2290,2320,3000,6500 > /dev/null
"$helper" audio "$dir/cut.mp4" "$dir/cut.wav" > /dev/null
ffmpeg -v error -y -i "$dir/cut.mp4" -vf "select='between(n,67,70)'" -fps_mode passthrough \
  -f rawvideo -pix_fmt gray "$dir/ffmpeg.gray"
python3 - "$dir" <<'PY'
import json, sys, wave
d = sys.argv[1]
def number(pixels, stride, offset=0):
    return sum(1 << k for k in range(10) if pixels[offset + (120 * 320 + 16 + 32 * k) * stride] > 128)
cut = json.load(open(f"{d}/cut.json"))
helper = [number(open(f"{d}/frames/frame-{i:04d}.rgba", "rb").read(), 4) for i in range(6)]
raw = open(f"{d}/ffmpeg.gray", "rb").read()
ffmpeg = [number(raw, 1, i * 320 * 240) for i in range(4)]
with wave.open(f"{d}/cut.wav") as w:
    audio_ms = round(w.getnframes() * 1000 / w.getframerate())
# New time -> source frame: 0 s, 2 s, the last frame before the cut, the first after it,
# then 3 s (source 6.4 s) and 6.5 s (source 9.9 s).
checks = [
    ("movie length", cut["durationMs"], 6600, 40),
    ("audio length", audio_ms, 6600, 40),
    ("helper frames at 0, 2000, 2290, 2320, 3000, 6500 ms", helper, [0, 60, 68, 171, 192, 297], 0),
    ("ffmpeg frames 67 to 70", ffmpeg, [67, 68, 171, 172], 0),
]
failed = False
for name, got, want, slack in checks:
    ok = got == want if slack == 0 else abs(got - want) <= slack
    failed |= not ok
    print(f"{'ok  ' if ok else 'FAIL'} {name}: {got} (expected {want})")
sys.exit(1 if failed else 0)
PY
echo "files: $dir"
