# UI icons

The desktop UI uses the 413 SVGs in [ant/](ant/), supplied as the `ant-icons`
attachment on 2026-11-05. The files on disk are copied unchanged, including
their `1000 × 1000` view boxes, outlined geometry, and `currentColor` fills.
The pack's 59-unit outlines are authored for a 20px glyph, where they read as
a 1.18px stroke; at 14px the same geometry is 0.83px and at 11px 0.65px.
`ui::icon` therefore requests a size-qualified asset path, and `Assets::load`
expands every supplied outline glyph with a `currentColor` stroke calibrated
to hold the rendered weight near 1.2px: about 27 units at 14px, 41 at 12px,
and none at 20px or above. GPUI renders the result as tinted alpha masks.
Glyphs that already declare a stroke (the `-bold` derivatives and Lucide
glyphs), filled marks, provider logos, and polychrome file-type icons are
served unchanged. Titlebar controls use the pack's authored 20px size and the
primary text color.

The complete pack is embedded in the binary and available at
`icons/ant/<name>.svg`. The `ICONS` table in [src/assets.rs](../../src/assets.rs)
maps existing control paths to these assets through the shared renderer and
interaction handlers. For example, `alert` uses `ant/warning`, `compose`
uses `ant/edit`, `terminal` uses `ant/prompt`, and `loader-circle` uses
`ant/loader`. These aliases reuse the supplied bytes without duplicate SVG
files and inherit the loader's normal stroke.

Seven specialized glyphs remain because the pack has no direct equivalents:
the five search controls (`case-sensitive`, `whole-word`, `regex`, `replace`,
and `replace-all`), the projectless `folder-dashed`, and Raku's six-spoke
`sparkle`. The dashed folder uses a `1.5`-unit stroke on its 16-unit box so
the dashes stay legible at 11px. Four Raku derivatives extend the pack so the
session status icons
hold up at a 12px row: `loader-circle-bold` redraws `ant/loader` at the same
`398`-unit outer radius and 270° arc with a `96`-unit ring instead of `58`,
and `alert-bold`, `hourglass-bold`, and `x-bold` reuse the supplied
`ant/warning`, `ant/clock`, and `ant/x` paths with a `38`-unit
`currentColor` stroke that expands every outline edge by roughly 19 units per
side. Four more derivatives serve the code-block header controls: `copy-bold`,
`play-bold`, `check-bold`, and `eye-bold` crop the pack's generous view-box
padding to the glyph bounds and add a 16–26-unit stroke calibrated to the same
~1.5px weight at the button's 15px glyph size. `check-bold-stroke` keeps the
`1000`-unit box and uses the same `38`-unit expanding stroke as the session
glyphs, for the project picker's selected-project check. The search glyphs marked
`lucide-static v1.69.0` come from
[`lucide-static` 1.69.0](https://www.npmjs.com/package/lucide-static/v/1.69.0);
their ISC and Feather MIT notices remain in [LICENSE-lucide.txt](LICENSE-lucide.txt).

The custom `file-diff.svg` reuses the Ant `square-alt` outline with a rounded
plus and minus. Its filled box receives the same expanding stroke as the pack,
and its stroked plus and minus are set to the resulting total weight, so the
boxed changes glyph matches its neighbours at both 12px rows and 20px
summaries. The environment summary uses Ant's `card-list` glyph.

Provider logos and polychrome file-type icons retain their own artwork.
File-type icons have separate [source notes](file-types/SOURCE.md).
The website uses `lucide-react` separately in `website/package.json` and the
workspace `bun.lock`.

To refresh the desktop set, replace the supplied SVGs in `ant/`, update
`ANT_ICONS` to include every filename, and review the control aliases in
`ICONS`. Keep these tables sorted by asset path.
