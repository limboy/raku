use gpui::{
    App, Font, FontWeight, Global, Hsla, PlatformTextSystem, Rems, SharedString, Window,
    WindowAppearance, hsla, rems, rgb,
};

use std::sync::Arc;

pub use raku_client::theme::ThemePreference;

/// Standard size for compact interface controls, descriptions, and metadata.
pub const SMALL_UI_FONT_SIZE: f32 = 13.0;

/// Shared default for interface text, prose, and code; emphasis can be heavier.
pub const DEFAULT_FONT_WEIGHT: FontWeight = FontWeight::NORMAL;

/// Fonts shaped outside the element tree must use the same default as the UI.
pub fn default_font(family: impl Into<SharedString>) -> Font {
    Font {
        weight: DEFAULT_FONT_WEIGHT,
        ..gpui::font(family)
    }
}

/// The platform inventory excludes GPUI's synthetic fallback family names.
/// Keep this same source as the app's text system, including bundled fonts.
#[derive(Clone)]
pub(crate) struct FontSource(pub Arc<dyn PlatformTextSystem>);

impl Global for FontSource {}

/// Shared family names keep font reads allocation-free on rendering paths.
#[derive(Clone)]
pub struct Fonts {
    pub interface: SharedString,
    pub transcript: SharedString,
}

impl Global for Fonts {}

impl Fonts {
    /// Called at startup and on explicit font changes, never while rendering.
    pub fn new(interface: Option<&str>, transcript: Option<&str>, cx: &App) -> Self {
        let source = cx.try_global::<FontSource>();
        let family = |name: Option<&str>| {
            resolve_font_family(name, |name| {
                source.is_none_or(|source| source.0.font_id(&default_font(name.to_owned())).is_ok())
            })
        };
        Self {
            interface: family(interface),
            transcript: family(transcript),
        }
    }

    pub fn current(cx: &App) -> Self {
        cx.try_global::<Self>()
            .cloned()
            .unwrap_or_else(|| Self::new(None, None, cx))
    }
}

fn resolve_font_family(
    name: Option<&str>,
    is_available: impl FnOnce(&str) -> bool,
) -> SharedString {
    name.filter(|name| !name.trim().is_empty())
        .filter(|name| is_available(name))
        .map(|name| SharedString::from(name.to_owned()))
        // Resolve the system family directly so GPUI keeps the requested weight
        // and style; its emergency fallback stack contains only regular fonts.
        .unwrap_or_else(|| ".SystemUIFont".into())
}

/// Scaled pixels: a dimension authored at the fixed 14px chrome size,
/// expressed in rems. The window's rem size is that same 14px, so this
/// resolves to exactly the authored pixel value.
///
/// Chrome text sizes and their line heights go through here. Content surfaces
/// that derive from a font-size setting — transcript markdown, the file
/// editor, diff rows, tool-output mono — stay in `px`.
pub fn sp(value: f32) -> Rems {
    rems(value / raku_client::persistence::DEFAULT_UI_FONT_SIZE)
}

fn resolves_to_dark(preference: ThemePreference, system_appearance: WindowAppearance) -> bool {
    match preference {
        ThemePreference::System => matches!(
            system_appearance,
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ),
        ThemePreference::Light => false,
        ThemePreference::Dark => true,
    }
}

fn native_override(preference: ThemePreference) -> Option<bool> {
    match preference {
        ThemePreference::System => None,
        ThemePreference::Light => Some(false),
        ThemePreference::Dark => Some(true),
    }
}

