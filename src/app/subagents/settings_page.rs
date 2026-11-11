//! Settings → Subagents: the list of agents a task may delegate to, and the
//! inline editor for one of them.

use super::super::*;
use crate::model::SubagentDefinition;

pub(in crate::app) struct SubagentSettingsState {
    pub(in crate::app) name: Entity<TextInput>,
    pub(in crate::app) description: Entity<TextInput>,
    pub(in crate::app) instructions: Entity<TextInput>,
    /// Index into the stored list of the agent whose editor is open.
    editing: Option<usize>,
    /// Text edits are applied in memory as they are typed and written once
    /// the editor closes; a save per keystroke would be a daemon round trip.
    dirty: bool,
}

impl SubagentSettingsState {
    pub(in crate::app) fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            name: cx
                .new(|cx| TextInput::new(window, cx).placeholder(tr!("subagent.name_placeholder"))),
            description: cx.new(|cx| {
                TextInput::new(window, cx).placeholder(tr!("subagent.description_placeholder"))
            }),
            instructions: cx.new(|cx| {
                TextInput::new(window, cx)
                    .multi_line()
                    .placeholder(tr!("subagent.instructions_placeholder"))
            }),
            editing: None,
            dirty: false,
        }
    }
}

/// A name no other agent uses: `agent`, then `agent-2`, `agent-3`, …
fn unused_agent_name(agents: &[SubagentDefinition]) -> String {
    let taken = |name: &str| agents.iter().any(|agent| agent.name == name);
    if !taken("agent") {
        return "agent".to_owned();
    }
    (2..)
        .map(|suffix| format!("agent-{suffix}"))
        .find(|name| !taken(name))
        .unwrap_or_else(|| "agent".to_owned())
}

/// Whether the name field's text may replace agent `index`'s name.
fn name_is_usable(agents: &[SubagentDefinition], index: usize, name: &str) -> bool {
    SubagentDefinition::is_valid_name(name)
        && !agents
            .iter()
            .enumerate()
            .any(|(other, agent)| other != index && agent.name == name)
}

impl Raku {
    /// Apply the editor's text fields to the agent being edited. An invalid
    /// or duplicate name stays in the field, flagged, and leaves the stored
    /// name alone.
    pub(in crate::app) fn sync_subagent_editor(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.subagents.settings.editing else {
            return;
        };
        let name = self
            .subagents
            .settings
            .name
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let description = self
            .subagents
            .settings
            .description
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let instructions = self
            .subagents
            .settings
            .instructions
            .read(cx)
            .content()
            .to_owned();
        let rename = name_is_usable(&self.state.subagents, index, &name);
        let Some(agent) = self.state.subagents.get_mut(index) else {
            return;
        };
        let mut changed = false;
        if !agent.builtin && rename && agent.name != name {
            agent.name = name;
            changed = true;
        }
        if agent.description != description {
            agent.description = description;
            changed = true;
        }
        if agent.instructions != instructions {
            agent.instructions = instructions;
            changed = true;
        }
        if changed {
            self.subagents.settings.dirty = true;
            self.invalidate_agent_completions();
            self.composer_sources_stale = true;
            cx.notify();
        }
    }

    /// Write pending agent edits. Called wherever the editor is left.
    pub(in crate::app) fn commit_subagent_settings(&mut self, cx: &mut Context<Self>) {
        self.sync_subagent_editor(cx);
        if std::mem::take(&mut self.subagents.settings.dirty) {
            self.release_runtimes_for_agent_change();
            self.save();
        }
    }

    fn load_subagent_editor(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(agent) = self.state.subagents.get(index).cloned() else {
            self.subagents.settings.editing = None;
            return;
        };
        self.subagents.settings.editing = Some(index);
        let settings = &self.subagents.settings;
        settings
            .name
            .update(cx, |input, cx| input.set_content(agent.name, cx));
        settings
            .description
            .update(cx, |input, cx| input.set_content(agent.description, cx));
        settings
            .instructions
            .update(cx, |input, cx| input.set_content(agent.instructions, cx));
    }

