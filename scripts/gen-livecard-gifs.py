#!/usr/bin/env python3
"""live 卡动效资产生成器:breathe.gif(呼吸圆点)+ shimmer.gif(Thinking 扫光)。

    python3 scripts/gen-livecard-gifs.py crates/summon/assets/livecard

产物已提交进仓库并 `include_bytes!` 进二进制 —— 跨机观感一致是前提,Pillow 不该成为 daemon 的
运行时依赖。本脚本只在需要重新生成时跑;里面的周期/灰阶/尺寸是 owner 2026-08-26 验收的定稿,
改动会改变已验收的观感。macOS 专用(取 Helvetica.ttc)。
"""
import math
import sys

from PIL import Image, ImageDraw, ImageFont


def gen_breathe(path):
    """48x48 呼吸圆点:蓝点半径+透明度正弦脉动,2s 周期,透明背景。"""
    n = 24
    frames = []
    for i in range(n):
        t = i / n
        k = (math.sin(2 * math.pi * t - math.pi / 2) + 1) / 2
        img = Image.new("RGBA", (48, 48), (0, 0, 0, 0))
        d = ImageDraw.Draw(img)
        r = 8 + 6 * k
        alpha = int(110 + 145 * k)
        d.ellipse([24 - r - 5, 24 - r - 5, 24 + r + 5, 24 + r + 5],
                  fill=(90, 150, 255, int(alpha * 0.25)))
        d.ellipse([24 - r, 24 - r, 24 + r, 24 + r], fill=(90, 150, 255, alpha))
        frames.append(img)
    frames[0].save(path, save_all=True, append_images=frames[1:],
                   duration=int(2000 / n), loop=0, disposal=2)


def gen_shimmer(path, text="Thinking"):
    """文字扫光:灰字 + 近白亮带左→右循环,1.56s 周期,只作用在字形内。"""
    font = ImageFont.truetype("/System/Library/Fonts/Helvetica.ttc", 30)
    pad = 6
    bbox = font.getbbox(text)
    w = bbox[2] - bbox[0] + pad * 2
    h = 40
    base = (150, 155, 163, 255)
    bright = (238, 242, 246, 255)

    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).text(
        (pad - bbox[0], (h - (bbox[3] - bbox[1])) // 2 - bbox[1]),
        text, font=font, fill=255)

    n = 24
    band_w = w * 0.55
    frames = []
    for i in range(n):
        t = i / n
        cx = -band_w + t * (w + 2 * band_w)
        grad = Image.new("RGBA", (w, h), base)
        gd = ImageDraw.Draw(grad)
        for x in range(w):
            d = abs(x - cx) / band_w
            if d < 1:
                k = (1 - d) ** 2
                c = tuple(int(base[j] + (bright[j] - base[j]) * k) for j in range(3)) + (255,)
                gd.line([(x, 0), (x, h)], fill=c)
        frame = Image.new("RGBA", (w, h), (0, 0, 0, 0))
        frame.paste(grad, (0, 0), mask)
        frames.append(frame)
    frames[0].save(path, save_all=True, append_images=frames[1:],
                   duration=65, loop=0, disposal=2)


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else "."
    gen_breathe(f"{out}/breathe.gif")
    gen_shimmer(f"{out}/shimmer.gif")
    print("ok:", f"{out}/breathe.gif", f"{out}/shimmer.gif")
