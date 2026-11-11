use super::*;

/// A visual gap before an inline mention. Source text and selection offsets
/// stay unchanged; only the shaped string contains this non-breaking space.
const GAP: &str = "\u{a0}";

#[derive(Debug)]
pub struct PromptSpacing {
    pub text: SharedString,
    offsets: Vec<usize>,
    display_offsets: Vec<usize>,
}

impl PromptSpacing {
    pub fn new(text: &str, tokens: &[Range<usize>]) -> Option<Rc<Self>> {
        let offsets = tokens
            .iter()
            .map(|range| range.start)
            .filter(|start| *start > 0 && !text[..*start].ends_with(['\n', '\r']))
            .collect::<Vec<_>>();
        if offsets.is_empty() {
            return None;
        }
        let mut display = String::with_capacity(text.len() + offsets.len() * GAP.len());
        let mut start = 0;
        for &offset in &offsets {
            display.push_str(&text[start..offset]);
            display.push_str(GAP);
            start = offset;
        }
        display.push_str(&text[start..]);
        let display_offsets = offsets
            .iter()
            .enumerate()
            .map(|(ordinal, offset)| offset + ordinal * GAP.len())
            .collect();
        Some(Rc::new(Self {
            text: display.into(),
            offsets,
            display_offsets,
        }))
    }

    pub fn display_range(&self, range: &Range<usize>) -> Range<usize> {
        let start = self
            .offsets
            .partition_point(|offset| *offset <= range.start);
        if range.is_empty() {
            let index = range.start + start * GAP.len();
            return index..index;
        }
        let end = self.offsets.partition_point(|offset| *offset < range.end);
        range.start + start * GAP.len()..range.end + end * GAP.len()
    }

    pub fn source_index(&self, index: usize) -> usize {
        let preceding = self
            .display_offsets
            .partition_point(|offset| offset + GAP.len() <= index);
        if let Some(&offset) = self.offsets.get(preceding)
            && index >= offset + preceding * GAP.len()
        {
            offset
        } else {
            index - preceding * GAP.len()
        }
    }

    pub fn runs(&self, runs: Vec<TextRun>) -> Vec<TextRun> {
        let mut display = Vec::with_capacity(runs.len() + self.offsets.len() * 2);
        let mut gaps = self.offsets.iter().copied().peekable();
        let mut start = 0;
        for run in runs {
            let end = start + run.len;
            while let Some(offset) = gaps.next_if(|offset| *offset < end) {
                if offset > start {
                    display.push(TextRun {
                        len: offset - start,
                        ..run.clone()
                    });
                }
                display.push(TextRun {
                    len: GAP.len(),
                    underline: None,
                    strikethrough: None,
                    ..run.clone()
                });
                start = offset;
            }
            if start < end {
                display.push(TextRun {
                    len: end - start,
                    ..run
                });
            }
            start = end;
        }
        display
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spacing_keeps_unicode_ranges_in_source_coordinates() {
        let source = "看 subagent @explore 🙂 @general?";
        let ranges = ["@explore", "@general"].map(|label| {
            let start = source.find(label).unwrap();
            start..start + label.len()
        });
        let spacing = PromptSpacing::new(source, &ranges).unwrap();
        assert_eq!(
            spacing.text.as_ref(),
            "看 subagent \u{a0}@explore 🙂 \u{a0}@general?"
        );
        for (index, character) in source.char_indices() {
            let range = index..index + character.len_utf8();
            let display = spacing.display_range(&range);
            assert_eq!(&spacing.text[display.clone()], &source[range]);
            assert_eq!(spacing.source_index(display.start), index);
        }
        for (ordinal, range) in ranges.iter().enumerate() {
            let start = range.start + ordinal * GAP.len();
            for index in start..start + GAP.len() {
                assert_eq!(spacing.source_index(index), range.start);
            }
        }
        assert_eq!(spacing.source_index(spacing.text.len()), source.len());
    }

    #[test]
    fn mentions_at_the_start_of_a_line_keep_their_alignment() {
        let source = "@explore\n@general";
        assert!(PromptSpacing::new(source, &[0..8, 9..17]).is_none());
    }

    #[test]
    fn display_runs_tile_without_underlining_the_added_gap() {
        let source = "Open @a.rs then @explore";
        let ranges = [5..10, 16..24];
        let spacing = PromptSpacing::new(source, &ranges).unwrap();
        let base = TextRun {
            font: font(SANS_FAMILY),
            underline: Some(UnderlineStyle {
                thickness: px(1.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let runs = spacing.runs(
            [5, 5, 6, 8]
                .into_iter()
                .map(|len| TextRun {
                    len,
                    ..base.clone()
                })
                .collect(),
        );
        let mut start = 0;
        for run in runs {
            let end = start + run.len;
            let text = &spacing.text[start..end];
            assert_eq!(run.underline.is_none(), text == GAP);
            start = end;
        }
        assert_eq!(start, spacing.text.len());
    }
}
