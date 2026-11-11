use super::*;

use chrono::{Datelike, Days};
use std::path::Path;

const USER_MESSAGE_MAX_HEIGHT: f32 = 400.0;
const USER_MESSAGE_VIEWPORT_MAX_HEIGHT: f32 = USER_MESSAGE_MAX_HEIGHT - 16.0;

pub(super) fn pulse_dot(size: f32, color: Hsla) -> AnyElement {
    motion::pulse(Duration::from_millis(1600), move |phase| {
        div()
            .w(px(size))
            .h(px(size))
            .flex_none()
            .rounded_full()
            .bg(color)
            .opacity(pulsating_between(0.3, 1.0)(phase))
            .into_any_element()
    })
    // Mounted for whole activities; its pane must not tick at full rate.
    .every(2)
    .into_any_element()
}

/// Four dots in a 2×2 square chasing a brightness wave, the transcript's
/// "still working" signal. Each dot rides the shared pulse clock with a phase
/// offset, so the bright spot circles the square clockwise. Under
/// reduce-motion the clock holds the cycle's first frame — the lead dot
/// bright, the rest dimming behind it — which reads as a static glyph.
pub(super) fn working_wave_dots(color: Hsla) -> AnyElement {
    const DOT_PHASE_STEP: f32 = 0.25;
    const DOT_SIZE: f32 = 4.0;
    const DOT_GAP: f32 = 2.5;
    motion::pulse(Duration::from_millis(1400), move |phase| {
        // Clockwise order: top-left, top-right, bottom-right, bottom-left.
        let dot = |index: usize| {
            let dot_phase = (phase + 1.0 - index as f32 * DOT_PHASE_STEP) % 1.0;
            let wave = ((dot_phase * std::f32::consts::TAU).sin() + 1.0) / 2.0;
            div()
                .size(px(DOT_SIZE))
                .flex_none()
                .rounded_full()
                .bg(color)
                .opacity(0.25 + 0.75 * wave)
        };
        let row = |left: usize, right: usize| {
            div()
                .flex()
                .gap(px(DOT_GAP))
                .child(dot(left))
                .child(dot(right))
        };
        div()
            // Align with the visible text rather than the center of its line box.
            .relative()
            .top(px(1.5))
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(DOT_GAP))
            .child(row(0, 1))
            .child(row(3, 2))
            .into_any_element()
    })
    // Mounted for the whole turn: this is what sets the transcript pane's
    // tick floor, and every tick rebuilds each visible row. The 1400 ms wave
    // reads identically at half cadence.
    .every(2)
    .into_any_element()
}

pub(super) fn format_message_time(created_at: u64) -> String {
    format_message_time_at(created_at, Local::now())
}

fn format_message_time_at(created_at: u64, now: DateTime<Local>) -> String {
    let Ok(seconds) = i64::try_from(created_at) else {
        return String::new();
    };
    DateTime::<Utc>::from_timestamp(seconds, 0)
        .map(|timestamp| {
            let timestamp = timestamp.with_timezone(&Local);
            let message_date = timestamp.date_naive();
            let today = now.date_naive();
            if crate::i18n::uses_east_asian_date_format() {
                let time = timestamp.format("%H:%M").to_string();
                if message_date >= today {
                    return time;
                }
                if today.pred_opt() == Some(message_date) {
                    return tr!("time.yesterday_at", time = time);
                }
                let week_start = today
                    .checked_sub_days(Days::new(today.weekday().num_days_from_monday().into()))
                    .unwrap_or(today);
                if message_date >= week_start {
                    let weekday = match timestamp.weekday() {
                        chrono::Weekday::Mon => tr!("time.monday"),
                        chrono::Weekday::Tue => tr!("time.tuesday"),
                        chrono::Weekday::Wed => tr!("time.wednesday"),
                        chrono::Weekday::Thu => tr!("time.thursday"),
                        chrono::Weekday::Fri => tr!("time.friday"),
                        chrono::Weekday::Sat => tr!("time.saturday"),
                        chrono::Weekday::Sun => tr!("time.sunday"),
                    };
                    return tr!("time.weekday_at", weekday = weekday, time = time);
                }
                if message_date.year() == today.year() {
                    return tr!(
                        "time.date_at",
                        month = timestamp.month(),
                        day = timestamp.day(),
                        time = time
                    );
                }
                return tr!(
                    "time.full_date_at",
                    year = timestamp.year(),
                    month = timestamp.month(),
                    day = timestamp.day(),
                    time = time
                );
            }
            let time = timestamp
                .format("%I:%M %p")
                .to_string()
                .trim_start_matches('0')
                .to_owned();

            if message_date >= today {
                return time;
            }

            if today.pred_opt() == Some(message_date) {
                return tr!("time.yesterday_at", time = time);
            }

            let week_start = today
                .checked_sub_days(Days::new(today.weekday().num_days_from_monday().into()))
                .unwrap_or(today);
            if message_date >= week_start {
                return format!("{} {time}", timestamp.format("%A"));
            }

            let day = timestamp.day();
            let ordinal_suffix = match day % 100 {
                11..=13 => "th",
                _ => match day % 10 {
                    1 => "st",
                    2 => "nd",
                    3 => "rd",
                    _ => "th",
                },
            };
            let date = if message_date.year() == today.year() {
                format!("{} {day}{ordinal_suffix}", timestamp.format("%b"))
            } else {
                format!(
                    "{} {day}{ordinal_suffix} {}",
                    timestamp.format("%b"),
                    timestamp.year()
                )
            };
            format!("{date}, {time}")
        })
        .unwrap_or_default()
}

impl Raku {
    pub(super) fn control_was_copied(&self, control_id: &str) -> bool {
        self.copied_control_feedback.contains_key(control_id)
    }

