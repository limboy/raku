//! Block-level markdown parsing over `pulldown-cmark`.
//!
//! A full parse produces a [`BlockTree`]: top-level blocks paired with their
//! byte ranges in the source. The range start of the second-to-last
//! source-level block is a *stable boundary* — appending to the source cannot
//! change anything before it (see [`IncrementalParser::settled_prefix`] for
//! why the final block alone is not enough) — which is what
//! [`IncrementalParser`] exploits so a streamed delta costs roughly
//! O(delta + last two blocks) instead of O(document).
//!
//! Soundness guard: link reference definitions (`[label]: url`) resolve
//! non-locally, so a source containing one drops back to full reparses.

use raku_protocol::composer::{PromptToken, PromptTokenKind};
use std::ops::Range;
use std::sync::LazyLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};
use regex::Regex;

/// CommonMark only recognizes angle-bracket autolinks. Transcript content is
/// conversational, so bare web URLs should be useful without requiring the
/// author to write `<https://...>` or `[label](https://...)`.
static BARE_WEB_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\bhttps?://[^\s<>"`\\]+"#).expect("bare web URL regex should compile")
});

// ── Tree model ─────────────────────────────────────────────────────────────

/// Inline styling threaded through nested emphasis and links.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InlineStyle {
    pub prompt_token: Option<PromptTokenKind>,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    /// An indivisible LaTeX expression, kept separate from adjacent math runs.
    pub math: bool,
    pub strikethrough: bool,
    /// Destination URL when inside a link.
    pub link: Option<String>,
}

/// One run of identically styled inline text.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineRun {
    pub text: String,
    pub style: InlineStyle,
}

impl InlineRun {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: InlineStyle::default(),
        }
    }
}

/// GFM column alignment. Unspecified renders as `Left`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TableAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// A list item: its blocks plus an optional task-list checkbox state.
#[derive(Clone, Debug, PartialEq)]
pub struct ListItem {
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

/// One piece of inline content. Images interrupt a run of text rather than
/// styling it, so they cannot be an [`InlineStyle`] flag.
#[derive(Clone, Debug, PartialEq)]
enum InlinePiece {
    Run(InlineRun),
    Image { url: String, alt: String },
    DisplayMath(String),
}

/// A markdown block. Containers nest.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Paragraph {
        runs: Vec<InlineRun>,
    },
    /// A standalone image. Inline images split their paragraph so the text
    /// before and after keeps its order around them.
    Image {
        url: String,
        alt: String,
    },
    Heading {
        level: u8,
        runs: Vec<InlineRun>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
        /// A shell fence the agent marked for the user to run (`sh run`).
        runnable: bool,
    },
    DisplayMath {
        latex: String,
    },
    BlockQuote {
        children: Vec<Block>,
    },
    List {
        /// `Some(n)` for an ordered list starting at `n`.
        ordered_start: Option<u64>,
        items: Vec<ListItem>,
    },
    Table {
        header: Vec<Vec<InlineRun>>,
        rows: Vec<Vec<Vec<InlineRun>>>,
        align: Vec<TableAlign>,
    },
    Rule,
}

/// A top-level block plus its byte range in the source.
#[derive(Clone, Debug, PartialEq)]
pub struct TopBlock {
    pub range: Range<usize>,
    pub block: Block,
}

/// Top-level blocks in document order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlockTree {
    pub blocks: Vec<TopBlock>,
}

/// Fence languages whose `run` marker offers the user a Run button.
fn is_shell_language(language: &str) -> bool {
    ["sh", "bash", "zsh", "shell"]
        .iter()
        .any(|shell| language.eq_ignore_ascii_case(shell))
}

#[cfg(test)]
impl BlockTree {
    pub fn len(&self) -> usize {
        self.blocks.len()
    }
}

// ── Full parse ─────────────────────────────────────────────────────────────

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
        | Options::ENABLE_CJK_FRIENDLY_EMPHASIS
        // Typographic quotes, apostrophes, dashes, and ellipses in prose.
        // Code spans and blocks keep their literal ASCII.
        | Options::ENABLE_SMART_PUNCTUATION
}

/// Parse a whole source into a [`BlockTree`].
pub fn parse(source: &str) -> BlockTree {
    parse_with_tokens(source, &[])
}

/// Protect accepted tokens from Markdown syntax (including paired `$` math)
/// while retaining the original source and selectable labels.
pub fn parse_with_tokens(source: &str, tokens: &[PromptToken]) -> BlockTree {
    let tokens = PromptToken::normalized(tokens.to_vec(), source);
    let mut prefix = if tokens.is_empty() {
        String::new()
    } else {
        String::from("RakuPromptToken")
    };
    while !tokens.is_empty() && source.contains(&prefix) {
        prefix.push('X');
    }
    let mut protected = String::new();
    let mut replacements = Vec::new();
    let mut positions = Vec::new();
    let mut cursor = 0;
    for (index, token) in tokens.iter().enumerate() {
        protected.push_str(&source[cursor..token.start]);
        let marker = format!("{prefix}{index}End");
        let start = protected.len();
        protected.push_str(&marker);
        positions.push((start..protected.len(), token.range()));
        replacements.push((marker, source[token.range()].to_owned(), token.kind));
        cursor = token.end;
    }
    let parsed_source = if tokens.is_empty() {
        source
    } else {
        protected.push_str(&source[cursor..]);
        &protected
    };
    let events = Parser::new_ext(parsed_source, options())
        .into_offset_iter()
        .collect::<Vec<_>>();
    let mut cursor = Cursor {
        events: &events,
        index: 0,
        replacements: &replacements,
    };
    let mut blocks = Vec::new();
    while let Some((event, range)) = cursor.peek() {
        let range = range.clone();
        match event {
            Event::Rule => {
                cursor.bump();
                blocks.push(TopBlock {
                    range,
                    block: Block::Rule,
                });
            }
            Event::Start(_) => {
                for block in parse_started_block(&mut cursor) {
                    blocks.push(TopBlock {
                        range: range.clone(),
                        block,
                    });
                }
            }
            // Stray inline events at the top level should not occur; skip them
            // rather than looping forever.
            _ => cursor.bump(),
        }
    }
    // The public block ranges always refer to the original visible source.
    let original_offset = |offset: usize| {
        let mut delta = 0isize;
        for (masked, original) in &positions {
            if offset < masked.start {
                break;
            }
            if offset < masked.end {
                return original.start;
            }
            delta += original.len() as isize - masked.len() as isize;
        }
        offset.checked_add_signed(delta).unwrap_or(0)
    };
    for top in &mut blocks {
        top.range = original_offset(top.range.start)..original_offset(top.range.end);
    }
    BlockTree { blocks }
}

