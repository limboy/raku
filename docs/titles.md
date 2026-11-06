# Session titles

How each session in the sidebar gets its name: who writes the title, how Raku
learns about it, and when.

The governing rule is that **the provider names its own session wherever it
can**. Raku reads those native titles and explicitly requests native naming
when a headless transport does not enable it automatically. Oh My Pi, for
example, enables automatic titles in its TUI but not RPC. Codex is the one
provider for which Raku runs its own title-generation turn.

## Title language

Automatic titles follow the predominant language and script of the user's
request. Mixed Chinese/English requests follow the user's prose rather than
English code, paths, technical identifiers, or background instructions;
technical terms can retain their original form.

Codex receives this policy in its title turn's system instructions. For
Claude and Pi, Raku appends the same naming context to the first ordinary
prompt whose language can be identified in each live driver. Language detection
uses a bounded sample of the original prose, excluding code blocks, inline code,
quoted background, paths, and identifiers. CJK characters are compared with Latin words rather
than Latin letters, so long English technical terms do not outweigh Chinese
prose. The context names the detected language explicitly, so the context's
own English wording cannot determine the title language. Uncertain input
keeps native behavior and leaves the context available for a later request.
This reaches native title generators that summarize that message,
including Oh My Pi's explicit native `/rename` request. Pi still relies on
its prompt fallback or an installed naming extension; it does not gain a
separate model call. Providers continue to own generation and model selection.

The context is added only to the provider transport. Raku's typed prompt is
unchanged, and cold imports hide the exact context suffix using display text
while retaining the provider's original message content. The context uses
bracketed markers: Oh My Pi strips paired XML blocks, including their contents,
before its title model sees a request. Legacy XML context remains hidden on
import and is upgraded if a prepared request is retried. Later prompts,
steering, slash commands (including explicit renames), Codex's main turn, and
review subtasks do not receive the context.

## Title icons

Automatic titles are plain text in Raku. Leading emoji and Nerd Font icons are
removed when a provider title arrives; existing stored automatic titles use the
same cleanup when displayed. Whole emoji sequences are removed, including skin
tones, joined emoji, flags, and variation selectors. Text after the icons,
including Oh My Pi's short code and emoji within the title body, is preserved.
Explicit user titles and provider-native session files are unchanged.

## The two title fields

[`AgentSession`](../crates/raku-protocol/src/model.rs) carries two:

| Field | Owner | Set by |
| --- | --- | --- |
| `title` | the user | inline rename; wins whenever it differs from `DEFAULT_TITLE` |
| `auto_title` | the provider | `DriverEvent::AutoTitleUpdated`, and the local fallback below |

