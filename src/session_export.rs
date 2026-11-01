//! Markdown snapshots of hydrated sessions. Formatting and file I/O belong on
//! a worker; this never reads the transcript's folded or virtualized UI rows.

use std::fmt::Write as _;

use crate::model::{ActivityItem, AgentSession, MessageAttachment, MessageRole};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExportScope {
    All,
    Conversation,
}

pub(crate) fn suggested_filename(session: &AgentSession) -> String {
    let mut name = String::new();
    for ch in session.display_title().chars() {
        let ch = if ch.is_control()
            || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        {
            '-'
        } else {
            ch
        };
        // Leave room for the extension, including on filesystems with a
        // 255-byte component limit. Do not split a Unicode code point.
        if name.len() + ch.len_utf8() > 180 {
            break;
        }
        name.push(ch);
    }
    let name = name.trim_matches(|ch: char| ch.is_whitespace() || ch == '.');
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            });
    if name.is_empty() {
        format!("Session-{}.md", session.id)
    } else if reserved {
        format!("Session-{name}.md")
    } else {
        format!("{name}.md")
    }
}

pub(crate) fn markdown(session: &AgentSession, scope: ExportScope) -> anyhow::Result<String> {
    anyhow::ensure!(
        session.detail_loaded,
        "the session transcript has not been loaded"
    );
    let mut out = String::new();
    writeln!(out, "# {}\n", inline(session.display_title())).unwrap();
    writeln!(out, "- Session: {}", session.id).unwrap();
    writeln!(out, "- Provider: {}", session.provider.display_name()).unwrap();
    if let Some(model) = &session.model {
        writeln!(out, "- Model: {}", inline(model)).unwrap();
    }
    writeln!(out, "- Created: {}", timestamp(session.created_at)).unwrap();
    writeln!(out, "- Updated: {}\n", timestamp(session.updated_at)).unwrap();

    // Blocks are anchored between messages. Stable sorting preserves provider
    // order for shared anchors, including blocks beyond the current tail.
    let mut blocks = if scope == ExportScope::All {
        session.transcript_blocks.iter().collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    blocks.sort_by_key(|block| block.after_message.min(session.messages.len()));
    let mut blocks = blocks.into_iter().peekable();
    for index in 0..=session.messages.len() {
        while let Some(block) =
            blocks.next_if(|block| block.after_message.min(session.messages.len()) == index)
        {
            for activity in &block.activities {
                write_activity(&mut out, activity);
            }
        }
        let Some(message) = session.messages.get(index) else {
            break;
        };
        if scope == ExportScope::Conversation && message.role == MessageRole::System {
            continue;
        }
        let role = match message.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System => "System",
        };
        writeln!(
            out,
            "---\n\n## {role}\n\n{}\n",
            timestamp(message.created_at)
        )
        .unwrap();
        body(&mut out, message.visible_content());
        write_attachments(&mut out, &message.attachments);
    }
    if scope == ExportScope::All && !session.queued_messages.is_empty() {
        out.push_str("---\n\n## Queued messages\n\n");
        for message in &session.queued_messages {
            writeln!(
                out,
                "### User (queued)\n\n{}\n",
                timestamp(message.created_at)
            )
            .unwrap();
            body(&mut out, message.visible_content());
            write_attachments(&mut out, &message.attachments);
        }
    }
    Ok(out)
}

fn write_activity(out: &mut String, activity: &ActivityItem) {
    use waku_protocol::compaction::{CompactionStatus, CompactionTrigger};
    let title = if activity.reasoning.is_some() {
        "Reasoning"
    } else {
        &activity.title
    };
    writeln!(out, "---\n\n### {}\n", inline(title)).unwrap();
    let status = if let Some(item) = &activity.compaction {
        match item.status {
            CompactionStatus::Running => "In progress",
            CompactionStatus::Completed => "Completed",
            CompactionStatus::Unconfirmed => "Unconfirmed",
            CompactionStatus::Failed => "Failed",
            CompactionStatus::Interrupted => "Interrupted",
        }
    } else if activity.failed {
        "Failed"
    } else if activity.complete {
        "Completed"
    } else {
        "In progress"
    };
    writeln!(out, "Status: {status}\n").unwrap();
    if let Some(item) = &activity.compaction {
        if let Some(trigger) = item.trigger {
            writeln!(
                out,
                "Trigger: {}\n",
                match trigger {
                    CompactionTrigger::Auto => "Automatic",
                    CompactionTrigger::Manual => "Manual",
                }
            )
            .unwrap();
        }
        if let Some(tokens) = item.before_tokens {
            writeln!(out, "Tokens before: {tokens}\n").unwrap();
        }
        if let Some(tokens) = item.after_tokens {
            writeln!(
                out,
                "Tokens after: {}{tokens}\n",
                if item.after_tokens_estimated {
                    "≈"
                } else {
                    ""
                }
            )
            .unwrap();
        }
        if item.started_at_ms > 0
            && let Some(end) = item.completed_at_ms
        {
            writeln!(
                out,
                "Duration: {:.1}s\n",
                end.saturating_sub(item.started_at_ms) as f64 / 1000.0
            )
            .unwrap();
        }
        let summary = item.summary_for_display();
        for (label, text) in [
            ("Summary", summary.as_deref()),
            ("Notice", item.notice.as_deref()),
            ("Error", item.error.as_deref()),
        ] {
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                writeln!(out, "#### {label}\n").unwrap();
                code(out, text, "text");
            }
        }
    }
    if let Some(reasoning) = &activity.reasoning {
        body(out, &reasoning.content);
    }
    if let Some(tool) = &activity.tool_name {
        writeln!(out, "Tool: {}\n", inline(tool)).unwrap();
    }
    if let Some(server) = &activity.mcp_server {
        writeln!(out, "MCP server: {}\n", inline(server)).unwrap();
    }
    for (label, value) in [
        ("Details", activity.detail.as_deref()),
        ("Arguments", activity.arguments.as_deref()),
        ("Output", activity.output.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            writeln!(out, "#### {label}\n").unwrap();
            code(out, value, "text");
        }
    }
    for change in &activity.file_changes {
        writeln!(out, "#### File: {}\n", inline(&change.path)).unwrap();
        if let Some(diff) = &change.diff {
            code(out, diff, "diff");
        }
    }
    for (index, url) in activity.image_urls.iter().enumerate() {
        writeln!(out, "![Tool image {}](<{}>)\n", index + 1, destination(url)).unwrap();
    }
}

