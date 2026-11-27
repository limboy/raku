//! Helpers for right-panel artifact consumption and annotation: file-type
//! classification, background CSV/TSV parsing + filtering/sorting, excerpt
//! formatting, and disposable local cache files for WebView PDF previews.

use std::cell::Cell;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{Entity, FocusHandle, ListState, ScrollHandle};

use crate::input::TextInput;
use crate::ui::scrollbar::ScrollbarState;

/// Hard bounds keep enormous multi-hundred-megabyte CSVs from exhausting
/// memory inside a right-panel preview while comfortably covering real tables.
pub(super) const CSV_MAX_PREVIEW_ROWS: usize = 50_000;
pub(super) const CSV_MAX_PREVIEW_COLS: usize = 128;

const CSV_MIN_COL_WIDTH: f32 = 96.0;
const CSV_MAX_COL_WIDTH: f32 = 280.0;
const CSV_WIDTH_SAMPLE_ROWS: usize = 200;
const CSV_NUMERIC_SAMPLE_ROWS: usize = 100;
const EXCERPT_TITLE_MAX_CHARS: usize = 60;

fn extension_lowercase(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
}

pub(super) fn is_pdf_file(path: &str) -> bool {
    extension_lowercase(path).as_deref() == Some("pdf")
}

pub(super) fn is_html_file(path: &str) -> bool {
    matches!(extension_lowercase(path).as_deref(), Some("html" | "htm"))
}

pub(super) fn is_tabular_file(path: &str) -> bool {
    matches!(extension_lowercase(path).as_deref(), Some("csv" | "tsv"))
}

