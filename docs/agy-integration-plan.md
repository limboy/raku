# Antigravity CLI (`agy`) Integration Plan

Detailed plan for adding Google Antigravity CLI (`agy`) as a first-class local coding-agent provider in Raku, alongside Claude Code, Codex CLI, and Pi.

---

## 1. Executive Summary

- **Feasibility**: High. `agy` (verified locally at `v1.5.18`) ships a native **Agent Client Protocol (ACP)** stdio JSON-RPC server (`agy acp`) that maps directly onto Raku's long-lived `DriverControl` / `DriverEvent` architecture.
- **Recommended Transport**: `agy acp` (stdio JSON-RPC) rather than `--input-format stream-json --output-format stream-json`.
  - Bidirectional JSON-RPC over stdio (same process model as `codex app-server --stdio`).
  - Native support for `initialize`, `session/new`, `session/resume`, `session/load`, `session/list`, `session/close`, `session/prompt`, `session/cancel`, `session/set_config_option`, and `session/request_permission`.
  - Advertises `_meta.antigravity.systemPrompt.append: true`, allowing clean injection of `USER_RUN_INSTRUCTIONS`, `SIDE_CHAT_INSTRUCTIONS`, and `SUBAGENT_INSTRUCTIONS`.
  - Advertises `promptCapabilities.image: true` and `embeddedContext: true`.
  - Streams `available_commands_update` (feeding `DriverEvent::AvailableCommands`) and `usage_update` (`used` / `size` context occupancy).
  - Advertises `authMethods`, so an unauthenticated install is a first-class state Raku must handle (§2.1, Phase 2).
- **Not free with the variant**: every existing provider honors `read_only` (side chat) and `subagent.child`, and the side-chat and subagent pickers list every installed provider in `ProviderKind::ALL`. Adding the variant exposes Antigravity there immediately, so the MVP must either implement those policies or gate them with new capabilities (§3).
- **Provider Identity**:
  - Enum variant: `ProviderKind::Antigravity`
  - `id()`: `"agy"`
  - `command()`: `"agy"`
  - `display_name()`: `"Antigravity"`
  - `short_name()`: `"Agy"`

---

## 2. Verified `agy acp` Protocol Surface (v1.5.18)

The following payloads were verified directly against the installed `agy acp` binary. Items marked **unverified** are Phase 0 probes.

### 2.1 `initialize` Handshake

**Request**:
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "protocolVersion": 1,
    "clientCapabilities": {
      "fs": { "readTextFile": false, "writeTextFile": false },
      "terminal": false
    },
    "clientInfo": { "name": "raku", "version": "<CARGO_PKG_VERSION>" }
  }
}
```

**Observed Response Highlights**:
- `protocolVersion`: `1`
- `agentInfo`: `{ "name": "Antigravity ACP Server", "version": "1.5.18" }`
- `agentCapabilities.loadSession`: `true`
- `agentCapabilities.promptCapabilities`: `{ "image": true, "audio": false, "embeddedContext": true }`
- `agentCapabilities.sessionCapabilities`: `{ "resume": {}, "close": {}, "load": {}, "list": {}, "additionalDirectories": {} }`
- `agentCapabilities.auth`: `{ "logout": {} }`
- `agentCapabilities._meta.antigravity`: `{ "tier": true, "systemPrompt": { "append": true } }`
- `authMethods`: `oauth-personal` (Log in with Google), `oauth-business` (Gemini Enterprise), `gemini-api-key`, `agent-platform` (Vertex AI / ADC).
- No `mcpCapabilities` (no HTTP/SSE MCP). ACP makes stdio MCP servers baseline, so `mcpServers` with stdio entries should work but is **unverified**.
- The response duplicates several keys in snake_case (`load_session`, `session_capabilities`). Parse the camelCase keys only.

### 2.2 `session/new` Response Shape

**Request**:
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "session/new",
  "params": {
    "cwd": "/path/to/workspace",
    "mcpServers": [],
    "_meta": {
      "antigravity": {
        "systemPrompt": { "append": "<Raku system instructions>" }
      }
    }
  }
}
```

