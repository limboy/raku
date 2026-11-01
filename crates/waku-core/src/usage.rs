//! Account plan-usage limits per provider, read the way CodexBar reads them:
//! Claude's OAuth credential (macOS keychain first, then
//! `~/.claude/.credentials.json`) calls `api.anthropic.com/api/oauth/usage`; Codex's
//! `~/.codex/auth.json` token calls the ChatGPT backend's usage endpoint;
//! OpenCode Go's API key calls `opencode.ai/zen/go/v1/usage`; Oh My Pi reports
//! its OAuth and subscription pools via
//! `omp usage --json --no-extensions`. Payload shapes were verified against
//! live responses or the providers' own schemas, not guessed.
//!
//! Everything in this module blocks on subprocesses and the network and must
//! run on the background executor. Render reads only the parsed snapshot the
//! app entity stores.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[cfg(target_os = "macos")]
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
/// The usage endpoint rejects requests without this beta header.
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
/// User-Agent when the CLI's probed version is not known yet.
const FALLBACK_CLI_VERSION: &str = "2.1.0";

/// The absolute path keeps a shadowed `curl` on `PATH` out of the credential
/// exchange. Windows 10 build 17063 and later ship the same tool in System32.
#[cfg(not(windows))]
const CURL_PATH: &str = "/usr/bin/curl";
#[cfg(windows)]
const CURL_PATH: &str = r"C:\Windows\System32\curl.exe";

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const OPENCODE_GO_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

pub use waku_protocol::usage::{PlanUsage, PlanWindow, format_tokens, reset_label};

struct OauthCredentials {
    access_token: String,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
}

/// Fetch the Claude account's plan usage. Blocking: keychain read, then one
/// HTTPS round trip. Never call from the UI thread.
pub fn fetch_claude_plan_usage(cli_version: Option<&str>) -> anyhow::Result<PlanUsage> {
    let credentials = read_credentials()?;
    let user_agent = format!(
        "claude-code/{}",
        cli_version.unwrap_or(FALLBACK_CLI_VERSION)
    );
    let (status, body) = http_get(
        CLAUDE_USAGE_URL,
        &[
            format!("Authorization: Bearer {}", credentials.access_token),
            format!("anthropic-beta: {OAUTH_BETA_HEADER}"),
            "Accept: application/json".to_owned(),
            format!("User-Agent: {user_agent}"),
        ],
    )?;
    match status {
        200 => {}
        401 | 403 => {
            return Err(anyhow!(tr!(
                "usage_error.signin_cannot_read",
                provider = "Claude Code",
                status = status
            )));
        }
        429 => return Err(anyhow!(tr!("usage_error.rate_limited"))),
        other => return Err(anyhow!(tr!("usage_error.http_status", status = other))),
    }
    let body: Value = serde_json::from_str(&body).context(tr!("usage_error.invalid_json"))?;
    let mut usage = parse_plan_usage(&body, &credentials);
    // The stored credential's tier is login-time metadata and survives plan
    // changes unchanged — verified live: a keychain saying `max_5x` against a
    // profile reporting `max_20x`. The profile's organization tier is the
    // account's current plan, so it wins; the credential label stays as the
    // fallback when the profile is unreachable.
    if let Some(label) = fetch_claude_profile_plan_label(&credentials.access_token, &user_agent) {
        usage.plan_label = Some(label);
    }
    Ok(usage)
}

fn fetch_claude_profile_plan_label(access_token: &str, user_agent: &str) -> Option<String> {
    let (status, body) = http_get(
        CLAUDE_PROFILE_URL,
        &[
            format!("Authorization: Bearer {access_token}"),
            "Accept: application/json".to_owned(),
            format!("User-Agent: {user_agent}"),
        ],
    )
    .ok()?;
    if status != 200 {
        return None;
    }
    profile_plan_label(&serde_json::from_str(&body).ok()?)
}

/// "Max (20x)" from the profile's organization: `organization_type`
/// ("claude_max") names the plan, `rate_limit_tier`
/// ("default_claude_max_20x") carries the usage multiple.
fn profile_plan_label(body: &Value) -> Option<String> {
    let organization = body.get("organization")?;
    let tier = organization.get("rate_limit_tier").and_then(Value::as_str);
    let subscription = organization
        .get("organization_type")
        .and_then(Value::as_str)
        .and_then(|organization_type| organization_type.strip_prefix("claude_"));
    plan_label(subscription, tier)
}

/// Fetch the ChatGPT account's Codex rate limits: `~/.codex/auth.json` holds
/// the OAuth token and account id, and the ChatGPT backend answers with the
/// same primary/secondary windows the CLI's own status view shows. Blocking.
pub fn fetch_codex_plan_usage() -> anyhow::Result<PlanUsage> {
    let path = dirs::home_dir()
        .ok_or_else(|| anyhow!(tr!("usage_error.no_home_directory")))?
        .join(".codex/auth.json");
    let auth: Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .with_context(|| tr!("usage_error.read_file", path = path.display()))?,
    )
    .context(tr!("usage_error.codex_auth_invalid"))?;
    let access_token = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!(tr!("usage_error.codex_access_token_missing")))?;
    let mut headers = vec![
        format!("Authorization: Bearer {access_token}"),
        "Accept: application/json".to_owned(),
        "User-Agent: waku".to_owned(),
    ];
    if let Some(account_id) = auth.pointer("/tokens/account_id").and_then(Value::as_str) {
        headers.push(format!("ChatGPT-Account-Id: {account_id}"));
    }
    let (status, body) = http_get(CODEX_USAGE_URL, &headers)?;
    match status {
        200 => {}
        401 | 403 => {
            return Err(anyhow!(tr!(
                "usage_error.signin_cannot_read",
                provider = "Codex",
                status = status
            )));
        }
        429 => return Err(anyhow!(tr!("usage_error.rate_limited"))),
        other => return Err(anyhow!(tr!("usage_error.http_status", status = other))),
    }
    let body: Value = serde_json::from_str(&body).context(tr!("usage_error.invalid_json"))?;
    parse_codex_plan_usage(&body).ok_or_else(|| anyhow!(tr!("usage_error.no_rate_limit_windows")))
}

/// Fetch OpenCode Go's rolling, weekly, and monthly subscription limits.
/// `None` means OpenCode has no Go credential, which is normal for people
/// using Zen or any of OpenCode's many other providers. Blocking.
pub fn fetch_opencode_go_plan_usage() -> anyhow::Result<Option<PlanUsage>> {
    let Some(api_key) = opencode_go_api_key() else {
        return Ok(None);
    };
    let (status, body) = http_get(
        OPENCODE_GO_USAGE_URL,
        &[
            format!("Authorization: Bearer {api_key}"),
            "Accept: application/json".to_owned(),
            "User-Agent: waku".to_owned(),
        ],
    )?;
    match status {
        200 => {}
        // 401 and 403 mean different things here and lead to different fixes.
        // The key is rejected only on 401; a 403 is a valid key whose account
        // is not entitled ("OpenCode Go subscription required."), and telling
        // that user to reconnect sends them round a loop that cannot help.
        401 => {
            return Err(anyhow!(tr!(
                "usage_error.opencode_go_key_rejected",
                status = status
            )));
        }
        403 => {
            return Err(anyhow!(
                usage_error_detail(&body)
                    .unwrap_or_else(|| tr!("usage_error.opencode_go_not_entitled"))
            ));
        }
        429 => return Err(anyhow!(tr!("usage_error.rate_limited"))),
        other => return Err(anyhow!(tr!("usage_error.http_status", status = other))),
    }
    let body: Value = serde_json::from_str(&body).context(tr!("usage_error.invalid_json"))?;
    parse_opencode_go_plan_usage(&body)
        .map(Some)
        .ok_or_else(|| anyhow!(tr!("usage_error.no_rate_limit_windows")))
}

/// The endpoint's own sentence, when it sent one.
///
/// These bodies are `{"type":"error","error":{"type":..,"message":..}}`, and
/// the message is the only part that says what to actually do — an
/// `EntitlementError` and a revoked key are both HTTP 403 otherwise.
fn usage_error_detail(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_owned)
}

/// Match OpenCode's credential precedence closely enough for its Go provider:
/// `OPENCODE_AUTH_CONTENT` replaces the stored credentials, a stored provider
/// entry overrides the catalog environment key, and `OPENCODE_API_KEY` remains
/// the fallback.
///
/// OpenCode keeps credentials in its own database, which it seeds once from
/// the older `auth.json` and never writes back. The database is therefore
/// authoritative wherever it exists — a key removed there must not return from
/// the file — and `auth.json` only answers for a machine that has not started
/// OpenCode since upgrading.
fn opencode_go_api_key() -> Option<String> {
    let stored = match std::env::var("OPENCODE_AUTH_CONTENT")
        .ok()
        .and_then(|payload| serde_json::from_str::<Value>(&payload).ok())
    {
        Some(auth) => opencode_go_api_key_from_auth(Some(&auth)),
        None => opencode_data_directory().and_then(|data| {
            opencode_go_api_key_from_store(&opencode_database_path(&data)).unwrap_or_else(|| {
                let payload = std::fs::read_to_string(data.join("auth.json")).ok()?;
                let auth = serde_json::from_str::<Value>(&payload).ok()?;
                opencode_go_api_key_from_auth(Some(&auth))
            })
        }),
    };
    stored.or_else(|| {
        std::env::var("OPENCODE_API_KEY")
            .ok()
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty())
    })
}

