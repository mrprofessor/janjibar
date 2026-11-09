//! Openable URLs under the pointer in one row.
//!
//! Derived from Herdr (Apache-2.0).

use std::ops::Range;

use crate::frame::TerminalRow;

/// Longest URL offered to the opener, so a pathological row cannot produce
/// an unbounded string.
const MAX_URL: usize = 2048;

const SCHEMES: [&str; 4] = ["https://", "http://", "file://", "mailto:"];

/// Only schemes worth handing to the platform opener. A program can put
/// any string in an OSC 8 target, so an unvalidated target would let output
/// choose an arbitrary URI scheme to launch.
#[must_use]
pub fn is_openable(url: &str) -> bool {
    let url = url.trim();
    url.len() <= MAX_URL
        && !url.contains(char::is_whitespace)
        && !url.contains(char::is_control)
        && SCHEMES
            .iter()
            .any(|scheme| url.len() > scheme.len() && url.starts_with(scheme))
}

/// A URL in one row, with the columns it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The URL.
    pub url: String,
    /// The columns the link covers.
    pub columns: Range<u16>,
}

/// The link covering `column`, if any. An OSC 8 hyperlink wins; only a cell
/// without one has the row text scanned for a bare URL.
#[must_use]
pub fn at(row: &TerminalRow, column: u16) -> Option<Link> {
    explicit(row, column).or_else(|| detected(row, column))
}

fn explicit(row: &TerminalRow, column: u16) -> Option<Link> {
    let cell = row
        .cells
        .iter()
        .find(|cell| cell.column == column && cell.hyperlink.is_some())?;
    let uri = cell.hyperlink.as_ref()?;
    if !is_openable(uri) {
        return None;
    }
    let mut columns = cell.column..cell.column.saturating_add(u16::from(cell.width.max(1)));
    for other in row.cells.iter() {
        if other.hyperlink.as_deref() != Some(uri.as_ref()) {
            continue;
        }
        let end = other.column.saturating_add(u16::from(other.width.max(1)));
        columns.start = columns.start.min(other.column);
        columns.end = columns.end.max(end);
    }
    Some(Link {
        url: uri.to_string(),
        columns,
    })
}

fn detected(row: &TerminalRow, column: u16) -> Option<Link> {
    let text = row.text.as_ref();
    for scheme in SCHEMES {
        let mut search = 0;
        while let Some(found) = text[search..].find(scheme) {
            let start = search + found;
            if is_url_byte(text.as_bytes().get(start.wrapping_sub(1)).copied()) {
                search = start + scheme.len();
                continue;
            }
            let end = end_of_url(text, start);
            if end - start <= MAX_URL
                && let Some(columns) = columns_for(row, start..end)
            {
                let url = trim_trailing(&text[start..end]);
                if columns.contains(&column) && is_openable(url) {
                    return Some(Link {
                        url: url.to_string(),
                        columns,
                    });
                }
            }
            search = end.max(start + scheme.len());
        }
    }
    None
}

fn end_of_url(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut end = start;
    while end < bytes.len() && is_url_byte(Some(bytes[end])) {
        end += 1;
    }
    start + trim_trailing(&text[start..end]).len()
}

fn is_url_byte(byte: Option<u8>) -> bool {
    match byte {
        None => false,
        Some(byte) => {
            !byte.is_ascii_whitespace()
                && !byte.is_ascii_control()
                && byte != b'"'
                && byte != b'\''
                && byte != b'<'
                && byte != b'>'
                && byte != b'`'
        }
    }
}

fn trim_trailing(url: &str) -> &str {
    url.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}'])
}

fn columns_for(row: &TerminalRow, text: Range<usize>) -> Option<Range<u16>> {
    let mut columns: Option<Range<u16>> = None;
    for cell in row.cells.iter() {
        if cell.text.start >= text.end || cell.text.end <= text.start {
            continue;
        }
        let end = cell.column.saturating_add(u16::from(cell.width.max(1)));
        columns = Some(match columns {
            None => cell.column..end,
            Some(range) => range.start.min(cell.column)..range.end.max(end),
        });
    }
    columns.filter(|range| range.start < range.end)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::frame::CellSpan;

    fn row(text: &str) -> TerminalRow {
        let cells: Vec<CellSpan> = text
            .char_indices()
            .enumerate()
            .map(|(index, (offset, ch))| CellSpan {
                text: offset..offset + ch.len_utf8(),
                column: u16::try_from(index).unwrap(),
                width: 1,
                ..CellSpan::default()
            })
            .collect();
        TerminalRow {
            text: Arc::from(text),
            cells: Arc::from(cells),
            ..TerminalRow::default()
        }
    }

    fn linked(text: &str, uri: &str, columns: Range<u16>) -> TerminalRow {
        let mut row = row(text);
        let cells: Vec<CellSpan> = row
            .cells
            .iter()
            .map(|cell| {
                let mut cell = cell.clone();
                if columns.contains(&cell.column) {
                    cell.hyperlink = Some(Arc::from(uri));
                }
                cell
            })
            .collect();
        row.cells = Arc::from(cells);
        row
    }

    #[test]
    fn a_url_is_found_only_under_the_columns_it_covers() {
        let found = row("see https://example.com/x now");
        assert!(at(&found, 0).is_none());
        let link = at(&found, 6).expect("link");
        assert_eq!(link.url, "https://example.com/x");
        assert_eq!(link.columns, 4..25);
        assert!(at(&found, 26).is_none());
        assert!(at(&row("xhttps://example.com"), 3).is_none());
    }

    #[test]
    fn trailing_punctuation_and_other_schemes_are_handled() {
        let punctuated = row("read https://example.com/page).");
        assert_eq!(
            at(&punctuated, 10).expect("link").url,
            "https://example.com/page"
        );
        for text in [
            "file:///tmp/log.txt",
            "mailto:someone@example.com",
            "http://localhost:8080/health",
        ] {
            assert_eq!(at(&row(text), 2).expect("link").url, text);
        }
        assert!(at(&row("example.com is not a url"), 2).is_none());
    }

    #[test]
    fn an_osc8_target_wins_and_unsupported_schemes_are_refused() {
        let row = linked("click here", "https://example.com/real", 0..5);
        let link = at(&row, 2).expect("link");
        assert_eq!(link.url, "https://example.com/real");
        assert_eq!(link.columns, 0..5);
        assert!(at(&row, 7).is_none());
        let bad = linked("click here", "javascript:alert(1)", 0..5);
        assert!(at(&bad, 2).is_none());
        assert!(!is_openable("file:///etc/passwd extra"));
        assert!(!is_openable("https://"));
        assert!(is_openable("https://example.com"));
    }
}
