//! One reusable native preview window. Jobs are cancellable and generation
//! checked; zoom/pan read an already rasterized image and never rerun Pele.

use super::*;

use gpui::{
    App, Bounds, ClipboardItem, Context, Div, FocusHandle, Global, IntoElement, KeyBinding,
    KeyDownEvent, ObjectFit, Render, ScrollHandle, SharedString, Stateful, TitlebarOptions, Window,
    WindowBounds, WindowHandle, WindowOptions, actions, div, img, point, prelude::*, px, size,
};

use crate::ui::ActivationExt;
use crate::ui::tooltip::Tooltip;

actions!(mermaid_preview, [ClosePreview, ZoomIn, ZoomOut, FitDiagram]);

const KEY_CONTEXT: &str = "MermaidPreview";
const TOOLBAR_HEIGHT: f32 = 44.0;
const PADDING: f32 = 16.0;

#[derive(Default)]
struct PreviewWindow(Option<WindowHandle<Preview>>);
impl Global for PreviewWindow {}

pub(crate) fn open(source: SharedString, parent: &mut Window, cx: &mut App) {
    if !cx.has_global::<PreviewWindow>() {
        cx.set_global(PreviewWindow::default());
        cx.bind_keys([
            KeyBinding::new("escape", ClosePreview, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary-w", ClosePreview, Some(KEY_CONTEXT)),
            KeyBinding::new("=", ZoomIn, Some(KEY_CONTEXT)),
            KeyBinding::new("+", ZoomIn, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary-=", ZoomIn, Some(KEY_CONTEXT)),
            KeyBinding::new("-", ZoomOut, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary--", ZoomOut, Some(KEY_CONTEXT)),
            KeyBinding::new("0", FitDiagram, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary-0", FitDiagram, Some(KEY_CONTEXT)),
        ]);
    }
    let source: Arc<str> = Arc::from(source.as_str());
    if let Some(handle) = cx.global::<PreviewWindow>().0
        && handle
            .update(cx, |view, window, cx| {
                view.set_source(source.clone(), cx);
                window.activate_window();
                window.focus(&view.focus, cx);
            })
            .is_ok()
    {
        return;
    }
    let display_id = parent.display(cx).map(|display| display.id());
    let result = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                display_id,
                size(px(920.0), px(640.0)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some(tr!("mermaid.title").into()),
                ..Default::default()
            }),
            window_min_size: Some(size(px(560.0), px(300.0))),
            display_id,
            ..Default::default()
        },
        |window, cx| {
            cx.new(|cx| {
                let preview = Preview::new(source, window, cx);
                window.focus(&preview.focus, cx);
                preview
            })
        },
    );
    match result {
        Ok(handle) => cx.global_mut::<PreviewWindow>().0 = Some(handle),
        Err(error) => {
            let _ = parent.prompt(
                gpui::PromptLevel::Critical,
                &tr!("mermaid.title"),
                Some(&error.to_string()),
                &[tr!("common.close").as_str()],
                cx,
            );
        }
    }
}

struct Preview {
    source: Arc<str>,
    focus: FocusHandle,
    controls: [FocusHandle; 5],
    scroll: ScrollHandle,
    rendered: Option<Rendered>,
    error: Option<SharedString>,
    pending: bool,
    style: Option<(Colors, u32)>,
    generation: u64,
    cancel: Arc<AtomicBool>,
    fit: bool,
    zoom: f32,
}

impl Preview {
    fn new(source: Arc<str>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        crate::platform::configure_window_mouse_tracking(window, cx);
        // GPUI's simulated windows have no native handle. Their theme still
        // follows the same global observer and background rendering path.
        #[cfg(not(test))]
        crate::platform::set_window_appearance(window, Some(Theme::current(cx).is_dark));
        cx.observe_global_in::<crate::theme::ActiveRakuTheme>(window, |_, window, cx| {
            #[cfg(test)]
            let _ = window;
            #[cfg(not(test))]
            crate::platform::set_window_appearance(window, Some(Theme::current(cx).is_dark));
            cx.notify();
        })
        .detach();
        cx.on_release(|view, cx| {
            view.cancel.store(true, Ordering::Relaxed);
            if let Some(rendered) = view.rendered.take() {
                App::defer(cx, move |cx| {
                    cx.drop_image(rendered.image, None);
                });
            }
        })
        .detach();
        Self {
            source,
            focus: cx.focus_handle(),
            controls: std::array::from_fn(|_| cx.focus_handle().tab_index(0).tab_stop(true)),
            scroll: ScrollHandle::new(),
            rendered: None,
            error: None,
            pending: false,
            style: None,
            generation: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            fit: true,
            zoom: 1.0,
        }
    }

    fn set_source(&mut self, source: Arc<str>, cx: &mut Context<Self>) {
        if self.source != source {
            self.cancel.store(true, Ordering::Relaxed);
            self.generation = self.generation.wrapping_add(1);
            self.source = source;
            self.style = None;
            self.error = None;
            if let Some(previous) = self.rendered.take() {
                App::defer(cx, move |cx| {
                    cx.drop_image(previous.image, None);
                });
            }
            self.fit = true;
            self.scroll.set_offset(point(px(0.0), px(0.0)));
            cx.notify();
        }
    }

    fn request(&mut self, colors: Colors, scale: f32, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.style = Some((colors.clone(), scale.to_bits()));
        self.pending = true;
        self.error = None;
        let source = self.source.clone();
        let cancel = self.cancel.clone();
        let renderer = cx.svg_renderer();
        let worker = cx.background_executor().spawn(async move {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                super::render(&source, &colors, scale, cancel, &renderer)
            }))
            .unwrap_or_else(|_| Err(tr!("mermaid.render_failed")))
        });
        cx.spawn(async move |view, cx| {
            let result = worker.await;
            let _ = view.update(cx, |view, cx| {
                if view.generation != generation {
                    return;
                }
                view.pending = false;
                match result {
                    Ok(rendered) => {
                        if let Some(previous) = view.rendered.replace(rendered) {
                            App::defer(cx, move |cx| {
                                cx.drop_image(previous.image, None);
                            });
                        }
                    }
                    Err(error) => view.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn current_zoom(&self, window: &Window) -> f32 {
        match (&self.rendered, self.fit) {
            (Some(rendered), true) => {
                let available = window.viewport_size();
                fit_zoom(
                    rendered.width,
                    rendered.height,
                    f32::from(available.width) - PADDING * 2.0,
                    f32::from(available.height) - TOOLBAR_HEIGHT - PADDING * 2.0,
                )
            }
            _ => self.zoom,
        }
    }

    fn change_zoom(&mut self, factor: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom = (self.current_zoom(window) * factor).clamp(0.025, 4.0);
        self.fit = false;
        cx.notify();
    }

    fn fit_diagram(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.fit = true;
        self.scroll.set_offset(point(px(0.0), px(0.0)));
        cx.notify();
    }

    fn pan(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        let (x, y) = match event.keystroke.key.as_str() {
            "left" => (48.0, 0.0),
            "right" => (-48.0, 0.0),
            "up" => (0.0, 48.0),
            "down" => (0.0, -48.0),
            _ => return,
        };
        let offset = self.scroll.offset();
        let max = self.scroll.max_offset();
        self.scroll.set_offset(point(
            (offset.x + px(x)).clamp(-max.x, px(0.0)),
            (offset.y + px(y)).clamp(-max.y, px(0.0)),
        ));
        cx.notify();
        cx.stop_propagation();
    }

    fn control(
        &self,
        id: &'static str,
        label: SharedString,
        index: usize,
        theme: Theme,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .track_focus(&self.controls[index])
            .tab_index(0)
            .h(px(28.0))
            .min_w(px(28.0))
            .px(px(8.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .cursor_default()
            .border_1()
            .border_color(gpui::transparent_black())
            .focus_visible(|style| style.border_color(theme.accent))
            .hover(|style| style.bg(theme.overlay))
            .child(label)
            .on_activation(cx, action)
    }
}

fn fit_zoom(width: f32, height: f32, available_width: f32, available_height: f32) -> f32 {
    (available_width.max(1.0) / width)
        .min(available_height.max(1.0) / height)
        .min(1.0)
        .max(0.001)
}

impl Render for Preview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        let colors = Colors::from_theme(theme);
        let scale = window.scale_factor();
        if self.style.as_ref() != Some(&(colors.clone(), scale.to_bits())) {
            self.request(colors, scale, cx);
        }
        let zoom = self.current_zoom(window);
        let mut toolbar = div()
            .w_full()
            .h(px(TOOLBAR_HEIGHT))
            .flex_none()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(tr_cow!("mermaid.title")),
            )
            .child(
                self.control(
                    "mermaid-zoom-out",
                    "−".into(),
                    0,
                    theme,
                    |view, window, cx| view.change_zoom(0.8, window, cx),
                    cx,
                )
                .tooltip(Tooltip::text(tr!("mermaid.zoom_out"))),
            )
            .child(
                div()
                    .min_w(px(44.0))
                    .flex()
                    .justify_center()
                    .text_color(theme.text_secondary)
                    .child(format!("{:.0}%", zoom * 100.0)),
            )
            .child(
                self.control(
                    "mermaid-zoom-in",
                    "+".into(),
                    1,
                    theme,
                    |view, window, cx| view.change_zoom(1.25, window, cx),
                    cx,
                )
                .tooltip(Tooltip::text(tr!("mermaid.zoom_in"))),
            )
            .child(self.control(
                "mermaid-fit",
                tr!("mermaid.fit").into(),
                2,
                theme,
                Self::fit_diagram,
                cx,
            ));
        if self.rendered.is_some() {
            toolbar = toolbar.child(self.control(
                "mermaid-copy-svg",
                tr!("mermaid.copy_svg").into(),
                3,
                theme,
                |view, _, cx| {
                    if let Some(rendered) = &view.rendered {
                        cx.write_to_clipboard(ClipboardItem::new_string(rendered.svg.to_string()));
                    }
                },
                cx,
            ));
        }
        toolbar = toolbar.child(self.control(
            "mermaid-close",
            tr!("common.close").into(),
            4,
            theme,
            |_, window, _| window.remove_window(),
            cx,
        ));
        let viewport = div()
            .id("mermaid-viewport")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_scroll()
            .track_scroll(&self.scroll)
            .child(if let Some(rendered) = &self.rendered {
                div()
                    .w(px(rendered.width * zoom + PADDING * 2.0))
                    .h(px(rendered.height * zoom + PADDING * 2.0))
                    .min_w_full()
                    .min_h_full()
                    .p(px(PADDING))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        img(rendered.image.clone())
                            .flex_none()
                            .w(px(rendered.width * zoom))
                            .h(px(rendered.height * zoom))
                            .object_fit(ObjectFit::Contain),
                    )
                    .into_any_element()
            } else {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px(24.0))
                    .text_color(theme.text_secondary)
                    .child(if self.error.is_some() {
                        tr!("mermaid.render_failed")
                    } else {
                        tr!("mermaid.rendering")
                    })
                    .into_any_element()
            });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.canvas)
            .text_color(theme.text)
            .font_family(crate::theme::Fonts::current(cx).interface)
            .text_size(px(12.5))
            .track_focus(&self.focus)
            .key_context(KEY_CONTEXT)
            .tab_group()
            .tab_stop(false)
            .on_action(cx.listener(|_, _: &ClosePreview, window, _| window.remove_window()))
            .on_action(
                cx.listener(|view, _: &ZoomIn, window, cx| view.change_zoom(1.25, window, cx)),
            )
            .on_action(
                cx.listener(|view, _: &ZoomOut, window, cx| view.change_zoom(0.8, window, cx)),
            )
            .on_action(cx.listener(|view, _: &FitDiagram, window, cx| view.fit_diagram(window, cx)))
            .on_key_down(cx.listener(Self::pan))
            .child(toolbar)
            .child(viewport)
            .when(self.pending && self.rendered.is_some(), |element| {
                element.child(
                    div()
                        .w_full()
                        .px(px(12.0))
                        .py(px(6.0))
                        .child(tr_cow!("mermaid.rendering")),
                )
            })
            .when_some(self.error.clone(), |element, error| {
                element.child(
                    div()
                        .w_full()
                        .max_h(px(120.0))
                        .px(px(12.0))
                        .py(px(8.0))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_both_axes_visible_without_enlarging_small_diagrams() {
        assert_eq!(fit_zoom(200.0, 64.0, 800.0, 500.0), 1.0);
        assert_eq!(fit_zoom(1600.0, 400.0, 800.0, 500.0), 0.5);
        assert_eq!(fit_zoom(400.0, 2000.0, 800.0, 500.0), 0.25);
        assert!(fit_zoom(100.0, 100.0, 0.0, 0.0).is_finite());
    }

    #[gpui::test]
    fn preview_reuses_its_window_ignores_old_jobs_and_zooms_without_rendering_again(
        cx: &mut gpui::TestAppContext,
    ) {
        struct Parent;
        impl Render for Parent {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }
        let (_, cx) = cx.add_window_view(|_, _| Parent);
        cx.update(|window, cx| {
            open("flowchart LR\n A[Old]".into(), window, cx);
            let first = cx.global::<PreviewWindow>().0.unwrap();
            // Queue an old job before replacing its source, so its eventual
            // completion must not overwrite the second diagram.
            first
                .update(cx, |view, window, cx| {
                    view.request(
                        Colors::from_theme(Theme::current(cx)),
                        window.scale_factor(),
                        cx,
                    )
                })
                .unwrap();
            open("flowchart LR\n A[New]".into(), window, cx);
            let second = cx.global::<PreviewWindow>().0.unwrap();
            assert_eq!(first.window_id(), second.window_id());
            assert_eq!(cx.windows().len(), 2);
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let handle = cx.global::<PreviewWindow>().0.unwrap();
            handle
                .update(cx, |view, window, cx| {
                    assert!(view.error.is_none(), "{:?}", view.error);
                    assert!(!view.pending);
                    let rendered = view
                        .rendered
                        .as_ref()
                        .expect("background rendering completed");
                    assert!(rendered.svg.contains("New"));
                    assert!(!rendered.svg.contains("Old"));
                    window.focus_next(cx);
                    assert!(
                        view.controls[0].is_focused(window),
                        "Tab must reach the zoom controls"
                    );
                    let image = rendered.image.clone();
                    let generation = view.generation;
                    view.change_zoom(1.25, window, cx);
                    view.fit_diagram(window, cx);
                    assert_eq!(view.generation, generation);
                    assert!(Arc::ptr_eq(&image, &view.rendered.as_ref().unwrap().image));
                    window.dispatch_action(Box::new(ClosePreview), cx);
                })
                .unwrap();
        });
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(cx.windows().len(), 1));
    }

    #[gpui::test]
    fn open_preview_recolors_when_the_app_theme_changes(cx: &mut gpui::TestAppContext) {
        struct Parent;
        impl Render for Parent {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }
        let (_, cx) = cx.add_window_view(|_, _| Parent);
        cx.update(|window, cx| {
            crate::theme::set_active_theme(Theme::light(), cx);
            open("flowchart LR\n A[Theme] --> B[Preview]".into(), window, cx);
        });
        cx.run_until_parked();
        let light = cx.update(|_, cx| {
            let handle = cx.global::<PreviewWindow>().0.unwrap();
            let svg = handle
                .read(cx)
                .unwrap()
                .rendered
                .as_ref()
                .unwrap()
                .svg
                .clone();
            crate::theme::set_active_theme(Theme::dark(), cx);
            svg
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let handle = cx.global::<PreviewWindow>().0.unwrap();
            let preview = handle.read(cx).unwrap();
            assert!(preview.error.is_none(), "{:?}", preview.error);
            let dark = &preview.rendered.as_ref().unwrap().svg;
            assert_ne!(&light, dark);
            assert!(dark.contains(&Colors::from_theme(Theme::dark()).text));
        });
    }
}
