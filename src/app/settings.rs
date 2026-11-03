use super::composer::next_picker_highlight;
use super::*;
use crate::ui::ActivationExt;

gpui::actions!(
    waku_settings,
    [CloseSettings, NextSettingsFocus, PreviousSettingsFocus]
);

const SETTINGS_CONTENT_MAX_WIDTH: f32 = 760.0;
const SETTINGS_POPUP_MAX_WIDTH: f32 = 1120.0;
const SETTINGS_POPUP_MAX_HEIGHT: f32 = 760.0;
const SETTINGS_CONTEXT: &str = "Settings";

/// The remaining settings sections, in navigation order.
const SETTINGS_PAGES: [(SettingsPage, &str, &str); 5] = [
    (
        SettingsPage::Appearance,
        "settings.appearance",
        "icons/appearance.svg",
    ),
    (
        SettingsPage::Providers,
        "settings.providers",
        "icons/bot.svg",
    ),
    (SettingsPage::Agents, "settings.agents", "icons/users.svg"),
    (SettingsPage::Skills, "settings.skills", "icons/package.svg"),
    (
        SettingsPage::Backups,
        "settings.backups",
        "icons/cloud-upload.svg",
    ),
];

/// Bind dismissal and focus traversal for the settings popup.
pub fn init(cx: &mut App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("escape", CloseSettings, Some(SETTINGS_CONTEXT)),
        KeyBinding::new("tab", NextSettingsFocus, Some(SETTINGS_CONTEXT)),
        KeyBinding::new("shift-tab", PreviousSettingsFocus, Some(SETTINGS_CONTEXT)),
    ]);
}

