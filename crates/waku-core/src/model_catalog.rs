//! Provider model and agent-preset discovery.

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

pub fn fallback_models(provider: ProviderKind) -> Vec<ProviderModel> {
    match provider {
        ProviderKind::Codex => [
            ProviderModel::new("gpt-5.6-sol", "GPT-5.6-Sol").default(),
            ProviderModel::new("gpt-5.6-terra", "GPT-5.6-Terra"),
            ProviderModel::new("gpt-5.6-luna", "GPT-5.6-Luna"),
            ProviderModel::new("gpt-5.5", "GPT-5.5"),
            ProviderModel::new("gpt-5.4", "GPT-5.4"),
        ]
        .into_iter()
        .map(|model| {
            model
                .reasoning(
                    reasoning_options(["low", "medium", "high", "xhigh"]),
                    "medium",
                )
                .service_tiers(
                    [ProviderModelOption::new("fast", tr!("model_option.fast"))
                        .description(tr!("model_option.fast_description"))],
                    "default",
                )
        })
        .collect(),
        ProviderKind::Claude => vec![
            claude_long_context(claude_ultracode_model("claude-fable-5", "Claude Fable 5")),
            claude_long_context(claude_ultracode_model("claude-opus-5", "Claude Opus 5")),
            claude_long_context(claude_ultracode_model("claude-opus-4-8", "Claude Opus 4.8")),
            claude_long_context(claude_ultracode_model("claude-opus-4-7", "Claude Opus 4.7")),
            claude_long_context(claude_reasoning_model("claude-opus-4-6", "Claude Opus 4.6")),
            claude_reasoning_model("claude-opus-4-5", "Claude Opus 4.5"),
            claude_long_context(claude_ultracode_model("claude-sonnet-5", "Claude Sonnet 5"))
                .default(),
            claude_long_context(claude_reasoning_model(
                "claude-sonnet-4-6",
                "Claude Sonnet 4.6",
            )),
            ProviderModel::new("claude-haiku-4-5", "Claude Haiku 4.5"),
        ],
        // Antigravity models depend on the signed-in Google or enterprise
        // account, and headless runs exit non-zero on ineligible slugs.
        ProviderKind::Antigravity => Vec::new(),
        // Pi takes its catalog from the user's configured LLM providers.
        // A fabricated fallback would make unavailable models look selectable.
        ProviderKind::Pi => Vec::new(),
    }
}

