//! Text search over a log buffer: matching lines, the current match, and
//! the ranges to highlight.
//!
//! Search is smart-case: a query with an upper-case ASCII letter matches
//! case exactly, otherwise ASCII case is ignored. Matches are whole lines;
//! "3 of 7" counts lines, and next and previous move between them.

use std::ops::Range;

use model::{LogBuffer, LogChange};

/// The search state of one log pane.
#[derive(Debug, Default)]
pub struct LogSearch {
    query: String,
    /// The query in lower case, when case does not matter.
    folded: Option<String>,
    /// Matching lines as positions that survive eviction: the buffer's
    /// eviction count plus the index. Ascending.
    matches: Vec<u64>,
    /// Index into `matches`.
    current: Option<usize>,
    /// Reused for case folding, so scanning does not allocate per line.
    scratch: String,
}

impl LogSearch {
    /// Whether there is a query.
    pub fn is_active(&self) -> bool {
        !self.query.is_empty()
    }

    /// Matching lines.
    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.matches.len()
    }

    /// The current match as "n of count", 1-based.
    pub fn position(&self) -> Option<(usize, usize)> {
        self.current.map(|c| (c + 1, self.matches.len()))
    }

    /// Replaces the query and searches the whole buffer. The current match
    /// becomes the newest one.
    pub fn set_query(&mut self, query: &str, buffer: &LogBuffer) {
        if query == self.query {
            return;
        }
        query.clone_into(&mut self.query);
        self.folded = (!query.bytes().any(|b| b.is_ascii_uppercase())).then(|| query.to_owned());
        self.matches.clear();
        self.current = None;
        if query.is_empty() {
            return;
        }
        self.scan(buffer, 0);
        self.current = self.matches.len().checked_sub(1);
    }

    /// Updates matches after a push, scanning only the lines that changed.
    pub fn update(&mut self, change: LogChange, buffer: &LogBuffer) {
        if self.query.is_empty() || change.inserted == 0 && change.evicted == 0 {
            return;
        }
        let current_line = self.current_line_abs();
        let base = buffer.evicted();
        let first_changed = base + change.first_changed as u64;
        self.matches.retain(|&m| m >= base && m < first_changed);
        self.scan(buffer, change.first_changed);
        // Keep the same line current if it survived; otherwise the nearest
        // match after it, or the newest.
        self.current = match current_line {
            Some(line) if !self.matches.is_empty() => Some(
                self.matches
                    .partition_point(|&m| m < line)
                    .min(self.matches.len() - 1),
            ),
            _ => self.matches.len().checked_sub(1),
        };
    }

    /// Moves to the next match, wrapping around. Returns its line index.
    pub fn next(&mut self, buffer: &LogBuffer) -> Option<usize> {
        let count = self.matches.len();
        if count == 0 {
            return None;
        }
        self.current = Some(self.current.map_or(0, |c| (c + 1) % count));
        self.current_line(buffer)
    }

    /// Moves to the previous match, wrapping around. Returns its line index.
    pub fn prev(&mut self, buffer: &LogBuffer) -> Option<usize> {
        let count = self.matches.len();
        if count == 0 {
            return None;
        }
        self.current = Some(self.current.map_or(count - 1, |c| (c + count - 1) % count));
        self.current_line(buffer)
    }

    /// The buffer index of the current match.
    pub fn current_line(&self, buffer: &LogBuffer) -> Option<usize> {
        let abs = self.current_line_abs()?;
        usize::try_from(abs.checked_sub(buffer.evicted())?).ok()
    }

    /// Byte ranges of every occurrence in `text`, for highlighting.
    pub fn ranges(&self, text: &str) -> Vec<Range<usize>> {
        if self.query.is_empty() {
            return Vec::new();
        }
        let len = self.query.len();
        match &self.folded {
            // ASCII lower-casing keeps byte offsets and char boundaries.
            Some(folded) => text
                .to_ascii_lowercase()
                .match_indices(folded.as_str())
                .map(|(at, _)| at..at + len)
                .collect(),
            None => text
                .match_indices(self.query.as_str())
                .map(|(at, _)| at..at + len)
                .collect(),
        }
    }

    fn current_line_abs(&self) -> Option<u64> {
        self.matches.get(self.current?).copied()
    }

    /// Appends matches from `from` to the end of the buffer.
    fn scan(&mut self, buffer: &LogBuffer, from: usize) {
        let base = buffer.evicted();
        for (ix, line) in buffer.iter().enumerate().skip(from) {
            let hit = match &self.folded {
                Some(folded) => {
                    self.scratch.clear();
                    self.scratch.push_str(&line.text);
                    self.scratch.make_ascii_lowercase();
                    self.scratch.contains(folded.as_str())
                }
                None => line.text.contains(self.query.as_str()),
            };
            if hit {
                self.matches.push(base + ix as u64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use model::{Clock, LineKind, LogLine, LogSource};

    use super::*;

    fn line(second: i64, text: &str) -> LogLine {
        LogLine {
            time: jiff::Timestamp::from_second(1_800_000_000 + second).unwrap(),
            clock: Clock::default(),
            source: Arc::new(LogSource {
                pod: "p".into(),
                container: "c".into(),
            }),
            kind: LineKind::Text(None),
            text: text.into(),
        }
    }

    fn buffer(cap: usize, texts: &[&str]) -> LogBuffer {
        let mut b = LogBuffer::new(cap);
        b.push(
            texts
                .iter()
                .enumerate()
                .map(|(i, t)| line(i64::try_from(i).unwrap(), t)),
        );
        b
    }

    #[test]
    fn counts_matching_lines_and_starts_at_the_newest() {
        let b = buffer(10, &["Out of memory", "ok", "out of memory again", "fine"]);
        let mut s = LogSearch::default();
        s.set_query("out of memory", &b);
        assert_eq!(s.count(), 2);
        assert_eq!(s.position(), Some((2, 2)));
        assert_eq!(s.current_line(&b), Some(2));
    }

    #[test]
    fn upper_case_queries_match_case() {
        let b = buffer(10, &["Error here", "error there"]);
        let mut s = LogSearch::default();
        s.set_query("Error", &b);
        assert_eq!(s.count(), 1);
        assert_eq!(s.current_line(&b), Some(0));
    }

    #[test]
    fn next_and_prev_wrap_around() {
        let b = buffer(10, &["x", "-", "x", "-", "x"]);
        let mut s = LogSearch::default();
        s.set_query("x", &b);
        assert_eq!(s.current_line(&b), Some(4));
        assert_eq!(s.next(&b), Some(0), "wraps to the first");
        assert_eq!(s.next(&b), Some(2));
        assert_eq!(s.prev(&b), Some(0));
        assert_eq!(s.prev(&b), Some(4), "wraps to the last");
        assert_eq!(s.position(), Some((3, 3)));
    }

    #[test]
    fn appended_lines_are_scanned_incrementally() {
        let mut b = buffer(10, &["hit", "miss"]);
        let mut s = LogSearch::default();
        s.set_query("hit", &b);
        let change = b.push([line(10, "another hit"), line(11, "miss")]);
        s.update(change, &b);
        assert_eq!(s.count(), 2);
        assert_eq!(s.current_line(&b), Some(0), "the current match stays put");
    }

    #[test]
    fn evicted_matches_disappear_and_indices_follow() {
        let mut b = buffer(3, &["hit a", "miss", "hit b"]);
        let mut s = LogSearch::default();
        s.set_query("hit", &b);
        s.prev(&b);
        assert_eq!(s.current_line(&b), Some(0), "on hit a");
        let change = b.push([line(10, "miss"), line(11, "hit c")]);
        s.update(change, &b);
        assert_eq!(s.count(), 2, "hit a fell off");
        // Buffer is now [hit b, miss, hit c]; hit a was current, so the
        // nearest later match takes over.
        assert_eq!(s.current_line(&b), Some(0));
        assert_eq!(s.next(&b), Some(2));
    }

    #[test]
    fn a_late_line_inserted_mid_buffer_is_found() {
        let mut b = buffer(10, &["miss", "miss", "miss"]);
        let mut s = LogSearch::default();
        s.set_query("late", &b);
        assert_eq!(s.count(), 0);
        let change = b.push([line(1, "late hit")]);
        s.update(change, &b);
        assert_eq!(s.count(), 1);
        assert_eq!(s.current_line(&b), Some(2));
        assert_eq!(&*b.get(2).unwrap().text, "late hit");
    }

    #[test]
    fn ranges_cover_every_occurrence() {
        let mut s = LogSearch::default();
        s.set_query("ab", &LogBuffer::default());
        assert_eq!(s.ranges("xAbyab"), [1..3, 4..6]);
        s.set_query("AB", &LogBuffer::default());
        assert_eq!(s.ranges("xAbyAB"), vec![4..6]);
        s.set_query("ab", &LogBuffer::default());
        assert_eq!(
            s.ranges("héllo AB"),
            vec![7..9],
            "byte offsets past multi-byte chars"
        );
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let b = buffer(10, &["a", "b"]);
        let mut s = LogSearch::default();
        s.set_query("", &b);
        assert!(!s.is_active());
        assert_eq!(s.count(), 0);
        assert_eq!(s.position(), None);
        assert_eq!(s.ranges("a").len(), 0);
    }
}
