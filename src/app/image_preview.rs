//! Window-modal preview for image attachments.
//!
//! Daemon-owned image bytes are retained as GPUI images only while the desktop
//! needs them. Opening a preview therefore changes only in-memory UI state;
//! the frame path never probes the filesystem or performs RPC.

use gpui::{KeyBinding, actions};

use super::*;
use crate::ui::image::{TRANSCRIPT_IMAGE_MAX_HEIGHT, TRANSCRIPT_IMAGE_MAX_WIDTH, fitted_image};

actions!(raku_image_preview, [DismissImagePreview]);

const IMAGE_PREVIEW_CONTEXT: &str = "ImagePreview";
const IMAGE_PREVIEW_ANIMATION_DURATION: Duration = Duration::from_millis(140);
const IMAGE_PREVIEW_PRIORITY: usize = 5;
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "escape",
        DismissImagePreview,
        Some(IMAGE_PREVIEW_CONTEXT),
    )]);
}

pub(super) struct ImagePreviewState {
    image: Arc<gpui::Image>,
    source_path: Option<PathBuf>,
    name: SharedString,
    focus: FocusHandle,
    close_focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    generation: u64,
}

#[derive(IntoElement)]
struct TranscriptImage {
    raku: gpui::WeakEntity<Raku>,
    id: SharedString,
    reference: String,
    name: SharedString,
    source_path: Option<PathBuf>,
    max_size: gpui::Size<Pixels>,
    framed: bool,
}

impl gpui::RenderOnce for TranscriptImage {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::current(cx);
        let raku = self.raku.clone();
        raku.update(cx, |raku, cx| {
            raku.render_transcript_image(self, &theme, cx)
        })
        .unwrap_or_else(|_| div().into_any_element())
    }
}

pub(super) fn markdown_image_renderer(raku: gpui::WeakEntity<Raku>) -> md::render::ImageRenderer {
    Rc::new(move |id, reference, alt| {
        let source_path = image_path_for_reference(reference);
        let max_size = gpui::size(
            px(TRANSCRIPT_IMAGE_MAX_WIDTH),
            px(TRANSCRIPT_IMAGE_MAX_HEIGHT),
        );
        if source_path.is_none()
            && !raku_protocol::blob::is_reference(reference)
            && !reference.starts_with(raku_protocol::attachments::ATTACHMENT_SCHEME)
            && !reference.starts_with("data:image/")
            && !reference.starts_with("https://")
            && !reference.starts_with("http://")
        {
            return fitted_image(reference.to_owned(), max_size, px(6.0)).into_any_element();
        }
        let name = if alt.trim().is_empty() {
            source_path
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| tr!("activity.image_output"))
        } else {
            alt.to_owned()
        };
        TranscriptImage {
            raku: raku.clone(),
            id: SharedString::from(format!("{id}-control")),
            reference: reference.to_owned(),
            name: name.into(),
            source_path,
            max_size,
            framed: false,
        }
        .into_any_element()
    })
}

pub(super) fn image_path_for_reference(reference: &str) -> Option<PathBuf> {
    if reference.starts_with("file://") {
        return url::Url::parse(reference).ok()?.to_file_path().ok();
    }
    let path = Path::new(reference);
    path.is_absolute().then(|| path.to_path_buf())
}

fn image_from_bytes(bytes: Vec<u8>, format: Option<gpui::ImageFormat>) -> Option<Arc<gpui::Image>> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    let format = image::guess_format(&bytes)
        .ok()
        .and_then(|format| gpui::ImageFormat::from_mime_type(format.to_mime_type()))
        .or(format)?;
    Some(Arc::new(gpui::Image::from_bytes(format, bytes)))
}

