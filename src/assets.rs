use std::borrow::Cow;

use anyhow::Result;
use gpui::{App, AssetSource, SharedString};

/// Icons embedded in the binary so the app stays a single artifact.
pub struct Assets;

macro_rules! icons {
    ($($name:literal $(=> $source:literal)?),+ $(,)?) => {
        &[$((
            concat!("icons/", $name, ".svg"),
            icons!(@bytes $name $(=> $source)?).as_slice(),
        )),+]
    };
    (@bytes $name:literal => $source:literal) => {
        include_bytes!(concat!("../assets/icons/", $source, ".svg"))
    };
    (@bytes $name:literal) => {
        include_bytes!(concat!("../assets/icons/", $name, ".svg"))
    };
}

// Stable paths for controls, provider logos, file types, and specialized glyphs.
const ICONS: &[(&str, &[u8])] = icons![
    "alert" => "ant/warning",
    "alert-bold",
    "appearance" => "ant/palette",
    "arrow-down" => "ant/arrow-down",
    "arrow-left" => "ant/arrow-left",
    "arrow-right" => "ant/arrow-right",
    "arrow-up-right" => "ant/arrow-up-right",
    "arrow-up" => "ant/arrow-up",
    "block" => "ant/ban",
    "bot" => "ant/robot",
    "case-sensitive",
    "changes" => "ant/git-compare",
    "chart-column" => "ant/bar-chart",
    "check" => "ant/check",
    "check-bold",
    "check-bold-stroke",
    "chevron-down" => "ant/chevron-down",
    "chevron-right" => "ant/chevron-right",
    "chevron-up" => "ant/chevron-up",
    "chevrons-up-down" => "ant/chevrons-expand",
    "cloud-upload" => "ant/upload",
    "command" => "ant/command",
    "compose" => "ant/edit",
    "copy" => "ant/copy",
    "copy-bold",
    "corner-down-right" => "ant/indent",
    "download" => "ant/download",
    "ellipsis" => "ant/ellipsis",
    "external-link" => "ant/export",
    "eye-off" => "ant/eye-off",
    "eye" => "ant/eye",
    "eye-bold",
    "file-bottom-left-arrow" => "ant/file-up",
    "file-diff",
    "file-types/angular",
    "file-types/astro",
    "file-types/audio",
    "file-types/babel",
    "file-types/biome",
    "file-types/bun",
    "file-types/c",
    "file-types/certificate",
    "file-types/clojure",
    "file-types/cmake",
    "file-types/coffee",
    "file-types/console",
    "file-types/cpp",
    "file-types/crystal",
    "file-types/csharp",
    "file-types/css",
    "file-types/dart",
    "file-types/database",
    "file-types/deno",
    "file-types/diff",
    "file-types/docker",
    "file-types/editorconfig",
    "file-types/elixir",
    "file-types/elm",
    "file-types/erlang",
    "file-types/eslint",
    "file-types/exe",
    "file-types/file",
    "file-types/firebase",
    "file-types/git",
    "file-types/gitlab",
    "file-types/go",
    "file-types/gradle",
    "file-types/graphql",
    "file-types/haskell",
    "file-types/haxe",
    "file-types/helm",
    "file-types/html",
    "file-types/image",
    "file-types/java",
    "file-types/javascript",
    "file-types/jinja",
    "file-types/json",
    "file-types/julia",
    "file-types/kotlin",
    "file-types/kubernetes",
    "file-types/lock",
    "file-types/lua",
    "file-types/makefile",
    "file-types/markdown",
    "file-types/nest",
    "file-types/next",
    "file-types/nginx",
    "file-types/nix",
    "file-types/nodejs",
    "file-types/npm",
    "file-types/nuxt",
    "file-types/ocaml",
    "file-types/pdf",
    "file-types/perl",
    "file-types/php",
    "file-types/pnpm",
    "file-types/powershell",
    "file-types/prettier",
    "file-types/prisma",
    "file-types/proto",
    "file-types/pug",
    "file-types/python",
    "file-types/react",
    "file-types/readme",
    "file-types/rollup",
    "file-types/ruby",
    "file-types/rust",
    "file-types/sass",
    "file-types/scala",
    "file-types/settings",
    "file-types/solidity",
    "file-types/storybook",
    "file-types/stylelint",
    "file-types/supabase",
    "file-types/svelte",
    "file-types/svg",
    "file-types/swift",
    "file-types/tailwindcss",
    "file-types/terraform",
    "file-types/tex",
    "file-types/turborepo",
    "file-types/typescript",
    "file-types/video",
    "file-types/vite",
    "file-types/vitest",
    "file-types/vue",
    "file-types/webassembly",
    "file-types/webpack",
    "file-types/xaml",
    "file-types/xml",
    "file-types/yaml",
    "file-types/yarn",
    "file-types/zig",
    "file-types/zip",
    "file" => "ant/file",
    "filter" => "ant/filter",
    "folder-dashed",
    "folder-new" => "ant/folder-plus",
    "folder-open" => "ant/folder-open",
    "folder" => "ant/folder",
    "fork" => "ant/arrows-split",
    "gauge" => "ant/gauge",
    "git-branch" => "ant/git-branch",
    "git-commit-horizontal" => "ant/git-commit",
    "github" => "ant/github",
    "globe" => "ant/globe",
    "hexagon" => "ant/cube",
    "hourglass" => "ant/clock",
    "hourglass-bold",
    "info" => "ant/info-circle",
    "laptop" => "ant/laptop",
    "list" => "ant/list",
    "loader-circle" => "ant/loader",
    "loader-circle-bold",
    "lock-open" => "ant/unlock",
    "lock" => "ant/lock",
    "package" => "ant/cube",
    "panel-left" => "ant/panel-left",
    "panel-right" => "ant/panel-right",
    "paperclip" => "ant/paperclip",
    "pencil" => "ant/pencil",
    "pin" => "ant/pushpin",
    "play" => "ant/play",
    "play-bold",
    "plus" => "ant/plus",
    "provider-claude",
    "provider-openai",
    "provider-pi",
    "queue" => "ant/redo",
    "regex",
    "replace-all",
    "replace",
    "rewind" => "ant/rotate-ccw",
    "rotate-cw" => "ant/rotate-cw",
    "search" => "ant/search",
    "server" => "ant/server",
    "settings" => "ant/settings",
    "slash" => "ant/slash",
    "sparkle",
    "star-filled" => "ant/star-filled",
    "star" => "ant/star",
    "stop-filled" => "ant/square-filled",
    "stop" => "ant/square-filled",
    "target" => "ant/target",
    "terminal-square" => "ant/terminal-square",
    "terminal" => "ant/prompt",
    "trash" => "ant/trash",
    "whole-word",
    "window-maximize" => "ant/square-alt",
    "window-minimize" => "ant/minus",
    "window-restore" => "ant/copy-alt",
    "wrench" => "ant/wrench",
    "x" => "ant/x",
    "x-bold",
    "zap" => "ant/bolt",
];