pub(super) fn tabular_delimiter(path: &str) -> u8 {
    if extension_lowercase(path).as_deref() == Some("tsv") {
        b'\t'
    } else {
        b','
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Format a right-panel excerpt (selected Markdown or CSV row) into a
/// `(chip_title, payload)` pair for staging in the composer.
pub(super) fn format_file_excerpt_snippet(display_path: &str, excerpt: &str) -> (String, String) {
    let filename = Path::new(display_path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(display_path);
    let trimmed = excerpt.trim();
    let first_line = trimmed
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let chip_title = if first_line.is_empty() {
        truncate_chars(filename, EXCERPT_TITLE_MAX_CHARS)
    } else {
        truncate_chars(
            &format!("{filename}: {first_line}"),
            EXCERPT_TITLE_MAX_CHARS,
        )
    };
    let content = if trimmed.is_empty() {
        format!("Source: {display_path}")
    } else {
        format!("Source: {display_path}\n\n{trimmed}")
    };
    (chip_title, content)
}

fn sanitize_markdown_cell(cell: &str) -> String {
    let collapsed = cell.replace("\r\n", "\n").replace(['\r', '\n'], " ");
    collapsed.trim().replace('|', "\\|")
}

/// Format a CSV header + row pair as a compact two-line Markdown table so the
/// model immediately sees column names aligned with values.
pub(super) fn format_csv_row_markdown(headers: &[String], row: &[String]) -> String {
    let cols = headers.len().max(row.len()).max(1);
    let header_cells: Vec<String> = (0..cols)
        .map(|index| {
            headers
                .get(index)
                .map(|value| sanitize_markdown_cell(value))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| format!("Column {}", index + 1))
        })
        .collect();
    let separator_cells = vec!["---"; cols];
    let row_cells: Vec<String> = (0..cols)
        .map(|index| {
            row.get(index)
                .map(|value| sanitize_markdown_cell(value))
                .unwrap_or_default()
        })
        .collect();
    format!(
        "| {} |\n| {} |\n| {} |",
        header_cells.join(" | "),
        separator_cells.join(" | "),
        row_cells.join(" | "),
    )
}

/// Format a CSV row as tab-separated values for spreadsheet-friendly pasting.
pub(super) fn format_csv_row_tsv(row: &[String]) -> String {
    row.iter()
        .map(|cell| cell.replace("\r\n", " ").replace(['\r', '\n', '\t'], " "))
        .collect::<Vec<_>>()
        .join("\t")
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct CsvTable {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub col_widths: Vec<f32>,
    pub col_numeric: Vec<bool>,
    /// Each row's cells, lowercased once at parse time and joined by
    /// [`CSV_SEARCH_SEPARATOR`], so filtering allocates nothing per keystroke.
    pub search_text: Vec<String>,
    pub truncated_rows: bool,
    pub truncated_cols: bool,
}

/// Joins a row's lowercased cells in [`CsvTable::search_text`]. A control
/// character nobody types, so a query cannot match across a cell boundary.
const CSV_SEARCH_SEPARATOR: char = '\u{1f}';

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CsvSortDirection {
    Asc,
    Desc,
}

pub(super) struct CsvTableViewState {
    pub filter: Entity<TextInput>,
    pub sort: Option<(usize, CsvSortDirection)>,
    pub visible_rows: Vec<usize>,
    pub list_state: ListState,
    pub v_scrollbar: Rc<ScrollbarState>,
    pub h_scroll_handle: ScrollHandle,
    /// `(raw_row_index, optional_col_index)` recorded on right-mouse-down
    /// before bubbling up to the single shared CSV `context_menu`.
    pub context_cell: Rc<Cell<Option<(usize, Option<usize>)>>>,
    /// Bumped for every background filter/sort pass; a pass that lands after
    /// a newer one started is discarded.
    pub rows_generation: u64,
    /// Keyboard cursor as `(visible_row_index, col_index)`.
    pub cursor: Option<(usize, usize)>,
    /// The grid's single tab stop; arrow keys move [`Self::cursor`].
    pub grid_focus: FocusHandle,
}

fn parse_numeric_cell(raw: &str) -> Option<f64> {
    let mut s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let negative = if let Some(rest) = s.strip_prefix('-') {
        s = rest.trim_start();
        true
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest.trim_start();
        false
    } else {
        false
    };
    if let Some(rest) = s
        .strip_prefix('$')
        .or_else(|| s.strip_prefix('€'))
        .or_else(|| s.strip_prefix('£'))
        .or_else(|| s.strip_prefix('¥'))
    {
        s = rest.trim_start();
    }
    let negative = if !negative && let Some(rest) = s.strip_prefix('-') {
        s = rest.trim_start();
        true
    } else {
        negative
    };
    if let Some(rest) = s.strip_suffix('%') {
        s = rest.trim_end();
    }
    if s.is_empty() {
        return None;
    }
    let cleaned = if s.contains(',') {
        s.replace(',', "")
    } else {
        s.to_owned()
    };
    let parsed: f64 = cleaned.parse().ok()?;
    if !parsed.is_finite() {
        return None;
    }
    Some(if negative { -parsed } else { parsed })
}

fn display_units_for_width(text: &str) -> f32 {
    let first_line = text.lines().next().unwrap_or("");
    let mut units = 0.0f32;
    for ch in first_line.chars().take(48) {
        if ch.is_ascii() {
            units += 1.0;
        } else {
            units += 1.8;
        }
    }
    units
}

fn parse_csv_records(input: &str, delimiter: u8) -> (Vec<Vec<String>>, bool, bool) {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    if input.is_empty() {
        return (Vec::new(), false, false);
    }

    let delim_char = delimiter as char;
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut truncated_rows = false;
    let mut truncated_cols = false;

    let mut chars = input.chars().peekable();
    let mut current_record: Vec<String> = Vec::new();
    let mut current_field = String::new();
    let mut at_field_start = true;
    let mut in_quotes = false;
    let mut record_col_count = 0usize;
    // Whether the current record opened any quoted field, so a line holding
    // just `""` stays a real (empty-valued) record rather than a blank line.
    let mut quoted_field = false;

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    current_field.push('"');
                } else {
                    in_quotes = false;
                }
            } else if ch == '\r' {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                current_field.push('\n');
            } else {
                current_field.push(ch);
            }
            continue;
        }

        if at_field_start && ch == '"' {
            in_quotes = true;
            quoted_field = true;
            at_field_start = false;
            continue;
        }

        if ch == delim_char {
            record_col_count += 1;
            if record_col_count <= CSV_MAX_PREVIEW_COLS {
                current_record.push(std::mem::take(&mut current_field));
            } else {
                truncated_cols = true;
                current_field.clear();
            }
            at_field_start = true;
        } else if ch == '\r' || ch == '\n' {
            if ch == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            record_col_count += 1;
            if record_col_count <= CSV_MAX_PREVIEW_COLS {
                current_record.push(std::mem::take(&mut current_field));
            } else {
                truncated_cols = true;
                current_field.clear();
            }
            // A blank line is not a record: skip it instead of emitting a
            // phantom all-empty row (this also covers trailing newlines).
            if record_col_count == 1
                && current_record.len() == 1
                && current_record[0].is_empty()
                && !quoted_field
            {
                current_record.clear();
                record_col_count = 0;
                at_field_start = true;
                continue;
            }
            // Data rows are capped at CSV_MAX_PREVIEW_ROWS (plus 1 header row).
            if records.len() > CSV_MAX_PREVIEW_ROWS {
                truncated_rows = true;
                current_record.clear();
                break;
            }
            records.push(std::mem::take(&mut current_record));
            record_col_count = 0;
            quoted_field = false;
            at_field_start = true;
        } else {
            current_field.push(ch);
            at_field_start = false;
        }
    }

    if !truncated_rows
        && (!current_record.is_empty() || !current_field.is_empty() || !at_field_start)
    {
        record_col_count += 1;
        if record_col_count <= CSV_MAX_PREVIEW_COLS {
            current_record.push(current_field);
        } else {
            truncated_cols = true;
        }
        if records.len() > CSV_MAX_PREVIEW_ROWS {
            truncated_rows = true;
        } else {
            records.push(current_record);
        }
    }

    (records, truncated_rows, truncated_cols)
}