/// `${XDG_DATA_HOME:-~/.local/share}/opencode`, on every platform.
fn opencode_data_directory() -> Option<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/share")))?;
    Some(data_home.join("opencode"))
}

/// OpenCode's database, which `OPENCODE_DB` can move. A relative override is
/// resolved against the data directory, as OpenCode itself resolves it.
fn opencode_database_path(data: &Path) -> PathBuf {
    match std::env::var_os("OPENCODE_DB").filter(|path| !path.is_empty()) {
        Some(path) => data.join(path),
        None => data.join("opencode.db"),
    }
}

/// The Go key in OpenCode's credential store: `None` when there is no store
/// to read, `Some(None)` when the store holds no usable Go key.
///
/// Opened read-only: the store belongs to the user's running OpenCode
/// service, and Waku must never write to it.
fn opencode_go_api_key_from_store(database: &Path) -> Option<Option<String>> {
    let connection = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = connection.busy_timeout(Duration::from_millis(500));
    // A database without the table predates OpenCode's credential store.
    let mut statement = connection
        .prepare(
            "SELECT value FROM credential WHERE integration_id = 'opencode-go' \
             ORDER BY active DESC, time_updated DESC",
        )
        .ok()?;
    let values = statement
        .query_map([], |row| row.get::<_, String>(0))
        .ok()?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    Some(values.iter().find_map(|value| {
        let credential = serde_json::from_str::<Value>(value).ok()?;
        if credential.get("type").and_then(Value::as_str) != Some("key") {
            return None;
        }
        credential
            .get("key")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
    }))
}

fn opencode_go_api_key_from_auth(auth: Option<&Value>) -> Option<String> {
    let entry = auth?.get("opencode-go")?;
    if entry.get("type").and_then(Value::as_str) != Some("api") {
        return None;
    }
    entry
        .get("key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
}

/// Live shape verified on 2026-08-12. Standard Zen has no corresponding
/// `/zen/v1/usage` route, so these rows deliberately represent Go only.
fn parse_opencode_go_plan_usage(body: &Value) -> Option<PlanUsage> {
    let usage = body.get("usage")?;
    let windows = [
        ("rolling", tr!("usage.hour_limit", count = 5)),
        ("weekly", tr!("usage.weekly_limit")),
        ("monthly", tr!("usage.monthly_limit")),
    ]
    .into_iter()
    .filter_map(|(key, label)| {
        let window = usage.get(key)?;
        Some(PlanWindow {
            label,
            percent: window
                .get("percent")
                .and_then(Value::as_f64)?
                .clamp(0.0, 100.0),
            resets_at: window
                .get("resetsAt")
                .and_then(Value::as_str)
                .and_then(|reset| chrono::DateTime::parse_from_rfc3339(reset).ok())
                .map(|date| date.timestamp()),
        })
    })
    .collect::<Vec<_>>();
    if windows.is_empty() {
        return None;
    }
    Some(PlanUsage {
        plan_label: Some("Go".to_owned()),
        windows,
    })
}

/// Map the ChatGPT backend's usage response (primary/secondary windows in
/// seconds, plus model-scoped `additional_rate_limits`) into the panel's
/// rows.
fn parse_codex_plan_usage(body: &Value) -> Option<PlanUsage> {
    let mut windows = Vec::new();
    if let Some(rate_limit) = body.get("rate_limit") {
        push_codex_windows(&mut windows, rate_limit, None);
    }
    for entry in body
        .get("additional_rate_limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(rate_limit) = entry.get("rate_limit") else {
            continue;
        };
        let name = entry
            .get("limit_name")
            .and_then(Value::as_str)
            .unwrap_or("Model");
        // The Spark bonus lane is promotional quota, not a limit the account
        // plans around; its row is noise next to the real lanes.
        if name.to_ascii_lowercase().contains("spark") {
            continue;
        }
        push_codex_windows(&mut windows, rate_limit, Some(name));
    }
    if windows.is_empty() {
        return None;
    }
    Some(PlanUsage {
        plan_label: openai_plan_label(body.get("plan_type").and_then(Value::as_str)),
        windows,
    })
}

/// The account-wide lanes read "5-hour limit"; a model-scoped lane reads
/// "Weekly · GPT-5.3-Codex-Spark", mirroring the Claude panel's scoped rows.
fn push_codex_windows(windows: &mut Vec<PlanWindow>, rate_limit: &Value, scope: Option<&str>) {
    for key in ["primary_window", "secondary_window"] {
        let Some(window) = rate_limit.get(key).filter(|window| !window.is_null()) else {
            continue;
        };
        let Some(percent) = window.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        let minutes = window
            .get("limit_window_seconds")
            .and_then(Value::as_i64)
            .map(|seconds| seconds / 60);
        let base = window_label_from_minutes(minutes);
        let label = match scope {
            Some(name) => {
                let suffix = tr!("usage.limit_suffix");
                let period = base.strip_suffix(&suffix).unwrap_or(&base);
                tr!("usage.scoped_limit", period = period, name = name)
            }
            None => base,
        };
        windows.push(PlanWindow {
            label,
            percent: percent.clamp(0.0, 100.0),
            resets_at: window.get("reset_at").and_then(Value::as_i64),
        });
    }
}

/// "5-hour limit" / "Weekly limit" from a window duration, shared by the
/// Codex stream notification (minutes) and the ChatGPT usage endpoint
/// (seconds, converted by the caller).
pub fn window_label_from_minutes(minutes: Option<i64>) -> String {
    let Some(minutes) = minutes.filter(|minutes| *minutes > 0) else {
        return tr!("usage.usage_limit");
    };
    if minutes < 24 * 60 {
        tr!("usage.hour_limit", count = (minutes + 59) / 60)
    } else if minutes == 7 * 24 * 60 {
        tr!("usage.weekly_limit")
    } else {
        tr!(
            "usage.day_limit",
            count = (minutes + 24 * 60 - 1) / (24 * 60)
        )
    }
}

/// ChatGPT plan names, shared by the Codex stream notification and the usage
/// endpoint. The tier strings themselves encode the usage multiple — plain
/// `pro` is the 20x plan and `prolite` the 5x one (CodexBar ships the same
/// mapping). Unknown tiers stay unlabeled rather than guessing.
pub fn openai_plan_label(plan: Option<&str>) -> Option<String> {
    Some(
        match plan? {
            "free" | "free_workspace" | "guest" => "Free",
            "go" => "Go",
            "plus" => "Plus",
            "pro" => "Pro (20x)",
            "prolite" | "pro_lite" => "Pro (5x)",
            "team" => "Team",
            "business" | "self_serve_business_usage_based" => "Business",
            "enterprise" | "ent26" | "enterprise_cbp_usage_based" => "Enterprise",
            "edu" | "education" | "k12" => "Edu",
            _ => return None,
        }
        .to_owned(),
    )
}

/// The Claude Code OAuth blob: keychain on macOS, with the credentials file as
/// the cross-platform fallback. Claude Code stores the macOS item via
/// `security`, so `security` is on its ACL and this read does not prompt.
fn read_credentials() -> anyhow::Result<OauthCredentials> {
    #[cfg(target_os = "macos")]
    let payload = keychain_payload().or_else(|keychain_error| {
        credentials_file_payload()
            .map_err(|_| keychain_error.context(tr!("usage_error.claude_credentials_missing")))
    })?;
    #[cfg(not(target_os = "macos"))]
    let payload =
        credentials_file_payload().context(tr!("usage_error.claude_credentials_missing"))?;
    parse_credentials(&payload)
}

