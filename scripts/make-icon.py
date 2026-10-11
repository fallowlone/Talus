#!/usr/bin/env python3
"""Draws the Talus app icon (black rounded square, three white talus bars) and writes assets/Talus.icns."""
import subprocess, sys, tempfile
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
S = 1024


def draw() -> Image.Image:
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    inset = 100  # macOS icon grid leaves a margin around the squircle
    d.rounded_rectangle((inset, inset, S - inset, S - inset), radius=190, fill=(14, 14, 14, 255))
    d.rounded_rectangle((inset, inset, S - inset, S - inset), radius=190, outline=(48, 48, 48, 255), width=6)
    # Three talus: widths shrink downward, greys fade like geological layers.
    bars = [(0.52, 255), (0.40, 200), (0.28, 140)]
    h, gap = 92, 62
    total = 3 * h + 2 * gap
    top = (S - total) // 2
    for i, (frac, grey) in enumerate(bars):
        w = int((S - 2 * inset) * frac)
        x0 = (S - w) // 2
        y0 = top + i * (h + gap)
        d.rounded_rectangle((x0, y0, x0 + w, y0 + h), radius=h // 2, fill=(grey, grey, grey, 255))
    return img


def main() -> int:
    base = draw()
    with tempfile.TemporaryDirectory() as tmp:
        iconset = Path(tmp) / "Talus.iconset"
        iconset.mkdir()
        for size in (16, 32, 128, 256, 512):
            base.resize((size, size), Image.LANCZOS).save(iconset / f"icon_{size}x{size}.png")
            base.resize((size * 2, size * 2), Image.LANCZOS).save(iconset / f"icon_{size}x{size}@2x.png")
        out = ROOT / "assets" / "Talus.icns"
        out.parent.mkdir(exist_ok=True)
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(out)], check=True)
    base.save(ROOT / "assets" / "icon-1024.png")
    print(f"wrote {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