// The complete supplied Ant pack is also available at `icons/ant/<name>.svg`.
const ANT_ICONS: &[(&str, &[u8])] = icons![
    "ant/accessibility",
    "ant/alarm-clock",
    "ant/alert-circle-filled",
    "ant/alert-circle",
    "ant/align-center",
    "ant/align-justify-alt",
    "ant/align-justify",
    "ant/align-left-alt-2",
    "ant/align-left-alt",
    "ant/align-right",
    "ant/android",
    "ant/apple",
    "ant/apps-plus",
    "ant/apps",
    "ant/archive",
    "ant/arrow-down-circle",
    "ant/arrow-down-left",
    "ant/arrow-down-right",
    "ant/arrow-down",
    "ant/arrow-left-circle",
    "ant/arrow-left",
    "ant/arrow-right-circle",
    "ant/arrow-right",
    "ant/arrow-turn-left",
    "ant/arrow-up-circle",
    "ant/arrow-up-left",
    "ant/arrow-up-right-alt",
    "ant/arrow-up-right",
    "ant/arrow-up",
    "ant/arrows-left-right",
    "ant/arrows-split",
    "ant/arrows-up-down",
    "ant/at-sign",
    "ant/atom",
    "ant/badge-check",
    "ant/ban",
    "ant/bandage",
    "ant/bank",
    "ant/bar-chart",
    "ant/bell-concierge",
    "ant/bell-off",
    "ant/bell-simple",
    "ant/bell",
    "ant/bird",
    "ant/blockquote",
    "ant/bold",
    "ant/bolt",
    "ant/book-open-alt",
    "ant/book-open",
    "ant/book",
    "ant/bookmark-filled",
    "ant/bookmark",
    "ant/books",
    "ant/braces",
    "ant/brain",
    "ant/briefcase",
    "ant/browser-cursor",
    "ant/browser",
    "ant/bug",
    "ant/building",
    "ant/calculator",
    "ant/calendar",
    "ant/camera",
    "ant/card-bar",
    "ant/card-block-center",
    "ant/card-block-left",
    "ant/card-block-right",
    "ant/card-block-top",
    "ant/card-header",
    "ant/card-list",
    "ant/card-minus",
    "ant/card-off",
    "ant/caret-down",
    "ant/caret-right",
    "ant/chart-square",
    "ant/check-circle-filled",
    "ant/check-circle",
    "ant/check-double",
    "ant/check-square",
    "ant/check",
    "ant/checklist-alt",
    "ant/checklist-filled",
    "ant/checklist",
    "ant/chevron-down-alt",
    "ant/chevron-down",
    "ant/chevron-left",
    "ant/chevron-right",
    "ant/chevron-up",
    "ant/chevrons-collapse",
    "ant/chevrons-expand",
    "ant/chevrons-left",
    "ant/chevrons-right",
    "ant/circle-dashed",
    "ant/circle-outline",
    "ant/circle",
    "ant/circles-merge",
    "ant/circles-overlap",
    "ant/clear-formatting",
    "ant/click",
    "ant/clipboard-export",
    "ant/clipboard-list",
    "ant/clipboard",
    "ant/clock-alert",
    "ant/clock",
    "ant/cloud-filled",
    "ant/cloud-off-filled",
    "ant/cloud-off",
    "ant/cloud",
    "ant/code-alt",
    "ant/code",
    "ant/coffee",
    "ant/collapse",
    "ant/columns-2",
    "ant/command",
    "ant/compress",
    "ant/copy-alt",
    "ant/copy",
    "ant/corner-bottom-left",
    "ant/corner-bottom-right",
    "ant/corner-top-left",
    "ant/corner-top-right",
    "ant/credit-card",
    "ant/crown",
    "ant/cube",
    "ant/cursor-filled",
    "ant/cursor-off",
    "ant/cursor",
    "ant/cut-off",
    "ant/cut",
    "ant/database",
    "ant/device",
    "ant/devices",
    "ant/diamond-arrow",
    "ant/diamond",
    "ant/dollar-sign",
    "ant/door-filled",
    "ant/door",
    "ant/download",
    "ant/droplet-filled",
    "ant/edit",
    "ant/ellipsis-circle",
    "ant/ellipsis-vertical",
    "ant/ellipsis",
    "ant/expand",
    "ant/export",
    "ant/eye-off",
    "ant/eye",
    "ant/fast-forward",
    "ant/file-plus",
    "ant/file-search",
    "ant/file-text",
    "ant/file-up",
    "ant/file",
    "ant/files",
    "ant/filter",
    "ant/flag",
    "ant/flask",
    "ant/flower-off",
    "ant/folder-filled",
    "ant/folder-open-filled",
    "ant/folder-open",
    "ant/folder-plus",
    "ant/folder-refresh",
    "ant/folder",
    "ant/footprints",
    "ant/gauge-alt",
    "ant/gauge",
    "ant/gavel",
    "ant/ghost-filled",
    "ant/ghost",
    "ant/gift",
    "ant/git-branch-x",
    "ant/git-branch",
    "ant/git-commit-alt",
    "ant/git-commit",
    "ant/git-compare",
    "ant/git-fork",
    "ant/git-merge-conflict",
    "ant/git-options",
    "ant/git-pull-request",
    "ant/github",
    "ant/globe",
    "ant/graduation-cap",
    "ant/grid",
    "ant/hand-heart",
    "ant/hand-pointer",
    "ant/hand-wave",
    "ant/hand",
    "ant/hash",
    "ant/heading-1",
    "ant/heading-2",
    "ant/heading-3",
    "ant/heart-pulse",
    "ant/heart",
    "ant/help-circle",
    "ant/highlighter",
    "ant/history",
    "ant/home",
    "ant/image",
    "ant/images",
    "ant/import",
    "ant/inbox",
    "ant/indent-text",
    "ant/indent",
    "ant/info-circle",
    "ant/info",
    "ant/italic",
    "ant/key",
    "ant/keyboard",
    "ant/laptop-alt",
    "ant/laptop-filled",
    "ant/laptop-off",
    "ant/laptop",
    "ant/layout-columns",
    "ant/layout-dashboard",
    "ant/leaf",
    "ant/lightbulb",
    "ant/line-chart",
    "ant/line-height",
    "ant/link-alt",
    "ant/link",
    "ant/list-ordered",
    "ant/list",
    "ant/loader",
    "ant/lock",
    "ant/logout",
    "ant/magnet-off",
    "ant/mail",
    "ant/map-pin",
    "ant/map",
    "ant/megaphone",
    "ant/menu-circle",
    "ant/message-circle-alt",
    "ant/message-circle-filled-alt",
    "ant/message-circle-filled",
    "ant/message-circle",
    "ant/message-plus-filled",
    "ant/message-plus",
    "ant/message-x-filled",
    "ant/message-x",
    "ant/messages-filled",
    "ant/messages",
    "ant/microphone-filled",
    "ant/microphone-off",
    "ant/microphone",
    "ant/minus",
    "ant/mobile-app",
    "ant/monitor-plus-alt",
    "ant/monitor-plus",
    "ant/monitor-up",
    "ant/monitor-x",
    "ant/monitor",
    "ant/moon-filled",
    "ant/moon-plus",
    "ant/moon",
    "ant/navigation",
    "ant/network",
    "ant/numbers",
    "ant/orbit",
    "ant/outdent-text",
    "ant/outdent",
    "ant/paint-bucket",
    "ant/paint-roller",
    "ant/paintbrush",
    "ant/palette-filled",
    "ant/palette",
    "ant/panel-left",
    "ant/panel-plus-left",
    "ant/panel-plus-right",
    "ant/panel-right",
    "ant/panel-x",
    "ant/paperclip",
    "ant/pause-filled",
    "ant/pause",
    "ant/pencil",
    "ant/percent",
    "ant/phone-book",
    "ant/phone-call",
    "ant/phone-ringing",
    "ant/phone",
    "ant/pip-bottom-left",
    "ant/pip-bottom-right",
    "ant/pip-top-left",
    "ant/pip-top-right",
    "ant/play-circle-filled",
    "ant/play-circle",
    "ant/play-filled",
    "ant/play",
    "ant/plug",
    "ant/plus-circle",
    "ant/plus-large",
    "ant/plus-square-filled",
    "ant/plus-square",
    "ant/plus",
    "ant/presentation",
    "ant/printer-alt",
    "ant/printer-filled",
    "ant/printer-x",
    "ant/printer",
    "ant/prompt",
    "ant/pushpin-filled",
    "ant/pushpin-off",
    "ant/pushpin",
    "ant/redo",
    "ant/refresh-square",
    "ant/refresh",
    "ant/reply",
    "ant/resize-horizontal",
    "ant/robot-face",
    "ant/robot",
    "ant/rotate-ccw",
    "ant/rotate-cw",
    "ant/route",
    "ant/scale",
    "ant/scan-eye",
    "ant/scan",
    "ant/scissors",
    "ant/search-circle",
    "ant/search-code",
    "ant/search-frame",
    "ant/search",
    "ant/select",
    "ant/send-help",
    "ant/send-off",
    "ant/send",
    "ant/server",
    "ant/settings",
    "ant/shapes",
    "ant/share",
    "ant/shield-alert",
    "ant/shield-check",
    "ant/shield-lock",
    "ant/shield-search",
    "ant/shuffle",
    "ant/sidebar-left",
    "ant/sidebar-right",
    "ant/sigma",
    "ant/signpost",
    "ant/sitemap",
    "ant/slash",
    "ant/sliders-alt",
    "ant/sliders",
    "ant/smiley-plus",
    "ant/smiley",
    "ant/snowflake",
    "ant/socket",
    "ant/spiral",
    "ant/square-alt",
    "ant/square-filled",
    "ant/square-slash",
    "ant/square",
    "ant/stairs",
    "ant/star-filled",
    "ant/star-off",
    "ant/star",
    "ant/sticky-note",
    "ant/stop-circle",
    "ant/stopwatch",
    "ant/strikethrough",
    "ant/subscript",
    "ant/sun-filled",
    "ant/sun",
    "ant/sunrise",
    "ant/sunset",
    "ant/superscript",
    "ant/table-alt",
    "ant/table-header",
    "ant/table-merged",
    "ant/table",
    "ant/tag",
    "ant/target",
    "ant/terminal-square-filled",
    "ant/terminal-square",
    "ant/text-center",
    "ant/text-left",
    "ant/text-right",
    "ant/text-select",
    "ant/text-size",
    "ant/text",
    "ant/thermometer-filled",
    "ant/thermometer",
    "ant/thumbs-down-filled",
    "ant/thumbs-down",
    "ant/thumbs-up-filled",
    "ant/thumbs-up",
    "ant/ticket",
    "ant/timer",
    "ant/toggles",
    "ant/trash",
    "ant/underline",
    "ant/undo",
    "ant/unlock",
    "ant/upload-line",
    "ant/upload",
    "ant/user-circle",
    "ant/user",
    "ant/users-alt",
    "ant/users",
    "ant/video",
    "ant/virus",
    "ant/volume",
    "ant/wallet",
    "ant/wand",
    "ant/warning-filled",
    "ant/warning",
    "ant/waveform",
    "ant/workflow",
    "ant/wrench-filled",
    "ant/wrench",
    "ant/x-circle",
    "ant/x",
    "ant/zoom-1",
    "ant/zoom-in",
    "ant/zoom-out",
];