#[cfg(target_os = "macos")]
fn keychain_payload() -> anyhow::Result<String> {
    let output = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .stdin(Stdio::null())
        .output()
        .context(tr!("usage_error.run_security"))?;
    if !output.status.success() {
        return Err(anyhow!(tr!(
            "usage_error.keychain_item_missing",
            service = KEYCHAIN_SERVICE
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn credentials_file_payload() -> anyhow::Result<String> {
    let path = dirs::home_dir()
        .ok_or_else(|| anyhow!(tr!("usage_error.no_home_directory")))?
        .join(".claude/.credentials.json");
    std::fs::read_to_string(&path)
        .with_context(|| tr!("usage_error.read_file", path = path.display()))
}

fn parse_credentials(payload: &str) -> anyhow::Result<OauthCredentials> {
    let value: Value = serde_json::from_str(payload.trim())
        .context(tr!("usage_error.claude_credentials_invalid"))?;
    let oauth = value
        .get("claudeAiOauth")
        .ok_or_else(|| anyhow!(tr!("usage_error.claude_oauth_missing")))?;
    let access_token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!(tr!("usage_error.claude_access_token_missing")))?
        .to_owned();
    let field = |name: &str| {
        oauth
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|value| !value.is_empty())
    };
    Ok(OauthCredentials {
        access_token,
        subscription_type: field("subscriptionType"),
        rate_limit_tier: field("rateLimitTier"),
    })
}

/// GET `url` with the given header lines. Headers travel to curl as a config
/// on stdin, never on argv, so bearer tokens cannot show up in the process
/// table. Shared with the usage-history rate-table fetch.
pub fn http_get(url: &str, headers: &[String]) -> anyhow::Result<(u16, String)> {
    let mut child = crate::command_env::plain_command(CURL_PATH)
        .args(["-sS", "--max-time", "15", "-D", "-", "-K", "-", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context(tr!("usage_error.run_curl"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!(tr!("usage_error.curl_stdin_unavailable")))?;
        for header in headers {
            writeln!(stdin, "header = \"{header}\"").context(tr!("usage_error.configure_curl"))?;
        }
    }
    let output = child
        .wait_with_output()
        .context(tr!("usage_error.curl_did_not_finish"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let error = stderr
            .lines()
            .last()
            .map(str::trim)
            .filter(|error| !error.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| tr!("usage_error.unknown_error"));
        return Err(anyhow!(tr!("usage_error.curl_failed", error = error)));
    }
    split_status_and_body(&String::from_utf8_lossy(&output.stdout))
}

/// `-D -` prefixes the body with the response headers; the status code is on
/// the first line and the body follows the blank separator line.
fn split_status_and_body(raw: &str) -> anyhow::Result<(u16, String)> {
    let status = raw
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow!(tr!("usage_error.curl_no_status")))?;
    let body = raw
        .split_once("\r\n\r\n")
        .or_else(|| raw.split_once("\n\n"))
        .map(|(_, body)| body)
        .unwrap_or_default();
    Ok((status, body.to_owned()))
}

fn parse_plan_usage(body: &Value, credentials: &OauthCredentials) -> PlanUsage {
    let mut windows = limit_entry_windows(body);
    if windows.is_empty() {
        windows = flat_field_windows(body);
    }
    PlanUsage {
        plan_label: plan_label(
            credentials.subscription_type.as_deref(),
            credentials.rate_limit_tier.as_deref(),
        ),
        windows,
    }
}

/// The modern shape: a `limits` array whose entries carry a `kind` and, for
/// model-scoped lanes, the model's display name.
fn limit_entry_windows(body: &Value) -> Vec<PlanWindow> {
    let Some(limits) = body.get("limits").and_then(Value::as_array) else {
        return Vec::new();
    };
    limits
        .iter()
        .filter_map(|entry| {
            let label = match entry.get("kind").and_then(Value::as_str)? {
                "session" => tr!("usage.hour_limit", count = 5),
                "weekly_all" => tr!("usage.weekly_all_models"),
                "weekly_scoped" => {
                    let model = entry
                        .pointer("/scope/model/display_name")
                        .and_then(Value::as_str)
                        .unwrap_or("model");
                    tr!("usage.weekly_model", model = model)
                }
                // Overage/credit lanes render elsewhere if ever wanted; the
                // meter mirrors the CLI's three quota rows.
                _ => return None,
            };
            Some(PlanWindow {
                label,
                percent: entry
                    .get("percent")
                    .and_then(Value::as_f64)?
                    .clamp(0.0, 100.0),
                resets_at: parse_reset(entry.get("resets_at")),
            })
        })
        .collect()
}

/// The older flat shape, kept as a fallback for accounts the `limits` array
/// has not reached.
fn flat_field_windows(body: &Value) -> Vec<PlanWindow> {
    [
        "five_hour",
        "seven_day",
        "seven_day_opus",
        "seven_day_sonnet",
    ]
    .into_iter()
    .filter_map(|key| {
        let window = body.get(key)?;
        let label = match key {
            "five_hour" => tr!("usage.hour_limit", count = 5),
            "seven_day" => tr!("usage.weekly_all_models"),
            "seven_day_opus" => tr!("usage.weekly_model", model = "Opus"),
            "seven_day_sonnet" => tr!("usage.weekly_model", model = "Sonnet"),
            _ => return None,
        };
        Some(PlanWindow {
            label,
            percent: window
                .get("utilization")
                .and_then(Value::as_f64)?
                .clamp(0.0, 100.0),
            resets_at: parse_reset(window.get("resets_at")),
        })
    })
    .collect()
}

fn parse_reset(value: Option<&Value>) -> Option<i64> {
    let text = value?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|date| date.timestamp())
}

/// "Max (5x)" from `subscriptionType: "max"` + `rateLimitTier:
/// "default_claude_max_5x"`, matching how the CLI titles its usage panel.
fn plan_label(subscription_type: Option<&str>, rate_limit_tier: Option<&str>) -> Option<String> {
    let tier_words = rate_limit_tier
        .map(|tier| {
            tier.to_ascii_lowercase()
                .split(['_', '-', ' '])
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let base = subscription_type.map(str::to_ascii_lowercase).or_else(|| {
        ["max", "pro", "team", "enterprise"]
            .into_iter()
            .find(|plan| tier_words.iter().any(|word| word == plan))
            .map(str::to_owned)
    })?;
    let mut label = match base.as_str() {
        "max" => "Max".to_owned(),
        "pro" => "Pro".to_owned(),
        "team" => "Team".to_owned(),
        "enterprise" => "Enterprise".to_owned(),
        other => {
            let mut chars = other.chars();
            let first = chars.next()?;
            first.to_uppercase().collect::<String>() + chars.as_str()
        }
    };
    if base == "max"
        && let Some(position) = tier_words.iter().position(|word| word == "max")
        && let Some(multiplier) = tier_words.get(position + 1)
        && multiplier.ends_with('x')
        && multiplier[..multiplier.len() - 1].parse::<u32>().is_ok()
    {
        label = format!("{label} ({multiplier})");
    }
    Some(label)
}

/// Short-lived snapshot cache for `omp usage --json --no-extensions`: one CLI
/// invocation fetches every authenticated upstream provider at once, so
/// switching models or Oh My Pi sessions re-projects the warm snapshot without
/// spawning a fresh Bun process, while any 30s+ stale refresh re-runs `omp`.
const OHMYPI_USAGE_SNAPSHOT_TTL: Duration = Duration::from_secs(20);
const OHMYPI_USAGE_TIMEOUT: Duration = Duration::from_secs(20);

/// Fetch plan usage for the upstream provider backing an Oh My Pi session's
/// active `<upstream>/<model_id>` slug. Returns `Ok(None)` when the model is
/// not known yet or belongs to an upstream with no metered usage report.
/// Blocking; never call from the UI thread.
pub fn fetch_ohmypi_plan_usage(
    binary: &Path,
    model: Option<&str>,
    session_file: Option<&Path>,
) -> anyhow::Result<Option<PlanUsage>> {
    let Some((upstream, model_id)) = split_ohmypi_model(model) else {
        return Ok(None);
    };
    let pinned_hash = session_file.and_then(|path| read_ohmypi_credential_pin(path, upstream));
    let payload = cached_ohmypi_usage_json(binary)?;
    parse_ohmypi_plan_usage(&payload, upstream, model_id, pinned_hash.as_deref())
}

fn split_ohmypi_model(model: Option<&str>) -> Option<(&str, &str)> {
    let (upstream, model_id) = model?.trim().split_once('/')?;
    let upstream = upstream.trim();
    let model_id = model_id.trim();
    (!upstream.is_empty() && !model_id.is_empty()).then_some((upstream, model_id))
}

fn cached_ohmypi_usage_json(binary: &Path) -> anyhow::Result<Value> {
    use std::collections::HashMap;
    use std::sync::LazyLock;
    use std::time::Instant;

    static SNAPSHOT_CACHE: LazyLock<parking_lot::Mutex<HashMap<PathBuf, (Instant, Value)>>> =
        LazyLock::new(Default::default);

    let key = binary.to_path_buf();
    let mut cache = SNAPSHOT_CACHE.lock();
    if let Some((fetched_at, payload)) = cache.get(&key)
        && fetched_at.elapsed() < OHMYPI_USAGE_SNAPSHOT_TTL
    {
        return Ok(payload.clone());
    }
    let payload = run_ohmypi_usage_json(binary)?;
    cache.insert(key, (Instant::now(), payload.clone()));
    Ok(payload)
}

fn run_ohmypi_usage_json(binary: &Path) -> anyhow::Result<Value> {
    use std::io::Read as _;

    let mut command = crate::command_env::command(binary);
    let command = command
        .args(["usage", "--json", "--no-extensions"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child =
        crate::command_env::spawn(command).context(tr!("usage_error.start_omp_probe"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!(tr!("usage_error.start_omp_probe")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!(tr!("usage_error.start_omp_probe")))?;

    let stdout_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = BufReader::new(stdout).read_to_string(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut buf);
        buf
    });

    let deadline = std::time::Instant::now() + OHMYPI_USAGE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow!(tr!("usage_error.omp_usage_timeout")));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!(error)).context(tr!("usage_error.start_omp_probe"));
            }
        }
    };

    let stdout_text = stdout_reader.join().unwrap_or_default();
    let stderr_text = stderr_reader.join().unwrap_or_default();
    if let Ok(value) = serde_json::from_str::<Value>(stdout_text.trim())
        && value.get("reports").is_some()
    {
        return Ok(value);
    }
    if !status.success() {
        let detail = crate::model::strip_ansi(&stderr_text);
        let detail = detail
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("exit Failure");
        return Err(anyhow!(tr!("usage_error.omp_usage_failed", error = detail)));
    }
    serde_json::from_str::<Value>(stdout_text.trim()).context(tr!("usage_error.invalid_json"))
}

/// Read the latest `credential_pin` hash recorded on the active branch of an
/// Oh My Pi session JSONL file for `upstream`, stopping at any
/// `reset_boundary` just as transcript hydration does.
fn read_ohmypi_credential_pin(session_file: &Path, upstream: &str) -> Option<String> {
    use std::collections::HashMap;

    struct PinNode {
        parent_id: Option<String>,
        kind: String,
        provider: Option<String>,
        hash: Option<String>,
    }

    let file = std::fs::File::open(session_file).ok()?;
    let mut nodes = Vec::new();
    let mut ids = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.trim_start().starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        if kind == "session" {
            continue;
        }
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        ids.push(id.to_owned());
        nodes.push(PinNode {
            parent_id: value
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            kind: kind.to_owned(),
            provider: value
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_owned),
            hash: value
                .get("hash")
                .and_then(Value::as_str)
                .filter(|hash| !hash.is_empty())
                .map(str::to_owned),
        });
    }
    let mut index_by_id = HashMap::with_capacity(ids.len());
    for (idx, id) in ids.iter().enumerate() {
        index_by_id.insert(id.as_str(), idx);
    }
    let mut cursor = nodes.len().checked_sub(1)?;
    for _ in 0..=nodes.len() {
        let node = &nodes[cursor];
        if node.kind == "reset_boundary" {
            break;
        }
        if node.kind == "credential_pin"
            && node
                .provider
                .as_deref()
                .is_some_and(|provider| provider.eq_ignore_ascii_case(upstream))
            && let Some(hash) = &node.hash
        {
            return Some(hash.clone());
        }
        let Some(parent_id) = node.parent_id.as_deref() else {
            break;
        };
        let Some(&next) = index_by_id.get(parent_id) else {
            break;
        };
        cursor = next;
    }
    None
}

/// Exact port of Oh My Pi's `credentialPinHash(provider, identity)` over a
/// `UsageReport`'s attributed identity (`email`, `accountId`, `orgId`,
/// `projectId`).
fn ohmypi_credential_pin_hash(provider: &str, report: &Value) -> Option<String> {
    let metadata = report.get("metadata");
    let meta_str = |key: &str| {
        metadata
            .and_then(|meta| meta.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let scoped_str = |key: &str| {
        report
            .get("limits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find_map(|limit| {
                limit
                    .get("scope")
                    .and_then(|scope| scope.get(key))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
            })
    };
    let email = meta_str("email");
    let account_id = meta_str("accountId")
        .or_else(|| meta_str("account_id"))
        .or_else(|| scoped_str("accountId"));
    if account_id.is_none() && email.is_none() {
        return None;
    }
    let org_id = meta_str("orgId").or_else(|| scoped_str("orgId"));
    let project_id = meta_str("projectId").or_else(|| scoped_str("projectId"));

    let mut hasher = Sha256::new();
    for (index, part) in [
        provider,
        account_id.unwrap_or(""),
        email.unwrap_or(""),
        org_id.unwrap_or(""),
        project_id.unwrap_or(""),
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            hasher.update(b"\0");
        }
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Some(hex)
}

fn parse_ohmypi_plan_usage(
    payload: &Value,
    upstream: &str,
    model_id: &str,
    pinned_hash: Option<&str>,
) -> anyhow::Result<Option<PlanUsage>> {
    let upstream_lower = upstream.to_ascii_lowercase();
    let matching = payload
        .get("reports")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|report| {
            report
                .get("provider")
                .and_then(Value::as_str)
                .is_some_and(|provider| provider.eq_ignore_ascii_case(upstream))
        })
        .collect::<Vec<_>>();

    if matching.is_empty() {
        if let Some(disabled) = payload
            .get("disabledCredentials")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|entry| {
                entry
                    .get("provider")
                    .and_then(Value::as_str)
                    .is_some_and(|provider| provider.eq_ignore_ascii_case(upstream))
            })
        {
            let reason = disabled
                .get("reason")
                .or_else(|| disabled.get("error"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .unwrap_or("credential disabled");
            return Err(anyhow!(tr!("usage_error.omp_usage_failed", error = reason)));
        }
        return Ok(None);
    }

    let report = select_ohmypi_account_report(&matching, &upstream_lower, pinned_hash);
    let raw_limits = report
        .get("limits")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let scoped_limits = scope_ohmypi_limits(&upstream_lower, model_id, raw_limits);
    let windows = scoped_limits
        .into_iter()
        .filter_map(|limit| {
            let percent = ohmypi_limit_used_percent(limit)?;
            let resets_at = limit
                .pointer("/window/resetsAt")
                .and_then(|value| {
                    value
                        .as_i64()
                        .or_else(|| value.as_f64().map(|ms| ms as i64))
                })
                .filter(|ms| *ms > 0)
                .map(|ms| ms / 1000);
            Some(PlanWindow {
                label: ohmypi_limit_label(&upstream_lower, limit),
                percent,
                resets_at,
            })
        })
        .collect::<Vec<_>>();

    let plan_label = ohmypi_plan_label(&upstream_lower, report);
    if windows.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlanUsage {
        plan_label,
        windows,
    }))
}

