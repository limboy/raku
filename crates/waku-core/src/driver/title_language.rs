//! Naming context for harnesses that derive their titles from user messages.

use std::sync::atomic::{AtomicBool, Ordering};
use whatlang::Lang;

use crate::model::{ExecutionPolicy, MessageRole, ProviderKind, ProviderSessionHistory};

// OMP's tiny-model input sanitizer drops paired XML blocks, including their
// contents. Bracketed metadata survives both its cleanup and conversation framing.
const CONTEXT_START: &str = "\n\n[[waku-session-title-language=";
const CONTEXT_LANGUAGE: &str = "]]\nAutomatic session titles MUST use ";
const CONTEXT_END: &str = concat!(
    ", unless the user explicitly requests another title language. ",
    "Preserve the user's original script and necessary technical terms. ",
    "Name the user's request above, not these naming instructions. ",
    "This metadata is not an additional task.\n"
);
const CONTEXT_CLOSE: &str = "[[/waku-session-title-language]]";
const LEGACY_CONTEXT_START: &str = "\n\n<waku-session-title-language language=\"";
const LEGACY_CONTEXT_LANGUAGE: &str = "\">\nAutomatic session titles MUST use ";
const LEGACY_CONTEXT_CLOSE: &str = "</waku-session-title-language>";

fn language_name(language: Lang) -> &'static str {
    match language {
        Lang::Cmn => "Chinese",
        _ => language.eng_name(),
    }
}