struct Cursor<'a, 'e> {
    events: &'a [(Event<'e>, Range<usize>)],
    index: usize,
    replacements: &'a [(String, String, PromptTokenKind)],
}

impl<'e> Cursor<'_, 'e> {
    fn restore(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for (marker, label, _) in self.replacements {
            if text.contains(marker) {
                text = text.replace(marker, label);
            }
        }
        text
    }

    fn text_runs(&self, text: &str, style: &InlineStyle) -> Vec<InlineRun> {
        if self.replacements.is_empty() {
            return vec![InlineRun {
                text: text.to_owned(),
                style: style.clone(),
            }];
        }
        let mut runs = Vec::new();
        let mut remaining = text;
        while !remaining.is_empty() {
            let next = self
                .replacements
                .iter()
                .filter_map(|(marker, label, kind)| {
                    remaining
                        .find(marker)
                        .map(|start| (start, marker, label, kind))
                })
                .min_by_key(|(start, ..)| *start);
            let Some((start, marker, label, kind)) = next else {
                runs.push(InlineRun {
                    text: remaining.to_owned(),
                    style: style.clone(),
                });
                break;
            };
            if start > 0 {
                runs.push(InlineRun {
                    text: remaining[..start].to_owned(),
                    style: style.clone(),
                });
            }
            runs.push(InlineRun {
                text: label.clone(),
                style: InlineStyle {
                    prompt_token: Some(*kind),
                    ..Default::default()
                },
            });
            remaining = &remaining[start + marker.len()..];
        }
        runs
    }

    fn peek(&self) -> Option<&(Event<'e>, Range<usize>)> {
        self.events.get(self.index)
    }

    fn peek_event(&self) -> Option<&Event<'e>> {
        self.peek().map(|(event, _)| event)
    }

    fn bump(&mut self) {
        self.index += 1;
    }

    fn next_event(&mut self) -> Option<Event<'e>> {
        let event = self.events.get(self.index).map(|(event, _)| event.clone());
        if event.is_some() {
            self.index += 1;
        }
        event
    }
}

fn is_block_tag(tag: &Tag) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::CodeBlock(_)
            | Tag::BlockQuote(_)
            | Tag::List(_)
            | Tag::Item
            | Tag::Table(_)
            | Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
    )
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Consume a `Start(tag)` and everything through its matching `End`. Unknown
/// containers are transparent: their children splice into the parent.
fn parse_started_block(cursor: &mut Cursor) -> Vec<Block> {
    let Some(Event::Start(tag)) = cursor.next_event() else {
        return Vec::new();
    };
    match tag {
        Tag::Paragraph => pieces_into_blocks(parse_inline_container(cursor)),
        Tag::Heading { level, .. } => vec![Block::Heading {
            level: heading_level(level),
            runs: pieces_into_runs(parse_inline_container(cursor)),
        }],
        Tag::CodeBlock(kind) => {
            let (language, runnable) = match kind {
                CodeBlockKind::Fenced(info) => {
                    let mut words = info.split_whitespace();
                    let language = words
                        .next()
                        .filter(|language| !language.is_empty())
                        .map(str::to_owned);
                    let runnable = language.as_deref().is_some_and(is_shell_language)
                        && words.any(|word| word.eq_ignore_ascii_case("run"));
                    (language, runnable)
                }
                CodeBlockKind::Indented => (None, false),
            };
            let mut code = String::new();
            loop {
                match cursor.next_event() {
                    Some(Event::Text(text)) => {
                        if cursor.replacements.is_empty() {
                            code.push_str(&text);
                        } else {
                            code.push_str(&cursor.restore(&text));
                        }
                    }
                    Some(Event::End(_)) | None => break,
                    Some(_) => {}
                }
            }
            // Fenced blocks carry a trailing newline; rendering is per line so
            // the empty final line would otherwise add a phantom row.
            if code.ends_with('\n') {
                code.pop();
            }
            vec![Block::CodeBlock {
                language,
                code,
                runnable,
            }]
        }
        Tag::BlockQuote(_) => vec![Block::BlockQuote {
            children: parse_block_sequence(cursor),
        }],
        Tag::List(ordered_start) => {
            let mut items = Vec::new();
            loop {
                match cursor.peek_event() {
                    Some(Event::Start(Tag::Item)) => {
                        cursor.bump();
                        items.push(parse_list_item(cursor));
                    }
                    Some(Event::End(_)) | None => {
                        cursor.bump();
                        break;
                    }
                    Some(_) => cursor.bump(),
                }
            }
            vec![Block::List {
                ordered_start,
                items,
            }]
        }
        Tag::Table(alignments) => {
            let align = alignments
                .iter()
                .map(|alignment| match alignment {
                    Alignment::Center => TableAlign::Center,
                    Alignment::Right => TableAlign::Right,
                    Alignment::None | Alignment::Left => TableAlign::Left,
                })
                .collect();
            vec![parse_table(cursor, align)]
        }
        Tag::HtmlBlock => {
            // Raw HTML renders literally: an agent transcript is far more
            // likely to be *discussing* markup than asking us to apply it.
            let mut text = String::new();
            loop {
                match cursor.next_event() {
                    Some(Event::Html(chunk) | Event::Text(chunk)) => text.push_str(&chunk),
                    Some(Event::End(_)) | None => break,
                    Some(_) => {}
                }
            }
            let text = text.trim_end_matches('\n').to_owned();
            if text.is_empty() {
                Vec::new()
            } else {
                vec![Block::Paragraph {
                    runs: cursor.text_runs(&text, &InlineStyle::default()),
                }]
            }
        }
        // Transparent containers (footnote definitions, and anything a future
        // pulldown-cmark adds).
        _ => parse_block_sequence(cursor),
    }
}

/// Parse a list item, lifting a leading task-list marker out of its content.
fn parse_list_item(cursor: &mut Cursor) -> ListItem {
    let task = match cursor.peek_event() {
        Some(Event::TaskListMarker(checked)) => {
            let checked = *checked;
            cursor.bump();
            Some(checked)
        }
        _ => None,
    };
    ListItem {
        task,
        blocks: parse_block_sequence(cursor),
    }
}