    fn toggle_subagent_editor(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_subagent_settings(cx);
        if self.subagents.settings.editing == Some(index) {
            self.subagents.settings.editing = None;
        } else {
            self.load_subagent_editor(index, cx);
            let field = if self
                .state
                .subagents
                .get(index)
                .is_some_and(|agent| agent.builtin)
            {
                &self.subagents.settings.description
            } else {
                &self.subagents.settings.name
            };
            let focus = field.read(cx).focus();
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    fn add_subagent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_subagent_settings(cx);
        self.state.subagents.push(SubagentDefinition {
            name: unused_agent_name(&self.state.subagents),
            description: String::new(),
            provider: None,
            model: None,
            reasoning_effort: None,
            read_only: false,
            instructions: String::new(),
            enabled: true,
            builtin: false,
        });
        let index = self.state.subagents.len() - 1;
        self.load_subagent_editor(index, cx);
        let focus = self.subagents.settings.name.read(cx).focus();
        window.focus(&focus, cx);
        self.subagent_settings_changed(cx);
    }

    fn delete_subagent(&mut self, index: usize, cx: &mut Context<Self>) {
        if self
            .state
            .subagents
            .get(index)
            .is_none_or(|agent| agent.builtin)
        {
            return;
        }
        self.state.subagents.remove(index);
        self.subagents.settings.editing = None;
        self.subagent_settings_changed(cx);
    }

    fn move_subagent(&mut self, index: usize, up: bool, cx: &mut Context<Self>) {
        let target = if up {
            index.checked_sub(1)
        } else {
            Some(index + 1).filter(|target| *target < self.state.subagents.len())
        };
        let Some(target) = target else {
            return;
        };
        self.sync_subagent_editor(cx);
        self.state.subagents.swap(index, target);
        self.subagents.settings.editing = self.subagents.settings.editing.map(|editing| {
            if editing == index {
                target
            } else if editing == target {
                index
            } else {
                editing
            }
        });
        self.subagent_settings_changed(cx);
    }

    fn update_subagent(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut SubagentDefinition),
    ) {
        if let Some(agent) = self.state.subagents.get_mut(index) {
            update(agent);
            self.subagent_settings_changed(cx);
        }
    }

    /// A structural change: saved at once, and visible to the composer's
    /// `@` list on its next refresh.
    fn subagent_settings_changed(&mut self, cx: &mut Context<Self>) {
        self.subagents.settings.dirty = false;
        self.release_runtimes_for_agent_change();
        self.invalidate_agent_completions();
        self.composer_sources_stale = true;
        self.save();
        cx.notify();
    }

    pub(in crate::app) fn render_subagent_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let editing = self.subagents.settings.editing;
        let count = self.state.subagents.len();
        // An agent on a fixed provider needs that provider; one that runs on
        // the task's provider needs any.
        let any_provider_available = ProviderKind::ALL
            .into_iter()
            .any(|provider| self.provider_is_available(provider));