    pub(super) fn show_control_copied(
        &mut self,
        control_id: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let control_id = control_id.into();
        self.copied_control_generation = self.copied_control_generation.wrapping_add(1);
        let generation = self.copied_control_generation;
        self.copied_control_feedback
            .insert(control_id.clone(), generation);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |this, cx| {
                if this.copied_control_feedback.get(&control_id) == Some(&generation) {
                    this.copied_control_feedback.remove(&control_id);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn show_message_copied(&mut self, message_id: Uuid, cx: &mut Context<Self>) {
        self.copied_message_generation = self.copied_message_generation.wrapping_add(1);
        let generation = self.copied_message_generation;
        self.copied_message_feedback.insert(message_id, generation);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |this, cx| {
                if this.copied_message_feedback.get(&message_id) == Some(&generation) {
                    this.copied_message_feedback.remove(&message_id);
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) struct MessageFooterControls {
    pub copy: FocusHandle,
    pub fork: FocusHandle,
    pub retry: FocusHandle,
}

pub(super) fn render_message_footer(
    theme: &Theme,
    message: &Message,
    footer_time: u64,
    copy_content: SharedString,
    copied: bool,
    group_name: SharedString,
    force_visible: bool,
    align_right: bool,
    assistant_message_action: Option<AssistantMessageAction>,
    retry_action: Option<UserMessageAction>,
    user_message_control: Option<UserMessageControl>,
    model_label: Option<SharedString>,
    controls: Option<MessageFooterControls>,
    raku: gpui::WeakEntity<Raku>,
) -> AnyElement {
    let theme = *theme;
    let message_id = message.id;
    let copy_raku = raku.clone();
    let footer_color = if theme.is_dark {
        gpui::hsla(126.93 / 360.0, 0.000_000_1, 0.543_95, 1.0)
    } else {
        theme.text_secondary
    };
    let timestamp = div()
        .flex_none()
        .h(px(27.0))
        .px(px(4.0))
        .flex()
        .items_center()
        .text_size(sp(SMALL_UI_FONT_SIZE))
        .line_height(sp(14.0))
        .text_color(footer_color)
        .child(format_message_time(footer_time));
    let key_copy_content = copy_content.clone();
    let key_copy_raku = copy_raku.clone();
    let copy_button = div()
        .id(SharedString::from(format!("copy-message-{message_id}")))
        .w(px(27.0))
        .h(px(27.0))
        .flex_none()
        .rounded(px(8.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_default()
        .when_some(controls.as_ref(), |element, controls| {
            element.track_focus(&controls.copy).tab_group().tab_index(0)
        })
        .focus_visible(|element| element.bg(theme.overlay_strong))
        .hover(|element| element.bg(theme.overlay_strong))
        .child(icon(
            if copied {
                "icons/check.svg"
            } else {
                "icons/copy.svg"
            },
            14.0,
            footer_color,
        ))
        .tooltip(Tooltip::text(if copied {
            tr!("common.copied")
        } else {
            tr!("common.copy_message")
        }))
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_content.to_string()));
            let _ = copy_raku.update(cx, |this, cx| {
                this.show_message_copied(message_id, cx);
            });
        })
        .on_key_down(move |event: &KeyDownEvent, _, cx| {
            if !event.keystroke.modifiers.modified()
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                cx.write_to_clipboard(ClipboardItem::new_string(key_copy_content.to_string()));
                let _ = key_copy_raku.update(cx, |this, cx| {
                    this.show_message_copied(message_id, cx);
                });
                cx.stop_propagation();
            }
        });
    let mut footer = div()
        .w_full()
        .min_w_0()
        .h(px(27.0))
        .flex()
        .items_center()
        .gap(px(1.0))
        .when(!force_visible, |element| {
            element
                // Opacity keeps keyboard targets registered while the footer
                // is hidden; a focused action reveals it on the next frame.
                .opacity(0.0)
                .group_hover(group_name, |element| element.opacity(1.0))
        })
        .when(!align_right, |element| element.ml(-px(7.0)))
        .when(align_right, |element| element.justify_end());

    if align_right {
        footer = footer.child(timestamp).child(copy_button);
    } else {
        footer = footer.child(copy_button);
        if let Some(target) = retry_action {
            let retry_raku = raku.clone();
            let key_retry_raku = raku.clone();
            footer = footer.child(
                div()
                    .id(SharedString::from(format!("retry-turn-{message_id}")))
                    .w(px(27.0))
                    .h(px(27.0))
                    .flex_none()
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_default()
                    .when_some(controls.as_ref(), |element, controls| {
                        element
                            .track_focus(&controls.retry)
                            .tab_group()
                            .tab_index(0)
                    })
                    .focus_visible(|element| element.bg(theme.overlay_strong))
                    .hover(|element| element.bg(theme.overlay_strong))
                    .child(icon("icons/rotate-cw.svg", 14.0, footer_color))
                    .tooltip(Tooltip::text(tr!("common.retry")))
                    .on_click(move |_, _, cx| {
                        let _ = retry_raku.update(cx, |this, cx| {
                            this.retry_failed_turn(target, cx);
                        });
                    })
                    .on_key_down(move |event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            let _ = key_retry_raku.update(cx, |this, cx| {
                                this.retry_failed_turn(target, cx);
                            });
                            cx.stop_propagation();
                        }
                    }),
            );
        }
        if let Some(action) = assistant_message_action {
            let fork_raku = raku.clone();
            let key_fork_raku = raku.clone();
            let fork_icon = if action.preparing {
                motion::spin(icon("icons/loader-circle.svg", 14.0, footer_color))
            } else {
                icon("icons/fork.svg", 14.0, footer_color).into_any_element()
            };
            let fork_button = div()
                .id(SharedString::from(format!("fork-response-{message_id}")))
                .w(px(27.0))
                .h(px(27.0))
                .flex_none()
                .rounded(px(8.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_default()
                .when_some(controls.as_ref(), |element, controls| {
                    element
                        .track_focus(&controls.fork)
                        .tab_group()
                        .tab_index(0)
                        .tab_stop(action.enabled)
                })
                .focus_visible(|element| element.bg(theme.overlay_strong))
                .when(!action.enabled && !action.preparing, |element| {
                    element.opacity(0.45)
                })
                .child(fork_icon)
                .tooltip(Tooltip::text(if action.enabled {
                    tr_cow!("session.fork_task")
                } else {
                    tr_cow!("session.forking_task")
                }));
            footer = footer.child(if action.enabled {
                fork_button
                    .hover(|element| element.bg(theme.overlay_strong))
                    .on_click(move |_, _, cx| {
                        let _ = fork_raku.update(cx, |this, cx| {
                            this.fork_session_from_response(
                                action.session_id,
                                action.turn_count,
                                cx,
                            );
                        });
                    })
                    .on_key_down(move |event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            let _ = key_fork_raku.update(cx, |this, cx| {
                                this.fork_session_from_response(
                                    action.session_id,
                                    action.turn_count,
                                    cx,
                                );
                            });
                            cx.stop_propagation();
                        }
                    })
            } else {
                fork_button
            });
        }
        footer = footer
            .when_some(model_label, |element, label| {
                element.child(
                    div()
                        .min_w_0()
                        .px(px(4.0))
                        .text_size(sp(SMALL_UI_FONT_SIZE))
                        .line_height(sp(14.0))
                        .text_color(footer_color)
                        .truncate()
                        .child(label),
                )
            })
            .child(timestamp);
    }

    if let Some(UserMessageControl { action, focus }) = user_message_control {
        let edit_raku = raku.clone();
        footer = footer.child(
            div()
                .id(SharedString::from(format!(
                    "user-message-action-{message_id}"
                )))
                .track_focus(&focus)
                .tab_group()
                .tab_index(0)
                .w(px(27.0))
                .h(px(27.0))
                .rounded(px(8.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_default()
                .focus_visible(|element| element.bg(theme.overlay_strong))
                .hover(|element| element.bg(theme.overlay_strong))
                .child(icon("icons/pencil.svg", 14.0, footer_color))
                .tooltip(Tooltip::text(tr_cow!("session.edit_prompt")))
                .on_click(move |_, window, cx| {
                    let _ = edit_raku.update(cx, |this, cx| {
                        this.begin_message_edit(action, window, cx);
                    });
                })
                .on_key_down(move |event: &KeyDownEvent, window, cx| {
                    if !event.keystroke.modifiers.modified()
                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        let _ = raku.update(cx, |this, cx| {
                            this.begin_message_edit(action, window, cx);
                        });
                        cx.stop_propagation();
                    }
                }),
        );
    }

    footer.into_any_element()
}

pub(super) struct UserMessageControl {
    pub(super) action: UserMessageAction,
    pub(super) focus: FocusHandle,
}

pub(super) struct MessageEditRender {
    pub(super) input: Entity<ComposerInput>,
    pub(super) cancel_focus: FocusHandle,
    pub(super) submit_focus: FocusHandle,
}

/// Everything one transcript message row needs to render itself. Bundled
/// because these travel together from `transcript_row` and nowhere else.
pub(super) struct MessageRender<'a> {
    pub(super) theme: &'a Theme,
    pub(super) message: &'a Message,
    pub(super) assistant_footer_copy_content: Option<SharedString>,
    pub(super) assistant_footer_time: Option<u64>,
    pub(super) copied: bool,
    pub(super) assistant_message_action: Option<AssistantMessageAction>,
    pub(super) user_message_control: Option<UserMessageControl>,
    pub(super) user_message_viewport: Option<&'a UserMessageScrollViewport>,
    pub(super) message_edit: Option<MessageEditRender>,
    pub(super) attachment_menus: Vec<ContextMenuHandle>,
    pub(super) attachment_images: Vec<Option<Arc<gpui::Image>>>,
    /// Captured from the selected daemon before the virtualized row is built.
    /// A row is laid out while the root `Raku` entity is already updating, so
    /// it must not read that entity again just to decide whether Finder reveal
    /// is available.
    pub(super) attachments_can_reveal: bool,
    /// The parsed human or assistant body. System messages remain verbatim.
    pub(super) markdown: Option<&'a MarkdownView>,
    pub(super) ctx: &'a MarkdownCtx<'a>,
    pub(super) menu: ContextMenuHandle,
    pub(super) raku: gpui::WeakEntity<Raku>,
    /// Whether a failed turn's provider detail is disclosed, and the focus
    /// handle of its disclosure control.
    pub(super) error_expanded: bool,
    pub(super) error_focus: FocusHandle,
}

/// The one-line chip a pasted text attachment renders as: a document icon and
/// the snippet's first line, truncated. Callers add identity, focus, and click
/// handling; the composer adds its remove control inside the same row.
pub(super) fn pasted_text_chip(theme: &Theme, title: &SharedString) -> Div {
    text_attachment_chip(theme, title, "icons/file.svg")
}

pub(super) fn session_link_chip(theme: &Theme, title: &SharedString) -> Div {
    text_attachment_chip(theme, title, "icons/external-link.svg")
}

fn text_attachment_chip(theme: &Theme, title: &SharedString, icon_path: &'static str) -> Div {
    div()
        .h(px(28.0))
        .min_h(px(28.0))
        .max_w(px(320.0))
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .bg(theme.inset)
        .px(px(9.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .cursor_default()
        .hover(|element| element.bg(theme.overlay))
        .child(icon(icon_path, 13.0, theme.text_secondary))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_size(sp(13.0))
                .text_color(theme.text)
                .child(title.clone()),
        )
}

/// Stable identity for a pasted text attachment's right-panel tab. The daemon
/// attachment a snippet was materialized into is what a reopened draft or a
/// sent message still carries, so the same chip keeps landing on one tab; an
/// attachment with no reference falls back to a hash of its text, which keeps
/// repeated clicks on it agreeing too.
pub(super) fn pasted_text_view_id(reference: Option<&str>, text: &str) -> Uuid {
    use std::hash::{Hash as _, Hasher as _};

    reference
        .and_then(|reference| reference.strip_prefix(raku_protocol::attachments::ATTACHMENT_SCHEME))
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hasher);
            Uuid::from_u128(hasher.finish().into())
        })
}

fn render_sent_message_attachments(
    message_id: Uuid,
    attachments: &[MessageAttachment],
    attachment_menus: &[ContextMenuHandle],
    attachment_images: &[Option<Arc<gpui::Image>>],
    can_reveal: bool,
    raku: &gpui::WeakEntity<Raku>,
    theme: &Theme,
) -> Option<AnyElement> {
    if attachments.is_empty() {
        return None;
    }
    let mut row = div()
        .max_w(px(540.0))
        .flex()
        .flex_wrap()
        .justify_end()
        .gap(px(8.0));
    for (index, attachment) in attachments.iter().enumerate() {
        let Some(menu) = attachment_menus.get(index) else {
            continue;
        };
        if attachment.pasted_text.is_some() || attachment.session_id.is_some() {
            let text = attachment.pasted_text.as_deref().unwrap_or_default();
            let session_id = attachment.session_id;
            let name = SharedString::from(attachment.name.clone());
            let id = session_id
                .unwrap_or_else(|| pasted_text_view_id(attachment.blob_reference.as_deref(), text));
            let click_name = name.clone();
            let click_text = SharedString::from(text);
            let key_name = name.clone();
            let key_text = SharedString::from(text);
            let click_raku = raku.clone();
            let key_raku = raku.clone();
            let key_menu = menu.clone();
            let chip = if session_id.is_some() {
                session_link_chip(theme, &name)
            } else {
                pasted_text_chip(theme, &name)
            }
            .id(SharedString::from(format!(
                "message-{message_id}-attachment-{index}"
            )))
            .track_focus(menu.trigger_focus_handle())
            .tab_index(0)
            .focus_visible(|style| style.border_color(theme.accent))
            .tooltip(Tooltip::text(if session_id.is_some() {
                tr!("session.open", name = name)
            } else {
                name.to_string()
            }))
            .on_click(move |_, _, cx| {
                let _ = click_raku.update(cx, |this, cx| {
                    this.open_text_attachment(
                        session_id,
                        id,
                        click_name.clone(),
                        click_text.clone(),
                        cx,
                    );
                });
                cx.stop_propagation();
            })
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                if matches!(key, "enter" | "space") {
                    let _ = key_raku.update(cx, |this, cx| {
                        this.open_text_attachment(
                            session_id,
                            id,
                            key_name.clone(),
                            key_text.clone(),
                            cx,
                        );
                    });
                    cx.stop_propagation();
                } else if key == "f10" && event.keystroke.modifiers.shift {
                    key_menu.open_context_menu(window, cx);
                    cx.stop_propagation();
                }
            });
            let reveal_path = attachment.path.clone();
            let menu_raku = raku.clone();
            let preview_name = session_id.map(|_| format!("{}.md", attachment.name));
            let preview_file = right_panel::AttachmentFile::for_preview(
                &attachment.path,
                attachment.blob_reference.as_deref(),
                preview_name.as_deref().unwrap_or(&attachment.name),
                attachment.is_dir,
                attachment.is_image,
            );
            row = row.child(context_menu(
                chip,
                SharedString::from(format!("message-{message_id}-attachment-{index}-menu")),
                menu,
                move |_| {
                    let mut items = session_id
                        .map(|session_id| {
                            super::session_links::session_link_menu_items(
                                session_id,
                                preview_file.clone(),
                                menu_raku.clone(),
                            )
                        })
                        .unwrap_or_default();
                    items.extend(image_preview::attachment_menu_items(
                        reveal_path.clone(),
                        can_reveal,
                    ));
                    items
                },
            ));
            continue;
        }
        let icon_path = if attachment.is_dir {
            "icons/folder.svg"
        } else {
            right_panel::file_icon_for_path(&attachment.mention)
        };
        let preview_file = right_panel::AttachmentFile::for_preview(
            &attachment.path,
            attachment.blob_reference.as_deref(),
            &attachment.name,
            attachment.is_dir,
            attachment.is_image,
        );
        let attachment_image = attachment_images.get(index).and_then(|image| image.clone());
        let mut tile = div()
            .id(SharedString::from(format!(
                "message-{message_id}-attachment-{index}"
            )))
            .w(px(96.0))
            .h(px(80.0))
            .rounded(px(9.0))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(theme.inset)
            .track_focus(menu.trigger_focus_handle())
            .tab_index(0)
            .focus_visible(|style| style.border_color(theme.accent))
            .tooltip(Tooltip::text(if preview_file.is_some() {
                tr!("attachments.open_preview", name = attachment.name)
            } else {
                attachment.name.clone()
            }));
        if attachment.is_image {
            let key_menu = menu.clone();
            if let Some(attachment_image) = attachment_image.as_ref() {
                let preview_raku = raku.clone();
                let key_raku = raku.clone();
                let preview_image = attachment_image.clone();
                let key_image = attachment_image.clone();
                let preview_name = SharedString::from(attachment.name.clone());
                let key_name = preview_name.clone();
                tile = tile.child(
                    div()
                        .id(SharedString::from(format!(
                            "message-{message_id}-attachment-{index}-preview"
                        )))
                        .size_full()
                        .cursor_default()
                        .on_click(move |_, window, cx| {
                            let _ = preview_raku.update(cx, |this, cx| {
                                this.open_image_preview(
                                    preview_image.clone(),
                                    preview_name.clone(),
                                    window,
                                    cx,
                                );
                            });
                            cx.stop_propagation();
                        })
                        .child(
                            img(attachment_image.clone())
                                .size_full()
                                .object_fit(ObjectFit::Cover),
                        ),
                );
                tile = tile.on_key_down(move |event: &KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    if matches!(key, "enter" | "space") {
                        let _ = key_raku.update(cx, |this, cx| {
                            this.open_image_preview(
                                key_image.clone(),
                                key_name.clone(),
                                window,
                                cx,
                            );
                        });
                        cx.stop_propagation();
                    } else if key == "f10" && event.keystroke.modifiers.shift {
                        key_menu.open_context_menu(window, cx);
                        cx.stop_propagation();
                    }
                });
            } else {
                tile = tile
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon("icons/file-types/image.svg", 18.0, theme.text_ghost)),
                    )
                    .on_key_down(move |event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "f10" && event.keystroke.modifiers.shift {
                            key_menu.open_context_menu(window, cx);
                            cx.stop_propagation();
                        }
                    });
            }
        } else {
            let key_menu = menu.clone();
            if let Some(preview_file) = preview_file.clone() {
                let click_raku = raku.clone();
                tile = tile
                    .cursor_default()
                    .hover(|style| style.bg(theme.overlay))
                    .on_click(move |_, _, cx| {
                        let _ = click_raku.update(cx, |this, cx| {
                            this.open_attachment_file(preview_file.clone(), cx);
                        });
                        cx.stop_propagation();
                    });
            }
            tile = tile.child(
                div()
                    .size_full()
                    .px(px(7.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(7.0))
                    .child(icon(icon_path, 18.0, theme.text_tertiary))
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_center()
                            .text_size(sp(SMALL_UI_FONT_SIZE))
                            .text_color(theme.text_secondary)
                            .child(attachment.name.clone()),
                    ),
            );
            let key_raku = raku.clone();
            let key_file = preview_file;
            tile = tile.on_key_down(move |event: &KeyDownEvent, window, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                    && let Some(preview_file) = key_file.as_ref()
                {
                    let _ = key_raku.update(cx, |this, cx| {
                        this.open_attachment_file(preview_file.clone(), cx);
                    });
                    cx.stop_propagation();
                } else if event.keystroke.key == "f10" && event.keystroke.modifiers.shift {
                    key_menu.open_context_menu(window, cx);
                    cx.stop_propagation();
                }
            });
        }
        let reveal_path = attachment.path.clone();
        row = row.child(context_menu(
            tile,
            SharedString::from(format!("message-{message_id}-attachment-{index}-menu")),
            menu,
            move |_| image_preview::attachment_menu_items(reveal_path.clone(), can_reveal),
        ));
    }
    Some(row.into_any_element())
}