**Observed Response Highlights**:
- `sessionId`: UUID string (stored in `ProviderResumeCursor::Antigravity { session_id }`)
- `modes.availableModes` & `configOptions[id="mode"]`:
  - `default` — *"Ask before edits and commands, per your permission settings."*
  - `accept-edits` — *"Approve file edits in the workspace without asking."*
  - `auto` — *"Approve edits and safe commands; ask before irreversible actions."*
  - `plan` — *"Plan carefully before executing a task."*
- `configOptions[id="model"]` (`category: "model"`): clean base model IDs (e.g. `gemini-4-neon`, `gemini-4-carbon`, `gemini-3.8-flash`, `claude-opus-5-5`, `claude-sonnet-5-5`, `gpt-oss-120b-medium`).
- `configOptions[id="thought_level"]` (`category: "thought_level"`): `low`, `medium`, `high`, `xhigh`. This is a single session-wide option, while the supported set differs per model (below).
- `models.availableModels`: full `(modelId, name)` Cartesian list (e.g. `gemini-4-neon-high`, `gemini-4-neon-xhigh`, `gemini-3.8-flash-medium`, `gemini-pro-orchestrator`).
- Immediate `session/update` notifications emitted right after `session/new`:
  - `sessionUpdate: "available_commands_update"` → `availableCommands: [{ name, description, ... }]`
  - `sessionUpdate: "usage_update"` → `{ used: 0, size: 1048576 }`

### 2.3 Sessionless CLI Surface

- `agy models` lists the account's models without opening a session (`Fetching available models...` on the first line, then `<id>\t<name>` rows, about 1 s). Observed effort sets differ per model: Gemini 4 Neon and Hydrogen have `low`–`xhigh`, Carbon, Argon, Gemini 3.8 Flash and the Claude models have `low`–`high`, `gpt-oss-120b-medium` has none, and `gemini-pro-orchestrator{,-neon,-argon}` suffixes are tiers, not efforts.
- Print mode: `-p`, `--output-format text|json|stream-json`, `--disable-slash-commands`, `--mode accept-edits|auto|plan` (no `default`), `--model`, `--effort low|medium|high|xhigh|max`, `--print-timeout`, `--settings <json|path>`, `--sandbox`. There is no equivalent of Pi's `--no-session` / `--no-tools`.
- `agy acp` flags of interest: `--dangerously-skip-permissions`, `--sandbox` ("terminal restrictions"), `--disable-telemetry`, `--login` (interactive Google login, then exit), `--process-per-session`, `--add-dir`. `--stdio` is the default transport.

### 2.4 Mapping to Raku Concepts

#### Access Modes & Plan Mode

| Raku Setting | `agy acp` Launch Flag | ACP `mode` Config Option | Permission requests |
| --- | --- | --- | --- |
| `RuntimeMode::Ask` | *(none)* | `"default"` | Routed to `DriverEvent::Permission` |
| `RuntimeMode::AutoAcceptEdits` | *(none)* | `"accept-edits"` | Routed to `DriverEvent::Permission` |
| `RuntimeMode::Auto` | *(none)* | `"auto"` | Routed to `DriverEvent::Permission` |
| `RuntimeMode::FullAccess` | *(none — see below)* | `"auto"` | Driver auto-selects the allow option |
| `plan_mode: true` (any mode) | *(none)* | `"plan"` (toggled in-session via `session/set_config_option`) | As for the access mode |

- **Full access without `--dangerously-skip-permissions`**: Claude's `--dangerously-skip-permissions` overrides its plan mode (`docs/providers.md`, Plan mode). Until Phase 0 proves agy's flag leaves `plan` intact, Full access is implemented in the driver by answering every `session/request_permission` with its `allow_always` / `allow_once` option. That keeps plan mode and the access mode independent. If the flag is proven safe, it may replace the auto-answer.
- **Supervised must mean "ask"**: ACP's `default` mode asks "per your permission settings", so a user's agy allowlist could skip prompts. Phase 0 checks whether it does; if so, pin the policy for Raku sessions with `--settings` so Supervised routes every edit and command to Raku.
- **Plan approval**: Raku's plan flow ends in `DriverEvent::PlanProposed`. What agy emits when a plan is ready (a permission request, a `current_mode_update`, or only the end of the turn) is **unverified**. Antigravity declares `plan_mode: true` only once that mapping is known; otherwise it ships with `plan_mode: false`.