fn image_menu_items(
    image: &Arc<gpui::Image>,
    source_path: Option<PathBuf>,
    raku: &gpui::WeakEntity<Raku>,
) -> Vec<MenuItem> {
    let add_image = image.clone();
    let add_raku = raku.clone();
    let reveal_image = image.clone();
    let reveal_raku = raku.clone();
    image_action_menu_items(
        image,
        Rc::new(move |window, cx| {
            let _ = add_raku.update(cx, |raku, cx| {
                if raku.image_preview.is_some() {
                    raku.close_image_preview(window, cx);
                }
                raku.stage_pasted_attachments(
                    vec![ClipboardEntry::Image((*add_image).clone())],
                    cx,
                );
                window.focus(&raku.composer_focus(cx), cx);
            });
        }),
        Rc::new(move |_, cx| {
            let _ = reveal_raku.update(cx, |raku, cx| {
                raku.reveal_transcript_image(reveal_image.clone(), source_path.clone(), cx);
            });
        }),
    )
}

type ImageActionCallback = Rc<dyn Fn(&mut Window, &mut App)>;

fn image_reveal_label() -> String {
    if cfg!(target_os = "macos") {
        tr!("attachments.reveal_in_finder")
    } else {
        tr!("common.reveal_in_finder")
    }
}

fn image_action_menu_items(
    image: &Arc<gpui::Image>,
    add_to_chat: ImageActionCallback,
    reveal_in_finder: ImageActionCallback,
) -> Vec<MenuItem> {
    let image = image.clone();
    vec![
        MenuItem::new(tr!("common.add_to_chat"), move |window, cx| {
            add_to_chat(window, cx)
        })
        .icon("icons/plus.svg"),
        MenuItem::new(tr!("common.copy_image"), move |_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_image(image.as_ref()));
        })
        .icon("icons/copy.svg"),
        MenuItem::new(image_reveal_label(), move |window, cx| {
            reveal_in_finder(window, cx)
        })
        .icon("icons/folder.svg"),
    ]
}

pub(super) fn attachment_menu_items(path: PathBuf, can_reveal: bool) -> Vec<MenuItem> {
    vec![
        MenuItem::new(tr!("common.reveal_in_finder"), move |_, cx| {
            crate::platform::reveal_in_file_manager(&path, cx);
        })
        .icon("icons/folder.svg")
        .disabled(!can_reveal),
    ]
}

