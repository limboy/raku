use gpui::{BorderStyle, Bounds, Hsla, PaintQuad, Pixels, point, px, quad, size};

/// Shared paint for an atomic prompt mention in the composer and transcript.
/// Derive the capsule from the line box so it follows the user's text scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PromptPillStyle {
    pub foreground: Hsla,
    background: Hsla,
    border: Hsla,
}

impl PromptPillStyle {
    pub fn new(accent: Hsla, is_dark: bool) -> Self {
        Self {
            foreground: accent,
            background: accent.opacity(if is_dark { 0.12 } else { 0.08 }),
            border: accent.opacity(if is_dark { 0.22 } else { 0.16 }),
        }
    }

    /// The incoming bounds cover the glyph advances and the full line height.
    /// Keep this decoration in paint so selection and copying use source bytes.
    pub fn quad(self, bounds: Bounds<Pixels>) -> PaintQuad {
        let scale = bounds.size.height / px(21.0);
        let padding = px(4.0 * scale);
        let inset = px(1.0 * scale);
        let bounds = Bounds::new(
            point(bounds.left() - padding, bounds.top() + inset),
            size(
                bounds.size.width + padding * 2.0,
                bounds.size.height - inset * 2.0,
            ),
        );
        quad(
            bounds,
            bounds.size.height / 2.0,
            self.background,
            px(0.75),
            self.border,
            BorderStyle::default(),
        )
    }
}