/// Parse blocks until the container's `End` (which is consumed). Bare inline
/// events — tight list items — accumulate into an implicit paragraph.
fn parse_block_sequence(cursor: &mut Cursor) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut inline: Vec<InlinePiece> = Vec::new();
    while let Some(event) = cursor.peek_event() {
        match event {
            Event::End(_) => {
                cursor.bump();
                break;
            }
            Event::Start(tag) if is_block_tag(tag) => {
                flush_paragraph(&mut blocks, &mut inline);
                blocks.extend(parse_started_block(cursor));
            }
            Event::Rule => {
                flush_paragraph(&mut blocks, &mut inline);
                cursor.bump();
                blocks.push(Block::Rule);
            }
            _ => parse_inline_event(cursor, &mut inline, &InlineStyle::default()),
        }
    }
    flush_paragraph(&mut blocks, &mut inline);
    blocks
}

fn flush_paragraph(blocks: &mut Vec<Block>, inline: &mut Vec<InlinePiece>) {
    if !inline.is_empty() {
        blocks.extend(pieces_into_blocks(merge_pieces(std::mem::take(inline))));
    }
}

fn parse_table(cursor: &mut Cursor, align: Vec<TableAlign>) -> Block {
    let mut header = Vec::new();
    let mut rows = Vec::new();
    loop {
        match cursor.peek_event() {
            Some(Event::Start(Tag::TableHead)) => {
                cursor.bump();
                header = parse_table_row(cursor);
            }
            Some(Event::Start(Tag::TableRow)) => {
                cursor.bump();
                rows.push(parse_table_row(cursor));
            }
            Some(Event::End(_)) | None => {
                cursor.bump();
                break;
            }
            Some(_) => cursor.bump(),
        }
    }
    Block::Table {
        header,
        rows,
        align,
    }
}

fn parse_table_row(cursor: &mut Cursor) -> Vec<Vec<InlineRun>> {
    let mut cells = Vec::new();
    loop {
        match cursor.peek_event() {
            Some(Event::Start(Tag::TableCell)) => {
                cursor.bump();
                // A table cell holds only inline text; an image there degrades to
                // its alt rather than breaking the row's geometry.
                cells.push(pieces_into_runs(parse_inline_container(cursor)));
            }
            Some(Event::End(_)) | None => {
                cursor.bump();
                break;
            }
            Some(_) => cursor.bump(),
        }
    }
    cells
}

/// Collect inline pieces until the container's `End` (which is consumed).
fn parse_inline_container(cursor: &mut Cursor) -> Vec<InlinePiece> {
    let mut pieces = Vec::new();
    while let Some(event) = cursor.peek_event() {
        if matches!(event, Event::End(_)) {
            cursor.bump();
            break;
        }
        parse_inline_event(cursor, &mut pieces, &InlineStyle::default());
    }
    merge_pieces(pieces)
}

/// Split inline pieces into blocks, so images become their own block and the
/// text around them keeps its order.
fn pieces_into_blocks(pieces: Vec<InlinePiece>) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut runs: Vec<InlineRun> = Vec::new();
    for piece in pieces {
        match piece {
            InlinePiece::Run(run) => runs.push(run),
            InlinePiece::Image { url, alt } => {
                if !runs.is_empty() {
                    blocks.push(Block::Paragraph {
                        runs: std::mem::take(&mut runs),
                    });
                }
                blocks.push(Block::Image { url, alt });
            }
            InlinePiece::DisplayMath(latex) => {
                if !runs.is_empty() {
                    blocks.push(Block::Paragraph {
                        runs: std::mem::take(&mut runs),
                    });
                }
                blocks.push(Block::DisplayMath { latex });
            }
        }
    }
    if !runs.is_empty() {
        blocks.push(Block::Paragraph { runs });
    }
    blocks
}

/// Flatten pieces to runs for contexts that cannot host a block-level image.
fn pieces_into_runs(pieces: Vec<InlinePiece>) -> Vec<InlineRun> {
    merge_runs(
        pieces
            .into_iter()
            .map(|piece| match piece {
                InlinePiece::Run(run) => run,
                InlinePiece::Image { alt, .. } => InlineRun::plain(alt),
                InlinePiece::DisplayMath(text) => InlineRun {
                    text,
                    style: InlineStyle {
                        math: true,
                        ..Default::default()
                    },
                },
            })
            .collect(),
    )
}

/// Consume one inline event, appending its runs with `style` applied. Nested
/// emphasis and links recurse with an extended style.
fn parse_inline_event(cursor: &mut Cursor, pieces: &mut Vec<InlinePiece>, style: &InlineStyle) {
    let Some(event) = cursor.next_event() else {
        return;
    };
    let mut push_run = |run: InlineRun| pieces.push(InlinePiece::Run(run));
    match event {
        Event::Text(text) => {
            if cursor.replacements.is_empty() {
                push_run(InlineRun {
                    text: text.to_string(),
                    style: style.clone(),
                });
            } else {
                for run in cursor.text_runs(&text, style) {
                    push_run(run);
                }
            }
        }
        Event::Code(text) => {
            let mut style = style.clone();
            style.code = true;
            push_run(InlineRun {
                text: cursor.restore(&text),
                style,
            });
        }
        Event::InlineMath(text) => {
            let mut style = style.clone();
            style.math = true;
            push_run(InlineRun {
                text: cursor.restore(&text),
                style,
            });
        }
        Event::DisplayMath(text) => pieces.push(InlinePiece::DisplayMath(cursor.restore(&text))),
        // A hard or soft break inside a paragraph is a line break in the
        // rendered run: shaped text splits on '\n' on its own.
        Event::SoftBreak | Event::HardBreak => push_run(InlineRun {
            text: "\n".to_owned(),
            style: style.clone(),
        }),
        Event::Start(Tag::Image {
            dest_url, title, ..
        }) => {
            // The image's children are its alt text.
            let mut alt_pieces = Vec::new();
            while let Some(event) = cursor.peek_event() {
                if matches!(event, Event::End(_)) {
                    cursor.bump();
                    break;
                }
                parse_inline_event(cursor, &mut alt_pieces, &InlineStyle::default());
            }
            let alt = pieces_into_runs(alt_pieces)
                .into_iter()
                .map(|run| run.text)
                .collect::<String>();
            let alt = if alt.trim().is_empty() {
                cursor.restore(&title)
            } else {
                alt
            };
            pieces.push(InlinePiece::Image {
                url: cursor.restore(&dest_url),
                alt,
            });
        }
        Event::Start(tag) => {
            let mut nested = style.clone();
            match &tag {
                Tag::Emphasis => nested.italic = true,
                Tag::Strong => nested.bold = true,
                Tag::Strikethrough => nested.strikethrough = true,
                Tag::Link { dest_url, .. } => nested.link = Some(cursor.restore(dest_url)),
                _ => {}
            }
            while let Some(event) = cursor.peek_event() {
                if matches!(event, Event::End(_)) {
                    cursor.bump();
                    break;
                }
                parse_inline_event(cursor, pieces, &nested);
            }
        }
        // `<br>` is the only way to break a line inside a GFM table cell, so it
        // becomes a line break like a hard break does. Other inline HTML
        // renders literally, matching the block-level choice.
        Event::Html(text) | Event::InlineHtml(text) => push_run(InlineRun {
            text: if is_br_tag(&text) {
                "\n".to_owned()
            } else {
                cursor.restore(&text)
            },
            style: style.clone(),
        }),
        Event::FootnoteReference(label) => push_run(InlineRun {
            text: format!("[{label}]"),
            style: style.clone(),
        }),
        Event::TaskListMarker(checked) => push_run(InlineRun {
            text: if checked { "[x] " } else { "[ ] " }.to_owned(),
            style: style.clone(),
        }),
        Event::End(_) | Event::Rule => {}
    }
}

