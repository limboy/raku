# Provider integrations

How Waku talks to each coding agent: the process it launches, the wire protocol
it speaks, how long that process lives, and what has to be emulated because the
CLI does not offer it.

How each of them names a session — which are read from the provider, which are
polled off disk, and the one Waku generates itself — is in
[titles.md](titles.md).

Every provider is reached through the same driver abstraction in
[driver/mod.rs](../crates/waku-core/src/driver/mod.rs). There are five
transport implementations behind six providers, and **every one of them holds a
session that spans the whole conversation**:

| Transport | File | Providers |
| --- | --- | --- |
| Codex app-server (JSON-RPC over stdio) | [driver/codex.rs](../crates/waku-core/src/driver/codex.rs) | Codex CLI |
| OpenCode background service (HTTP + server-sent events) | [driver/opencode.rs](../crates/waku-core/src/driver/opencode.rs) | OpenCode |
| Pi RPC mode (NDJSON request/response over stdio) | [driver/pi.rs](../crates/waku-core/src/driver/pi.rs) | Pi, Oh My Pi |
| Claude streaming-input session (NDJSON over stdio) | [driver/claude.rs](../crates/waku-core/src/driver/claude.rs) | Claude Code |
| Harness client API (typed HTTP + downlink streams) | [driver/deepseek.rs](../crates/waku-core/src/driver/deepseek.rs) | DeepSeek Harness |

DeepSeek Harness has no dedicated section below yet; its driver's module
comment is the current reference.

## The driver contract

`driver::start(provider, DriverStartOptions, Sender<DriverEvent>)` returns a
`DriverHandle`. The UI never touches a process: it sends commands through
`DriverControl` and receives `DriverEvent`s on a `crossbeam` channel that the
frame loop drains.