/// Parse CSV or TSV text into a normalized, column-typed table suitable for
/// virtualized rendering. Intended to run off the UI thread.
pub(super) fn parse_csv(input: &str, delimiter: u8) -> CsvTable {
    let (mut records, truncated_rows, truncated_cols) = parse_csv_records(input, delimiter);
    if records.is_empty() {
        return CsvTable {
            headers: Vec::new(),
            rows: Vec::new(),
            col_widths: Vec::new(),
            col_numeric: Vec::new(),
            search_text: Vec::new(),
            truncated_rows,
            truncated_cols,
        };
    }

    let mut headers = records.remove(0);
    let col_count = headers
        .len()
        .max(records.iter().map(Vec::len).max().unwrap_or(0))
        .min(CSV_MAX_PREVIEW_COLS);
    for col in headers.len()..col_count {
        headers.push(format!("Column {}", col + 1));
    }
    for row in &mut records {
        if row.len() < col_count {
            row.resize(col_count, String::new());
        } else if row.len() > col_count {
            row.truncate(col_count);
        }
    }

    let mut col_widths = Vec::with_capacity(col_count);
    let mut col_numeric = Vec::with_capacity(col_count);
    for col in 0..col_count {
        let header_units = headers
            .get(col)
            .map(|h| display_units_for_width(h))
            .unwrap_or(0.0);
        let max_cell_units = records
            .iter()
            .take(CSV_WIDTH_SAMPLE_ROWS)
            .filter_map(|row| row.get(col))
            .map(|cell| display_units_for_width(cell))
            .fold(0.0f32, f32::max);
        // Reserve extra space in header cells for the sort indicator icon.
        let width = (header_units * 8.2 + 44.0)
            .max(max_cell_units * 8.2 + 28.0)
            .clamp(CSV_MIN_COL_WIDTH, CSV_MAX_COL_WIDTH)
            .round();
        col_widths.push(width);

        let mut non_empty = 0usize;
        let mut numeric = 0usize;
        for row in &records {
            let Some(cell) = row.get(col).map(|s| s.trim()) else {
                continue;
            };
            if cell.is_empty() {
                continue;
            }
            non_empty += 1;
            if parse_numeric_cell(cell).is_some() {
                numeric += 1;
            }
            if non_empty >= CSV_NUMERIC_SAMPLE_ROWS {
                break;
            }
        }
        col_numeric.push(non_empty > 0 && numeric * 5 >= non_empty * 4);
    }

    let search_text = records
        .iter()
        .map(|row| {
            let mut text = String::new();
            for (index, cell) in row.iter().enumerate() {
                if index > 0 {
                    text.push(CSV_SEARCH_SEPARATOR);
                }
                text.extend(cell.chars().flat_map(char::to_lowercase));
            }
            text
        })
        .collect();

    CsvTable {
        headers,
        rows: records,
        col_widths,
        col_numeric,
        search_text,
        truncated_rows,
        truncated_cols,
    }
}