/// Whether an inline HTML chunk is a line-break tag: `<br>`, `<br/>`, or
/// `<br />`, in any case.
fn is_br_tag(html: &str) -> bool {
    let Some(inner) = html
        .trim()
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
    else {
        return false;
    };
    let inner = inner.trim_end().trim_end_matches('/').trim_end();
    inner.eq_ignore_ascii_case("br")
}

/// Sentence punctuation and unmatched closing delimiters are prose around a
/// URL, not part of it. Balanced delimiters remain valid URL characters, as in
/// Wikipedia paths ending in `(disambiguation)`.
fn trimmed_bare_url_end(text: &str, start: usize, candidate_end: usize) -> usize {
    let mut end = candidate_end;
    let mut parens = delimiter_balance(&text[start..end], '(', ')');
    let mut brackets = delimiter_balance(&text[start..end], '[', ']');
    let mut braces = delimiter_balance(&text[start..end], '{', '}');
    loop {
        let Some((offset, last)) = text[start..end].char_indices().next_back() else {
            return start;
        };
        let last_index = start + offset;
        let should_trim = match last {
            '.' | ',' | ':' | ';' | '?' | '!' | '\'' => true,
            ')' if parens < 0 => {
                parens += 1;
                true
            }
            ']' if brackets < 0 => {
                brackets += 1;
                true
            }
            '}' if braces < 0 => {
                braces += 1;
                true
            }
            _ => false,
        };
        if !should_trim {
            return end;
        }
        end = last_index;
    }
}

fn delimiter_balance(text: &str, open: char, close: char) -> i32 {
    text.chars().fold(0, |balance, character| {
        balance + i32::from(character == open) - i32::from(character == close)
    })
}

/// Coalesce neighbouring runs that share a style, leaving images in place.
fn merge_pieces(pieces: Vec<InlinePiece>) -> Vec<InlinePiece> {
    let mut merged: Vec<InlinePiece> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        match piece {
            InlinePiece::Run(run) if run.text.is_empty() => {}
            InlinePiece::Run(run) => match merged.last_mut() {
                Some(InlinePiece::Run(last))
                    if !run.style.math
                        && run.style.prompt_token.is_none()
                        && last.style == run.style =>
                {
                    last.text.push_str(&run.text)
                }
                _ => merged.push(InlinePiece::Run(run)),
            },
            image => merged.push(image),
        }
    }
    linkify_bare_urls(merged)
}

/// Linkify after Markdown has produced and merged its inline runs. Pulldown
/// can split ordinary text at potential emphasis punctuation inside a URL;
/// merging first lets the detector recover the whole displayed target.
fn linkify_bare_urls(pieces: Vec<InlinePiece>) -> Vec<InlinePiece> {
    let mut linked = Vec::with_capacity(pieces.len());
    for piece in pieces {
        match piece {
            InlinePiece::Run(run)
                if !run.style.code
                    && !run.style.math
                    && run.style.prompt_token.is_none()
                    && run.style.link.is_none() =>
            {
                push_linkified_run(run, &mut linked);
            }
            piece => linked.push(piece),
        }
    }
    linked
}

fn push_linkified_run(run: InlineRun, pieces: &mut Vec<InlinePiece>) {
    let mut cursor = 0;
    for candidate in BARE_WEB_URL.find_iter(&run.text) {
        let end = trimmed_bare_url_end(&run.text, candidate.start(), candidate.end());
        if end <= candidate.start() {
            continue;
        }
        if cursor < candidate.start() {
            pieces.push(InlinePiece::Run(InlineRun {
                text: run.text[cursor..candidate.start()].to_owned(),
                style: run.style.clone(),
            }));
        }

        let url = &run.text[candidate.start()..end];
        let mut link_style = run.style.clone();
        link_style.link = Some(url.to_owned());
        pieces.push(InlinePiece::Run(InlineRun {
            text: url.to_owned(),
            style: link_style,
        }));
        cursor = end;
    }

    if cursor < run.text.len() {
        pieces.push(InlinePiece::Run(InlineRun {
            text: run.text[cursor..].to_owned(),
            style: run.style,
        }));
    }
}

/// Coalesce neighbouring runs that share a style, so shaping sees the fewest
/// possible `TextRun`s.
fn merge_runs(runs: Vec<InlineRun>) -> Vec<InlineRun> {
    let mut merged: Vec<InlineRun> = Vec::with_capacity(runs.len());
    for run in runs {
        if run.text.is_empty() {
            continue;
        }
        match merged.last_mut() {
            Some(last)
                if !run.style.math
                    && run.style.prompt_token.is_none()
                    && last.style == run.style =>
            {
                last.text.push_str(&run.text)
            }
            _ => merged.push(run),
        }
    }
    merged
}

// ── Incremental parse ──────────────────────────────────────────────────────

/// Streaming parser over an append-only source.
///
/// Keeps the settled prefix of top-level blocks and reparses only the tail from
/// the last stable boundary. `display_tree` additionally mends hanging inline
/// markers (see [`super::mend`]) so a closing `**` never reflows text that is
/// already on screen.
pub struct IncrementalParser {
    text: String,
    tree: BlockTree,
    /// Blocks before this index are settled: no append can change them.
    stable_prefix: usize,
    /// A link reference definition anywhere forces full reparses.
    full_reparse_only: bool,
}

impl Default for IncrementalParser {
    fn default() -> Self {
        Self::new()
    }
}

impl IncrementalParser {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            tree: BlockTree::default(),
            stable_prefix: 0,
            full_reparse_only: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn tree(&self) -> &BlockTree {
        &self.tree
    }

