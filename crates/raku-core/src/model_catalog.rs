//! Provider model and agent-preset discovery.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::model::{ProviderAgentPreset, ProviderKind, ProviderModel, ProviderModelOption};

const CODEX_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// The probe answers only after the agent has finished loading its extensions,
/// resources and model catalog, which can outlast a live request's budget.
const PI_RPC_TIMEOUT: Duration = Duration::from_secs(30);
const AGY_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

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
        // Pi takes its catalog from the user's configured LLM providers.
        // A fabricated fallback would make unavailable models look selectable.
        ProviderKind::Pi | ProviderKind::Antigravity => Vec::new(),
    }
}

/// Discovers ordinary models and provider-owned agent compositions.
pub fn discover_catalog(
    provider: ProviderKind,
    binary: &Path,
) -> (Vec<ProviderModel>, Vec<ProviderAgentPreset>) {
    let (discovered, discovered_presets, enabled_models) = match provider {
        ProviderKind::Codex => (discover_codex_models(binary), None, HashMap::new()),
        ProviderKind::Claude => (discover_claude_models(binary), None, HashMap::new()),
        ProviderKind::Pi => std::thread::scope(|scope| {
            // The gateway round trips overlap the RPC probe instead of
            // extending discovery.
            let enabled = scope.spawn(opencode_enabled_models);
            let models = discover_pi_models(binary);
            (models, None, enabled.join().unwrap_or_default())
        }),
        ProviderKind::Antigravity => (discover_agy_models(binary), None, HashMap::new()),
    };
    let refresh_cache = !discovered.is_empty() || !enabled_models.is_empty();
    let models = resolve_catalog_models(discovered, &enabled_models, || {
        cached_models(provider).unwrap_or_else(|| fallback_models(provider))
    });
    if refresh_cache {
        write_cached_models(provider, &models);
    }
    let presets = discovered_presets.unwrap_or_default();
    (models, presets)
}

fn resolve_catalog_models(
    discovered: Vec<ProviderModel>,
    enabled_models: &HashMap<String, HashSet<String>>,
    fallback: impl FnOnce() -> Vec<ProviderModel>,
) -> Vec<ProviderModel> {
    // Only a failed or empty CLI probe falls back. An authoritative empty
    // gateway list must stay empty, and applies to cached catalogs too.
    let models = if discovered.is_empty() {
        fallback()
    } else {
        deduplicate(discovered)
    };
    models
        .into_iter()
        .filter(|model| {
            model.id.split_once('/').is_none_or(|(provider, id)| {
                enabled_models
                    .get(provider)
                    .is_none_or(|enabled| enabled.contains(id))
            })
        })
        .collect()
}

/// Where a provider's last discovered catalog is cached. Debug builds keep it
/// in the checkout's gitignored `temp/` beside the debug database, so
/// development never touches the installed app's cache.
fn model_cache_path(provider: ProviderKind) -> PathBuf {
    let directory = if cfg!(debug_assertions) {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("temp")
            .join("model-cache")
    } else {
        dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(crate::identity::DATA_DIRECTORY_NAME)
            .join("models")
    };
    directory.join(format!("{}.json", provider.id()))
}

/// The catalog cached by the last successful discovery, or `None` when no run
/// has cached one or the file no longer parses. Reads the filesystem, so call
/// it from the discovery thread, never from render.
pub fn cached_models(provider: ProviderKind) -> Option<Vec<ProviderModel>> {
    read_models_file(&model_cache_path(provider))
}

fn read_models_file(path: &Path) -> Option<Vec<ProviderModel>> {
    let contents = std::fs::read(path).ok()?;
    let models = serde_json::from_slice::<Vec<ProviderModel>>(&contents).ok()?;
    (!models.is_empty()).then_some(models)
}

/// Best-effort: a cache that fails to write only costs the next launch its
/// head start.
fn write_cached_models(provider: ProviderKind, models: &[ProviderModel]) {
    let _ = write_models_file(&model_cache_path(provider), models);
}

fn write_models_file(path: &Path, models: &[ProviderModel]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write-then-rename so a crash mid-write can't leave a torn file for the
    // next launch to trip over.
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(models)?)?;
    std::fs::rename(temporary, path)
}

/// Claude Code's sessionless SDK initialization response carries the exact
/// catalog behind `/model`. Unlike a fixed Anthropic list, it reflects account
/// availability and role mappings supplied by tools such as CC Switch.
fn discover_claude_models(binary: &Path) -> Vec<ProviderModel> {
    crate::claude_metadata::initialize(binary, None, "user")
        .map(|value| parse_claude_models(&value))
        .unwrap_or_default()
}

fn parse_claude_models(value: &Value) -> Vec<ProviderModel> {
    value
        .pointer("/response/response/models")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry
                .get("value")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())?;
            let resolved = entry
                .get("resolvedModel")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty());
            let description = entry.get("description").and_then(Value::as_str);
            let name = entry
                .get("displayName")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|label| claude_release_label(label, description))
                .or_else(|| resolved.map(display_name_from_slug))
                .unwrap_or_else(|| display_name_from_slug(id));

            let mut model = ProviderModel::new(id, name);
            model.is_default =
                id == "default" || entry.get("isDefault").and_then(Value::as_bool) == Some(true);
            if entry.get("supportsEffort").and_then(Value::as_bool) != Some(false) {
                model.reasoning_efforts = entry
                    .get("supportedEffortLevels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|effort| !effort.is_empty())
                    .map(|effort| ProviderModelOption::new(effort, reasoning_effort_label(effort)))
                    .collect();
                // `ultracode` is Raku's orchestration effort. Claude accepts
                // it wherever the provider metadata says xhigh is supported.
                if model
                    .reasoning_efforts
                    .iter()
                    .any(|option| option.id == "xhigh")
                {
                    model.reasoning_efforts.push(ProviderModelOption::new(
                        "ultracode",
                        reasoning_effort_label("ultracode"),
                    ));
                }
                model.default_reasoning_effort = ["high", "medium"]
                    .into_iter()
                    .find(|preferred| {
                        model
                            .reasoning_efforts
                            .iter()
                            .any(|option| option.id == *preferred)
                    })
                    .or_else(|| {
                        model
                            .reasoning_efforts
                            .first()
                            .map(|option| option.id.as_str())
                    })
                    .map(str::to_owned);
            }
            Some(model)
        })
        .collect()
}

