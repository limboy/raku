//! The "text generation" model: a provider and model the user picks to write
//! short labels, currently headlines for finished reasoning blocks.
//!
//! Generation is off until a provider is chosen, because every label is a
//! model request. Once on, each reasoning block that finishes while Raku is
//! running gets one background request through the daemon; the headline is
//! stored on the block and persisted with the transcript. Older reasoning is
//! never backfilled, and its row keeps the opening-line fallback.

use super::side_chat::{resolve_side_chat_reasoning_effort, resolve_side_chat_target};
use super::*;

/// Concurrent headline requests across all sessions. Thinking blocks can end
/// every few seconds; a short queue keeps that from fanning out into a burst
/// of provider processes.
const MAX_HEADLINES_IN_FLIGHT: usize = 2;
/// Transcript blocks scanned per drain. A finished block is always near the
/// tail, so the scan stays independent of transcript length.
const SCANNED_TAIL_BLOCKS: usize = 2;

#[derive(Default)]
pub(super) struct ReasoningHeadlines {
    /// Reasoning activities already requested, successfully or not. A failed
    /// request is not retried; the row keeps its fallback.
    requested: HashSet<Uuid>,
    in_flight: usize,
    /// Reasoning that finished before this moment predates this run and is not
    /// backfilled.
    since_ms: u64,
}

impl ReasoningHeadlines {
    pub(super) fn new() -> Self {
        Self {
            since_ms: unix_time_millis(),
            ..Self::default()
        }
    }
}

impl Raku {
    /// The provider and model that write labels, or `None` while generation
    /// is off or the chosen provider is unavailable.
    pub(super) fn text_generation_target(&self) -> Option<(ProviderKind, Option<String>)> {
        resolve_side_chat_target(
            &self.probes,
            &self.state.disabled_providers,
            self.state.text_generation_provider?,
            self.state.text_generation_model.as_deref(),
        )
    }

    fn text_generation_invocation(&self) -> Option<crate::git_commit::AgentInvocation> {
        let (provider, model) = self.text_generation_target()?;
        let probe = self.provider_probe(provider)?;
        let reasoning_effort = model
            .as_deref()
            .and_then(|model| probe.model(model))
            .and_then(|model| {
                resolve_side_chat_reasoning_effort(
                    model,
                    self.state.text_generation_reasoning_effort.as_deref(),
                )
            })
            .map(str::to_owned);
        Some(crate::git_commit::AgentInvocation {
            provider,
            binary: probe.path.clone()?,
            model,
            reasoning_effort,
        })
    }