fn render_markdown_message_body<'a>(
    content: &str,
    markdown: Option<&'a MarkdownView>,
    theme: &Theme,
    ctx: &MarkdownCtx<'a>,
) -> AnyElement {
    markdown
        .and_then(|markdown| md::render::markdown(markdown, ctx))
        // Empty or not-yet-parsed content still needs a selectable fallback.
        .unwrap_or_else(|| {
            md::render::plain_text(
                content.to_owned(),
                md::render::SANS_FAMILY,
                FontWeight::NORMAL,
                theme.text,
                ctx,
            )
        })
}

/// A failed turn's reply: the plain headline in error style, with the
/// provider's own reason one disclosure away for anyone who wants it.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_error_reply(
    message_id: Uuid,
    headline: &str,
    error: &crate::model::MessageError,
    expanded: bool,
    focus: FocusHandle,
    raku: gpui::WeakEntity<Raku>,
    theme: &Theme,
    ctx: &MarkdownCtx,
) -> AnyElement {
    let headline_row = div()
        .w_full()
        .min_w_0()
        .flex()
        .items_start()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .pt(px(3.0))
                .child(icon("icons/alert.svg", 14.0, theme.danger)),
        )
        .child(div().min_w_0().flex_1().child(md::render::plain_text(
            headline.to_owned(),
            md::render::SANS_FAMILY,
            FontWeight::NORMAL,
            theme.danger,
            ctx,
        )));
    let Some(detail) = error.detail.clone() else {
        return headline_row.into_any_element();
    };
    let click_raku = raku.clone();
    let key_raku = raku;
    let toggle = div()
        .id(SharedString::from(format!(
            "message-error-details-{message_id}"
        )))
        .track_focus(&focus)
        .tab_index(0)
        .ml(px(18.0))
        .h(px(22.0))
        .px(px(4.0))
        .rounded(px(5.0))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(3.0))
        .cursor_pointer()
        .text_size(sp(12.0))
        .text_color(theme.text_tertiary)
        .hover(|style| style.bg(theme.overlay).text_color(theme.text_secondary))
        .focus_visible(|style| style.border_1().border_color(theme.accent))
        .child(icon(
            if expanded {
                "icons/chevron-down.svg"
            } else {
                "icons/chevron-right.svg"
            },
            11.0,
            theme.text_tertiary,
        ))
        .child(if expanded {
            tr!("message_error.hide_details")
        } else {
            tr!("message_error.details")
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            let _ = click_raku.update(cx, |this, cx| this.toggle_message_error(message_id, cx));
        })
        .on_key_down(move |event: &KeyDownEvent, _, cx| {
            if !event.keystroke.modifiers.modified()
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                cx.stop_propagation();
                let _ = key_raku.update(cx, |this, cx| this.toggle_message_error(message_id, cx));
            }
        });
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .items_start()
        .gap(px(4.0))
        .child(headline_row)
        .child(toggle)
        .when(expanded, |column| {
            column.child(
                div()
                    .ml(px(22.0))
                    .w_full()
                    .min_w_0()
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .bg(theme.danger_soft)
                    .text_size(sp(12.0))
                    .line_height(sp(18.0))
                    .child(md::render::plain_text(
                        detail,
                        md::render::MONO_FAMILY,
                        FontWeight::NORMAL,
                        theme.text_secondary,
                        ctx,
                    )),
            )
        })
        .into_any_element()
}