impl Raku {
    /// Resolve one daemon-owned image for a visible row. Frames consult only
    /// in-memory state; the first miss starts a deduplicated background RPC and
    /// a later notification lets GPUI render the returned bytes from memory.
    pub(super) fn image_for_reference(
        &self,
        reference: &str,
        daemon_path: Option<&Path>,
        name: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Option<Arc<gpui::Image>> {
        if let Some(state) = self.remote_images.borrow().get(reference) {
            return match state {
                RemoteImageState::Ready(image) => Some(image.clone()),
                RemoteImageState::Loading | RemoteImageState::Unavailable => None,
            };
        }
        let attachment_reference =
            reference.starts_with(raku_protocol::attachments::ATTACHMENT_SCHEME);
        let inline_image = reference.starts_with("data:image/");
        let source_path = image_path_for_reference(reference);
        let web_image = reference.starts_with("https://") || reference.starts_with("http://");
        if !raku_protocol::blob::is_reference(reference)
            && !attachment_reference
            && !inline_image
            && source_path.is_none()
            && !web_image
        {
            return None;
        }

        let format = name
            .and_then(image_format_for_name)
            .or_else(|| {
                source_path
                    .as_ref()
                    .and_then(|path| path.to_str())
                    .and_then(image_format_for_name)
            })
            .or_else(|| image_format_for_name(reference));

        self.remote_images
            .borrow_mut()
            .insert(reference.to_owned(), RemoteImageState::Loading);
        let cache_key = reference.to_owned();
        let fetch_reference = cache_key.clone();
        let daemon_path = daemon_path.map(Path::to_path_buf);
        let daemon = self.daemon.clone();
        let http = cx.http_client();
        cx.spawn(async move |raku, cx| {
            let image = cx
                .background_executor()
                .spawn(async move {
                    if inline_image {
                        if fetch_reference.len() as u64 > MAX_IMAGE_BYTES * 4 / 3 + 128 {
                            return None;
                        }
                        return md::render::decode_data_url(&fetch_reference);
                    }
                    if let Some(path) = source_path {
                        use std::io::Read as _;
                        let mut bytes = Vec::new();
                        std::fs::File::open(path)
                            .ok()?
                            .take(MAX_IMAGE_BYTES + 1)
                            .read_to_end(&mut bytes)
                            .ok()?;
                        return image_from_bytes(bytes, format);
                    }
                    if web_image {
                        use futures::AsyncReadExt as _;
                        let mut response =
                            http.get(&fetch_reference, ().into(), true).await.ok()?;
                        if !response.status().is_success() {
                            return None;
                        }
                        let response_format = response
                            .headers()
                            .get("content-type")
                            .and_then(|header| header.to_str().ok())
                            .and_then(|mime| {
                                gpui::ImageFormat::from_mime_type(mime.split(';').next()?.trim())
                            });
                        let mut bytes = Vec::new();
                        response
                            .body_mut()
                            .take(MAX_IMAGE_BYTES + 1)
                            .read_to_end(&mut bytes)
                            .await
                            .ok()?;
                        return image_from_bytes(bytes, response_format.or(format));
                    }
                    let bytes = raku_client::persistence::read_remote_reference(
                        &fetch_reference,
                        daemon_path.as_deref(),
                        &daemon,
                    )?;
                    image_from_bytes(bytes, format)
                })
                .await;
            let _ = raku.update(cx, |raku, cx| {
                raku.remote_images.borrow_mut().insert(
                    cache_key,
                    image.map_or(RemoteImageState::Unavailable, RemoteImageState::Ready),
                );
                cx.notify();
            });
        })
        .detach();
        None
    }

    fn reveal_transcript_image(
        &self,
        image: Arc<gpui::Image>,
        source_path: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let daemon = self.daemon.clone();
        cx.spawn(async move |raku, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(path) = source_path.filter(|path| path.is_file()) {
                        return Ok(path);
                    }
                    match daemon.client().request(
                        Uuid::nil(),
                        Uuid::nil(),
                        raku_client::Command::StoreBlob {
                            mime_type: image.format.mime_type().into(),
                            bytes: image.bytes.clone(),
                        },
                    )? {
                        raku_client::ResponsePayload::BlobStored { path, .. } => Ok(path),
                        _ => anyhow::bail!("the daemon returned an invalid blob response"),
                    }
                })
                .await;
            let _ = raku.update(cx, |raku, cx| match result {
                Ok(path) => crate::platform::reveal_in_file_manager(&path, cx),
                Err(error) => {
                    raku.show_toast(error.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_image_generation_row(
        &self,
        activity: &ActivityItem,
        surface: &transcript_view::ActivityRenderContext<'_>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if surface.part == transcript_view::ActivityClusterPart::Item {
            return div().into_any_element();
        }
        let color = if activity.failed {
            theme.danger
        } else {
            theme.text_secondary
        };
        let mut card = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .items_start()
            .gap(px(6.0))
            .child(
                div()
                    .min_h(px(28.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .text_color(color)
                    .child(icon(
                        if activity.failed {
                            "icons/alert.svg"
                        } else {
                            "icons/file-types/image.svg"
                        },
                        14.0,
                        color,
                    ))
                    .child(activity_display_title(activity))
                    .when(!activity.complete, |row| {
                        row.child(pulse_dot(5.0, theme.accent))
                    }),
            );
        if let Some(detail) = activity.detail.as_ref() {
            card = card.child(
                div()
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .text_color(color)
                    .child(detail.clone()),
            );
        }
        for (index, reference) in activity.image_urls.iter().enumerate() {
            card = card.child(
                self.render_activity_image(
                    reference,
                    activity.id,
                    index,
                    activity
                        .output
                        .as_deref()
                        .and_then(image_path_for_reference),
                    theme,
                    cx,
                ),
            );
        }
        card.into_any_element()
    }

    /// Both generated outputs and tool screenshots share the in-memory cache
    /// and the keyboard-operable window preview used by user attachments.
    pub(super) fn render_activity_image(
        &self,
        reference: &str,
        activity_id: Uuid,
        index: usize,
        source_path: Option<PathBuf>,
        _theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        TranscriptImage {
            raku: cx.entity().downgrade(),
            id: format!("activity-image-{activity_id}-{index}").into(),
            reference: reference.to_owned(),
            name: tr!("activity.image_output").into(),
            source_path: source_path.or_else(|| image_path_for_reference(reference)),
            max_size: gpui::size(px(ACTIVITY_IMAGE_WIDTH), px(ACTIVITY_IMAGE_HEIGHT)),
            framed: true,
        }
        .into_any_element()
    }

    fn render_transcript_image(
        &self,
        target: TranscriptImage,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let TranscriptImage {
            raku,
            id,
            reference,
            name,
            source_path,
            max_size,
            framed,
        } = target;
        let image = self.image_for_reference(&reference, source_path.as_deref(), None, cx);
        let mut tile = div()
            .id(id.clone())
            .max_w(gpui::relative(1.0))
            .self_start()
            .rounded(px(6.0))
            .overflow_hidden()
            .border_1()
            .border_color(if framed {
                theme.border
            } else {
                gpui::hsla(0.0, 0.0, 0.0, 0.0)
            })
            .flex()
            .flex_col()
            .items_start();
        if let Some(image) = image {
            let focus = self.transcript_control_focus(id.clone(), cx);
            let restore_focus = focus.clone();
            let menu = self.menu_handle_with(format!("{id}-menu"), cx, move |open, window, cx| {
                if !open {
                    window.focus(&restore_focus, cx);
                }
            });
            let key_menu = menu.clone();
            let key_image = image.clone();
            let key_name = name.clone();
            let click_image = image.clone();
            let click_path = source_path.clone();
            let key_path = source_path.clone();
            let label = tr!("attachments.open_preview", name = name.as_ref());
            tile = tile
                .track_focus(&focus)
                .tab_index(0)
                .cursor_default()
                .focus_visible(|style| style.border_color(theme.accent))
                .when(framed, |tile| {
                    tile.hover(|style| style.border_color(theme.border_strong))
                })
                .tooltip(Tooltip::text(label))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_image_preview(click_image.clone(), name.clone(), window, cx);
                    if let Some(preview) = this.image_preview.as_mut() {
                        preview.source_path = click_path.clone();
                    }
                    cx.stop_propagation();
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    if !event.keystroke.modifiers.modified() && matches!(key, "enter" | "space") {
                        this.open_image_preview(key_image.clone(), key_name.clone(), window, cx);
                        if let Some(preview) = this.image_preview.as_mut() {
                            preview.source_path = key_path.clone();
                        }
                        cx.stop_propagation();
                    } else if key == "f10" && event.keystroke.modifiers.shift {
                        key_menu.open_context_menu(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(fitted_image(image.clone(), max_size, px(5.0)));
            return context_menu(
                tile,
                SharedString::from(format!("{id}-context-menu")),
                &menu,
                move |_| image_menu_items(&image, source_path.clone(), &raku),
            );
        } else {
            let unavailable = matches!(
                self.remote_images.borrow().get(&reference),
                Some(RemoteImageState::Unavailable)
            );
            tile = tile
                .w(max_size.width)
                .h(max_size.height)
                .flex_row()
                .items_center()
                .justify_center()
                .bg(theme.inset)
                .gap(px(8.0))
                .text_size(sp(12.0))
                .text_color(theme.text_ghost)
                .child(icon("icons/file-types/image.svg", 18.0, theme.text_ghost))
                .when(unavailable, |tile| {
                    tile.child(tr!("attachments.preview_unavailable"))
                });
        }
        tile.into_any_element()
    }

    pub(super) fn open_image_preview(
        &mut self,
        image: Arc<gpui::Image>,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.image_preview_generation = self.image_preview_generation.wrapping_add(1);
        let generation = self.image_preview_generation;
        let focus = cx.focus_handle();
        self.image_preview = Some(ImagePreviewState {
            image,
            source_path: None,
            name,
            focus: focus.clone(),
            close_focus: cx.focus_handle(),
            previous_focus: window.focused(cx),
            generation,
        });

        // The preview is deferred onto GPUI's overlay plane. Wait until that
        // subtree has joined the dispatch tree before focusing it, so Escape
        // is reliable on the first key press.
        let weak = cx.entity().downgrade();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| {
                let mut should_focus = false;
                let _ = weak.update(cx, |this, _| {
                    should_focus = this
                        .image_preview
                        .as_ref()
                        .is_some_and(|preview| preview.generation == generation);
                });
                if should_focus {
                    window.focus(&focus, cx);
                }
            });
        });
        cx.notify();
    }

    fn close_image_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = self.image_preview.take() else {
            return;
        };
        if let Some(menu) = self.menus.borrow().get("image-preview-image-menu") {
            menu.close(window, cx);
        }
        self.image_preview_generation = self.image_preview_generation.wrapping_add(1);
        if let Some(previous_focus) = preview.previous_focus {
            window.focus(&previous_focus, cx);
        } else {
            let focus = self.composer_focus(cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    pub(super) fn render_image_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let preview = self.image_preview.as_ref()?;
        let theme = Theme::current(cx);
        let image_source = preview.image.clone();
        let name = preview.name.clone();
        let focus = preview.focus.clone();
        let close_focus = preview.close_focus.clone();
        let generation = preview.generation;
        let image_focus = self.transcript_control_focus("image-preview-image-focus", cx);
        let restore_focus = image_focus.clone();
        let image_menu =
            self.menu_handle_with("image-preview-image-menu", cx, move |open, window, cx| {
                if !open {
                    window.focus(&restore_focus, cx);
                }
            });
        let keyboard_menu = image_menu.clone();
        let menu_image = preview.image.clone();
        let menu_path = preview.source_path.clone();
        let menu_raku = cx.entity().downgrade();

        let close = div()
            .id("image-preview-close")
            .absolute()
            .top(px(14.0))
            .right(px(14.0))
            .track_focus(&close_focus)
            .tab_index(0)
            .size(px(32.0))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .bg(gpui::hsla(0.0, 0.0, 0.0, 0.48))
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|style| style.bg(gpui::hsla(0.0, 0.0, 0.0, 0.66)))
            .active(|style| style.opacity(0.8))
            .tooltip(Tooltip::text(tr!("attachments.close_preview")))
            .child(icon("icons/x.svg", 13.0, gpui::white()))
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_image_preview(window, cx);
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.close_image_preview(window, cx);
                    cx.stop_propagation();
                }
            }));

        let unavailable_color = gpui::white().opacity(0.78);
        let image = div()
            .id("image-preview-image")
            .track_focus(&image_focus)
            .tab_index(0)
            .border_1()
            .border_color(gpui::hsla(0.0, 0.0, 0.0, 0.0))
            .focus_visible(|style| style.border_color(theme.accent))
            .size_full()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .child(
                img(image_source)
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .with_fallback(move || {
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(sp(SMALL_UI_FONT_SIZE))
                            .text_color(unavailable_color)
                            .child(icon("icons/alert.svg", 18.0, unavailable_color))
                            .child(tr_cow!("attachments.preview_unavailable"))
                            .into_any_element()
                    }),
            );
        let image = crate::ui::menu::context_menu_with_priority(
            image,
            "image-preview-context-menu",
            &image_menu,
            IMAGE_PREVIEW_PRIORITY + 1,
            move |_| image_menu_items(&menu_image, menu_path.clone(), &menu_raku),
        );
        let content = div()
            .relative()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(12.0))
            .child(div().w_full().flex_1().min_h_0().child(image))
            .child(
                div()
                    .max_w(px(560.0))
                    .px(px(11.0))
                    .py(px(5.0))
                    .rounded_full()
                    .bg(gpui::hsla(0.0, 0.0, 0.0, 0.48))
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .text_color(gpui::white().opacity(0.9))
                    .truncate()
                    .child(name),
            )
            .child(close);

        let layer = div()
            .id(SharedString::from(format!(
                "image-preview-layer-{generation}"
            )))
            .absolute()
            .inset_0()
            .occlude()
            .track_focus(&focus)
            .key_context(IMAGE_PREVIEW_CONTEXT)
            .on_action(cx.listener(|this, _: &DismissImagePreview, window, cx| {
                this.close_image_preview(window, cx);
            }))
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "f10" && event.keystroke.modifiers.shift {
                    keyboard_menu.open_context_menu(window, cx);
                    cx.stop_propagation();
                }
            })
            .tab_group()
            .tab_stop(false)
            .bg(gpui::hsla(0.0, 0.0, 0.0, 0.82))
            .p(px(36.0))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.close_image_preview(window, cx);
                }),
            )
            .child(content)
            .with_animation(
                SharedString::from(format!("image-preview-enter-{generation}")),
                Animation::new(IMAGE_PREVIEW_ANIMATION_DURATION).with_easing(ease_out_quint()),
                |element, delta| element.opacity(delta),
            );

        Some(
            gpui::deferred(layer)
                .with_priority(IMAGE_PREVIEW_PRIORITY)
                .into_any_element(),
        )
    }
}