/// Settings sections available in this build.
pub(super) fn visible_settings_pages() -> impl Iterator<Item = (SettingsPage, String, &'static str)>
{
    SETTINGS_PAGES
        .into_iter()
        .map(|(page, label_key, icon)| (page, crate::i18n::translate(label_key), icon))
}

fn settings_popup_size(viewport_width: f32, viewport_height: f32) -> (f32, f32) {
    (
        (viewport_width - 48.0).clamp(0.0, SETTINGS_POPUP_MAX_WIDTH),
        (viewport_height - 64.0).clamp(0.0, SETTINGS_POPUP_MAX_HEIGHT),
    )
}

impl Waku {
    pub(super) fn open_settings_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        self.settings_page = Some(page);
        // Each page starts at its own top; a scroll position carried over
        // from the previous page would land mid-content.
        self.settings_scroll.set_offset(gpui::Point::default());
        if page == SettingsPage::Skills {
            self.ensure_skills_catalog(false, cx);
        }
        if page == SettingsPage::Backups {
            self.refresh_backups(cx);
        }
        cx.notify();
    }

    pub(super) fn settings_pane_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .size_full()
            .relative()
            .children(self.render_settings_popup(window, cx))
            .into_any_element()
    }

    pub(super) fn render_settings_popup(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.settings_page?;
        let theme = Theme::current(cx);
        let (width, height) = settings_popup_size(
            f32::from(window.viewport_size().width),
            f32::from(window.viewport_size().height),
        );
        let close = div()
            .id("settings-close")
            .track_focus(&self.settings_close_focus)
            .tab_index(2)
            .absolute()
            .top(px(14.0))
            .right(px(14.0))
            .size(px(28.0))
            .rounded(px(7.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|style| style.bg(theme.overlay))
            .active(|style| style.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(tr_cow!("settings.close")))
            .child(icon("icons/x.svg", 14.0, theme.text_tertiary))
            .on_activation(cx, |this, window, cx| this.close_settings_popup(window, cx));
        let card = div()
            .id("settings-popup")
            .key_context(SETTINGS_CONTEXT)
            .track_focus(&self.settings_focus)
            .tab_index(0)
            .tab_group()
            .tab_stop(false)
            .on_action(cx.listener(|this, _: &CloseSettings, window, cx| {
                this.close_settings_popup(window, cx)
            }))
            .on_action(cx.listener(|this, _: &CloseWindow, window, cx| {
                this.close_settings_popup(window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextSettingsFocus, window, cx| {
                this.cycle_settings_focus(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousSettingsFocus, window, cx| {
                this.cycle_settings_focus(true, window, cx)
            }))
            .w(px(width))
            .h(px(height))
            .relative()
            .flex()
            .rounded(px(14.0))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .shadow_xl()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(self.render_settings_sidebar(cx))
            .child(self.render_settings_content(window, cx))
            .child(close);
        let layer = div()
            .id("settings-popup-layer")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(
                0.0,
                0.0,
                0.0,
                if theme.is_dark { 0.32 } else { 0.16 },
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_settings_popup(window, cx)),
            )
            .child(card);
        Some(gpui::deferred(layer).into_any_element())
    }

    pub(super) fn close_settings_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_page.take().is_none() {
            return;
        }
        self.settings_focus_generation = self.settings_focus_generation.wrapping_add(1);
        let menus = self
            .menus
            .borrow()
            .values()
            .filter(|menu| menu.is_open())
            .cloned()
            .collect::<Vec<_>>();
        if !menus.is_empty() {
            window.defer(cx, move |window, cx| {
                for menu in menus {
                    menu.close(window, cx);
                }
            });
        }
        let focus = self
            .settings_previous_focus
            .take()
            .unwrap_or_else(|| self.composer_focus(cx));
        window.focus(&focus, cx);
        cx.notify();
    }

    fn cycle_settings_focus(&mut self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        if reverse {
            window.focus_prev(cx);
        } else {
            window.focus_next(cx);
        }
        if !self.settings_focus.contains_focused(window, cx) {
            let focus = if reverse {
                self.settings_close_focus.clone()
            } else {
                self.settings_tab_focus[SettingsPage::Appearance as usize].clone()
            };
            window.focus(&focus, cx);
        }
    }

    fn render_settings_sidebar(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let current_page = self.settings_page.unwrap_or(SettingsPage::Appearance);
        let mut navigation = div().flex().flex_col().gap(px(3.0));
        for (page, label, icon_path) in visible_settings_pages() {
            let selected = current_page == page;
            navigation = navigation.child(
                div()
                    .id(SharedString::from(format!(
                        "settings-tab-{}",
                        page as usize
                    )))
                    .track_focus(&self.settings_tab_focus[page as usize])
                    .tab_index(0)
                    .h(px(36.0))
                    .px(px(10.0))
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .cursor_default()
                    .text_size(sp(13.0))
                    .text_color(if selected {
                        theme.text
                    } else {
                        theme.text_secondary
                    })
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .when(selected, |style| style.bg(theme.sidebar_item_background))
                    .hover(|style| style.bg(theme.sidebar_item_background))
                    .active(|style| style.bg(theme.sidebar_item_background))
                    .child(icon(icon_path, 15.0, theme.text_tertiary))
                    .child(label)
                    .on_activation(cx, move |this, window, cx| {
                        this.open_settings_page(page, cx);
                        window.focus(&this.settings_tab_focus[page as usize], cx);
                    })
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        let key = event.keystroke.key.as_str();
                        if matches!(key, "up" | "down" | "home" | "end")
                            && !event.keystroke.modifiers.modified()
                        {
                            if matches!(key, "home" | "end") {
                                let pages = visible_settings_pages()
                                    .map(|(page, ..)| page)
                                    .collect::<Vec<_>>();
                                if let Some(page) = if key == "home" {
                                    pages.first()
                                } else {
                                    pages.last()
                                } {
                                    this.open_settings_page(*page, cx);
                                }
                            } else {
                                this.cycle_settings_page(key, cx);
                            }
                            if let Some(page) = this.settings_page {
                                window.focus(&this.settings_tab_focus[page as usize], cx);
                            }
                            cx.stop_propagation();
                        }
                    })),
            );
        }
        div()
            .tab_index(0)
            .tab_group()
            .tab_stop(false)
            .w(px(200.0))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .rounded_l(px(13.0))
            .bg(theme.sidebar)
            .child(
                div()
                    .h(px(58.0))
                    .px(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .text_size(sp(16.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child(tr!("common.settings")),
            )
            .child(
                div()
                    .id("settings-navigation")
                    .min_h_0()
                    .flex_1()
                    .overflow_y_scroll()
                    .px(px(10.0))
                    .child(navigation),
            )
    }

    /// Move through the remaining settings tabs, wrapping at both ends.
    fn cycle_settings_page(&mut self, key: &str, cx: &mut Context<Self>) {
        let pages = visible_settings_pages()
            .map(|(page, ..)| page)
            .collect::<Vec<_>>();
        let current_page = self.settings_page.unwrap_or(SettingsPage::Appearance);
        let current = pages.iter().position(|page| *page == current_page);
        let Some(next) = next_picker_highlight(current, pages.len(), key) else {
            return;
        };
        self.open_settings_page(pages[next], cx);
    }

    fn render_settings_content(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let page = self.settings_page.unwrap_or(SettingsPage::Appearance);
        let title = match page {
            SettingsPage::Appearance => tr!("settings.appearance"),
            SettingsPage::Providers => tr!("settings.providers"),
            SettingsPage::Agents => tr!("settings.agents"),
            SettingsPage::Skills => tr!("settings.skills"),
            SettingsPage::Backups => tr!("settings.backups"),
        };
        let header = div()
            .h(px(58.0))
            .flex_none()
            .pl(px(24.0))
            .pr(px(52.0))
            .flex()
            .items_center()
            .text_size(sp(16.0))
            .font_weight(FontWeight::MEDIUM)
            .child(title);
        let content = if page == SettingsPage::Skills {
            div()
                .flex_1()
                .min_h_0()
                .child(self.render_skills_settings(cx))
        } else {
            let inner = div()
                .w_full()
                .max_w(px(SETTINGS_CONTENT_MAX_WIDTH))
                .mx_auto()
                .child(match page {
                    SettingsPage::Appearance => self.render_appearance_settings(cx),
                    SettingsPage::Providers => self.render_providers_settings(cx),
                    SettingsPage::Agents => self.render_agent_settings(window, cx),
                    SettingsPage::Backups => self.render_backup_settings(cx),
                    SettingsPage::Skills => unreachable!(),
                });
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(
                    div()
                        .id("settings-content-scroll")
                        .size_full()
                        .px(px(24.0))
                        .overflow_y_scroll()
                        .track_scroll(&self.settings_scroll)
                        .pb(px(24.0))
                        .child(inner),
                )
                .child(scrollbar::vertical(
                    &self.settings_scroll,
                    &self.settings_scrollbar,
                ))
        };
        div()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .tab_index(1)
            .tab_group()
            .tab_stop(false)
            .rounded_r(px(13.0))
            .border_l_1()
            .border_color(theme.sidebar_border)
            .bg(theme.surface)
            .child(header)
            .child(content)
    }

    fn render_appearance_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let selected_theme = self.state.theme;
        let selected_language = self.state.language;
        let weak = cx.entity().downgrade();
        let theme_handle = self.menu_handle("theme-selector", cx);
        let theme_selector = dropdown_menu(
            MenuChip::new("theme-selector")
                .label(selected_theme.label())
                .outlined()
                .selected(theme_handle.is_open())
                .w(px(116.0))
                .justify_between(),
            "theme-selector-menu",
            &theme_handle,
            MenuAlign::BelowRight,
            move |_| {
                ThemePreference::ALL
                    .into_iter()
                    .map(|preference| {
                        let weak = weak.clone();
                        MenuItem::new(preference.label(), move |window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.set_theme_preference(preference, window, cx);
                            });
                        })
                        .selected(preference == selected_theme)
                    })
                    .collect()
            },
        );

        let selected_ui_font_size = self.state.ui_font_size;
        let weak = cx.entity().downgrade();
        let ui_font_size_handle = self.menu_handle("ui-font-size-selector", cx);
        let ui_font_size_selector = dropdown_menu(
            MenuChip::new("ui-font-size-selector")
                .label(font_size_label(selected_ui_font_size))
                .outlined()
                .selected(ui_font_size_handle.is_open())
                .w(px(116.0))
                .justify_between(),
            "ui-font-size-selector-menu",
            &ui_font_size_handle,
            MenuAlign::BelowRight,
            move |_| {
                FONT_SIZES
                    .into_iter()
                    .map(|size| {
                        let weak = weak.clone();
                        MenuItem::new(font_size_label(size), move |window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.set_ui_font_size(size, window, cx);
                            });
                        })
                        .selected(size == selected_ui_font_size)
                    })
                    .collect()
            },
        );

        let selected_code_font_size = self.state.code_font_size;
        let weak = cx.entity().downgrade();
        let code_font_size_handle = self.menu_handle("code-font-size-selector", cx);
        let code_font_size_selector = dropdown_menu(
            MenuChip::new("code-font-size-selector")
                .label(font_size_label(selected_code_font_size))
                .outlined()
                .selected(code_font_size_handle.is_open())
                .w(px(116.0))
                .justify_between(),
            "code-font-size-selector-menu",
            &code_font_size_handle,
            MenuAlign::BelowRight,
            move |_| {
                FONT_SIZES
                    .into_iter()
                    .map(|size| {
                        let weak = weak.clone();
                        MenuItem::new(font_size_label(size), move |_, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.set_code_font_size(size, cx);
                            });
                        })
                        .selected(size == selected_code_font_size)
                    })
                    .collect()
            },
        );

        let weak = cx.entity().downgrade();
        let language_handle = self.menu_handle("language-selector", cx);
        let language_selector = dropdown_menu(
            MenuChip::new("language-selector")
                .label(selected_language.label())
                .outlined()
                .selected(language_handle.is_open())
                .w(px(116.0))
                .justify_between(),
            "language-selector-menu",
            &language_handle,
            MenuAlign::BelowRight,
            move |_| {
                crate::i18n::AppLanguage::ALL
                    .into_iter()
                    .map(|language| {
                        let weak = weak.clone();
                        MenuItem::new(language.label(), move |window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.set_language(language, window, cx);
                            });
                        })
                        .selected(language == selected_language)
                    })
                    .collect()
            },
        );

        div()
            .mt(px(15.0))
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(13.0))
            .overflow_hidden()
            .bg(theme.raised)
            .child(
                div()
                    .w_full()
                    .min_h(px(60.0))
                    .px(px(20.0))
                    .py(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(24.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(sp(13.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(tr!("settings.theme")),
                            )
                            .child(
                                div()
                                    .mt(px(5.0))
                                    .text_size(sp(12.5))
                                    .line_height(sp(18.0))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("settings.theme_description")),
                            ),
                    )
                    .child(theme_selector),
            )
            .child(div().mx(px(20.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .w_full()
                    .min_h(px(60.0))
                    .px(px(20.0))
                    .py(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(24.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(sp(13.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(tr!("language.title")),
                            )
                            .child(
                                div()
                                    .mt(px(5.0))
                                    .text_size(sp(12.5))
                                    .line_height(sp(18.0))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("language.description")),
                            ),
                    )
                    .child(language_selector),
            )
            .child(div().mx(px(20.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .w_full()
                    .min_h(px(60.0))
                    .px(px(20.0))
                    .py(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(24.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(sp(13.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(tr!("settings.ui_font_size")),
                            )
                            .child(
                                div()
                                    .mt(px(5.0))
                                    .text_size(sp(12.5))
                                    .line_height(sp(18.0))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("settings.ui_font_size_description")),
                            ),
                    )
                    .child(ui_font_size_selector),
            )
            .child(div().mx(px(20.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .w_full()
                    .min_h(px(60.0))
                    .px(px(20.0))
                    .py(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(24.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(sp(13.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(tr!("settings.code_font_size")),
                            )
                            .child(
                                div()
                                    .mt(px(5.0))
                                    .text_size(sp(12.5))
                                    .line_height(sp(18.0))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("settings.code_font_size_description")),
                            ),
                    )
                    .child(code_font_size_selector),
            )
            .into_any_element()
    }

    fn set_ui_font_size(&mut self, size: f32, window: &mut Window, cx: &mut Context<Self>) {
        let size = waku_client::persistence::sanitized_ui_font_size(size);
        if self.state.ui_font_size == size {
            return;
        }
        self.state.ui_font_size = size;
        // Chrome is authored in `sp` rems; the rem size is the setting.
        window.set_rem_size(px(size));
        self.remeasure_font_sized_surfaces();
        self.save();
        window.refresh();
        cx.notify();
    }

    fn set_code_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        let size = waku_client::persistence::sanitized_code_font_size(size);
        if self.state.code_font_size == size {
            return;
        }
        self.state.code_font_size = size;
        self.remeasure_font_sized_surfaces();
        self.save();
        cx.notify();
    }

    /// Drop every cached row height that a font size participates in. The
    /// virtualized lists remember measured heights, so a stale entry would
    /// misplace scroll anchors until the row happened to remeasure. The
    /// sidebar list keeps its uniform row height and needs no reset.
    fn remeasure_font_sized_surfaces(&self) {
        self.reset_transcript_rows(self.transcript_row_count());
        let line_count = self
            .right_panel_diff_snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.lines.len());
        self.right_panel_diff_list_state.reset(line_count);
        self.right_panel_diff_tree_list_state
            .reset(self.right_panel_diff_tree_rows.borrow().len());
        self.skills_list_state
            .reset(self.skills_rows.borrow().len());
    }

    fn render_providers_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let checking = self.provider_detection_remaining > 0;
        let checked_label = self
            .provider_detection_checked_at
            .filter(|_| !checking)
            .map(|checked_at| detection_checked_label(checked_at.elapsed()));

        let refresh = div()
            .id("refresh-providers")
            .tab_index(0)
            .focus_visible(|style| style.border_color(theme.accent))
            .h(px(28.0))
            .px(px(11.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_default()
            .text_size(sp(12.5))
            .text_color(theme.text_secondary)
            .opacity(if checking { 0.6 } else { 1.0 })
            .hover(|element| element.bg(theme.overlay))
            .child(icon("icons/rotate-cw.svg", 11.0, theme.text_tertiary))
            .child(if checking {
                tr!("common.checking")
            } else {
                tr!("common.refresh")
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.refresh_provider_detection(None);
                cx.notify();
            }));

        let mut rows = div().mt(px(4.0)).flex().flex_col();
        let provider_count = ProviderKind::ALL.len();
        for (index, kind) in ProviderKind::ALL.into_iter().enumerate() {
            let probe = self.provider_probe(kind);
            let installed = probe.is_some_and(|probe| probe.installed);
            let binary_path = probe
                .filter(|probe| probe.installed)
                .and_then(|probe| probe.path.as_deref())
                .map(|path| abbreviate_home_path(path, self.home_directory.as_deref()));
            let model_count = probe.map(|probe| probe.models.len()).unwrap_or(0);
            let version = self
                .provider_versions
                .get(&kind)
                .and_then(|version| version.clone());
            let disabled = self.state.disabled_providers.contains(&kind);

            let dot_color = if !installed {
                theme.text_ghost
            } else if disabled {
                theme.warning
            } else {
                theme.success
            };

            let detail: AnyElement = if installed {
                let mut parts = Vec::new();
                if let Some(path) = binary_path {
                    parts.push(path);
                }
                if disabled {
                    parts.push(tr!("providers.disabled_for_new_tasks"));
                } else if model_count > 0 {
                    parts.push(if model_count == 1 {
                        tr!("providers.model_count_one", count = model_count)
                    } else {
                        tr!("providers.model_count_many", count = model_count)
                    });
                }
                div()
                    .truncate()
                    .child(SharedString::from(parts.join("  ·  ")))
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .items_baseline()
                    .child(SharedString::from(tr!(
                        "providers.not_detected_as",
                        command = kind.command()
                    )))
                    .into_any_element()
            };

            let toggle_on = !disabled;
            let toggle = toggle_switch(
                SharedString::from(format!("provider-enabled-{}", kind.id())),
                toggle_on,
                false,
                theme,
                cx,
                move |this, _, cx| this.set_provider_enabled(kind, disabled, cx),
            );

            let expanded = self.expanded_provider_settings == Some(kind);
            let expand_button = icon_button(
                SharedString::from(format!("provider-expand-{}", kind.id())),
                if expanded {
                    "icons/chevron-down.svg"
                } else {
                    "icons/chevron-right.svg"
                },
                theme,
            )
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_provider_expanded(kind, window, cx);
            }));

            let header = div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .relative()
                        .w(px(30.0))
                        .h(px(30.0))
                        .flex_none()
                        .rounded(px(7.0))
                        .bg(theme.overlay)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(provider_mark(
                            kind,
                            16.0,
                            provider_color(&theme, kind).opacity(if installed { 1.0 } else { 0.5 }),
                        ))
                        .child(
                            div()
                                .absolute()
                                .bottom(px(-2.0))
                                .right(px(-2.0))
                                .w(px(10.0))
                                .h(px(10.0))
                                .rounded_full()
                                .border_2()
                                .border_color(theme.raised)
                                .bg(dot_color),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .flex()
                                .items_baseline()
                                .gap(px(7.0))
                                .child(
                                    div()
                                        .text_size(sp(12.5))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(if installed {
                                            theme.text
                                        } else {
                                            theme.text_secondary
                                        })
                                        .child(kind.display_name()),
                                )
                                .when_some(version, |element, version| {
                                    element.child(
                                        div()
                                            .font_family(crate::md::render::MONO_FAMILY)
                                            .text_size(sp(12.5))
                                            .text_color(theme.text_tertiary)
                                            .child(SharedString::from(format!("v{version}"))),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .mt(px(3.0))
                                .text_size(sp(12.5))
                                .text_color(theme.text_tertiary)
                                .child(detail),
                        ),
                )
                .child(expand_button)
                .when(installed, |element| element.child(toggle));

            rows = rows.child(
                div()
                    .py(px(11.0))
                    .flex()
                    .flex_col()
                    .when(index + 1 != provider_count, |element| {
                        element.border_b_1().border_color(theme.border)
                    })
                    .child(header)
                    .when(expanded, |element| {
                        element.child(self.render_provider_expanded_settings(kind, theme, cx))
                    }),
            );
        }

        div()
            .mt(px(15.0))
            .w_full()
            .px(px(20.0))
            .py(px(14.0))
            .rounded(px(13.0))
            .bg(theme.raised)
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(20.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(sp(13.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(tr!("providers.coding_agents")),
                            )
                            .child(
                                div()
                                    .mt(px(5.0))
                                    .text_size(sp(12.5))
                                    .line_height(sp(18.0))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("providers.description")),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_col()
                            .items_end()
                            .gap(px(6.0))
                            .child(refresh)
                            .when_some(checked_label, |element, label| {
                                element.child(
                                    div()
                                        .text_size(sp(12.5))
                                        .text_color(theme.text_ghost)
                                        .child(SharedString::from(label)),
                                )
                            }),
                    ),
            )
            .child(rows)
            .into_any_element()
    }

    /// The expanded row's settings body: the binary override for this
    /// provider, with the detection result as its caption.
    fn render_provider_expanded_settings(
        &self,
        kind: ProviderKind,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let override_value = self.state.provider_binary_overrides.get(&kind).cloned();
        let full_path = self
            .provider_probe(kind)
            .filter(|probe| probe.installed)
            .and_then(|probe| probe.path.as_ref())
            .map(|path| path.display().to_string());

        let caption = match (&override_value, full_path) {
            (Some(_), Some(path)) => tr!("providers.using_override", path = path),
            (Some(_), None) => tr!("providers.invalid_override"),
            (None, Some(path)) => tr!("providers.detected_at", path = path),
            (None, None) => tr!("providers.searches_path", command = kind.command()),
        };

        let reset = div()
            .id(SharedString::from(format!(
                "provider-path-reset-{}",
                kind.id()
            )))
            .tab_index(0)
            .focus_visible(|style| style.border_color(theme.accent))
            .h(px(29.0))
            .px(px(10.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .flex_none()
            .items_center()
            .cursor_default()
            .text_size(sp(12.5))
            .text_color(theme.text_secondary)
            .hover(|element| element.bg(theme.overlay))
            .child(tr!("common.reset"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.provider_path_input
                    .update(cx, |input, cx| input.clear(cx));
                this.apply_provider_path_override(cx);
            }));

        div()
            .mt(px(10.0))
            .pl(px(42.0))
            .flex()
            .flex_col()
            .gap(px(5.0))
            .child(
                div()
                    .text_size(sp(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(tr!("providers.binary_path")),
            )
            .child(
                div()
                    .text_size(sp(12.5))
                    .line_height(sp(15.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::from(tr!(
                        "providers.binary_path_description",
                        provider = kind.short_name()
                    ))),
            )
            .child(
                div()
                    .mt(px(3.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        TextField::new(
                            SharedString::from(format!("provider-path-field-{}", kind.id())),
                            self.provider_path_input.clone(),
                        )
                        .flex_1()
                        .max_w(px(430.0)),
                    )
                    .when(override_value.is_some(), |element| element.child(reset)),
            )
            .child(
                div()
                    .text_size(sp(12.5))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from(caption)),
            )
    }

    fn toggle_provider_expanded(
        &mut self,
        provider: ProviderKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Commit any pending edit for the previously expanded provider before
        // the input is handed to another row.
        self.apply_provider_path_override(cx);
        if self.expanded_provider_settings == Some(provider) {
            self.expanded_provider_settings = None;
        } else {
            self.expanded_provider_settings = Some(provider);
            let override_value = self
                .state
                .provider_binary_overrides
                .get(&provider)
                .cloned()
                .unwrap_or_default();
            self.provider_path_input
                .update(cx, |input, cx| input.set_content(override_value, cx));
            let focus = self.provider_path_input.read(cx).focus();
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Commit the binary override edit for the expanded provider: empty means
    /// detect from PATH. Re-detects that provider and refreshes every catalog
    /// keyed by the executable path.
    pub(super) fn apply_provider_path_override(&mut self, cx: &mut Context<Self>) {
        let Some(provider) = self.expanded_provider_settings else {
            return;
        };
        let text = self
            .provider_path_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let current = self
            .state
            .provider_binary_overrides
            .get(&provider)
            .cloned()
            .unwrap_or_default();
        if text == current {
            return;
        }
        if text.is_empty() {
            self.state.provider_binary_overrides.remove(&provider);
        } else {
            self.state.provider_binary_overrides.insert(provider, text);
        }
        self.save();
        self.refresh_provider_detection(Some(provider));
        self.refresh_composer_sources(cx);
        cx.notify();
    }

    /// Providers switched off here stop offering models to new sessions;
    /// sessions already locked to them keep working.
    fn set_provider_enabled(
        &mut self,
        provider: ProviderKind,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        if enabled {
            self.state
                .disabled_providers
                .retain(|kind| *kind != provider);
        } else if !self.state.disabled_providers.contains(&provider) {
            self.state.disabled_providers.push(provider);
        }
        if !enabled
            && let Some(fallback) = ProviderKind::ALL
                .into_iter()
                .find(|kind| self.provider_enabled(*kind))
        {
            // New work must land somewhere usable: move the new-session
            // default and any unstarted drafts off the switched-off provider.
            // The remembered model belongs to the old provider, so it resets
            // with it.
            self.state.replace_new_session_provider(provider, fallback);
            let draft_ids = self
                .state
                .sessions
                .iter()
                .filter(|session| session.provider == provider && !session.has_started())
                .map(|session| session.id)
                .collect::<Vec<_>>();
            for id in draft_ids {
                if let Some(session) = self.state.session_mut(id) {
                    session.provider = fallback;
                    session.model = None;
                    session.reasoning_effort = None;
                    session.service_tier = None;
                    session.context_window = None;
                }
            }
        }
        self.save();
        self.refresh_delegation_model_index();
        cx.notify();
    }

    fn set_theme_preference(
        &mut self,
        preference: ThemePreference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.theme == preference {
            return;
        }
        self.state.theme = preference;
        crate::theme::apply_theme_preference(preference, window, cx);
        self.save();
        cx.notify();
    }

    fn set_language(
        &mut self,
        language: crate::i18n::AppLanguage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.language == language {
            return;
        }

        self.state.language = language;
        crate::i18n::set_language(language);

        self.composer.update(cx, |input, cx| {
            input.set_placeholder(tr!("input.do_anything"), cx)
        });
        self.model_search.update(cx, |input, cx| {
            input.set_placeholder(tr!("input.search_models"), cx)
        });
        self.branch_search.update(cx, |input, cx| {
            input.set_placeholder(tr!("input.search_branches"), cx)
        });
        self.project_search.update(cx, |input, cx| {
            input.set_placeholder(tr!("input.search_projects"), cx)
        });
        self.skills_search.update(cx, |input, cx| {
            input.set_placeholder(tr!("skills.search"), cx)
        });
        self.provider_path_input.update(cx, |input, cx| {
            input.set_placeholder(tr!("input.detected_automatically"), cx)
        });
        self.refresh_command_palette_localized_text(cx);
        self.refresh_file_search_localized_text(cx);
        self.refresh_transcript_search_localized_text(cx);
        for browser in self.right_panel_browsers.values() {
            browser.update(cx, |browser, cx| browser.refresh_localized_text(cx));
        }
        for terminal in self.right_panel_terminals.values() {
            terminal.update(cx, |terminal, cx| terminal.refresh_localized_text(cx));
        }
        for probe in &mut self.probes {
            probe.models = crate::model_catalog::fallback_models(probe.provider);
        }
        self.assistant_footer_fingerprint.set(None);
        self.refresh_provider_detection(None);
        self.invalidate_composer_sources(cx);

        let updater_available = cx
            .try_global::<crate::updater::UpdaterState>()
            .and_then(|updater| updater.0.as_ref())
            .is_some();
        crate::set_app_menus(cx, updater_available);
        self.save();
        window.refresh();
        cx.notify();
    }
}

/// Sizes offered by the font-size dropdowns. A hand-edited `app.json` may
/// hold values outside this list; they render as-is and simply select
/// nothing here.
const FONT_SIZES: [f32; 8] = [11.0, 12.0, 13.0, 14.0, 15.0, 16.0, 18.0, 20.0];

fn font_size_label(size: f32) -> String {
    if size.fract() == 0.0 {
        format!("{size:.0} px")
    } else {
        format!("{size} px")
    }
}

/// "Checked …" caption for the Providers page. Recomputed whenever the page
/// redraws; precision beyond the minute is noise here.
fn detection_checked_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 90 {
        tr!("providers.checked_just_now")
    } else if seconds < 3600 {
        tr!("providers.checked_minutes_ago", count = seconds / 60)
    } else {
        tr!("providers.checked_hours_ago", count = seconds / 3600)
    }
}

/// Keep the full binary path, abbreviating only the user's home directory.
fn abbreviate_home_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(relative) if relative.as_os_str().is_empty() => "~".to_owned(),
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::abbreviate_home_path;
    use std::path::Path;

    #[test]
    fn provider_paths_abbreviate_only_the_home_prefix() {
        let home = Path::new("/Users/example");

        assert_eq!(
            abbreviate_home_path(Path::new("/Users/example/.local/bin/amp"), Some(home)),
            "~/.local/bin/amp"
        );
        assert_eq!(
            abbreviate_home_path(Path::new("/opt/homebrew/bin/codex"), Some(home)),
            "/opt/homebrew/bin/codex"
        );
    }
}