/// Bound the sample, omit Markdown code/quotes, and avoid letting paths and
/// identifiers decide a language. This runs once on the daemon request thread.
fn user_prose(prompt: &str) -> String {
    const SAMPLE_CHARS: usize = 2_048;
    let mut sample = String::new();
    let mut count = 0;
    let mut fence = None;
    'lines: for line in prompt.lines().take(4_096) {
        let line = line.trim_start();
        if let Some(marker) = line
            .starts_with("```")
            .then_some('`')
            .or_else(|| line.starts_with("~~~").then_some('~'))
        {
            match fence {
                Some(open) if open == marker => fence = None,
                None => fence = Some(marker),
                _ => {}
            }
            continue;
        }
        if fence.is_some() || line.starts_with('>') {
            continue;
        }
        let mut inline_code = false;
        for character in line.chars() {
            if character == '`' {
                inline_code = !inline_code;
                sample.push(' ');
            } else if !inline_code {
                sample.push(character);
                count += 1;
                if count >= SAMPLE_CHARS {
                    break 'lines;
                }
            }
        }
        sample.push(' ');
    }
    sample
        .split_whitespace()
        .filter(|word| {
            !word.is_ascii()
                || !(word.contains(['/', '\\', '_'])
                    || word.contains("::")
                    || word.chars().skip(1).any(|c| c.is_ascii_uppercase()))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn predominant_language(prompt: &str) -> Option<Lang> {
    let prose = user_prose(prompt);
    let mut han = 0;
    let mut kana = 0;
    let mut hangul = 0;
    let mut latin_words = 0;
    let mut in_latin_word = false;
    for character in prose.chars() {
        match character {
            '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{20000}'..='\u{3134f}' => {
                han += 1
            }
            '\u{3040}'..='\u{30ff}' | '\u{31f0}'..='\u{31ff}' => kana += 1,
            '\u{1100}'..='\u{11ff}' | '\u{ac00}'..='\u{d7af}' => hangul += 1,
            _ => {}
        }
        let latin = character.is_ascii_alphabetic();
        latin_words += usize::from(latin && !in_latin_word);
        in_latin_word = latin;
    }
    // CJK characters are roughly word-sized. Counting Latin letters instead
    // lets a handful of long English technical terms outweigh Chinese prose.
    if kana > 0 && han + kana >= latin_words {
        return Some(Lang::Jpn);
    }
    if hangul > 0 && hangul >= latin_words {
        return Some(Lang::Kor);
    }
    if han > 0 && han >= latin_words {
        return Some(Lang::Cmn);
    }
    whatlang::detect(&prose)
        // A small foreign-language quote lowers statistical confidence even
        // when a full sentence clearly uses the surrounding prose's language.
        .filter(|info| info.is_reliable() || (latin_words >= 8 && info.confidence() >= 0.5))
        .map(|info| info.lang())
}

fn strip_context(text: &str) -> Option<&str> {
    for (start, separator, close) in [
        (CONTEXT_START, CONTEXT_LANGUAGE, CONTEXT_CLOSE),
        (
            LEGACY_CONTEXT_START,
            LEGACY_CONTEXT_LANGUAGE,
            LEGACY_CONTEXT_CLOSE,
        ),
    ] {
        let Some((prompt, context)) = text
            .strip_suffix(close)
            .and_then(|text| text.strip_suffix(CONTEXT_END))
            .and_then(|text| text.rsplit_once(start))
        else {
            continue;
        };
        if let Some((code, name)) = context.split_once(separator)
            && let Some(language) = Lang::from_code(code)
            && name == language_name(language)
        {
            return Some(prompt);
        }
    }
    None
}

/// Shared by cloned driver handles, so only the first ordinary prompt carries
/// the policy. Commands and steering retain their provider-native syntax.
#[derive(Default)]
pub(super) struct TitleLanguageContext {
    pending: AtomicBool,
}

impl TitleLanguageContext {
    pub(super) fn new(provider: ProviderKind, execution_policy: ExecutionPolicy) -> Self {
        Self {
            // Codex receives this policy in its isolated title turn's system
            // instructions. Review subtasks never request a session title.
            pending: AtomicBool::new(
                provider != ProviderKind::Codex
                    && execution_policy != ExecutionPolicy::ReviewReadOnly,
            ),
        }
    }

    pub(super) fn prepare(&self, mut prompt: String) -> String {
        if !self.pending.load(Ordering::Acquire) {
            return prompt;
        }
        let text = prompt.trim_start();
        let command = text
            .split_whitespace()
            .next()
            .and_then(|word| word.strip_prefix('/'))
            .is_some_and(|word| !word.contains('/'));
        if text.is_empty() || command {
            return prompt;
        }
        if let Some(original) = strip_context(&prompt) {
            // Retries may carry an already prepared payload. Rebuild it so
            // legacy XML context is upgraded instead of forwarded unchanged.
            prompt = original.to_owned();
        }
        let Some(language) = predominant_language(&prompt) else {
            return prompt;
        };
        if self.pending.swap(false, Ordering::AcqRel) {
            prompt.push_str(CONTEXT_START);
            prompt.push_str(language.code());
            prompt.push_str(CONTEXT_LANGUAGE);
            prompt.push_str(language_name(language));
            prompt.push_str(CONTEXT_END);
            prompt.push_str(CONTEXT_CLOSE);
        }
        prompt
    }
}

/// A cold import must display the request the user typed, while retaining the
/// provider's original content for native history and attachment fidelity.
pub(crate) fn hide_title_language_context(history: &mut ProviderSessionHistory) {
    for message in &mut history.messages {
        if message.role == MessageRole::User
            && let Some(text) = strip_context(message.visible_content())
        {
            message.display_content = Some(text.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Message;

    #[test]
    fn native_harnesses_receive_the_naming_context_once() {
        for provider in ProviderKind::ALL {
            let context = TitleLanguageContext::new(provider, ExecutionPolicy::Interactive);
            let prompt = "修复 sidebar 的 title 更新";
            let first = context.prepare(prompt.into());
            if provider == ProviderKind::Codex {
                assert_eq!(first, prompt);
            } else {
                assert_eq!(strip_context(&first), Some(prompt));
            }
            assert_eq!(context.prepare("继续处理".into()), "继续处理");
        }
    }

    #[test]
    fn commands_and_empty_prompts_do_not_consume_the_naming_context() {
        let context = TitleLanguageContext::new(ProviderKind::OhMyPi, ExecutionPolicy::Interactive);
        for command in ["", "   ", "/rename 用户指定标题", "  /compact"] {
            assert_eq!(context.prepare(command.into()), command);
        }
        let prompt = "/tmp/project/main.rs 修复标题语言";
        assert_eq!(strip_context(&context.prepare(prompt.into())), Some(prompt));
    }

    #[test]
    fn review_subtasks_never_receive_naming_instructions() {
        for provider in ProviderKind::ALL {
            let context = TitleLanguageContext::new(provider, ExecutionPolicy::ReviewReadOnly);
            assert_eq!(
                context.prepare("Review this change".into()),
                "Review this change"
            );
        }
    }

    #[test]
    fn imports_hide_only_our_complete_user_message_suffix() {
        let prompt = "修复标题语言\n";
        let raw = TitleLanguageContext::new(ProviderKind::Claude, ExecutionPolicy::Interactive)
            .prepare(prompt.into());
        let quoted_marker = "Explain <waku-session-title-language> in this file";
        let mut history = ProviderSessionHistory {
            messages: vec![
                Message::new(MessageRole::User, raw.clone()),
                Message::new(MessageRole::Assistant, raw.clone()),
                Message::new(MessageRole::User, quoted_marker),
                Message::new(MessageRole::User, raw.clone())
                    .with_presentation(Some("用户可见的附件说明".into()), Vec::new()),
            ],
            ..Default::default()
        };

        hide_title_language_context(&mut history);

        assert_eq!(history.messages[0].visible_content(), prompt);
        assert_eq!(history.messages[0].content, raw);
        assert_eq!(history.messages[1].visible_content(), raw);
        assert_eq!(history.messages[2].visible_content(), quoted_marker);
        assert_eq!(history.messages[3].visible_content(), "用户可见的附件说明");
    }

    #[test]
    fn language_detection_follows_prose_instead_of_english_code_and_paths() {
        for (prompt, expected) in [
            (
                "auto generate title 时，不要自动变成英文，要 follow 给定的语言，比如 prompt 是中文为主，就 generate 成 中文",
                Lang::Cmn,
            ),
            (
                "請修正設定頁的字體大小，讓側邊欄和主要內容使用一致的文字尺寸。",
                Lang::Cmn,
            ),
            (
                "Automatically generated task titles should follow the language of the user request, preserving technical terms.",
                Lang::Eng,
            ),
            (
                "Les titres des conversations doivent suivre la langue de la demande, même quand elle contient des termes techniques anglais.",
                Lang::Fra,
            ),
            (
                "会話のタイトルはユーザーの質問と同じ言語で生成してください。",
                Lang::Jpn,
            ),
            (
                "Please fix the parser so that quoted Chinese text like 中文 does not change the language of the title.",
                Lang::Eng,
            ),
            (
                "请修复这段代码中的标题问题。\n```rust\nfn EnglishIdentifier() { process_many_english_tokens(); }\n```\n`AnotherEnglishIdentifier` /long/english/path/to/title.rs",
                Lang::Cmn,
            ),
        ] {
            assert_eq!(predominant_language(prompt), Some(expected), "{prompt}");
        }
    }

    #[test]
    fn indeterminate_input_keeps_the_policy_available_for_a_real_request() {
        let context = TitleLanguageContext::new(ProviderKind::Claude, ExecutionPolicy::Interactive);
        assert_eq!(context.prepare("1234".into()), "1234");
        let prompt = "请修复中文会话标题的语言";
        assert_eq!(strip_context(&context.prepare(prompt.into())), Some(prompt));
    }

    #[test]
    fn legacy_context_is_hidden_on_import_and_upgraded_on_retry() {
        let prompt = "你觉得生活中有哪些堪称「跃迁」的事件？";
        let legacy = format!(
            "{prompt}{LEGACY_CONTEXT_START}cmn{LEGACY_CONTEXT_LANGUAGE}Chinese{CONTEXT_END}{LEGACY_CONTEXT_CLOSE}"
        );
        let mut history = ProviderSessionHistory {
            messages: vec![Message::new(MessageRole::User, legacy.clone())],
            ..Default::default()
        };
        hide_title_language_context(&mut history);
        assert_eq!(history.messages[0].visible_content(), prompt);

        let context = TitleLanguageContext::new(ProviderKind::OhMyPi, ExecutionPolicy::Interactive);
        let upgraded = context.prepare(legacy);
        assert_eq!(strip_context(&upgraded), Some(prompt));
        assert!(upgraded.ends_with(CONTEXT_CLOSE));
        let retry = TitleLanguageContext::new(ProviderKind::OhMyPi, ExecutionPolicy::Interactive);
        assert_eq!(retry.prepare(upgraded.clone()), upgraded);
    }

    #[test]
    #[ignore = "requires an npm-installed omp and Bun to run OMP's real title-input sanitizer"]
    fn omp_title_preprocessing_preserves_the_language_context() {
        use std::io::Write;
        use std::process::Stdio;

        let binary = crate::command_env::find_executable("omp")
            .expect("omp is not installed")
            .canonicalize()
            .unwrap();
        let source = binary
            .parent()
            .and_then(|parent| parent.parent())
            .unwrap()
            .join("src/tiny/message-preproc.ts");
        assert!(
            source.is_file(),
            "OMP source is unavailable: {}",
            source.display()
        );
        let bun = crate::command_env::find_executable("bun").expect("Bun is not installed");
        let run = |input: &str| {
            let mut command = crate::command_env::command(&bun);
            command
                .args([
                    "--eval",
                    "const {preprocessTinyMessage} = await import(process.env.WAKU_TEST_OMP_PREPROCESSOR); process.stdout.write(preprocessTinyMessage(await Bun.stdin.text()));",
                ])
                .env("WAKU_TEST_OMP_PREPROCESSOR", &source)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = crate::command_env::spawn(&mut command).unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        let prompt = "你觉得生活中有哪些堪称「跃迁」的事件？";
        let legacy = format!(
            "{prompt}{LEGACY_CONTEXT_START}cmn{LEGACY_CONTEXT_LANGUAGE}Chinese{CONTEXT_END}{LEGACY_CONTEXT_CLOSE}"
        );
        assert_eq!(run(&legacy), prompt, "legacy XML loses the language policy");
        let prepared =
            TitleLanguageContext::new(ProviderKind::OhMyPi, ExecutionPolicy::Interactive)
                .prepare(prompt.into());
        assert_eq!(strip_context(&run(&prepared)), Some(prompt));
    }

    #[test]
    #[ignore = "requires an authenticated native harness; set WAKU_TEST_TITLE_HARNESS (default omp)"]
    fn native_titles_follow_the_original_prompt_language() {
        use std::time::{Duration, Instant};

        use crate::driver::{DriverStartOptions, start_local, test_event_channel};
        use crate::model::{DriverEvent, RuntimeMode};

        let command = std::env::var("WAKU_TEST_TITLE_HARNESS").unwrap_or_else(|_| "omp".into());
        let provider = ProviderKind::ALL
            .into_iter()
            .find(|provider| provider.command() == command)
            .expect("unknown harness");
        let binary =
            crate::command_env::find_executable(&command).expect("harness is not installed");
        let model = std::env::var("WAKU_TEST_TITLE_MODEL")
            .ok()
            .or_else(|| (provider == ProviderKind::Claude).then(|| "haiku".into()));
        let reasoning_effort = std::env::var("WAKU_TEST_TITLE_EFFORT").ok();
        let cwd =
            std::env::temp_dir().join(format!("waku-title-language-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&cwd).unwrap();
        for (prompt, chinese) in [
            ("你觉得生活中有哪些堪称「跃迁」的事件？", true),
            (
                "auto generate title 时，不要自动变成英文，要 follow 给定的语言，比如 prompt 是中文为主，就 generate 成 中文。这里只测试命名，不要调用工具，只回复收到。",
                true,
            ),
            (
                "Automatically generated titles should follow the user's prompt language. This is only a naming test; do not use tools and reply only OK.",
                false,
            ),
        ] {
            let (events, received) = test_event_channel();
            let driver = start_local(
                provider,
                DriverStartOptions {
                    binary: binary.clone(),
                    cwd: cwd.clone(),
                    mode: RuntimeMode::FullAccess,
                    execution_policy: ExecutionPolicy::Interactive,
                    model: model.clone(),
                    reasoning_effort: reasoning_effort.clone(),
                    service_tier: None,
                    context_window: None,
                    agent_preset: None,
                    provider_cursor: None,
                    delegation_session_path: None,
                    delegation_registry_dir: None,
                },
                events,
            )
            .expect("harness should start");
            driver.prompt(prompt.into());
            let deadline = Instant::now() + Duration::from_secs(60);
            let title = loop {
                match received.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(DriverEvent::AutoTitleUpdated(Some(title))) => break title,
                    Ok(DriverEvent::Error(error)) => panic!("harness error: {error}"),
                    Ok(_) => {}
                    Err(_) => panic!("native title did not arrive"),
                }
            };
            driver.cancel();
            eprintln!("{} native title: {title}", provider.display_name());
            let mut session = crate::model::AgentSession::new(uuid::Uuid::new_v4(), provider);
            session.set_auto_title(Some(title));
            let title = session.display_title();
            assert_eq!(
                waku_protocol::title::without_leading_icons(title),
                title,
                "the displayed title must not contain leading icons"
            );
            assert_eq!(
                title.chars().any(|c| matches!(c, '\u{3400}'..='\u{9fff}')),
                chinese
            );
        }
        let _ = std::fs::remove_dir_all(cwd);
    }
}
