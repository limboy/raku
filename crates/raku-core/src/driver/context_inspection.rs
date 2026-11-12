//! Maps each provider's native usage payload into a [`ContextBreakdown`] and
//! captures the [`LaunchManifest`] of the process Raku spawned.

use std::ffi::OsStr;
use std::process::Command;

use raku_protocol::context_inspection::{
    ContextBreakdown, ContextCategory, ContextCategoryKind, ContextDetail, ContextRow,
    ContextSection, ContextTool, ContextToolCatalog, ContextToolServer, LaunchManifest,
    PromptInjection, RakuToolContext, RakuToolContextSource, TokenSplit, instructions_placeholder,
};
use serde_json::Value;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

pub(crate) fn raku_tool_context(
    source: RakuToolContextSource,
    subagents: &[crate::model::SubagentToolAgent],
    definitions: Option<Vec<Value>>,
) -> RakuToolContext {
    RakuToolContext {
        source,
        recorded_at_ms: now_ms(),
        subagents: subagents.to_vec(),
        definitions,
    }
}

/// Build the manifest from the very `Command` about to be spawned. Only the
/// variables Raku sets on top of the login environment are kept: the login
/// environment is the user's own and may hold credentials.
pub(crate) fn launch_manifest(
    command: &Command,
    appended_instructions: &str,
    raku_tools: Vec<String>,
    injections: Vec<PromptInjection>,
) -> LaunchManifest {
    let placeholder = instructions_placeholder(appended_instructions);
    let args = command
        .get_args()
        .map(|arg| {
            let arg = arg.to_string_lossy();
            if !appended_instructions.is_empty() && arg == appended_instructions {
                placeholder.clone()
            } else {
                arg.into_owned()
            }
        })
        .collect();
    let shell = crate::command_env::shell_environment();
    let env = command
        .get_envs()
        .filter_map(|(key, value)| Some((key, value?)))
        .filter(|(key, value)| {
            *key != OsStr::new("PATH")
                && !shell
                    .iter()
                    .any(|(shell_key, shell_value)| shell_key == key && shell_value == value)
        })
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    LaunchManifest {
        program: command.get_program().to_string_lossy().into_owned(),
        args,
        env,
        appended_instructions: appended_instructions.to_owned(),
        raku_tools,
        injections,
    }
}