/// Claude Code labels the rows that follow a family's newest release with the
/// bare family — `Opus (1M context)`, `Sonnet` — and names the release only in
/// the description `/model` prints beside it: `Opus 5.5 with 1M context · …`.
/// Its pinned rows already carry the release in the label (`Opus 4.6 (1M
/// context)`), so the described release is folded in the same way. The default
/// row, custom role mappings, and already-versioned labels keep Claude's text.
fn claude_release_label(label: &str, description: Option<&str>) -> String {
    let (family, qualifier) = label.split_once(' ').unwrap_or((label, ""));
    let release = description
        .filter(|_| qualifier.is_empty() || qualifier.starts_with('('))
        .and_then(|description| {
            let mut words = description.split_whitespace();
            let described_family = words.next()?;
            let release = words.next()?;
            let is_release = release
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
            (described_family.eq_ignore_ascii_case(family) && is_release).then_some(release)
        });
    match release {
        Some(release) if qualifier.is_empty() => format!("{family} {release}"),
        Some(release) => format!("{family} {release} {qualifier}"),
        None => label.to_owned(),
    }
}
/// Each gateway reports the models enabled for its own API key's workspace.
const PI_OPENCODE_PROVIDERS: [(&str, &str); 2] = [
    ("opencode", "https://opencode.ai/zen/v1/models"),
    (
        crate::usage::PI_OPENCODE_GO_PROVIDER,
        "https://opencode.ai/zen/go/v1/models",
    ),
];

/// Pi lists every model it ships for Zen and Go, even models the workspace
/// hasn't enabled. Use the gateways' enabled catalogs rather than inferring
/// availability from the Console's sparse disabled-model overrides. A failed
/// request leaves that provider unfiltered; a successful empty list hides all
/// of its models. Blocking file and network I/O.
fn opencode_enabled_models() -> HashMap<String, HashSet<String>> {
    let mut enabled = HashMap::new();
    for (provider, url) in PI_OPENCODE_PROVIDERS {
        let Some(api_key) = crate::usage::pi_opencode_api_key(provider) else {
            continue;
        };
        if let Some(models) = fetch_opencode_enabled_model_ids(url, &api_key) {
            enabled.insert(provider.to_owned(), models);
        }
    }
    enabled
}

fn fetch_opencode_enabled_model_ids(url: &str, api_key: &str) -> Option<HashSet<String>> {
    let (status, body) = crate::usage::http_get(
        url,
        &[
            format!("Authorization: Bearer {api_key}"),
            "Accept: application/json".to_owned(),
            "User-Agent: raku".to_owned(),
        ],
    )
    .ok()?;
    if status != 200 {
        return None;
    }
    parse_opencode_enabled_model_ids(&serde_json::from_str(&body).ok()?)
}

/// Live shape verified on 2026-11-13 for both Zen and Go:
/// `{"object":"list","data":[{"id":"gemini-4-argon",...}]}`.
fn parse_opencode_enabled_model_ids(response: &Value) -> Option<HashSet<String>> {
    response
        .get("data")?
        .as_array()?
        .iter()
        .map(|model| {
            let id = model.get("id")?.as_str()?.trim();
            (!id.is_empty()).then(|| id.to_owned())
        })
        .collect()
}

const PI_PROBE_ARGS: &[&str] = &[
    "--mode",
    "rpc",
    "--no-session",
    "--no-skills",
    "--no-prompt-templates",
    "--no-context-files",
];

fn discover_pi_models(binary: &Path) -> Vec<ProviderModel> {
    let mut command = crate::command_env::command(binary);
    command.env("PI_SKIP_VERSION_CHECK", "1");
    let command = command
        .args(PI_PROBE_ARGS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = crate::command_env::spawn(command) else {
        return Vec::new();
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        return Vec::new();
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return Vec::new();
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                let _ = tx.send(value);
            }
        }
    });

    let models_request = json!({"id": "raku-models", "type": "get_available_models"});
    let result = if write_json_line(&mut stdin, &models_request).is_ok()
        && let Some(models) = recv_pi_rpc_response(&rx, "raku-models", PI_RPC_TIMEOUT)
    {
        // The catalog is useful even when an older Pi cannot report its
        // current state. State only marks the default model and thinking
        // level, so ask for it after the required model response.
        let state_request = json!({"id": "raku-state", "type": "get_state"});
        let state = if write_json_line(&mut stdin, &state_request).is_ok() {
            recv_pi_rpc_response(&rx, "raku-state", PI_RPC_TIMEOUT).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        parse_pi_model_response(&state, &models)
    } else {
        Vec::new()
    };
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn parse_pi_model_response(state: &Value, response: &Value) -> Vec<ProviderModel> {
    let default_provider = state
        .pointer("/data/model/provider")
        .and_then(Value::as_str);
    let default_model = state.pointer("/data/model/id").and_then(Value::as_str);
    let default_slug = default_provider
        .zip(default_model)
        .map(|(provider, model)| format!("{provider}/{model}"));
    let current_thinking = state.pointer("/data/thinkingLevel").and_then(Value::as_str);

    response
        .pointer("/data/models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let provider = value.get("provider").and_then(Value::as_str)?;
            let model_id = value.get("id").and_then(Value::as_str)?;
            if provider.trim().is_empty() || model_id.trim().is_empty() {
                return None;
            }
            let slug = format!("{provider}/{model_id}");
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| display_name_from_slug(model_id));
            let mut model = ProviderModel::new(slug.clone(), name)
                .sub_provider(display_name_from_slug(provider));
            model.is_default = default_slug.as_deref() == Some(slug.as_str());
            model.reasoning_efforts = pi_reasoning_options(value);
            if !model.reasoning_efforts.is_empty() {
                let preferred = model
                    .is_default
                    .then_some(current_thinking)
                    .flatten()
                    .filter(|effort| {
                        model
                            .reasoning_efforts
                            .iter()
                            .any(|option| option.id == **effort)
                    })
                    .or_else(|| {
                        model
                            .reasoning_efforts
                            .iter()
                            .any(|option| option.id == "medium")
                            .then_some("medium")
                    })
                    .or_else(|| {
                        model
                            .reasoning_efforts
                            .first()
                            .map(|option| option.id.as_str())
                    });
                model.default_reasoning_effort = preferred.map(str::to_owned);
            }
            Some(model)
        })
        .collect()
}