[`display_title`](../crates/raku-protocol/src/model.rs#L940) resolves them:
an explicit `title` first, then `auto_title`, then `DEFAULT_TITLE`
(`"New task"`, [model.rs:841](../crates/raku-protocol/src/model.rs#L841)).
A provider title therefore never overwrites a name the user typed.

### The local fallback

[`set_title_from_prompt`](../crates/raku-protocol/src/model.rs#L964) takes the
**first seven words** of the first prompt, capped at 54 characters, and writes
them into `auto_title`. It is called once per session from
[runtime.rs:2949](../src/app/runtime.rs#L2949) and no-ops if the session
already has a second message, a user title, or any `auto_title`.

This is a placeholder, not a title. It shares the `auto_title` field precisely
so the real one replaces it silently when it arrives — which makes **latency
the thing that matters**. A provider title that lands after the turn ends is,
from the user's side, indistinguishable from no title at all: they stare at
truncated prompt text for the entire run. Every path below is judged on how
fast it replaces that placeholder, not merely on whether it eventually does.

## The three delivery shapes

Every provider funnels into `DriverEvent::AutoTitleUpdated(Option<String>)`,
consumed once in [streaming.rs:258](../src/app/streaming.rs#L258) →
`set_auto_title`, which trims, removes leading icons, maps empty to `None`, and
is the universal last-stage normalizer. `AutoTitleUpdated` is in the `force_save` set
([runtime.rs](../src/app/runtime.rs)), so a title persists the moment it lands.

What differs is how the title reaches that event:

1. **Pushed on the provider's own stream.** The agent sends a title event and
   the driver forwards it. Nothing to poll, nothing to schedule. Codex's native
   paths work this way; current Claude responds to an explicit native title
   control request. Pi also pushes name changes, but its built-in rename
   support does not generate a name automatically.
2. **Polled from a native store.** The agent writes the title to disk (or to a
   store reachable by CLI) but never puts it on the wire. The driver polls with
   [`NativeTitleRefresh`](../crates/raku-core/src/driver/title_refresh.rs) on a
   backoff schedule, latching `RESOLVED` on the first hit. Older Claude versions
   work this way.
3. **Generated by Raku.** Only Codex.

### `NativeTitleRefresh`

A single `AtomicU8` — `IDLE` / `RUNNING` / `RESOLVED`. `start()` spawns a named
thread that walks a `Vec<Duration>`, sleeping each delay before its lookup, and
on the first non-empty result emits the event and latches `RESOLVED` for the
life of the driver. A schedule that runs dry returns to `IDLE`, so the next
trigger re-arms it. Concurrent `start()` calls are no-ops.

The schedule is the whole design decision: it must be dense enough to catch the
title near when the provider writes it, and long enough to survive a slow start.

## Per provider

| Provider | Author | Transport | Trigger | Emit site |
| --- | --- | --- | --- | --- |
| Claude Code | Claude's native title generator | `generate_session_title` control response; transcript JSONL fallback | first ordinary prompt in a new interactive session; polling and turn-end reads for compatibility | [claude.rs](../crates/raku-core/src/driver/claude.rs) |
| Codex CLI | **Raku** (`gpt-5.6-luna`, effort `low`) | second `codex app-server` process | first `turn/started`, new threads only | [codex.rs:636](../crates/raku-core/src/driver/codex.rs#L636) |
| Codex CLI | Codex | JSON-RPC stream | thread start/resume, `thread/name/updated` | [codex.rs:1612](../crates/raku-core/src/driver/codex.rs#L1612), [codex.rs:1735](../crates/raku-core/src/driver/codex.rs#L1735) |
| Pi | user or extension; no built-in automatic generator in installed 1.4.0 | NDJSON stream | connect, explicit `session_info_changed` | [pi.rs](../crates/raku-core/src/driver/pi.rs) |

### Claude Code

Claude owns title generation and model selection. On CLI **2.1.316**, direct
`claude -p` sessions do **not** automatically generate a title. The current
[session documentation](https://code.claude.com/docs/en/sessions#name-your-sessions)
confirms this distinction from interactive CLI sessions. Merely polling the
transcript therefore leaves Raku showing its prompt fallback indefinitely.

Raku explicitly requests native generation after writing the **first ordinary
prompt of a new interactive session**, using the same live process:

```json
{"type":"control_request","request_id":"raku-session-title","request":{"subtype":"generate_session_title","description":"<prompt including naming context>","persist":true}}
```

The control request runs alongside the main turn. Its successful response
contains the title at `response.response.title`; Raku immediately forwards it
as `AutoTitleUpdated`. `persist: true` also saves a native `ai-title` entry.
There is no extra user message or assistant turn, no second CLI, and no
Raku-selected title model. Slash commands do not consume the request; later
prompts, steering, resumed sessions, and review subtasks do not request one.
Blank results and unsupported-control errors are silent and preserve the
prompt fallback. The reader deduplicates the response against later native
metadata reads.

Verified through the real CLI **2.1.316** with the configured DeepSeek
V4.1 Flash 1017 gateway model: the main turn completed at **~1.9s**, the Chinese
title response arrived at **~2.0s**, and the transcript contained an `ai-title`.
The CLI selected that gateway model for its title request as well. Native
Anthropic configurations normally use the small/fast (Haiku) tier; Raku does
not override gateway mappings or `ANTHROPIC_DEFAULT_HAIKU_MODEL`.

**Older CLI behavior.** Verified against 2.1.228 with `--debug api`: the run
automatically issued two requests within the same millisecond, one for the
turn and one for the title.

```
[API:timing] dispatching to firstParty model=claude-haiku-4-5-20251001
[API:timing] dispatching to firstParty model=claude-opus-5
[API REQUEST] /v1/messages … source=generate_session_title
[API REQUEST] /v1/messages … source=sdk
```

That version's title call fired in parallel with the turn's own first model
call. Measured end to end, it reached disk **~2.9s after the prompt**.

The model is the CLI's Haiku tier, and setting `ANTHROPIC_DEFAULT_HAIKU_MODEL`
moves the title call with it — verified by running with
`ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-sonnet-5`, which dispatched
`generate_session_title` to Sonnet while the turn stayed on Opus.

**Claude does not title every session.** On 2.1.228, a provider-facing first
message shorter than **10 characters** never fired `generate_session_title`:
9 characters fired nothing, 10 fired the request. This threshold applies to
the transport text, including Raku's naming context, rather than only the
typed request. CLI versions and settings can also suppress native naming.
When no title arrives, Raku keeps the user's prompt fallback in its original
language; a missing cosmetic title must not fail the turn.

The older automatic path does not put its title on the stream. It writes to
the native transcript, `~/.claude/projects/*/<session_id>.jsonl` (or under
`$CLAUDE_CONFIG_DIR`), as a standalone entry:

```json
{"type":"ai-title","aiTitle":"Refactor sidebar row builder allocations","sessionId":"…"}
```

[`claude_session::session_metadata`](../crates/raku-core/src/claude_session.rs#L27)
returns the last `ai-title` (`aiTitle`) or `custom-title` (`customTitle`) entry.
`custom-title` is the explicit one — a rename inside Claude Code, or the title
Raku writes when it forks a session — and it wins by being later in the file.

Raku retains two transcript reads for compatibility and later retitles:

- [`start_claude_title_refresh`](../crates/raku-core/src/driver/claude.rs#L124),
  armed from the writer thread on **every prompt**, polling at `5s, 10s` —
  attempts land at 5s and 15s, since each delay is slept before its lookup.
  The ~2.9s write is already there on the first look; the second is insurance
  for a slow start.
- The turn-end read at [claude.rs:1295](../crates/raku-core/src/driver/claude.rs#L1295),
  on the `result` message, deduplicated against `last_auto_title`. This one
  also carries the rewind cursor, and it is what picks up a *later* retitle,
  since the poll latches after its first success.

The turn-end read used to be the only path. Because Claude settles `result`
only when the whole turn is done, an agentic first turn showed the truncated
prompt for its entire run, and an interrupted one never got a title at all —
even though the real title had been sitting on disk since second three.

### Codex — the one Raku generates

The Codex app-server **persists** a thread name but never generates one; naming
is the client's job, and Codex Desktop does it client-side too. So Raku matches
that behavior in
[`generate_codex_title`](../crates/raku-core/src/driver/codex.rs#L1006): a
second, short-lived `codex app-server --stdio` process runs one ephemeral,
read-only turn.

- Model `gpt-5.6-luna` ([codex.rs:958](../crates/raku-core/src/driver/codex.rs#L958)),
  `effort: "low"`, `summary: "none"` — the cheapest tier, since a title is a
  fixed classification.
- `CODEX_TITLE_INSTRUCTIONS` is passed as both `baseInstructions` and
  `developerInstructions`; the turn's only input is the raw user prompt.
  Titles follow the predominant language and script of the user's prose,
  including Chinese in mixed Chinese/English requests. Code, paths, and
  technical identifiers do not determine the language; technical terms may
  retain their original form.
- An `outputSchema` pins `{title: string}`, max 80 chars.
- `ephemeral: true`, so it leaves no thread behind. 60s timeout.
- Gated to sessions Codex did not already name (`enabled: provider_session_id
  .is_none()`) and to one run per driver.

The result is emitted as `AutoTitleUpdated` **before** being written back to
Codex with `thread/name/set`, so the sidebar updates even if that write fails.
Output is cleaned by `normalize_codex_title` — it decodes JSON or `{title}`,
drops code fences, strips a `Title:` prefix, trims quotes and markdown emphasis,
collapses whitespace, and caps at 80 characters.

### Pi

Pi delivers titles on its own stream.

- **Pi** — `/data/sessionName` at connect, then the `session_info_changed` stream
  event with the title under `name` ([pi.rs:69](../crates/raku-core/src/driver/pi.rs#L69)).
  An empty value becomes `AutoTitleUpdated(None)` and clears the title back to
  the fallback. This event alone does not establish automatic generation.

**Pi naming gap.** The installed upstream Pi 1.4.0 has `setSessionName`, the
`set_session_name` RPC, and the explicit `/name <name>` command. Its built-in
session and mode code contains no automatic title-generation call. Raku
currently forwards name changes but does not generate a missing Pi name.
Both existing Pi conversations checked during the audit retained their prompt
fallback and had no native `session_info` name entries. An extension can supply
a name; built-in rename support is not automatic naming.

## Adding a provider

In order of preference:

1. **Does the agent already generate a title?** Several do — for their own
   UI, on their own cheap model. Verify the active transport actually enables
   generation; a title field or rename event is not sufficient evidence.
   Find where it surfaces before writing any model call.
2. **Does it push that title on the stream?** Then forward it and stop. Prefer
   trimming and dropping the provider's own placeholder strings.
3. **Does it only write the title to a native store?** Use `NativeTitleRefresh`.
   Measure when the title actually lands and pick a schedule that brackets it —
   do not guess. Arm it from the earliest point the title could exist: prompt
   time if the agent titles eagerly (Claude), turn end if it titles from the
   response.
4. **Only if the agent genuinely never generates one**, generate it, following
   the Codex shape: cheapest model, lowest effort, an ephemeral session, the
   raw user prompt as the only input, a structured output schema, a timeout,
   once per session, never on resume. Reuse `normalize_codex_title`.

Whatever the path, failure must be silent and the fallback must hold. A missing
title is a cosmetic gap; a driver that errors or blocks on one is not.
