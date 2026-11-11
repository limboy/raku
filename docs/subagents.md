# Subagents

**Status: design, not implemented.** This records the intended shape of
Raku-owned subagents so the work can be phased. Sections marked *Unverified*
must be probed against the real CLIs before the code that depends on them is
written.

A subagent is a child agent that a task's main agent delegates a piece of work
to. It runs in its own context window and hands back a final report, so the
parent spends tokens on the conclusion rather than on the search that produced
it.

The design borrows from OpenCode's subagent system, where a subagent is a
first-class child session rather than an in-memory coroutine. It departs from
OpenCode where Raku's architecture differs; see
[Why the app orchestrates](#why-the-app-orchestrates).

## Scope

- **Pi first.** Pi has no subagents at all, so it gets a Raku-owned `subagent`
  tool.
- **Claude Code and Codex keep their native subagents.** They already surface
  as `BackgroundWorkKind::Subagent` through
  [driver/claude.rs](../crates/raku-core/src/driver/claude.rs) and
  [driver/codex.rs](../crates/raku-core/src/driver/codex.rs).
- **Cross-provider delegation for Claude and Codex comes last**, through an MCP
  bridge. Their MCP tool timeouts need probing before a blocking call can be
  trusted.

## Why the app orchestrates

OpenCode keeps subagent jobs in its server so they survive a client restart.
That does not carry over:

- **The daemon dies with the app.** `raku-daemon` is launched with
  `--parent-pid` and shuts down when the app exits, so there is no UI-less
  period for a job to survive.
- **The daemon does not build transcripts.** It owns provider processes and
  persistence. Turns, messages and the follow-up queue are assembled in the app
  by `handle_driver_event` ([src/app/streaming.rs](../src/app/streaming.rs)) and
  then saved.

So the app orchestrates subagents, exactly as it does side chats, and the
daemon gains only a thin tool bridge. A daemon-owned job system would require
moving the whole transcript reducer into `raku-core` first, for no
user-visible gain. Revisit this if the daemon ever outlives the app.

## Model

A subagent is a hidden child `AgentSession`, the same pattern as
[side chats](side-chat.md):

```rust
pub struct SubagentOrigin {
    parent_session_id: Uuid,
    call_id: String,        // the parent's tool activity id
    agent: String,          // "explore" | "general" | custom
    description: String,    // 3–5 word label from the model
    background: bool,
    delivered: bool,        // result handed to the parent exactly once
}
// AgentSession.subagent: Option<SubagentOrigin>
```

- **Persistence.** The origin is stored in a nullable `sessions.subagent` JSON
  column, so a list-only startup knows which sessions are subagents before
  hydration.
- **Hidden from task lists.** `AgentSession::is_listed_task` excludes
  subagents, as it does side chats.
- **No task side effects.** A subagent creates no git checkpoints, completion
  notifications, unread markers or provider titles. Its title is its
  `description`. The parent's turn checkpoint covers the child's edits.
- **Workspace.** The child runs in the parent's workspace and never creates a
  worktree of its own.
- **Deletion.** Deleting a task deletes its subagents.
- **Lookups.** Raku keeps a parent → subagents index, rebuilt only when
  subagents are created, deleted or synced, never per frame.

## Tool

```text
subagent({ agent, description, prompt, session_id?, background? })
```

| Field | Meaning |
| --- | --- |
| `agent` | Which agent to run. The tool description lists the available ones as `- id: description`. |
| `description` | Short label shown in the UI. |
| `prompt` | The task. A fresh child's prompt is prefixed with a line saying it is a subagent spawned by another session. |
| `session_id` | Continue an existing child instead of starting one. Rejected unless that child belongs to the calling parent. |
| `background` | Return immediately and deliver the result later. |

The model-visible result is:

```xml
<subagent session_id="…" state="completed">
…the child's final assistant message…
</subagent>
```

`state` is `completed`, `failed`, `cancelled`, `interrupted` or `running`. A
`running` result tells the model the subagent is working in the background,
that it will be notified when it finishes, and that it must not sleep, poll or
duplicate the work.

## Triggering

A subagent starts in one of four ways. The first two need no UI at all.

| Trigger | What happens | Phase |
| --- | --- | --- |
| The model decides | The agent calls `subagent` on its own when the tool description fits the task. | 1 |
| The user asks in prose | "Use a subagent to find every caller of X." The model makes the call. | 1 |
| `@agent` in the composer | Typing `@` offers the available agents beside files in the existing autocomplete. The chosen agent becomes a chip, and the prompt is sent with an instruction to delegate to it. The model still makes the call, so the result returns to it in the same turn. | 1 |
| `/agent <task>` | Launches a background child directly, with no parent model turn. The result arrives through background completion delivery. Works while the parent is busy. | 2 |

`agent` here is the name of any enabled agent from
[Settings → Subagents](#custom-agents): with agents named `explore` and
`reviewer`, the composer offers `@explore`, `@reviewer`, `/explore` and
`/reviewer`. Several `@agent` chips may appear in one prompt.

`/agent` commands are listed in the command palette and the slash-command
autocomplete, so every trigger is reachable from the keyboard.

`@agent` needs the parent's model to make the tool call, so it is offered only
where the parent's provider has the Raku subagent tool (Pi at first). `/agent`
does not: the app launches the child itself and delivers the result as a
message, so it works with any parent provider.

## Bridge

A tool call is a request the app answers, following the existing
`Permission` → `Respond` pattern:

1. A bundled Pi extension (`resources/pi/subagent.ts`) registers the tool and
   posts the call to the daemon over loopback.
2. The daemon emits a `SubagentRequested { call_id, … }` driver event on the
   parent's stream and holds the request open.
3. The app creates or resumes the child, submits the prompt, and on the child's
   `TurnFinished` sends `Command::RespondSubagent { call_id, result }`.
4. The daemon answers the held request, and the extension returns the result
   to the model.

The extension authenticates with a per-runtime token passed in the Pi
process's environment and scoped to that one session and endpoint. The
daemon's own token never reaches a provider process. The event and command are
protocol version 19.

Pi's extension API supports this directly. A tool's
`execute(toolCallId, params, signal, onUpdate, ctx)` is an unbounded async
call: the `AbortSignal` carries the parent's Stop to the bridge, `onUpdate`
can stream child progress into the parent's tool row, and
`executionMode: "parallel"` lets several subagent calls run at once. Pi's own
example extension (`packages/coding-agent/examples/extensions/subagent`) holds
a call open the same way while it runs child `pi` processes. Read from the Pi
source; still run one real delegated turn before shipping.

## Agents

| Agent | Access | Model |
| --- | --- | --- |
| `explore` | The existing read-only launch policy (`DriverStartOptions::read_only`) | A cheap model chosen in **Settings → Subagents** |
| `general` | The parent's access mode, never wider | The parent's provider and model |

Access is deliberately coarse. Pi has no permission system, so an OpenCode
style rule engine has nothing to enforce against; the launch-time tool
allowlist is the real boundary on every provider.

### Custom agents

**Settings → Subagents** lists the agents and lets the user add, edit, reorder
and delete them. The two built-ins can be edited or disabled but not deleted.
Each agent has:

| Field | Notes |
| --- | --- |
| Name | Lowercase, unique, no spaces. It is what the user types after `@` or `/` and what the model passes as `agent`. |
| Description | One line telling the model when to pick this agent. Shown in the tool description and in autocomplete. |
| Provider and model | Any installed provider, or "Same as task". A child may run on a different provider than its parent. |
| Reasoning | Options come from the selected model, as in the side-chat settings. |
| Access | Read-only, or "Same as task". Never wider than the parent's mode. |
| Instructions | Optional system prompt for the child. |

Agents are stored with the app settings, beside the side-chat settings in
[persistence.rs](../crates/raku-client/src/persistence.rs), and are global to
all projects. The app sends the enabled list to the bridge when a runtime
starts and again when the list changes, so the tool description and the
composer autocomplete always agree.

An agent whose provider is missing or disabled stays in the list, marked
unavailable with an icon and text, and is left out of the tool description and
autocomplete.

Per-project agents as Markdown files with frontmatter are a possible later
addition; Settings is the only source for now.

## Limits

- **Depth 1.** A child is launched without the subagent extension, so it
  cannot spawn children of its own. The limit is structural, not a prompt
  instruction.
- **Concurrency.** At most 4 live children per parent. A further call returns
  an error result the model can act on.
- **No user questions.** A child's user-input request is declined
  automatically, telling it to decide for itself or report back. A background
  child must never hang on a card the user is not looking at.
- **Approvals.** Once Claude and Codex children exist, a child's permission
  request surfaces on the parent, labelled with the subagent.

## Lifecycle

- **Foreground (default).** The tool call blocks until the child's turn
  settles. Stopping the parent cancels its foreground children.
- **Promotion on steer.** If the user steers the parent while a foreground
  call blocks, the app answers the call with `state="running"` and the child
  continues in the background. The parent can respond to the user at once.
- **Background completion.** If the parent is busy and its runtime supports
  steering, the result is steered in. Otherwise it becomes a synthetic entry
  in the existing `queued_messages`, which starts a turn as soon as the parent
  is idle. It is drawn as a compact notice, not a user bubble, and is not
  editable.
- **Resume.** Passing `session_id` continues a child with its context intact.
  If that child is still running, the prompt steers it.
- **App quit mid-run.** On relaunch, an undelivered child reports
  `state="interrupted"` once. `SubagentOrigin::delivered` guards against a
  second delivery.

## UI

- **Transcript row.** Bot icon, description, agent and model, with status as
  an icon plus text. Status reads the child session's in-memory state rather
  than the tool call, so a backgrounded child keeps showing as running after
  its call has returned.
- **Right panel.** Click or Enter on the row opens a
  `RightPanelSurface::Subagent` tab that reuses the side chat's virtualized
  transcript. The header shows status, elapsed time, agent, provider and
  model, and a Stop button. Its composer steers the child. Escape returns
  focus to the main composer.
- **Background Work.** Each child is projected as a `BackgroundWorkItem`
  (`control_id` is the child session id, `origin_activity_id` is the call id),
  so the existing chip, menu and stop control work unchanged for native and
  Raku subagents alike.
- **Usage.** Child tokens stay out of the parent's context meter and show on
  the child's header.

Everything the row and header draw comes from sessions already in memory. The
elapsed time ticks at 1 Hz and is skipped under reduce-motion only where it
animates; see [performance.md](performance.md).

## Phases

1. **Pi foreground.** Origin column and migration, bridge, extension, the
   settings page with built-in and custom agents, `@agent` in the composer,
   transcript row, right-panel viewer, Stop.
2. **Background.** Promotion on steer, completion delivery, resume, Background
   Work projection, `/agent` direct launch.
3. **Reach.** MCP bridge so Claude and Codex parents get the tool and
   `@agent`.

## Unverified

- MCP tool-call timeouts in Claude Code and Codex, which decide whether a
  foreground call can block for minutes or must always return `running`.
