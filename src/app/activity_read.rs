//! The code a file-read activity returned, presented as the file it came from.
//!
//! Providers prefix read output with the file's own line numbers
//! (`18\tlet top = 1`). This module strips that gutter, keeps the numbers real,
//! and tokenizes the code once when the activity is expanded, so a frame only
//! positions rows — the same split [`super::activity_diff`] uses for edits.

use std::path::Path;

use gpui::SharedString;

use crate::md::highlight::{self, Token};
use crate::model::{ActivityItem, ActivityKind};

/// A read inside a transcript row is a summary, not a code browser: past this
/// many lines, the editor is where the file should be read.
const MAX_LINES: usize = 400;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ReadLine {
    pub(super) number: u32,
    /// The gutter text, formatted once so a frame only clones it.
    pub(super) label: SharedString,
    pub(super) content: SharedString,
    /// Paint-only syntax spans over `content`, tokenized once when the
    /// activity expands.
    pub(super) tokens: Vec<Token>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ReadPreview {
    /// Workspace-relative when the read happened inside it, so the card names
    /// the file the way the project does.
    pub(super) path: String,
    pub(super) lines: Vec<ReadLine>,
    /// Lines dropped at [`MAX_LINES`], so the card can say so instead of
    /// quietly presenting part of a file as the whole one.
    pub(super) hidden_lines: usize,
    /// Length of the output this was built from, so a replaced result is
    /// rebuilt rather than shown stale.
    pub(super) source_len: usize,
}

/// Build the preview for one read activity.
///
/// Runs once when the activity is expanded — never from a row builder — and the
/// caller keeps the result until the activity's output is replaced.
pub(super) fn build(activity: &ActivityItem, workspace: Option<&Path>) -> Option<ReadPreview> {
    if activity.kind != ActivityKind::FileRead || activity.failed || !activity.image_urls.is_empty()
    {
        return None;
    }
    let output = activity
        .output
        .as_deref()
        .filter(|output| !output.trim().is_empty())?;
    let raw_path = activity.display_target.as_deref().unwrap_or_default();
    let path = display_path(raw_path, workspace);
    let lang = Path::new(&path)
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(highlight::lang_for_tag);

    // Claude, Pi and Codex all number read output; a provider that hands back
    // bare content still gets the line numbers its arguments implied.
    let numbered = numbered_output(output);
    let fallback_start = activity.read_range.map_or(1, |range| range.start);
    let mut raw_lines = Vec::new();
    let mut next = fallback_start;
    for raw in output.lines() {
        let (number, content) = match (numbered, strip_line_number(raw)) {
            (true, Some((number, content))) => (number, content),
            _ => {
                let number = next;
                next = next.saturating_add(1);
                (number, raw)
            }
        };
        raw_lines.push((number, content.to_owned()));
    }
    let hidden_lines = raw_lines.len().saturating_sub(MAX_LINES);
    raw_lines.truncate(MAX_LINES);

    let code = raw_lines
        .iter()
        .map(|(_, content)| content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut token_lines = lang
        .map(|lang| highlight::tokenize(lang, &code))
        .unwrap_or_default()
        .into_iter();
    let lines = raw_lines
        .into_iter()
        .map(|(number, content)| ReadLine {
            number,
            label: number.to_string().into(),
            content: content.into(),
            tokens: token_lines.next().unwrap_or_default(),
        })
        .collect();

    Some(ReadPreview {
        path,
        lines,
        hidden_lines,
        source_len: output.len(),
    })
}

/// Whether the output carries a line-number gutter on most of its lines. A
/// plain file whose lines happen to start with digits is content, not a
/// gutter, so the majority has to agree.
fn numbered_output(output: &str) -> bool {
    let mut lines = 0usize;
    let mut numbered = 0usize;
    for line in output.lines() {
        lines += 1;
        if strip_line_number(line).is_some() {
            numbered += 1;
        }
    }
    lines > 0 && numbered * 4 >= lines * 3
}

/// `<number>\t<code>`, the shape providers emit, and the `→` variant some
/// builds use instead of the tab. Returns the number and the code after it.
fn strip_line_number(line: &str) -> Option<(u32, &str)> {
    let trimmed = line.trim_start_matches(' ');
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 7 {
        return None;
    }
    let rest = trimmed.get(digits..)?;
    let rest = rest
        .strip_prefix('\t')
        .or_else(|| rest.strip_prefix('\u{2192}'))?;
    Some((trimmed.get(..digits)?.parse().ok()?, rest))
}

pub(super) fn display_path(path: &str, workspace: Option<&Path>) -> String {
    let path = Path::new(path.trim());
    let relative = workspace.and_then(|workspace| path.strip_prefix(workspace).ok());
    relative
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(output: Option<&str>) -> ActivityItem {
        let mut item = ActivityItem::new(None, ActivityKind::FileRead, "Read", None, true);
        item.display_target = Some("/work/src/player.swift".into());
        item.output = output.map(str::to_owned);
        item
    }

    #[test]
    fn numbered_output_keeps_its_own_line_numbers() {
        let activity = read(Some("18\tlet top = 1\n19\t\n20\tlet end = 2\n"));
        let preview = build(&activity, Some(Path::new("/work"))).expect("preview");

        assert_eq!(preview.path, "src/player.swift");
        assert_eq!(
            preview
                .lines
                .iter()
                .map(|line| (line.number, line.content.as_ref()))
                .collect::<Vec<_>>(),
            vec![(18, "let top = 1"), (19, ""), (20, "let end = 2")]
        );
        assert!(!preview.lines[0].tokens.is_empty(), "Swift is highlighted");
        assert_eq!(preview.hidden_lines, 0);
    }

    #[test]
    fn unnumbered_output_starts_where_the_read_did() {
        let mut activity = read(Some("let top = 1\nlet end = 2\n"));
        activity.read_range = Some(crate::model::ActivityReadRange {
            start: 18,
            end: 117,
        });
        let preview = build(&activity, None).expect("preview");

        assert_eq!(preview.path, "/work/src/player.swift");
        assert_eq!(
            preview
                .lines
                .iter()
                .map(|line| line.number)
                .collect::<Vec<_>>(),
            vec![18, 19]
        );
    }

    #[test]
    fn content_that_merely_starts_with_digits_is_not_a_gutter() {
        let activity = read(Some("2026 was a year\n3 things happened\n"));
        let preview = build(&activity, None).expect("preview");
        assert_eq!(preview.lines[0].number, 1);
        assert_eq!(preview.lines[0].content.as_ref(), "2026 was a year");
    }

    #[test]
    fn long_reads_stop_at_the_row_budget() {
        let output = (1..=MAX_LINES + 12)
            .map(|line| format!("{line}\tline {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = build(&read(Some(&output)), None).expect("preview");
        assert_eq!(preview.lines.len(), MAX_LINES);
        assert_eq!(preview.hidden_lines, 12);
    }

    #[test]
    fn failures_and_empty_outputs_have_no_preview() {
        let mut failed = read(Some("18\tlet top = 1\n"));
        failed.failed = true;
        assert!(build(&failed, None).is_none());
        assert!(build(&read(Some("   \n")), None).is_none());
        assert!(build(&read(None), None).is_none());
    }
}