pub(super) fn render_message(
    params: MessageRender,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let MessageRender {
        theme,
        message,
        assistant_footer_copy_content,
        assistant_footer_time,
        copied,
        assistant_message_action,
        user_message_control,
        user_message_viewport,
        message_edit,
        attachment_menus,
        attachment_images,
        attachments_can_reveal,
        markdown,
        ctx,
        menu,
        raku,
        error_expanded,
        error_focus,
    } = params;

    let content = message.visible_content().to_owned();
    let selection = ctx.selection().clone();
    let message_id = message.id;
    let role = message.role;
    let user_footer_focused = menu.trigger_focus_handle().is_focused(window)
        || attachment_menus
            .iter()
            .any(|menu| menu.trigger_focus_handle().is_focused(window))
        || user_message_control
            .as_ref()
            .is_some_and(|control| control.focus.is_focused(window));
    let element = match role {
        MessageRole::User => {
            let group_name = SharedString::from(format!("user-message-{message_id}"));
            let mut column = div()
                .w_full()
                .flex()
                .flex_col()
                .items_end()
                .gap(px(3.0))
                .group(group_name.clone());
            if let Some(attachments) = render_sent_message_attachments(
                message_id,
                &message.attachments,
                &attachment_menus,
                &attachment_images,
                attachments_can_reveal,
                &raku,
                theme,
            ) {
                column = column.child(attachments);
            }
            if let Some(edit) = message_edit {
                let can_submit = !edit.input.read(cx).content(cx).trim().is_empty()
                    || !message.attachments.is_empty();
                let cancel_raku = raku.clone();
                let cancel_key_raku = raku.clone();
                let submit_raku = raku.clone();
                let submit_key_raku = raku.clone();
                column = column.child(
                    div()
                        .w_full()
                        .max_w(px(540.0))
                        .tab_group()
                        .rounded(px(12.0))
                        .bg(theme.raised)
                        .pt(px(9.0))
                        .pb(px(8.0))
                        .child(edit.input)
                        .child(
                            div()
                                .mt(px(7.0))
                                .px(px(12.0))
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "cancel-message-edit-{message_id}"
                                        )))
                                        .track_focus(&edit.cancel_focus)
                                        .tab_index(0)
                                        .h(px(26.0))
                                        .px(px(10.0))
                                        .rounded(px(7.0))
                                        .border_1()
                                        .border_color(theme.border)
                                        .bg(theme.overlay)
                                        .flex()
                                        .items_center()
                                        .text_size(sp(SMALL_UI_FONT_SIZE))
                                        .text_color(theme.text_secondary)
                                        .cursor_default()
                                        .focus_visible(|element| element.border_color(theme.accent))
                                        .hover(|element| element.bg(theme.overlay_strong))
                                        .child(tr_cow!("common.cancel"))
                                        .on_click(move |_, window, cx| {
                                            let _ = cancel_raku.update(cx, |this, cx| {
                                                this.cancel_message_edit(window, cx);
                                            });
                                        })
                                        .on_key_down(move |event: &KeyDownEvent, window, cx| {
                                            if !event.keystroke.modifiers.modified()
                                                && matches!(
                                                    event.keystroke.key.as_str(),
                                                    "enter" | "space"
                                                )
                                            {
                                                let _ = cancel_key_raku.update(cx, |this, cx| {
                                                    this.cancel_message_edit(window, cx);
                                                });
                                                cx.stop_propagation();
                                            }
                                        }),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "submit-message-edit-{message_id}"
                                        )))
                                        .track_focus(&edit.submit_focus)
                                        .tab_index(0)
                                        .h(px(26.0))
                                        .px(px(11.0))
                                        .rounded(px(7.0))
                                        .border_1()
                                        .border_color(if can_submit {
                                            theme.inverse
                                        } else {
                                            theme.overlay_strong
                                        })
                                        .bg(if can_submit {
                                            theme.inverse
                                        } else {
                                            theme.overlay_strong
                                        })
                                        .flex()
                                        .items_center()
                                        .text_size(sp(SMALL_UI_FONT_SIZE))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(if can_submit {
                                            theme.on_inverse
                                        } else {
                                            theme.text_ghost
                                        })
                                        .focus_visible(|element| element.border_color(theme.accent))
                                        .when(can_submit, |element| {
                                            element
                                                .cursor_default()
                                                .hover(|element| element.opacity(0.9))
                                        })
                                        .child(tr_cow!("session.edit_and_resend"))
                                        .on_click(move |_, _, cx| {
                                            if can_submit {
                                                let _ = submit_raku.update(cx, |this, cx| {
                                                    this.submit_message_edit(cx);
                                                });
                                            }
                                        })
                                        .on_key_down(move |event: &KeyDownEvent, _, cx| {
                                            if can_submit
                                                && !event.keystroke.modifiers.modified()
                                                && matches!(
                                                    event.keystroke.key.as_str(),
                                                    "enter" | "space"
                                                )
                                            {
                                                let _ = submit_key_raku.update(cx, |this, cx| {
                                                    this.submit_message_edit(cx);
                                                });
                                                cx.stop_propagation();
                                            }
                                        }),
                                ),
                        ),
                );
            } else {
                if !content.trim().is_empty() {
                    let body = render_markdown_message_body(&content, markdown, theme, ctx);
                    column = column.child(
                        div()
                            .id(SharedString::from(format!(
                                "user-message-bubble-{message_id}"
                            )))
                            .max_w(px(540.0))
                            .max_h(px(USER_MESSAGE_MAX_HEIGHT))
                            .min_w_0()
                            .relative()
                            .overflow_hidden()
                            .rounded(px(12.0))
                            .border_1()
                            .border_color(theme.raised)
                            .bg(theme.raised)
                            .px(px(3.0))
                            .py(px(7.0))
                            .text_size(sp(14.0))
                            .line_height(sp(20.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .relative()
                                    .max_h(px(USER_MESSAGE_VIEWPORT_MAX_HEIGHT))
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "user-message-scroll-{message_id}"
                                            )))
                                            .min_w_0()
                                            .max_h(px(USER_MESSAGE_VIEWPORT_MAX_HEIGHT))
                                            // Keep 8px of the bubble's 11px inset inside
                                            // both clipping ancestors. Inline pills can
                                            // then overhang the text at either row edge.
                                            .px(px(8.0))
                                            .when_some(user_message_viewport, |body, viewport| {
                                                let wheel_scroll = viewport.scroll_handle.clone();
                                                body.overflow_y_scroll()
                                                    .track_scroll(&viewport.scroll_handle)
                                                    .on_scroll_wheel(move |_, _, cx| {
                                                        contain_scroll(&wheel_scroll, cx);
                                                    })
                                            })
                                            .child(body),
                                    )
                                    // Keep the fades inside the bubble's padding so
                                    // square fade quads cannot paint over its rounded corners.
                                    .when_some(user_message_viewport, |body, viewport| {
                                        body.child(scrollbar::edge_fade(
                                            viewport.scroll_handle.clone(),
                                            scrollbar::FadeEdge::Top,
                                            theme.raised,
                                        ))
                                        .child(
                                            scrollbar::edge_fade(
                                                viewport.scroll_handle.clone(),
                                                scrollbar::FadeEdge::Bottom,
                                                theme.raised,
                                            ),
                                        )
                                    }),
                            )
                            .when_some(user_message_viewport, |bubble, viewport| {
                                let key_scroll = viewport.scroll_handle.clone();
                                let key_menu = menu.clone();
                                let key_selection = selection.clone();
                                let key_owner = raku.entity_id();
                                bubble
                                    .track_focus(menu.trigger_focus_handle())
                                    .tab_group()
                                    .tab_index(0)
                                    .focus_visible(|style| style.border_color(theme.accent))
                                    .on_key_down(move |event: &KeyDownEvent, window, cx| {
                                        if event.keystroke.key == "f10"
                                            && event.keystroke.modifiers.shift
                                        {
                                            // The menu only offers adding the
                                            // selection, so opening it with
                                            // nothing selected would show an
                                            // empty card.
                                            if key_selection
                                                .selection
                                                .borrow()
                                                .selected_text()
                                                .is_some()
                                            {
                                                key_menu.open_context_menu(window, cx);
                                            }
                                            cx.stop_propagation();
                                            return;
                                        }
                                        let modifiers = &event.keystroke.modifiers;
                                        if modifiers.control
                                            || modifiers.alt
                                            || modifiers.platform
                                            || (modifiers.shift && event.keystroke.key != "space")
                                        {
                                            return;
                                        }
                                        let max_offset = key_scroll.max_offset().y;
                                        if max_offset <= px(0.5) {
                                            return;
                                        }
                                        let offset = key_scroll.offset();
                                        let page = key_scroll.bounds().size.height * 0.9;
                                        let next = match event.keystroke.key.as_str() {
                                            "up" => offset.y + px(40.0),
                                            "down" => offset.y - px(40.0),
                                            "pageup" => offset.y + page,
                                            "pagedown" => offset.y - page,
                                            "home" => Pixels::ZERO,
                                            "end" => -max_offset,
                                            "space" if modifiers.shift => offset.y + page,
                                            "space" => offset.y - page,
                                            _ => return,
                                        }
                                        .clamp(-max_offset, Pixels::ZERO);
                                        if next != offset.y {
                                            key_scroll.set_offset(point(offset.x, next));
                                            cx.notify(key_owner);
                                        }
                                        cx.stop_propagation();
                                    })
                                    .child(
                                        scrollbar::vertical(
                                            &viewport.scroll_handle,
                                            &viewport.scrollbar,
                                        )
                                        .top(px(8.0))
                                        .bottom(px(8.0)),
                                    )
                            }),
                    );
                }
                column = column.child(render_message_footer(
                    theme,
                    message,
                    message.created_at,
                    SharedString::from(content.clone()),
                    copied,
                    group_name,
                    user_footer_focused,
                    true,
                    None,
                    None,
                    user_message_control,
                    None,
                    None,
                    raku.clone(),
                ));
            }
            column
        }
        MessageRole::Assistant => {
            let group_name = SharedString::from(format!("assistant-message-{message_id}"));
            let body = match &message.error {
                Some(error) => render_error_reply(
                    message_id,
                    &content,
                    error,
                    error_expanded,
                    error_focus,
                    raku.clone(),
                    theme,
                    ctx,
                ),
                None => render_markdown_message_body(&content, markdown, theme, ctx),
            };
            let mut column = div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .py(px(4.0))
                .gap(px(3.0))
                .group(group_name.clone())
                .child(body);
            // Turn-backed responses render one footer after every ordered row
            // in the turn. Only legacy/unkeyed assistant messages retain an
            // inline footer because they have no turn boundary to target.
            if message.turn_id.is_none()
                && let Some(copy_content) = assistant_footer_copy_content
            {
                column = column.child(render_message_footer(
                    theme,
                    message,
                    assistant_footer_time.unwrap_or(message.created_at),
                    copy_content,
                    copied,
                    group_name,
                    false,
                    false,
                    assistant_message_action,
                    None,
                    None,
                    None,
                    None,
                    raku.clone(),
                ));
            }
            column
        }
        MessageRole::System => div().w_full().flex().justify_center().child(
            div()
                .px(px(10.0))
                .py(px(4.0))
                .rounded_full()
                .bg(theme.overlay)
                .text_size(sp(SMALL_UI_FONT_SIZE))
                .line_height(sp(16.0))
                .child(md::render::plain_text(
                    content.clone(),
                    md::render::SANS_FAMILY,
                    FontWeight::NORMAL,
                    theme.text_tertiary,
                    ctx,
                )),
        ),
    };

    let menu_raku = raku.clone();
    context_menu(
        element.id(message_id),
        SharedString::from(format!("message-menu-{message_id}")),
        &menu,
        move |_cx| message_menu_items(message_id, &selection, &menu_raku),
    )
}

/// The message row's context menu. It offers one action — adding the current
/// transcript selection to the composer — and stages each selection as its own
/// pasted-text snippet, so repeated adds accumulate like repeated pastes. An
/// empty selection yields no menu at all.
///
/// "Side chat" asks a read-only side agent about the selection without
/// touching this task's conversation; the quote then links to that chat.
fn message_menu_items(
    message_id: Uuid,
    selection: &TranscriptSelection,
    raku: &gpui::WeakEntity<Raku>,
) -> Vec<MenuItem> {
    let Some(selected) = selection.selection.borrow().selected_text() else {
        return Vec::new();
    };
    let add_raku = raku.clone();
    let side_raku = raku.clone();
    vec![
        MenuItem::new(tr!("common.add_to_chat"), move |window, cx| {
            let text = selected.clone();
            let _ = add_raku.update(cx, |this, cx| {
                this.stage_pasted_text_snippet(text, cx);
                let focus = this.composer.read(cx).focus();
                window.focus(&focus, cx);
            });
        })
        .icon("icons/plus.svg"),
        MenuItem::new(tr!("side_chat.menu_item"), move |window, cx| {
            let _ = side_raku.update(cx, |this, cx| {
                this.start_side_chat_from_selection(message_id, window, cx);
            });
        })
        .icon("icons/ant/message-plus.svg"),
    ]
}

pub(super) fn activity_summary(activities: &[ActivityItem]) -> String {
    summarize_activities(activities.iter())
}

fn summarize_activities<'a>(activities: impl Iterator<Item = &'a ActivityItem> + Clone) -> String {
    // Subagent calls are tool calls, but are counted as what they are.
    let mut counts: Vec<((crate::model::ActivityKind, bool), usize)> = Vec::new();
    for activity in activities.clone() {
        let key = (activity.kind, activity.is_subagent());
        if let Some(entry) = counts.iter_mut().find(|(candidate, _)| *candidate == key) {
            entry.1 += 1;
        } else {
            counts.push((key, 1));
        }
    }
    let parts = counts
        .into_iter()
        .map(|((kind, subagent), count)| {
            let (singular, plural) = if subagent {
                (tr!("activity.subagent"), tr!("activity.subagents"))
            } else {
                activity_noun(kind)
            };
            tr!(
                "activity.count",
                count = count,
                activity = if count == 1 { singular } else { plural }
            )
        })
        .collect::<Vec<_>>();
    let running = activities.clone().any(|activity| !activity.complete);
    if running {
        tr!("activity.running", activities = parts.join(" · "))
    } else {
        tr!("activity.ran", activities = parts.join(" · "))
    }
}

/// What the live turn is doing right now, phrased for the working row: the
/// newest unfinished activity in the turn's tail block, or — in the gap
/// between one tool finishing and the next starting — the newest finished
/// one, so the label does not flash back to "Working" on every step. `None`
/// means the turn has no work at its tail (it has not started, or it is
/// writing text after its last activity).
pub(super) fn live_activity_label(session: &AgentSession) -> Option<String> {
    let turn_id = session.active_turn_id()?;
    let block = session.transcript_blocks.last()?;
    if block.turn_id != Some(turn_id) || block.after_message != session.messages.len() {
        return None;
    }
    let labeled = |activity: &&ActivityItem| {
        !matches!(activity.kind, ActivityKind::Plan | ActivityKind::Notice)
    };
    let activity = block
        .activities
        .iter()
        .rev()
        .filter(labeled)
        .find(|activity| !activity.complete)
        .or_else(|| block.activities.iter().rev().find(labeled))?;
    Some(match activity.kind {
        ActivityKind::Reasoning => tr!("transcript.thinking"),
        ActivityKind::Command => tr!("transcript.live_command"),
        ActivityKind::FileChange => tr!("transcript.live_file_change"),
        ActivityKind::FileRead => tr!("transcript.live_file_read"),
        ActivityKind::FileSearch => tr!("transcript.live_file_search"),
        ActivityKind::FileList => tr!("transcript.live_file_list"),
        ActivityKind::Search => tr!("transcript.live_search"),
        // Subagents run in parallel, so the row says when it is several.
        ActivityKind::Tool if activity.is_subagent() => {
            let running = block
                .activities
                .iter()
                .filter(|activity| activity.is_subagent() && !activity.complete)
                .count();
            if running > 1 {
                tr!("transcript.live_subagents")
            } else {
                tr!("transcript.live_subagent")
            }
        }
        ActivityKind::Tool => tr!("transcript.live_tool"),
        ActivityKind::ImageGeneration => tr!("activity.generating_image"),
        ActivityKind::Compaction => tr!("activity.compacting_context"),
        ActivityKind::Plan | ActivityKind::Notice => return None,
    })
}