fn write_attachments(out: &mut String, attachments: &[MessageAttachment]) {
    if attachments.is_empty() {
        return;
    }
    out.push_str("Attachments:\n\n");
    for attachment in attachments {
        let url = url::Url::from_file_path(&attachment.path)
            .map(|url| url.to_string())
            .unwrap_or_else(|_| attachment.mention.clone());
        let prefix = if attachment.is_image { "!" } else { "" };
        writeln!(
            out,
            "- {prefix}[{}](<{}>)",
            inline(&attachment.name),
            destination(&url)
        )
        .unwrap();
    }
    out.push('\n');
}

fn timestamp(seconds: u64) -> String {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| seconds.to_string())
}

fn inline(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            out.push(' ');
        } else {
            if matches!(ch, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#') {
                out.push('\\');
            }
            out.push(ch);
        }
    }
    out
}

fn destination(url: &str) -> String {
    url.replace('\n', "%0A")
        .replace('\r', "%0D")
        .replace('<', "%3C")
        .replace('>', "%3E")
}

fn body(out: &mut String, text: &str) {
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
}

fn code(out: &mut String, text: &str, language: &str) {
    let longest = text.split(|ch| ch != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(3.max(longest + 1));
    writeln!(out, "{fence}{language}").unwrap();
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    writeln!(out, "{fence}\n").unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        ActivityFileChange, ActivityKind, Message, ProviderKind, QueuedMessage, ReasoningBlock,
        TranscriptBlock,
    };
    use uuid::Uuid;

    fn session() -> AgentSession {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        session.title = "导出 [Markdown]".into();
        session.messages = vec![
            Message::new(MessageRole::User, "User prompt"),
            Message::new(MessageRole::Assistant, "Interim reply"),
            Message::new(
                MessageRole::Assistant,
                "Final answer\n\n```rust\nfn main() {}\n```\n\n[Source](https://example.com)",
            ),
            Message::new(MessageRole::System, "System notice"),
        ];
        let reasoning = ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "Reasoning text".into(),
                started_at_ms: 0,
                finished_at_ms: 1,
            },
            true,
        );
        let tool = ActivityItem::new(None, ActivityKind::Command, "Run command", None, true)
            .with_arguments(Some("printf '```'".into()))
            .with_output(Some("Tool output\n```\n#### raw heading".into()))
            .with_failed(true);
        // Deliberately stored out of anchor order, with a shared anchor and
        // an over-tail anchor, matching the transcript renderer's rules.
        session.transcript_blocks = vec![
            TranscriptBlock {
                after_message: 2,
                turn_id: None,
                activities: vec![tool],
            },
            TranscriptBlock {
                after_message: 1,
                turn_id: None,
                activities: vec![reasoning],
            },
            TranscriptBlock {
                after_message: 2,
                turn_id: None,
                activities: vec![ActivityItem::new(
                    None,
                    ActivityKind::Tool,
                    "Second tool",
                    None,
                    true,
                )],
            },
            TranscriptBlock {
                after_message: 99,
                turn_id: None,
                activities: vec![ActivityItem::new(
                    None,
                    ActivityKind::Tool,
                    "Tail tool",
                    None,
                    false,
                )],
            },
        ];
        session
            .queued_messages
            .push(QueuedMessage::new("Pending prompt"));
        session
    }

    #[test]
    fn all_content_preserves_order_and_fences_tool_output() {
        let out = markdown(&session(), ExportScope::All).unwrap();
        let ordered = [
            "User prompt",
            "Reasoning text",
            "Interim reply",
            "Run command",
            "Second tool",
            "Final answer",
            "System notice",
            "Tail tool",
            "Pending prompt",
        ];
        let offsets = ordered.map(|text| out.find(text).unwrap());
        assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(out.contains("Status: Failed"));
        assert!(out.contains("````text\nTool output\n```\n#### raw heading\n````"));
        assert!(out.contains("```rust\nfn main() {}\n```\n\n[Source](https://example.com)"));
    }

    #[test]
    fn full_export_preserves_compaction_details_and_interrupted_status() {
        use waku_protocol::compaction::{CompactionStatus, CompactionTrigger, ContextCompaction};
        let mut session = session();
        let mut item = ContextCompaction::new("compact", Some(CompactionTrigger::Manual), 1000);
        item.summary = Some("Keep the architecture constraint".into());
        item.error = Some("Provider connection lost".into());
        item.before_tokens = Some(180000);
        item.after_tokens = Some(20000);
        item.after_tokens_estimated = true;
        item.finish(CompactionStatus::Interrupted, 2500);
        session.transcript_blocks[0]
            .activities
            .push(ActivityItem::from_compaction(item));
        let out = markdown(&session, ExportScope::All).unwrap();
        for text in [
            "Status: Interrupted",
            "Trigger: Manual",
            "Tokens before: 180000",
            "Tokens after: ≈20000",
            "Duration: 1.5s",
            "Keep the architecture constraint",
            "Provider connection lost",
        ] {
            assert!(out.contains(text), "missing {text}: {out}");
        }
    }

    #[test]
    fn conversation_only_keeps_messages_and_attachments_without_work_or_system_records() {
        let mut session = session();
        session.messages[0].content = "Provider prompt with private attachment mention".into();
        session.messages[0].display_content = Some("User prompt".into());
        session.messages[0].attachments.push(MessageAttachment {
            path: if cfg!(windows) {
                "C:/tmp/a [b].png"
            } else {
                "/tmp/a [b].png"
            }
            .into(),
            mention: "@a [b].png".into(),
            name: "a [b].png".into(),
            is_dir: false,
            is_image: true,
            blob_reference: None,
            pasted_text: None,
        });
        let out = markdown(&session, ExportScope::Conversation).unwrap();
        for included in [
            "User prompt",
            "Interim reply",
            "Final answer",
            if cfg!(windows) {
                "![a \\[b\\].png](<file:///C:/tmp/a%20[b].png>)"
            } else {
                "![a \\[b\\].png](<file:///tmp/a%20[b].png>)"
            },
        ] {
            assert!(out.contains(included), "missing {included}: {out}");
        }
        for excluded in [
            "Reasoning text",
            "Run command",
            "Tool output",
            "Second tool",
            "System notice",
            "Tail tool",
            "Pending prompt",
            "private attachment mention",
        ] {
            assert!(!out.contains(excluded), "unexpected {excluded}");
        }
    }

    #[test]
    fn full_export_keeps_stored_details_diffs_and_tool_images_without_truncation() {
        let mut session = session();
        let activity = &mut session.transcript_blocks[0].activities[0];
        activity.detail = Some("Tool details".into());
        activity.tool_name = Some("apply_patch".into());
        activity.mcp_server = Some("workspace".into());
        let output = format!("{}\nEND OF OUTPUT", "output line\n".repeat(5_000));
        activity.output = Some(output.clone());
        activity.file_changes.push(ActivityFileChange {
            path: "src/main.rs".into(),
            additions: Some(1),
            deletions: Some(1),
            status: None,
            diff: Some("@@ -1 +1 @@\n-old\n+new".into()),
        });
        activity
            .image_urls
            .push("data:image/png;base64,aGVsbG8=".into());
        let out = markdown(&session, ExportScope::All).unwrap();
        for included in [
            "Tool details",
            "Tool: apply\\_patch",
            "MCP server: workspace",
            &output,
            "```diff\n@@ -1 +1 @@\n-old\n+new\n```",
            "![Tool image 1](<data:image/png;base64,aGVsbG8=>)",
        ] {
            assert!(out.contains(included), "missing full export data");
        }
    }

    #[test]
    fn skeleton_sessions_must_be_hydrated_before_export() {
        let session = session().list_projection();
        assert!(markdown(&session, ExportScope::All).is_err());
        assert!(markdown(&session, ExportScope::Conversation).is_err());
    }

    #[test]
    fn filenames_preserve_unicode_without_paths_or_overlong_components() {
        let mut session = session();
        session.title = "../导出/内容:\\测试?".into();
        assert_eq!(suggested_filename(&session), "-导出-内容--测试-.md");
        session.title = "导".repeat(200);
        let name = suggested_filename(&session);
        assert!(name.len() <= 183);
        assert!(name.ends_with(".md"));
        session.title = "CON".into();
        assert_eq!(suggested_filename(&session), "Session-CON.md");
    }
}