const PI_THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

fn pi_reasoning_options(model: &Value) -> Vec<ProviderModelOption> {
    if model.get("reasoning").and_then(Value::as_bool) != Some(true) {
        return Vec::new();
    }
    let level_map = model.get("thinkingLevelMap").and_then(Value::as_object);
    PI_THINKING_LEVELS
        .into_iter()
        .filter(|level| {
            let mapped = level_map.and_then(|map| map.get(*level));
            if matches!(*level, "xhigh" | "max") {
                mapped.is_some_and(|value| !value.is_null())
            } else {
                mapped.is_none_or(|value| !value.is_null())
            }
        })
        .map(|level| ProviderModelOption::new(level, reasoning_effort_label(level)))
        .collect()
}

fn discover_codex_models(binary: &Path) -> Vec<ProviderModel> {
    let mut command = crate::command_env::command(binary);
    let command = command
        .args(["app-server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = crate::command_env::spawn(command) else {
        return Vec::new();
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        return Vec::new();
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return Vec::new();
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                let _ = tx.send(value);
            }
        }
    });

    let initialize = json!({
        "method": "initialize",
        "id": 0,
        "params": {
            "clientInfo": {
                "name": "raku",
                "title": "Raku",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {
                "experimentalApi": true
            }
        }
    });
    if write_json_line(&mut stdin, &initialize).is_err()
        || recv_rpc_response(&rx, 0, CODEX_RPC_TIMEOUT).is_none()
        || write_json_line(&mut stdin, &json!({"method": "initialized", "params": {}})).is_err()
    {
        let _ = child.kill();
        return Vec::new();
    }

    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    for request_id in 1..=32_u64 {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
        if write_json_line(
            &mut stdin,
            &json!({"method": "model/list", "id": request_id, "params": params}),
        )
        .is_err()
        {
            break;
        }
        let Some(response) = recv_rpc_response(&rx, request_id, CODEX_RPC_TIMEOUT) else {
            break;
        };
        models.extend(parse_codex_model_response(&response));
        cursor = response
            .pointer("/result/nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    models
}

fn parse_codex_model_response(response: &Value) -> Vec<ProviderModel> {
    response
        .pointer("/result/data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let id = value.get("model").and_then(Value::as_str)?;
            let name = value
                .get("displayName")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| display_name_from_slug(id));
            let mut model = ProviderModel::new(id, normalize_codex_name(&name));
            model.is_default = value
                .get("isDefault")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            model.reasoning_efforts = value
                .get("supportedReasoningEfforts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    let id = option.get("reasoningEffort").and_then(Value::as_str)?;
                    let description = option
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    Some(
                        ProviderModelOption::new(id, reasoning_effort_label(id))
                            .description(description),
                    )
                })
                .collect();
            model.default_reasoning_effort = value
                .get("defaultReasoningEffort")
                .and_then(Value::as_str)
                .map(str::to_owned);
            model.service_tiers = value
                .get("serviceTiers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|tier| {
                    let id = tier.get("id").and_then(Value::as_str)?;
                    let name = tier
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or(id);
                    let description = tier
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    Some(ProviderModelOption::new(id, name).description(description))
                })
                .collect();
            if model.service_tiers.is_empty() {
                model.service_tiers = value
                    .get("additionalSpeedTiers")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|id| {
                        ProviderModelOption::new(
                            id,
                            if id == "fast" {
                                "Fast".to_owned()
                            } else {
                                display_name_from_slug(id)
                            },
                        )
                    })
                    .collect();
            }
            model.default_service_tier = value
                .get("defaultServiceTier")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| (!model.service_tiers.is_empty()).then(|| "default".to_owned()));
            Some(model)
        })
        .collect()
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
        other => display_name_from_slug(other),
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

/// `ultracode` is an effort value Claude Code accepts alongside the ordinary
/// ladder: it resolves to xhigh and turns on standing dynamic-workflow
/// orchestration for that session. It therefore needs an xhigh-capable model —
/// older ones clamp xhigh back to high, which would leave the entry inert.
fn claude_ultracode_model(id: &str, name: &str) -> ProviderModel {
    ProviderModel::new(id, name).reasoning(
        reasoning_options(["low", "medium", "high", "xhigh", "max", "ultracode"]),
        "high",
    )
}

/// Claude Code serves a 200K window by default and reaches the 1M one through
/// a `[1m]` model-id suffix, so the window is a per-session trait rather than a
/// separate model. The CLI refuses the suffix on Claude 3, Opus 4.0/4.1/4.5,
/// and Haiku 4.5, so only the models that honor it carry the choice.
fn claude_long_context(model: ProviderModel) -> ProviderModel {
    model.context_windows(
        [
            ProviderModelOption::new("200k", tr!("model_option.context_200k")),
            ProviderModelOption::new("1m", tr!("model_option.context_1m")),
        ],
        "200k",
    )
}

fn recv_rpc_response(rx: &Receiver<Value>, id: u64, timeout: Duration) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let value = rx.recv_timeout(remaining).ok()?;
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            return Some(value);
        }
    }
}

fn recv_pi_rpc_response(rx: &Receiver<Value>, id: &str, timeout: Duration) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let value = rx.recv_timeout(remaining).ok()?;
        if value.get("id").and_then(Value::as_str) == Some(id) {
            return (value.get("success").and_then(Value::as_bool) == Some(true)).then_some(value);
        }
    }
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn normalize_codex_name(name: &str) -> String {
    let name = if name
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("gpt"))
    {
        format!("GPT{}", &name[3..])
    } else {
        name.to_owned()
    };
    let mut capitalize_next = false;
    name.chars()
        .flat_map(|char| {
            if capitalize_next {
                capitalize_next = false;
                char.to_uppercase().collect::<Vec<_>>()
            } else {
                capitalize_next = char == '-';
                vec![char]
            }
        })
        .collect()
}

