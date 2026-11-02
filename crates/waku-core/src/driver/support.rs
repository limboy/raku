//! Helpers shared by the provider drivers: stderr triage and tool-name
//! classification.

use serde_json::Value;

use crate::model::ActivityKind;

/// The context-window occupancy of one API call from a Claude-wire `usage`
/// object (Claude Code and Amp share the format): prompt (fresh + cached) plus
/// output. Multi-call messages carry per-call `iterations`; the last one is
/// the live context, and summed outer fields would double-count cache reads.
pub(super) fn claude_context_tokens(usage: &Value) -> Option<u64> {
    let call = usage
        .get("iterations")
        .and_then(Value::as_array)
        .and_then(|iterations| iterations.last())
        .unwrap_or(usage);
    let field = |name: &str| call.get(name).and_then(Value::as_u64).unwrap_or(0);
    let total = field("input_tokens")
        + field("cache_read_input_tokens")
        + field("cache_creation_input_tokens")
        + field("output_tokens");
    (total > 0).then_some(total)
}

pub(super) fn provider_stderr_error(lines: Vec<String>) -> Option<String> {
    let first_error = lines
        .iter()
        .find(|line| line.to_ascii_lowercase().contains("error"))?
        .trim();

    // CLI parsers can echo a rejected multi-line argument in full. The first
    // diagnostic already identifies the failure; forwarding the rest would
    // turn provider stderr into an enormous assistant message.
    if first_error.to_ascii_lowercase().starts_with("error:") {
        return Some(truncate_error(first_error, 400));
    }

    let mut message = String::new();
    let first_error_index = lines
        .iter()
        .position(|line| line.trim() == first_error)
        .unwrap_or_default();
    for line in lines.iter().skip(first_error_index).take(6) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !message.is_empty() {
            message.push('\n');
        }
        message.push_str(line);
        if message.chars().count() >= 800 {
            break;
        }
    }
    Some(truncate_error(&message, 800))
}

fn truncate_error(message: &str, max_chars: usize) -> String {
    if message.chars().count() <= max_chars {
        return message.to_owned();
    }
    let mut truncated = message.chars().take(max_chars).collect::<String>();
    truncated.push('…');
    truncated
}

pub(super) fn classify_tool(name: &str) -> ActivityKind {
    ActivityKind::from_tool_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_tools_are_plans_not_file_writes() {
        assert_eq!(classify_tool("TodoWrite"), ActivityKind::Plan);
        assert_eq!(classify_tool("todo_write"), ActivityKind::Plan);
        assert_eq!(classify_tool("apply_patch"), ActivityKind::FileChange);
        assert_eq!(classify_tool("read"), ActivityKind::FileRead);
        assert_eq!(classify_tool("ReadFile"), ActivityKind::FileRead);
        assert_eq!(classify_tool("grep"), ActivityKind::FileSearch);
        assert_eq!(classify_tool("glob"), ActivityKind::FileSearch);
        assert_eq!(classify_tool("ls"), ActivityKind::FileList);
        assert_eq!(classify_tool("websearch"), ActivityKind::Search);
        assert_eq!(classify_tool("create_thread"), ActivityKind::Tool);
        assert_eq!(classify_tool("read_mcp_resource"), ActivityKind::Tool);
        assert_eq!(classify_tool("list_threads"), ActivityKind::Tool);
    }

    #[test]
    fn provider_stderr_keeps_cli_argument_errors_compact() {
        let message = provider_stderr_error(vec![
            "error: unexpected argument '---".into(),
            "name: waku-test-skill".into(),
            "description: a very long bundled skill".into(),
            "---' found".into(),
            "tip: to pass it as a value, use '-- ---'".into(),
        ]);

        assert_eq!(message.as_deref(), Some("error: unexpected argument '---"));
    }

    #[test]
    fn provider_stderr_ignores_non_error_diagnostics() {
        assert_eq!(
            provider_stderr_error(vec!["warning: optional integration unavailable".into()]),
            None
        );
    }
}