> Note: Per Raku's policy in `docs/providers.md`, changing `RuntimeMode` restarts the driver (`apply_options` returns `false`), whereas changing `model`, `reasoning_effort` (`thought_level`), or `plan_mode` applies in-session via `session/set_config_option`.

#### Model & Thinking-Level Discovery

- Primary source: `agy models` (sessionless). Derive each base model by stripping a known effort suffix (`-low`, `-medium`, `-high`, `-xhigh`, `-max`) and collect the stripped suffixes as that model's `ProviderModel::reasoning_efforts`. Display names drop the trailing `(Effort)`.
- Fallback source: an ACP `initialize` → `session/new` → `session/close` probe, combining `configOptions[category == "model"]` with `models.availableModels`. Use it only if `agy models` fails, and only after Phase 0 confirms `session/close` leaves no empty session behind.
- Models without suffixed variants (e.g. `gemini-pro-orchestrator`, `gpt-oss-120b-medium`) get an empty `reasoning_efforts` list, which automatically hides the effort picker in Raku's UI.
- Default model/effort comes from `configOptions[category == "model"].currentValue` and `configOptions[category == "thought_level"].currentValue` of the live session.
- Because `thought_level` is session-wide, a model change clamps the effort to the new model's set (nearest lower level, else the model's default), and the driver sends `model` before `thought_level`. Whether `model` takes the base ID or the suffixed ID is **unverified**.

#### `DriverControl` ↔ ACP Methods

| `DriverControl` Method | ACP JSON-RPC Action |
| --- | --- |
| `start` (fresh) | `initialize` → `session/new` → `session/set_config_option` (model → thought_level → mode) |
| `start` (resume) | `initialize` → `session/resume` (fallback `session/load`, replay suppressed) → `session/set_config_option` |
| `prompt(text)` | `session/prompt` with text + image content blocks |
| `cancel()` | `session/cancel` notification; answer any open permission request with `{ outcome: { outcome: "cancelled" } }` (`retain_runtime_after_cancel` is already `true` for non-Codex providers) |
| `respond(request_id, option_id)` | JSON-RPC response to `session/request_permission` with selected `optionId` |
| `apply_options(opts)` | In-session `session/set_config_option` for `model`, `thought_level`, `mode` (when only `plan_mode` changed); return `false` if `RuntimeMode` changed |
| `context_breakdown(detail)` | Required trait method. Answer `ContextBreakdownFailed` (or a breakdown built from the last `usage_update`) without blocking |
| `rollback(turns)` | Required trait method. `bail!` as unsupported (`conversation.rollback: false`) |
| `capabilities()` | `RuntimeCapabilities { steer: false }` |
| `Drop` / shutdown | Optional `session/close`, then close stdin (EOF) and reap `Child` on waiter thread |

#### ACP `session/update` → `DriverEvent`

| ACP Notification / Request | `DriverEvent` |
| --- | --- |
| Process spawn | `LaunchManifest` (via `driver/context_inspection.rs`, as every driver does) |
| `session/new` / `session/resume` result | `Connected { provider_cursor: Some(ProviderResumeCursor::Antigravity { session_id }) }` |
| `available_commands_update` | `AvailableCommands(Vec<ReportedCommand>)` |
| `agent_message_chunk` | `TextDelta(text)` |
| `agent_thought_chunk` | `ReasoningStarted` / `ReasoningBlockDelta` / `ReasoningCompleted` (or `ReasoningDelta`) |
| `tool_call` / `tool_call_update` | `RichActivity(ActivityItem)` via `driver/activity.rs` |
| `plan` | `RichActivity` (`ActivityKind::Plan`) |
| End of a plan-mode plan (**unverified**) | `PlanProposed { request_id }` |
| `usage_update` | `UsageUpdated { context_tokens: Some(used), context_window: Some(size) }` |
| `session_info_update` (if title present) | `AutoTitleUpdated(Some(title))` |
| `session/request_permission` (server request) | `Permission { request_id, title, detail, options }`, or auto-answered under Full access |
| `session/prompt` response, `stopReason: "end_turn"` | `TurnFinished { success: true, ... }` |
| `session/prompt` response, `cancelled` / `max_tokens` / `max_turn_requests` / `refusal` | `TurnFinished { success: false, ... }`, never a clean success |
| Auth error on `session/new` / `session/prompt` | `Error(...)` with login guidance (Phase 2) |
| JSON-RPC error / non-zero exit | `Error(...)` / `ProcessExited` |