pub(crate) fn display_name_from_slug(slug: &str) -> String {
    let words = slug
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| match part.to_ascii_lowercase().as_str() {
            "gpt" => "GPT".to_owned(),
            "ai" => "AI".to_owned(),
            "xai" => "xAI".to_owned(),
            _ if part
                .chars()
                .all(|char| char.is_ascii_digit() || char == '.') =>
            {
                part.to_owned()
            }
            _ => {
                let mut chars = part.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().collect::<String>() + chars.as_str()
                })
            }
        })
        .collect::<Vec<_>>();
    if words.first().is_some_and(|word| word == "GPT") {
        words.join("-")
    } else {
        words.join(" ")
    }
}

fn deduplicate(models: Vec<ProviderModel>) -> Vec<ProviderModel> {
    let mut seen = std::collections::HashSet::new();
    models
        .into_iter()
        .filter(|model| seen.insert(model.id.clone()))
        .collect()
}

const AGY_EFFORT_SUFFIXES: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

fn discover_agy_models(binary: &Path) -> Vec<ProviderModel> {
    let cwd = crate::session_catalog::catalog_working_directory()
        .unwrap_or_else(|_| std::env::temp_dir());
    let mut command = crate::command_env::command(binary);
    command
        .arg("models")
        .current_dir(&cwd)
        .env("NO_COLOR", "1")
        .env("CI", "1");
    if let Ok(output) = crate::git_commit::run_capture(&mut command, AGY_DISCOVERY_TIMEOUT)
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed = parse_agy_models_output(&stdout);
        if !parsed.is_empty() {
            return parsed;
        }
    }
    discover_agy_models_via_acp(binary, &cwd)
}

fn parse_agy_models_output(stdout: &str) -> Vec<ProviderModel> {
    let mut rows = Vec::<(String, String)>::new();
    for raw_line in stdout.lines() {
        let clean = crate::model::strip_ansi(raw_line);
        let line = clean.trim();
        if line.is_empty() {
            continue;
        }
        let Some((id, name)) = line
            .split_once('\t')
            .or_else(|| line.split_once(char::is_whitespace))
        else {
            continue;
        };
        let id = id.trim();
        let name = name.trim();
        if id.is_empty() || name.is_empty() {
            continue;
        }
        rows.push((id.to_owned(), name.to_owned()));
    }
    group_agy_model_rows(&rows, None, None, None)
}

fn split_agy_effort_suffix(id: &str) -> Option<(&str, &str)> {
    for suffix in AGY_EFFORT_SUFFIXES {
        if let Some(stem) = id.strip_suffix(&format!("-{suffix}"))
            && !stem.is_empty()
        {
            return Some((stem, suffix));
        }
    }
    None
}

fn strip_trailing_effort_label(name: &str, effort: &str) -> String {
    let suffix = format!(" ({effort})");
    if name.len() > suffix.len() {
        let tail_start = name.len() - suffix.len();
        if name.is_char_boundary(tail_start) && name[tail_start..].eq_ignore_ascii_case(&suffix) {
            return name[..tail_start].trim_end().to_owned();
        }
    }
    name.to_owned()
}

fn group_agy_model_rows(
    rows: &[(String, String)],
    known_bases: Option<&HashSet<&str>>,
    default_model: Option<&str>,
    default_effort: Option<&str>,
) -> Vec<ProviderModel> {
    let mut stem_counts = HashMap::<&str, usize>::new();
    for (id, _) in rows {
        if let Some((stem, _)) = split_agy_effort_suffix(id) {
            *stem_counts.entry(stem).or_default() += 1;
        }
    }

    let mut order = Vec::<String>::new();
    let mut grouped = HashMap::<String, (String, Vec<String>)>::new();
    for (id, name) in rows {
        let is_listed_standalone = known_bases.is_some_and(|bases| bases.contains(id.as_str()));
        if !is_listed_standalone
            && let Some((stem, effort)) = split_agy_effort_suffix(id)
            && (known_bases.is_some_and(|bases| bases.contains(stem))
                || stem_counts.get(stem).copied().unwrap_or(0) >= 2)
        {
            let entry = grouped.entry(stem.to_owned()).or_insert_with(|| {
                order.push(stem.to_owned());
                (strip_trailing_effort_label(name, effort), Vec::new())
            });
            if !entry.1.iter().any(|existing| existing == effort) {
                entry.1.push(effort.to_owned());
            }
        } else if !grouped.contains_key(id) {
            order.push(id.clone());
            grouped.insert(id.clone(), (name.clone(), Vec::new()));
        }
    }

    let mut models: Vec<ProviderModel> = order
        .into_iter()
        .filter_map(|id| {
            let (name, mut efforts) = grouped.remove(&id)?;
            efforts.sort_by_key(|effort| {
                AGY_EFFORT_SUFFIXES
                    .iter()
                    .position(|candidate| candidate == effort)
                    .unwrap_or(usize::MAX)
            });
            let mut model = ProviderModel::new(id.clone(), name);
            if !efforts.is_empty() {
                let options: Vec<ProviderModelOption> = efforts
                    .iter()
                    .map(|effort| {
                        ProviderModelOption::new(effort.clone(), reasoning_effort_label(effort))
                    })
                    .collect();
                let preferred = default_effort
                    .filter(|eff| options.iter().any(|opt| opt.id == *eff))
                    .or_else(|| {
                        ["high", "medium"]
                            .into_iter()
                            .find(|pref| options.iter().any(|opt| opt.id == *pref))
                    })
                    .or_else(|| options.first().map(|opt| opt.id.as_str()))
                    .map(str::to_owned);
                model.reasoning_efforts = options;
                model.default_reasoning_effort = preferred;
            }
            if default_model == Some(id.as_str()) {
                model.is_default = true;
            }
            Some(model)
        })
        .collect();

    if !models.is_empty() && !models.iter().any(|m| m.is_default) {
        models[0].is_default = true;
    }
    models
}