fn u64_at(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn array_at<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn row(label: impl Into<String>, detail: Option<String>, tokens: u64) -> ContextRow {
    ContextRow {
        label: label.into(),
        detail,
        tokens,
    }
}

/// Rows from a list of `{ <label_key>, tokens }` objects.
fn rows(
    items: &[Value],
    label_key: &str,
    detail: impl Fn(&Value) -> Option<String>,
) -> Vec<ContextRow> {
    items
        .iter()
        .filter_map(|item| {
            Some(row(
                str_at(item, label_key)?,
                detail(item),
                u64_at(item, "tokens").unwrap_or_default(),
            ))
        })
        .collect()
}

fn push_section(sections: &mut Vec<ContextSection>, title: String, rows: Vec<ContextRow>) {
    if !rows.is_empty() {
        sections.push(ContextSection::new(title, rows));
    }
}

fn claude_category_kind(category: &Value) -> ContextCategoryKind {
    match str_at(category, "kind") {
        Some("free") => ContextCategoryKind::Free,
        Some("buffer") => ContextCategoryKind::Buffer,
        Some("deferred") => ContextCategoryKind::Deferred,
        Some(_) => ContextCategoryKind::Used,
        // A CLI that predates `kind` only flags deferred rows; the rest of
        // its rows are content, plus the free-space and buffer rows that
        // sit at the end. Ids, unlike names, are stable.
        None if category.get("isDeferred").and_then(Value::as_bool) == Some(true) => {
            ContextCategoryKind::Deferred
        }
        None => match str_at(category, "id") {
            Some("free_space" | "free") => ContextCategoryKind::Free,
            Some("autocompact_buffer" | "buffer") => ContextCategoryKind::Buffer,
            _ => ContextCategoryKind::Used,
        },
    }
}

/// Claude Code's `get_context_usage` control response payload.
pub(crate) fn claude_breakdown(payload: &Value, detail: ContextDetail) -> ContextBreakdown {
    let categories = array_at(payload, "categories")
        .iter()
        .filter_map(|category| {
            Some(ContextCategory {
                id: str_at(category, "id").map(str::to_owned),
                name: str_at(category, "name")?.to_owned(),
                tokens: u64_at(category, "tokens").unwrap_or_default(),
                kind: claude_category_kind(category),
            })
        })
        .collect();

    let mut sections = Vec::new();
    push_section(
        &mut sections,
        tr!("context_inspector.section_memory_files"),
        rows(array_at(payload, "memoryFiles"), "path", |item| {
            str_at(item, "type").map(str::to_owned)
        }),
    );
    push_section(
        &mut sections,
        tr!("context_inspector.section_system_prompt"),
        rows(array_at(payload, "systemPromptSections"), "name", |_| None),
    );
    push_section(
        &mut sections,
        tr!("context_inspector.section_system_tools"),
        rows(array_at(payload, "systemTools"), "name", |_| None),
    );
    push_section(
        &mut sections,
        tr!("context_inspector.section_deferred_tools"),
        rows(array_at(payload, "deferredBuiltinTools"), "name", |item| {
            loaded_detail(item)
        }),
    );
    push_section(
        &mut sections,
        tr!("context_inspector.section_mcp_tools"),
        rows(array_at(payload, "mcpTools"), "name", |item| {
            let server = str_at(item, "serverName").map(str::to_owned);
            match (server, loaded_detail(item)) {
                (Some(server), Some(loaded)) => Some(format!("{server} · {loaded}")),
                (server, loaded) => server.or(loaded),
            }
        }),
    );
    push_section(
        &mut sections,
        tr!("context_inspector.section_mcp_instructions"),
        rows(array_at(payload, "mcpInstructions"), "serverName", |_| None),
    );
    push_section(
        &mut sections,
        tr!("context_inspector.section_agents"),
        rows(array_at(payload, "agents"), "agentType", |item| {
            str_at(item, "source").map(str::to_owned)
        }),
    );
    if let Some(skills) = payload.get("skills") {
        push_section(
            &mut sections,
            tr!(
                "context_inspector.section_skills",
                included = u64_at(skills, "includedSkills").unwrap_or_default(),
                total = u64_at(skills, "totalSkills").unwrap_or_default()
            ),
            rows(array_at(skills, "skillFrontmatter"), "name", |item| {
                str_at(item, "pluginName")
                    .or_else(|| str_at(item, "source"))
                    .map(str::to_owned)
            }),
        );
    }
    if let Some(messages) = payload.get("messageBreakdown") {
        let summary = [
            ("userMessageTokens", "context_inspector.messages_user"),
            (
                "assistantMessageTokens",
                "context_inspector.messages_assistant",
            ),
            ("toolCallTokens", "context_inspector.messages_tool_calls"),
            (
                "toolResultTokens",
                "context_inspector.messages_tool_results",
            ),
            ("attachmentTokens", "context_inspector.messages_attachments"),
            (
                "redirectedContextTokens",
                "context_inspector.messages_redirected",
            ),
            (
                "unattributedTokens",
                "context_inspector.messages_unattributed",
            ),
        ]
        .into_iter()
        .filter_map(|(key, label)| {
            u64_at(messages, key)
                .filter(|tokens| *tokens > 0)
                .map(|tokens| row(tr!(label), None, tokens))
        })
        .collect();
        push_section(
            &mut sections,
            tr!("context_inspector.section_messages"),
            summary,
        );
        let tools = array_at(messages, "toolCallsByType")
            .iter()
            .filter_map(|item| {
                let call = u64_at(item, "callTokens").unwrap_or_default();
                let result = u64_at(item, "resultTokens").unwrap_or_default();
                Some(row(
                    str_at(item, "name")?,
                    Some(tr!(
                        "context_inspector.tool_call_detail",
                        call = call,
                        result = result
                    )),
                    call + result,
                ))
            })
            .collect();
        push_section(
            &mut sections,
            tr!("context_inspector.section_tool_use"),
            tools,
        );
        push_section(
            &mut sections,
            tr!("context_inspector.section_attachments"),
            rows(array_at(messages, "attachmentsByType"), "name", |_| None),
        );
    }

    let token_split = payload
        .get("apiUsage")
        .map(|usage| TokenSplit {
            input: u64_at(usage, "input_tokens"),
            cache_read: u64_at(usage, "cache_read_input_tokens"),
            cache_write: u64_at(usage, "cache_creation_input_tokens"),
            output: u64_at(usage, "output_tokens"),
            reasoning: None,
        })
        .unwrap_or_default();

    ContextBreakdown {
        detail,
        model: str_at(payload, "model").map(str::to_owned),
        measured_at_ms: now_ms(),
        total_tokens: u64_at(payload, "totalTokens"),
        window: u64_at(payload, "maxTokens").filter(|window| *window > 0),
        auto_compact_threshold: u64_at(payload, "autoCompactThreshold"),
        categories,
        sections,
        token_split,
        tools: Some(claude_tool_catalog(payload)),
    }
}

fn reported_tool(value: &Value) -> Option<ContextTool> {
    Some(ContextTool {
        name: str_at(value, "name")?.to_owned(),
        description: str_at(value, "description").map(str::to_owned),
        deferred: value.get("isLoaded").and_then(Value::as_bool) == Some(false),
    })
}

fn claude_tool_catalog(payload: &Value) -> ContextToolCatalog {
    let builtin = payload
        .get("systemTools")
        .and_then(Value::as_array)
        .map(|tools| {
            let mut tools = tools
                .iter()
                .chain(array_at(payload, "deferredBuiltinTools"))
                .filter_map(reported_tool)
                .collect::<Vec<_>>();
            tools.sort_by(|left, right| left.name.cmp(&right.name));
            tools.dedup_by(|left, right| left.name == right.name);
            tools
        });
    let mcp = payload
        .get("mcpTools")
        .and_then(Value::as_array)
        .map(|tools| {
            let mut servers = std::collections::BTreeMap::<String, Vec<ContextTool>>::new();
            for tool in tools {
                if let (Some(server), Some(tool)) =
                    (str_at(tool, "serverName"), reported_tool(tool))
                {
                    servers.entry(server.to_owned()).or_default().push(tool);
                }
            }
            servers
                .into_iter()
                .map(|(name, mut tools)| {
                    tools.sort_by(|left, right| left.name.cmp(&right.name));
                    ContextToolServer {
                        name,
                        tools,
                        status: None,
                        error: None,
                    }
                })
                .collect()
        });
    ContextToolCatalog {
        builtin,
        mcp,
        error: None,
    }
}

/// One page of Codex's `mcpServerStatus/list` result. Keep server failures and
/// empty catalogs as well as tools; neither means the catalog is unavailable.
pub(crate) fn codex_tool_servers(payload: &Value) -> Vec<ContextToolServer> {
    array_at(payload, "data")
        .iter()
        .filter_map(|server| {
            let mut tools = server
                .get("tools")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|tools| tools.values())
                .filter_map(reported_tool)
                .collect::<Vec<_>>();
            tools.sort_by(|left, right| left.name.cmp(&right.name));
            Some(ContextToolServer {
                name: str_at(server, "name")?.to_owned(),
                status: str_at(server, "runtimeStatus").map(str::to_owned),
                tools,
                error: str_at(server, "toolsError").map(str::to_owned),
            })
        })
        .collect()
}

