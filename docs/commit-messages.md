# Commit-message generation

When the commit dialog's message box is left empty, Waku generates the subject
line by running **the session's own agent CLI** once, headlessly. No coding
agent exposes a commit-message endpoint and Waku holds no provider API key, so
the CLI binary is the only route to a model.

Every provider gets the same prompt. The only per-provider code is the argument
vector that puts its CLI into a one-shot, tool-free mode
([`agent_arguments`](../crates/waku-core/src/git_commit.rs#L210)).

## Which model

A commit subject is a fixed classification over a diff that is already in the
prompt, so it does not need the model the task runs on. Where a provider names a
cheap tier, generation is **pinned** to it at the lowest effort that tier's API
accepts, whatever the session selected — the same reasoning that puts Codex
titles on `gpt-5.6-luna` ([titles.md](titles.md)):

| Provider | Model | Effort | How |
| --- | --- | --- | --- |
| Claude Code | `claude-haiku-4-5` | `low` | `--model` / `--effort`; `low` is the floor `--effort` accepts |
| Codex CLI | `gpt-5.6-luna` | `none` | `--model`, plus `-c model_reasoning_effort="none"` — `codex exec` has no effort flag |

`none` is genuinely the floor for Codex: `minimal` comes back as a 400
`unsupported_value` naming `none, low, medium, high, xhigh, max` as the
supported set for this model.

Every other provider still runs on the session's own selection, captured when
the dialog opens into an
[`AgentInvocation`](../crates/waku-protocol/src/git.rs#L45) alongside the probed
binary path ([commit_dialog.rs:129](../src/app/commit_dialog.rs#L129)):

| Field | Source |
| --- | --- |
| `model` | [`model_for_session`](../src/app/runtime.rs#L1492) — the session's explicit model, else the provider probe's preferred model (the one flagged `is_default`, else the first discovered) |
| `reasoning_effort` | the session's own `reasoning_effort`, verbatim |

Both are `Option`, and a `None` omits the flag, leaving the CLI on its own
default. **DeepSeek** is passed no model flag at all, so it always runs whatever
`--profile headless` resolves to.

## The prompt

[`commit_prompt`](../crates/waku-core/src/git_commit.rs#L173) builds it from Git
alone — no transcript, no session history:

| Include unstaged | Status | Diff |
| --- | --- | --- |
| off | `git diff --cached --name-status` | `git diff --cached` |
| on (default) | `git status --short --untracked-files=all` | `git diff --cached`, then `git diff` under its own heading |

Diffs run `--no-ext-diff --no-color`. The context is capped at 96 KiB, cut on a
UTF-8 boundary and marked `[diff truncated]`, which also adds a sentence telling
the model to summarize only what it can still see. Then:

```
Generate a concise Git commit subject for the changes below.
Return exactly one line and nothing else: no quotes, Markdown, prefix, explanation, or trailing period.
Use imperative mood and at most 72 characters. Do not call tools; all context is included here.
```

## Per provider

Same prompt, same pipeline. Unless noted, the prompt is the **last positional
argument**; `NO_COLOR=1` and `CI=1` are set for all of them.

| Provider | Command | Headless / tool-free flags | Model | Effort |
| --- | --- | --- | --- | --- |
| Claude Code | `claude` | `--print --output-format text --permission-mode plan --tools "" --disable-slash-commands --no-session-persistence --no-chrome` | pinned `claude-haiku-4-5` | pinned `low` |
| Codex CLI | `codex exec` | `--sandbox read-only --ephemeral --color never --skip-git-repo-check` | pinned `gpt-5.6-luna` | pinned `none`, via `-c` |
| DeepSeek Harness | `dsh` | `--profile headless` | — | — |
| OpenCode | `opencode run` | `--standalone --agent plan` | `--model` | `#<variant>` suffix on the model |
| Pi | `pi` | `--print --no-session --no-tools --no-context-files --no-extensions --no-skills --no-prompt-templates --no-approve` | `--model` | `--thinking` |
| Oh My Pi | `omp` | `--print --no-session --no-tools --no-rules --no-extensions --no-skills` | `--model` | `--thinking` |

Where a provider is not simply "flags plus prompt":

- **Claude Code** runs in `plan` permission mode with an empty `--tools` list,
  and `--no-session-persistence` keeps the run out of `~/.claude/projects`,
  where it would look like a task the user started. Haiku exposes no reasoning
  tiers in Waku's catalog, but the CLI still accepts `--effort low` for it.
- **Codex** runs `exec` as an ephemeral read-only turn; `--skip-git-repo-check`
  lets a workspace that is not a repo root run. Effort rides on a `-c` config
  override because `codex exec` has no flag for it.
- **DeepSeek** gets only `--profile headless`, Harness's one-shot stdout-only
  client. It has no tool switch — the prompt's "do not call tools" is the whole
  guard, which holds because all context is inlined.
- **Oh My Pi** rejects unknown flags outright, so it gets its own list rather
  than Pi's: context files are `--no-rules`, and it has no prompt-template or
  project-trust switch to turn off.
- **OpenCode** runs `plan`, its read-only agent, on a private `--standalone`
  server rather than on the user's background service, so a commit subject
  neither starts that service nor queues behind it. The run is still an
  ordinary session in OpenCode's history. Effort is the model's variant, sent
  as `--model provider/model#low`; OpenCode rejects a variant without a model,
  so effort is dropped when no model is chosen.
- **Pi** needs the long list because each capability is its own switch.
  `--no-context-files` is its "disable `AGENTS.md` and `CLAUDE.md` discovery"
  flag, so repo instructions cannot contradict the fixed prompt.

`every_provider_uses_a_noninteractive_generation_mode`
([git_commit.rs:691](../crates/waku-core/src/git_commit.rs#L691)) walks
`ProviderKind::ALL` and asserts each of these, so a new provider cannot be added
without choosing its headless shape.
`claude_and_codex_generate_on_a_pinned_cheap_tier` guards the pins by passing
`claude-opus-5` / `gpt-5.6-sol` and asserting neither reaches the CLI.

## Normalizing the output

CLIs disagree about what "one line and nothing else" means — preamble lines,
code fences, ANSI under `NO_COLOR`. [`normalize_message`](../crates/waku-core/src/git_commit.rs#L349)
strips ANSI, drops empty lines, bare ``` fences and `[tool]` / `[thinking]`
lines, takes the **last** surviving line, then strips backticks, a
`Commit message:` / `Commit subject:` prefix, wrapping quotes and a trailing
period, and caps at 200 characters. Empty means failure, reported as
`<Provider> returned no commit message`.

Taking the last line makes the result a subject only: a model that returns a
subject, blank line, and body has the body discarded.