/// Discovers ordinary models and provider-owned agent compositions.
pub fn discover_catalog(
    provider: ProviderKind,
    binary: &Path,
) -> (Vec<ProviderModel>, Vec<ProviderAgentPreset>) {
    let (discovered, discovered_presets) = match provider {
        ProviderKind::Codex => (discover_codex_models(binary), None),
        ProviderKind::Antigravity => (discover_antigravity_models(binary), None),
        ProviderKind::Claude => (discover_claude_models(binary), None),
        ProviderKind::Pi => (discover_pi_models(binary), None),
    };
    let models = if discovered.is_empty() {
        // A failed or empty probe keeps the last successful discovery over
        // the hardcoded catalog, so one bad CLI run can't shrink the picker.
        cached_models(provider).unwrap_or_else(|| fallback_models(provider))
    } else {
        let models = deduplicate(discovered);
        write_cached_models(provider, &models);
        models
    };
    let presets = discovered_presets.unwrap_or_default();
    (models, presets)
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
                // `ultracode` is Waku's orchestration effort. Claude accepts
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
const ANTIGRAVITY_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

pub(crate) fn split_antigravity_effort_suffix(slug: &str) -> Option<(&str, &str)> {
    let (base, suffix) = slug.rsplit_once('-')?;
    if base.is_empty() {
        return None;
    }
    let canonical = ANTIGRAVITY_EFFORTS
        .iter()
        .copied()
        .find(|known| suffix.eq_ignore_ascii_case(known))?;
    Some((base, canonical))
}

/// Resolve `(model, reasoning_effort)` into non-conflicting CLI arguments for
/// `agy`. `agy` rejects `--model foo-medium --effort low` with a conflict error
/// while accepting `--model foo --effort low`, and single-variant slugs such as
/// `gpt-oss-120b-medium` carry their effort in the slug itself.
pub(crate) fn resolve_antigravity_model_and_effort(
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> (Option<String>, Option<String>) {
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    let effort = reasoning_effort.map(str::trim).filter(|e| !e.is_empty());
    match (model, effort) {
        (Some(slug), Some(chosen_effort)) => {
            if let Some((base, embedded_effort)) = split_antigravity_effort_suffix(slug) {
                if embedded_effort.eq_ignore_ascii_case(chosen_effort) {
                    (Some(slug.to_owned()), None)
                } else {
                    (Some(base.to_owned()), Some(chosen_effort.to_owned()))
                }
            } else {
                (Some(slug.to_owned()), Some(chosen_effort.to_owned()))
            }
        }
        (Some(slug), None) => (Some(slug.to_owned()), None),
        (None, Some(chosen_effort)) => (None, Some(chosen_effort.to_owned())),
        (None, None) => (None, None),
    }
}

fn discover_antigravity_models(binary: &Path) -> Vec<ProviderModel> {
    let Some(models_json) =
        run_antigravity_json_command(binary, &["--output-format", "json", "models"])
    else {
        return Vec::new();
    };
    let active =
        run_antigravity_json_command(binary, &["--output-format", "json", "--print", "/model"]);
    parse_antigravity_models(&models_json, active.as_ref())
}

fn run_antigravity_json_command(binary: &Path, args: &[&str]) -> Option<Value> {
    let cwd = crate::session_catalog::catalog_working_directory().ok()?;
    let mut command = crate::command_env::command(binary);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = crate::command_env::spawn(&mut command).ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(val) = serde_json::from_str::<Value>(&line) {
                let _ = tx.send(val);
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut result = None;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(remaining) {
            Ok(val) => {
                if val.get("status").and_then(Value::as_str) == Some("SUCCESS") {
                    result = Some(val);
                    break;
                }
            }
            Err(_) => break,
        }
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    result
}

fn strip_antigravity_effort_label<'a>(label: &'a str, effort: &str) -> &'a str {
    let trimmed = label.trim();
    if let Some((prefix, rest)) = trimmed.rsplit_once('(')
        && let Some(inner) = rest.strip_suffix(')')
        && inner.trim().eq_ignore_ascii_case(effort)
    {
        let cleaned = prefix.trim();
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    trimmed
}

fn parse_antigravity_models(
    models_envelope: &Value,
    active_envelope: Option<&Value>,
) -> Vec<ProviderModel> {
    let mut raw_entries: Vec<(String, String)> = models_envelope
        .pointer("/command/data/models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?.trim();
            if id.is_empty() {
                return None;
            }
            let label = entry
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| display_name_from_slug(id));
            Some((id.to_owned(), label))
        })
        .collect();

    if raw_entries.is_empty()
        && let Some(response) = models_envelope.get("response").and_then(Value::as_str)
    {
        for line in response.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (id, label) = match line.split_once('\t') {
                Some((id, label)) => (id.trim(), label.trim()),
                None => (line, ""),
            };
            if !id.is_empty() {
                let label = if label.is_empty() {
                    display_name_from_slug(id)
                } else {
                    label.to_owned()
                };
                raw_entries.push((id.to_owned(), label));
            }
        }
    }

    if raw_entries.is_empty() {
        return Vec::new();
    }

    let active_slug = active_envelope
        .and_then(|val| val.pointer("/command/data/id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let active_base_and_effort = active_slug.and_then(|slug| {
        split_antigravity_effort_suffix(slug)
            .map(|(b, e)| (b.to_owned(), Some(e.to_owned())))
            .or_else(|| Some((slug.to_owned(), None)))
    });

    struct Group {
        base_id: String,
        base_label: String,
        raw_variants: Vec<(String, String, &'static str)>,
    }

    let mut groups: Vec<Group> = Vec::new();
    for (slug, label) in raw_entries {
        if let Some((base, effort_str)) = split_antigravity_effort_suffix(&slug)
            && let Some(&canonical_effort) = ANTIGRAVITY_EFFORTS.iter().find(|&&e| e == effort_str)
        {
            let base_label = strip_antigravity_effort_label(&label, canonical_effort).to_owned();
            if let Some(existing) = groups
                .iter_mut()
                .find(|g| g.base_id == base && !g.raw_variants.is_empty())
            {
                if !existing
                    .raw_variants
                    .iter()
                    .any(|(_, _, e)| *e == canonical_effort)
                {
                    existing.raw_variants.push((slug, label, canonical_effort));
                }
            } else {
                groups.push(Group {
                    base_id: base.to_owned(),
                    base_label,
                    raw_variants: vec![(slug, label, canonical_effort)],
                });
            }
        } else {
            groups.push(Group {
                base_id: slug,
                base_label: label,
                raw_variants: Vec::new(),
            });
        }
    }

    let mut out = Vec::with_capacity(groups.len());
    let mut marked_default = false;

    for group in groups {
        if group.raw_variants.len() >= 2 {
            let mut sorted_efforts: Vec<&'static str> = group
                .raw_variants
                .iter()
                .map(|(_, _, effort)| *effort)
                .collect();
            sorted_efforts.sort_by_key(|effort| {
                ANTIGRAVITY_EFFORTS
                    .iter()
                    .position(|known| known == effort)
                    .unwrap_or(usize::MAX)
            });
            let is_active_group = active_base_and_effort
                .as_ref()
                .is_some_and(|(active_base, _)| active_base == &group.base_id);
            let default_effort = active_base_and_effort
                .as_ref()
                .filter(|(active_base, _)| active_base == &group.base_id)
                .and_then(|(_, eff)| eff.as_deref())
                .filter(|eff| sorted_efforts.contains(eff))
                .or_else(|| sorted_efforts.iter().copied().find(|e| *e == "high"))
                .or_else(|| sorted_efforts.iter().copied().find(|e| *e == "medium"))
                .unwrap_or(sorted_efforts[0]);
            let options = sorted_efforts
                .iter()
                .map(|effort| ProviderModelOption::new(*effort, reasoning_effort_label(effort)));
            let mut model = ProviderModel::new(group.base_id, group.base_label)
                .reasoning(options, default_effort);
            if is_active_group && !marked_default {
                model = model.default();
                marked_default = true;
            }
            out.push(model);
        } else if let Some((raw_slug, raw_label, _)) = group.raw_variants.into_iter().next() {
            let is_active = active_slug == Some(raw_slug.as_str());
            let mut model = ProviderModel::new(raw_slug, raw_label);
            if is_active && !marked_default {
                model = model.default();
                marked_default = true;
            }
            out.push(model);
        } else {
            let is_active = active_slug == Some(group.base_id.as_str());
            let mut model = ProviderModel::new(group.base_id, group.base_label);
            if is_active && !marked_default {
                model = model.default();
                marked_default = true;
            }
            out.push(model);
        }
    }

    if !marked_default && let Some(first) = out.first_mut() {
        first.is_default = true;
    }
    out
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

    let models_request = json!({"id": "waku-models", "type": "get_available_models"});
    let result = if write_json_line(&mut stdin, &models_request).is_ok()
        && let Some(models) = recv_pi_rpc_response(&rx, "waku-models", PI_RPC_TIMEOUT)
    {
        // The catalog is useful even when an older Pi cannot report its
        // current state. State only marks the default model and thinking
        // level, so ask for it after the required model response.
        let state_request = json!({"id": "waku-state", "type": "get_state"});
        let state = if write_json_line(&mut stdin, &state_request).is_ok() {
            recv_pi_rpc_response(&rx, "waku-state", PI_RPC_TIMEOUT).unwrap_or(Value::Null)
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

fn parse_pi_model_response(
    state: &Value,
    response: &Value,
) -> Vec<ProviderModel> {
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
                "name": "waku",
                "title": "Waku",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_fake_model_cli(name: &str, contents: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = std::env::temp_dir().join(format!(
            "waku-{name}-{}-model-discovery.sh",
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
            std::env::temp_dir().join(format!("waku-model-cache-test-{}", std::process::id()));
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
printf '%s\n' '{"type":"control_response","response":{"request_id":"waku-initialize-catalog","response":{"models":[{"value":"cc-switch-model","resolvedModel":"cc-switch-model","displayName":"CC Switch Model"}]}}}'
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
    fn parses_pi_models_and_model_specific_thinking_levels() {
        let state = json!({
            "id": "waku-state",
            "type": "response",
            "success": true,
            "data": {
                "model": {"provider": "github-copilot", "id": "gpt-5.6-terra"},
                "thinkingLevel": "xhigh"
            }
        });
        let response = json!({
            "id": "waku-models",
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
    *waku-models*)
      printf '%s\n' '{"id":"waku-models","type":"response","success":true,"data":{"models":[{"provider":"extension-provider","id":"extension-model","name":"Extension Model","reasoning":false}]}}'
      ;;
    *waku-state*)
      printf '%s\n' '{"id":"waku-state","type":"response","success":true,"data":{"model":{"provider":"extension-provider","id":"extension-model"},"thinkingLevel":"medium"}}'
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
    fn antigravity_models_collapse_multi_effort_families_and_keep_singletons() {
        let models_json = json!({
            "status": "SUCCESS",
            "command": {
                "name": "models",
                "data": {
                    "models": [
                        {"id": "gemini-4-argon-medium", "label": "Gemini 4 Argon (Medium)"},
                        {"id": "gemini-4-argon-high", "label": "Gemini 4 Argon (High)"},
                        {"id": "gemini-4-argon-low", "label": "Gemini 4 Argon (Low)"},
                        {"id": "claude-opus-5-5-low", "label": "Claude Opus 5.5 (Low)"},
                        {"id": "claude-opus-5-5-high", "label": "Claude Opus 5.5 (High)"},
                        {"id": "gpt-oss-120b-medium", "label": "GPT-OSS 120B (Medium)"}
                    ]
                }
            }
        });
        let active_json = json!({
            "status": "SUCCESS",
            "command": {
                "name": "model",
                "data": {"id": "claude-opus-5-5-low", "effort": "low"}
            }
        });

        let parsed = parse_antigravity_models(&models_json, Some(&active_json));
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].id, "gemini-4-argon");
        assert_eq!(parsed[0].name, "Gemini 4 Argon");
        assert_eq!(
            parsed[0]
                .reasoning_efforts
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            ["low", "medium", "high"]
        );
        assert!(!parsed[0].is_default);

        assert_eq!(parsed[1].id, "claude-opus-5-5");
        assert_eq!(parsed[1].name, "Claude Opus 5.5");
        assert!(parsed[1].is_default);
        assert_eq!(parsed[1].default_reasoning_effort.as_deref(), Some("low"));

        assert_eq!(parsed[2].id, "gpt-oss-120b-medium");
        assert_eq!(parsed[2].name, "GPT-OSS 120B (Medium)");
        assert!(parsed[2].reasoning_efforts.is_empty());

        assert_eq!(
            resolve_antigravity_model_and_effort(Some("gemini-4-argon"), Some("high")),
            (Some("gemini-4-argon".into()), Some("high".into()))
        );
        assert_eq!(
            resolve_antigravity_model_and_effort(Some("gemini-4-argon-medium"), Some("low")),
            (Some("gemini-4-argon".into()), Some("low".into()))
        );
        assert_eq!(
            resolve_antigravity_model_and_effort(Some("gpt-oss-120b-medium"), None),
            (Some("gpt-oss-120b-medium".into()), None)
        );
    }
}