/// Filter row indices by substring query and apply stable numeric-aware sorting
/// with empty cells always ordered last. Runs on a background worker: a full
/// table is far too much work for a keystroke on the UI thread.
pub(super) fn compute_visible_rows(
    table: &CsvTable,
    query: &str,
    sort: Option<(usize, CsvSortDirection)>,
) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    let indices: Vec<usize> = if needle.is_empty() {
        (0..table.rows.len()).collect()
    } else {
        table
            .search_text
            .iter()
            .enumerate()
            .filter(|(_, text)| text.contains(needle.as_str()))
            .map(|(index, _)| index)
            .collect()
    };

    let Some((col, direction)) = sort else {
        return indices;
    };

    enum SortKey {
        Number(f64),
        Text(String),
    }

    let is_numeric = table.col_numeric.get(col).copied().unwrap_or(false);
    // One key per row instead of two lowercased strings per comparison.
    let mut keyed: Vec<(Option<SortKey>, usize)> = indices
        .into_iter()
        .map(|index| {
            let raw = table.rows[index]
                .get(col)
                .map(String::as_str)
                .unwrap_or("")
                .trim();
            let key = if raw.is_empty() {
                None
            } else if let Some(number) = is_numeric.then(|| parse_numeric_cell(raw)).flatten() {
                Some(SortKey::Number(number))
            } else {
                Some(SortKey::Text(raw.to_lowercase()))
            };
            (key, index)
        })
        .collect();
    keyed.sort_by(|(a, _), (b, _)| {
        let ordering = match (a, b) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Greater,
            (Some(_), None) => return Ordering::Less,
            (Some(SortKey::Number(a)), Some(SortKey::Number(b))) => a.total_cmp(b),
            (Some(SortKey::Number(_)), Some(SortKey::Text(_))) => Ordering::Less,
            (Some(SortKey::Text(_)), Some(SortKey::Number(_))) => Ordering::Greater,
            (Some(SortKey::Text(a)), Some(SortKey::Text(b))) => a.cmp(b),
        };
        match direction {
            CsvSortDirection::Asc => ordering,
            CsvSortDirection::Desc => ordering.reverse(),
        }
    });
    keyed.into_iter().map(|(_, index)| index).collect()
}

