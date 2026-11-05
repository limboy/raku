# UI icons

SVGs marked `lucide-static v1.69.0` come from
[`lucide-static` 1.69.0](https://www.npmjs.com/package/lucide-static/v/1.69.0),
matching [Lucide 1.69.0](https://github.com/lucide-icons/lucide/releases/tag/1.69.0).
Their upstream ISC and Feather MIT notices are in [LICENSE-lucide.txt](LICENSE-lucide.txt).

The SVGs are compacted, with CSS classes and fixed dimensions removed. Their
existing stroke colors are retained for GPUI's tinted rendering. These local
filenames map to different upstream names:

| Waku asset | Lucide icon | Adjustment |
| --- | --- | --- |
| `alert.svg` | `triangle-alert` | None |
| `rewind.svg` | `rotate-ccw` | None |
| `star-filled.svg` | `star` | Black fill for the selected state |

To refresh, replace the marked SVGs from the next pinned `lucide-static`
release, preserving these aliases, color adjustments, and dimension removal.
Copy its complete license and update the version in this file and the SVG
license comments. The website uses `lucide-react` separately in
`website/package.json` and the workspace `bun.lock`.

The unmarked assets include custom glyphs, platform symbols, provider logos,
and Tabler icons. `github.svg` is retained from an older Lucide release; current
Lucide releases no longer include brand icons. The custom `sparkle.svg` keeps
Waku's six-spoke glyph. File-type icons have their own
[source notes](file-types/SOURCE.md).
