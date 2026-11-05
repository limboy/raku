# UI icons

The desktop UI uses the 413 SVGs in [ant/](ant/), supplied as the `ant-icons`
attachment on 2026-11-05. The supplied files are copied unchanged, including
their `1000 × 1000` view boxes, outlined geometry, and `currentColor` fills.
GPUI renders them as tinted alpha masks through the existing `ui::icon` helper.
Titlebar controls use the pack's authored 20px size and the primary text color,
matching the supplied toolbar reference in both light and dark themes.

The complete pack is embedded in the binary and available at
`icons/ant/<name>.svg`. The `ICONS` table in [src/assets.rs](../../src/assets.rs)
maps existing control paths to these assets through the shared renderer and
interaction handlers. For example, `alert` uses `ant/warning`, `compose`
uses `ant/edit`, `terminal` uses `ant/prompt`, and `loader-circle` uses
`ant/loader`. These aliases reuse the supplied bytes without duplicate SVG files.

Seven specialized glyphs remain because the pack has no direct equivalents:
the five search controls (`case-sensitive`, `whole-word`, `regex`, `replace`,
and `replace-all`), the projectless `folder-dashed`, and Waku's six-spoke
`sparkle`. The search glyphs marked `lucide-static v1.69.0` come from
[`lucide-static` 1.69.0](https://www.npmjs.com/package/lucide-static/v/1.69.0);
their ISC and Feather MIT notices remain in [LICENSE-lucide.txt](LICENSE-lucide.txt).

The custom `file-diff.svg` reuses the Ant `square-alt` outline with a rounded
plus and minus at the same 59-unit weight, matching the boxed changes glyph in
the toolbar reference. The environment summary uses Ant's `card-list` glyph.

Provider logos and polychrome file-type icons retain their own artwork.
File-type icons have separate [source notes](file-types/SOURCE.md).
The website uses `lucide-react` separately in `website/package.json` and the
workspace `bun.lock`.

To refresh the desktop set, replace the supplied SVGs in `ant/`, update
`ANT_ICONS` to include every filename, and review the control aliases in
`ICONS`. Keep these tables sorted by asset path.