    /// Point the parser at `text`. Appends reparse incrementally; any other
    /// change falls back to a full reparse.
    pub fn set_text(&mut self, text: &str) {
        if text == self.text {
            return;
        }
        match text.strip_prefix(self.text.as_str()) {
            Some(delta) if !self.text.is_empty() && !self.full_reparse_only => {
                let delta = delta.to_owned();
                self.append(&delta);
            }
            _ => self.reset(text),
        }
    }

    /// Discard all state and parse `text` from scratch.
    pub fn reset(&mut self, text: &str) {
        self.text = text.to_owned();
        self.tree = parse(&self.text);
        self.full_reparse_only = has_link_definition(&self.text);
        self.stable_prefix = self.settled_prefix();
    }

    /// A settled user prompt; token protection never participates in streaming.
    pub fn reset_prompt(&mut self, text: &str, tokens: &[PromptToken]) {
        self.text = text.to_owned();
        self.tree = parse_with_tokens(text, tokens);
        self.stable_prefix = 0;
        self.full_reparse_only = true;
    }

    /// Append `delta`, reparsing only from the last stable block boundary.
    pub fn append(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if self.full_reparse_only {
            let mut text = std::mem::take(&mut self.text);
            text.push_str(delta);
            self.reset(&text);
            return;
        }

        let boundary = self
            .tree
            .blocks
            .get(self.stable_prefix)
            .map_or(self.text.len(), |block| block.range.start);
        self.text.push_str(delta);
        if has_link_definition(delta) {
            let text = std::mem::take(&mut self.text);
            self.reset(&text);
            return;
        }

        let tail = parse(&self.text[boundary..]);
        self.tree.blocks.truncate(self.stable_prefix);
        self.tree
            .blocks
            .extend(tail.blocks.into_iter().map(|mut block| {
                block.range.start += boundary;
                block.range.end += boundary;
                block
            }));
        self.stable_prefix = self.settled_prefix();
    }

    /// Replacement blocks for the final block while streaming, with its hanging
    /// inline markers closed so styling does not flip as the closer arrives.
    /// `None` means the canonical tree already renders correctly.
    ///
    /// This returns only the tail — never the whole tree — so a streaming
    /// response re-derives O(last block) per delta rather than cloning every
    /// settled block on every frame.
    pub fn display_tail(&self) -> Option<Vec<TopBlock>> {
        let last = self.tree.blocks.last()?;
        // A code block's content is literal: mending would corrupt it, and a
        // half-typed fence must not be reinterpreted.
        if matches!(
            last.block,
            Block::CodeBlock { .. } | Block::DisplayMath { .. }
        ) {
            return None;
        }
        let mended = super::mend::close_hanging(&self.text[last.range.start..])?;
        let offset = last.range.start;
        Some(
            parse(&mended)
                .blocks
                .into_iter()
                .map(|mut block| {
                    block.range.start += offset;
                    block.range.end = (block.range.end + offset).min(self.text.len());
                    block
                })
                .collect(),
        )
    }

    /// The whole tree as it should be displayed. The renderer works from
    /// [`Self::display_tail`] to avoid this clone; this exists so the mending
    /// contract can be asserted directly.
    #[cfg(test)]
    pub fn display_tree(&self) -> BlockTree {
        let Some(tail) = self.display_tail() else {
            return self.tree.clone();
        };
        let mut blocks = self.tree.blocks[..self.display_tail_start()].to_vec();
        blocks.extend(tail);
        BlockTree { blocks }
    }

    /// Images and display math can split one source paragraph into several
    /// blocks. Mending replaces that whole source group, not just its last
    /// rendered piece.
    pub fn display_tail_start(&self) -> usize {
        let Some(last) = self.tree.blocks.last() else {
            return 0;
        };
        self.tree
            .blocks
            .partition_point(|block| block.range.start < last.range.start)
    }

    /// Index of the first block an append could still change.
    ///
    /// Appending mostly only extends the final block, but two cases reach
    /// further back, so the last *two* source-level groups stay unsettled:
    ///
    /// - A GFM table absorbs the line after it once that line becomes a valid
    ///   row, yet a partial row of just `|` transiently parses as its own
    ///   paragraph. Settling the table then would strand every later row in
    ///   that trailing paragraph.
    /// - A paragraph split around an inline image yields several blocks that
    ///   share one source range; they must settle and reparse as a unit or the
    ///   pieces before the image get re-emitted on the next append.
    fn settled_prefix(&self) -> usize {
        let blocks = &self.tree.blocks;
        let mut index = blocks.len();
        for _ in 0..2 {
            let Some(group_start) = index.checked_sub(1).map(|last| blocks[last].range.start)
            else {
                break;
            };
            while index > 0 && blocks[index - 1].range.start == group_start {
                index -= 1;
            }
        }
        index
    }
}