fn select_ohmypi_account_report<'a>(
    reports: &[&'a Value],
    upstream_lower: &str,
    pinned_hash: Option<&str>,
) -> &'a Value {
    if reports.len() == 1 {
        return reports[0];
    }
    if let Some(pin) = pinned_hash
        && let Some(&matched) = reports.iter().find(|report| {
            ohmypi_credential_pin_hash(upstream_lower, report).as_deref() == Some(pin)
        })
    {
        return matched;
    }
    reports
        .iter()
        .copied()
        .find(|report| {
            let allowed = report
                .pointer("/metadata/allowed")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let has_headroom = report
                .get("limits")
                .and_then(Value::as_array)
                .is_none_or(|limits| {
                    limits.is_empty()
                        || limits.iter().any(|limit| {
                            limit.get("status").and_then(Value::as_str) != Some("exhausted")
                        })
                });
            allowed && has_headroom
        })
        .unwrap_or(reports[0])
}

fn scope_ohmypi_limits<'a>(
    upstream_lower: &str,
    model_id: &str,
    limits: &'a [Value],
) -> Vec<&'a Value> {
    let model_lower = model_id.to_ascii_lowercase();
    match upstream_lower {
        "google-antigravity" => {
            // Scope to the active model's backend counter BEFORE collapsing
            // `sharedGroup`, so `openai` models keep their `google-antigravity:openai:*`
            // rows instead of losing them to the `anthropic` copy first.
            let candidates: Vec<&Value> =
                if let Some(counter) = antigravity_counter_for_model(&model_lower) {
                    let prefix = format!("google-antigravity:{counter}:");
                    let matched = limits
                        .iter()
                        .filter(|limit| {
                            limit
                                .get("id")
                                .and_then(Value::as_str)
                                .is_some_and(|id| id.to_ascii_lowercase().starts_with(&prefix))
                        })
                        .collect::<Vec<_>>();
                    if !matched.is_empty() {
                        matched
                    } else {
                        let fallback = limits
                            .iter()
                            .filter(|limit| {
                                limit.get("id").and_then(Value::as_str).is_some_and(|id| {
                                    id.to_ascii_lowercase()
                                        .starts_with("google-antigravity:default:")
                                })
                            })
                            .collect::<Vec<_>>();
                        if !fallback.is_empty() {
                            fallback
                        } else {
                            limits.iter().collect()
                        }
                    }
                } else {
                    limits.iter().collect()
                };
            let mut deduped = collapse_ohmypi_shared_groups(candidates);
            // Oh My Pi sorts Antigravity limits by remainingFraction ascending
            // for credential ranking; sort display rows by window duration
            // (5-hour before Weekly) and put the active subfamily first so rows
            // never jump order between refreshes.
            let is_argon = model_lower.contains("argon");
            deduped.sort_by_key(|limit| {
                let duration = limit
                    .pointer("/window/durationMs")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MAX);
                let limit_is_argon = limit
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.to_ascii_lowercase().contains("argon"));
                let subfamily_rank = u8::from(limit_is_argon != is_argon);
                (duration, subfamily_rank)
            });
            deduped
        }
        "openai-codex" => {
            let deduped = collapse_ohmypi_shared_groups(limits.iter().collect());
            let is_spark = model_lower.contains("-spark");
            let has_plan_windows = deduped.iter().any(|limit| {
                matches!(
                    limit.get("id").and_then(Value::as_str),
                    Some("openai-codex:primary" | "openai-codex:secondary")
                )
            });
            let has_spark_windows = deduped.iter().any(|limit| {
                limit
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| id.split(':').nth(1))
                    == Some("spark")
            });
            deduped
                .into_iter()
                .filter(|limit| {
                    let Some(id) = limit.get("id").and_then(Value::as_str) else {
                        return true;
                    };
                    if id == "openai-codex:spend-cap" {
                        return !has_plan_windows;
                    }
                    let second = id.split(':').nth(1);
                    if second == Some("spark") {
                        return is_spark;
                    }
                    if matches!(id, "openai-codex:primary" | "openai-codex:secondary")
                        && is_spark
                        && has_spark_windows
                    {
                        return false;
                    }
                    true
                })
                .collect()
        }
        "google-gemini-cli" => {
            let deduped = collapse_ohmypi_shared_groups(limits.iter().collect());
            let exact = deduped
                .iter()
                .copied()
                .filter(|limit| {
                    limit
                        .pointer("/scope/modelId")
                        .and_then(Value::as_str)
                        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(model_id))
                })
                .collect::<Vec<_>>();
            if !exact.is_empty() {
                return exact;
            }
            if let Some(tier) = gemini_cli_tier_for_model(&model_lower) {
                let tiered = deduped
                    .iter()
                    .copied()
                    .filter(|limit| {
                        limit
                            .pointer("/scope/tier")
                            .and_then(Value::as_str)
                            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(tier))
                    })
                    .collect::<Vec<_>>();
                if !tiered.is_empty() {
                    return tiered;
                }
            }
            deduped
        }
        "anthropic" => {
            let deduped = collapse_ohmypi_shared_groups(limits.iter().collect());
            let without_extra = deduped
                .iter()
                .copied()
                .filter(|limit| limit.get("id").and_then(Value::as_str) != Some("anthropic:extra"))
                .collect::<Vec<_>>();
            if without_extra.is_empty() {
                deduped
            } else {
                without_extra
            }
        }
        _ => collapse_ohmypi_shared_groups(limits.iter().collect()),
    }
}