/// The live turn's activity group at the transcript tail, if any. It renders
/// under the working row, which names and folds it, instead of in its own row.
pub(super) fn live_activity_block(session: &AgentSession) -> Option<usize> {
    let block_index = session.transcript_blocks.len().checked_sub(1)?;
    let block = &session.transcript_blocks[block_index];
    (!block.activities.is_empty()
        && activity_group_is_live(
            session
                .active_turn_id()
                .is_some_and(|turn_id| block.turn_id == Some(turn_id)),
            true,
            block.after_message,
            session.messages.len(),
        ))
    .then_some(block_index)
}

pub(super) fn activity_group_is_live(
    live_turn: bool,
    latest_block: bool,
    after_message: usize,
    message_count: usize,
) -> bool {
    live_turn && latest_block && after_message == message_count
}

pub(super) fn activity_header_title(
    activities: &[ActivityItem],
    live_group: bool,
    live_reasoning_id: Option<Uuid>,
) -> String {
    if live_group && let Some(activity) = activities.last() {
        return activity.reasoning.as_ref().map_or_else(
            || activity_display_title(activity),
            |reasoning| reasoning_activity_title(reasoning, live_reasoning_id == Some(activity.id)),
        );
    }

    // A settled group reads as what it did — "Ran 5 commands · 2 file
    // reads". Thinking only rides along with the work, so it is named only
    // when a group holds nothing else.
    let work = activities
        .iter()
        .filter(|activity| activity.kind != crate::model::ActivityKind::Reasoning);
    if work.clone().next().is_some() {
        return summarize_activities(work);
    }
    if let [activity] = activities
        && let Some(reasoning) = activity.reasoning.as_ref()
    {
        return reasoning_activity_title(reasoning, false);
    }
    activity_summary(activities)
}

fn tool_name_leaf(name: &str) -> &str {
    let name = name.trim();
    let leaf = name.rsplit("__").next().unwrap_or(name);
    leaf.rsplit([':', '.', '/']).next().unwrap_or(leaf)
}

fn is_ask_user_question(activity: &ActivityItem) -> bool {
    activity.kind == crate::model::ActivityKind::Tool
        && tool_name_leaf(&activity.title)
            .chars()
            .filter(|character| !matches!(*character, '_' | '-' | ' '))
            .flat_map(char::to_lowercase)
            .collect::<String>()
            == "askuserquestion"
}

fn humanize_tool_name(name: &str) -> String {
    let name = name.trim();
    if name.chars().any(char::is_whitespace) {
        return name.to_owned();
    }

    let leaf = tool_name_leaf(name);
    let characters = leaf.chars().collect::<Vec<_>>();
    let mut display = String::with_capacity(leaf.len() + 4);
    for (index, character) in characters.iter().copied().enumerate() {
        if matches!(character, '_' | '-') {
            if !display.ends_with(' ') {
                display.push(' ');
            }
            continue;
        }
        let previous = index.checked_sub(1).and_then(|index| characters.get(index));
        let next = characters.get(index + 1);
        let starts_word = character.is_ascii_uppercase()
            && previous.is_some_and(|previous| {
                previous.is_ascii_lowercase()
                    || previous.is_ascii_digit()
                    || (previous.is_ascii_uppercase()
                        && next.is_some_and(|next| next.is_ascii_lowercase()))
            });
        if starts_word && !display.ends_with(' ') {
            display.push(' ');
        }
        display.push(character);
    }

    let display = display.trim();
    let mut characters = display.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
        .unwrap_or_else(|| tr!("activity.tool"))
}

/// A `subagent` call as the row names it: the agent, then the label its
/// model gave the delegated work.
fn subagent_activity_label(activity: &ActivityItem) -> String {
    let part = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    match (
        part(&activity.display_target),
        part(&activity.display_description),
    ) {
        (Some(agent), Some(description)) => format!("{agent} · {description}"),
        (Some(label), None) | (None, Some(label)) => label,
        (None, None) => tr!("subagent.title"),
    }
}

fn activity_tool_display_name(activity: &ActivityItem) -> String {
    if is_ask_user_question(activity) {
        return tr!("activity.ask_questions");
    }
    if activity.is_subagent() {
        return subagent_activity_label(activity);
    }
    if let Some(target) = activity
        .display_target
        .as_deref()
        .map(str::trim)
        .filter(|target| !target.is_empty())
    {
        return target.to_owned();
    }
    if !crate::model::is_generic_activity_title(activity.kind, &activity.title) {
        return humanize_tool_name(&activity.title);
    }
    tr!("activity.tool")
}

pub(super) fn activity_display_title(activity: &ActivityItem) -> String {
    use crate::model::ActivityKind;

    match activity.kind {
        ActivityKind::FileChange => {
            let subject = match activity.file_changes.as_slice() {
                [change] => Some(change.display_name().to_owned()),
                changes if !changes.is_empty() => {
                    Some(tr!("activity.file_count", count = changes.len()))
                }
                _ => None,
            };
            if subject.is_none()
                && !crate::model::is_generic_activity_title(activity.kind, &activity.title)
            {
                return activity.title.clone();
            }
            match (activity.complete, activity.failed, subject) {
                (false, _, Some(file)) => tr!("activity.editing_named_file", file = file),
                (true, false, Some(file)) => tr!("activity.edited_named_file", file = file),
                (true, true, Some(file)) => tr!("activity.edit_failed_named_file", file = file),
                (false, _, None) => tr!("activity.editing_files"),
                (true, false, None) => tr!("activity.edited_files"),
                (true, true, None) => tr!("activity.edit_failed"),
            }
        }
        ActivityKind::FileRead => {
            let file = activity.display_target.as_deref().map(activity_path_name);
            if file.is_none()
                && !crate::model::is_generic_activity_title(activity.kind, &activity.title)
            {
                return activity.title.clone();
            }
            match (activity.complete, activity.failed, file) {
                (false, _, Some(file)) => tr!("activity.reading_named_file", file = file),
                (true, false, Some(file)) => tr!("activity.read_named_file", file = file),
                (true, true, Some(file)) => tr!("activity.read_named_file_failed", file = file),
                (false, _, None) => tr!("activity.reading_file"),
                (true, false, None) => tr!("activity.read_file_completed"),
                (true, true, None) => tr!("activity.read_file_failed"),
            }
        }
        ActivityKind::FileSearch => {
            let query = activity.display_target.as_deref();
            if query.is_none()
                && !crate::model::is_generic_activity_title(activity.kind, &activity.title)
            {
                return activity.title.clone();
            }
            match (activity.complete, activity.failed, query) {
                (false, _, Some(query)) => tr!("activity.searching_files_for", query = query),
                (true, false, Some(query)) => tr!("activity.searched_files_for", query = query),
                (true, true, Some(query)) => tr!("activity.file_search_failed_for", query = query),
                (false, _, None) => tr!("activity.searching_files"),
                (true, false, None) => tr!("activity.searched_files"),
                (true, true, None) => tr!("activity.file_search_failed"),
            }
        }
        ActivityKind::FileList => {
            let directory = activity.display_target.as_deref().map(activity_path_name);
            if directory.is_none()
                && !crate::model::is_generic_activity_title(activity.kind, &activity.title)
            {
                return activity.title.clone();
            }
            match (activity.complete, activity.failed, directory) {
                (false, _, Some(directory)) => {
                    tr!("activity.listing_files_in", directory = directory)
                }
                (true, false, Some(directory)) => {
                    tr!("activity.listed_files_in", directory = directory)
                }
                (true, true, Some(directory)) => {
                    tr!("activity.file_list_failed_in", directory = directory)
                }
                (false, _, None) => tr!("activity.listing_files"),
                (true, false, None) => tr!("activity.listed_files"),
                (true, true, None) => tr!("activity.file_list_failed"),
            }
        }
        ActivityKind::Command => {
            if let Some(description) = activity.display_description.as_deref() {
                return match (activity.complete, activity.failed) {
                    (false, _) => {
                        tr!(
                            "activity.running_described_command",
                            description = description
                        )
                    }
                    (true, false) => {
                        tr!("activity.ran_described_command", description = description)
                    }
                    (true, true) => {
                        tr!(
                            "activity.described_command_failed",
                            description = description
                        )
                    }
                };
            }
            if let Some(command) = activity.display_target.as_deref() {
                return match (activity.complete, activity.failed) {
                    (false, _) => tr!("activity.running_named_command", command = command),
                    (true, false) => tr!("activity.ran_named_command", command = command),
                    (true, true) => tr!("activity.named_command_failed", command = command),
                };
            }
            if !crate::model::is_generic_activity_title(activity.kind, &activity.title) {
                return activity.title.clone();
            }
            match (activity.complete, activity.failed) {
                (false, _) => tr!("activity.running_command"),
                (true, false) => tr!("activity.ran_command"),
                (true, true) => tr!("activity.command_failed"),
            }
        }
        ActivityKind::Search => {
            if let Some(query) = activity.display_target.as_deref() {
                return match (activity.complete, activity.failed) {
                    (false, _) => tr!("activity.searching_web_for", query = query),
                    (true, false) => tr!("activity.searched_web_for", query = query),
                    (true, true) => tr!("activity.web_search_failed_for", query = query),
                };
            }
            if ActivityKind::from_tool_name(&activity.title) == ActivityKind::Search {
                return match (activity.complete, activity.failed) {
                    (false, _) => tr!("activity.searching_web"),
                    (true, false) => tr!("activity.searched_the_web"),
                    (true, true) => tr!("activity.web_search_failed"),
                };
            }
            activity.title.clone()
        }
        ActivityKind::Plan => {
            if !crate::model::is_generic_activity_title(activity.kind, &activity.title) {
                return activity.title.clone();
            }
            match (activity.complete, activity.failed) {
                (false, _) => tr!("activity.updating_plan"),
                (true, false) => tr!("activity.updated_plan"),
                (true, true) => tr!("activity.plan_update_failed"),
            }
        }
        ActivityKind::ImageGeneration => tr!(if activity.failed {
            "activity.image_generation_failed"
        } else if activity.complete {
            "activity.generated_image"
        } else {
            "activity.generating_image"
        }),
        ActivityKind::Tool | ActivityKind::Compaction => activity_tool_display_name(activity),
        ActivityKind::Reasoning | ActivityKind::Notice => activity.title.clone(),
    }
}