/// Cheap scan for a link reference definition (`[label]: destination`), which
/// resolves references anywhere in the document and so breaks locality.
fn has_link_definition(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix('[') else {
            return false;
        };
        rest.find("]:").is_some_and(|end| end > 0)
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn prompt_tokens_preserve_markdown_without_becoming_math_or_markup() {
        use raku_protocol::composer::{PromptToken, PromptTokenKind};
        let source = "**Ship** with $deploy and $docs then @src/a_b[1].rs\n\nMore";
        let tokens = [
            ("$deploy", PromptTokenKind::Skill),
            ("$docs", PromptTokenKind::Skill),
            ("@src/a_b[1].rs", PromptTokenKind::File),
        ]
        .into_iter()
        .map(|(label, kind)| {
            let start = source.find(label).unwrap();
            PromptToken {
                start,
                end: start + label.len(),
                kind,
                name: label[1..].into(),
            }
        })
        .collect::<Vec<_>>();
        let parsed = super::parse_with_tokens(source, &tokens);
        let super::Block::Paragraph { runs } = &parsed.blocks[0].block else {
            panic!("paragraph");
        };
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<String>(),
            "Ship with $deploy and $docs then @src/a_b[1].rs"
        );
        assert!(runs[0].style.bold);
        assert_eq!(
            runs.iter()
                .filter(|run| run.style.prompt_token.is_some())
                .count(),
            3
        );
        assert!(runs.iter().all(|run| !run.style.math));
        assert_eq!(parsed.blocks[1].range.start, source.find("More").unwrap());
    }

    #[test]
    fn protected_labels_never_escape_into_code_or_literal_html() {
        use raku_protocol::composer::{PromptToken, PromptTokenKind};
        for source in [
            "```sh\n@deploy\n```",
            "<span>@deploy</span>",
            "<div>\n@deploy\n</div>",
            "`@deploy`",
            "RakuPromptToken and @deploy",
        ] {
            let start = source.find("@deploy").unwrap();
            let tokens = [PromptToken {
                start,
                end: start + 7,
                kind: PromptTokenKind::Skill,
                name: "deploy".into(),
            }];
            let parsed = super::parse_with_tokens(source, &tokens);
            let printed = format!("{parsed:?}");
            assert!(!printed.contains("0End"), "{printed}");
            assert!(printed.contains("@deploy"), "{printed}");
        }
    }

    use super::*;

    #[test]
    fn cjk_emphasis_handles_punctuation_without_extra_spaces() {
        for (source, bold) in [
            ("**这些星号。**接着", "这些星号。"),
            ("**日本語。**続き", "日本語。"),
            ("**한국어(parenthesis)**가", "한국어(parenthesis)"),
            ("前**「重要」**後", "「重要」"),
            ("前**(重要)**後", "(重要)"),
            ("前**`code`**後", "code"),
            ("前**字\u{e0100}。**後", "字\u{e0100}。"),
        ] {
            let tree = parse(source);
            let Block::Paragraph { runs } = &tree.blocks[0].block else {
                panic!("{tree:?}")
            };
            assert_eq!(
                runs.iter()
                    .filter(|run| run.style.bold)
                    .map(|run| run.text.as_str())
                    .collect::<String>(),
                bold,
                "{source}"
            );
            assert!(runs.iter().all(|run| !run.text.contains("**")), "{source}");
        }

        let tree = parse("前*「斜体」*後、前~~「削除」~~後");
        let Block::Paragraph { runs } = &tree.blocks[0].block else {
            panic!("{tree:?}")
        };
        assert!(
            runs.iter()
                .any(|run| run.style.italic && run.text == "「斜体」")
        );
        assert!(
            runs.iter()
                .any(|run| run.style.strikethrough && run.text == "「削除」")
        );
    }

    #[test]
    fn cjk_emphasis_preserves_literal_code_and_escaped_markers() {
        let tree = parse(r"`前**「代码」**後` 和 \*\*「文字」\*\*後");
        let Block::Paragraph { runs } = &tree.blocks[0].block else {
            panic!("{tree:?}")
        };
        assert!(runs.iter().all(|run| !run.style.bold));
        assert_eq!(
            paragraph_text(&tree.blocks[0].block),
            "前**「代码」**後 和 **「文字」**後"
        );

        let tree = parse("```md\n前**「代码」**後\n```");
        assert!(
            matches!(&tree.blocks[0].block, Block::CodeBlock { code, .. } if code == "前**「代码」**後")
        );
    }

    #[test]
    fn streamed_cjk_emphasis_matches_full_parses_and_keeps_source_ranges() {
        let source = "# 前**「标题」**後\n\n**日本語。**続き\n\n- **한국어(parenthesis)**가\n\n| 内容 |\n| --- |\n| 前~~「删除」~~後 |\n\n前*「斜体」*後";
        let mut parser = IncrementalParser::new();
        for ch in source.chars() {
            parser.append(ch.encode_utf8(&mut [0; 4]));
            assert_eq!(parser.tree(), &parse(parser.text()), "{}", parser.text());
        }

        parser.set_text("前**「重要");
        let display = parser.display_tree();
        let Block::Paragraph { runs } = &display.blocks[0].block else {
            panic!("{display:?}")
        };
        assert!(
            runs.iter()
                .any(|run| run.style.bold && run.text == "「重要")
        );
        parser.append("」**後");
        assert_eq!(parser.display_tree(), parse(parser.text()));
    }

    #[test]
    fn math_keeps_inline_order_styles_and_display_blocks() {
        let tree = parse(r"Before **$a_1$**$b^2$ after $$\frac{1}{2}$$ done");
        let Block::Paragraph { runs } = &tree.blocks[0].block else {
            panic!("{tree:?}")
        };
        let math = runs.iter().filter(|run| run.style.math).collect::<Vec<_>>();
        assert_eq!(math.len(), 2);
        assert_eq!(math[0].text, "a_1");
        assert!(math[0].style.bold);
        assert_eq!(math[1].text, "b^2");
        assert!(
            matches!(&tree.blocks[1].block, Block::DisplayMath { latex } if latex == r"\frac{1}{2}")
        );
        assert_eq!(paragraph_text(&tree.blocks[2].block), " done");
    }

    #[test]
    fn math_is_literal_in_code_escapes_and_unclosed_delimiters() {
        for source in [r"`$a_1$`", r"\$a_1\$", r"unfinished $\frac{a}{b}"] {
            let tree = parse(source);
            let Block::Paragraph { runs } = &tree.blocks[0].block else {
                panic!("{tree:?}")
            };
            assert!(runs.iter().all(|run| !run.style.math), "{source}");
        }
        let tree = parse("```latex\n$$x^2$$\n```");
        assert!(
            matches!(&tree.blocks[0].block, Block::CodeBlock { code, .. } if code == "$$x^2$$")
        );
    }

    #[test]
    fn math_works_in_lists_quotes_headings_and_tables_without_linkifying_tex() {
        let tree = parse("# $x$\n\n> $y$\n\n- $z$\n\n| Value |\n| --- |\n| $x^2$ |\n");
        assert!(matches!(&tree.blocks[0].block, Block::Heading { runs, .. } if runs[0].style.math));
        let Block::Table { rows, .. } = &tree.blocks[3].block else {
            panic!("{tree:?}")
        };
        assert!(rows[0][0][0].style.math);
        let tree = parse(r"$\text{https://example.com}$");
        let Block::Paragraph { runs } = &tree.blocks[0].block else {
            panic!()
        };
        assert_eq!(runs.len(), 1);
        assert!(runs[0].style.math && runs[0].style.link.is_none());
    }

    #[test]
    fn streamed_math_matches_full_parse_at_every_character_boundary() {
        for source in [
            "Intro\n\nBefore $x_1$ and $y$ after\n\n$$\n\\frac{a}{b}\n$$\n\nDone",
            "- Before $$x$$ after\n- Next $y$\n",
            "before $$x$$ after **bold**",
        ] {
            let mut parser = IncrementalParser::new();
            for ch in source.chars() {
                parser.append(&ch.to_string());
                assert_eq!(parser.tree(), &parse(parser.text()), "{}", parser.text());
            }
        }
    }

    #[test]
    fn mended_math_does_not_duplicate_a_split_paragraph_or_edit_tex() {
        let mut parser = IncrementalParser::new();
        parser.set_text("before $$x$$ after **bold");
        assert_eq!(parser.display_tree().blocks.len(), 3);
        for source in [
            r"$[x]_1$",
            r"$$\left[x\right]_1$$",
            r"$a*b$",
            r"$\frac{a_1}{b}",
        ] {
            parser.set_text(source);
            assert_eq!(parser.display_tree(), *parser.tree(), "{source}");
        }
    }

    fn paragraph_text(block: &Block) -> String {
        match block {
            Block::Paragraph { runs } | Block::Heading { runs, .. } => {
                runs.iter().map(|run| run.text.as_str()).collect()
            }
            _ => panic!("expected a text block, got {block:?}"),
        }
    }

    #[test]
    fn parses_top_level_blocks_with_source_ranges() {
        let source = "# Title\n\nBody text.\n\n```rust\nfn main() {}\n```\n";
        let tree = parse(source);

        assert_eq!(tree.len(), 3);
        assert!(matches!(
            tree.blocks[0].block,
            Block::Heading { level: 1, .. }
        ));
        assert_eq!(paragraph_text(&tree.blocks[0].block), "Title");
        assert_eq!(paragraph_text(&tree.blocks[1].block), "Body text.");
        assert_eq!(
            tree.blocks[2].block,
            Block::CodeBlock {
                language: Some("rust".into()),
                code: "fn main() {}".into(),
                runnable: false,
            }
        );
        // Ranges point back into the source and are ascending.
        assert!(tree.blocks[0].range.start < tree.blocks[1].range.start);
        assert_eq!(&source[tree.blocks[1].range.clone()].trim(), &"Body text.");
    }

    #[test]
    fn inline_styles_nest_and_merge() {
        let tree = parse("plain **bold `code`** and *italic* [link](https://example.com)");
        let Block::Paragraph { runs } = &tree.blocks[0].block else {
            panic!("expected a paragraph");
        };

        let bold_code = runs
            .iter()
            .find(|run| run.style.bold && run.style.code)
            .expect("bold inline code should keep both styles");
        assert_eq!(bold_code.text, "code");

        let italic = runs
            .iter()
            .find(|run| run.style.italic)
            .expect("italic run");
        assert_eq!(italic.text, "italic");

        let link = runs
            .iter()
            .find(|run| run.style.link.is_some())
            .expect("link run");
        assert_eq!(link.text, "link");
        assert_eq!(link.style.link.as_deref(), Some("https://example.com"));

        // Adjacent identically styled runs are coalesced.
        assert_eq!(runs[0].text, "plain ");
    }

    #[test]
    fn bare_web_urls_become_links_without_swallowing_prose_punctuation() {
        let tree = parse(
            "See https://example.com/docs?q=one, then \
             (https://en.wikipedia.org/wiki/Rust_(programming_language)).",
        );
        let Block::Paragraph { runs } = &tree.blocks[0].block else {
            panic!("expected a paragraph");
        };
        let links = runs
            .iter()
            .filter_map(|run| {
                run.style
                    .link
                    .as_deref()
                    .map(|target| (run.text.as_str(), target))
            })
            .collect::<Vec<_>>();

        assert_eq!(
            links,
            vec![
                (
                    "https://example.com/docs?q=one",
                    "https://example.com/docs?q=one"
                ),
                (
                    "https://en.wikipedia.org/wiki/Rust_(programming_language)",
                    "https://en.wikipedia.org/wiki/Rust_(programming_language)"
                ),
            ]
        );
        assert_eq!(
            paragraph_text(&tree.blocks[0].block),
            "See https://example.com/docs?q=one, then \
             (https://en.wikipedia.org/wiki/Rust_(programming_language))."
        );
    }

    #[test]
    fn explicit_links_and_inline_code_are_not_relinkified() {
        let tree =
            parse("[docs at https://example.com](https://waku.gg) and `https://example.com/code`");
        let Block::Paragraph { runs } = &tree.blocks[0].block else {
            panic!("expected a paragraph");
        };

        assert!(runs.iter().any(|run| {
            run.text == "docs at https://example.com"
                && run.style.link.as_deref() == Some("https://waku.gg")
        }));
        assert!(runs.iter().any(|run| {
            run.text == "https://example.com/code" && run.style.code && run.style.link.is_none()
        }));
    }

    #[test]
    fn task_list_markers_lift_out_of_item_content() {
        let tree = parse("- [x] done\n- [ ] pending\n- plain\n");
        let Block::List { items, .. } = &tree.blocks[0].block else {
            panic!("expected a list");
        };

        assert_eq!(items.len(), 3);
        assert_eq!(items[0].task, Some(true));
        assert_eq!(items[1].task, Some(false));
        assert_eq!(items[2].task, None);
        assert_eq!(paragraph_text(&items[0].blocks[0]), "done");
        assert_eq!(paragraph_text(&items[2].blocks[0]), "plain");
    }

    #[test]
    fn tables_keep_alignment_and_cells() {
        let tree = parse("| a | b |\n|:--|--:|\n| 1 | 2 |\n");
        let Block::Table {
            header,
            rows,
            align,
        } = &tree.blocks[0].block
        else {
            panic!("expected a table");
        };

        assert_eq!(align, &[TableAlign::Left, TableAlign::Right]);
        assert_eq!(header.len(), 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1][0].text, "2");
    }

    /// An image is content, not styling, so it becomes its own block and the
    /// text around it keeps its order.
    #[test]
    fn images_split_their_paragraph_and_keep_order() {
        let tree = parse("before ![a shot](https://example.com/x.png) after");
        assert_eq!(tree.len(), 3);
        assert_eq!(paragraph_text(&tree.blocks[0].block), "before ");
        assert_eq!(
            tree.blocks[1].block,
            Block::Image {
                url: "https://example.com/x.png".into(),
                alt: "a shot".into(),
            }
        );
        assert_eq!(paragraph_text(&tree.blocks[2].block), " after");
    }

    #[test]
    fn a_standalone_image_is_one_block() {
        let tree = parse("![](data:image/png;base64,aGk=)");
        assert_eq!(tree.len(), 1);
        assert!(matches!(tree.blocks[0].block, Block::Image { .. }));
    }

    /// A table cell has no room for a block-level image, so it degrades to alt
    /// text rather than dropping the content entirely.
    #[test]
    fn images_in_table_cells_fall_back_to_alt_text() {
        let tree = parse("| a | b |\n|---|---|\n| ![alt](x.png) | plain |\n");
        let Block::Table { rows, .. } = &tree.blocks[0].block else {
            panic!("expected a table");
        };
        assert_eq!(rows[0][0][0].text, "alt");
    }

    #[test]
    fn br_tags_in_table_cells_become_line_breaks() {
        let tree = parse("| a | b |\n|---|---|\n| **Gemini**<br>`gemini` | x<br/>y<BR />z |\n");
        let Block::Table { rows, .. } = &tree.blocks[0].block else {
            panic!("expected a table");
        };
        let cell = |index: usize| {
            rows[0][index]
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>()
        };
        assert_eq!(cell(0), "Gemini\ngemini");
        assert_eq!(cell(1), "x\ny\nz");
    }

    #[test]
    fn other_inline_html_stays_literal() {
        let tree = parse("a <span>b</span> <brx>");
        assert_eq!(
            paragraph_text(&tree.blocks[0].block),
            "a <span>b</span> <brx>"
        );
    }

    #[test]
    fn soft_breaks_become_newlines_in_the_run() {
        let tree = parse("first\nsecond");
        assert_eq!(paragraph_text(&tree.blocks[0].block), "first\nsecond");
    }

    /// The incremental path must agree with a full parse at every prefix —
    /// that equality is the whole justification for the fast path.
    #[test]
    fn incremental_appends_match_full_parses() {
        let source = "# Heading\n\nA paragraph with **bold**.\n\n- one\n- two\n\n```js\nlet x = 1;\n```\n\nTail.";
        for chunk_size in [1, 3, 7, 64] {
            let mut incremental = IncrementalParser::new();
            let mut built = String::new();
            let mut chars = source.chars().peekable();
            while chars.peek().is_some() {
                let chunk = chars.by_ref().take(chunk_size).collect::<String>();
                built.push_str(&chunk);
                incremental.append(&chunk);
                assert_eq!(
                    incremental.tree(),
                    &parse(&built),
                    "divergence at {} bytes with chunk size {chunk_size}",
                    built.len()
                );
            }
        }
    }

    /// Tables must survive streaming: a chunk boundary that lands after the
    /// delimiter row must not settle a header-only table and strand the body
    /// rows in a trailing paragraph.
    #[test]
    fn streamed_tables_match_full_parses() {
        let source = "**After** \u{2014} it's a proper neutral chip:\n\n\
            | State | Fill | Icon |\n\
            |---|---|---|\n\
            | Rest | white @ 0.12 | `.labelColor` (~85%) |\n\
            | Hover | white @ 0.20 | pure white |\n\
            | Pressed | white @ 0.26 | pure white |\n\
            | Copied | green @ 0.12 | `.systemGreen` |\n\n\
            It now reads as a real button sitting beside Export.";
        for chunk_size in [1, 2, 3, 5, 7, 11, 17, 64] {
            let mut incremental = IncrementalParser::new();
            let mut built = String::new();
            let mut chars = source.chars().peekable();
            while chars.peek().is_some() {
                let chunk = chars.by_ref().take(chunk_size).collect::<String>();
                built.push_str(&chunk);
                incremental.append(&chunk);
                assert_eq!(
                    incremental.tree(),
                    &parse(&built),
                    "divergence at {} bytes with chunk size {chunk_size}",
                    built.len()
                );
            }
        }
    }

    /// The blocks a paragraph splits into around an inline image share one
    /// source range, so they must settle and reparse as a unit: settling only
    /// part of the group would re-emit the earlier pieces on the next append.
    #[test]
    fn streamed_inline_images_do_not_duplicate_blocks() {
        let source = "before ![a shot](https://example.com/x.png) after, and more prose.";
        for chunk_size in [1, 3, 7] {
            let mut incremental = IncrementalParser::new();
            let mut built = String::new();
            let mut chars = source.chars().peekable();
            while chars.peek().is_some() {
                let chunk = chars.by_ref().take(chunk_size).collect::<String>();
                built.push_str(&chunk);
                incremental.append(&chunk);
                assert_eq!(
                    incremental.tree(),
                    &parse(&built),
                    "divergence at {} bytes with chunk size {chunk_size}",
                    built.len()
                );
            }
        }
    }

    #[test]
    fn set_text_appends_incrementally_and_resets_on_rewrite() {
        let mut parser = IncrementalParser::new();
        parser.set_text("hello");
        parser.set_text("hello world");
        assert_eq!(parser.text(), "hello world");
        assert_eq!(parser.tree(), &parse("hello world"));

        // A non-append edit still lands on the right tree.
        parser.set_text("different");
        assert_eq!(parser.tree(), &parse("different"));
    }

    #[test]
    fn link_definitions_force_full_reparses() {
        let mut parser = IncrementalParser::new();
        parser.set_text("See [docs].\n\n[docs]: https://example.com\n");
        assert!(parser.full_reparse_only);

        parser.set_text("See [docs].\n\n[docs]: https://example.com\n\nMore.");
        assert_eq!(
            parser.tree(),
            &parse("See [docs].\n\n[docs]: https://example.com\n\nMore.")
        );
        let Block::Paragraph { runs } = &parser.tree().blocks[0].block else {
            panic!("expected a paragraph");
        };
        assert_eq!(
            runs.iter().find(|run| run.style.link.is_some()).is_some(),
            true,
            "the definition should resolve the earlier reference"
        );
    }

    #[test]
    fn display_tree_closes_hanging_emphasis_while_streaming() {
        let mut parser = IncrementalParser::new();
        parser.set_text("Settled.\n\nNow **bold");

        // The canonical tree keeps the markers literal.
        let Block::Paragraph { runs } = &parser.tree().blocks[1].block else {
            panic!("expected a paragraph");
        };
        assert!(runs.iter().all(|run| !run.style.bold));

        // The display tree styles it immediately.
        let display = parser.display_tree();
        assert_eq!(display.len(), 2);
        let Block::Paragraph { runs } = &display.blocks[1].block else {
            panic!("expected a paragraph");
        };
        assert!(
            runs.iter().any(|run| run.style.bold && run.text == "bold"),
            "streaming bold should be styled before its closer arrives: {runs:?}"
        );
        // Settled blocks are untouched.
        assert_eq!(display.blocks[0], parser.tree().blocks[0]);
    }

    #[test]
    fn display_tree_leaves_code_blocks_literal() {
        let mut parser = IncrementalParser::new();
        parser.set_text("```rust\nlet a = **b;\n");
        assert_eq!(parser.display_tree(), *parser.tree());
    }

    #[test]
    fn only_shell_fences_marked_run_are_runnable() {
        let runnable = |source: &str| match &parse(source).blocks[0].block {
            Block::CodeBlock { runnable, .. } => *runnable,
            other => panic!("expected a code block, got {other:?}"),
        };
        assert!(runnable("```sh run\nls\n```"));
        assert!(runnable("```bash  RUN\nls\n```"));
        assert!(!runnable("```sh\nls\n```"));
        assert!(!runnable("```python run\nprint(1)\n```"));
        assert!(!runnable("```run\nls\n```"));
    }
}