fn collapse_ohmypi_shared_groups<'a>(limits: Vec<&'a Value>) -> Vec<&'a Value> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(limits.len());
    for limit in limits {
        if let Some(group) = limit
            .pointer("/scope/sharedGroup")
            .and_then(Value::as_str)
            .filter(|group| !group.is_empty())
            && !seen.insert(group)
        {
            continue;
        }
        out.push(limit);
    }
    out
}

fn antigravity_counter_for_model(model_lower: &str) -> Option<&'static str> {
    if model_lower.contains("claude-") {
        Some("anthropic")
    } else if model_lower.contains("gemini-")
        || model_lower.contains("gemma-")
        || model_lower.contains("tab_")
    {
        Some("google")
    } else if model_lower.contains("gpt-") || model_lower.contains("openai/") {
        Some("openai")
    } else {
        None
    }
}

fn gemini_cli_tier_for_model(model_lower: &str) -> Option<&'static str> {
    if matches!(
        model_lower,
        "gemini-3-flash-preview" | "gemini-3-flash" | "gemini-3.5-flash"
    ) {
        Some("3-Flash")
    } else if model_lower.contains("flash") {
        Some("Flash")
    } else if model_lower.contains("pro") {
        Some("Pro")
    } else {
        None
    }
}

/// Exact port of Oh My Pi's `resolveUsedFraction(limit)` scaled to `0..=100`.
fn ohmypi_limit_used_percent(limit: &Value) -> Option<f64> {
    let amount = limit.get("amount")?;
    let fraction = if let Some(used_fraction) = amount.get("usedFraction").and_then(Value::as_f64) {
        used_fraction
    } else if let (Some(used), Some(max)) = (
        amount.get("used").and_then(Value::as_f64),
        amount
            .get("limit")
            .and_then(Value::as_f64)
            .filter(|max| *max > 0.0),
    ) {
        used / max
    } else if amount.get("unit").and_then(Value::as_str) == Some("percent")
        && let Some(used) = amount.get("used").and_then(Value::as_f64)
    {
        used / 100.0
    } else if let Some(remaining_fraction) = amount.get("remainingFraction").and_then(Value::as_f64)
    {
        (1.0 - remaining_fraction).max(0.0)
    } else {
        return None;
    };
    fraction
        .is_finite()
        .then(|| (fraction * 100.0).clamp(0.0, 100.0))
}

fn ohmypi_limit_label(upstream_lower: &str, limit: &Value) -> String {
    let id = limit.get("id").and_then(Value::as_str).unwrap_or("");
    if upstream_lower == "anthropic" && id == "anthropic:7d" {
        return tr!("usage.weekly_all_models");
    }

    let period_base = ohmypi_period_label(limit.get("window"));
    let raw_label = limit
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let window_label = limit
        .pointer("/window/label")
        .and_then(Value::as_str)
        .map(str::trim);
    let scope_name = extract_ohmypi_limit_scope(upstream_lower, raw_label, window_label);

    match (period_base, scope_name) {
        (Some(base), Some(name)) => {
            let suffix = tr!("usage.limit_suffix");
            let period = base.strip_suffix(&suffix).unwrap_or(&base);
            tr!("usage.scoped_limit", period = period, name = name)
        }
        (Some(base), None) => base,
        (None, Some(name)) => name,
        (None, None) if !raw_label.is_empty() => raw_label.to_owned(),
        (None, None) => tr!("usage.usage_limit"),
    }
}

fn ohmypi_period_label(window: Option<&Value>) -> Option<String> {
    let window = window?;
    if let Some(duration_ms) = window
        .get("durationMs")
        .and_then(Value::as_i64)
        .filter(|ms| *ms > 0)
    {
        let minutes = (duration_ms + 59_999) / 60_000;
        if minutes == 24 * 60 {
            return Some(tr!("usage.daily_limit"));
        }
        if (28 * 24 * 60..=31 * 24 * 60).contains(&minutes) {
            return Some(tr!("usage.monthly_limit"));
        }
        return Some(window_label_from_minutes(Some(minutes)));
    }
    let token = window
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| window.get("label").and_then(Value::as_str))?
        .trim()
        .to_ascii_lowercase();
    match token.as_str() {
        "5h" | "5 hour" | "5 hours" | "5-hour" => Some(tr!("usage.hour_limit", count = 5)),
        "daily" | "24h" | "1d" | "1 day" => Some(tr!("usage.daily_limit")),
        "weekly" | "7d" | "7 day" | "7 days" => Some(tr!("usage.weekly_limit")),
        "monthly" | "30d" | "30 days" => Some(tr!("usage.monthly_limit")),
        _ => None,
    }
}

/// Separate a bucket/model scope name from a generic window title:
/// - `"5 hours"` / `"Claude 5 Hour"` / `"Quota window"` → `None` (pure window)
/// - `"7 days (GPT-5.3-Codex-Spark)"` / `"Claude 7 Day (Fable)"` / `"Usage (Google)"`
///   → `Some("GPT-5.3-Codex-Spark")` / `Some("Fable")` / `Some("Google")`
/// - `"Gemini (Argon)"` → `Some("Gemini (Argon)")`
/// - `"Claude & GPT (shared)"` → `Some("Claude & GPT")`
fn extract_ohmypi_limit_scope(
    upstream_lower: &str,
    raw_label: &str,
    window_label: Option<&str>,
) -> Option<String> {
    if raw_label.is_empty() {
        return None;
    }
    if window_label.is_some_and(|win| win.eq_ignore_ascii_case(raw_label)) {
        return None;
    }
    let cleaned = raw_label
        .strip_suffix(" (shared)")
        .or_else(|| raw_label.strip_suffix(" (Shared)"))
        .unwrap_or(raw_label)
        .trim();
    if is_generic_ohmypi_window_phrase(upstream_lower, cleaned, window_label) {
        return None;
    }
    if let Some((prefix, rest)) = cleaned.split_once('(')
        && let Some(inner) = rest.strip_suffix(')')
        && is_generic_ohmypi_window_phrase(upstream_lower, prefix.trim(), window_label)
    {
        let inner = inner.trim();
        return (!inner.is_empty()).then(|| inner.to_owned());
    }
    Some(cleaned.to_owned())
}

fn is_generic_ohmypi_window_phrase(
    upstream_lower: &str,
    phrase: &str,
    window_label: Option<&str>,
) -> bool {
    if window_label.is_some_and(|win| win.eq_ignore_ascii_case(phrase)) {
        return true;
    }
    let mut normalized = phrase.trim().to_ascii_lowercase();
    if upstream_lower == "anthropic"
        && let Some(rest) = normalized.strip_prefix("claude ")
    {
        normalized = rest.trim().to_owned();
    }
    matches!(
        normalized.as_str(),
        "usage"
            | "quota"
            | "quota window"
            | "primary window"
            | "secondary window"
            | "5h"
            | "5 hour"
            | "5 hours"
            | "5-hour"
            | "24h"
            | "1d"
            | "1 day"
            | "daily"
            | "7d"
            | "7 day"
            | "7 days"
            | "weekly"
            | "30d"
            | "30 days"
            | "monthly"
    )
}

