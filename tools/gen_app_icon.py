#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成 macOS 应用图标（像素风，与 slint-pixel 的界面配色同源）。

用法：python3 tools/gen_app_icon.py            # 写 assets/icon.iconset/ 与 assets/icon.icns
      python3 tools/gen_app_icon.py --png-only # 只出 iconset（没装 iconutil 时）

为什么要生成而不是随手画一个二进制：图标要能跟着配色改。配色取的是 `slint-pixel`
主题里那套 PICO-8 色（界面用的就是它），所以图标和 UI 是同一套颜色，不会各调各的。

图案：深色圆角方块 + 一排高低起伏的竖条（波形）。竖条对齐到 32 格网格 —— 放大看是
像素块，缩到 16px 也还认得出是"音频"，这正是像素风图标需要的性质。
"""

import argparse
import os
import shutil
import subprocess
import sys

try:
    from PIL import Image, ImageDraw
except ImportError:  # pragma: no cover
    sys.exit("需要 Pillow：python3 -m pip install --user Pillow")

# ── 配色（slint-pixel 主题里的值，别自己编） ──────────────────────────────
BG = (0x1A, 0x1A, 0x1A)          # #1a1a1a 深底
ACCENT = (0xFF, 0x00, 0x4D)      # #ff004d 主色（与界面强调色一致）
ACCENT_2 = (0xFF, 0xE1, 0x4D)    # #ffe14d 次色（点缀）
EDGE = (0x2E, 0x2E, 0x2E)        # 比底略亮的一圈，避免在深色壁纸上糊成一团

SIDE = 1024                      # 最终边长
SS = 2                           # 超采样倍数（圆角才不毛糙）
GRID = 32                        # 像素格数：竖条都对齐到它
BARS = [7, 12, 18, 23, 18, 12, 7]  # 每根竖条的高度（格），中间高两边低


def render() -> Image.Image:
    n = SIDE * SS
    cell = n // GRID
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # 圆角方块：留 1 格边距，圆角取 5.5 格（近似 macOS 的 squircle 观感）
    pad, radius = cell, int(cell * 5.5)
    d.rounded_rectangle([pad, pad, n - pad, n - pad], radius=radius, fill=BG, outline=EDGE, width=cell // 3)

    # 波形：7 根竖条居中排布，底部对齐同一条基线
    gap = 2                                      # 条间距（格）
    width = (GRID - 2 * pad // cell - (len(BARS) - 1) * gap) // len(BARS)
    total = len(BARS) * width + (len(BARS) - 1) * gap
    x0 = (GRID - total) // 2
    base = GRID - 7                              # 基线（格）
    for i, h in enumerate(BARS):
        x = (x0 + i * (width + gap)) * cell
        bar = [x, (base - h) * cell, x + width * cell, base * cell]
        # 中间的条用主色，两侧两根用次色 —— 一点点层次，16px 下也能看出中间是重心
        color = ACCENT if 1 <= i <= len(BARS) - 2 else ACCENT_2
        d.rectangle(bar, fill=color)

    return img.resize((SIDE, SIDE), Image.LANCZOS)


SIZES = [16, 32, 64, 128, 256, 512, 1024]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--png-only", action="store_true", help="只产出 iconset，不调用 iconutil")
    ap.add_argument("--out", default="assets", help="输出目录（默认 assets/）")
    a = ap.parse_args()

    iconset = os.path.join(a.out, "icon.iconset")
    shutil.rmtree(iconset, ignore_errors=True)
    os.makedirs(iconset, exist_ok=True)
    base = render()
    for s in SIZES:
        if s <= 512:
            base.resize((s, s), Image.LANCZOS).save(os.path.join(iconset, f"icon_{s}x{s}.png"))
        if s >= 32:
            base.resize((s, s), Image.LANCZOS).save(os.path.join(iconset, f"icon_{s // 2}x{s // 2}@2x.png"))
    print(f"iconset → {iconset}")

    if a.png_only:
        return 0
    icns = os.path.join(a.out, "icon.icns")
    r = subprocess.run(["iconutil", "-c", "icns", iconset, "-o", icns], capture_output=True, text=True)
    if r.returncode != 0:
        print(f"iconutil 失败：{r.stderr.strip()}", file=sys.stderr)
        print("（Linux/CI 上没有 iconutil；用 --png-only，或在 macOS 上重跑）", file=sys.stderr)
        return r.returncode
    print(f"icns    → {icns}  ({os.path.getsize(icns)} 字节)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
