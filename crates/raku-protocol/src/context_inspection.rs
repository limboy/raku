//! What occupies a live session's context window, and what Raku itself hands
//! the provider. The Context inspector keeps the last recorded data available
//! after the provider stops and across app restarts.

use serde::{Deserialize, Serialize};

/// How much work the provider spends on a breakdown. `Full` may call the
/// provider's token-count API per category; `Summary` answers from the last
/// response and local estimates.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ContextDetail {
    #[default]
    Full,
    Summary,
}

/// What a category row represents. Classify on this, never on the provider's
/// English name, which is display text only.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ContextCategoryKind {
    /// Content that occupies the window.
    Used,
    /// The window still available.
    Free,
    /// Reserved for compaction.
    Buffer,
    /// Out of the window until loaded, such as deferred tool schemas.
    Deferred,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextCategory {
    /// The provider's stable row id, when it reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub tokens: u64,
    pub kind: ContextCategoryKind,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextRow {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub tokens: u64,
}

/// One drill-down list — memory files, MCP tools, tool results by type — in a
/// provider-neutral shape so a new provider field needs no new UI type.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSection {
    pub title: String,
    /// Rows sorted by tokens, largest first.
    pub rows: Vec<ContextRow>,
}

impl ContextSection {
    pub fn new(title: impl Into<String>, mut rows: Vec<ContextRow>) -> Self {
        rows.sort_by_key(|row| std::cmp::Reverse(row.tokens));
        Self {
            title: title.into(),
            rows,
        }
    }

    pub fn total_tokens(&self) -> u64 {
        self.rows.iter().map(|row| row.tokens).sum()
    }
}

/// The token counters of the latest model call, for providers that report
/// counters instead of categories.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenSplit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
}

impl TokenSplit {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A provider-reported tool. Token accounting and tool discovery are separate:
/// a tool can be available on demand without its schema occupying the window.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deferred: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextToolServer {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub tools: Vec<ContextTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `None` means the provider did not report a catalog; `Some([])` is a
/// confirmed empty catalog. Never present missing metadata as zero tools.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextToolCatalog {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<Vec<ContextTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<Vec<ContextToolServer>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextBreakdown {
    pub detail: ContextDetail,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Milliseconds since the Unix epoch, stamped by the driver.
    pub measured_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_threshold: Option<u64>,
    /// Empty when the provider reports no categories.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<ContextCategory>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<ContextSection>,
    #[serde(default, skip_serializing_if = "TokenSplit::is_empty")]
    pub token_split: TokenSplit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ContextToolCatalog>,
}

/// Text Raku adds to the conversation beyond what the user typed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptInjection {
    pub label: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RakuToolContextSource {
    ToolDefinition,
    PromptUpdate,
    ExtensionConfiguration,
}

/// What Raku supplied to this provider runtime, captured from its outgoing
/// payload. A resumed prompt updates the roster without replacing the
/// thread's original tool definition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RakuToolContext {
    pub source: RakuToolContextSource,
    pub recorded_at_ms: u64,
    pub subagents: Vec<crate::model::SubagentToolAgent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definitions: Option<Vec<serde_json::Value>>,
}

/// Exactly how Raku launched the provider for this runtime, captured from the
/// command it spawned so it cannot drift from what actually ran.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchManifest {
    pub program: String,
    /// The argument list, with the appended instructions folded to a
    /// placeholder so the full text appears once, in `appended_instructions`.
    pub args: Vec<String>,
    /// Only variables Raku sets itself. The inherited login environment is
    /// left out, so no user secrets cross into the inspector.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// System-prompt text Raku adds to the provider's own.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub appended_instructions: String,
    /// Tools Raku serves to the model itself (the `subagent` tool).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub raku_tools: Vec<String>,
    /// Session-level text Raku adds outside the system prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub injections: Vec<PromptInjection>,
}

/// The placeholder standing for the appended instructions inside `args`.
pub fn instructions_placeholder(instructions: &str) -> String {
    format!(
        "<appended instructions, {} chars>",
        instructions.chars().count()
    )
}

/// Rough tokens for text the provider does not count for us, at four
/// characters per token. Always labelled as an estimate in the UI.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_sort_largest_first() {
        let section = ContextSection::new(
            "Tools",
            vec![
                ContextRow {
                    label: "small".into(),
                    detail: None,
                    tokens: 3,
                },
                ContextRow {
                    label: "large".into(),
                    detail: None,
                    tokens: 30,
                },
            ],
        );
        assert_eq!(section.rows[0].label, "large");
        assert_eq!(section.total_tokens(), 33);
    }

    #[test]
    fn breakdown_round_trips_without_empty_fields() {
        let breakdown = ContextBreakdown {
            detail: ContextDetail::Summary,
            model: None,
            measured_at_ms: 1,
            total_tokens: Some(10),
            window: None,
            auto_compact_threshold: None,
            categories: Vec::new(),
            sections: Vec::new(),
            token_split: TokenSplit::default(),
            tools: None,
        };
        let value = serde_json::to_value(&breakdown).unwrap();
        assert!(value.get("categories").is_none());
        assert!(value.get("tokenSplit").is_none());
        assert!(value.get("tools").is_none());
        assert_eq!(
            serde_json::from_value::<ContextBreakdown>(value).unwrap(),
            breakdown
        );
    }

    #[test]
    fn unknown_and_empty_tool_catalogs_remain_distinct() {
        let catalog = ContextToolCatalog {
            builtin: None,
            mcp: Some(Vec::new()),
            error: None,
        };
        let value = serde_json::to_value(&catalog).unwrap();
        assert!(value.get("builtin").is_none());
        assert_eq!(value["mcp"], serde_json::json!([]));
        assert_eq!(
            serde_json::from_value::<ContextToolCatalog>(value).unwrap(),
            catalog
        );
    }
}
