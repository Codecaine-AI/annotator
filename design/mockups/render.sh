#!/usr/bin/env bash
# Render design/mockups/out/*.html to design/mockups/images/*.png at 2x, cropped to the content.
# Renders the light theme. Needs Google Chrome, python3 with Pillow, and pngquant (optional, shrinks each PNG).
set -euo pipefail
cd "$(dirname "$0")/../.."
chrome="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
out=design/mockups/images
mkdir -p "$out"
for html in design/mockups/out/*.html; do
  name="$(basename "$html" .html)"
  png="$out/$name.png"
  "$chrome" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=2 \
    --blink-settings=preferredColorScheme=1 \
    --window-size=1000,2400 --screenshot="$png" "file://$PWD/$html" >/dev/null 2>&1
  python3 - "$png" <<'PY'
import sys
from PIL import Image, ImageChops
path = sys.argv[1]
img = Image.open(path).convert("RGB")
bg = Image.new("RGB", img.size, img.getpixel((img.width - 1, img.height - 1)))
box = ImageChops.difference(img, bg).getbbox()
if box:
    img = img.crop(box)
# Add a 32 px (16 pt) margin of the page color on every side.
pad = 32
framed = Image.new("RGB", (img.width + 2 * pad, img.height + 2 * pad), bg.getpixel((0, 0)))
framed.paste(img, (pad, pad))
framed.save(path)
PY
  if command -v pngquant >/dev/null; then
    pngquant --force --skip-if-larger --quality 80-95 --output "$png" "$png" || true
  fi
  echo "$png $(($(stat -f%z "$png") / 1024)) KB"
done
