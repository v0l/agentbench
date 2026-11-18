#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=${OUT:-/tmp/abshots}
INSET=${INSET:-28}
BOTTOM=${BOTTOM:-20}
SCALE=${SCALE:-1}
NAMES=${NAMES:-"393x852 360x800 412x915 320x640"}
COLS=${COLS:-7}

spec() {
  local name=$1 w h
  w=${name%%x*}
  h=${name##*x}
  case "$name" in
    *-land) w=${name%%x*}; h=${name#*x}; h=${h%-land}; echo "$name=${h}x${w}:$SCALE:48,24,0,$BOTTOM" ;;
    desktop) echo "desktop=1280x800:1:0,0,0,0" ;;
    wide) echo "wide=1920x1080:1:0,0,0,0" ;;
    tablet) echo "tablet=800x1280:1:0,0,0,0" ;;
    tablet-land) echo "tablet-land=1280x800:1:0,0,0,0" ;;
    *) echo "$name=${w}x${h}:$SCALE:0,$INSET,0,$BOTTOM" ;;
  esac
}

sizes=""
for n in $NAMES; do sizes="$sizes;$(spec "$n")"; done
sizes=${sizes#;}

export AB_OUT=$OUT AB_SIZES=$sizes AB_ONLY=${ONLY:-}
rm -rf "$OUT"
mkdir -p "$OUT"
ulimit -n 65536 2>/dev/null || true
cargo test -p agentbench --test sweep render_the_sheets -- --nocapture

python3 - "$OUT" "$COLS" <<'EOF'
import sys, glob, os
from PIL import Image, ImageDraw
out, cols = sys.argv[1], int(sys.argv[2])
for d in sorted(glob.glob(f"{out}/*/")):
    files = sorted(glob.glob(f"{d}[0-9]*.png"))
    if not files:
        continue
    ims = [Image.open(f).convert("RGB") for f in files]
    w, h = ims[0].size
    rows = (len(ims) + cols - 1) // cols
    sheet = Image.new("RGB", (w * cols, h * rows), "white")
    draw = ImageDraw.Draw(sheet)
    for i, (f, im) in enumerate(zip(files, ims)):
        x, y = (i % cols) * w, (i // cols) * h
        sheet.paste(im, (x, y))
        draw.rectangle([x, y, x + w - 1, y + h - 1], outline=(255, 0, 255))
        draw.text((x + 4, y + h - 14), os.path.basename(f)[:-4], fill=(255, 255, 0))
    sheet.thumbnail((2400, 2400 * rows))
    sheet.save(f"{d}sheet.png")
    print(f"{d}sheet.png")
EOF
