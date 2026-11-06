//! Raku's tooltip surface.
//!
//! GPUI already owns tooltip *behaviour* — hover timing, placement, dismissal —
//! through `InteractiveElement::tooltip`, which asks only for a view to render.
//! This is that view, and nothing more.

use gpui::{
    AnyView, App, AppContext, IntoElement, ParentElement, Render, SharedString, Styled, Window,
    div, px,
};

use crate::theme::{Theme, sp};

/// A compact hover hint.
pub struct Tooltip {
    label: SharedString,
}

impl Tooltip {
    pub const MAX_WIDTH: f32 = 380.0;
    pub const MAX_LINES: usize = 4;

    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
        }
    }

    /// Build the view GPUI's `.tooltip(..)` expects.
    pub fn build(self, _window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|_| self).into()
    }

    /// Shorthand for the overwhelmingly common case:
    /// `.tooltip(Tooltip::text("Copy message"))`.
    pub fn text(
        label: impl Into<SharedString>,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let label = label.into();
        move |window, cx| Tooltip::new(label.clone()).build(window, cx)
    }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        // The outer wrapper is transparent and only offsets the card from the
        // cursor; the shadow needs a parent that does not clip it.
        div().pt(px(4.0)).pl(px(2.0)).child(
            div()
                .max_w(px(Self::MAX_WIDTH))
                .px(px(7.0))
                .py(px(4.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.raised)
                .shadow_md()
                .text_size(sp(12.5))
                .line_height(sp(16.0))
                .text_color(theme.text_secondary)
                .whitespace_normal()
                .line_clamp(Self::MAX_LINES)
                .text_ellipsis()
                .child(self.label.clone()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn tooltip_has_bounded_maximum_dimensions() {
        assert!(Tooltip::MAX_WIDTH > 0.0 && Tooltip::MAX_WIDTH <= 480.0);
        assert!(Tooltip::MAX_LINES > 0 && Tooltip::MAX_LINES <= 6);
    }

    #[gpui::test]
    fn tooltip_renders_long_labels_without_panic(cx: &mut TestAppContext) {
        let long_label = "cd /Users/limboy/Projects/raku && python3 - <<'PY' import io p='docs/multi-model-delegation-plan.md' s=io.open(p,encoding='utf-8').read() old = '\"\"\"### 相对提案的偏差\"\"\" new = '\"\"\"### 父 agent 矩阵与提示词契约（首版实测后补充）- 父 provider 新增 **Pi / Oh My Pi**: `res...".repeat(2);
        let (_, cx) = cx.add_window_view(|_window, _cx| Tooltip::new(long_label));
        cx.run_until_parked();
    }
}
