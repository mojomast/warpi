#!/usr/bin/env python3
"""Compose the warpi Inno Setup wizard images from the fork's checked-in brand assets.

Sizes follow the Inno Setup 6.7 "modern" wizard image areas for the post-6.6.0
defaults (100% = 202x386 large / 58x58 small; 200% = 430x824 / 124x124).
See https://jrsoftware.org/ishelp/topic_setup_wizardimagefile.htm
"""
import os
import sys

from PIL import Image

WORDMARK = "app/assets/bundled/png/warpi-wordmark.png"
ICON = "app/channels/warpi/icon/no-padding/512x512.png"
TILE = (0x0B, 0x0D, 0x10, 0xFF)  # #0B0D10, same dark tile as AboutPage on light themes


def load_trimmed(path):
    im = Image.open(path).convert("RGBA")
    return im.crop(im.getchannel("A").getbbox())


def fit(im, max_w, max_h):
    scale = min(max_w / im.width, max_h / im.height)
    return im.resize((max(1, round(im.width * scale)), max(1, round(im.height * scale))), Image.LANCZOS)


def make_banner(w, h, out):
    canvas = Image.new("RGBA", (w, h), TILE)
    icon = fit(load_trimmed(ICON), round(w * 0.44), round(h * 0.30))
    word = fit(load_trimmed(WORDMARK), round(w * 0.84), round(h * 0.30))
    gap = round(h * 0.075)
    block_h = icon.height + gap + word.height
    y = (h - block_h) // 2
    canvas.alpha_composite(icon, ((w - icon.width) // 2, y))
    canvas.alpha_composite(word, ((w - word.width) // 2, y + icon.height + gap))
    canvas.save(out)
    print(f"{out}: {canvas.size[0]}x{canvas.size[1]} (icon {icon.size[0]}x{icon.size[1]}, word {word.size[0]}x{word.size[1]})")


def make_small(size, out):
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    margin = round(size * 0.05)
    icon = fit(load_trimmed(ICON), size - 2 * margin, size - 2 * margin)
    canvas.alpha_composite(icon, ((size - icon.width) // 2, (size - icon.height) // 2))
    canvas.save(out)
    print(f"{out}: {canvas.size[0]}x{canvas.size[1]} (glyph {icon.size[0]}x{icon.size[1]})")


if __name__ == "__main__":
    repo = sys.argv[1] if len(sys.argv) > 1 else "."
    out_dir = sys.argv[2] if len(sys.argv) > 2 else os.path.join(repo, "script/windows/installer-images")
    os.chdir(repo)
    make_banner(202, 386, os.path.join(out_dir, "warpi-banner.png"))
    make_banner(430, 824, os.path.join(out_dir, "warpi-banner-2x.png"))
    make_small(58, os.path.join(out_dir, "warpi-logo.png"))
    make_small(124, os.path.join(out_dir, "warpi-logo-2x.png"))