---

## 3. Initial Capability Matrix

Declared explicitly in `crates/raku-protocol/src/capabilities.rs`:

```rust
Self::Antigravity => ProviderCapabilities {
    access_modes: &RuntimeMode::ACCESS_OPTIONS,
    conversation: ConversationCapabilities {
        rollback: false,
        fork: false,
    },
    compaction: CompactionCapabilities {
        manual: false,
        instructions: false,
        lifecycle: false,
        summary: false,
    },
    model_discovery: true,
    goals: false,
    plan_mode: true, // only if Phase 0 maps plan approval; otherwise false
    side_chat: false, // new field, see below
    subagent_target: false, // new field, see below
},
```

And `RuntimeCapabilities { steer: false }` on the live transport.

### Side chat and subagents are not hidden today

The side-chat provider picker (`src/app/side_chat.rs`, `render_side_chat_settings`) and the subagent provider picker (`src/app/subagents/settings_page.rs`) list every installed, enabled provider in `ProviderKind::ALL`. No existing capability excludes a provider, because Claude, Codex, and Pi all honor `DriverStartOptions::read_only` and `subagent.child`. Without a change, Antigravity would be offered as a side chat whose `read_only` policy the driver ignores (a side chat that can edit files), and as a subagent whose Ask-mode permission cards nobody can answer.

The MVP therefore adds two fields to `ProviderCapabilities`, exhaustive like the rest:

- `side_chat`: the transport enforces the read-only execution policy. Both pickers' filters, `resolve_side_chat_target`, and daemon start validation read it. `true` for Claude, Codex, and Pi.
- `subagent_target`: the transport can run as `subagent.child` without asking the user. `true` for Claude, Codex, and Pi.

As defense in depth, `AgyDriver::start` returns an error when `read_only` is set while `side_chat` is `false`, and never raises `Permission` for a `subagent.child` launch (it auto-declines, so the subagent reports what it could not do).

Delegation *from* an Antigravity task is simply absent until the `subagent` tool is bridged: the driver ignores `subagent.agents`, and `system_instructions` already omits the tool when it is not offered.

### Features deferred to follow-up phases

- Native rollback & fork (not advertised in `agy acp` `sessionCapabilities`). Note that `docs/providers.md` step 7 expects rewind and branch to be implemented or emulated; this is a deliberate MVP gap, not the end state.
- Mid-turn steering (standard ACP `session/prompt` is turn-scoped).
- Manual/automatic compaction lifecycle (unless `agy acp` exposes compaction events/RPCs).
- Provider-persisted thread goals & usage-meter quota API.
- Read-only side chat and subagent targeting (flip `side_chat` / `subagent_target` once Phase 5 verifies a tool restriction).
- Raku-hosted `subagent` MCP tool for delegation from Antigravity tasks.

---

## 4. Phased Implementation Plan

### Phase 0: Live Protocol Verification (Pre-Code Spike)

Before writing Rust code, run a short Python probe script against `agy acp` (outside sandbox) to capture exact wire samples for:

1. `_meta.antigravity.systemPrompt.append` acceptance on `session/new`.
2. `session/prompt` streaming lifecycle: exact JSON shape of `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`, and final `session/prompt` result (`stopReason`).
3. `session/request_permission` exact parameter shape (`options[].optionId`, `options[].name`, `options[].kind`, `toolCall`) and expected response (`{ outcome: { outcome: "selected", optionId } }` vs cancelled).
4. `session/set_config_option` request/response shape for `model`, `thought_level`, and `mode`, and whether `model` takes the base or suffixed ID. Check what happens when `thought_level` is unsupported by the current model.
5. `session/list` and `session/load` / `session/resume` response shapes. Confirm whether `session/list` without `cwd` returns every session or only the current directory's, whether `session/load` replays history as `session/update` notifications, and whether `session/new` followed immediately by `session/close` leaves an empty session on disk.
6. One-shot `agy -p --output-format text --disable-slash-commands --mode plan` behavior for commit messages and reasoning headlines: whether each run creates a conversation in agy's history, whether it can still read files, and which `--model` ID form it takes.
7. **Auth**: with credentials removed (or a fresh `--app-data-dir`), capture the error code and message from `session/new` and `session/prompt`, and what `agy acp --login` does.
8. **Failed turns**: an exhausted quota or invalid key mid-session. Confirm the `stopReason` is not `end_turn` with empty output (`docs/providers.md` step 8).
9. **Supervised semantics**: in `default` mode with an agy permission allowlist present, does every edit and command still raise `session/request_permission`? If not, find the `--settings` keys that force asking.
10. **Full access vs plan mode**: does `--dangerously-skip-permissions` override `plan`? Does auto-answering permissions under `auto` cover everything the flag does?
11. **Plan approval**: what agy emits when a plan-mode turn finishes its plan, and how leaving plan mode resumes implementation.
12. **Titles**: whether `session_info_update` ever carries a title, and when.
13. **Cancel**: `session/cancel` with a permission request open; the resulting `stopReason`.
14. **Read-only candidates** (for Phase 5): what `--sandbox`, `plan` mode, and `--settings` tool restrictions actually block; whether stdio entries in `mcpServers` are honored.

Save sanitized sample fixtures for unit tests in `crates/raku-core/src/driver/fixtures/` (alongside the existing driver fixtures).

---

### Phase 1: Protocol & Capability Foundation (`crates/raku-protocol`)

1. **`crates/raku-protocol/src/model.rs`**:
   - Add `ProviderKind::Antigravity` and update `ProviderKind::ALL` (`[Self::Claude, Self::Codex, Self::Pi, Self::Antigravity]`). Update the `ALL.len()` test.
   - Implement `id()` (`"agy"`), `display_name()` (`"Antigravity"`), `short_name()` (`"Agy"`), `command()` (`"agy"`).
   - Add `ProviderResumeCursor::Antigravity { session_id: String }` and update `from_session_id`, `provider`, and `native_id`.
2. **`crates/raku-protocol/src/capabilities.rs`**:
   - Add the `side_chat` and `subagent_target` fields (`true` for the existing providers) and the `ProviderKind::Antigravity` arm.
   - Update unit tests to assert `Antigravity`'s access modes and capabilities.
3. **`crates/raku-protocol/src/composer.rs`**: add the skill invocation syntax for Antigravity in the `match provider` near line 289 (`$name` for Codex, `/name` for Claude, `/skill:name` for Pi); Phase 0 confirms agy's form.
4. **`crates/raku-protocol/src/model_catalog.rs`**: add the `fallback_models` arm (`Vec::new()`).
5. Let the compiler's non-exhaustive-match errors drive the rest of the crate.

---

### Phase 2: ACP Driver (`crates/raku-core/src/driver/agy.rs`)

