#!/usr/bin/env python3
"""Derive the optical-size mono cut used for inline code.

A GPUI paragraph is shaped at a single font size, so inline `code` cannot be
set smaller than the prose around it. This script bakes the smaller size into
the font instead: every outline and advance is drawn at 90% inside an unchanged
em box, while the ascender/descender stay exactly as they are. Inline code then
renders at the code scale, on the prose baseline, without changing how any
paragraph is laid out.

The result is `assets/fonts/JetBrainsMonoSmall-{Regular,Italic}.ttf`, family
"JetBrains Mono Small". Regenerate only when the bundled JetBrains Mono faces
change; the source faces are the ones already in `assets/fonts`.

Usage: python3 scripts/derive-inline-mono.py [--scale 0.9]
Requires fonttools (`pip install fonttools`).
"""

from __future__ import annotations

import argparse
from pathlib import Path

from fontTools.misc.transform import Transform
from fontTools.pens.transformPen import TransformPen
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import TTFont

ROOT = Path(__file__).resolve().parent.parent
FONT_DIR = ROOT / "assets" / "fonts"
FAMILY = "JetBrains Mono Small"
FACES = [
    # (source, style, output)
    ("JetBrainsMono-Regular.ttf", "Regular", "JetBrainsMonoSmall-Regular.ttf"),
    ("JetBrainsMono-Italic.ttf", "Italic", "JetBrainsMonoSmall-Italic.ttf"),
]


def scale_glyphs(font: TTFont, scale: float) -> dict:
    glyf = font["glyf"]
    glyph_set = font.getGlyphSet()
    scaled = {}
    for name in font.getGlyphOrder():
        glyph = glyf[name]
        if glyph.numberOfContours == 0:
            scaled[name] = glyph
            continue
        pen = TTGlyphPen(glyph_set)
        glyph.draw(TransformPen(pen, Transform(scale, 0, 0, scale, 0, 0)), glyf)
        scaled[name] = pen.glyph()
    for name, glyph in scaled.items():
        glyf[name] = glyph
        glyph.recalcBounds(glyf)
    return scaled


def scale_horizontal_metrics(font: TTFont, glyphs: dict, scale: float) -> None:
    hmtx = font["hmtx"]
    for name, glyph in glyphs.items():
        advance, _ = hmtx.metrics[name]
        metrics = (round(advance * scale), glyph.xMin if glyph.numberOfContours else 0)
        hmtx.metrics[name] = metrics

    hhea = font["hhea"]
    drawn = [glyph for glyph in glyphs.values() if glyph.numberOfContours]
    hhea.advanceWidthMax = max(advance for advance, _ in hmtx.metrics.values())
    hhea.minLeftSideBearing = min(lsb for _, lsb in hmtx.metrics.values())
    hhea.minRightSideBearing = min(
        advance - lsb - (glyph.xMax - glyph.xMin)
        for name, (advance, lsb) in hmtx.metrics.items()
        for glyph in [glyphs[name]]
        if glyph.numberOfContours
    )
    hhea.xMaxExtent = max(
        lsb + (glyph.xMax - glyph.xMin)
        for name, (_, lsb) in hmtx.metrics.items()
        for glyph in [glyphs[name]]
        if glyph.numberOfContours
    )

    head = font["head"]
    head.xMin = min(glyph.xMin for glyph in drawn)
    head.yMin = min(glyph.yMin for glyph in drawn)
    head.xMax = max(glyph.xMax for glyph in drawn)
    head.yMax = max(glyph.yMax for glyph in drawn)


def scale_typography(font: TTFont, scale: float) -> None:
    os2 = font["OS/2"]
    # Ascender/descender and the win metrics stay untouched: the run has to keep
    # sitting on the prose baseline, and the line box is measured from them.
    for field in (
        "sxHeight",
        "sCapHeight",
        "yStrikeoutSize",
        "yStrikeoutPosition",
        "ySubscriptXSize",
        "ySubscriptYSize",
        "ySubscriptYOffset",
        "ySuperscriptXSize",
        "ySuperscriptYSize",
        "ySuperscriptYOffset",
    ):
        setattr(os2, field, round(getattr(os2, field) * scale))


def rename(font: TTFont, style: str) -> None:
    full = FAMILY if style == "Regular" else f"{FAMILY} {style}"
    postscript = f"JetBrainsMonoSmall-{style}"
    for record in font["name"].names:
        if record.nameID in (1, 16):
            record.string = FAMILY
        elif record.nameID in (2, 17):
            record.string = style
        elif record.nameID == 4:
            record.string = full
        elif record.nameID == 6:
            record.string = postscript


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--scale",
        type=float,
        default=0.9,
        help="glyph scale inside the em box (default: 0.9, a notch under the "
        "prose size)",
    )
    args = parser.parse_args()

    for source, style, output in FACES:
        font = TTFont(FONT_DIR / source)
        glyphs = scale_glyphs(font, args.scale)
        scale_horizontal_metrics(font, glyphs, args.scale)
        scale_typography(font, args.scale)
        rename(font, style)
        # Keep the source build stamp so regenerating is byte-for-byte repeatable.
        font.recalcTimestamp = False
        font.save(FONT_DIR / output)
        print(f"wrote assets/fonts/{output}")


if __name__ == "__main__":
    main()
