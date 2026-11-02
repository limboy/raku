# Provider integrations

How Waku talks to each coding agent: the process it launches, the wire protocol
it speaks, how long that process lives, and what has to be emulated because the
CLI does not offer it.

How each of them names a session — which are read from the provider, which are
polled off disk, and the one Waku generates itself — is in
[titles.md](titles.md).

Every provider is reached through the same driver abstraction in
[driver/mod.rs](../crates/waku-core/src/driver/mod.rs). There are four
transport implementations behind four providers, and **every one of them holds a
session that spans the whole conversation**:

| Transport | File | Providers |
| --- | --- | --- |
| Codex app-server (JSON-RPC over stdio) | [driver/codex.rs](../crates/waku-core/src/driver/codex.rs) | Codex CLI |
| Pi RPC mode (NDJSON request/response over stdio) | [driver/pi.rs](../crates/waku-core/src/driver/pi.rs) | Pi |
| Claude streaming-input session (NDJSON over stdio) | [driver/claude.rs](../crates/waku-core/src/driver/claude.rs) | Claude Code |
| Antigravity streaming-input session (NDJSON over stdio) | [driver/antigravity.rs](../crates/waku-core/src/driver/antigravity.rs) | Antigravity CLI |

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
    provider_cursor,
}
```

Outputs ([model.rs:973](../crates/waku-core/src/model.rs)): `Connected`,
`AvailableCommands`, `TurnStarted`, `TextDelta`, `ReasoningDelta`, `Activity`,
`RichActivity`, `Permission`, `SteerAccepted`, `SteerRejected`,
`TurnFinished`, `Error`, `ProcessExited`.

A transport that can inject a user message into the *running* turn advertises
it through `DriverControl::capabilities().steer` and delivers it with `steer`; the
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

### Capability ownership

[capabilities.rs](../crates/waku-protocol/src/capabilities.rs) separates three
sources of truth:

- `ProviderKind::capabilities()` is the exhaustive declaration of Waku's
  provider-level support: interactive access modes, conversation actions,
  compaction, delegation parent/child roles, model discovery and goals.
  UI visibility, command discovery and daemon validation read this declaration.
- `ProviderModel` describes model-specific reasoning efforts, service tiers and
  context windows discovered from the provider. An empty option list hides its
  control; provider capabilities do not duplicate these model options.
- `DriverControl::capabilities()` returns `RuntimeCapabilities` for the live
  transport. Start and attach responses carry this snapshot as `capabilities`,
  including `steer`; clients never infer runtime support from a provider id.
  This wire shape is protocol version 15.

All lookups read in-memory data. Declaring conversation rollback/fork means
Waku offers the action, including native session helpers; it does not promise
the transport itself can perform it.

The access menu lists only the provider's supported modes. A mode inherited
from another provider or restored from disk is preserved: if incompatible, the
chip marks it unavailable and sending explains the supported choices before
consuming the draft. The daemon also validates starts before replacing a live
runtime and rejects unsupported options/commands before delivery. Review
subtasks use their own execution policy, so Pi's read-only tool
allowlist remains available independently of their interactive access modes.
Rewind-and-resend validates the selected interactive mode before changing
history or Git. Pure history forks may borrow a supported launch mode in a
temporary transport; that transport is closed when the operation ends, and
the source and forked tasks keep their selected interactive mode.

### Context compaction

Compaction belongs to the provider's native session. Waku offers a manual
`/compact` command, a usage-panel action, and a command-palette action for
Codex, Claude Code, Antigravity CLI, and Pi. Pi
and Claude accept optional focus instructions; Codex and Antigravity CLI
reject that argument instead of silently discarding it. Other
providers keep their native command handling.

The driver contract carries `CompactionRequest` and complete
`CompactionUpdated` snapshots. Manual requests use Codex's
`thread/compact/start`, Pi's
`compact` RPC, or Claude's native `/compact` user message. An acknowledgement
is not completion. Codex's `contextCompaction` items,
Pi's compaction events/results (including older
`auto_compaction_*` events), and Claude's status/compact-boundary messages
supply the lifecycle. A failed or disconnected operation terminalizes too.

Each operation has its own stable identity.
Summary deltas accumulate in the driver before it emits a snapshot; tool-output
snapshots keep their replacement semantics. Cold import retains
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
| The user stops a turn, **Codex and Antigravity** | [src/app/sessions.rs:3](../src/app/sessions.rs#L3) |
| The provider changes, or an option changes that the transport cannot apply in session | `apply_session_options`, [src/app/runtime.rs](../src/app/runtime.rs) |
| The session is deleted | [src/app/sessions.rs:178](../src/app/sessions.rs#L178) |
| A rewind or branch leaves the driver on a stale native session | [src/app/runtime.rs](../src/app/runtime.rs) |
| The driver reports `ProcessExited` (the handler returns `false`, so the runtime is not reinserted) | [src/app/streaming.rs:352](../src/app/streaming.rs#L352) |
| Nobody has touched the session for 30 minutes | `reap_idle_sessions`, [src/app/runtime.rs](../src/app/runtime.rs) |
| Waku quits | `cx.quit()` |

Stop drops the runtime for Codex (whose app-server owns its provider process
tree) and Antigravity CLI (whose `stream-json` loop flushes an `interrupted`
result and exits on `SIGINT`); the next prompt resumes the native thread or
conversation (`thread/resume` / `--conversation <id>`). Every other provider has
a protocol interrupt and keeps its runtime (`retain_runtime_after_cancel`).

Option changes go through `DriverControl::apply_options`, which returns whether
the transport absorbed the change or wants to be restarted:

| Change | Codex | Pi | Claude | Antigravity |
| --- | --- | --- | --- | --- |
| Model, reasoning effort, service tier | in session — they ride on every `turn/start` | in session — `set_model`, `set_thinking_level` | in session — a `set_model` control request | restart (`--conversation <id>`) |
| Access mode | restart | restart | restart | restart (`--conversation <id>`) |
| Provider | restart | restart | restart | restart |

The permission policy is deliberately excluded even for Codex, which does carry
`approvalPolicy` and `sandboxPolicy` on every `turn/start`: loosening or
tightening what an already-running agent may touch deserves a fresh thread. T3
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

## At a glance

| | Codex CLI | Pi | Claude Code | Antigravity CLI |
| --- | --- | --- | --- | --- |
| Binary | `codex` | `pi` | `claude` | `agy` |
| Wire protocol | JSON-RPC over stdio | NDJSON RPC over stdio | stream-json over stdio | stream-json over stdio |
| Process spans the whole session | yes | yes | yes | yes |
| Process spawned per turn | no | no | no | no |
| Bidirectional | yes | yes | yes | prompts only |
| Reasoning stream | yes | yes | yes | token count only |
| Interactive approvals | yes | no | yes | no (headless policy) |
| Mid-turn steering | yes | yes | yes | no (follow-up queue) |
| Model discovery | yes | yes | yes | yes |
| Restricted to Full access | no | yes | no | no |
| Rewind and branch at a turn | yes | yes | yes | no |

Every provider now holds a session across turns. That was not true when this
document was first written: five of the seven spawned a process per prompt, and
everything stateful — resume, rewind, branch, approvals — had to be reconstructed
from a session id, an on-disk transcript, or a side-channel. In each case the CLI
turned out to already serve a session protocol; nobody had looked.

---

## Codex CLI

**Launch** — `codex app-server --stdio`
([driver/codex.rs:164](../crates/waku-core/src/driver/codex.rs#L164)).

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
3. `thread/start` or `thread/resume` (id `1`) with `cwd`, `approvalPolicy`,
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

**Image generation** — native `imageGeneration` items become ordered,
standalone image rows. The provider keeps control of model/account capability
and feature enablement. Waku handles both base64 `result` and the newer saved
file output (`fileId` / `savedPath`), copies the bytes into its blob store on
the daemon worker, and sends short references to the desktop. Generation
failures and image quota exhaustion remain visible. Completed images stay
outside folded work and open the attachment preview by click or Enter/Space.
Native history import retains their original message boundaries too. Other
providers can use their own image-generation tools; recognized image tools
share the same image row and output handling.

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

---

## Pi

Pi speaks an NDJSON request/response protocol over stdio ([pi.rs](../crates/waku-core/src/driver/pi.rs)):

**Launch** — `pi --mode rpc --approve` with `PI_SKIP_VERSION_CHECK=1`.

**Protocol** — NDJSON over stdio, request/response rather than JSON-RPC:
Waku stamps each request with a string id (`waku-<n>`) and Pi answers with
`{"type": "response", "id", "success", "data"}`. Everything else on the stream
is an unsolicited event. Requests are issued synchronously by the writer thread
with a 10 s timeout ([pi.rs](../crates/waku-core/src/driver/pi.rs));
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

**Inbound stream** ([pi.rs](../crates/waku-core/src/driver/pi.rs)):

| Event | Becomes |
| --- | --- |
| `agent_start`, `turn_start` | `TurnStarted` (once per run) |
| `message_update` → `text_delta` / `thinking_delta` | `TextDelta` / `ReasoningDelta` |
| `message_end` | fallback text/thinking when no delta was streamed |
| `tool_execution_start` / `_update` / `_end` | `RichActivity` |
| `auto_retry_end` | clears or sets the failure flag |
| `agent_settled` | `TurnFinished`, then resets stream state |
| `extension_ui_request` | auto-cancelled — Waku has no UI for extension prompts |

**Access modes** — Full access only, enforced at driver start rather than
degraded silently: any other selection fails with "currently supports Full
access only". Pi has no permission system at all, so `--approve` is the whole story.

**Cancel** — `{"type": "abort"}`.

**Steer** — `{"type": "steer", "message": …}`; the request acknowledgment
resolves to `SteerAccepted` or `SteerRejected`.

**Rewind and branch** — both go through `get_fork_messages` → `fork {entryId}`
or `clone` when nothing is removed, then `get_state`. Rewind adopts the fork
as the session's new cursor. Branch additionally `switch_session`es back to the
source file and verifies it landed on the right session; if that restore fails
the runtime is dropped, because the RPC process may still be sitting on the fork
([runtime.rs](../src/app/runtime.rs)).

**Models** — a separate `pi --mode rpc --no-session --no-skills
--no-prompt-templates --no-context-files` process answering
`get_available_models` and `get_state`. Extensions stay enabled because they can
register model providers. Ids are `provider/model` slugs and are validated as
such before launch. Pi maps reasoning levels through a per-model `thinkingLevelMap`.

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

## Antigravity CLI

**Launch** — `agy --input-format stream-json --output-format stream-json
--print-timeout 0` ([driver/antigravity.rs](../crates/waku-core/src/driver/antigravity.rs)),
plus `--dangerously-skip-permissions` (`FullAccess`), `--mode auto` (`Auto`), or
`--mode accept-edits` (`AutoAcceptEdits`), resolved `--model` and `--effort`,
optional `--agent`, and `--conversation <id>` when resuming.

**Lifetime** — long-lived across normal turns and manual `/compact` runs; ends
by closing `stdin` (EOF exits `0`).

**Per turn** — write `{"event":"user","message":{"content":"…"}}` on `stdin`;
the stream emits one `init` frame per process, any number of `step_update`
frames (`user_input`, `agent_response`, `tool`, `checkpoint`), and one `result`
frame per turn. Context occupancy comes from the active `agent_response` step's
`input_tokens + cache_read_tokens + output_tokens` rather than `result.usage`,
which accumulates across the whole session.

**Approvals and option changes** — `agy` rejects `control_request` and `/model`
inside `--input-format stream-json` (exit code `2`), and unapproved tools in
headless mode are soft-denied rather than prompted. `apply_options` therefore
requests a runtime restart on mode, model, or effort changes and resumes via
`--conversation <id>`.

**Cancel** — `SIGINT` to the isolated process group, which makes `agy` flush the
partial turn with `finished_reason: "interrupted"` and exit; Waku drops the
runtime and resumes `--conversation <id>` on the next prompt.

**Models, skills, plan usage, and native history** — `agy --output-format json
models` and `--print /model` populate the model catalog (collapsing multi-effort
`-<effort>` slugs while preserving singletons so `--model` and `--effort` never
conflict); `--print /skills` populates skill autocomplete; `--print /usage`
supplies 5-hour and weekly plan quotas scoped to the active model family; and
[antigravity_session.rs](../crates/waku-core/src/antigravity_session.rs) reads
`~/.gemini/antigravity-cli/conversation_summaries.db` and
`conversations/<id>.db` in read-only SQLite mode for `/resume` discovery, cold
turn import, and background title polling.

---

## Access modes across providers

Waku's `RuntimeMode` (Supervised / Auto-accept edits / Auto / Full access)
maps into each CLI's own vocabulary.

| Waku | Codex (`approvalPolicy` / `sandbox` / reviewer) | Claude `--permission-mode` |
| --- | --- | --- |
| Supervised | `untrusted` / `read-only` / `user` | `default` + `can_use_tool` reaches the user |
| Auto-accept edits | `on-request` / `workspace-write` / `user` | `acceptEdits` |
| Auto | `on-request` / `workspace-write` / `auto_review` | `auto` |
| Full access | `never` / `danger-full-access` / `user` | `bypassPermissions` + `--dangerously-skip-permissions` |

Pi accepts Full access only and always runs wide open (`--approve`).

Codex and Claude can route Supervised permission requests to Waku.
Antigravity selects its mode through launch flags but cannot prompt through its
headless stream-json transport; unapproved tools are soft-denied instead. Pi
has no permission system.

## Resume cursors

`ProviderResumeCursor` ([model.rs](../crates/waku-protocol/src/model.rs)) is
persisted with the session and is what makes a Waku task outlive its process:

| Provider | Cursor fields | Why |
| --- | --- | --- |
| Codex | `thread_id` | `thread/resume` |
| Pi | `session_id`, `session_file` | `switch_session` needs the path |
| Claude | `session_id`, `resume_at` | `resume_at` is the transcript message uuid used for forking |

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

**They all hold a long-lived session**, and Claude reaches the same place
without the SDK: there is no Rust Agent SDK, but the SDK is a wrapper around the
`claude` CLI's own streaming-input protocol, which Waku speaks directly. No Node
sidecar and no npm dependency.

Waku goes further: Pi, which T3 Code does not
support, is long-lived here too. Every provider holds a session.

What the long-lived session buys, and what Waku pays for not having it:

| Capability | T3 Code | Waku |
| --- | --- | --- |
| Interactive approvals | Claude via the SDK's `canUseTool` (including `AskUserQuestion` and `ExitPlanMode`), Codex via `*requestApproval*` | Codex and Claude route requests into Waku |
| Interrupt | `session/cancel`, `query.interrupt()` (plus `stopTask()` for runaway subagents) | Protocol interrupt everywhere |
| Change model mid-session | `capabilities.sessionModelSwitch: "in-session"` → `session/set_model`, `query.setModel()` | Every transport keeps the session |
| Mid-turn prompt | Queued into the live agent loop as a **steer**, same turn | `⌘↩` steers where the runtime reports support; otherwise queues a follow-up. Plain `Enter` queues a visible, editable follow-up |
| Native rollback | `rollbackThread` on the adapter contract | Codex/Pi natively; the rest emulated out-of-band by the `*_session.rs` helpers |
| Idle cleanup | `ProviderSessionReaper` stops sessions idle 30 min, swept every 5 min, skipping threads with an active turn | same, on the same thresholds |

The adapter contract itself is wider than `DriverControl`:
`startSession` / `sendTurn` / `interruptTurn` / `respondToRequest` /
`respondToUserInput` / `stopSession` / `listSessions` / `hasSession` /
`readThread` / `rollbackThread` / `stopAll` / `streamEvents`, plus a declared
`capabilities` record. Waku's equivalent surface is split between
`DriverControl` and the out-of-band `*_session.rs` helpers, which is why
capabilities like "can Waku fork this provider's history?" live in
`ProviderKind::capabilities().conversation`, while transport support lives in
`RuntimeCapabilities`.

Note the parts that are *not* a gap. Waku's Codex path is the same app-server
protocol against the same methods. Both projects normalize provider events into
one canonical activity/event stream that the UI consumes provider-agnostically.
Both keep a per-session resume cursor and both had to special-case Claude's
transcript uuid as a rewind checkpoint.

## Adding a provider

1. Add the variant to `ProviderKind`
   ([model.rs](../crates/waku-protocol/src/model.rs)) with `id`,
   `display_name`, `short_name` and `command`. Add its explicit declaration to
   `ProviderKind::capabilities()` in
   [capabilities.rs](../crates/waku-protocol/src/capabilities.rs). The
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
   `RuntimeCapabilities::steer`: inject an instruction while a slow tool runs
   and count the turn completions. Claude folds a plain message into the running turn.
5. Map the access modes. If the transport can ask the user, route
   Supervised to a real `Permission` event; if it cannot, pick the safe
   degradation and say so in a comment at the call site.
6. Add an `#[ignore]`d integration test that drives the real provider through the
   driver. It is the only check that catches a
   transport wired to nothing.
7. Implement rewind and branch, or emulate them the way Claude does.
   Native truncation is preferable; seeding a fresh session with retained history
   is the fallback.
8. **Do not trust a clean stop reason.** Probe what the provider does when the
   turn cannot run at all: an expired plan, a spent quota, a rejected key.
   Where the cause is recoverable, recover it; where it is not, at least do not
   report success for a turn that produced nothing.
9. Wire model discovery in `model_catalog.rs`, plus a fallback list for when the
   binary is missing or the command fails.