fn ohmypi_plan_label(upstream_lower: &str, report: &Value) -> Option<String> {
    let upstream_name = match upstream_lower {
        "openai-codex" | "openai-codex-device" => "Codex".to_owned(),
        "google-antigravity" => "Antigravity".to_owned(),
        "anthropic" => "Claude".to_owned(),
        "google-gemini-cli" => "Gemini CLI".to_owned(),
        "github-copilot" => "GitHub Copilot".to_owned(),
        "kimi-code" => "Kimi Code".to_owned(),
        "opencode-go" => "OpenCode Go".to_owned(),
        "xai-oauth" => "Grok".to_owned(),
        "zai" | "zai-coding-plan" => "Z.AI".to_owned(),
        other => crate::model_catalog::display_name_from_slug(other),
    };
    let metadata = report.get("metadata");
    let meta_str = |key: &str| {
        metadata
            .and_then(|meta| meta.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let tier = match upstream_lower {
        "openai-codex" | "openai-codex-device" => meta_str("planType").and_then(|raw| {
            let normalized = raw.to_ascii_lowercase().replace([' ', '-'], "_");
            let stripped = normalized
                .strip_prefix("chatgpt_")
                .unwrap_or(normalized.as_str());
            openai_plan_label(Some(stripped))
        }),
        "anthropic" => plan_label(
            meta_str("subscriptionType").or_else(|| meta_str("planType")),
            meta_str("rateLimitTier"),
        ),
        _ => meta_str("planType")
            .or_else(|| meta_str("plan"))
            .or_else(|| meta_str("currentTierName"))
            .or_else(|| meta_str("subscriptionTier"))
            .map(|tier| {
                if tier.chars().all(|ch| !ch.is_ascii_uppercase()) {
                    crate::model_catalog::display_name_from_slug(tier)
                } else {
                    tier.to_owned()
                }
            }),
    };
    match tier {
        Some(tier) if !tier.eq_ignore_ascii_case(&upstream_name) => {
            Some(format!("{upstream_name} · {tier}"))
        }
        _ => Some(upstream_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a live response captured on 2026-08-07.
    const LIVE_BODY: &str = r#"{
        "five_hour": {"utilization": 41.0, "resets_at": "2026-08-07T14:59:59.729061+00:00"},
        "seven_day": {"utilization": 20.0, "resets_at": "2026-08-13T11:59:59.729091+00:00"},
        "seven_day_opus": null,
        "limits": [
            {"kind": "session", "group": "session", "percent": 41, "severity": "normal",
             "resets_at": "2026-08-07T14:59:59.729061+00:00", "scope": null, "is_active": true},
            {"kind": "weekly_all", "group": "weekly", "percent": 20, "severity": "normal",
             "resets_at": "2026-08-13T11:59:59.729091+00:00", "scope": null, "is_active": false},
            {"kind": "weekly_scoped", "group": "weekly", "percent": 38, "severity": "normal",
             "resets_at": "2026-08-13T11:59:59.729307+00:00",
             "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null},
             "is_active": false}
        ]
    }"#;

    fn credentials(tier: Option<&str>, subscription: Option<&str>) -> OauthCredentials {
        OauthCredentials {
            access_token: "token".into(),
            subscription_type: subscription.map(str::to_owned),
            rate_limit_tier: tier.map(str::to_owned),
        }
    }

    #[test]
    fn parses_the_limits_array_into_the_three_quota_rows() {
        let body: Value = serde_json::from_str(LIVE_BODY).unwrap();
        let usage = parse_plan_usage(
            &body,
            &credentials(Some("default_claude_max_5x"), Some("max")),
        );
        assert_eq!(usage.plan_label.as_deref(), Some("Max (5x)"));
        let rows = usage
            .windows
            .iter()
            .map(|window| (window.label.as_str(), window.percent))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                ("5-hour limit", 41.0),
                ("Weekly · all models", 20.0),
                ("Weekly · Fable", 38.0),
            ]
        );
        assert!(
            usage
                .windows
                .iter()
                .all(|window| window.resets_at.is_some())
        );
    }

    #[test]
    fn falls_back_to_flat_fields_when_the_limits_array_is_missing() {
        let body: Value = serde_json::from_str(
            r#"{
                "five_hour": {"utilization": 12.5, "resets_at": "2026-08-07T14:59:59+00:00"},
                "seven_day": {"utilization": 3.0, "resets_at": "2026-08-13T11:59:59+00:00"},
                "seven_day_opus": {"utilization": 7.0, "resets_at": "2026-08-13T11:59:59+00:00"}
            }"#,
        )
        .unwrap();
        let usage = parse_plan_usage(&body, &credentials(None, Some("pro")));
        assert_eq!(usage.plan_label.as_deref(), Some("Pro"));
        assert_eq!(
            usage
                .windows
                .iter()
                .map(|window| window.label.as_str())
                .collect::<Vec<_>>(),
            ["5-hour limit", "Weekly · all models", "Weekly · Opus"]
        );
    }

    #[test]
    fn the_profile_organization_names_the_live_plan() {
        // Shape captured live on 2026-08-07: the keychain still said 5x while
        // the profile reported the account's actual 20x tier.
        let body: Value = serde_json::from_str(
            r#"{
                "account": {"has_claude_max": true},
                "organization": {
                    "organization_type": "claude_max",
                    "billing_type": "stripe_subscription",
                    "rate_limit_tier": "default_claude_max_20x"
                }
            }"#,
        )
        .unwrap();
        assert_eq!(profile_plan_label(&body).as_deref(), Some("Max (20x)"));
        assert_eq!(profile_plan_label(&serde_json::json!({})), None);
    }

    #[test]
    fn plan_labels_cover_tier_multipliers_and_missing_metadata() {
        assert_eq!(
            plan_label(Some("max"), Some("default_claude_max_20x")).as_deref(),
            Some("Max (20x)")
        );
        // The tier alone still names the plan.
        assert_eq!(
            plan_label(None, Some("default_claude_max_5x")).as_deref(),
            Some("Max (5x)")
        );
        assert_eq!(plan_label(Some("pro"), None).as_deref(), Some("Pro"));
        assert_eq!(plan_label(None, None), None);
    }

    #[test]
    fn credentials_parse_reads_the_keychain_blob_shape() {
        let parsed = parse_credentials(
            r#"{"claudeAiOauth": {"accessToken": "sk-ant-oat01-abc",
                "subscriptionType": "max", "rateLimitTier": "default_claude_max_5x"}}"#,
        )
        .unwrap();
        assert_eq!(parsed.access_token, "sk-ant-oat01-abc");
        assert_eq!(parsed.subscription_type.as_deref(), Some("max"));
        assert!(parse_credentials(r#"{"mcpOAuth": {}}"#).is_err());
    }

    #[test]
    fn status_line_and_body_split_from_curl_header_dump() {
        let (status, body) =
            split_status_and_body("HTTP/2 200 \r\ncontent-type: application/json\r\n\r\n{\"a\":1}")
                .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"a\":1}");
    }

    #[test]
    fn codex_usage_endpoint_maps_windows_and_plan() {
        // Shape mirrors the ChatGPT backend response CodexBar decodes.
        let body: Value = serde_json::from_str(
            r#"{
                "plan_type": "plus",
                "rate_limit": {
                    "primary_window":
                        {"used_percent": 37, "reset_at": 1800000000, "limit_window_seconds": 18000},
                    "secondary_window":
                        {"used_percent": 8, "reset_at": 1800500000, "limit_window_seconds": 604800}
                }
            }"#,
        )
        .unwrap();
        let usage = parse_codex_plan_usage(&body).expect("both windows should map");
        assert_eq!(usage.plan_label.as_deref(), Some("Plus"));
        assert_eq!(
            usage
                .windows
                .iter()
                .map(|window| (window.label.as_str(), window.percent, window.resets_at))
                .collect::<Vec<_>>(),
            [
                ("5-hour limit", 37.0, Some(1_800_000_000)),
                ("Weekly limit", 8.0, Some(1_800_500_000)),
            ]
        );
        assert!(parse_codex_plan_usage(&serde_json::json!({"plan_type": "plus"})).is_none());
    }

    #[test]
    fn codex_model_scoped_limits_become_named_rows_except_spark() {
        // Shape captured live on 2026-08-07: a Pro account with a weekly
        // account lane plus model-scoped weekly lanes. The Spark bonus lane
        // is dropped; other scoped lanes keep their named rows.
        let body: Value = serde_json::from_str(
            r#"{
                "plan_type": "pro",
                "rate_limit": {
                    "primary_window":
                        {"used_percent": 99, "limit_window_seconds": 604800, "reset_at": 1786160310},
                    "secondary_window": null
                },
                "additional_rate_limits": [{
                    "limit_name": "GPT-5.3-Codex-Spark",
                    "rate_limit": {
                        "primary_window":
                            {"used_percent": 0, "limit_window_seconds": 604800, "reset_at": 1786720969}
                    }
                }, {
                    "limit_name": "GPT-5.3-Codex",
                    "rate_limit": {
                        "primary_window":
                            {"used_percent": 12, "limit_window_seconds": 604800, "reset_at": 1786720969}
                    }
                }]
            }"#,
        )
        .unwrap();
        let usage = parse_codex_plan_usage(&body).unwrap();
        assert_eq!(usage.plan_label.as_deref(), Some("Pro (20x)"));
        assert_eq!(
            usage
                .windows
                .iter()
                .map(|window| (window.label.as_str(), window.percent))
                .collect::<Vec<_>>(),
            [("Weekly limit", 99.0), ("Weekly · GPT-5.3-Codex", 12.0),]
        );
    }

    /// The verbatim 403 body opencode.ai/zen/go/v1/usage returns for a valid
    /// key on an account without the subscription. Reporting this as a
    /// rejected key sent the user to reconnect, which cannot fix an
    /// entitlement.
    #[test]
    fn usage_error_detail_reads_the_endpoints_own_sentence() {
        let body = r#"{"type":"error","error":{"type":"EntitlementError","message":"OpenCode Go subscription required."}}"#;
        assert_eq!(
            usage_error_detail(body).as_deref(),
            Some("OpenCode Go subscription required.")
        );
    }

    #[test]
    fn usage_error_detail_ignores_a_bodyless_or_shapeless_error() {
        assert_eq!(usage_error_detail(""), None);
        assert_eq!(usage_error_detail("not json"), None);
        assert_eq!(usage_error_detail(r#"{"error":{"type":"X"}}"#), None);
        assert_eq!(usage_error_detail(r#"{"error":{"message":"   "}}"#), None);
    }

    #[test]
    fn opencode_go_usage_maps_live_subscription_windows() {
        // Exact response shape captured from the live Go endpoint on
        // 2026-08-12. Percentages vary by account; the envelope and reset
        // timestamps are the contract this parser relies on.
        let body: Value = serde_json::from_str(
            r#"{
                "usage": {
                    "rolling": {
                        "status": "ok",
                        "percent": 0,
                        "resetsAt": "2026-08-12T05:52:15.153Z"
                    },
                    "weekly": {
                        "status": "ok",
                        "percent": 8,
                        "resetsAt": "2026-08-17T00:00:00.153Z"
                    },
                    "monthly": {
                        "status": "ok",
                        "percent": 36,
                        "resetsAt": "2026-09-01T13:40:30.153Z"
                    }
                }
            }"#,
        )
        .unwrap();
        let usage = parse_opencode_go_plan_usage(&body).expect("all Go windows should map");
        assert_eq!(usage.plan_label.as_deref(), Some("Go"));
        assert_eq!(
            usage
                .windows
                .iter()
                .map(|window| (window.label.as_str(), window.percent))
                .collect::<Vec<_>>(),
            [
                ("5-hour limit", 0.0),
                ("Weekly limit", 8.0),
                ("Monthly limit", 36.0),
            ]
        );
        assert!(
            usage
                .windows
                .iter()
                .all(|window| window.resets_at.is_some())
        );
        assert!(parse_opencode_go_plan_usage(&serde_json::json!({"usage": {}})).is_none());
    }

    #[test]
    fn opencode_credential_store_answers_for_the_go_integration_only() {
        let directory =
            std::env::temp_dir().join(format!("waku-opencode-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("opencode.db");

        // No database at all: nothing to say, so `auth.json` gets to answer.
        assert_eq!(opencode_go_api_key_from_store(&database), None);

        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE kv (key text PRIMARY KEY, value text NOT NULL);")
            .unwrap();
        // A database from before the credential store is not authoritative.
        assert_eq!(opencode_go_api_key_from_store(&database), None);

        // The exact table OpenCode creates.
        connection
            .execute_batch(
                "CREATE TABLE credential (
                    id text PRIMARY KEY,
                    integration_id text,
                    label text NOT NULL,
                    value text NOT NULL,
                    connector_id text,
                    method_id text,
                    active integer,
                    time_created integer NOT NULL,
                    time_updated integer NOT NULL
                );",
            )
            .unwrap();
        // A store without a Go key is authoritative: no stale fallback.
        assert_eq!(opencode_go_api_key_from_store(&database), Some(None));

        let insert = |id: &str,
                      integration: &str,
                      value: &str,
                      active: Option<i64>,
                      updated: i64| {
            connection
                .execute(
                    "INSERT INTO credential (id, integration_id, label, value, active, time_created, time_updated)
                     VALUES (?1, ?2, 'API key', ?3, ?4, 0, ?5)",
                    rusqlite::params![id, integration, value, active, updated],
                )
                .unwrap();
        };
        insert(
            "crd_zen",
            "opencode",
            r#"{"type":"key","key":"zen-key"}"#,
            None,
            9,
        );
        insert(
            "crd_old",
            "opencode-go",
            r#"{"type":"key","key":"old-key"}"#,
            None,
            1,
        );
        insert(
            "crd_new",
            "opencode-go",
            r#"{"type":"key","key":" new-key "}"#,
            None,
            2,
        );
        assert_eq!(
            opencode_go_api_key_from_store(&database),
            Some(Some("new-key".to_owned())),
            "the newest key wins when none is marked active"
        );
        insert(
            "crd_active",
            "opencode-go",
            r#"{"type":"key","key":"active-key"}"#,
            Some(1),
            0,
        );
        assert_eq!(
            opencode_go_api_key_from_store(&database),
            Some(Some("active-key".to_owned()))
        );

        drop(connection);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn opencode_auth_uses_only_the_go_provider_entry() {
        let auth = serde_json::json!({
            "opencode": {"type": "api", "key": "zen-key"},
            "opencode-go": {"type": "api", "key": " go-key "}
        });
        assert_eq!(
            opencode_go_api_key_from_auth(Some(&auth)).as_deref(),
            Some("go-key")
        );
        assert_eq!(
            opencode_go_api_key_from_auth(Some(&serde_json::json!({
                "opencode": {"type": "api", "key": "zen-key"}
            }))),
            None
        );
        assert_eq!(
            opencode_go_api_key_from_auth(Some(&serde_json::json!({
                "opencode-go": {"type": "oauth", "key": "wrong-kind"}
            }))),
            None
        );
    }

    #[test]
    fn token_counts_format_like_the_cli_meter() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(87_650), "87.7k");
        assert_eq!(format_tokens(999_600), "1.0M");
        assert_eq!(format_tokens(1_000_000), "1.0M");
    }

    #[test]
    fn reset_labels_stay_relative_until_a_day_out() {
        let now = 1_700_000_000;
        assert_eq!(reset_label(now + 49 * 60, now), "Resets in 49 min");
        assert_eq!(
            reset_label(now + 3 * 3600 + 120, now),
            "Resets in 3 hr 2 min"
        );
        assert_eq!(reset_label(now - 5, now), "Resets soon");
        // Beyond a day the label goes absolute in local time; the exact text
        // depends on the machine's zone, so assert only the shape.
        let far = reset_label(now + 3 * 24 * 3600, now);
        assert!(far.starts_with("Resets ") && !far.contains(" in "), "{far}");
    }

    /// Trimmed from a live `omp usage --json` response covering both
    /// `openai-codex` and `google-antigravity` OAuth accounts.
    const LIVE_OMP_USAGE_BODY: &str = r#"{
        "generatedAt": 1793521132516,
        "reports": [
            {
                "provider": "openai-codex",
                "fetchedAt": 1793521131808,
                "limits": [
                    {
                        "id": "openai-codex:primary",
                        "label": "5 hours",
                        "scope": {"provider": "openai-codex", "windowId": "5h", "shared": true},
                        "window": {"id": "5h", "label": "5 hours", "durationMs": 18000000, "resetsAt": 1793522184000},
                        "amount": {"used": 99, "limit": 100, "remaining": 1, "usedFraction": 0.99, "remainingFraction": 0.01, "unit": "percent"},
                        "status": "warning"
                    },
                    {
                        "id": "openai-codex:secondary",
                        "label": "7 days",
                        "scope": {"provider": "openai-codex", "windowId": "7d", "shared": true},
                        "window": {"id": "7d", "label": "7 days", "durationMs": 604800000, "resetsAt": 1794108984000},
                        "amount": {"used": 16, "limit": 100, "remaining": 84, "usedFraction": 0.16, "remainingFraction": 0.84, "unit": "percent"},
                        "status": "ok"
                    },
                    {
                        "id": "openai-codex:spark:primary",
                        "label": "5 hours (GPT-5.3-Codex-Spark)",
                        "scope": {"provider": "openai-codex", "windowId": "5h"},
                        "window": {"id": "5h", "label": "5 hours", "durationMs": 18000000, "resetsAt": 1793522184000},
                        "amount": {"used": 5, "limit": 100, "usedFraction": 0.05, "unit": "percent"},
                        "status": "ok"
                    }
                ],
                "metadata": {
                    "planType": "plus",
                    "allowed": true,
                    "email": "dev@example.com",
                    "accountId": "acct-plus"
                }
            },
            {
                "provider": "google-antigravity",
                "fetchedAt": 1793521131809,
                "limits": [
                    {
                        "id": "google-antigravity:google:default:gemini-argon-wtu-weekly",
                        "label": "Gemini (Argon)",
                        "scope": {"provider": "google-antigravity", "projectId": "proj-1", "windowId": "weekly"},
                        "window": {"id": "weekly", "label": "Weekly", "durationMs": 604800000, "resetsAt": 1794124606000},
                        "amount": {"unit": "percent", "remainingFraction": 0.85, "usedFraction": 0.15, "used": 15, "limit": 100},
                        "status": "ok"
                    },
                    {
                        "id": "google-antigravity:google:default:gemini-5h",
                        "label": "Gemini",
                        "scope": {"provider": "google-antigravity", "projectId": "proj-1", "windowId": "5h"},
                        "window": {"id": "5h", "label": "5 Hour", "durationMs": 18000000, "resetsAt": 1793538784000},
                        "amount": {"unit": "percent", "remainingFraction": 0.98, "usedFraction": 0.02, "used": 2, "limit": 100},
                        "status": "ok"
                    },
                    {
                        "id": "google-antigravity:google:default:gemini-weekly",
                        "label": "Gemini",
                        "scope": {"provider": "google-antigravity", "projectId": "proj-1", "windowId": "weekly"},
                        "window": {"id": "weekly", "label": "Weekly", "durationMs": 604800000, "resetsAt": 1794125584000},
                        "amount": {"unit": "percent", "remainingFraction": 0.99, "usedFraction": 0.01, "used": 1, "limit": 100},
                        "status": "ok"
                    },
                    {
                        "id": "google-antigravity:anthropic:default:3p-weekly",
                        "label": "Claude & GPT (shared)",
                        "scope": {"provider": "google-antigravity", "projectId": "proj-1", "windowId": "weekly", "shared": true, "sharedGroup": "3p-weekly:weekly"},
                        "window": {"id": "weekly", "label": "Weekly", "durationMs": 604800000, "resetsAt": 1794125932000},
                        "amount": {"unit": "percent", "remainingFraction": 0.6, "usedFraction": 0.4, "used": 40, "limit": 100},
                        "status": "ok"
                    },
                    {
                        "id": "google-antigravity:openai:default:3p-weekly",
                        "label": "Claude & GPT (shared)",
                        "scope": {"provider": "google-antigravity", "projectId": "proj-1", "windowId": "weekly", "shared": true, "sharedGroup": "3p-weekly:weekly"},
                        "window": {"id": "weekly", "label": "Weekly", "durationMs": 604800000, "resetsAt": 1794125932000},
                        "amount": {"unit": "percent", "remainingFraction": 0.6, "usedFraction": 0.4, "used": 40, "limit": 100},
                        "status": "ok"
                    },
                    {
                        "id": "google-antigravity:anthropic:default:3p-5h",
                        "label": "Claude & GPT (shared)",
                        "scope": {"provider": "google-antigravity", "projectId": "proj-1", "windowId": "5h", "shared": true, "sharedGroup": "3p-5h:5h"},
                        "window": {"id": "5h", "label": "5 Hour", "durationMs": 18000000, "resetsAt": 1793539132000},
                        "amount": {"unit": "percent", "remainingFraction": 0.9, "usedFraction": 0.1, "used": 10, "limit": 100},
                        "status": "ok"
                    },
                    {
                        "id": "google-antigravity:openai:default:3p-5h",
                        "label": "Claude & GPT (shared)",
                        "scope": {"provider": "google-antigravity", "projectId": "proj-1", "windowId": "5h", "shared": true, "sharedGroup": "3p-5h:5h"},
                        "window": {"id": "5h", "label": "5 Hour", "durationMs": 18000000, "resetsAt": 1793539132000},
                        "amount": {"unit": "percent", "remainingFraction": 0.9, "usedFraction": 0.1, "used": 10, "limit": 100},
                        "status": "ok"
                    }
                ],
                "metadata": {
                    "endpoint": "https://daily-cloudcode-pa.googleapis.com",
                    "projectId": "proj-1",
                    "email": "dev@example.com"
                }
            }
        ],
        "accountsWithoutUsage": [],
        "disabledCredentials": []
    }"#;

    #[test]
    fn ohmypi_codex_report_maps_plan_windows_and_hides_spark_by_default() {
        let payload: Value = serde_json::from_str(LIVE_OMP_USAGE_BODY).unwrap();
        let usage = parse_ohmypi_plan_usage(&payload, "openai-codex", "gpt-5.3-codex", None)
            .unwrap()
            .expect("codex windows should map");
        assert_eq!(usage.plan_label.as_deref(), Some("Codex · Plus"));
        assert_eq!(
            usage
                .windows
                .iter()
                .map(|w| (w.label.as_str(), w.percent, w.resets_at))
                .collect::<Vec<_>>(),
            vec![
                ("5-hour limit", 99.0, Some(1_793_522_184)),
                ("Weekly limit", 16.0, Some(1_794_108_984)),
            ]
        );

        let spark = parse_ohmypi_plan_usage(&payload, "openai-codex", "gpt-5.3-codex-spark", None)
            .unwrap()
            .expect("spark model should surface its dedicated meter");
        assert_eq!(spark.windows.len(), 1);
        assert_eq!(spark.windows[0].label, "5-hour · GPT-5.3-Codex-Spark");
        assert_eq!(spark.windows[0].percent, 5.0);
    }

    #[test]
    fn ohmypi_antigravity_scopes_by_model_family_and_deduplicates_shared_groups() {
        let payload: Value = serde_json::from_str(LIVE_OMP_USAGE_BODY).unwrap();

        let argon =
            parse_ohmypi_plan_usage(&payload, "google-antigravity", "gemini-4-argon", None)
                .unwrap()
                .expect("argon model should map google counter buckets");
        assert_eq!(argon.plan_label.as_deref(), Some("Antigravity"));
        assert_eq!(
            argon
                .windows
                .iter()
                .map(|w| (w.label.as_str(), w.percent))
                .collect::<Vec<_>>(),
            vec![
                ("5-hour · Gemini", 2.0),
                ("Weekly · Gemini (Argon)", 15.0),
                ("Weekly · Gemini", 1.0),
            ]
        );

        // Both Claude and GPT-OSS models on Antigravity share the third-party
        // pool; neither should duplicate rows across anthropic/openai copies.
        for third_party_model in ["claude-sonnet-4-6", "gpt-oss-120b"] {
            let usage =
                parse_ohmypi_plan_usage(&payload, "google-antigravity", third_party_model, None)
                    .unwrap()
                    .expect("third-party antigravity pool should map");
            assert_eq!(
                usage
                    .windows
                    .iter()
                    .map(|w| (w.label.as_str(), w.percent))
                    .collect::<Vec<_>>(),
                vec![
                    ("5-hour · Claude & GPT", 10.0),
                    ("Weekly · Claude & GPT", 40.0),
                ],
                "unexpected rows for {third_party_model}"
            );
        }
    }

    #[test]
    fn ohmypi_credential_pin_selects_the_sessions_pinned_account() {
        let payload = serde_json::json!({
            "reports": [
                {
                    "provider": "openai-codex",
                    "limits": [{
                        "id": "openai-codex:primary",
                        "label": "5 hours",
                        "window": {"id": "5h", "label": "5 hours", "durationMs": 18_000_000},
                        "amount": {"used": 12, "limit": 100, "usedFraction": 0.12, "unit": "percent"}
                    }],
                    "metadata": {"planType": "plus", "accountId": "acct-1", "email": "first@example.com", "orgId": "org-1"}
                },
                {
                    "provider": "openai-codex",
                    "limits": [{
                        "id": "openai-codex:primary",
                        "label": "5 hours",
                        "window": {"id": "5h", "label": "5 hours", "durationMs": 18_000_000},
                        "amount": {"used": 64, "limit": 100, "usedFraction": 0.64, "unit": "percent"}
                    }],
                    "metadata": {"planType": "pro", "accountId": "acct-2", "email": "second@example.com", "orgId": "org-2"}
                }
            ]
        });
        let second_pin =
            ohmypi_credential_pin_hash("openai-codex", &payload["reports"][1]).unwrap();

        let temp = std::env::temp_dir().join(format!(
            "waku-omp-pin-{}.jsonl",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(
            &temp,
            format!(
                "{{\"type\":\"session\",\"id\":\"s1\",\"cwd\":\"/tmp\"}}\n\
                 {{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null}}\n\
                 {{\"type\":\"credential_pin\",\"id\":\"p1\",\"parentId\":\"m1\",\"provider\":\"openai-codex\",\"hash\":\"{second_pin}\"}}\n"
            ),
        )
        .unwrap();
        let resolved_pin = read_ohmypi_credential_pin(&temp, "openai-codex");
        let _ = std::fs::remove_file(&temp);
        assert_eq!(resolved_pin.as_deref(), Some(second_pin.as_str()));

        let usage =
            parse_ohmypi_plan_usage(&payload, "openai-codex", "gpt-5.3-codex", resolved_pin.as_deref())
                .unwrap()
                .expect("pinned account should resolve");
        assert_eq!(usage.plan_label.as_deref(), Some("Codex · Pro (20x)"));
        assert_eq!(usage.windows[0].percent, 64.0);
    }

    #[test]
    fn ohmypi_unmetered_upstream_returns_none_while_disabled_credential_errors() {
        let payload = serde_json::json!({
            "reports": [],
            "disabledCredentials": [{
                "provider": "anthropic",
                "reason": "OAuth refresh token expired; run /login anthropic"
            }]
        });
        assert!(
            parse_ohmypi_plan_usage(&payload, "openrouter", "deepseek/deepseek-v4", None)
                .unwrap()
                .is_none()
        );
        let err = parse_ohmypi_plan_usage(&payload, "anthropic", "claude-sonnet-4-6", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("OAuth refresh token expired"), "{err}");
    }
}