fn loaded_detail(item: &Value) -> Option<String> {
    (item.get("isLoaded").and_then(Value::as_bool) == Some(false))
        .then(|| tr!("context_inspector.not_loaded"))
}

/// Codex's `thread/tokenUsage/updated` `tokenUsage` object. Codex reports
/// counters, not categories; `last` is the newest model call.
pub(crate) fn codex_breakdown(token_usage: Option<&Value>) -> ContextBreakdown {
    let last = token_usage.and_then(|usage| usage.get("last"));
    let total = token_usage.and_then(|usage| usage.get("total"));
    let mut sections = Vec::new();
    if let Some(total) = total {
        push_section(
            &mut sections,
            tr!("context_inspector.section_thread_totals"),
            codex_counter_rows(total),
        );
    }
    ContextBreakdown {
        detail: ContextDetail::Full,
        model: None,
        measured_at_ms: now_ms(),
        total_tokens: last
            .and_then(|last| u64_at(last, "totalTokens"))
            .filter(|tokens| *tokens > 0),
        window: token_usage
            .and_then(|usage| u64_at(usage, "modelContextWindow"))
            .filter(|window| *window > 0),
        auto_compact_threshold: None,
        categories: Vec::new(),
        sections,
        token_split: last
            .map(|last| TokenSplit {
                input: u64_at(last, "inputTokens"),
                cache_read: u64_at(last, "cachedInputTokens"),
                cache_write: None,
                output: u64_at(last, "outputTokens"),
                reasoning: u64_at(last, "reasoningOutputTokens"),
            })
            .unwrap_or_default(),
        tools: None,
    }
}