fn discover_agy_models_via_acp(binary: &Path, cwd: &Path) -> Vec<ProviderModel> {
    let mut command = crate::command_env::command(binary);
    let command = command
        .arg("acp")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = crate::command_env::spawn(command) else {
        return Vec::new();
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Vec::new();
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Vec::new();
    };
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                let _ = tx.send(value);
            }
        }
    });

    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": { "readTextFile": false, "writeTextFile": false },
                "terminal": false
            },
            "clientInfo": { "name": "raku", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    let mut models = Vec::new();
    if write_json_line(&mut stdin, &init).is_ok()
        && recv_rpc_response(&rx, 1, AGY_DISCOVERY_TIMEOUT).is_some()
    {
        let new_session = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": cwd.display().to_string(), "mcpServers": [] }
        });
        if write_json_line(&mut stdin, &new_session).is_ok()
            && let Some(resp) = recv_rpc_response(&rx, 2, AGY_DISCOVERY_TIMEOUT)
            && let Some(result) = resp.get("result")
        {
            models = parse_agy_acp_models(result);
            if let Some(session_id) = result.get("sessionId").and_then(Value::as_str) {
                let close = json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session/close",
                    "params": { "sessionId": session_id }
                });
                if write_json_line(&mut stdin, &close).is_ok() {
                    let _ = recv_rpc_response(&rx, 3, Duration::from_secs(5));
                }
            }
        }
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    models
}

fn parse_agy_acp_models(result: &Value) -> Vec<ProviderModel> {
    let config_options = result
        .get("configOptions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut default_model = None;
    let mut default_effort = None;
    let mut base_options = Vec::<(String, String)>::new();

    for entry in config_options {
        let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
        let category = entry.get("category").and_then(Value::as_str).unwrap_or(id);
        let current = entry
            .get("currentValue")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty());
        if category == "model" || id == "model" {
            default_model = current;
            if let Some(opts) = entry.get("options").and_then(Value::as_array) {
                for opt in opts {
                    if let (Some(val), Some(name)) = (
                        opt.get("value").and_then(Value::as_str),
                        opt.get("name").and_then(Value::as_str),
                    ) {
                        base_options.push((val.trim().to_owned(), name.trim().to_owned()));
                    }
                }
            }
        } else if category == "thought_level" || id == "thought_level" {
            default_effort = current;
        }
    }

    let available_rows: Vec<(String, String)> = result
        .pointer("/models/availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("modelId")?.as_str()?.trim();
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .trim();
            (!id.is_empty()).then(|| (id.to_owned(), name.to_owned()))
        })
        .collect();

    if available_rows.is_empty() {
        let mut models: Vec<ProviderModel> = base_options
            .into_iter()
            .map(|(id, name)| {
                let mut model = ProviderModel::new(id.clone(), name);
                if default_model == Some(id.as_str()) {
                    model.is_default = true;
                }
                model
            })
            .collect();
        if !models.is_empty() && !models.iter().any(|m| m.is_default) {
            models[0].is_default = true;
        }
        return models;
    }

    let known_bases: HashSet<&str> = base_options.iter().map(|(id, _)| id.as_str()).collect();
    let grouped = group_agy_model_rows(
        &available_rows,
        (!known_bases.is_empty()).then_some(&known_bases),
        default_model,
        default_effort,
    );
    if base_options.is_empty() {
        return grouped;
    }

    let mut by_id: HashMap<String, ProviderModel> =
        grouped.into_iter().map(|m| (m.id.clone(), m)).collect();
    let mut ordered = Vec::new();
    for (base_id, base_name) in base_options {
        let mut model = by_id
            .remove(&base_id)
            .unwrap_or_else(|| ProviderModel::new(base_id.clone(), base_name));
        model.is_default = default_model == Some(base_id.as_str());
        ordered.push(model);
    }
    if !ordered.is_empty() && !ordered.iter().any(|m| m.is_default) {
        ordered[0].is_default = true;
    }
    ordered
}

/// Reconciles `model-cache/agy.json` against the authoritative base model IDs
/// and `models.availableModels` reported by a live ACP session open. Called on
/// the driver's background writer thread, never on the UI thread.
#[cfg(not(test))]
pub(crate) fn reconcile_agy_session_models(session_result: &Value) {
    let existing = cached_models(ProviderKind::Antigravity);
    if let Some(reconciled) = reconcile_agy_catalog(existing.as_deref(), session_result)
        && existing.as_ref() != Some(&reconciled)
    {
        write_cached_models(ProviderKind::Antigravity, &reconciled);
    }
}

