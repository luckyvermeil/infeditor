"""Regenerate every raster icon asset from the two source SVGs.

    python materials/build.py

Sources (this directory - the design material, not a build input)
    infeditor.svg        >= 32px design (soft drop shadow under the page)
    infeditor-small.svg  16/20/24px optical variant (no shadow, bigger page)

Outputs (all overwritten; these are what the installers and Cargo bundle
reference, so they stay under packaging/)
    packaging/icon/png/infeditor-<size>.png   one file per size, for docs/READMEs
    packaging/windows/infeditor.ico           MSI / NSIS / shortcut icon
    packaging/linux/infeditor.png             64x64, the hicolor path in Cargo.toml

Why hand-roll the ICO instead of `Pillow.save(format="ICO")`: Pillow resizes one
master bitmap into every frame, so the 16px frame would be a downscaled blurred
256 - here each frame is rasterised from the SVG at its native size. Sizes up to
64px are stored as 32-bit BMP (widest compatibility, NSIS reads these), 128/256
as PNG (a 256px BMP frame is legal but rare, and PNG keeps the file small).

Requires: pillow, resvg-py (`pip install pillow resvg-py`).
"""

from __future__ import annotations

import io
import struct
from pathlib import Path

import resvg_py
from PIL import Image

SOURCE_DIR = Path(__file__).resolve().parent
ROOT = SOURCE_DIR.parent
PNG_DIR = ROOT / "packaging" / "icon" / "png"

MASTER = SOURCE_DIR / "infeditor.svg"
SMALL = SOURCE_DIR / "infeditor-small.svg"

SMALL_SIZES = (16, 20, 24)
# 1024 is not a Windows/Docs size: the macOS iconset needs icon_512x512@2x, and
# upscaling the 512 there is visible in Finder on a Retina display.
ALL_SIZES = (16, 20, 24, 32, 40, 48, 64, 96, 128, 256, 512, 1024)
ICO_SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
ICO_PNG_FROM = 128  # >= this size the frame is PNG-compressed


def raster(src: Path, size: int) -> Image.Image:
    png = resvg_py.svg_to_bytes(svg_path=str(src), width=size, height=size)
    return Image.open(io.BytesIO(png)).convert("RGBA")


def source_for(size: int) -> Path:
    return SMALL if size in SMALL_SIZES else MASTER


def bmp_frame(img: Image.Image) -> bytes:
    """32-bit BGRA DIB + all-zero AND mask, as stored inside an .ico."""
    w, h = img.size
    px = img.load()
    rows = []
    for y in range(h - 1, -1, -1):  # DIB rows are bottom-up
        row = bytearray()
        for x in range(w):
            r, g, b, a = px[x, y]
            row += bytes((b, g, r, a))
        rows.append(bytes(row))
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, w * h * 4, 0, 0, 0, 0)
    mask_stride = ((w + 31) // 32) * 4  # 1bpp, rows padded to 4 bytes
    mask = bytes(mask_stride * h)
    return header + b"".join(rows) + mask


def png_frame(img: Image.Image) -> bytes:
    buf = io.BytesIO()
    img.save(buf, format="PNG", optimize=True)
    return buf.getvalue()


def write_ico(frames: dict[int, Image.Image], path: Path) -> None:
    entries, blobs = [], []
    offset = 6 + 16 * len(frames)  # ICONDIR + one ICONDIRENTRY per frame
    for size in sorted(frames):
        blob = png_frame(frames[size]) if size >= ICO_PNG_FROM else bmp_frame(frames[size])
        entries.append(
            struct.pack(
                "<BBBBHHII",
                0 if size >= 256 else size,
                0 if size >= 256 else size,
                0,  # palette colours (0 = truecolour)
                0,  # reserved
                1,  # colour planes
                32,  # bits per pixel
                len(blob),
                offset,
            )
        )
        blobs.append(blob)
        offset += len(blob)
    path.write_bytes(
        struct.pack("<HHH", 0, 1, len(frames)) + b"".join(entries) + b"".join(blobs)
    )


def main() -> None:
    PNG_DIR.mkdir(parents=True, exist_ok=True)

    rendered: dict[int, Image.Image] = {}
    for size in ALL_SIZES:
        img = raster(source_for(size), size)
        rendered[size] = img
        img.save(PNG_DIR / f"infeditor-{size}.png", optimize=True)
    print(f"png/      {len(rendered)} sizes -> {PNG_DIR.relative_to(ROOT)}")

    write_ico({s: rendered[s] for s in ICO_SIZES}, ROOT / "packaging" / "windows" / "infeditor.ico")
    print(f"ico       {len(ICO_SIZES)} frames -> packaging/windows/infeditor.ico")

    rendered[64].save(ROOT / "packaging" / "linux" / "infeditor.png", optimize=True)
    print("linux     64x64 -> packaging/linux/infeditor.png")


if __name__ == "__main__":
    main()