fn codex_counter_rows(counters: &Value) -> Vec<ContextRow> {
    [
        ("inputTokens", "context_inspector.split_input"),
        ("cachedInputTokens", "context_inspector.split_cache_read"),
        ("outputTokens", "context_inspector.split_output"),
        ("reasoningOutputTokens", "context_inspector.split_reasoning"),
    ]
    .into_iter()
    .filter_map(|(key, label)| u64_at(counters, key).map(|tokens| row(tr!(label), None, tokens)))
    .collect()
}

/// Pi's `get_session_stats` data plus the usage of its newest assistant
/// message. Session stats are cumulative, so the split of what occupies the
/// window right now comes from that last message instead.
pub(crate) fn pi_breakdown(stats: &Value, last_usage: Option<&Value>) -> ContextBreakdown {
    let data = stats.get("data").unwrap_or(stats);
    let context = data.get("contextUsage");
    let mut sections = Vec::new();
    let counts = [
        ("userMessages", "context_inspector.pi_user_messages"),
        (
            "assistantMessages",
            "context_inspector.pi_assistant_messages",
        ),
        ("toolCalls", "context_inspector.pi_tool_calls"),
        ("toolResults", "context_inspector.pi_tool_results"),
    ]
    .into_iter()
    .filter_map(|(key, label)| {
        u64_at(data, key).map(|count| row(tr!(label), Some(count.to_string()), 0))
    })
    .collect::<Vec<_>>();
    if !counts.is_empty() {
        // Counts, not tokens: keep the provider's order rather than sorting.
        sections.push(ContextSection {
            title: tr!("context_inspector.section_session_counts"),
            rows: counts,
        });
    }
    if let Some(tokens) = data.get("tokens") {
        push_section(
            &mut sections,
            tr!("context_inspector.section_session_totals"),
            [
                ("input", "context_inspector.split_input"),
                ("cacheRead", "context_inspector.split_cache_read"),
                ("cacheWrite", "context_inspector.split_cache_write"),
                ("output", "context_inspector.split_output"),
            ]
            .into_iter()
            .filter_map(|(key, label)| {
                u64_at(tokens, key).map(|tokens| row(tr!(label), None, tokens))
            })
            .collect(),
        );
    }
    let usage = last_usage.and_then(|message| message.get("usage"));
    ContextBreakdown {
        detail: ContextDetail::Full,
        model: None,
        measured_at_ms: now_ms(),
        total_tokens: context.and_then(|context| u64_at(context, "tokens")),
        window: context
            .and_then(|context| u64_at(context, "contextWindow"))
            .filter(|window| *window > 0),
        auto_compact_threshold: None,
        categories: Vec::new(),
        sections,
        token_split: usage
            .map(|usage| TokenSplit {
                input: u64_at(usage, "input"),
                cache_read: u64_at(usage, "cacheRead"),
                cache_write: u64_at(usage, "cacheWrite"),
                output: u64_at(usage, "output"),
                reasoning: None,
            })
            .unwrap_or_default(),
        tools: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_payload_maps_categories_by_kind_and_sorts_sections() {
        let payload = json!({
            "categories": [
                {"id": "system_prompt", "name": "System prompt", "tokens": 3000, "color": "x", "kind": "used"},
                {"id": "mcp_tools", "name": "MCP tools", "tokens": 900, "color": "x", "kind": "deferred", "isDeferred": true},
                {"id": "free_space", "name": "Free space", "tokens": 150000, "color": "x", "kind": "free"},
                {"id": "autocompact_buffer", "name": "Autocompact buffer", "tokens": 45000, "color": "x", "kind": "buffer"},
                {"name": "Future row", "tokens": 7, "color": "x"}
            ],
            "totalTokens": 25000,
            "maxTokens": 200000,
            "rawMaxTokens": 200000,
            "percentage": 12.5,
            "model": "claude-opus-5-5",
            "memoryFiles": [
                {"path": "/repo/CLAUDE.md", "type": "Project", "tokens": 400},
                {"path": "/home/.claude/CLAUDE.md", "type": "User", "tokens": 1200}
            ],
            "mcpTools": [
                {"name": "mcp__raku__subagent", "serverName": "raku", "tokens": 300, "isLoaded": true},
                {"name": "mcp__docs__read", "serverName": "docs", "tokens": 80, "isLoaded": false}
            ],
            "skills": {"totalSkills": 4, "includedSkills": 2, "tokens": 60, "skillFrontmatter": [
                {"name": "review", "source": "project", "tokens": 60}
            ]},
            "messageBreakdown": {
                "toolCallTokens": 100, "toolResultTokens": 5000, "attachmentTokens": 0,
                "assistantMessageTokens": 800, "userMessageTokens": 200,
                "redirectedContextTokens": 0, "unattributedTokens": 0,
                "toolCallsByType": [
                    {"name": "Read", "callTokens": 40, "resultTokens": 4000},
                    {"name": "Bash", "callTokens": 60, "resultTokens": 1000}
                ],
                "attachmentsByType": []
            },
            "isAutoCompactEnabled": true,
            "autoCompactThreshold": 155000,
            "apiUsage": {"input_tokens": 5, "output_tokens": 70, "cache_creation_input_tokens": 300, "cache_read_input_tokens": 24000}
        });
        let breakdown = claude_breakdown(&payload, ContextDetail::Full);
        let kinds = breakdown
            .categories
            .iter()
            .map(|category| category.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                ContextCategoryKind::Used,
                ContextCategoryKind::Deferred,
                ContextCategoryKind::Free,
                ContextCategoryKind::Buffer,
                ContextCategoryKind::Used,
            ]
        );
        assert_eq!(breakdown.total_tokens, Some(25000));
        assert_eq!(breakdown.window, Some(200000));
        assert_eq!(breakdown.auto_compact_threshold, Some(155000));
        assert_eq!(breakdown.model.as_deref(), Some("claude-opus-5-5"));
        let memory = &breakdown.sections[0];
        assert_eq!(memory.rows[0].label, "/home/.claude/CLAUDE.md");
        assert_eq!(memory.rows[0].detail.as_deref(), Some("User"));
        let tool_use = breakdown
            .sections
            .iter()
            .find(|section| section.rows.iter().any(|row| row.label == "Read"))
            .unwrap();
        assert_eq!(tool_use.rows[0].tokens, 4040);
        // Zero-token message rows and empty lists are left out.
        assert!(
            breakdown
                .sections
                .iter()
                .all(|section| !section.rows.is_empty())
        );
        assert_eq!(breakdown.token_split.cache_read, Some(24000));
        assert_eq!(breakdown.token_split.cache_write, Some(300));
    }

    #[test]
    fn claude_rows_without_kind_fall_back_to_stable_ids() {
        let payload = json!({"categories": [
            {"id": "free_space", "name": "Espace libre", "tokens": 1},
            {"id": "autocompact_buffer", "name": "Tampon", "tokens": 1},
            {"name": "Deferred", "tokens": 1, "isDeferred": true}
        ]});
        let kinds = claude_breakdown(&payload, ContextDetail::Summary)
            .categories
            .into_iter()
            .map(|category| category.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                ContextCategoryKind::Free,
                ContextCategoryKind::Buffer,
                ContextCategoryKind::Deferred
            ]
        );
    }

    #[test]
    fn codex_usage_maps_last_call_split_and_thread_totals() {
        let usage = json!({
            "last": {"inputTokens": 9000, "cachedInputTokens": 8000, "outputTokens": 300, "reasoningOutputTokens": 120, "totalTokens": 9300},
            "total": {"inputTokens": 40000, "cachedInputTokens": 30000, "outputTokens": 1200, "reasoningOutputTokens": 500, "totalTokens": 41200},
            "modelContextWindow": 272000
        });
        let breakdown = codex_breakdown(Some(&usage));
        assert_eq!(breakdown.total_tokens, Some(9300));
        assert_eq!(breakdown.window, Some(272000));
        assert_eq!(breakdown.token_split.cache_read, Some(8000));
        assert_eq!(breakdown.token_split.reasoning, Some(120));
        assert!(breakdown.categories.is_empty());
        assert_eq!(breakdown.sections.len(), 1);

        let empty = codex_breakdown(None);
        assert!(empty.token_split.is_empty());
        assert!(empty.sections.is_empty());
    }

    #[test]
    fn claude_tool_catalog_includes_deferred_tools_and_server_names() {
        let catalog = claude_tool_catalog(&json!({
            "systemTools": [{"name": "Read"}],
            "deferredBuiltinTools": [{"name": "TaskCreate", "isLoaded": false}],
            "mcpTools": [{"serverName": "docs", "name": "mcp__docs__read", "isLoaded": false}]
        }));
        let builtin = catalog.builtin.unwrap();
        assert_eq!(
            builtin
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["Read", "TaskCreate"]
        );
        assert!(builtin[1].deferred);
        let servers = catalog.mcp.unwrap();
        assert_eq!(servers[0].name, "docs");
        assert_eq!(servers[0].tools[0].name, "mcp__docs__read");
        assert!(servers[0].tools[0].deferred);
    }

    #[test]
    fn codex_tool_catalog_preserves_descriptions_empty_servers_and_failures() {
        let servers = codex_tool_servers(&json!({"data": [
            {"name": "docs", "runtimeStatus": "connected", "tools": {
                "search": {"name": "search", "description": "Find documentation", "inputSchema": {"type": "object"}},
                "read": {"name": "read", "inputSchema": {"type": "object"}}
            }, "toolsError": null},
            {"name": "unavailable", "runtimeStatus": "failed", "tools": {}, "toolsError": "Connection failed"}
        ]}));
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].tools[0].name, "read");
        assert_eq!(
            servers[0].tools[1].description.as_deref(),
            Some("Find documentation")
        );
        assert!(servers[1].tools.is_empty());
        assert_eq!(servers[1].error.as_deref(), Some("Connection failed"));
    }

    #[test]
    fn pi_stats_use_last_message_for_the_split() {
        let stats = json!({"data": {
            "userMessages": 2, "assistantMessages": 3, "toolCalls": 4, "toolResults": 4,
            "tokens": {"input": 100, "output": 50, "cacheRead": 900, "cacheWrite": 10, "total": 1060},
            "contextUsage": {"tokens": 700, "contextWindow": 200000}
        }});
        let last = json!({"usage": {"input": 20, "output": 5, "cacheRead": 600, "cacheWrite": 75}});
        let breakdown = pi_breakdown(&stats, Some(&last));
        assert_eq!(breakdown.total_tokens, Some(700));
        assert_eq!(breakdown.window, Some(200000));
        assert_eq!(breakdown.token_split.cache_read, Some(600));
        assert_eq!(breakdown.sections[0].rows.len(), 4);
        assert_eq!(breakdown.sections[0].rows[0].detail.as_deref(), Some("2"));
    }

    #[test]
    fn manifest_folds_instructions_and_drops_the_login_environment() {
        let mut command = Command::new("/usr/bin/true");
        command
            .args(["-p", "--append-system-prompt", "be helpful"])
            .env("PATH", "/bin")
            .env("RAKU_TEST_FLAG", "1");
        let manifest = launch_manifest(&command, "be helpful", vec!["subagent".into()], vec![]);
        assert_eq!(manifest.program, "/usr/bin/true");
        assert_eq!(
            manifest.args,
            [
                "-p",
                "--append-system-prompt",
                "<appended instructions, 10 chars>"
            ]
        );
        assert_eq!(
            manifest.env,
            [("RAKU_TEST_FLAG".to_owned(), "1".to_owned())]
        );
        assert_eq!(manifest.appended_instructions, "be helpful");
    }
}
