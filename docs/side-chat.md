# Side chats

A side chat is for a quick question about a task, such as "what does this paragraph mean?", that should not become a turn in the task's conversation. It branches off the task the way a git branch leaves trunk alone. The main agent never sees it, and it cannot change the project.

## Where it lives

- **Standard side chat.** Each task has one unanchored side chat. To open it, use the side-chat button in the header (the speech-bubbles icon next to the info button), **View → Toggle Side Chat**, or `⌘⇧J`.
- **Quoted side chats.** Select text in any message, right-click, and choose **Side chat**. The quote becomes the side chat's subject, and it stays in the transcript with a dashed underline. Clicking the quote, or tabbing to it and pressing Enter, reopens that side chat.
- **Popover.** The header button lists the task's side chats: the standard one first, then quoted ones, newest first. The list supports arrow keys, Enter to open, and Delete or Backspace to remove a side chat.
- **Right panel.** A side chat opens as a right-panel tab (`RightPanelSurface::SideChat`). The quote appears as a pasted-text style chip (first line only, not clickable; hover shows the whole quote): inside the composer until the first prompt is sent, then above that prompt. The tab also shows the provider and model, a **Read-only** badge, the side chat's own transcript, and a composer. Escape returns focus to the main composer.

## Model

A side chat is a hidden `AgentSession` with `side_chat: Some(SideChatOrigin)`:

```rust
SideChatOrigin { parent_session_id, anchor: Option<SideChatAnchor> }
SideChatAnchor { message_id, quote, spans: Vec<SideChatAnchorSpan { index, start, end }> }
```

- **Spans** use the transcript selection's coordinates: the text element's ordinal within the message row (`TextKey::index`) and a byte range. The transcript turns them into `md::render::InlineAnchor`s.
- **Anchor links.** The dashed underline is painted in the text element's canvas underlay, because GPUI underlines have no dashed style. Each anchor joins the element's links under a private `raku-side-chat:<id>` target, so click, Tab focus and Enter reuse the link path.
- **Stale anchors.** An anchor whose range no longer fits its message is not drawn. The side chat itself stays reachable from the popover.
- **Persistence.** The origin is stored in the nullable `sessions.side_chat` JSON column (migration `0006_side_chat`), so a list-only startup knows which sessions are side chats before hydration.
- **Lookups.** Raku keeps a parent → side chats index and a message → anchors index. Both are rebuilt only when side chats are created, deleted or synced, never per frame.
- **Hidden from task lists.** `AgentSession::is_listed_task` and `is_task_draft` keep side chats out of the following:
  - the sidebar
  - the task switcher
  - the command palette
  - project activity
  - New Task draft reuse
- **No task side effects.** A side chat creates no git checkpoints, completion notifications, unread markers or provider titles. Its title comes from its quote.
- **Deletion.** Deleting a task deletes its side chats.

## Context

The first prompt carries hidden context. The user's bubble shows only what they typed (`display_content`).

- **Conversation snapshot.** The parent task's user and agent messages, newest kept, capped at 32 KB.
- **Excerpt.** The quoted text, when the side chat was started from a selection.

Later turns in the main task are not pushed into the side chat.

## Provider and model

Set them in **Settings → Side Chat**. The defaults are **Pi**, a discovered
**DeepSeek** model, and reasoning **Low**. Provider and Model always show
concrete choices. Pi prefers its own default model when that model is DeepSeek,
then the first discovered DeepSeek model, then Pi's default if none is offered.
Other providers use their own default model until a model is selected. Side
chats stay on the selected provider; an unavailable provider must be installed,
enabled, or changed in Settings.

Set **Reasoning** in the same settings page to choose the effort for new side
chats. The options come from the selected model's capabilities; models without
reasoning options hide the row.
Changing the provider or model resets the effort to `low` when offered,
otherwise the model's default. Existing side chats keep their own effort.

## Read-only tool policy

`DriverStartOptions::read_only` (wire field `readOnly`, protocol 18) replaces the interactive access mode with a read-only execution policy:

| Provider | Launch |
| --- | --- |
| Pi | `--tools read,grep,find,ls,web_search,fetch_content,get_search_content,source_check --no-mcp --no-skills --no-prompt-templates`. The web tools come from the `pi-web-access` extension when it is installed; unregistered names are ignored. The title extension is not loaded. |
| Claude Code | `--tools Read,Grep,Glob,WebSearch,WebFetch --allowedTools Read,Grep,Glob,WebSearch,WebFetch --permission-mode dontAsk --strict-mcp-config --disable-slash-commands`. Raku requests no title and runs no title polling. |
| Codex | approval `never`, sandbox `read-only` on the thread and on every turn. Web search follows the user's Codex configuration. No Raku title turn. |

All three replace Raku's run-block instructions with `SIDE_CHAT_INSTRUCTIONS`, which tell the model to answer from the shared conversation first and to search project files or the web when the question needs more.

The policy was verified against the real CLIs:

- **Pi 1.5.0 with DeepSeek V4.1 Flash** exposed only `read` and `ls`, and did not create the requested file.
- **Claude Code 2.1.316** answered that it could not create the file.