fn reconcile_agy_catalog(
    existing: Option<&[ProviderModel]>,
    session_result: &Value,
) -> Option<Vec<ProviderModel>> {
    let has_available = session_result
        .pointer("/models/availableModels")
        .and_then(Value::as_array)
        .is_some_and(|rows| !rows.is_empty());
    if has_available {
        let parsed = parse_agy_acp_models(session_result);
        return (!parsed.is_empty()).then_some(parsed);
    }

    let existing = existing?;
    let base_options: Vec<(String, String)> = session_result
        .get("configOptions")
        .and_then(Value::as_array)?
        .iter()
        .find(|entry| {
            entry.get("category").and_then(Value::as_str) == Some("model")
                || entry.get("id").and_then(Value::as_str) == Some("model")
        })?
        .get("options")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|opt| {
            Some((
                opt.get("value")?.as_str()?.trim().to_owned(),
                opt.get("name")?.as_str()?.trim().to_owned(),
            ))
        })
        .filter(|(id, _)| !id.is_empty())
        .collect();
    if base_options.is_empty() {
        return None;
    }
    let known_bases: HashSet<&str> = base_options.iter().map(|(id, _)| id.as_str()).collect();
    let base_names: HashMap<&str, &str> = base_options
        .iter()
        .map(|(id, name)| (id.as_str(), name.as_str()))
        .collect();

    let mut changed = false;
    let mut reconciled = Vec::with_capacity(existing.len());
    for model in existing {
        if model.reasoning_efforts.is_empty()
            && !known_bases.contains(model.id.as_str())
            && let Some((stem, effort)) = split_agy_effort_suffix(&model.id)
            && known_bases.contains(stem)
        {
            changed = true;
            let name = base_names
                .get(stem)
                .map(|n| (*n).to_owned())
                .unwrap_or_else(|| strip_trailing_effort_label(&model.name, effort));
            let mut collapsed = ProviderModel::new(stem, name).reasoning(
                [ProviderModelOption::new(
                    effort,
                    reasoning_effort_label(effort),
                )],
                effort,
            );
            collapsed.is_default = model.is_default;
            reconciled.push(collapsed);
        } else {
            reconciled.push(model.clone());
        }
    }
    changed.then_some(reconciled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_fake_model_cli(name: &str, contents: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = std::env::temp_dir().join(format!(
            "raku-{name}-{}-model-discovery.sh",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[test]
    fn model_cache_round_trips_and_rejects_empty_or_invalid_files() {
        let directory =
            std::env::temp_dir().join(format!("raku-model-cache-test-{}", std::process::id()));
        let path = directory.join("codex.json");
        let models = vec![
            ProviderModel::new("gpt-5.6-sol", "GPT-5.6-Sol")
                .default()
                .reasoning(reasoning_options(["low", "high"]), "high"),
        ];

        assert_eq!(read_models_file(&path), None);
        write_models_file(&path, &models).unwrap();
        assert_eq!(read_models_file(&path), Some(models));

        write_models_file(&path, &[]).unwrap();
        assert_eq!(read_models_file(&path), None);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_models_file(&path), None);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn claude_catalog_offers_ultracode_only_on_xhigh_capable_models() {
        let models = fallback_models(ProviderKind::Claude);
        let efforts = |id: &str| {
            models
                .iter()
                .find(|model| model.id == id)
                .map(|model| {
                    model
                        .reasoning_efforts
                        .iter()
                        .map(|option| option.id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };

        assert_eq!(
            efforts("claude-opus-5"),
            ["low", "medium", "high", "xhigh", "max", "ultracode"]
        );
        assert_eq!(
            efforts("claude-sonnet-4-6"),
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert!(efforts("claude-haiku-4-5").is_empty());
    }

    #[test]
    fn parses_claude_initialization_models_without_repeating_resolved_ids() {
        let models = parse_claude_models(&json!({
            "type": "control_response",
            "response": {"response": {"models": [
                {
                    "value": "default",
                    "resolvedModel": "deepseek-v4-pro",
                    "displayName": "DeepSeek V4 Pro",
                    "supportsEffort": true,
                    "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]
                },
                {
                    "value": "sonnet",
                    "resolvedModel": "claude-sonnet-5",
                    "displayName": "Sonnet",
                    "supportsEffort": false
                },
                {"value": "  ", "displayName": "ignored"}
            ]}}
        }));

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "default");
        assert_eq!(models[0].name, "DeepSeek V4 Pro");
        assert!(models[0].is_default);
        assert_eq!(
            models[0]
                .reasoning_efforts
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            ["low", "medium", "high", "xhigh", "max", "ultracode"]
        );
        assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(models[1].name, "Sonnet");
        assert!(models[1].reasoning_efforts.is_empty());
    }

    /// Descriptions as Claude Code 2.1.280 reports them, first-party and with
    /// CC Switch-style role mappings.
    #[test]
    fn names_claude_family_aliases_by_their_described_release() {
        let models = parse_claude_models(&json!({"models": [
            {
                "value": "default",
                "resolvedModel": "claude-opus-5-5[1m]",
                "displayName": "Default (recommended)",
                "description": "Use the default model (currently Opus 5.5 (1M context)) · $4/$20 per Mtok"
            },
            {
                "value": "opus[1m]",
                "resolvedModel": "claude-opus-5-5[1m]",
                "displayName": "Opus (1M context)",
                "description": "Opus 5.5 with 1M context · Best for everyday, complex tasks · $4/$20 per Mtok"
            },
            {
                "value": "claude-fable-5-1[1m]",
                "resolvedModel": "claude-fable-5-1",
                "displayName": "Fable",
                "description": "Fable 5.1 · Most capable for your hardest and longest-running tasks"
            },
            {
                "value": "sonnet",
                "resolvedModel": "claude-sonnet-5",
                "displayName": "Sonnet",
                "description": "Sonnet 5 · Efficient for routine tasks · $2/$10 per Mtok"
            },
            {
                "value": "haiku",
                "resolvedModel": "claude-haiku-4-5-20251001",
                "displayName": "Haiku",
                "description": "Haiku 4.5 · Fastest for quick answers · $1/$5 per Mtok"
            },
            {
                "value": "claude-opus-4-6[1m]",
                "displayName": "Opus 4.6 (1M context)",
                "description": "Opus 4.6 for long sessions"
            },
            {
                "value": "opus",
                "resolvedModel": "custom-k2-0905",
                "displayName": "custom-k2-0905",
                "description": "Custom Opus model"
            },
            {"value": "sonnet[1m]", "displayName": "Sonnet"}
        ]}));

        assert_eq!(
            models
                .iter()
                .map(|model| model.name.as_str())
                .collect::<Vec<_>>(),
            [
                "Default (recommended)",
                "Opus 5.5 (1M context)",
                "Fable 5.1",
                "Sonnet 5",
                "Haiku 4.5",
                "Opus 4.6 (1M context)",
                "custom-k2-0905",
                "Sonnet",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn discovers_claude_models_from_sessionless_initialization() {
        let binary = write_fake_model_cli(
            "claude",
            r#"#!/bin/sh
read -r request
printf '%s\n' '{"type":"control_response","response":{"request_id":"raku-initialize-catalog","response":{"models":[{"value":"cc-switch-model","resolvedModel":"cc-switch-model","displayName":"CC Switch Model"}]}}}'
"#,
        );

        let models = discover_claude_models(&binary);

        let _ = std::fs::remove_file(binary);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "cc-switch-model");
        assert_eq!(models[0].name, "CC Switch Model");
    }

    #[test]
    fn parses_codex_model_list_metadata() {
        let response = json!({
            "id": 1,
            "result": {
                "data": [{
                    "model": "gpt-5.6-luna",
                    "displayName": "gpt-5.6-luna",
                    "isDefault": true,
                    "supportedReasoningEfforts": [
                        {"reasoningEffort": "low", "description": "Quick responses"},
                        {"reasoningEffort": "xhigh", "description": "Deep reasoning"}
                    ],
                    "defaultReasoningEffort": "xhigh",
                    "serviceTiers": [{
                        "id": "fast",
                        "name": "Fast",
                        "description": "Priority processing"
                    }],
                    "defaultServiceTier": "default"
                }]
            }
        });
        let models = parse_codex_model_response(&response);
        assert_eq!(models[0].name, "GPT-5.6-Luna");
        assert!(models[0].is_default);
        assert_eq!(models[0].reasoning_efforts[1].label, "Extra High");
        assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("xhigh"));
        assert_eq!(models[0].service_tiers[0].id, "fast");
        assert_eq!(
            models[0].service_tiers[0].description.as_deref(),
            Some("Priority processing")
        );
    }

    #[test]
    fn parses_opencode_enabled_catalogs_and_distinguishes_empty_from_invalid() {
        let enabled = parse_opencode_enabled_model_ids(&json!({
            "object": "list",
            "data": [
                {"id": "gemini-4-argon", "object": "model", "owned_by": "opencode"},
                {"id": "deepseek-v4.1-flash", "object": "model"},
                {"id": "gemini-4-argon", "object": "model"}
            ]
        }));
        assert_eq!(
            enabled,
            Some(HashSet::from([
                "gemini-4-argon".to_owned(),
                "deepseek-v4.1-flash".to_owned(),
            ]))
        );
        assert_eq!(
            parse_opencode_enabled_model_ids(&json!({"data": []})),
            Some(HashSet::new())
        );
        for invalid in [
            json!({"error": {"message": "Unauthorized"}}),
            json!({"data": null}),
            json!({"data": [{"name": "Missing id"}]}),
            json!({"data": [{"id": "  "}]}),
        ] {
            assert!(parse_opencode_enabled_model_ids(&invalid).is_none());
        }
    }

    #[test]
    fn opencode_catalogs_leave_only_enabled_models_for_each_provider() {
        let enabled = HashMap::from([
            ("opencode".to_owned(), HashSet::from(["kimi-k3".to_owned()])),
            (
                "opencode-go".to_owned(),
                HashSet::from(["glm-5.3".to_owned(), "glm-5.2".to_owned()]),
            ),
        ]);
        let models = [
            "opencode-go/kimi-k3",
            "opencode-go/glm-5.3",
            // Zen's switch must not hide Go's model of the same name.
            "opencode-go/glm-5.2",
            "opencode/glm-5.2",
            "opencode/kimi-k3",
            // A model absent from the gateway must not leak from Pi's catalog.
            "opencode/unlisted-model",
            "anthropic/kimi-k3",
        ]
        .map(|id| ProviderModel::new(id, id))
        .to_vec();

        let ids = resolve_catalog_models(models, &enabled, || panic!("live catalog is present"))
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            [
                "opencode-go/glm-5.3",
                "opencode-go/glm-5.2",
                "opencode/kimi-k3",
                "anthropic/kimi-k3"
            ]
        );
    }

    #[test]
    fn opencode_enabled_catalogs_also_filter_cached_fallbacks() {
        let enabled = HashMap::from([
            ("opencode".to_owned(), HashSet::new()),
            (
                "opencode-go".to_owned(),
                HashSet::from(["gemini-4-argon".to_owned()]),
            ),
        ]);
        let models = resolve_catalog_models(Vec::new(), &enabled, || {
            [
                "opencode/gemini-4-carbon",
                "opencode-go/gemini-4-argon",
                "opencode-go/kimi-k3",
                "anthropic/kimi-k3",
            ]
            .map(|id| ProviderModel::new(id, id))
            .to_vec()
        });
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["opencode-go/gemini-4-argon", "anthropic/kimi-k3"]
        );

        let models = resolve_catalog_models(
            vec![ProviderModel::new("opencode/kimi-k3", "Kimi K3")],
            &enabled,
            || panic!("an empty enabled catalog must not revive cached models"),
        );
        assert!(models.is_empty());
    }

    #[test]
    fn unavailable_opencode_catalogs_preserve_pi_models() {
        let models = resolve_catalog_models(Vec::new(), &HashMap::new(), || {
            vec![ProviderModel::new("opencode/kimi-k3", "Kimi K3")]
        });
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "opencode/kimi-k3");
    }

    #[test]
    fn parses_pi_models_and_model_specific_thinking_levels() {
        let state = json!({
            "id": "raku-state",
            "type": "response",
            "success": true,
            "data": {
                "model": {"provider": "github-copilot", "id": "gpt-5.6-terra"},
                "thinkingLevel": "xhigh"
            }
        });
        let response = json!({
            "id": "raku-models",
            "type": "response",
            "success": true,
            "data": {"models": [
                {
                    "provider": "github-copilot",
                    "id": "gpt-5.6-terra",
                    "name": "GPT-5.6 Terra",
                    "reasoning": true,
                    "thinkingLevelMap": {"off": null, "xhigh": "xhigh", "max": "max"}
                },
                {
                    "provider": "openai",
                    "id": "gpt-4o-mini",
                    "name": "GPT-4o mini",
                    "reasoning": false
                }
            ]}
        });

        let models = parse_pi_model_response(&state, &response);

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "github-copilot/gpt-5.6-terra");
        assert_eq!(models[0].sub_provider.as_deref(), Some("Github Copilot"));
        assert!(models[0].is_default);
        assert_eq!(
            models[0]
                .reasoning_efforts
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            ["minimal", "low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("xhigh"));
        assert!(models[1].reasoning_efforts.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn pi_rpc_discovery_keeps_model_provider_extensions_enabled() {
        let binary = write_fake_model_cli(
            "extensions",
            r#"#!/bin/sh
for argument in "$@"; do
  if [ "$argument" = "--no-extensions" ]; then
    exit 1
  fi
done
while IFS= read -r request; do
  case "$request" in
    *raku-models*)
      printf '%s\n' '{"id":"raku-models","type":"response","success":true,"data":{"models":[{"provider":"extension-provider","id":"extension-model","name":"Extension Model","reasoning":false}]}}'
      ;;
    *raku-state*)
      printf '%s\n' '{"id":"raku-state","type":"response","success":true,"data":{"model":{"provider":"extension-provider","id":"extension-model"},"thinkingLevel":"medium"}}'
      ;;
  esac
done
"#,
        );

        let models = discover_pi_models(&binary);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "extension-provider/extension-model");
        assert_eq!(models[0].name, "Extension Model");
        assert!(models[0].is_default);
        let _ = std::fs::remove_file(binary);
    }

    #[test]
    fn parses_piped_agy_models_without_collapsing_standalone_or_tier_suffixes() {
        let sample = "\
gemini-4-neon-high\tGemini 4 Neon (High)
gemini-4-neon-xhigh\tGemini 4 Neon (XHigh)
gemini-4-neon-medium\tGemini 4 Neon (Medium)
gemini-4-neon-low\tGemini 4 Neon (Low)
gemini-4-carbon-medium\tGemini 4 Carbon (Medium)
gemini-4-carbon-high\tGemini 4 Carbon (High)
gemini-4-carbon-low\tGemini 4 Carbon (Low)
gemini-pro-orchestrator-neon\tOrchestrator (Neon)
gemini-pro-orchestrator\tOrchestrator (Carbon)
gemini-pro-orchestrator-argon\tOrchestrator (Argon)
gemini-3.8-flash-high\tGemini 3.8 Flash (High)
gemini-3.8-flash-medium\tGemini 3.8 Flash (Medium)
gemini-3.8-flash-low\tGemini 3.8 Flash (Low)
gpt-oss-120b-medium\tGPT-OSS 120B (Medium)
";
        let models = parse_agy_models_output(sample);
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gemini-4-neon",
                "gemini-4-carbon",
                "gemini-pro-orchestrator-neon",
                "gemini-pro-orchestrator",
                "gemini-pro-orchestrator-argon",
                "gemini-3.8-flash",
                "gpt-oss-120b-medium",
            ]
        );
        assert!(models[0].is_default);
        assert_eq!(models[0].name, "Gemini 4 Neon");
        assert_eq!(
            models[0]
                .reasoning_efforts
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            ["low", "medium", "high", "xhigh"]
        );
        assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("high"));

        let flash = models.iter().find(|m| m.id == "gemini-3.8-flash").unwrap();
        assert_eq!(
            flash
                .reasoning_efforts
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            ["low", "medium", "high"]
        );

        let gpt_oss = models
            .iter()
            .find(|m| m.id == "gpt-oss-120b-medium")
            .unwrap();
        assert!(gpt_oss.reasoning_efforts.is_empty());
        assert_eq!(gpt_oss.name, "GPT-OSS 120B (Medium)");

        let orch = models
            .iter()
            .find(|m| m.id == "gemini-pro-orchestrator-neon")
            .unwrap();
        assert!(orch.reasoning_efforts.is_empty());
    }

    #[test]
    fn parses_agy_models_tty_whitespace_and_ansi_fallback() {
        let sample = "\r\x1b[2K\x1b[32mgemini-3.8-flash-low   Gemini 3.8 Flash (Low)\x1b[0m\n\
                      gemini-3.8-flash-high  Gemini 3.8 Flash (High)\n";
        let models = parse_agy_models_output(sample);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3.8-flash");
        assert_eq!(models[0].name, "Gemini 3.8 Flash");
        assert_eq!(models[0].reasoning_efforts.len(), 2);
    }

    #[test]
    fn parses_agy_acp_session_new_models_fallback() {
        let result = json!({
            "sessionId": "s1",
            "configOptions": [
                {
                    "id": "model",
                    "category": "model",
                    "currentValue": "gemini-3.8-flash",
                    "options": [
                        { "value": "gemini-4-neon", "name": "Gemini 4 Neon" },
                        { "value": "gemini-3.8-flash", "name": "Gemini 3.8 Flash" },
                        { "value": "gpt-oss-120b-medium", "name": "GPT-OSS 120B (Medium)" }
                    ]
                },
                {
                    "id": "thought_level",
                    "category": "thought_level",
                    "currentValue": "medium",
                    "options": [
                        { "value": "low", "name": "Low" },
                        { "value": "medium", "name": "Medium" },
                        { "value": "high", "name": "High" },
                        { "value": "xhigh", "name": "Extra" }
                    ]
                }
            ],
            "models": {
                "availableModels": [
                    { "modelId": "gemini-4-neon-low", "name": "Gemini 4 Neon (Low)" },
                    { "modelId": "gemini-4-neon-xhigh", "name": "Gemini 4 Neon (XHigh)" },
                    { "modelId": "gemini-3.8-flash-low", "name": "Gemini 3.8 Flash (Low)" },
                    { "modelId": "gemini-3.8-flash-medium", "name": "Gemini 3.8 Flash (Medium)" },
                    { "modelId": "gemini-3.8-flash-high", "name": "Gemini 3.8 Flash (High)" }
                ]
            }
        });
        let models = parse_agy_acp_models(&result);
        assert_eq!(models.len(), 3);
        assert!(!models[0].is_default);
        assert!(models[1].is_default);
        assert_eq!(
            models[0]
                .reasoning_efforts
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            ["low", "xhigh"]
        );
        assert_eq!(
            models[1]
                .reasoning_efforts
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            ["low", "medium", "high"]
        );
        // The session's current effort is the default only where the model
        // offers it; Gemini 4 Neon has no `medium`, so it keeps its own.
        assert_eq!(
            models[1].default_reasoning_effort.as_deref(),
            Some("medium")
        );
        assert_ne!(
            models[0].default_reasoning_effort.as_deref(),
            Some("medium")
        );
        assert!(models[2].reasoning_efforts.is_empty());
    }

    #[test]
    fn reconciles_single_effort_agy_model_against_authoritative_session_bases() {
        // `agy models` left a single-effort variant uncollapsed because only 1
        // row shared its stem; the live session's base option list resolves it.
        let cached = parse_agy_models_output(
            "claude-sonnet-5-5-medium\tClaude Sonnet 5.5 (Medium)\n\
             gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n",
        );
        assert_eq!(cached[0].id, "claude-sonnet-5-5-medium");
        let session_result = json!({
            "configOptions": [{
                "id": "model",
                "category": "model",
                "currentValue": "claude-sonnet-5-5",
                "options": [
                    { "value": "claude-sonnet-5-5", "name": "Claude Sonnet 5.5" },
                    { "value": "gpt-oss-120b-medium", "name": "GPT-OSS 120B (Medium)" }
                ]
            }]
        });
        let reconciled = reconcile_agy_catalog(Some(&cached), &session_result)
            .expect("single-effort base should reconcile");
        assert_eq!(reconciled[0].id, "claude-sonnet-5-5");
        assert_eq!(reconciled[0].name, "Claude Sonnet 5.5");
        assert_eq!(reconciled[0].reasoning_efforts.len(), 1);
        assert_eq!(reconciled[0].reasoning_efforts[0].id, "medium");
        assert_eq!(reconciled[1].id, "gpt-oss-120b-medium");
        assert!(reconciled[1].reasoning_efforts.is_empty());
    }
}
