#!/usr/bin/env python3
"""生成 RDM Firefox 扩展图标（PNG）。

Firefox 对扩展图标最好提供 PNG（16/32/48/96），SVG 在部分 UI 场景
（about:addons、AMO 列表、系统通知）兼容性不稳定。本脚本用 Pillow 从
384×384 主图缩放生成，保证小尺寸下依然清晰。

用法：
    python extension/firefox/icons/generate-icons.py
"""

from __future__ import annotations

import sys
from pathlib import Path

try:
    from PIL import Image, ImageDraw
except ImportError:  # pragma: no cover - 仅开发期使用
    sys.exit("需要 Pillow：pip install pillow")

MASTER = 384
SIZES = (16, 32, 48, 96)
OUT_DIR = Path(__file__).resolve().parent

TOP_COLOR = (47, 129, 247)      # #2f81f7
BOTTOM_COLOR = (26, 79, 181)    # #1a4fb5
WHITE = (255, 255, 255, 255)


def rounded_mask(size: int, radius: int) -> Image.Image:
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, size - 1, size - 1), radius=radius, fill=255)
    return mask


def gradient(size: int) -> Image.Image:
    image = Image.new("RGB", (size, size))
    pixels = image.load()
    for y in range(size):
        ratio = y / max(1, size - 1)
        color = tuple(
            round(TOP_COLOR[channel] + (BOTTOM_COLOR[channel] - TOP_COLOR[channel]) * ratio)
            for channel in range(3)
        )
        for x in range(size):
            pixels[x, y] = color
    return image


def draw_master() -> Image.Image:
    size = MASTER
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.paste(gradient(size), (0, 0), rounded_mask(size, radius=84))
    draw = ImageDraw.Draw(canvas)

    # 下载箭头：竖杆 + 三角箭头
    shaft_half = 23
    shaft_top = 88
    shaft_bottom = 224
    draw.rounded_rectangle(
        (size // 2 - shaft_half, shaft_top, size // 2 + shaft_half, shaft_bottom),
        radius=14,
        fill=WHITE,
    )
    draw.polygon(
        [
            (size // 2 - 73, shaft_bottom - 4),
            (size // 2 + 73, shaft_bottom - 4),
            (size // 2, shaft_bottom + 72),
        ],
        fill=WHITE,
    )

    # 底座托盘
    draw.rounded_rectangle(
        (94, 306, size - 94, 330),
        radius=12,
        fill=WHITE,
    )

    return canvas


def main() -> None:
    master = draw_master()
    for size in SIZES:
        icon = master.resize((size, size), Image.LANCZOS)
        path = OUT_DIR / f"rdm-{size}.png"
        icon.save(path, format="PNG", optimize=True)
        print(f"wrote {path} ({size}x{size})")
    # 供 AMO 列表/文档使用的较大图，不写进 manifest 也能用
    large = master.resize((128, 128), Image.LANCZOS)
    large.save(OUT_DIR / "rdm-128.png", format="PNG", optimize=True)
    print(f"wrote {OUT_DIR / 'rdm-128.png'} (128x128)")


if __name__ == "__main__":
    main()
