"""Builds every app icon size from art/BlazingDL.png.

Writes crates/app/assets/icons/app/blazingdl-<size>.png, blazingdl.ico (all sizes) and the
browser extension's icons. Run from the repository root: python -I tools/icon/build_icon.py
"""

from pathlib import Path
import base64
import re

from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
SRC = ROOT / "art" / "BlazingDL.png"
APP = ROOT / "crates" / "app" / "assets" / "icons" / "app"
EXT = ROOT / "extension" / "icons"
VIDEO_JS = ROOT / "extension" / "video.js"
SIZES = [16, 20, 24, 32, 48, 64, 128, 256]


def opaque(im: Image.Image) -> Image.Image:
    r, g, b, a = im.split()
    return Image.merge("RGBA", (r, g, b, a.point(lambda v: 255 if v >= 240 else v)))


def square(im: Image.Image, box, pad: float) -> Image.Image:
    c = im.crop(box)
    w, h = c.size
    s = int(max(w, h) * (1 + 2 * pad))
    out = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    out.paste(c, ((s - w) // 2, (s - h) // 2), c)
    return out


def main() -> None:
    src = opaque(Image.open(SRC).convert("RGBA"))
    bbox = src.split()[3].point(lambda v: 255 if v > 16 else 0).getbbox()
    full = square(src, bbox, 0.02)
    icons = {}
    for size in SIZES:
        icons[size] = full.resize((size, size), Image.LANCZOS)
        icons[size].save(APP / f"blazingdl-{size}.png")
    big = icons[256]
    big.save(
        APP / "blazingdl.ico",
        sizes=[(s, s) for s in SIZES],
        append_images=[icons[s] for s in SIZES if s != 256],
    )
    for size in (16, 32, 48, 128):
        icons[size].save(EXT / f"icon-{size}.png")
    if VIDEO_JS.exists():
        b64 = base64.b64encode((EXT / "icon-48.png").read_bytes()).decode("ascii")
        txt = VIDEO_JS.read_text(encoding="utf-8")
        new_txt, n = re.subn(
            r'const LOGO = "data:image/png;base64,[^"]+";',
            f'const LOGO = "data:image/png;base64,{b64}";',
            txt,
            count=1,
        )
        if n:
            VIDEO_JS.write_text(new_txt, encoding="utf-8")
    print("wrote", ", ".join(str(s) for s in SIZES), "+ blazingdl.ico + extension icons + video.js LOGO")


if __name__ == "__main__":
    main()