/// Raku's visual language, take two: neutral graphite surfaces in the spirit
/// of Cursor — color is reserved for meaning. Every surface is opaque, the
/// sidebar included, so the window never asks the compositor for vibrancy.
/// Light mode is a white canvas over a cool gray sidebar, with
/// selected sidebar rows as a ~10% neutral layer (dark mode stays at 6%).
#[derive(Clone, Copy)]
pub struct Theme {
    pub is_dark: bool,
    pub canvas: Hsla,
    pub sidebar: Hsla,
    pub sidebar_item_background: Hsla,
    /// Hovered sidebar row: a lighter step of `sidebar_item_background` so
    /// the selected row still reads as distinct.
    pub sidebar_item_hover: Hsla,
    pub surface: Hsla,
    pub raised: Hsla,
    /// Context and dropdown menu cards.
    pub menu: Hsla,
    pub composer: Hsla,
    pub inset: Hsla,
    /// Terminal screen surface: paper-white in light mode, near-black in dark.
    pub terminal: Hsla,
    pub overlay: Hsla,
    pub overlay_strong: Hsla,

    pub border: Hsla,
    pub border_strong: Hsla,
    pub sidebar_border: Hsla,

    pub text: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,
    pub text_ghost: Hsla,

    /// Brand coral. Logo, caret, live-activity pulses — nothing structural.
    pub accent: Hsla,
    /// Meter fills in the usage panel. Quota-meter blue by convention;
    /// warning/danger take over as a lane fills.
    pub gauge: Hsla,

    /// Text-selection wash. Painted *under* the glyphs, so it stays
    /// translucent and deliberately reads as the familiar browser blue rather
    /// than as brand color.
    pub selection: Hsla,
    /// Inline `code` foreground and its rounded wash.
    pub code_wash: Hsla,

    /// Light fill for primary buttons (send, allow), dark glyph on top.
    pub inverse: Hsla,
    pub on_inverse: Hsla,

    pub warning: Hsla,
    pub success: Hsla,
    pub favorite: Hsla,
    pub danger: Hsla,
    pub danger_soft: Hsla,
}

impl Theme {
    pub fn current(cx: &App) -> Self {
        if cx.has_global::<ActiveRakuTheme>() {
            cx.global::<ActiveRakuTheme>().0
        } else {
            Self::dark()
        }
    }

    pub fn dark() -> Self {
        Self {
            is_dark: true,
            canvas: rgb(0x1A1A1A).into(),
            sidebar: rgb(0x181818).into(),
            sidebar_item_background: hsla(0.0, 0.0, 0.941, 0.06),
            sidebar_item_hover: hsla(0.0, 0.0, 0.941, 0.035),
            surface: rgb(0x1A1A1A).into(),
            raised: rgb(0x232323).into(),
            menu: rgb(0x232323).into(),
            composer: rgb(0x212121).into(),
            inset: rgb(0x151515).into(),
            terminal: rgb(0x151515).into(),
            overlay: hsla(220.0 / 360.0, 0.10, 0.90, 0.05),
            overlay_strong: hsla(220.0 / 360.0, 0.10, 0.90, 0.09),

            border: hsla(220.0 / 360.0, 0.10, 0.90, 0.07),
            border_strong: hsla(220.0 / 360.0, 0.10, 0.90, 0.14),
            sidebar_border: hsla(126.93 / 360.0, 0.000_000_1, 0.16077, 1.0),

            text: rgb(0xE2E2E2).into(),
            text_secondary: rgb(0xA3A3A3).into(),
            text_tertiary: rgb(0x7D7D7D).into(),
            text_ghost: rgb(0x575757).into(),

            accent: rgb(0xE2795B).into(),
            gauge: rgb(0x3B82F6).into(),

            selection: hsla(211.0 / 360.0, 1.0, 0.50, 0.55),
            code_wash: hsla(220.0 / 360.0, 0.10, 0.90, 0.08),

            inverse: rgb(0xE7E9EC).into(),
            on_inverse: rgb(0x17181C).into(),

            warning: rgb(0xE0B36A).into(),
            success: rgb(0x62C987).into(),
            favorite: rgb(0xEAB308).into(),
            danger: rgb(0xE2726A).into(),
            danger_soft: hsla(4.0 / 360.0, 0.55, 0.63, 0.10),
        }
    }