        let mut card = div()
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(13.0))
            .overflow_hidden()
            .bg(theme.raised);
        for (index, agent) in self.state.subagents.iter().enumerate() {
            if index > 0 {
                card = card.child(div().mx(px(20.0)).h(px(1.0)).bg(theme.border));
            }
            let expanded = editing == Some(index);
            let available = agent.provider.map_or(any_provider_available, |provider| {
                self.provider_is_available(provider)
            });
            let enabled = agent.enabled;
            let toggle = toggle_switch(
                SharedString::from(format!("subagent-enabled-{index}")),
                enabled,
                false,
                theme,
                cx,
                move |this, _, cx| {
                    this.update_subagent(index, cx, |agent| agent.enabled = !enabled);
                },
            );
            let expand = icon_button(
                SharedString::from(format!("subagent-expand-{index}")),
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
                this.toggle_subagent_editor(index, window, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.toggle_subagent_editor(index, window, cx);
                    cx.stop_propagation();
                }
            }));
            let header = div()
                .w_full()
                .min_h(px(60.0))
                .px(px(20.0))
                .py(px(12.0))
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .size(px(30.0))
                        .flex_none()
                        .rounded(px(7.0))
                        .bg(theme.overlay)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon(
                            "icons/bot.svg",
                            16.0,
                            if enabled && available {
                                theme.text_secondary
                            } else {
                                theme.text_ghost
                            },
                        )),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(sp(13.5))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(format!("@{}", agent.name)),
                                )
                                .when(agent.builtin, |row| {
                                    row.child(
                                        div()
                                            .flex_none()
                                            .px(px(6.0))
                                            .rounded(px(5.0))
                                            .bg(theme.overlay)
                                            .text_size(sp(11.0))
                                            .text_color(theme.text_tertiary)
                                            .child(tr!("subagent.builtin")),
                                    )
                                })
                                .when(!available, |row| {
                                    // The warning is an icon and words, not
                                    // a tint alone.
                                    row.child(
                                        div()
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .gap(px(4.0))
                                            .text_size(sp(11.5))
                                            .text_color(theme.danger)
                                            .child(icon("icons/alert.svg", 11.0, theme.danger))
                                            .child(tr!("subagent.unavailable")),
                                    )
                                }),
                        )
                        .when(!agent.description.is_empty(), |column| {
                            column.child(
                                div()
                                    .truncate()
                                    .text_size(sp(SMALL_UI_FONT_SIZE))
                                    .text_color(theme.text_tertiary)
                                    .child(single_line_label(&agent.description)),
                            )
                        }),
                )
                .child(toggle)
                .child(expand);
            card = card.child(header);
            if expanded {
                card = card.child(self.render_subagent_editor(index, agent, count, cx));
            }
        }

        let add = div()
            .id("subagent-add")
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
            .gap(px(6.0))
            .cursor_default()
            .text_size(sp(SMALL_UI_FONT_SIZE))
            .text_color(theme.text_secondary)
            .hover(|element| element.bg(theme.overlay))
            .child(icon("icons/plus.svg", 12.0, theme.text_secondary))
            .child(tr!("subagent.add"))
            .on_click(cx.listener(|this, _, window, cx| this.add_subagent(window, cx)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.add_subagent(window, cx);
                    cx.stop_propagation();
                }
            }));

        div()
            .mt(px(15.0))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(card)
            .child(div().flex().child(add))
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .px(px(4.0))
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .line_height(sp(18.0))
                    .text_color(theme.text_secondary)
                    .whitespace_normal()
                    .child(tr!("subagent.settings_description")),
            )
            .into_any_element()
    }

    fn render_subagent_editor(
        &self,
        index: usize,
        agent: &SubagentDefinition,
        count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let settings = &self.subagents.settings;
        let field_row = |title: String, control: AnyElement| {
            div()
                .w_full()
                .min_h(px(44.0))
                .flex()
                .items_center()
                .gap(px(24.0))
                .child(
                    div()
                        .w(px(110.0))
                        .flex_none()
                        .text_size(sp(SMALL_UI_FONT_SIZE))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(title),
                )
                .child(div().flex_1().min_w_0().flex().justify_end().child(control))
        };

        // Name: fixed for the built-ins, which the tool and docs refer to.
        let typed_name = settings.name.read(cx).content().trim().to_owned();
        let name_invalid = !agent.builtin && typed_name != agent.name;
        let name_control = if agent.builtin {
            div()
                .text_size(sp(SMALL_UI_FONT_SIZE))
                .text_color(theme.text_secondary)
                .child(agent.name.clone())
                .into_any_element()
        } else {
            TextField::new("subagent-name-field", settings.name.clone())
                .w(px(220.0))
                .into_any_element()
        };
        let name_hint = (!agent.builtin).then(|| {
            div()
                .w_full()
                .flex()
                .justify_end()
                .items_center()
                .gap(px(4.0))
                .text_size(sp(11.5))
                .text_color(if name_invalid {
                    theme.danger
                } else {
                    theme.text_ghost
                })
                .when(name_invalid, |hint| {
                    hint.child(icon("icons/alert.svg", 11.0, theme.danger))
                })
                .child(if name_invalid {
                    tr!("subagent.name_invalid")
                } else {
                    tr!("subagent.name_hint")
                })
        });

        // Provider: the task's own, or a fixed one.
        let providers = ProviderKind::ALL
            .into_iter()
            .filter(|provider| !self.state.disabled_providers.contains(provider))
            .filter(|provider| {
                self.provider_probe(*provider)
                    .is_some_and(|probe| probe.path.is_some())
            })
            .collect::<Vec<_>>();
        let selected_provider = agent.provider;
        let provider_handle = self.menu_handle("subagent-provider-selector", cx);
        let weak = cx.entity().downgrade();
        let provider_selector = dropdown_menu(
            MenuChip::new("subagent-provider-selector")
                .label(match selected_provider {
                    Some(provider) => provider.display_name().to_owned(),
                    None => tr!("subagent.same_as_task"),
                })
                .outlined()
                .selected(provider_handle.is_open())
                .w(px(220.0))
                .justify_between(),
            "subagent-provider-selector-menu",
            &provider_handle,
            MenuAlign::BelowRight,
            move |_| {
                let choose = |label: String, provider: Option<ProviderKind>| {
                    let weak = weak.clone();
                    MenuItem::new(label, move |_, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.update_subagent(index, cx, |agent| {
                                if agent.provider != provider {
                                    agent.provider = provider;
                                    agent.model = None;
                                    agent.reasoning_effort = None;
                                }
                            });
                        });
                    })
                    .selected(selected_provider == provider)
                };
                std::iter::once(choose(tr!("subagent.same_as_task"), None))
                    .chain(providers.iter().map(|provider| {
                        choose(provider.display_name().to_owned(), Some(*provider))
                    }))
                    .collect()
            },
        );

        // Model: only a fixed provider has a catalog to choose from.
        let model_probe = selected_provider.and_then(|provider| self.provider_probe(provider));
        let models = model_probe
            .map(|probe| {
                probe
                    .models
                    .iter()
                    .map(|model| {
                        let label = match &model.sub_provider {
                            Some(sub) => format!("{sub} · {}", model.name),
                            None => model.name.clone(),
                        };
                        (model.id.clone(), label)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let selected_model = agent.model.clone();
        let model_label = match (&selected_model, selected_provider) {
            (Some(model), Some(provider)) => self.model_display_name(provider, Some(model)),
            (_, None) => tr!("subagent.same_as_task"),
            (None, Some(_)) => tr!("subagent.default_model"),
        };
        let model_handle = self.menu_handle("subagent-model-selector", cx);
        let weak = cx.entity().downgrade();
        let model_selector = dropdown_menu(
            MenuChip::new("subagent-model-selector")
                .label(model_label)
                .outlined()
                .disabled(models.is_empty())
                .selected(model_handle.is_open())
                .w(px(220.0))
                .justify_between(),
            "subagent-model-selector-menu",
            &model_handle,
            MenuAlign::BelowRight,
            move |_| {
                let choose = |label: String, model: Option<String>| {
                    let weak = weak.clone();
                    let selected = selected_model == model;
                    MenuItem::new(label, move |_, cx| {
                        let model = model.clone();
                        let _ = weak.update(cx, |this, cx| {
                            this.update_subagent(index, cx, |agent| {
                                if agent.model != model {
                                    agent.model = model;
                                    agent.reasoning_effort = None;
                                }
                            });
                        });
                    })
                    .selected(selected)
                };
                std::iter::once(choose(tr!("subagent.default_model"), None))
                    .chain(
                        models
                            .iter()
                            .map(|(id, label)| choose(label.clone(), Some(id.clone()))),
                    )
                    .collect()
            },
        );

        // Reasoning: offered once a model with effort options is chosen.
        let effort_selector = model_probe
            .zip(agent.model.as_deref())
            .and_then(|(probe, model)| probe.model(model))
            .filter(|model| !model.reasoning_efforts.is_empty())
            .map(|model| {
                let efforts = model.reasoning_efforts.clone();
                let selected_effort = agent.reasoning_effort.clone();
                let label = selected_effort
                    .as_deref()
                    .and_then(|effort| efforts.iter().find(|option| option.id == effort))
                    .map(|option| option.label.clone())
                    .unwrap_or_else(|| tr!("subagent.default_model"));
                let handle = self.menu_handle("subagent-effort-selector", cx);
                let weak = cx.entity().downgrade();
                dropdown_menu(
                    MenuChip::new("subagent-effort-selector")
                        .label(label)
                        .outlined()
                        .selected(handle.is_open())
                        .w(px(220.0))
                        .justify_between(),
                    "subagent-effort-selector-menu",
                    &handle,
                    MenuAlign::BelowRight,
                    move |_| {
                        let choose = |label: String, effort: Option<String>| {
                            let weak = weak.clone();
                            let selected = selected_effort == effort;
                            MenuItem::new(label, move |_, cx| {
                                let effort = effort.clone();
                                let _ = weak.update(cx, |this, cx| {
                                    this.update_subagent(index, cx, |agent| {
                                        agent.reasoning_effort = effort;
                                    });
                                });
                            })
                            .selected(selected)
                        };
                        std::iter::once(choose(tr!("subagent.default_model"), None))
                            .chain(efforts.iter().map(|option| {
                                choose(option.label.clone(), Some(option.id.clone()))
                            }))
                            .collect()
                    },
                )
                .into_any_element()
            });

        let read_only = agent.read_only;
        let access_handle = self.menu_handle("subagent-access-selector", cx);
        let weak = cx.entity().downgrade();
        let access_selector = dropdown_menu(
            MenuChip::new("subagent-access-selector")
                .label(if read_only {
                    tr!("subagent.access_read_only")
                } else {
                    tr!("subagent.access_same_as_task")
                })
                .outlined()
                .selected(access_handle.is_open())
                .w(px(220.0))
                .justify_between(),
            "subagent-access-selector-menu",
            &access_handle,
            MenuAlign::BelowRight,
            move |_| {
                [
                    (tr!("subagent.access_read_only"), true),
                    (tr!("subagent.access_same_as_task"), false),
                ]
                .into_iter()
                .map(|(label, value)| {
                    let weak = weak.clone();
                    MenuItem::new(label, move |_, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.update_subagent(index, cx, |agent| agent.read_only = value);
                        });
                    })
                    .selected(read_only == value)
                })
                .collect()
            },
        );

        let action = |id: &'static str,
                      path: &'static str,
                      label: String,
                      disabled: bool,
                      run: fn(&mut Self, usize, &mut Context<Self>)| {
            div()
                .id(id)
                .tab_index(0)
                .focus_visible(|style| style.border_color(theme.accent))
                .h(px(27.0))
                .px(px(9.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(theme.border_strong)
                .flex()
                .flex_none()
                .items_center()
                .gap(px(5.0))
                .cursor_default()
                .text_size(sp(SMALL_UI_FONT_SIZE))
                .text_color(theme.text_secondary)
                .when(disabled, |button| button.opacity(0.45))
                .when(!disabled, |button| {
                    button
                        .hover(|element| element.bg(theme.overlay))
                        .on_click(cx.listener(move |this, _, _, cx| run(this, index, cx)))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                run(this, index, cx);
                                cx.stop_propagation();
                            }
                        }))
                })
                .child(icon(path, 11.0, theme.text_secondary))
                .child(label)
        };
        let actions = div()
            .w_full()
            .pt(px(6.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(action(
                "subagent-move-up",
                "icons/chevron-up.svg",
                tr!("subagent.move_up"),
                index == 0,
                |this, index, cx| this.move_subagent(index, true, cx),
            ))
            .child(action(
                "subagent-move-down",
                "icons/chevron-down.svg",
                tr!("subagent.move_down"),
                index + 1 >= count,
                |this, index, cx| this.move_subagent(index, false, cx),
            ))
            .when(!agent.builtin, |row| {
                row.child(div().flex_1()).child(action(
                    "subagent-delete",
                    "icons/trash.svg",
                    tr!("subagent.delete"),
                    false,
                    |this, index, cx| this.delete_subagent(index, cx),
                ))
            });

        div()
            .w_full()
            .px(px(20.0))
            .pb(px(16.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(field_row(tr!("subagent.name"), name_control))
            .children(name_hint)
            .child(field_row(
                tr!("subagent.description"),
                TextField::new("subagent-description-field", settings.description.clone())
                    .w_full()
                    .max_w(px(360.0))
                    .into_any_element(),
            ))
            .child(field_row(
                tr!("side_chat.provider"),
                provider_selector.into_any_element(),
            ))
            .child(field_row(
                tr!("side_chat.model"),
                model_selector.into_any_element(),
            ))
            .when_some(effort_selector, |form, selector| {
                form.child(field_row(tr!("models.reasoning"), selector))
            })
            .child(field_row(
                tr!("subagent.access"),
                access_selector.into_any_element(),
            ))
            .child(
                div()
                    .pt(px(8.0))
                    .pb(px(6.0))
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(tr!("subagent.instructions")),
            )
            .child(
                div()
                    .id("subagent-instructions-field")
                    .w_full()
                    .h(px(112.0))
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(theme.inset)
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .line_height(sp(18.0))
                    .text_color(theme.text)
                    .child(settings.instructions.clone()),
            )
            .child(actions)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_agents_get_an_unused_valid_name() {
        let mut agents = SubagentDefinition::builtins();
        let first = unused_agent_name(&agents);
        assert_eq!(first, "agent");
        assert!(SubagentDefinition::is_valid_name(&first));
        agents.push(SubagentDefinition {
            name: first,
            ..agents[0].clone()
        });
        assert_eq!(unused_agent_name(&agents), "agent-2");
    }

    #[test]
    fn a_name_must_be_valid_and_unique_to_replace_another() {
        let agents = SubagentDefinition::builtins();
        assert!(name_is_usable(&agents, 0, "explore"));
        assert!(name_is_usable(&agents, 0, "scout"));
        assert!(!name_is_usable(&agents, 0, "general"));
        assert!(!name_is_usable(&agents, 0, "Scout"));
        assert!(!name_is_usable(&agents, 0, "two words"));
        assert!(!name_is_usable(&agents, 0, ""));
    }
}