pub(super) fn activity_action_label(activity: &ActivityItem) -> String {
    use crate::model::ActivityKind;

    match activity.kind {
        ActivityKind::Reasoning => tr!("activity.action_think"),
        ActivityKind::Command => tr!("activity.action_run"),
        ActivityKind::FileChange => match activity.file_changes.as_slice() {
            [change] if change.status == Some(crate::model::ActivityFileChangeStatus::Added) => {
                tr!("activity.action_create")
            }
            _ => tr!("activity.action_edit"),
        },
        ActivityKind::FileRead => tr!("activity.action_read"),
        ActivityKind::FileSearch | ActivityKind::Search => tr!("activity.action_search"),
        ActivityKind::FileList => tr!("activity.action_list"),
        ActivityKind::Plan => tr!("activity.action_plan"),
        ActivityKind::ImageGeneration => tr!("activity.generating_image"),
        ActivityKind::Notice => activity.title.clone(),
        ActivityKind::Tool if is_ask_user_question(activity) => tr!("activity.ask_questions"),
        ActivityKind::Tool if activity.is_subagent() => tr!("subagent.badge"),
        ActivityKind::Tool | ActivityKind::Compaction => tr!("activity.tool"),
    }
}

pub(super) fn activity_row_detail(activity: &ActivityItem, reasoning_live: bool) -> String {
    use crate::model::ActivityKind;

    let custom_title = || {
        (!crate::model::is_generic_activity_title(activity.kind, &activity.title))
            .then(|| activity.title.clone())
    };
    match activity.kind {
        ActivityKind::Reasoning => activity.reasoning.as_ref().map_or_else(
            || activity.title.clone(),
            |reasoning| {
                reasoning_row_headline(reasoning, reasoning_live)
                    .or_else(|| reasoning_duration(reasoning, reasoning_live))
                    .unwrap_or_default()
            },
        ),
        ActivityKind::Command => activity
            .display_description
            .clone()
            .or_else(|| activity.display_target.clone())
            .or_else(custom_title)
            .unwrap_or_default(),
        ActivityKind::FileChange => match activity.file_changes.as_slice() {
            [change] => change.display_name().to_owned(),
            changes if !changes.is_empty() => {
                tr!("activity.file_count", count = changes.len())
            }
            _ => custom_title().unwrap_or_default(),
        },
        ActivityKind::FileRead | ActivityKind::FileList => activity
            .display_target
            .as_deref()
            .map(activity_path_name)
            .or_else(custom_title)
            .unwrap_or_default(),
        ActivityKind::FileSearch | ActivityKind::Search => activity
            .display_target
            .clone()
            .or_else(custom_title)
            .unwrap_or_default(),
        ActivityKind::Plan => custom_title().unwrap_or_default(),
        ActivityKind::ImageGeneration => String::new(),
        ActivityKind::Notice => activity.detail.clone().unwrap_or_default(),
        ActivityKind::Tool if is_ask_user_question(activity) => String::new(),
        ActivityKind::Tool if activity.is_subagent() => subagent_activity_label(activity),
        ActivityKind::Tool | ActivityKind::Compaction => {
            let has_name = activity
                .display_target
                .as_deref()
                .is_some_and(|target| !target.trim().is_empty())
                || !crate::model::is_generic_activity_title(activity.kind, &activity.title);
            has_name
                .then(|| activity_tool_display_name(activity))
                .unwrap_or_default()
        }
    }
}

/// How long finished reasoning took, shown beside a headline. Unknown while
/// it streams and for snapshot-only reasoning, which has no start time.
fn reasoning_duration(reasoning: &ReasoningBlock, live: bool) -> Option<String> {
    (!live && reasoning.started_at_ms != 0).then(|| {
        format_worked_duration(
            reasoning
                .finished_at_ms
                .saturating_sub(reasoning.started_at_ms)
                .div_ceil(1000)
                .max(1),
        )
    })
}

/// The duration a reasoning row shows after its headline, when the headline
/// took the detail slot that otherwise holds the duration.
pub(super) fn reasoning_row_duration(activity: &ActivityItem, live: bool) -> Option<String> {
    let reasoning = activity.reasoning.as_ref()?;
    reasoning_row_headline(reasoning, live)?;
    reasoning_duration(reasoning, live)
}

/// The text-generation model's headline once it lands, else the opening line.
fn reasoning_row_headline(reasoning: &ReasoningBlock, live: bool) -> Option<String> {
    reasoning
        .headline
        .clone()
        .or_else(|| reasoning_headline(&reasoning.content, live))
}