    pub fn light() -> Self {
        Self {
            is_dark: false,
            canvas: rgb(0xFFFFFF).into(),
            sidebar: rgb(0xEBECED).into(),
            sidebar_item_background: hsla(0.0, 0.0, 0.0, 0.10),
            sidebar_item_hover: hsla(0.0, 0.0, 0.0, 0.055),
            surface: rgb(0xFFFFFF).into(),
            raised: rgb(0xF3F3F3).into(),
            menu: rgb(0xFEFEFE).into(),
            composer: rgb(0xFFFFFF).into(),
            inset: rgb(0xE5E5E5).into(),
            terminal: rgb(0xFFFFFF).into(),
            overlay: hsla(220.0 / 360.0, 0.10, 0.12, 0.05),
            overlay_strong: hsla(220.0 / 360.0, 0.10, 0.12, 0.09),

            border: hsla(220.0 / 360.0, 0.10, 0.12, 0.08),
            border_strong: hsla(220.0 / 360.0, 0.10, 0.12, 0.15),
            sidebar_border: hsla(0.0, 0.0, 0.078, 0.12),

            text: rgb(0x141414).into(),
            text_secondary: rgb(0x666666).into(),
            text_tertiary: rgb(0x858585).into(),
            text_ghost: rgb(0xA4A4A4).into(),

            accent: rgb(0xC85F44).into(),
            gauge: rgb(0x2563EB).into(),

            selection: hsla(211.0 / 360.0, 1.0, 0.50, 0.35),
            code_wash: hsla(220.0 / 360.0, 0.10, 0.12, 0.07),

            inverse: rgb(0x202227).into(),
            on_inverse: rgb(0xF8F8F9).into(),

            warning: rgb(0xA66B20).into(),
            success: rgb(0x2F8F52).into(),
            favorite: rgb(0xCA8A04).into(),
            danger: rgb(0xC64A42).into(),
            danger_soft: hsla(4.0 / 360.0, 0.55, 0.52, 0.10),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ActiveRakuTheme(Theme);

impl Global for ActiveRakuTheme {}

/// Publish the resolved palette. [`Theme::current`] reads it back from the
/// global, which is how every view gets its colors.
pub(crate) fn set_active_theme(theme: Theme, cx: &mut App) {
    cx.set_global(ActiveRakuTheme(theme));
}

/// Resolve and publish the startup palette, before any window exists.
pub fn init(cx: &mut App) {
    let system_appearance = cx.window_appearance();
    let theme = if resolves_to_dark(ThemePreference::System, system_appearance) {
        Theme::dark()
    } else {
        Theme::light()
    };
    set_active_theme(theme, cx);
}

pub fn apply_theme_preference(preference: ThemePreference, window: &mut Window, cx: &mut App) {
    crate::platform::set_window_appearance(window, native_override(preference));
    let is_dark = resolves_to_dark(preference, cx.window_appearance());
    set_active_theme(
        if is_dark {
            Theme::dark()
        } else {
            Theme::light()
        },
        cx,
    );
    window.refresh();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_font_uses_the_system_family_instead_of_emergency_fallback() {
        let family = resolve_font_family(Some("Missing Font"), |_| false);
        assert_eq!(family.as_ref(), ".SystemUIFont");
    }

    #[test]
    fn available_font_keeps_the_selected_family() {
        let family = resolve_font_family(Some("Source Han Sans SC"), |name| {
            name == "Source Han Sans SC"
        });
        assert_eq!(family.as_ref(), "Source Han Sans SC");
    }

    #[test]
    fn default_font_preferences_skip_platform_lookup() {
        for name in [None, Some(""), Some("  ")] {
            let family = resolve_font_family(name, |_| panic!("no font to look up"));
            assert_eq!(family.as_ref(), ".SystemUIFont");
        }
    }
}