const TEXT_FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Italic.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-BoldItalic.ttf"),
    // Optical-size cut for inline code; see `md::render::INLINE_MONO_FAMILY`
    // and `scripts/derive-inline-mono.py`.
    include_bytes!("../assets/fonts/JetBrainsMonoSmall-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMonoSmall-Italic.ttf"),
];

/// Symbols-only icon face resolved via CoreText cascade (`FontFallbacks`),
/// never as a primary GPUI family; see `register_fonts_with_coretext`.
const SYMBOLS_FONT: &[u8] = include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf");

/// Family name of [`SYMBOLS_FONT`] for `FontFallbacks` lists.
pub const SYMBOLS_FONT_FAMILY: &str = "Symbols Nerd Font Mono";

pub fn register_fonts(cx: &App) -> Result<()> {
    cx.text_system().add_fonts(
        TEXT_FONTS
            .iter()
            .map(|font| Cow::Borrowed(*font))
            .collect::<Vec<_>>(),
    )?;
    crate::platform::register_fonts_with_coretext(&[SYMBOLS_FONT])
}

/// Target rendered weight for a supplied outline glyph. The pack's own
/// 59-unit outline is 1.18px at its authored 20px size, so small controls
/// get only enough stroke to reach this floor and larger glyphs stay as
/// supplied.
const NORMAL_ICON_WEIGHT: f32 = 1.2;
/// Outline thickness and view box of the supplied pack.
const PACK_OUTLINE_UNITS: f32 = 59.0;
const PACK_VIEW_BOX: f32 = 1000.0;