/// A one-line summary of what the model was thinking about, so a reasoning
/// row says more than "Think".
///
/// Codex reasoning summaries open with a bold heading, which is used as is.
/// Raw thinking (Pi, DeepSeek, Claude) has no heading, so its first sentence
/// stands in. Only the opening few hundred bytes are read, keeping the row
/// builder's cost independent of how long the reasoning grew.
///
/// While reasoning streams, a headline appears only once its heading or first
/// sentence has closed, so the row never shows a half-written phrase.
pub(super) fn reasoning_headline(content: &str, live: bool) -> Option<String> {
    const SCAN_BYTES: usize = 480;
    const MAX_CHARS: usize = 120;

    let text = content.trim_start();
    let mut end = text.len().min(SCAN_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let scanned = &text[..end];
    let truncated_scan = end < text.len();

    let (candidate, closed) = if let Some(rest) = scanned.strip_prefix("**") {
        match rest.find("**") {
            Some(close) => (&rest[..close], true),
            None => (rest.lines().next().unwrap_or(rest), false),
        }
    } else {
        let (line, line_closed) = match scanned.find('\n') {
            Some(newline) => (&scanned[..newline], true),
            None => (scanned, false),
        };
        match first_sentence_end(line) {
            Some(sentence_end) => (&line[..sentence_end], true),
            None => (line, line_closed),
        }
    };
    if live && !closed {
        return None;
    }

    let headline = candidate
        .trim()
        .trim_start_matches(['#', '>', '-', ' '])
        .trim_matches(['*', '_', '`', ' '])
        .trim_end_matches(['.', ':', '：', '。', ' '])
        .trim();
    if headline.is_empty() {
        return None;
    }
    let mut chars = headline.chars();
    let mut shortened: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() || (!closed && truncated_scan) {
        shortened = shortened.trim_end().to_owned();
        shortened.push('…');
    }
    Some(shortened)
}

/// Byte offset just past the first sentence of `line`, keeping its
/// terminator. A period ends a sentence only before whitespace, so file names
/// and version numbers stay whole; CJK terminators end one outright.
fn first_sentence_end(line: &str) -> Option<usize> {
    let mut chars = line.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '。' | '！' | '？' => return Some(index + ch.len_utf8()),
            '.' | '!' | '?' => {
                // End of line is handled by the caller, which knows whether
                // the line itself has closed.
                if chars.peek().is_some_and(|(_, next)| next.is_whitespace()) {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

pub(super) fn reasoning_activity_title(reasoning: &ReasoningBlock, live: bool) -> String {
    if live {
        tr!("transcript.thinking")
    } else if reasoning.started_at_ms == 0 {
        tr!("activity.action_think")
    } else {
        tr!(
            "transcript.thought_for",
            duration = format_worked_duration(
                reasoning
                    .finished_at_ms
                    .saturating_sub(reasoning.started_at_ms)
                    .div_ceil(1000)
                    .max(1)
            )
        )
    }
}

fn activity_path_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_owned()
}

/// Whether this activity's expanded view shows a diff instead of the tool
/// arguments that produced it.
pub(super) fn activity_shows_diff(activity: &ActivityItem) -> bool {
    activity.kind == ActivityKind::FileChange
        && activity
            .file_changes
            .iter()
            .any(|change| change.diff.is_some())
}

pub(super) fn activity_file_change_stats(activity: &ActivityItem) -> Option<(u64, u64)> {
    if activity.kind != crate::model::ActivityKind::FileChange
        || !activity.complete
        || activity.failed
        || activity.file_changes.is_empty()
    {
        return None;
    }
    let additions = activity
        .file_changes
        .iter()
        .map(|change| change.additions)
        .sum::<Option<u64>>()?;
    let deletions = activity
        .file_changes
        .iter()
        .map(|change| change.deletions)
        .sum::<Option<u64>>()?;
    Some((additions, deletions))
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum ActivityDisclosureSectionKind {
    McpServer,
    ToolName,
    Command,
    Arguments,
    Output,
    Detail,
}

impl ActivityDisclosureSectionKind {
    pub(super) fn id(self) -> &'static str {
        match self {
            Self::McpServer => "mcp-server",
            Self::ToolName => "tool-name",
            Self::Command => "command",
            Self::Arguments => "arguments",
            Self::Output => "output",
            Self::Detail => "detail",
        }
    }

    pub(super) fn label(self) -> Option<String> {
        match self {
            Self::McpServer => Some(tr!("activity.mcp_server")),
            Self::ToolName => Some(tr!("activity.tool_name")),
            Self::Command => Some(tr!("activity.command_detail")),
            Self::Arguments => Some(tr!("activity.input")),
            Self::Output => Some(tr!("activity.output")),
            Self::Detail => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ActivityDisclosureSection {
    pub(super) kind: ActivityDisclosureSectionKind,
    pub(super) content: String,
}

pub(super) fn activity_disclosure_sections(
    activity: &ActivityItem,
) -> Vec<ActivityDisclosureSection> {
    let mut sections = Vec::new();
    // A subagent's row discloses its report: the envelope, the tool name and
    // the raw arguments are plumbing, and the whole run is one click away.
    if activity.is_subagent() {
        if let Some(output) = activity
            .output
            .as_deref()
            .map(str::trim)
            .filter(|output| !output.is_empty())
        {
            let result = crate::model::parse_subagent_result(output);
            // A background call's answer only says the work continues; the
            // report arrives later as its own notice.
            if result
                .as_ref()
                .is_some_and(|result| result.outcome == crate::model::SubagentOutcome::Running)
            {
                return sections;
            }
            let report = result.map_or(output, |result| result.body);
            sections.push(ActivityDisclosureSection {
                kind: ActivityDisclosureSectionKind::Output,
                content: report.to_owned(),
            });
        }
        return sections;
    }
    // A read that returned a picture is the picture: the path is already in
    // the row, and the arguments and payload would only bury it.
    if activity.kind == ActivityKind::FileRead
        && !activity.failed
        && !activity.image_urls.is_empty()
    {
        return sections;
    }
    for (kind, content) in [
        (
            ActivityDisclosureSectionKind::McpServer,
            activity.mcp_server.as_deref(),
        ),
        (
            ActivityDisclosureSectionKind::ToolName,
            activity.tool_name.as_deref(),
        ),
    ] {
        if let Some(content) = content.map(str::trim).filter(|content| !content.is_empty()) {
            sections.push(ActivityDisclosureSection {
                kind,
                content: content.to_owned(),
            });
        }
    }
    let metadata_count = sections.len();
    if activity.kind == ActivityKind::Command {
        if let Some(command) = activity
            .arguments
            .as_deref()
            .or(activity.display_target.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            sections.push(ActivityDisclosureSection {
                kind: ActivityDisclosureSectionKind::Command,
                content: command.to_owned(),
            });
        }
        if let Some(output) = activity
            .output
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            sections.push(ActivityDisclosureSection {
                kind: ActivityDisclosureSectionKind::Output,
                content: output.to_owned(),
            });
        } else if !activity.image_urls.is_empty() {
            sections.push(ActivityDisclosureSection {
                kind: ActivityDisclosureSectionKind::Output,
                content: String::new(),
            });
        }
        return sections;
    }
    // An edit renders as a diff, which says everything the raw arguments would
    // and reads. What the tool replied is only worth the room when it failed.
    let shows_diff = activity_shows_diff(activity);
    if let Some(arguments) = activity
        .arguments
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|_| !shows_diff)
    {
        sections.push(ActivityDisclosureSection {
            kind: ActivityDisclosureSectionKind::Arguments,
            content: arguments.to_owned(),
        });
    }
    if let Some(output) = activity
        .output
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|_| !shows_diff || activity.failed)
        .filter(|output| activity.image_urls.is_empty() || !is_image_payload(output))
    {
        sections.push(ActivityDisclosureSection {
            kind: ActivityDisclosureSectionKind::Output,
            content: output.to_owned(),
        });
    } else if !activity.image_urls.is_empty() {
        sections.push(ActivityDisclosureSection {
            kind: ActivityDisclosureSectionKind::Output,
            content: String::new(),
        });
    }
    if sections.len() == metadata_count
        && let Some(detail) = activity
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        sections.push(ActivityDisclosureSection {
            kind: ActivityDisclosureSectionKind::Detail,
            content: detail.to_owned(),
        });
    }
    sections
}

/// Whether a tool's output text is only the serialized image it returned.
/// Sessions saved before images were split out of the text still carry that
/// payload; the image itself renders below, so the base64 is never shown.
/// Looks only at the head, so a frame never scans a 16 KB payload.
fn is_image_payload(output: &str) -> bool {
    if !output.starts_with('{') {
        return false;
    }
    let mut end = output.len().min(80);
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    let head = &output[..end];
    head.contains("\"type\": \"image\"") || head.contains("\"type\":\"image\"")
}

pub(super) fn activity_preview(activity: &ActivityItem) -> String {
    let detail = activity.detail.as_deref().unwrap_or_default().trim();
    if detail.eq_ignore_ascii_case("failed")
        && let Some(output) = activity.output.as_deref()
        && let Some(first_line) = output.lines().find(|line| !line.trim().is_empty())
    {
        return first_line.trim().to_owned();
    }
    if (detail.is_empty() || detail.eq_ignore_ascii_case("failed"))
        && !activity.image_urls.is_empty()
    {
        return tr!("activity.image_output");
    }
    detail.to_owned()
}

#[cfg(test)]
mod pasted_text_chip_tests {
    use super::*;

    #[test]
    fn a_chip_reuses_the_tab_of_the_attachment_it_came_from() {
        let id = Uuid::new_v4();
        let reference = format!("{}{id}", raku_protocol::attachments::ATTACHMENT_SCHEME);

        // A restored draft or a sent message carries the same reference, so
        // both reopen the one tab rather than stacking duplicates.
        assert_eq!(pasted_text_view_id(Some(&reference), "ignored"), id);
        assert_eq!(
            pasted_text_view_id(None, "same text"),
            pasted_text_view_id(None, "same text")
        );
        assert_ne!(
            pasted_text_view_id(None, "first paste"),
            pasted_text_view_id(None, "second paste")
        );
        // A reference that is not an attachment id falls back to the text
        // rather than inventing a new tab on every click.
        assert_eq!(
            pasted_text_view_id(Some("not-an-attachment"), "text"),
            pasted_text_view_id(None, "text")
        );
    }
}

#[cfg(test)]
mod message_time_tests {
    use super::*;
    use chrono::TimeZone;

    /// Test-only rendering of disclosure sections into plain text; production
    /// renders them interactively via [`activity_disclosure_sections`].
    fn activity_disclosure_text(activity: &ActivityItem) -> Option<String> {
        let sections = activity_disclosure_sections(activity);
        (!sections.is_empty()).then(|| {
            sections
                .into_iter()
                .map(
                    |section| match (section.kind.label(), section.content.is_empty()) {
                        (Some(label), false) => format!("{label}\n{}", section.content),
                        (Some(label), true) => label.to_owned(),
                        (None, _) => section.content,
                    },
                )
                .collect::<Vec<_>>()
                .join("\n\n")
        })
    }

    fn local_datetime(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("test date should be valid in the local timezone")
    }

    fn unix_seconds(timestamp: DateTime<Local>) -> u64 {
        timestamp
            .timestamp()
            .try_into()
            .expect("test date should have a positive Unix timestamp")
    }

    #[test]
    fn message_time_includes_calendar_context_for_older_messages() {
        let now = local_datetime(2026, 8, 9, 16, 0); // Sunday

        assert_eq!(
            format_message_time_at(unix_seconds(local_datetime(2026, 8, 9, 9, 5)), now),
            "9:05 AM"
        );
        assert_eq!(
            format_message_time_at(unix_seconds(local_datetime(2026, 8, 8, 17, 0)), now),
            "Yesterday 5:00 PM"
        );
        assert_eq!(
            format_message_time_at(unix_seconds(local_datetime(2026, 8, 7, 13, 12)), now),
            "Friday 1:12 PM"
        );
        assert_eq!(
            format_message_time_at(unix_seconds(local_datetime(2026, 5, 12, 23, 0)), now),
            "May 12th, 11:00 PM"
        );
        assert_eq!(
            format_message_time_at(unix_seconds(local_datetime(2024, 8, 4, 11, 0)), now),
            "Aug 4th 2024, 11:00 AM"
        );
    }

    #[test]
    fn message_time_uses_correct_ordinal_suffixes() {
        let now = local_datetime(2026, 8, 9, 16, 0);

        for (day, suffix) in [
            (1, "st"),
            (2, "nd"),
            (3, "rd"),
            (11, "th"),
            (12, "th"),
            (13, "th"),
            (21, "st"),
        ] {
            let formatted =
                format_message_time_at(unix_seconds(local_datetime(2026, 5, day, 9, 0)), now);
            assert!(formatted.starts_with(&format!("May {day}{suffix},")));
        }
    }

    #[test]
    fn activity_disclosure_distinguishes_mcp_identity_from_the_title() {
        let activity = ActivityItem::new(
            Some("tool-1".into()),
            crate::model::ActivityKind::Tool,
            "Query the index",
            None,
            true,
        )
        .with_tool_name(Some("search"))
        .with_mcp_server(Some("raku_test"))
        .with_arguments(Some("{}".into()));
        assert_eq!(activity_display_title(&activity), "Query the index");
        assert_eq!(
            activity_disclosure_sections(&activity),
            vec![
                ActivityDisclosureSection {
                    kind: ActivityDisclosureSectionKind::McpServer,
                    content: "raku_test".into()
                },
                ActivityDisclosureSection {
                    kind: ActivityDisclosureSectionKind::ToolName,
                    content: "search".into()
                },
                ActivityDisclosureSection {
                    kind: ActivityDisclosureSectionKind::Arguments,
                    content: "{}".into()
                },
            ]
        );

        let regular = ActivityItem::new(
            None,
            crate::model::ActivityKind::Tool,
            "Read notes",
            Some("Could not read notes".into()),
            true,
        )
        .with_tool_name(Some("read_file"));
        assert_eq!(
            activity_disclosure_sections(&regular),
            vec![
                ActivityDisclosureSection {
                    kind: ActivityDisclosureSectionKind::ToolName,
                    content: "read_file".into()
                },
                ActivityDisclosureSection {
                    kind: ActivityDisclosureSectionKind::Detail,
                    content: "Could not read notes".into()
                },
            ]
        );
    }

    #[test]
    fn image_reads_show_only_the_image() {
        let payload = "{\n  \"type\": \"image\",\n  \"data\": \"iVBORw0KGgo=\"\n}";
        let read = ActivityItem::new(
            None,
            crate::model::ActivityKind::FileRead,
            "Read",
            None,
            true,
        )
        .with_tool_name(Some("read"))
        .with_arguments(Some("{\"path\": \"/tmp/a.png\"}".into()))
        .with_output(Some(payload.into()))
        .with_image_urls(vec!["waku-blob:a.png".into()]);
        assert!(activity_disclosure_sections(&read).is_empty());

        // A saved session's tool output that is only the serialized image is
        // dropped; the image renders in its place.
        let tool = ActivityItem::new(None, crate::model::ActivityKind::Tool, "Shot", None, true)
            .with_output(Some(payload.into()))
            .with_image_urls(vec!["waku-blob:a.png".into()]);
        assert_eq!(
            activity_disclosure_sections(&tool),
            vec![ActivityDisclosureSection {
                kind: ActivityDisclosureSectionKind::Output,
                content: String::new(),
            }]
        );
    }

    #[test]
    fn activity_disclosure_keeps_arguments_and_output() {
        let activity = ActivityItem::new(
            Some("tool-1".into()),
            crate::model::ActivityKind::Tool,
            "Use Helium",
            Some("failed".into()),
            true,
        )
        .with_arguments(Some("{\n  \"actions\": []\n}".into()))
        .with_output(Some("Helper closed its session".into()))
        .with_failed(true);

        assert_eq!(
            activity_disclosure_sections(&activity),
            vec![
                ActivityDisclosureSection {
                    kind: ActivityDisclosureSectionKind::Arguments,
                    content: "{\n  \"actions\": []\n}".into(),
                },
                ActivityDisclosureSection {
                    kind: ActivityDisclosureSectionKind::Output,
                    content: "Helper closed its session".into(),
                },
            ]
        );
        assert_eq!(
            activity_disclosure_text(&activity).as_deref(),
            Some("Input\n{\n  \"actions\": []\n}\n\nOutput\nHelper closed its session")
        );
        assert_eq!(activity_preview(&activity), "Helper closed its session");

        let image_only = ActivityItem::new(
            Some("tool-2".into()),
            crate::model::ActivityKind::Tool,
            "Screenshot",
            None,
            true,
        )
        .with_image_urls(vec!["data:image/png;base64,aGVsbG8=".into()]);
        assert_eq!(
            activity_disclosure_text(&image_only).as_deref(),
            Some("Output")
        );
        assert_eq!(activity_preview(&image_only), "Image output");
    }

    #[test]
    fn command_disclosure_shows_only_the_command_and_output() {
        let activity = ActivityItem::new(
            Some("command-1".into()),
            crate::model::ActivityKind::Command,
            "bash",
            Some("Completed".into()),
            true,
        )
        .with_arguments(Some(
            r#"{"command":"git status --short","description":"Check status"}"#.into(),
        ))
        .with_output(Some("clean".into()));

        assert_eq!(
            activity_disclosure_sections(&activity),
            vec![
                ActivityDisclosureSection {
                    kind: ActivityDisclosureSectionKind::Command,
                    content: "git status --short".into(),
                },
                ActivityDisclosureSection {
                    kind: ActivityDisclosureSectionKind::Output,
                    content: "clean".into(),
                },
            ]
        );
        assert_eq!(
            activity_disclosure_text(&activity).as_deref(),
            Some("Command\ngit status --short\n\nOutput\nclean")
        );
    }

    #[test]
    fn activity_display_title_prefers_the_human_facing_tool_argument() {
        let titled = ActivityItem::new(
            Some("tool-1".into()),
            crate::model::ActivityKind::Tool,
            "Js",
            None,
            true,
        )
        .with_arguments(Some(
            r#"{"title":"Inspect Helium browser","code":"sky.get_app_state()"}"#.into(),
        ));
        let untitled = ActivityItem::new(
            Some("tool-2".into()),
            crate::model::ActivityKind::Tool,
            "Js",
            None,
            true,
        )
        .with_arguments(Some(r#"{"code":"sky.list_apps()"}"#.into()));

        assert_eq!(activity_display_title(&titled), "Inspect Helium browser");
        assert_eq!(activity_display_title(&untitled), "Js");
    }

    #[test]
    fn generic_tool_rows_keep_a_humanized_provider_name() {
        let named = ActivityItem::new(
            Some("tool-1".into()),
            crate::model::ActivityKind::Tool,
            "mcp__threads__create_thread",
            None,
            true,
        );
        let unnamed = ActivityItem::new(
            Some("tool-2".into()),
            crate::model::ActivityKind::Tool,
            "Tool",
            None,
            true,
        );

        assert_eq!(activity_action_label(&named), "Tool");
        assert_eq!(activity_row_detail(&named, false), "Create thread");
        assert_eq!(activity_display_title(&named), "Create thread");
        assert_eq!(activity_action_label(&unnamed), "Tool");
        assert_eq!(activity_row_detail(&unnamed, false), "");
    }

    #[test]
    fn ask_user_question_has_a_purpose_specific_label() {
        let activity = ActivityItem::new(
            Some("tool-1".into()),
            crate::model::ActivityKind::Tool,
            "AskUserQuestion",
            None,
            true,
        )
        .with_arguments(Some(r#"{"questions":[]}"#.into()));

        assert_eq!(activity_action_label(&activity), "Ask questions");
        assert_eq!(activity_row_detail(&activity, false), "");
        assert_eq!(activity_display_title(&activity), "Ask questions");
    }

    #[test]
    fn snapshot_only_reasoning_does_not_invent_a_duration() {
        let activity = ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "summary".into(),
                started_at_ms: 0,
                finished_at_ms: 1_000_000,
                headline: None,
            },
            true,
        );
        assert_eq!(activity_row_detail(&activity, false), "summary");
        assert_eq!(reasoning_row_duration(&activity, false), None);
        assert_eq!(
            reasoning_activity_title(activity.reasoning.as_ref().unwrap(), false),
            "Think"
        );
    }

    #[test]
    fn reasoning_headlines_summarize_the_opening_thought() {
        let cases = [
            // Codex summary headings.
            (
                "**Inspecting the diff**\n\nI'm checking hunks.",
                false,
                Some("Inspecting the diff"),
            ),
            ("**Inspecting the diff**", true, Some("Inspecting the diff")),
            ("**Inspecting the di", true, None),
            // Raw thinking: the first sentence.
            (
                "I need to commit only my changes, not the parallel session's work.\n\nMy files:",
                false,
                Some("I need to commit only my changes, not the parallel session's work"),
            ),
            (
                "The user says inline code is bold. Let me find the styling.",
                true,
                Some("The user says inline code is bold"),
            ),
            ("Check src/app.rs before v1.2 ships", true, None),
            (
                "Check src/app.rs before v1.2 ships",
                false,
                Some("Check src/app.rs before v1.2 ships"),
            ),
            ("Plan:\n1. Read", true, Some("Plan")),
            (
                "用户说行内代码被加粗了。我来找找样式。",
                true,
                Some("用户说行内代码被加粗了"),
            ),
            ("  \n\n", false, None),
        ];
        for (content, live, expected) in cases {
            assert_eq!(
                reasoning_headline(content, live).as_deref(),
                expected,
                "{content:?}"
            );
        }

        let mut generated = ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "I'm putting together a simple Wordle clone as one HTML file.".into(),
                started_at_ms: 1_000,
                finished_at_ms: 2_000,
                headline: Some("Planning a single-file Wordle clone".into()),
            },
            true,
        );
        assert_eq!(
            activity_row_detail(&generated, false),
            "Planning a single-file Wordle clone"
        );
        assert_eq!(
            reasoning_row_duration(&generated, false).as_deref(),
            Some("1 second")
        );
        generated.reasoning.as_mut().unwrap().headline = None;
        assert_eq!(
            activity_row_detail(&generated, false),
            "I'm putting together a simple Wordle clone as one HTML file"
        );

        let long = "word ".repeat(200);
        let headline = reasoning_headline(&long, false).unwrap();
        assert!(headline.ends_with('…'));
        assert!(headline.chars().count() <= 121);
    }

    #[test]
    fn activity_header_summarizes_only_after_the_group_leaves_the_live_tail() {
        let reasoning = ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "Inspecting history".into(),
                started_at_ms: 1_000,
                finished_at_ms: 2_000,
                headline: None,
            },
            true,
        );
        let command = ActivityItem::new(
            Some("command-1".into()),
            crate::model::ActivityKind::Command,
            "bash",
            None,
            false,
        )
        .with_arguments(Some(
            serde_json::json!({"command": "git log --oneline -15"}).to_string(),
        ));
        let mut activities = vec![reasoning, command];

        assert_eq!(
            activity_header_title(&activities, true, None),
            "Running git log --oneline -15"
        );
        assert!(activity_group_is_live(true, true, 1, 1));
        activities[1].complete = true;
        assert_eq!(
            activity_header_title(&activities, true, None),
            "Ran git log --oneline -15"
        );
        assert!(!activity_group_is_live(true, true, 1, 2));
        assert_eq!(
            activity_header_title(&activities, false, None),
            "Ran 1 command"
        );
        assert_eq!(activity_summary(&activities), "Ran 1 thought · 1 command");
        assert_eq!(
            activity_header_title(&activities[..1], false, None),
            reasoning_activity_title(activities[0].reasoning.as_ref().unwrap(), false)
        );
        assert_eq!(
            activity_row_detail(&activities[0], false),
            "Inspecting history"
        );
        assert_eq!(
            reasoning_row_duration(&activities[0], false).as_deref(),
            Some("1 second")
        );
        // An unfinished first line is not shown while it streams.
        assert_eq!(activity_row_detail(&activities[0], true), "");
        assert!(!activity_group_is_live(true, false, 1, 1));
        assert!(!activity_group_is_live(false, true, 1, 1));
        assert_eq!(activity_action_label(&activities[1]), "Run");
        assert_eq!(
            activity_row_detail(&activities[1], false),
            "git log --oneline -15"
        );
    }

    #[test]
    fn file_edit_title_and_stats_follow_the_activity_state() {
        let mut activity = ActivityItem::new(
            Some("edit-1".into()),
            crate::model::ActivityKind::FileChange,
            "apply_patch",
            None,
            false,
        )
        .with_arguments(Some(
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: /tmp/raku/src/app.rs\n@@\n-old\n+new\n+more\n*** End Patch"
            })
            .to_string(),
        ));

        assert_eq!(activity_display_title(&activity), "Editing app.rs");
        assert_eq!(activity_file_change_stats(&activity), None);

        activity.complete = true;
        assert_eq!(activity_display_title(&activity), "Edited app.rs");
        assert_eq!(activity_file_change_stats(&activity), Some((2, 1)));

        activity.failed = true;
        assert_eq!(activity_display_title(&activity), "Failed to edit app.rs");
        assert_eq!(activity_file_change_stats(&activity), None);
    }

    #[test]
    fn multi_file_edits_use_a_compact_count() {
        let activity = ActivityItem::new(
            Some("edit-2".into()),
            crate::model::ActivityKind::FileChange,
            "apply_patch",
            None,
            true,
        )
        .with_arguments(Some(
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n*** Update File: src/b.rs\n@@\n-c\n+d\n*** End Patch"
            })
            .to_string(),
        ));

        assert_eq!(activity_display_title(&activity), "Edited 2 files");
        assert_eq!(activity_file_change_stats(&activity), Some((2, 2)));
    }

    #[test]
    fn file_tool_titles_include_the_target_and_state() {
        let mut read = ActivityItem::new(
            Some("read-1".into()),
            crate::model::ActivityKind::FileRead,
            "read",
            None,
            false,
        )
        .with_arguments(Some(
            serde_json::json!({"filePath": "/tmp/raku/src/model.rs"}).to_string(),
        ));
        assert_eq!(activity_display_title(&read), "Reading model.rs");
        read.complete = true;
        assert_eq!(activity_display_title(&read), "Read model.rs");
        read.failed = true;
        assert_eq!(activity_display_title(&read), "Failed to read model.rs");

        let search = ActivityItem::new(
            Some("grep-1".into()),
            crate::model::ActivityKind::FileSearch,
            "grep",
            None,
            true,
        )
        .with_arguments(Some(
            serde_json::json!({"pattern": "ActivityKind"}).to_string(),
        ));
        assert_eq!(
            activity_display_title(&search),
            "Searched files for ActivityKind"
        );
        assert_eq!(activity_row_detail(&search, false), "ActivityKind");

        let list = ActivityItem::new(
            Some("list-1".into()),
            crate::model::ActivityKind::FileList,
            "ls",
            None,
            false,
        )
        .with_arguments(Some(
            serde_json::json!({"path": "/tmp/raku/src"}).to_string(),
        ));
        assert_eq!(activity_display_title(&list), "Listing files in src");

        let custom = ActivityItem::new(
            Some("read-2".into()),
            crate::model::ActivityKind::FileRead,
            "Inspect generated manifest",
            None,
            true,
        );
        assert_eq!(
            activity_display_title(&custom),
            "Inspect generated manifest"
        );
    }

    #[test]
    fn command_web_search_and_plan_titles_include_their_state() {
        let mut command = ActivityItem::new(
            Some("command-1".into()),
            crate::model::ActivityKind::Command,
            "bash",
            None,
            true,
        )
        .with_arguments(Some(
            serde_json::json!({
                "description": "Run focused tests",
                "command": "cargo test activity"
            })
            .to_string(),
        ));
        assert_eq!(
            activity_display_title(&command),
            "Ran command: Run focused tests"
        );
        command.complete = false;
        assert_eq!(
            activity_display_title(&command),
            "Running command: Run focused tests"
        );

        let web_search = ActivityItem::new(
            Some("search-1".into()),
            crate::model::ActivityKind::Search,
            "web_search",
            None,
            true,
        )
        .with_arguments(Some(serde_json::json!({"query": "Raku GPUI"}).to_string()));
        assert_eq!(
            activity_display_title(&web_search),
            "Searched the web for Raku GPUI"
        );

        let plan = ActivityItem::new(
            Some("plan-1".into()),
            crate::model::ActivityKind::Plan,
            "update_plan",
            None,
            false,
        );
        assert_eq!(activity_display_title(&plan), "Updating plan");
    }
}