Inputs ([driver/mod.rs:67](../crates/waku-core/src/driver/mod.rs#L79)):

```rust
pub struct DriverStartOptions {
    binary, cwd, mode,
    model, reasoning_effort, service_tier,
    computer_use_enabled, provider_cursor,
}
```

Outputs ([model.rs:973](../crates/waku-core/src/model.rs)): `Connected`,
`AvailableCommands`, `TurnStarted`, `TextDelta`, `ReasoningDelta`, `Activity`,
`RichActivity`, `Permission`, `ComputerUseUpdated`, `SteerAccepted`,
`SteerRejected`, `TurnFinished`, `Error`, `ProcessExited`.

A transport that can inject a user message into the *running* turn advertises
it through `DriverControl::supports_steer` and delivers it with `steer`; the
outcome comes back asynchronously as `SteerAccepted` or `SteerRejected`. When
steering is unsupported, refused, or the session is still connecting, the app
falls back to its own follow-up queue — the message stays visible above the
composer and starts a fresh turn once the current one settles.

Every driver normalizes its tool events into one `ActivityItem`
(`Reasoning | Command | FileChange | Search | Plan | Tool`) via
[driver/activity.rs](../crates/waku-core/src/driver/activity.rs), so the transcript renders
provider-agnostic rows. Tool titles prefer a `title` argument when the tool
supplies one, then fall back to the command, the query, or a de-camel-cased
tool name.

### Context compaction

Compaction belongs to the provider's native session. Waku offers a manual
`/compact` command, a usage-panel action, and a command-palette action for
Codex, Claude Code, OpenCode, Pi and Oh My Pi. Pi, Oh My Pi and Claude accept
optional focus instructions; Codex and OpenCode reject that argument instead
of silently discarding it. Other providers keep their native command handling.

The driver contract carries `CompactionRequest` and complete
`CompactionUpdated` snapshots. Manual requests use Codex's
`thread/compact/start`, OpenCode's `POST /api/session/{id}/compact`, Pi/OMP's
`compact` RPC, or Claude's native `/compact` user message. An acknowledgement
is not completion. Codex's `contextCompaction` items, OpenCode's
`session.compaction.*`, Pi's compaction events/results (including older
`auto_compaction_*` events), and Claude's status/compact-boundary messages
supply the lifecycle. A failed or disconnected operation terminalizes too.

Each operation has its own stable identity. OpenCode uses `inputID` or the
opening event ID transformed from `evt_` to `msg_`, matching its stored history.
Summary deltas accumulate in the driver before it emits a snapshot; tool-output
snapshots keep their replacement semantics. Reconnection repairs only the
observed, still-running OpenCode operation with the same native ID. It never
replays an unrelated historical boundary as a live event. Cold import retains
public compaction metadata at its original message boundary, while excluding
provider-private checkpoints and control state.

Compaction activities have their own transcript blocks and remain visible when
a turn's ordinary work folds. Summary/error detail is collapsed and keyboard
operable. Trigger, duration and token counts appear only when known; estimated
after counts are marked as estimates. The occupancy gauge waits for a measured
usage report instead of presenting a summary estimate or stale pre-compaction
count as current usage. Completed snapshots replay idempotently.

Manual compaction can start an idle/cold runtime without creating a user prompt.
New messages queue while compaction runs. Completion resumes that queue; failure
or Stop retains it for the user. Provider-native compaction turns do not create
conversation turns, completion notifications, or Git checkpoints. Claude command
success without a compact boundary is shown as unconfirmed, with its returned
message preserved; it does not claim the context was reduced. Standalone
compaction stays busy across list-only reloads, and forced cleanup terminalizes
both its transcript row and lifecycle metadata. Provider configuration and
automatic thresholds remain owned by the installed CLI.

### Runtime lifetime in the app

A driver is created lazily per session by `ensure_driver`
([src/app/runtime.rs:927](../src/app/runtime.rs#L1016)) and stored in
`Waku::runtimes` keyed by session id. Runtimes are per session, not per view:
switching sessions in the sidebar does not touch them, so a background session
keeps streaming into its transcript.

A runtime — and with it that session's provider process — is dropped when:

| Trigger | Where |
| --- | --- |
| The user stops a turn, **Codex only** | [src/app/sessions.rs:3](../src/app/sessions.rs#L3) |
| The provider changes, or an option changes that the transport cannot apply in session | `apply_session_options`, [src/app/runtime.rs](../src/app/runtime.rs) |
| The session is deleted | [src/app/sessions.rs:178](../src/app/sessions.rs#L178) |
| A rewind or branch leaves the driver on a stale native session | [src/app/runtime.rs](../src/app/runtime.rs) |
| The driver reports `ProcessExited` (the handler returns `false`, so the runtime is not reinserted) | [src/app/streaming.rs:352](../src/app/streaming.rs#L352) |
| Nobody has touched the session for 30 minutes | `reap_idle_sessions`, [src/app/runtime.rs](../src/app/runtime.rs) |
| Waku quits | `cx.quit()` |

Stop drops the runtime for Codex, whose app-server owns the Computer Use process
tree — stopping means ending the process, and the next prompt resumes the native thread
(`thread/resume`). Every other provider has a protocol
interrupt and keeps its runtime (`retain_runtime_after_cancel`).

Option changes go through `DriverControl::apply_options`, which returns whether
the transport absorbed the change or wants to be restarted:

| Change | Codex | Pi | OpenCode | Claude |
| --- | --- | --- | --- | --- |
| Model, reasoning effort, service tier | in session — they ride on every `turn/start` | in session — `set_model`, `set_thinking_level` | in session — `POST /api/session/{id}/model`, with effort as the model's variant | in session — a `set_model` control request |
| Access mode | restart | restart | in session — the mode only decides who answers a permission request, which is the driver's own state | restart |
| Provider | restart | restart | restart | restart |

The permission policy is deliberately excluded even for Codex, which does carry
`approvalPolicy` and `sandboxPolicy` on every `turn/start`: loosening or
tightening what an already-running agent may touch deserves a fresh thread.
OpenCode is the exception because nothing about the mode reaches the provider:
its agent's own rules still decide what to ask, and Waku only decides whether
to ask the user or answer `once` itself. T3
Code draws the line in the same place — it restarts on `runtimeModeChanged` and
keeps the session only for a model change the adapter declares it can switch.

The idle sweep runs at most every 5 minutes off the existing frame tick and skips
any session with an active turn, so a slow tool call or an unanswered approval is
never reaped out from under the user.

Note what is *not* on the teardown list: finishing a turn. `TurnFinished` leaves
the long-lived processes resident and idle, which is the point of them — until
the idle sweep decides otherwise.

### How the long-lived processes actually die

Two shapes, depending on the transport.

**The stdio drivers — Codex, Pi, and Claude — are never
signalled**.
Termination is by **closing stdin**:

1. The driver is dropped, which sends `CommandMessage::Shutdown` (and drops the
   command `Sender`, so a missed send has the same effect).
2. The writer thread breaks out of its loop and returns, dropping the
   `ChildStdin` it owns.
3. The provider sees EOF on stdin and exits.
4. Its stdout closes, ending the reader thread, and `ProcessExited` is emitted.

So the process is asked to leave by having its input closed, and a provider that
ignored stdin EOF would linger. On quit the same thing happens for free:
`cx.quit()` may not run `Drop`, but the OS closes the descriptors, which is the
identical signal.

Each of these drivers moves its `Child` into a dedicated thread that blocks on
`wait()`, so the process is reaped and a non-zero exit status becomes an `Error`
when stderr has not already explained itself. Rust's `Child::drop` neither kills
nor reaps, so a driver that skipped that thread would leave a zombie for the life
of the app — which Pi did until it was given one.

**OpenCode is different again**: Waku does not own the process at all. Its
background service belongs to the user and outlives every Waku task, so
dropping a runtime only unsubscribes that session from the shared event stream
([opencode_service.rs](../crates/waku-core/src/opencode_service.rs)). Waku never
signals the service, never calls its stop route, and never writes its
registration file.

## At a glance

| | Codex CLI | Pi | Oh My Pi | Claude Code | OpenCode |
| --- | --- | --- | --- | --- | --- |
| Binary | `codex` | `pi` | `omp` | `claude` | `opencode` |
| Wire protocol | JSON-RPC over stdio | NDJSON RPC over stdio | NDJSON RPC over stdio | stream-json over stdio | HTTP + SSE |
| Process spans the whole session | yes | yes | yes | yes | yes |
| Process spawned per turn | no | no | no | no | no |
| Bidirectional | yes | yes | yes | yes | yes |
| Reasoning stream | yes | yes | yes | yes | yes |
| Interactive approvals | yes | no | no (has them; Waku runs `--yolo`) | yes | yes |
| Mid-turn steering | yes | yes | yes | yes | yes |
| Model discovery | yes | yes | yes | no (fixed) | yes |
| Computer Use | yes | yes | no (ships its own) | no | yes |
| Restricted to Full access | no | yes | yes | no | no |
| Rewind and branch at a turn | yes | yes | yes | yes | yes |

Every provider now holds a session across turns. That was not true when this
document was first written: five of the seven spawned a process per prompt, and
everything stateful — resume, rewind, branch, approvals — had to be reconstructed
from a session id, an on-disk transcript, or a side-channel. In each case the CLI
turned out to already serve a session protocol; nobody had looked.

---

## Codex CLI

**Launch** — `codex app-server --stdio`
([driver/codex.rs:164](../crates/waku-core/src/driver/codex.rs#L164)), plus `-c` config
overrides when Computer Use is on.

**Protocol** — newline-delimited JSON-RPC over stdio, genuinely bidirectional:
Codex can send Waku requests (approvals) and Waku answers them by id. Three
threads: writer (owns stdin and the command queue), reader (parses stdout),
stderr collector; a fourth waits on the process and emits `ProcessExited`.

**Lifetime** — long-lived: one app-server serves the whole session, staying
resident and idle between turns. It ends when the runtime is dropped — pressing
Stop, changing a launch option, deleting the session, or quitting — by closing
its stdin, never by a signal. See
[Runtime lifetime in the app](#runtime-lifetime-in-the-app).

**Handshake**

1. `initialize` (id `0`) with `clientInfo` and `capabilities.experimentalApi`.
2. `initialized`.
3. `skills/extraRoots/set` when Computer Use is on, so Waku's bundled skill is
   discovered like Codex's own skills rather than injected as instructions.
4. `thread/start` or `thread/resume` (id `1`) with `cwd`, `approvalPolicy`,
   `sandbox`, `approvalsReviewer`, and optional `model` / `serviceTier`.

The reply to id `1` carries `result.thread.id` (→ `Connected` with a
`ProviderResumeCursor::Codex`) and `result.thread.turns[]`, whose ids are
retained because `thread/fork` needs a `lastTurnId`.

**Per turn** — `turn/start` with `threadId`, `input: [{type: "text", …}]`,
`approvalPolicy`, `approvalsReviewer`, `sandboxPolicy`, and optional `model`,
`effort`, `serviceTier`.

**Inbound stream** ([driver/codex.rs:851](../crates/waku-core/src/driver/codex.rs#L882)):

| Method | Becomes |
| --- | --- |
| `turn/started` | `TurnStarted` (records the turn id) |
| `item/agentMessage/delta` | `TextDelta` |
| `item/reasoning/summaryTextDelta`, `item/reasoning/textDelta` | `ReasoningDelta` |
| `item/started`, `item/completed` | `RichActivity` (command, patch, web search, plan, MCP tool) |
| `turn/completed` | `TurnFinished { success: status == "completed" }` |
| `error`, `mcpServer/startupStatus/updated` (failed) | `Error` |
| `*requestApproval*` (a request, has an `id`) | `Permission` |

**Approvals** — Codex is the only provider with a real approval channel. The
request becomes a `Permission` event with `accept` / `acceptForSession` /
`decline`, and the answer is written back as a JSON-RPC *response*:
`{"id": <original>, "result": {"decision": …}}`. Because JSON-RPC ids are
per-peer, the reader only treats method-less messages as replies to Waku's own
requests ([driver/codex.rs:779](../crates/waku-core/src/driver/codex.rs#L809)).

**Cancel** — `turn/interrupt {threadId, turnId}`.

**Steer** — `turn/steer {threadId, expectedTurnId, input}`. The RPC response
resolves the pending steer to `SteerAccepted`, or to `SteerRejected` with the
CLI's reason when the expected turn no longer matches — the server-side check
that makes Codex the one provider whose steer cannot race a settling turn.

**Rewind** — `thread/rollback {threadId, numTurns}`, in place; the cursor is
unchanged. **Branch** — `thread/fork {threadId, lastTurnId}` returns a new
thread id. Both are synchronous from the UI's perspective: the command carries a
response channel and blocks up to 15 s.

**Citations** — Codex marks web citations with private-use characters
(`U+E200`/`U+E201`/`U+E202`). They are buffered across deltas and rewritten into
markdown links against the `webSearch` results captured earlier in the turn;
unknown markers are dropped. Private control markers never reach the transcript
([driver/codex.rs:660](../crates/waku-core/src/driver/codex.rs#L690)).

**Models** — a throwaway app-server, `model/list` paged via `nextCursor`, up to
32 pages ([model_catalog.rs:367](../crates/waku-core/src/model_catalog.rs#L367)).

**Computer Use** — `-c mcp_servers.waku_js_repl.command=…` registers Waku's
QuickJS MCP server, with several `-c` flags disabling Codex's own external
computer-use plugin/MCP/skill so only Waku's `js` / `js_reset` surface is
visible.

---

## Pi and Oh My Pi

Oh My Pi is a fork of Pi that kept the RPC transport and renamed part of its
surface, so one driver serves both. `PiFlavor`
([pi.rs:39](../crates/waku-core/src/driver/pi.rs#L39)) carries every divergence,
which is what keeps the two from drifting into near-copies:

| | Pi | Oh My Pi |
| --- | --- | --- |
| Binary | `pi` | `omp` |
| Full-access flag | `--approve` | `--yolo` |
| Update check | skipped by `PI_SKIP_VERSION_CHECK=1` | no env opt-out; gated by a setting, and off the startup path either way |
| Oversized frames | whole | chunked, once `negotiate_protocol {protocolVersion: 2}` is accepted |
| Run settles on | `agent_settled` | `agent_end` |
| Title event / field | `session_info_changed` / `name` | `session_info_update` / `title` |
| Branch commands | `get_fork_messages`, `fork` | `get_branch_messages`, `branch` |
| Whole-session copy | in place | only at launch, so Waku shells out (see below) |
| Computer Use | Waku's Pi extension | none — Oh My Pi ships its own `/computer` |
| Catalog probe's context-files flag | `--no-context-files` | `--no-rules` |

Everything below is shared unless noted.

**Launch** — `pi --mode rpc --approve` with `PI_SKIP_VERSION_CHECK=1`;
`omp --mode rpc --yolo`
([pi.rs:246](../crates/waku-core/src/driver/pi.rs#L246)). Oh My Pi negotiates
protocol v2 first, before `get_state`, so a large first response arrives chunked
rather than shrunk to an error frame. Its opening `ready` frame is what makes
that worth doing — it reports `supportedProtocolVersions: [1, 2]` alongside a
`maxFrameBytes` of 1 MiB and a `maxReassembledFrameBytes` of 64 MiB, so v1 caps
a response at the frame size while v2 reassembles up to 64× that.

Neither `--yolo` nor `--fork` appears in `omp --help`, but both are accepted
(verified against 17.3.8). Do not "fix" them by reading the help text: omp
rejects a genuinely unknown flag outright with `Error: unknown flag`, so the
absence is the help being abridged, not the flag being gone. That same
strictness is why its catalog probe cannot borrow Pi's argument list.

**Protocol** — NDJSON over stdio, but request/response rather than JSON-RPC:
Waku stamps each request with a string id (`waku-<n>`) and Pi answers with
`{"type": "response", "id", "success", "data"}`. Everything else on the stream
is an unsolicited event. Requests are issued synchronously by the writer thread
with a 10 s timeout ([pi.rs:800](../crates/waku-core/src/driver/pi.rs#L800));
events keep flowing on the reader thread meanwhile. The handshake gets 30 s
instead: the agent does not answer at all until it has finished loading its
extensions, resources and — when model networking is on — its model catalog,
which Pi itself budgets 15 s for, so the live-process timeout there would fail
a session that was merely slow to start.

**Lifetime** — long-lived, and unlike Codex it survives Stop: cancelling sends
`abort` over the existing connection. It ends when the runtime is dropped, by
stdin EOF; nothing reaps it afterwards.

**Handshake** — `get_state` → optional `switch_session {sessionPath}` when
resuming → `set_model {provider, modelId}` → `set_thinking_level {level}` →
`get_state`. The final state supplies `/data/sessionId` and `/data/sessionFile`;
both go into the cursor, and resume needs the **file path**, not just the id.

**Per turn** — `{"type": "prompt", "message": …}`.

**Inbound stream** ([pi.rs:1182](../crates/waku-core/src/driver/pi.rs#L1182)):

| Event | Becomes |
| --- | --- |
| `agent_start`, `turn_start` | `TurnStarted` (once per run) |
| `message_update` → `text_delta` / `thinking_delta` | `TextDelta` / `ReasoningDelta` |
| `message_end` | fallback text/thinking when no delta was streamed |
| `tool_execution_start` / `_update` / `_end` | `RichActivity` |
| `auto_retry_end` | clears or sets the failure flag |
| `agent_settled` (Pi) / `agent_end` (Oh My Pi) | `TurnFinished`, then resets stream state |
| `extension_ui_request` | auto-cancelled — Waku has no UI for extension prompts |

**Access modes** — Full access only, enforced at driver start rather than
degraded silently: any other selection fails with "currently supports Full
access only" ([pi.rs:209](../crates/waku-core/src/driver/pi.rs#L209)).
Pi has no permission system at all, so `--approve` is the whole story. Oh My Pi
*does* have one, which Waku's `--yolo` then bypasses — the restriction is Waku's
here, not the CLI's, and lifting it is a matter of wiring Oh My Pi's permission
requests to a `Permission` event.

**Cancel** — `{"type": "abort"}`.

**Steer** — `{"type": "steer", "message": …}`; the request acknowledgment
resolves to `SteerAccepted` or `SteerRejected`.

**Rewind and branch** — both go through `get_fork_messages` → `fork {entryId}`
(`get_branch_messages` → `branch` on Oh My Pi), or `clone` when nothing is
removed, then `get_state`
([pi.rs:996](../crates/waku-core/src/driver/pi.rs#L996)). Rewind adopts the fork
as the session's new cursor. Branch additionally `switch_session`es back to the
source file and verifies it landed on the right session; if that restore fails
the runtime is dropped, because the RPC process may still be sitting on the fork
([runtime.rs](../src/app/runtime.rs)).

**Copying a whole session differs.** Removing no turns is a plain copy, which Pi
performs in place. Oh My Pi only copies at launch, so Waku shells out to a
throwaway `omp --mode rpc --yolo --fork <session file>` and reads the new cursor
off it ([pi.rs:1108](../crates/waku-core/src/driver/pi.rs#L1108)). That is the
better shape anyway: the out-of-process copy never moves the live session, so
unlike the in-place path it needs no restore afterwards and cannot strand the
RPC process on the fork.

**Models** — a separate `pi --mode rpc --no-session --no-skills
--no-prompt-templates --no-context-files` process answering
`get_available_models` and `get_state`. Extensions stay enabled because they can
register model providers. Ids are `provider/model` slugs and are validated as
such before launch.

Oh My Pi rejects unknown flags outright, so its probe is its own list —
`--no-session --no-skills --no-rules --no-extensions` — and the two describe
thinking differently. Pi maps levels through a per-model `thinkingLevelMap`; Oh
My Pi advertises the levels a model actually honors under `thinking.efforts`.
`off` never appears in that list because it bypasses provider mapping entirely,
yet it is always accepted, so it is added back
([model_catalog.rs](../crates/waku-core/src/model_catalog.rs)).

**Plan usage** — Oh My Pi only: `omp usage --json --no-extensions`
([usage.rs](../crates/waku-core/src/usage.rs)), queried off-thread for the
selected task's `<upstream>/<model>` slug. Reports are matched to the active
branch's `credential_pin` in the session file when multiple OAuth accounts are
configured, scoped to the active model family (e.g. Antigravity's Gemini vs
Claude/GPT pools, Codex's standard vs Spark lanes), and deduplicated across
`sharedGroup` routing copies. Unmetered API-key upstreams return `None` and omit
the plan section.

**Computer Use** — Pi only: `--extension <waku pi extension>` and
`--skill <SKILL.md>`, with the REPL and helper paths passed through the
environment. Waku's bridge is written against Pi's extension API, and Oh My Pi
ships its own `/computer` instead, so the flag is never passed to it.

---

## Claude Code

**Launch** — `claude -p --input-format stream-json --output-format stream-json
--verbose --include-partial-messages --replay-user-messages
--permission-prompt-tool stdio --permission-mode <mode>`
([driver/claude.rs](../crates/waku-core/src/driver/claude.rs)), plus `--model`, `--effort`,
and `--session-id` or `--resume`.

This is the transport the Claude Agent SDK's `query()` drives; the SDK is a
wrapper around these flags, not a separate capability, and there is no Rust SDK
to use instead. Both `--input-format stream-json` and `--permission-prompt-tool`
were verified against the real binary — **the latter is undocumented and absent
from `claude --help`**, and without it the CLI decides permissions itself and
only reports denials after the fact on `result`.

**Lifetime** — long-lived. One process serves the conversation, with turns fed
as newline-delimited user messages on stdin.

**Per turn** — write `{"type":"user","message":{"role":"user","content":[…]},
"parent_tool_use_id":null}`; the turn ends with a `result` message carrying
`is_error`, `stop_reason`, usage, and `permission_denials`.

**Inbound stream**

| Message | Becomes |
| --- | --- |
| `system` / `init` | the session id |
| `stream_event` → `text_delta`, `thinking_delta` | `TextDelta`, `ReasoningDelta` |
| `assistant` content blocks | `tool_use` → `RichActivity`; text and thinking only as a fallback when no delta of that kind streamed |
| `user` with `tool_result` | completes the matching activity |
| `user` with `isReplay: true` | ignored — Waku's own prompt echoed by `--replay-user-messages` |
| `result` | `TurnFinished` |
| `system` status/thinking-token notices, `rate_limit_event` | ignored |

**Approvals** — `control_request` / `subtype: "can_use_tool"` carries the tool
name, input, `tool_use_id`, the `blocked_path` that tripped the check, and
`permission_suggestions`. Waku answers with a `control_response` whose result is
`{"behavior":"allow"}` or `{"behavior":"deny","message":…}`. Outside Supervised it
answers allow itself.

**Cancel** — a `control_request` with `subtype: "interrupt"`.

**Steer** — the same user-message write as a prompt, sent while a turn is
running and without arming a new turn. The CLI holds the message and folds it
into the running turn at its next model call — one `result` still settles the
whole exchange, and the `isReplay` echo arrives at the moment of absorption
rather than at write time. Verified against the real CLI by injecting an
instruction while a Bash `sleep` ran: the same turn's reply honored it.

**Model changes** — a `control_request` with `subtype: "set_model"`, so switching
models keeps the session. The permission posture is a launch flag and still
restarts.

**Native checkpoints** — after each turn Waku reads Claude's own transcript at
`$CLAUDE_CONFIG_DIR/projects/**/<session>.jsonl`, walks the `parentUuid` chain to
find the active branch, and records the latest message uuid as the turn's
`provider_resume_at` ([claude_session.rs](../crates/waku-core/src/claude_session.rs)). That
per-turn checkpoint is what makes rewind and branch possible. Because Claude
accepts a caller-chosen `--session-id`, the cursor exists before the first turn
does.

**Rewind and branch** — `claude_session::fork_session_at` rewrites the JSONL
transcript into a *new* session file, truncated at the checkpoint and re-keyed
with fresh uuids; the returned id map is applied to Waku's retained turns.
Rewinding to turn zero clears the cursor and starts clean. The CLI also exposes
`--fork-session` (with `--resume`), which likely replaces this hand-rolled
rewrite — unverified, and the reason it is still hand-rolled is that the flag was
found after the fork code was written.

**Models** — the sessionless SDK `initialize` control response publishes the
same account- and configuration-aware list used by `/model`, including custom
routes resolved through CC Switch. Waku probes it in the background and caches
the last successful catalog; the curated list is only the startup/failure
fallback ([model_catalog.rs](../crates/waku-core/src/model_catalog.rs)).

---

## OpenCode background service

OpenCode 2 is the `opencode` command. Its 2.0 release replaced the
per-workspace `opencode serve` of OpenCode 1 with one background service per
user that serves every workspace, and the CLI, TUI and desktop app all share
it.

**Launch** — usually none. Waku adopts the service the user's own `opencode`
already started, found through its registration file
(`${XDG_STATE_HOME:-~/.local/state}/opencode/service.json`, opened read-only)
and confirmed with `GET /api/info`, whose `pid` and `version` must match the
registration
([opencode_service.rs](../crates/waku-core/src/opencode_service.rs)). Only
when none is registered does a session start run `opencode serve --service`,
which yields to a healthy incumbent, and poll the registration for up to 20 s.
An OpenCode 1 binary is refused before that spawn: its `serve` has no service
mode, would never register, and would be left running as an orphan.

**Protocol** — OpenCode's HTTP API, authenticated with HTTP Basic (`opencode`
and the registration's password), plus one server-sent event stream,
`GET /api/event`, that carries every session the service runs — the user's own
terminal sessions included
([opencode_api.rs](../crates/waku-core/src/opencode_api.rs)). Routes and
payloads were checked against the 2.0.22 OpenAPI document and schema sources.
Waku demultiplexes the stream by `data.sessionID`: a frame reaches only the
task that owns its session, and `tui.*` remote-control events are dropped.

**Lifetime** — the service is never Waku's; see
[How the long-lived processes actually die](#how-the-long-lived-processes-actually-die).
A stream that breaks reconnects and repairs each session against the server
rather than reporting the provider gone, and the reader keeps reconnecting —
never starting the service itself — for as long as a task is watching.

**Service restarts** — stopping the service mid-turn suspends the run rather
than ending it: the service keeps the run's claim and resumes it as it boots
again, with no `idle` marker in between, so OpenCode counts one turn. Waku
does the same. The turn stays open with a "Waiting for the OpenCode service"
row, the stream usually closes before any `shutdown` interruption reaches it,
and after the reconnect a run that is neither active nor followed by a marker
counts as suspended rather than finished; the resumed run then streams into
the same turn. A turn the user stops while the service is down cannot be
interrupted, so its run is interrupted once it resumes instead of appearing as
a turn of its own.

**Handshake** — subscribe first, then `POST /api/session` with a client-minted
`ses_…` id, `location.directory` (the canonical workspace path), the agent
(`build` unless a preset is chosen) and the model, so the session's first event
cannot arrive before Waku listens for it. A resume reads
`GET /api/session/{id}` instead.

**Per turn** — `POST /api/session/{id}/prompt` with `{text}`, answered with the
inbox entry it became. A registered `/command` goes to
`POST /api/session/{id}/command` with `{name, text}` instead, so the service
expands it itself.

**Steer** — the same prompt with `delivery: "steer"`; an entry the service
queued anyway is promoted with `PATCH /api/session/{id}/inbox/{inboxID}`. The
steered message joins the running execution, so its one outcome still settles
the turn, and a `session.inbox.cancelled` for it falls back to the app's own
follow-up queue.

**Inbound stream**:

| Event | Becomes |
| --- | --- |
| `session.execution.started` | `TurnStarted` |
| `session.text.delta`, `session.reasoning.delta` | `TextDelta`, `ReasoningDelta`; text and reasoning share one ordinal namespace per assistant message |
| `session.tool.input.started` through `session.tool.success` / `session.tool.failed` | `RichActivity`, opened while the arguments stream and kept through `session.tool.progress` |
| `session.step.ended` | `UsageUpdated` from that request's tokens, against `/api/model`'s context window |
| `session.execution.succeeded` / `failed` / `interrupted` | `TurnFinished` — the only settle point, since the service never publishes `session.idle`; a `shutdown` interruption suspends the turn instead |
| `permission.asked` | `Permission` |
| `form.created` | `UserInputRequested`, one question per form field |
| `session.shell.started` / `ended` | `BackgroundWork` |
| `session.compaction.*` | `CompactionUpdated`, an independent transcript milestone |
| `session.retry.scheduled`, a retrying `session.status` | one upserted activity row each |
| `session.deleted` | `Error`, then `ProcessExited` |

**Approvals** — `POST /api/session/{id}/permission/{requestID}/reply` with
`{decision: "once" | "reject"}`. `always` never goes on the wire: it would
write a rule into `/api/permission/saved`, a global store shared with the
user's own OpenCode, so a durable choice stays in the driver and every reply is
one-shot. The auto modes answer `once`.

**Forms** — `POST /api/session/{id}/form/{formID}/reply` with an answer keyed
by each field's own `key`.

**Cancel** — `POST /api/session/{id}/interrupt`.

**Rewind and branch** — `POST /api/session/{id}/fork` with `{before}` naming
the user message that opens the first removed turn, or no `before` to copy the
whole history. A user message is not a turn: a steer is stored as a `user`
message of its own, so turns are delimited by the `idle` transcript messages
the service writes whenever an execution settles, OpenCode's own rule in
`session/diff.ts`. History imported from OpenCode 1 carries no markers; there
a message joined the running turn when the step before it handed back to
tools, or had not completed yet when the message was written. Live rewinds,
cold task forks and transcript import all count turns this way
([opencode_session.rs](../crates/waku-core/src/opencode_session.rs)).

**OpenCode 1 history** — OpenCode 2's first start imports OpenCode 1 sessions
into the same database under their original ids, in the background. A cursor
written for OpenCode 1 therefore resumes as it is: when its session is still
missing while `GET /api/experimental/migration/v1` reports `running`, the
resume waits up to 60 s for the import to reach it rather than starting an
empty session under its id.

**Cold paths** — the Resume picker, transcript import
(`GET /api/experimental/session/{id}/export`), cold forks and the model and
agent catalogs attach to a running service but never start one, so with no
service running the model picker keeps its cached catalog.

**Computer Use** — a runtime-only MCP registration per workspace and a session
instruction entry; see [computer-use.md](computer-use.md).

---

## Access modes across providers

Waku's `RuntimeMode` (Supervised / Auto-accept edits / Auto / Full access)
maps into each CLI's own vocabulary.

| Waku | Codex (`approvalPolicy` / `sandbox` / reviewer) | Claude `--permission-mode` | OpenCode |
| --- | --- | --- | --- |
| Supervised | `untrusted` / `read-only` / `user` | `default` + `can_use_tool` reaches the user | permission requests reach the user |
| Auto-accept edits | `on-request` / `workspace-write` / `user` | `acceptEdits` | edits auto-answered (`once`), the rest reach the user |
| Auto | `on-request` / `workspace-write` / `auto_review` | `auto` | auto-answered (`once`) |
| Full access | `never` / `danger-full-access` / `user` | `bypassPermissions` + `--dangerously-skip-permissions` | auto-answered (`once`) |

Pi and Oh My Pi accept Full access only and always run wide open
(`--approve`, `--yolo`).

Every provider except those two distinguishes Supervised from the auto modes
in a way the user can actually answer. They decide by launch flag, so
"Supervised" degrades there to whatever the CLI does without a human at the
terminal — for Pi because it has no permission system to ask with, and for Oh My Pi
because `--yolo` bypasses the one it has. Only the last of those is Waku's own
limitation rather than the CLI's.

## Resume cursors

`ProviderResumeCursor` ([model.rs](../crates/waku-protocol/src/model.rs)) is
persisted with the session and is what makes a Waku task outlive its process:

| Provider | Cursor fields | Why |
| --- | --- | --- |
| Codex | `thread_id` | `thread/resume` |
| Pi | `session_id`, `session_file` | `switch_session` needs the path |
| Oh My Pi | `session_id`, `session_file` | same, plus `--fork <file>` for a whole-session copy |
| Claude | `session_id`, `resume_at` | `resume_at` is the transcript message uuid used for forking |
| OpenCode | `session_id`, `directory` | `GET /api/session/{id}` / service fork; `directory` is the workspace string the service stored |

A cursor from the wrong provider is rejected at driver start rather than
silently ignored.

## Compared with T3 Code

[T3 Code](https://github.com/pingdotgg/t3code) solves the same problem with drivers
registered in `apps/server/src/provider/builtInDrivers.ts` and documented in its
own `docs/internals/providers.md`.

**Its one structural difference: no provider is a per-turn process.** They
hold a long-lived session; the transport differs, the lifetime does not.

| Provider | T3 Code transport | Waku transport |
| --- | --- | --- |
| Codex | `codex app-server` JSON-RPC (`packages/effect-codex-app-server`) | same |
| Claude | `@anthropic-ai/claude-agent-sdk` `query()` with an `AsyncIterable` prompt queue | same protocol, spoken directly — the SDK is a wrapper around these flags |
| OpenCode | long-lived `opencode serve` + HTTP SDK | the user's shared OpenCode 2 background service, adopted rather than started per workspace |

**They all hold a long-lived session**, and Claude reaches the same place
without the SDK: there is no Rust Agent SDK, but the SDK is a wrapper around the
`claude` CLI's own streaming-input protocol, which Waku speaks directly. No Node
sidecar and no npm dependency.

Waku goes further: Pi and Oh My Pi, which T3 Code does not
support, are long-lived here too. Every provider holds a session.

What the long-lived session buys, and what Waku pays for not having it:

| Capability | T3 Code | Waku |
| --- | --- | --- |
| Interactive approvals | Claude via the SDK's `canUseTool` (including `AskUserQuestion` and `ExitPlanMode`), Codex via `*requestApproval*` | Every provider except Pi, which does not expose a request to answer |
| Interrupt | `session/cancel`, `query.interrupt()` (plus `stopTask()` for runaway subagents) | Protocol interrupt everywhere |
| Change model mid-session | `capabilities.sessionModelSwitch: "in-session"` → `session/set_model`, `query.setModel()` | Every transport keeps the session |
| Mid-turn prompt | Queued into the live agent loop as a **steer**, same turn | Steered into the live turn on every provider (`⌘↩`); plain `Enter` queues a visible, editable follow-up instead |
| Native rollback | `rollbackThread` on the adapter contract | Codex/Pi natively; the rest emulated out-of-band by the `*_session.rs` helpers |
| Idle cleanup | `ProviderSessionReaper` stops sessions idle 30 min, swept every 5 min, skipping threads with an active turn | same, on the same thresholds |

The adapter contract itself is wider than `DriverControl`:
`startSession` / `sendTurn` / `interruptTurn` / `respondToRequest` /
`respondToUserInput` / `stopSession` / `listSessions` / `hasSession` /
`readThread` / `rollbackThread` / `stopAll` / `streamEvents`, plus a declared
`capabilities` record. Waku's equivalent surface is split between
`DriverControl` and the out-of-band `*_session.rs` helpers, which is why
capabilities like "can this provider fork?" live on `ProviderKind` rather than on
the driver that would have to implement them.

Note the parts that are *not* a gap. Waku's Codex path is the same app-server
protocol against the same methods. Both projects normalize provider events into
one canonical activity/event stream that the UI consumes provider-agnostically.
Both keep a per-session resume cursor and both had to special-case Claude's
transcript uuid as a rewind checkpoint.

## Adding a provider

1. Add the variant to `ProviderKind`
   ([model.rs](../crates/waku-protocol/src/model.rs)) with `id`,
   `display_name`, `short_name`, `command`, and the capability predicates. The
   compiler's non-exhaustive-match errors are the reliable to-do list for
   everything that follows.
2. Add a `ProviderResumeCursor` variant carrying whatever resume actually needs
   (an id is often not enough — see Pi's session file and Claude's message uuid).
3. Pick a transport, and look hard before settling for the one-shot path. Ask
   whether the CLI serves an HTTP API or has a persistent RPC mode; check
   what persistent interface the CLI provides. Route the choice in `driver::start`.
4. Map its stream onto `DriverEvent` and its tools onto `ActivityKind`. **Read
   the payloads off a live provider** — every driver here was written from a
   probe transcript or an OpenAPI document, and the two bugs that reached code
   anyway (a dead event subscription, a discarded permission reason) were both
   caught by running a real turn rather than by unit tests. Preserve ordering,
   and never leak private control markers into the transcript. If the transport
   accepts user messages mid-turn, probe *which* behavior it has before wiring
   `supports_steer`: inject an instruction while a slow tool runs and count the
   turn completions. Claude folds a plain message into the running turn;
   OpenCode joins it to the running execution when the prompt says
   `delivery: "steer"`.
5. Map the access modes. If the transport can ask the user, route
   Supervised to a real `Permission` event; if it cannot, pick the safe
   degradation and say so in a comment at the call site.
6. Add an `#[ignore]`d integration test that drives the real provider through the
   driver, as `opencode.rs` does. It is the only check that catches a
   transport wired to nothing.
7. Implement rewind and branch, or emulate them the way Claude and OpenCode do.
   Native truncation is preferable; seeding a fresh session with retained history
   is the fallback.
8. **Do not trust a clean stop reason.** Probe what the provider does when the
   turn cannot run at all: an expired plan, a spent quota, a rejected key.
   Where the cause is recoverable, recover it; where it is not, at least do not
   report success for a turn that produced nothing.
9. Wire model discovery in `model_catalog.rs`, plus a fallback list for when the
   binary is missing or the command fails.