    /// Request headlines for reasoning in `session_id` that finished during
    /// this run. Called after a drain changed the session; reads only the
    /// transcript tail.
    pub(super) fn queue_reasoning_headlines(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if self.reasoning_headlines.in_flight >= MAX_HEADLINES_IN_FLIGHT
            || self.state.text_generation_provider.is_none()
        {
            return;
        }
        let Some(session) = self.state.sessions.iter().find(|s| s.id == session_id) else {
            return;
        };
        let since_ms = self.reasoning_headlines.since_ms;
        let pending = session
            .transcript_blocks
            .iter()
            .rev()
            .take(SCANNED_TAIL_BLOCKS)
            .flat_map(|block| block.activities.iter())
            .filter(|activity| activity.complete && !activity.failed)
            .filter(|activity| !self.reasoning_headlines.requested.contains(&activity.id))
            .filter_map(|activity| {
                let reasoning = activity.reasoning.as_ref()?;
                (reasoning.headline.is_none()
                    && reasoning.started_at_ms != 0
                    && reasoning.finished_at_ms >= since_ms
                    && !reasoning.content.trim().is_empty())
                .then(|| (activity.id, reasoning.content.clone()))
            })
            .take(MAX_HEADLINES_IN_FLIGHT - self.reasoning_headlines.in_flight)
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return;
        }
        let Some(invocation) = self.text_generation_invocation() else {
            return;
        };
        for (activity_id, reasoning) in pending {
            self.reasoning_headlines.requested.insert(activity_id);
            self.reasoning_headlines.in_flight += 1;
            self.spawn_reasoning_headline(
                session_id,
                activity_id,
                reasoning,
                invocation.clone(),
                cx,
            );
        }
    }

    fn spawn_reasoning_headline(
        &mut self,
        session_id: Uuid,
        activity_id: Uuid,
        reasoning: String,
        invocation: crate::git_commit::AgentInvocation,
        cx: &mut Context<Self>,
    ) {
        let workspace_client = raku_client::WorkspaceClient::new(self.daemon.client());
        cx.spawn(async move |raku, cx| {
            let headline = cx
                .background_executor()
                .spawn(async move {
                    match workspace_client.request(
                        raku_client::WorkspaceOperation::GenerateReasoningHeadline {
                            reasoning,
                            invocation,
                        },
                    ) {
                        Ok(raku_client::WorkspaceResult::ReasoningHeadline { headline }) => {
                            Some(headline)
                        }
                        Ok(_) => None,
                        Err(error) => {
                            eprintln!("reasoning headline generation failed: {error:#}");
                            None
                        }
                    }
                })
                .await;
            let _ = raku.update(cx, |raku, cx| {
                raku.reasoning_headlines.in_flight =
                    raku.reasoning_headlines.in_flight.saturating_sub(1);
                if let Some(headline) = headline
                    && let Some(reasoning) = raku
                        .state
                        .session_mut(session_id)
                        .and_then(|session| {
                            session
                                .transcript_blocks
                                .iter_mut()
                                .rev()
                                .flat_map(|block| block.activities.iter_mut())
                                .find(|activity| activity.id == activity_id)
                        })
                        .and_then(|activity| activity.reasoning.as_mut())
                {
                    reasoning.headline = Some(headline);
                    raku.stream_state_dirty = true;
                    cx.notify();
                }
                // A request slot opened; anything that queued behind it can go.
                raku.queue_reasoning_headlines(session_id, cx);
            });
        })
        .detach();
    }

    // ── Settings ──────────────────────────────────────────────────────────

    pub(super) fn render_text_generation_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let provider = self.state.text_generation_provider;
        let resolved = self.text_generation_target();
        let providers = ProviderKind::ALL
            .into_iter()
            .filter(|provider| !self.state.disabled_providers.contains(provider))
            .filter(|provider| {
                self.provider_probe(*provider)
                    .is_some_and(|probe| probe.path.is_some())
            })
            .collect::<Vec<_>>();

        let weak = cx.entity().downgrade();
        let provider_handle = self.menu_handle("text-generation-provider-selector", cx);
        let provider_selector = dropdown_menu(
            MenuChip::new("text-generation-provider-selector")
                .label(provider.map_or_else(
                    || tr!("text_generation.off"),
                    |provider| provider.display_name().to_owned(),
                ))
                .outlined()
                .selected(provider_handle.is_open())
                .w(px(180.0))
                .justify_between(),
            "text-generation-provider-selector-menu",
            &provider_handle,
            MenuAlign::BelowRight,
            move |_| {
                let off = {
                    let weak = weak.clone();
                    MenuItem::new(tr!("text_generation.off"), move |_, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.set_text_generation_target(None, None, cx);
                        });
                    })
                    .selected(provider.is_none())
                };
                std::iter::once(off)
                    .chain(providers.iter().map(|option| {
                        let option = *option;
                        let weak = weak.clone();
                        MenuItem::new(option.display_name(), move |_, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.set_text_generation_target(Some(option), None, cx);
                            });
                        })
                        .selected(provider == Some(option))
                    }))
                    .collect()
            },
        );

        let model_selector = provider.map(|provider| {
            let selected_model = resolved
                .as_ref()
                .filter(|(resolved, _)| *resolved == provider)
                .and_then(|(_, model)| model.clone());
            let models = self
                .provider_probe(provider)
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
            let weak = cx.entity().downgrade();
            let handle = self.menu_handle("text-generation-model-selector", cx);
            let label = self.model_display_name(provider, selected_model.as_deref());
            scrollable_dropdown_menu(
                MenuChip::new("text-generation-model-selector")
                    .label(label)
                    .outlined()
                    .disabled(models.is_empty())
                    .selected(handle.is_open())
                    .w(px(220.0))
                    .justify_between(),
                "text-generation-model-selector-menu",
                &handle,
                MenuAlign::BelowRight,
                move |_| {
                    models
                        .iter()
                        .map(|(id, label)| {
                            let weak = weak.clone();
                            let model = id.clone();
                            MenuItem::new(label.clone(), move |_, cx| {
                                let model = model.clone();
                                let _ = weak.update(cx, |this, cx| {
                                    this.set_text_generation_target(
                                        Some(provider),
                                        Some(model),
                                        cx,
                                    );
                                });
                            })
                            .selected(selected_model.as_deref() == Some(id.as_str()))
                        })
                        .collect()
                },
            )
            .into_any_element()
        });

        let effort_selector = resolved.as_ref().and_then(|(provider, model)| {
            let model = self.provider_probe(*provider)?.model(model.as_deref()?)?;
            let selected_effort = resolve_side_chat_reasoning_effort(
                model,
                self.state.text_generation_reasoning_effort.as_deref(),
            )?;
            let label = model
                .reasoning_efforts
                .iter()
                .find(|option| option.id == selected_effort)?
                .label
                .clone();
            let selected_effort = selected_effort.to_owned();
            let efforts = model.reasoning_efforts.clone();
            let weak = cx.entity().downgrade();
            let handle = self.menu_handle("text-generation-effort-selector", cx);
            Some(
                dropdown_menu(
                    MenuChip::new("text-generation-effort-selector")
                        .label(label)
                        .outlined()
                        .selected(handle.is_open())
                        .w(px(180.0))
                        .justify_between(),
                    "text-generation-effort-selector-menu",
                    &handle,
                    MenuAlign::BelowRight,
                    move |_| {
                        efforts
                            .iter()
                            .map(|option| {
                                let weak = weak.clone();
                                let effort = option.id.clone();
                                MenuItem::new(option.label.clone(), move |_, cx| {
                                    let _ = weak.update(cx, |this, cx| {
                                        this.set_text_generation_reasoning_effort(
                                            effort.clone(),
                                            cx,
                                        );
                                    });
                                })
                                .selected(option.id == selected_effort)
                            })
                            .collect()
                    },
                )
                .into_any_element(),
            )
        });

        let row = |title: String, control: AnyElement| {
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
                        .text_size(sp(13.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(title),
                )
                .child(control)
        };
        let divider = || div().mx(px(20.0)).h(px(1.0)).bg(theme.border);

        div()
            .mt(px(15.0))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .rounded(px(13.0))
                    .overflow_hidden()
                    .bg(theme.raised)
                    .child(row(
                        tr!("side_chat.provider"),
                        provider_selector.into_any_element(),
                    ))
                    .when_some(model_selector, |card, selector| {
                        card.child(divider())
                            .child(row(tr!("side_chat.model"), selector))
                    })
                    .when_some(effort_selector, |card, selector| {
                        card.child(divider())
                            .child(row(tr!("models.reasoning"), selector))
                    }),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .px(px(4.0))
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .line_height(sp(18.0))
                    .text_color(theme.text_secondary)
                    .whitespace_normal()
                    .child(tr!("text_generation.description")),
            )
            .into_any_element()
    }

    fn set_text_generation_target(
        &mut self,
        provider: Option<ProviderKind>,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.state.text_generation_provider != provider
            || self.state.text_generation_model != model
        {
            self.state.text_generation_reasoning_effort = None;
        }
        self.state.text_generation_provider = provider;
        self.state.text_generation_model = model;
        self.save();
        cx.notify();
    }

    fn set_text_generation_reasoning_effort(&mut self, effort: String, cx: &mut Context<Self>) {
        self.state.text_generation_reasoning_effort = Some(effort);
        self.save();
        cx.notify();
    }
}