fn fnv1a_128(parts: &[&[u8]]) -> u128 {
    const OFFSET_BASIS: u128 = 0x6c62272e07bb0142_62b821756295c58d;
    const PRIME: u128 = 0x0000000001000000_000000000000013b;
    let mut hash = OFFSET_BASIS;
    for part in parts {
        for &byte in *part {
            hash ^= u128::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Write a PDF attachment or non-local PDF payload to Raku's disposable temp
/// cache so the native WebView can load it by `file://` URL. Always run off
/// the UI thread.
pub(super) fn materialize_pdf_preview_cache(key: &str, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        bytes.len() <= raku_protocol::attachments::MAX_ATTACHMENT_BYTES,
        "PDF exceeds 32 MB"
    );
    let cache_dir = std::env::temp_dir().join("raku-preview-cache");
    std::fs::create_dir_all(&cache_dir)?;
    prune_pdf_preview_cache(&cache_dir);
    let hash = fnv1a_128(&[key.as_bytes(), bytes]);
    let path = cache_dir.join(format!("{hash:032x}.pdf"));
    // Only a complete file ever appears under the final name: a crash or full
    // disk mid-write leaves a stray temp file, never a truncated cache hit.
    let reusable = std::fs::metadata(&path)
        .is_ok_and(|meta| meta.is_file() && meta.len() == bytes.len() as u64);
    if reusable {
        // Refresh the timestamp so pruning keeps a file that is still in use.
        if let Ok(file) = std::fs::File::options().write(true).open(&path) {
            let _ = file.set_modified(std::time::SystemTime::now());
        }
        return Ok(path);
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let temp = cache_dir.join(format!("{hash:032x}.{}-{nonce}.tmp", std::process::id()));
    if let Err(error) = std::fs::write(&temp, bytes).and_then(|()| std::fs::rename(&temp, &path)) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(path)
}

/// Cached previews and abandoned temp files older than this are removed the
/// next time a PDF is materialized, so the cache cannot grow without bound.
const PDF_PREVIEW_CACHE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

fn prune_pdf_preview_cache(cache_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let stale = meta.is_file()
            && meta
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > PDF_PREVIEW_CACHE_MAX_AGE);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_classifiers_recognize_pdf_html_and_tabular_extensions() {
        assert!(is_pdf_file("report.PDF"));
        assert!(!is_pdf_file("report.pdf.txt"));

        assert!(is_html_file("dashboard.html"));
        assert!(is_html_file("PAGE.HTM"));
        assert!(!is_html_file("styles.css"));

        assert!(is_tabular_file("metrics.csv"));
        assert!(is_tabular_file("export.TSV"));
        assert_eq!(tabular_delimiter("metrics.csv"), b',');
        assert_eq!(tabular_delimiter("export.TSV"), b'\t');
    }

    #[test]
    fn csv_parser_handles_bom_quotes_embedded_newlines_and_jagged_rows() {
        let raw = "\u{feff}Name,Notes,Revenue\r\n\"Alice, B.\",\"Line 1\nLine 2\",\"$1,250.50\"\r\nBob,\"Quoted \"\"ok\"\"\",42%\r\nCara\r\n";
        let table = parse_csv(raw, b',');
        assert_eq!(table.headers, vec!["Name", "Notes", "Revenue"]);
        assert_eq!(table.rows.len(), 3);
        assert_eq!(
            table.rows[0],
            vec!["Alice, B.", "Line 1\nLine 2", "$1,250.50"]
        );
        assert_eq!(table.rows[1], vec!["Bob", "Quoted \"ok\"", "42%"]);
        assert_eq!(table.rows[2], vec!["Cara", "", ""]);
        assert!(!table.col_numeric[0]);
        assert!(!table.col_numeric[1]);
        assert!(table.col_numeric[2]);
        assert!(!table.truncated_rows);
        assert!(!table.truncated_cols);
    }

    #[test]
    fn csv_parser_skips_blank_lines_without_tripping_the_row_cap() {
        let table = parse_csv("a,b\n\n1,2\r\n\r\n3,4\n\n", b',');
        assert_eq!(table.rows, vec![vec!["1", "2"], vec!["3", "4"]]);

        let quoted_empty = parse_csv("a\n\"\"\nx\n", b',');
        assert_eq!(quoted_empty.rows, vec![vec![""], vec!["x"]]);

        let mut raw = String::from("n\n");
        for row in 0..CSV_MAX_PREVIEW_ROWS {
            raw.push_str(&format!("{row}\n"));
        }
        raw.push('\n');
        let table = parse_csv(&raw, b',');
        assert_eq!(table.rows.len(), CSV_MAX_PREVIEW_ROWS);
        assert!(!table.truncated_rows);
    }

    #[test]
    fn csv_parser_handles_tsv_and_extra_data_columns() {
        let raw = "a\tb\n1\t2\t3\n";
        let table = parse_csv(raw, b'\t');
        assert_eq!(table.headers, vec!["a", "b", "Column 3"]);
        assert_eq!(table.rows, vec![vec!["1", "2", "3"]]);
        assert_eq!(table.col_numeric, vec![true, true, true]);
    }

    #[test]
    fn csv_parser_enforces_row_and_column_caps() {
        let mut raw = String::new();
        for col in 0..(CSV_MAX_PREVIEW_COLS + 5) {
            if col > 0 {
                raw.push(',');
            }
            raw.push_str(&format!("c{col}"));
        }
        raw.push('\n');
        for row in 0..(CSV_MAX_PREVIEW_ROWS + 3) {
            raw.push_str(&format!("{row}\n"));
        }
        let table = parse_csv(&raw, b',');
        assert!(table.truncated_cols);
        assert!(table.truncated_rows);
        assert_eq!(table.headers.len(), CSV_MAX_PREVIEW_COLS);
        assert_eq!(table.rows.len(), CSV_MAX_PREVIEW_ROWS);
    }

    #[test]
    fn csv_visible_rows_filter_and_sort_with_empty_cells_last() {
        let raw = "Item,Amount\nBeta,100\nAlpha,$20\nGamma,\nDelta,-5\n";
        let table = parse_csv(raw, b',');

        assert_eq!(
            compute_visible_rows(&table, "", Some((1, CsvSortDirection::Asc))),
            vec![3, 1, 0, 2]
        );
        assert_eq!(
            compute_visible_rows(&table, "", Some((1, CsvSortDirection::Desc))),
            vec![0, 1, 3, 2]
        );
        assert_eq!(compute_visible_rows(&table, "al", None), vec![1]);
        assert_eq!(compute_visible_rows(&table, "ALPHA", None), vec![1]);
        // A query never matches across the boundary between two cells.
        assert!(compute_visible_rows(&table, "beta100", None).is_empty());
    }

    #[test]
    fn excerpt_and_row_formatting_preserve_source_and_markdown_table_syntax() {
        let (title, payload) =
            format_file_excerpt_snippet("docs/plan.md", "\n  First line of plan\nSecond line");
        assert_eq!(title, "plan.md: First line of plan");
        assert_eq!(
            payload,
            "Source: docs/plan.md\n\nFirst line of plan\nSecond line"
        );

        let md = format_csv_row_markdown(
            &["Col | A".into(), "Col B".into()],
            &["x | y".into(), "line1\nline2".into()],
        );
        assert_eq!(
            md,
            "| Col \\| A | Col B |\n| --- | --- |\n| x \\| y | line1 line2 |"
        );

        let tsv = format_csv_row_tsv(&["a\tb".into(), "c\nd".into()]);
        assert_eq!(tsv, "a b\tc d");
    }

    #[test]
    fn pdf_preview_cache_writes_content_hashed_files() {
        let path_a = materialize_pdf_preview_cache("doc.pdf", b"%PDF-1.4 A").unwrap();
        let path_b = materialize_pdf_preview_cache("doc.pdf", b"%PDF-1.4 B").unwrap();
        assert_ne!(path_a, path_b);
        assert_eq!(std::fs::read(&path_a).unwrap(), b"%PDF-1.4 A");
        assert_eq!(std::fs::read(&path_b).unwrap(), b"%PDF-1.4 B");
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }
}
