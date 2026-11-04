//! Provider fallback choices used before daemon-side discovery completes.

use crate::model::{ProviderKind, ProviderModel, ProviderModelOption};

pub fn fallback_models(provider: ProviderKind) -> Vec<ProviderModel> {
    match provider {
        // Mirrors Codex 0.166's `model/list`: `ultra` only where the model
        // advertises it, and the fast tier under its canonical `priority` id.
        ProviderKind::Codex => [
            ("gpt-6.1-sol", "GPT-6.1-Sol", true, "low"),
            ("gpt-6-astra", "GPT-6-Astra", true, "low"),
            ("gpt-6.1-terra", "GPT-6.1-Terra", true, "low"),
            ("gpt-6-sol", "GPT-6-Sol", true, "medium"),
            ("gpt-6-luna", "GPT-6-Luna", false, "medium"),
            ("gpt-5.6-sol", "GPT-5.6-Sol", true, "low"),
            ("gpt-5.6-terra", "GPT-5.6-Terra", true, "medium"),
            ("gpt-5.6-luna", "GPT-5.6-Luna", false, "medium"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (id, name, ultra, default_effort))| {
            let efforts = if ultra {
                reasoning_options(["low", "medium", "high", "xhigh", "max", "ultra"])
            } else {
                reasoning_options(["low", "medium", "high", "xhigh", "max"])
            };
            let model = ProviderModel::new(id, name)
                .reasoning(efforts, default_effort)
                .service_tiers(
                    [
                        ProviderModelOption::new("priority", tr!("model_option.fast"))
                            .description(tr!("model_option.fast_description")),
                    ],
                    "default",
                );
            if index == 0 { model.default() } else { model }
        })
        .collect(),
        ProviderKind::Claude => vec![
            claude_long_context(claude_ultracode_model(
                "claude-fable-5-1",
                "Claude Fable 5.1",
            )),
            claude_long_context(claude_ultracode_model("claude-opus-5-5", "Claude Opus 5.5")),
            claude_long_context(claude_ultracode_model(
                "claude-sonnet-5-5",
                "Claude Sonnet 5.5",
            ))
            .default(),
            claude_long_context(claude_ultracode_model(
                "claude-haiku-5-5",
                "Claude Haiku 5.5",
            )),
            claude_long_context(claude_ultracode_model("claude-fable-5", "Claude Fable 5")),
            claude_long_context(claude_ultracode_model("claude-opus-5", "Claude Opus 5")),
            claude_long_context(claude_ultracode_model("claude-opus-4-8", "Claude Opus 4.8")),
            claude_long_context(claude_ultracode_model("claude-opus-4-7", "Claude Opus 4.7")),
            claude_long_context(claude_reasoning_model("claude-opus-4-6", "Claude Opus 4.6")),
            claude_reasoning_model("claude-opus-4-5", "Claude Opus 4.5"),
            claude_long_context(claude_ultracode_model("claude-sonnet-5", "Claude Sonnet 5")),
            claude_long_context(claude_reasoning_model(
                "claude-sonnet-4-6",
                "Claude Sonnet 4.6",
            )),
            ProviderModel::new("claude-haiku-4-5", "Claude Haiku 4.5"),
        ],
        ProviderKind::Pi => Vec::new(),
    }
}

fn reasoning_effort_label(effort: &str) -> String {
    match effort {
        "none" => tr!("model_option.none"),
        "minimal" => tr!("model_option.minimal"),
        "low" => tr!("model_option.low"),
        "medium" => tr!("model_option.medium"),
        "high" => tr!("model_option.high"),
        "xhigh" => tr!("model_option.extra_high"),
        "max" => tr!("model_option.max"),
        "ultra" => tr!("model_option.ultra"),
        "ultracode" => tr!("model_option.ultracode"),
        other => other.replace(['-', '_'], " "),
    }
}

fn reasoning_options<const N: usize>(efforts: [&str; N]) -> Vec<ProviderModelOption> {
    efforts
        .into_iter()
        .map(|effort| ProviderModelOption::new(effort, reasoning_effort_label(effort)))
        .collect()
}

fn claude_reasoning_model(id: &str, name: &str) -> ProviderModel {
    ProviderModel::new(id, name).reasoning(
        reasoning_options(["low", "medium", "high", "xhigh", "max"]),
        "high",
    )
}

/// Mirrors the daemon catalog: `ultracode` resolves to xhigh plus standing
/// dynamic-workflow orchestration, so it is only offered on xhigh-capable
/// models.
fn claude_ultracode_model(id: &str, name: &str) -> ProviderModel {
    ProviderModel::new(id, name).reasoning(
        reasoning_options(["low", "medium", "high", "xhigh", "max", "ultracode"]),
        "high",
    )
}

/// Mirrors the daemon catalog: the 1M window is opt-in behind a `[1m]` model-id
/// suffix the CLI refuses on its older models.
fn claude_long_context(model: ProviderModel) -> ProviderModel {
    model.context_windows(
        [
            ProviderModelOption::new("200k", tr!("model_option.context_200k")),
            ProviderModelOption::new("1m", tr!("model_option.context_1m")),
        ],
        "200k",
    )
}