pub(super) fn image_format_for_name(name: &str) -> Option<gpui::ImageFormat> {
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some(gpui::ImageFormat::Png),
        "jpg" | "jpeg" => Some(gpui::ImageFormat::Jpeg),
        "webp" => Some(gpui::ImageFormat::Webp),
        "gif" => Some(gpui::ImageFormat::Gif),
        "svg" => Some(gpui::ImageFormat::Svg),
        "bmp" => Some(gpui::ImageFormat::Bmp),
        "tif" | "tiff" => Some(gpui::ImageFormat::Tiff),
        "ico" => Some(gpui::ImageFormat::Ico),
        "pnm" | "pbm" | "pgm" | "ppm" => Some(gpui::ImageFormat::Pnm),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_image() -> Arc<gpui::Image> {
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba([18, 45, 90, 255]),
        ))
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
        image_from_bytes(encoded.into_inner(), None).unwrap()
    }

    #[test]
    fn image_sources_decode_file_urls_without_treating_provider_references_as_paths() {
        assert_eq!(
            image_path_for_reference("file:///tmp/flying%20cat.png"),
            Some(PathBuf::from("/tmp/flying cat.png"))
        );
        for reference in [
            "waku-blob:12345678.png",
            "data:image/png;base64,AAAA",
            "https://example.com/image.png",
        ] {
            assert!(image_path_for_reference(reference).is_none());
        }
        let image = sample_image();
        assert_eq!(image.format, gpui::ImageFormat::Png);
        assert_eq!(
            image_from_bytes(image.bytes.clone(), Some(gpui::ImageFormat::Jpeg))
                .unwrap()
                .format,
            gpui::ImageFormat::Png
        );
        assert!(image_from_bytes(Vec::new(), Some(gpui::ImageFormat::Png)).is_none());
        assert!(image_from_bytes(vec![1, 2, 3], None).is_none());
    }

    #[gpui::test]
    fn image_menu_actions_copy_the_actual_image_and_keep_the_requested_order(
        cx: &mut gpui::TestAppContext,
    ) {
        struct Harness;
        impl Render for Harness {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }
        let image = sample_image();
        let added = Rc::new(Cell::new(0));
        let revealed = Rc::new(Cell::new(0));
        let add_count = added.clone();
        let reveal_count = revealed.clone();
        let items = image_action_menu_items(
            &image,
            Rc::new(move |_, _| add_count.set(add_count.get() + 1)),
            Rc::new(move |_, _| reveal_count.set(reveal_count.get() + 1)),
        );
        let labels = items
            .iter()
            .map(|item| match item {
                MenuItem::Entry {
                    label, disabled, ..
                } => {
                    assert!(!disabled);
                    label.to_string()
                }
                _ => panic!("image menu should contain exactly three actions"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                tr!("common.add_to_chat"),
                tr!("common.copy_image"),
                image_reveal_label()
            ]
        );
        let (_, window) = cx.add_window_view(|_, _| Harness);
        window.update(|window, cx| {
            for item in items {
                if let MenuItem::Entry { on_click, .. } = item {
                    on_click(window, cx);
                }
            }
        });
        assert_eq!(added.get(), 1);
        assert_eq!(revealed.get(), 1);
        let clipboard = window.update(|_, cx| cx.read_from_clipboard()).unwrap();
        assert!(
            matches!(clipboard.entries(), [ClipboardEntry::Image(copied)] if copied == image.as_ref())
        );
    }
}
