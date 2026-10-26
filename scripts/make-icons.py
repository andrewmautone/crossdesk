"""Render assets/icon.svg and assets/tray.svg into every icon file the app needs.

Needs `resvg` (cargo install resvg) and Pillow. Outputs go to assets/generated/
and are committed, so normal builds do not need either tool.
"""
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"
OUT = ASSETS / "generated"
RESVG = shutil.which("resvg") or str(Path.home() / ".cargo" / "bin" / "resvg")


def render(svg_text: str, size: int, dest: Path, height: int | None = None) -> Image.Image:
    with tempfile.TemporaryDirectory() as tmp:
        src = Path(tmp) / "in.svg"
        src.write_text(svg_text, encoding="utf-8")
        h = height or size
        subprocess.run([RESVG, "-w", str(size), "-h", str(h), str(src), str(dest)], check=True)
    return Image.open(dest).convert("RGBA")


def main() -> None:
    OUT.mkdir(exist_ok=True)
    icon = (ASSETS / "icon.svg").read_text(encoding="utf-8")
    # Windows icons fill their square, so crop away the macOS grid margin.
    tight = re.sub(r'viewBox="[^"]*"', 'viewBox="100 100 824 824"', icon, count=1)
    small = (ASSETS / "icon-small.svg").read_text(encoding="utf-8")
    tray = (ASSETS / "tray.svg").read_text(encoding="utf-8")

    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        # Small sizes use the simplified drawing so the screens stay readable.
        win = [render(small if s <= 32 else tight, s, tmp / f"w{s}.png") for s in (16, 20, 24, 32, 40, 48, 64, 128, 256)]
        win[-1].save(OUT / "crossdesk.ico", sizes=[im.size for im in win], append_images=win[:-1])

        mac = render(icon, 1024, tmp / "m1024.png")
        mac.save(OUT / "crossdesk.icns")

    render(tight, 256, OUT / "window-256.png")
    render(small, 32, OUT / "tray-color-32.png")
    render(tray, 64, OUT / "tray-template-64x48.png", height=48)
    print("icons written to", OUT)


if __name__ == "__main__":
    sys.exit(main())