1. **Create `crates/raku-core/src/driver/agy.rs`**:
   - Modeled after `codex.rs`'s named-thread stdio JSON-RPC architecture:
     - **Writer thread**: owns `ChildStdin` and `Receiver<CommandMessage>`, serializes JSON-RPC requests/responses/notifications, tracks in-flight request IDs (`PendingResponses`), handles clean EOF on `Shutdown`.
     - **Reader thread**: reads newline-delimited JSON from `ChildStdout`, distinguishes:
       - Responses to Raku's requests (`id` present, no `method`)
       - Server→client requests (`id` and `method` present, e.g. `session/request_permission`)
       - Notifications (`method` present, no `id`, e.g. `session/update`)
     - **Stderr thread**: collects bounded stderr for diagnostics on failure.
     - **Reaper thread**: blocks on `child.wait()`, emits `Error` on unexpected non-zero exit, and always emits `ProcessExited`.
   - **Startup**:
     - Spawn `agy acp` (no `--dangerously-skip-permissions`; see §2.4), plus any `--settings` policy Phase 0 requires for Supervised.
     - Emit `LaunchManifest` via `context_inspection::launch_manifest`.
     - Send `initialize` (with 30 s startup timeout, matching `pi.rs` headroom for language server startup).
     - Send `session/resume` (if `provider_cursor` is present) or `session/new` with `cwd`, `mcpServers: []`, and `_meta.antigravity.systemPrompt.append` carrying `system_instructions(read_only, &subagent)`. If resume falls back to `session/load`, drop every `session/update` until the `session/load` response arrives, so the replayed history never reaches the transcript twice.
     - Reject a cursor from another provider at start, like the other drivers.
     - Apply requested `model`, then `reasoning_effort` (clamped to the model's set), then `mode` (`plan` if `options.plan_mode`, else mapped `options.mode`) via `session/set_config_option`.
     - Emit `Connected { provider_cursor }`.
   - **Auth**: when `session/new` or `session/prompt` fails with the auth error captured in Phase 0, emit `Error` with localized guidance to log in (`agy`, or `agy acp --login`, offered as a `sh run` block), never a generic failure. Leave the runtime closed so the next prompt retries after login.
   - **Turn Execution & Streaming**:
     - `prompt(text)`: parse Raku's inline attachment markers (if any) into ACP `text` + `image` content blocks, send `session/prompt`, emit `TurnStarted`, stream updates, emit `TurnFinished` on response, mapping `stopReason` per §2.4.
     - Normalize `tool_call` / `tool_call_update` into `ActivityItem` (`Command`, `FileChange`, `Search`, `Plan`, `Tool`) via `driver/activity.rs`.
     - Map `session/request_permission` to `DriverEvent::Permission`, store pending JSON-RPC `id`, and reply on `respond(request_id, option_id)`. Under Full access, answer with the allow option directly. For a `subagent.child` launch, never raise `Permission`; decline instead.
     - Map `cancel()` to the `session/cancel` notification, answering any open permission request with the `cancelled` outcome.
     - Map the plan-ready signal from Phase 0 to `PlanProposed`.
   - **Policy guards**: `start` fails when `read_only` is set (until `side_chat` is supported). `subagent.agents` is ignored (no delegation yet).
   - **Option Changes**:
     - `apply_options(new)`: if `mode` changed, return `false` (restart). Otherwise send `session/set_config_option` for changed `model`, `thought_level`, and `plan_mode` (`"plan"` vs base mode) and return `true`.
   - **Required trait methods**: `context_breakdown` answers asynchronously (`ContextBreakdownFailed` until a breakdown exists); `rollback` bails as unsupported.
   - **Unit Tests**:
     - Handshake & `session/new` parsing (camelCase keys only).
     - Streaming `agent_message_chunk`, `agent_thought_chunk`, `tool_call`/`tool_call_update`, `available_commands_update`, `usage_update`.
     - Bidirectional `session/request_permission` round-trip, Full-access auto-answer, and subagent-child decline.
     - `session/load` replay suppression.
     - Cancel with an open permission request.
     - `stopReason` mapping, including non-`end_turn` reasons.
     - Auth error mapping.
     - `apply_options` in-session vs restart matrix, including effort clamping on model change.
     - Stdin-EOF teardown and non-zero exit error reporting.
   - **Integration test**: an `#[ignore]`d test that drives the real `agy acp` through the driver for one turn (`docs/providers.md` step 6).
2. **Wire into `crates/raku-core/src/driver/mod.rs`**:
   - Declare `mod agy;` and add `ProviderKind::Antigravity => Arc::new(agy::AgyDriver::start(options, events)?)` in `start_local`.

---

### Phase 3: Discovery, Auxiliary Services & Session Import (`crates/raku-core`)

1. **Model Discovery (`crates/raku-core/src/model_catalog.rs`)**:
   - Add the `ProviderKind::Antigravity` arm to `discover_catalog`, calling `discover_agy_models(binary: &Path) -> Vec<ProviderModel>`:
     - Run `agy models` with `current_dir(catalog_working_directory())` and a bounded timeout, and parse it per §2.4.
     - Fall back to the ACP `session/new` probe only under the condition in §2.4.
   - Add `fallback_models(ProviderKind::Antigravity)` returning `Vec::new()` (same as `Pi`, relying on live discovery + on-disk cache in `model-cache/agy.json` so unavailable account models aren't fabricated).
2. **Slash Commands (`crates/raku-core/src/slash_command_catalog.rs`)**:
   - Return `None` for sessionless discovery (live sessions immediately publish authoritative commands via `available_commands_update` → `DriverEvent::AvailableCommands`, matching the comment already in `model.rs`).
3. **Skills & Composer Completion**:
   - `crates/raku-core/src/skills.rs`: add user-scope skill root `~/.gemini/antigravity-cli/skills` under `SkillSource::Provider(ProviderKind::Antigravity)`. Project-scope `.agents/skills` is already scanned under `SkillSource::Shared`.
   - `crates/raku-core/src/composer_complete.rs`: add the Antigravity arm near line 129 with agy's command and skill roots.
4. **One-Shot Text Generation & Commit Messages**:
   - `crates/raku-core/src/text_generation.rs`: add `ProviderKind::Antigravity` arm using `agy -p --output-format text --disable-slash-commands --mode plan` (plus `--model` / `--effort` when provided).
   - `crates/raku-core/src/git_commit.rs`: add `ProviderKind::Antigravity` arms (near lines 233 and 603) using the same print-mode flags, pinned to a fast model/low effort when available.
   - Pi's arm passes `--no-session --no-tools`; agy has neither. If Phase 0 shows each run lands in agy's history or can read files, restrict it with `--settings` / `--sandbox`, or leave Antigravity out of commit and headline generation until it can run clean.
5. **External Session Listing & Cold Import (`crates/raku-core/src/agy_session.rs` + `daemon.rs`)**:
   - The `match provider` arms in `daemon.rs` for listing (near line 529) and history (near line 586) need Antigravity arms in the MVP even if import is deferred; a deferred arm returns an empty list.
   - Implement `list_provider_sessions` using `agy acp` `session/list` (scoped per Phase 0's cwd finding), mapping into `ProviderSessionSummary`.
   - Implement cold history load via `session/load` (capturing replayed `session/update` notifications) or defer cold import to Phase 5 if `session/load` replay needs extra mapping.
6. **Daemon conversation arms (`crates/raku-core/src/daemon.rs`)**: the fork (near line 1137) and rollback (near line 1245) paths are `match provider`, not capability-driven. Add explicit unsupported arms.
7. **Session Titles**:
   - `crates/raku-core/src/driver/title_language.rs`: handle `ProviderKind::Antigravity` in title-language context injection / stripping.
   - If Phase 0 shows agy never names its sessions over ACP, give Antigravity a Raku-run title turn like Codex's (`docs/titles.md`), using the print-mode arguments from item 4.

---

### Phase 4: Client, Desktop UI, Assets & Localization

1. **Provider Icon & Assets**:
   - Add `assets/icons/provider-agy.svg` (monochrome SVG matching `provider-claude.svg`, `provider-openai.svg`, `provider-pi.svg` sizing/viewBox) and register it in `assets/icons/SOURCE.md`, `src/assets.rs`, and `provider_icon` in `src/ui/mod.rs`.
   - Add the `provider_color` arm in `src/ui/mod.rs` (near line 167), legible in both themes.
2. **Desktop App Exhaustive Matches (`src/app/`)**:
   - `src/app/runtime.rs`: the rollback (near line 459) and fork (near line 612) paths are `match provider`; add explicit unsupported arms. The `matches!(provider, Codex | Pi)` driver-start checks need no change.
   - `src/app/side_chat.rs` and `src/app/subagents/settings_page.rs`: filter the provider pickers by `capabilities().side_chat` / `subagent_target`, and make `resolve_side_chat_target` reject a provider without `side_chat` (falling back as it does for a disabled provider).
   - `src/app/settings.rs`: automatically renders `ProviderKind::ALL` for enable/disable and binary path override; verify layout/order and keyboard navigation.
   - `src/app/usage_meter.rs`: no change; `plan_usage_key_for_session` already falls through to `None` for providers without a meter.
   - `retain_runtime_after_cancel` (`src/app/sessions.rs`) already returns `true` for every non-Codex provider; no change.
   - Everything else that only iterates `ProviderKind::ALL` (`composer.rs`, `command_palette.rs`, `sessions.rs`, `text_generation.rs`) picks Antigravity up automatically; let the compiler find any remaining matches.
3. **`crates/raku-client/`**:
   - Update `persistence.rs` for `ProviderKind::Antigravity` where it matches; `composer_complete.rs` only iterates `ALL`.
4. **Localization (`locales/`)**:
   - Add the login-guidance error and any other new provider-specific strings to `locales/app.yml`, `locales/ja.yml`, and `locales/zh-CN.yml`.
5. **Documentation (`docs/providers.md`)**:
   - Add an `## Antigravity CLI` section and update the comparison table.
   - Add Antigravity to the option-change table, the access-modes table, the plan-mode table, and the resume-cursor table.
   - Document the `side_chat` / `subagent_target` capabilities under Capability ownership.

---

### Phase 5: Follow-Up Enhancements (Post-MVP)

Once core interactive sessions are solid:
- **Read-only Side Chat**: implement `read_only` with the restriction Phase 0 item 14 found (or pass an appropriate agent/tool policy), then set `side_chat: true`.
- **Subagent target**: once child launches run without asking, set `subagent_target: true`.
- **Raku Subagent Delegation**: bridge Raku's `subagent` tool through a stdio `mcpServers` entry on `session/new`.
- **Rewind & branch**: emulate them the way Claude does (seed a fresh session with retained history) if agy never exposes native truncation.
- **Usage Meter**: evaluate whether `agy` exposes account quota metadata via ACP `_meta` or CLI for `src/app/usage_meter.rs`.
- **Generic ACP Refactor**: if a second ACP agent is added later, extract the JSON-RPC ACP core from `driver/agy.rs` into `driver/acp.rs` with provider-specific hooks.

---

## 5. Verification Plan

1. **Automated Tests**:
   - `cargo test --workspace` covering `raku-protocol` capabilities/serialization, `raku-core` ACP driver unit tests, model catalog parsing (`agy models` output), and `raku-daemon` validation.
   - The `#[ignore]`d live integration test, run manually against the installed `agy`.
2. **Live App Validation** (in the dev-watcher-managed `Raku Debug.app`):
   - Settings page: `Antigravity` appears, binary resolves (`agy`), models and thinking levels populate after discovery, and per-model effort lists match `agy models`.
   - Logged out: a prompt shows the login guidance, and the next prompt after logging in succeeds.
   - New session: send prompt, verify streaming reasoning + markdown text + tool activities + `usage_update` occupancy.
   - Access modes: verify `Ask` triggers an interactive permission card and `Accept` / `Decline` unblocks the tool, even with an agy allowlist present; verify `FullAccess` runs without prompting; verify `Plan` mode toggle works in-session, including under Full access, and that the plan approval card appears (if `plan_mode` ships).
   - In-session model & thinking-level switching without restarting the process, including a switch to a model that lacks the current effort.
   - Cancel mid-turn (`Stop`), including while a permission card is open, then send a follow-up prompt on the same live runtime.
   - Image attachment sent and acknowledged by the model.
   - Restart app / reopen session and verify `session/resume` continues the conversation without duplicated history.
   - Antigravity is absent from the side-chat and subagent provider pickers.
   - Commit message generation and reasoning headline generation using `Antigravity`, and confirm they leave no stray conversations in agy's history.
   - New sessions get a title.