/// Returns the asset path for `path` at `size`, which asks [`Assets::load`]
/// for the size-specific stroke. Paths are cached because row builders call
/// this for every visible icon on every frame.
pub(crate) fn sized_icon_path(path: &'static str, size: f32) -> SharedString {
    use std::collections::HashMap;
    use std::sync::{LazyLock, RwLock};

    static PATHS: LazyLock<RwLock<HashMap<(&'static str, u32), SharedString>>> =
        LazyLock::new(|| RwLock::new(HashMap::new()));

    // Half-pixel buckets keep the cache small without visibly changing the
    // stroke; sizes are authored in 0.5px steps.
    let half_px = (size * 2.0).round() as u32;
    if let Some(cached) = PATHS
        .read()
        .expect("icon path cache poisoned")
        .get(&(path, half_px))
    {
        return cached.clone();
    }
    let sized = SharedString::from(format!("{path}#{half_px}"));
    PATHS
        .write()
        .expect("icon path cache poisoned")
        .insert((path, half_px), sized.clone());
    sized
}

fn split_sized_path(path: &str) -> (&str, Option<f32>) {
    match path.rsplit_once('#') {
        Some((base, half_px)) => match half_px.parse::<u32>() {
            Ok(half_px) => (base, Some(half_px as f32 / 2.0)),
            Err(_) => (path, None),
        },
        None => (path, None),
    }
}

/// Rounds a stroke width to whole view-box units.
fn stroke_units(value: f32) -> String {
    format!("{value:.0}")
}

/// Inserts the expanding stroke into the root `<svg>` tag so every unstroked
/// path inherits it.
fn inject_stroke(svg: &str, width: &str) -> Option<Vec<u8>> {
    let open_end = svg.find('>')?;
    if !svg[..open_end].contains("<svg") {
        return None;
    }
    let mut stroked = String::with_capacity(svg.len() + 96);
    stroked.push_str(&svg[..open_end]);
    stroked.push_str(" stroke=\"currentColor\" stroke-width=\"");
    stroked.push_str(width);
    stroked.push_str("\" stroke-linejoin=\"round\" stroke-linecap=\"round\"");
    stroked.push_str(&svg[open_end..]);
    Some(stroked.into_bytes())
}

/// Applies the size-specific normal stroke to an outline glyph. Artwork that
/// already carries its own weight keeps the authored bytes: the hand-tuned
/// `-bold` derivatives and Lucide glyphs declare a stroke width, filled marks
/// and provider logos are solid shapes, and file-type icons are polychrome.
/// `loader-circle-bold` is a redrawn ring rather than a stroked path, so it
/// stays with its authored geometry too.
fn stroked_svg(path: &str, bytes: &'static [u8], size: f32) -> Option<Vec<u8>> {
    if !path.starts_with("icons/")
        || path.starts_with("icons/file-types/")
        || path.starts_with("icons/provider-")
        || path.contains("-filled")
        || path == "icons/loader-circle-bold.svg"
    {
        return None;
    }
    let svg = std::str::from_utf8(bytes).ok()?;
    let total = NORMAL_ICON_WEIGHT * PACK_VIEW_BOX / size;
    let expansion = total - PACK_OUTLINE_UNITS;
    // At 20px and above the supplied weight is already normal.
    if expansion < 2.0 {
        return None;
    }
    if path == "icons/file-diff.svg" {
        // The box is a filled 59-unit outline while the plus and minus are
        // stroked paths, so each needs the full target weight rather than
        // the expanding remainder.
        let svg = svg.replace(
            "stroke-width=\"59\"",
            &format!("stroke-width=\"{}\"", stroke_units(total)),
        );
        return inject_stroke(&svg, &stroke_units(expansion));
    }
    if svg.contains("stroke-width") {
        return None;
    }
    inject_stroke(svg, &stroke_units(expansion))
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let (base, size) = split_sized_path(path);
        Ok(ICONS
            .iter()
            .chain(ANT_ICONS)
            .find(|(name, _)| *name == base)
            .map(
                |(_, bytes)| match size.and_then(|size| stroked_svg(base, bytes, size)) {
                    Some(stroked) => Cow::Owned(stroked),
                    None => Cow::Borrowed(*bytes),
                },
            ))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .chain(ANT_ICONS)
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AssetSource;

    fn authored(path: &str) -> &'static [u8] {
        ICONS
            .iter()
            .chain(ANT_ICONS)
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| *bytes)
            .unwrap_or_else(|| panic!("missing embedded icon: {path}"))
    }

    #[test]
    fn small_glyphs_load_with_the_normal_stroke() {
        let loaded = Assets.load("icons/copy.svg#28").unwrap().unwrap();
        let svg = std::str::from_utf8(&loaded).unwrap();
        // 14px: target 1.2px * 1000 / 14 = 85.7 units, minus 59 = 26.7.
        assert!(svg.contains("stroke-width=\"27\""), "{svg}");
        assert_eq!(svg.matches("stroke-width=").count(), 1);

        let loaded = Assets.load("icons/ant/panel-left.svg#24").unwrap().unwrap();
        let svg = std::str::from_utf8(&loaded).unwrap();
        // 12px: target 1.2px * 1000 / 12 = 100 units, minus 59 = 41.
        assert!(svg.contains("stroke-width=\"41\""), "{svg}");
    }

    #[test]
    fn large_glyphs_keep_their_supplied_weight() {
        for path in ["icons/copy.svg#40", "icons/ant/panel-left.svg#40"] {
            let loaded = Assets.load(path).unwrap().unwrap();
            assert_eq!(
                loaded.as_ref(),
                authored(path.split('#').next().unwrap()),
                "{path}"
            );
        }
    }

    #[test]
    fn file_diff_matches_the_normal_weight_at_small_sizes() {
        let loaded = Assets.load("icons/file-diff.svg#24").unwrap().unwrap();
        let svg = std::str::from_utf8(&loaded).unwrap();
        // 12px: the box keeps its 59-unit fill plus a 41-unit expanding
        // stroke, and the stroked plus and minus use the 100-unit total.
        assert!(svg.contains("stroke-width=\"41\""), "{svg}");
        assert!(svg.contains("stroke-width=\"100\""), "{svg}");
    }

    #[test]
    fn weighted_and_polychrome_artwork_keeps_its_bytes() {
        for path in [
            "icons/copy-bold.svg#28",
            "icons/loader-circle-bold.svg#24",
            "icons/sparkle.svg#40",
            "icons/provider-claude.svg#40",
            "icons/file-types/rust.svg#32",
            "icons/star-filled.svg#24",
        ] {
            let loaded = Assets.load(path).unwrap().unwrap();
            assert_eq!(
                loaded.as_ref(),
                authored(path.split('#').next().unwrap()),
                "{path}"
            );
        }
    }
}
