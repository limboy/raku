# Changelog

All notable changes to Raku. This file is the **source of truth for the release
notes shown in the in-app updater**: [`scripts/release.ts`](scripts/release.ts)
extracts the section whose heading matches the version being released
(`Cargo.toml`) and publishes it next to the update, so Sparkle shows it in
the update prompt.

Format follows [Keep a Changelog](https://keepachangelog.com). Add a new
`## [<version>]` section at the top for each release, matching the version you
set in `Cargo.toml`.

Write release notes for the final product users receive, not the development
history. When a feature is still unreleased, fold its fixes and refinements into
the original feature bullet instead of adding separate entries for them.

## [unreleased]

## [2.5.7] - 2026-11-12

- Add a Context inspector showing recorded context snapshots, tool catalogs, and subagent definitions
- Show recorded context usage when live counts are unavailable
- Enter plan mode with `/plan <prompt>` and send the text as the prompt

## [2.5.6] - 2026-11-12

- Run parallel subagent calls at the same time on Claude instead of one after another

## [2.5.5] - 2026-11-12

- Show prompt mentions as accent-coloured text instead of pills in the composer and transcript
- Shimmer the live status label instead of showing wave dots while the agent works

## [2.5.4] - 2026-11-12

- Show how long finished commands ran in the activity row
- Remove the duplicate Running indicator and the Completed badge on background commands
- Align expanded activity details with the row label

## [2.5.3] - 2026-11-12

- Open an agent's settings from its prompt pill
- Polish agent pills and fix spacing around inline pills in prompts
- Make the subagent model picker scrollable
- Center the subagent Steer input text vertically
- Fix working dots alignment and activity indicator shifts
- Align activity rows and expanded details to a shared text column

## [2.5.2] - 2026-11-12

- Add configurable subagents for Pi, Claude Code, and Codex, with provider and model choices in Settings
- Run subagents in the background, resume them, and open their reports from the transcript
- Launch subagents with @ references or /agent-name commands
- Keep composer commands and references as inline pills through drafts, history, editing, and copy/paste
- Find commands, prompts, and skills under /, and files, agents, and skills under @
- Simplify composer suggestions and hide the project chip when the sidebar already names the project
- Refine live activity disclosures and animate in-progress task list steps

## [2.5.1] - 2026-11-11

- Improve the task list to follow the transcript font size setting
- Shrink the code block header buttons

## [2.5.0] - 2026-11-11

- Add a pinned task list above the working row that Claude and Codex fill as they plan multi-step work
- Add follow-up cards under a reply that leaves you a decision, with one button per suggested answer
- Improve the working row to name the current action, such as "Running a command 12s", and keep it between tool calls
- Improve the live activity list to open beneath the working row instead of in its own transcript row
- Improve the working label to match the activity text, with internal actions and reasoning folded by default
- Remove plan-update rows from the transcript, including in sessions saved earlier

## [2.4.9] - 2026-11-10

- Fix the response footer disappearing for a frame when the pointer moves from a response to its footer
- Improve footer icon and label contrast in the light theme
- Fix Settings showing a focus ring on its first tab when opened with ⌘,
- Fix sidebar shortcut badges and remove buttons staying visible after a missed Cmd or Shift release

## [2.4.8] - 2026-11-10

- Add interface and transcript font preferences
- Add a Text Generation setting that gives thinking rows short generated headlines
- Pin the agent's task list above the composer as a checklist while a turn runs
- Filter the sidebar history by project
- Add a Shift-revealed trash button and Cmd-Backspace to remove sidebar sessions
- Show reads, edits, and tool calls as framed cards and bash commands as prompt lines
- Summarize turn work as plain rows such as "Ran 5 commands · 2 file reads"
- Describe Pi commands and thinking in activity rows
- Refine markdown typography with typographic punctuation, disc list markers, and softer link underlines
- Raise transcript font sizes by one pixel and set inline code smaller and in the prose color
- Render clickable inline code as a regular link
- Improve text rendering with lighter glyph smoothing and darker light-theme body text
- Make status, check, and code block icons bolder and keep thin icon strokes legible at small sizes
- Improve usage popover typography and contrast and standardize small interface text at 13pt
- Keep sidebar rows highlighted while their menu is open and lighten row hover
- Fire menu and autocomplete clicks on release instead of press
- Reduce disk writes while streaming by saving only changed transcript blocks
- Fix sidebar shortcut badges shifting status icons

## [2.4.7] - 2026-11-09

- Add model-specific reasoning levels for side chats, defaulting to Pi, DeepSeek, and Low
- Simplify side chat settings and wrap the read-only description
- Disable side chat submission when the input is empty or contains only whitespace

## [2.4.6] - 2026-11-07

- Add line counts to the Git popover's Review changes row
- Improve scrolling performance by using an opaque window and a solid sidebar instead of vibrancy

## [2.4.5] - 2026-11-07

- Improve the project picker by showing the finished dot beside the project title and No project's status icon beside its name
- Fix row controls losing the focus they take

## [2.4.4] - 2026-11-07

- Add Retry to a failed turn's response footer
- Hide the pinned No project row while searching projects and show its running state
- Fix the finished dot being hidden when a project has other busy tasks or a turn fails

## [2.4.3] - 2026-11-06

- Add Option-arrow session navigation in the sidebar
- Run /compact from the slash menu instead of a popover button
- Increase the header session title font size
- Fix Pi hanging on locally-handled commands like /curator

## [2.4.2] - 2026-11-06

- Show the finished dot on the No project row

## [2.4.1] - 2026-11-06

- Fix the app icon so macOS 26 draws it without the glass tile

## [2.4.0] - 2026-11-06

- Rename Waku to Raku, with a new app icon
- Move existing data from `~/.waku` and `Application Support/Waku` to the Raku locations on first launch, leaving links at the old paths so existing projects, worktrees, and sessions keep working

## [2.3.0] - 2026-11-06

- Add read-only side chats: ask a quick question about a task, or about quoted transcript text via right-click → Side chat, without adding turns to the conversation
- Show failed turns as an error reply with the provider's reason behind a Details disclosure
- Recall earlier prompts with up/down in an empty composer
- Add a control to turn off automatic backups
- Limit response hover to the content column so the side gutters no longer reveal the footer
- Remove the inset background from transcript code blocks

## [2.2.10] - 2026-11-06

- Let agents hand you a command as a runnable block: a Run button opens an inline terminal, and the agent resumes with the exit code and output when it finishes
- Move Plan mode into the commands list
- Add ⌘1–⌘9 shortcuts for sidebar sessions
- Add a search filter to the Files tree, and show it beside an empty editor pane by default
- Add image and SVG previews and file context actions to the right sidebar
- Add session deeplinks that paste as conversation attachments
- Open a file's diff from the changed-files card
- Notify when another project's turn finishes while Raku is frontmost, prefixed with the project name
- Steer Codex async question answers into the running turn instead of queuing them
- Replace the UI font size setting with a transcript font size (Small, Medium, Large)
- Restyle context menus after native macOS menus, and keep them anchored to the cursor
- Refresh the light palette, sidebar project selector, picker search fields, and icons
- Simplify the right sidebar and header controls, and remove the header app launcher
- Fix a maximized window reopening with a gap at the right and bottom, and show it already zoomed on launch

## [2.2.8] - 2026-11-05

- List skills under `$` in the composer, separate from `/` commands; a leading `/skill` typed from habit still runs the skill
- Remove sub-agent delegation, including the Settings → Agents page and the composer's `^` model trigger
- Fix long command argument hints pushing the description off the autocomplete row

## [2.2.7] - 2026-11-04

- Add plan mode for Claude and Codex tasks: the agent investigates and proposes a plan, then implements it once you approve, or revises it from your reply. Toggle it in the composer or with ⌘⇧P
- Name new Pi tasks automatically with a short title in the language of your first message
- Fix rewinding and editing earlier messages in Codex sessions with Codex 0.166 and later
- Show when Claude or Codex retries a request, switches models, or reports a warning, instead of an unexplained spinner
- Show Codex task checklists in the transcript when the plan tool is enabled in your Codex config
- Update built-in Claude and Codex model choices for first-run and offline use
- Simplify the task info popover and show clearer Git actions and repository status

## [2.2.6] - 2026-11-04

- Separate consecutive Codex messages with paragraph breaks so progress updates and final answers do not run together
- Hide OpenCode models disabled in the Console from Pi's model picker

## [2.2.5] - 2026-11-04

- Show OpenCode Go and Go Plus plan usage limits for Pi tasks using OpenCode Go models

## [2.2.4] - 2026-11-04

- Fix Codex questions never appearing, and answer or dismiss them from the question sheet
- Remove Antigravity CLI support

## [2.2.3] - 2026-11-04

- Replace the composer's Local/New worktree selector with a project chip that opens the sidebar's project picker
- Limit the composer's branch picker to switching between existing branches
- Align the traffic lights with the sidebar toggle and search icons, and tighten the sidebar project label spacing

## [2.2.2] - 2026-11-03

- Add a titlebar search button that opens task search (⌘K) over titles and message content, replacing the sidebar title filter
- Move New task to a compose button in the sidebar titlebar (⌘N) and widen the sidebar's minimum width to fit it
- Add ⌘P to open the sidebar project selector, and outline the selector like a text field
- Show shortcuts in the sidebar toggle, search, and New task tooltips
- Title untitled tasks "New task"

## [2.2.0] - 2026-11-03

- Add named sub-agents in Settings → Agents, listed under `^` with a model route and standing instructions
- Add an agents card beside the changed-files card that opens each subtask's transcript in the right panel
- Let Claude sessions delegate work to sub-agents without per-call prompts in Supervised mode
- Move the project selector to the top of the sidebar and add a task filter that searches titles as you type
- Replace the titlebar back/forward arrows and the New Task row with a New Task button (⌘[ and ⌘] still navigate)
- Show a dashed folder instead of an × for "No project"
- Fix line breaks in markdown table cells rendering as literal `<br>`

## [2.1.2] - 2026-11-03

- Add an error page with Retry when a browser page fails to load, and stop the toolbar from loading forever
- Fix renaming a task that has not been opened since launch deleting it
- Warn with a Retry banner when saving task changes fails, and retry automatically instead of losing edits
- Speed up sending messages in large repositories by seeding checkpoint snapshots from the worktree index
- Improve responsiveness and idle CPU use by writing app state off the UI thread and waiting on daemon sockets instead of polling

## [2.1.1] - 2026-11-03

- Restore automatic titles for new Claude Code sessions using Claude's native title generator
- Improve generated image sizing and previews, with context-menu actions to add images to chat, copy them, or reveal them in Finder
- Fold completed image-generation activity when its images already appear in the response
- Add transcript selections to chat from the context menu as text snippets
- Skip whitespace-only text between tool calls in streaming transcripts
- Fix startup after updating when saved preferences still reference Oh My Pi or OpenCode

## [2.1.0] - 2026-11-02

- Add transcript selections to chat from the context menu as text snippets
- Skip whitespace-only text between tool calls in streaming transcripts
- Fix startup after updating when saved preferences still reference Oh My Pi or OpenCode

## [2.0.3] - 2026-11-02

- Support native image generation and transcript previews, including Antigravity image generation subagents
- Make workspace file mentions clickable in transcripts
- Remember provider and model selections per project
- Generate session titles matching the prompt language and strip decorative leading icons
- Remove Oh My Pi, OpenCode, DeepSeek Harness, and Computer Use integrations to focus on core providers
- Fix shell environment capture when `noclobber` is enabled

## [2.0.2] - 2026-11-02

- Open files in their default app or reveal them in the file manager from the right sidebar
- Refresh context usage after Pi and Oh My Pi compaction and preserve accurate token counts
- Show a readable archived-history notice in compaction details and conversation exports

## [2.0.1] - 2026-11-01

- Fork and rewind Antigravity tasks at a selected turn, with new forks appearing above their source tasks in the sidebar
- Open text file attachments from the composer or conversation in the right detail panel
- Remove the usage dashboard from settings

## [2.0.0] - 2026-11-01

- Install signed updates directly from GitHub Releases using the install icon at the top of the sidebar, with native update flows on macOS, Linux, and Windows
- Add Antigravity CLI support alongside Claude Code, Codex CLI, DeepSeek, Oh My Pi, OpenCode, and Pi; retire the Kimi, Amp, Cursor, Fx, and Grok integrations
- Focus Raku on native desktop workflows with local agent sessions and storage; remove the web and mobile clients and remote daemon access
- Add native context compaction across providers, automatic task titles for Oh My Pi, and collapsible text snippet attachments for long pastes
- Pin tasks, see ongoing and unread completed tasks in the project picker, and browse session history grouped by date
- Export conversations as Markdown, copy task details from session menus, and restore daily backups with a clean relaunch
- Preview Mermaid diagrams natively, support CJK Markdown emphasis, and remember model picker favorites
- Show plan usage quotas in the usage panel (`⌘U` / `Ctrl+U`) for Oh My Pi tasks backed by OAuth or subscription providers such as Codex and Antigravity, scoped to the active model family and pinned account
- Fix cross-model subagent tool calls and OpenCode routing, accept long wait requests as bounded progress polls, keep cancelled tasks stopped, make retries start a new attempt, and report failed or timed-out groups accurately; code reviews use complete snapshots that are reclaimed when their parent task is deleted
- Show delegated subagents alongside background work under the info icon, with a live indicator and task controls; their details share the main agent's thinking, tool activity, and answer presentation with compact spacing, folded historical thinking, and stable scrolling
- Keep selected delegation models inline in the composer instead of duplicate pills, with one-step Backspace deletion and undo/redo; preserve model and channel selections across draft restoration and queued messages

## [0.1.20]

- Support the OpenCode 2.0 release, which now installs as `opencode`: OpenCode runs on its shared background service, OpenCode 2 beta and OpenCode 1 tasks continue as OpenCode tasks, and OpenCode's free models work in Raku tasks
- Fix rewinding or branching an OpenCode task after steering it mid-turn, which forked OpenCode's conversation at the wrong turn
- Fix OpenCode tasks going silent when the OpenCode service restarts; a turn it interrupted now continues in place once the service is back
- Fix Cursor model discovery and options so the pickers list every model the Cursor CLI advertises, with its reasoning effort, fast mode, and context window
- Show the release version in Claude model names, such as Opus 4.6 (1M context), in the model picker and composer
- Make the project picker searchable and scrollable, keeping long project lists reachable with the new-project and no-project actions pinned below
- Fix Pi sessions failing to start with "Could not initialize Pi" when Pi is slow to load extensions
- Keep long user messages compact: they scroll inside their bubble with edge fades instead of taking over the transcript

## [0.1.19]

- Render inline and block LaTeX math in Markdown, with a Copy Expression action and a setting to show the source
- Fix OpenCode 2 context usage to reflect the latest request instead of cumulative session totals, and cap the meter at 100%
- Clear model search when selecting a provider or Favorites tab
- Make loading spinners smoother with animations at up to 60 FPS

## [0.1.18]

- Fix Codex session forking
- Add OpenCode 2 support, add reasoning effort option for both OpenCode and OpenCode 2
- Fix memory usage for long-running sessions

## [0.1.17]

- Fix the OpenCode Resume list showing only sessions started outside a git checkout; it now lists sessions from every project
- Hold Claude's turn open while it waits on background work

## [0.1.16]

- Import and continue conversations started in agent CLIs with `/resume` or the command palette across every provider, in both Raku and Raku Web
- Linux: add signed in-app updates with clean relaunch and automatic rollback
- Copy Raku task IDs and agent CLI thread IDs from task info or the command palette
- Keep each response's actions and changed-file summary after its final tool activity
- Keep the selected task visible when navigating the sidebar
- Let nested transcript and command-output scrollers hand wheel gestures to the page only at their boundaries
- Keep multiline background-work titles on one line in summaries and panel headers

## [0.1.15]

- Codex thread goals: type /goal to set a persistent objective the task keeps pursuing — before or after the first message — with its autonomous pursuit streaming into the transcript, a status chip showing live budget or elapsed time, and a dialog to edit, pause, resume, or clear the goal (also in Raku Web)
- Discover provider-native slash commands and skills from installed agent CLIs, including multiline YAML descriptions
- Add reasoning effort selection for Grok
- Reconnect remote daemon sessions automatically after connection interruptions
- Fix Command/Ctrl+Enter steering after a provider response starts streaming
- Fix transcript file links on Windows
- Fix OpenCode dropping the first streamed event and hanging during cancellation on Windows

## [0.1.14]

- Group sidebar tasks by project or update date, order them newest or oldest first, and collapse sections
- Find in page: Search the full transcript by keywords using cmd-f or ctrl-f
- Switch between recent tasks with Ctrl+Tab and Ctrl+Shift+Tab
- Carry the current access mode into new tasks and remember it between launches
- Fix OpenCode access-mode permissions and restore pending permission prompts when resuming sessions
- Show Codex file reads, listings, and searches as file activity instead of raw commands
- Keep long panel and background-work titles on one truncated line
- Increase the minimum UI text size for better legibility

## [0.1.13]

- Add Vercel Fx support
- Support DeepSeek Harness 0.1.1 without opening its web UI
- Collapse earlier activity groups when a running turn moves on to newer transcript output

## [0.1.12]

- Invoke Codex, Pi, and Oh My Pi skills with their native syntax
- Stream live output from Claude background tasks
- Steer the oldest queued follow-up with Command/Ctrl+Enter in an empty composer
- Fix model and reasoning option selection for Cursor
- Fix npm-installed provider detection on Windows
- Fix daemon terminal sessions hanging during shutdown
- Exclude copied history from forked Codex sessions from usage totals
- Keep separate Codex reasoning sections on separate lines

## [0.1.11]

- Highlight Markdown in the file editor, and toggle between source and a rendered preview
- Add UI and code font size settings
- macOS: Add "Open in.." button to open project folder in selected application

## [0.1.10]

- Add Kimi Code support
- Add Oh My Pi support
- Fix markdown table rendering

## [0.1.8]

- Fix `PATH` resolution on Windows

## [0.1.4]

- Fix text selection in diff view

## [0.1.3]

- Pin Codex and Claude commit message generation to cheap models: gpt-5.6-luna and claude-4.5-haiku
- Animate sidebars
- Render provider file edits as inline diffs in the transcript
- Fix claude task title generation

## [0.1.2]

- Fix regression: user bubble should fit its content width

## [0.1.1]

- Give nested Markdown the full message width
- Cap composer height and scroll overflow with an overlay scrollbar
- Keep drag-selecting text past the input bounds
- Fix char boundary panic when sliding the live reasoning window

## [0.1.0]

- Add standalone Raku daemon and browser client
- Add Linux support (X11 and Wayland, you need to build from source for now)
- Answer agent questions directly in the composer
- Redesign queued follow-ups as composer cards with per-message steering
- Add DeepSeek agent preset selection (Standard, Code, Minimal, and Creator)
- Add Claude context window and ultracode effort options
- Add /fast command to toggle fast mode for Codex
- Show the latest activity in live transcript headers
- Add soft wrapping and keyboard copy feedback
- Add terminal overlay scrollbar and measure cell width from the font
- Restore window position, size, and display across launches
- Contain wheel scrolling in activity and command output viewports
- Smooth streaming markdown and reduce CPU usage while streaming

## [0.0.13]

- Add DeepSeek Harness provider
- Render user message as Markdown and linkify bare URLs
- Share one resident OpenCode serve per workspace across sessions

## [0.0.12]

- Inherit the login-shell environment for provider commands
- Fix model traits across provider switches
- Keep branch change counts current and include untracked files
- Normalize SIGCHLD for provider children
- Fix Grok model discovery

## [0.0.11]

- Fix provider detection for CLIs installed through shell PATH managers such as
  nvm and fnm
- Show models registered by Pi extensions
- Fix the model picker closing when entering a space in search
- Fix duplicate transcript history when resuming ACP sessions

## [0.0.10]

- Fix crash in due to IME composition
- Fix typo

## [0.0.9]

- Add OpenCode Go support in usage popover
- Fix app icon
- Fix Cursor model detection

## [0.0.8]

- Initial release
