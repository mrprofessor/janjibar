//! Turning raw log API lines into text: timestamps, colour codes, levels.

use std::borrow::Cow;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use model::{Clock, LogLevel};

/// Lines longer than this are cut. Laying out a megabyte of text in one row
/// would stall the UI, and nobody reads that far right.
const MAX_LINE_BYTES: usize = 16 * 1024;

/// How far into a line to look for a level. Levels sit near the start.
const LEVEL_SCAN_BYTES: usize = 256;

/// Tabs become this many spaces, so columns line up in the pane.
const TAB_WIDTH: usize = 4;

/// Splits the timestamp that `timestamps=true` puts in front of each line.
///
/// Returns `None` for the time when the line has none or it does not parse.
pub fn split_timestamp(raw: &str) -> (Option<Timestamp>, &str) {
    let Some((head, rest)) = raw.split_once(' ') else {
        return (raw.parse().ok(), "");
    };
    match head.parse::<Timestamp>() {
        Ok(time) => (Some(time), rest),
        Err(_) => (None, raw),
    }
}

/// Makes raw bytes printable: lossy UTF-8, no line ending, no ANSI colour
/// codes or other control characters, tabs expanded, long lines cut.
pub fn clean(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_end_matches(['\n', '\r']);
    let mut out = String::with_capacity(text.len().min(MAX_LINE_BYTES + 4));
    let mut chars = text.chars().peekable();
    let mut column = 0usize;
    while let Some(c) = chars.next() {
        if out.len() >= MAX_LINE_BYTES {
            out.push('…');
            break;
        }
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            '\t' => {
                let pad = TAB_WIDTH - column % TAB_WIDTH;
                out.extend(std::iter::repeat_n(' ', pad));
                column += pad;
            }
            c if c.is_control() => {}
            c => {
                out.push(c);
                column += 1;
            }
        }
    }
    out
}

/// Skips the rest of an escape sequence after ESC: CSI (`ESC [ ... final`),
/// OSC (`ESC ] ... BEL` or `ESC ] ... ESC \`), or a two-character escape.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => {
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
        Some(']') => {
            while let Some(c) = chars.next() {
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                    chars.next();
                    break;
                }
            }
        }
        _ => {}
    }
}

/// Local wall-clock time for display.
pub fn clock(time: Timestamp, tz: &TimeZone) -> Clock {
    let dt = tz.to_datetime(time);
    Clock {
        hour: u8::try_from(dt.hour()).unwrap_or(0),
        minute: u8::try_from(dt.minute()).unwrap_or(0),
        second: u8::try_from(dt.second()).unwrap_or(0),
        millis: u16::try_from(dt.millisecond()).unwrap_or(0),
    }
}

/// Finds the log level of a line, if it shows one.
///
/// Looks, in order, for a klog header (`E1109 11:42:07.902`), a level key
/// (`level=warn`, `"level":"error"`, `severity=INFO`), then a level word:
/// any case among the first three fields (`info: ready`, zap's tab-separated
/// `2026-11-09T11:42:07Z  info  ...`), or upper case anywhere near the start
/// (`[main] ERROR db: ...`). Plain words deeper in a sentence do not count.
pub fn detect_level(text: &str) -> Option<LogLevel> {
    let head = prefix(text, LEVEL_SCAN_BYTES);
    klog(head)
        .or_else(|| keyed(head))
        .or_else(|| level_word(head))
}

fn prefix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// klog lines start with a severity letter, `MMDD`, a space and `hh:mm:ss`.
fn klog(head: &str) -> Option<LogLevel> {
    let b = head.as_bytes();
    let shape = b.len() >= 14
        && b[1..5].iter().all(u8::is_ascii_digit)
        && b[5] == b' '
        && b[8] == b':'
        && b[11] == b':';
    if !shape {
        return None;
    }
    match b[0] {
        b'I' => Some(LogLevel::Info),
        b'W' => Some(LogLevel::Warn),
        b'E' | b'F' => Some(LogLevel::Error),
        _ => None,
    }
}

/// A `level`, `lvl` or `severity` key followed by `=` or `:` and a value.
fn keyed(head: &str) -> Option<LogLevel> {
    let lower: Cow<'_, str> = if head.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(head.to_ascii_lowercase())
    } else {
        Cow::Borrowed(head)
    };
    for key in ["level", "lvl", "severity"] {
        let mut from = 0;
        while let Some(found) = lower[from..].find(key) {
            let start = from + found;
            let end = start + key.len();
            from = end;
            let boundary = lower[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
            if !boundary {
                continue;
            }
            let rest = lower[end..].trim_start_matches('"');
            let Some(rest) = rest.strip_prefix(['=', ':']) else {
                continue;
            };
            let value: String = rest
                .trim_start_matches([' ', '"', '\''])
                .chars()
                .take_while(char::is_ascii_alphabetic)
                .collect();
            if let Some(level) = parse_level(&value) {
                return Some(level);
            }
        }
    }
    None
}

fn level_word(head: &str) -> Option<LogLevel> {
    for (field_ix, field) in head.split_whitespace().enumerate() {
        for word in field.split(|c: char| !c.is_ascii_alphanumeric()) {
            let Some(level) = parse_level(word) else {
                continue;
            };
            let upper = word.bytes().all(|b| b.is_ascii_uppercase());
            if upper || field_ix < 3 {
                return Some(level);
            }
        }
    }
    None
}

fn parse_level(word: &str) -> Option<LogLevel> {
    if word.is_empty() || word.len() > 11 {
        return None;
    }
    match word.to_ascii_lowercase().as_str() {
        "trace" | "trc" => Some(LogLevel::Trace),
        "debug" | "dbg" => Some(LogLevel::Debug),
        "info" | "inf" | "information" | "notice" => Some(LogLevel::Info),
        "warn" | "wrn" | "warning" => Some(LogLevel::Warn),
        "error" | "err" | "eror" | "fatal" | "ftl" | "panic" | "critical" | "crit" | "alert"
        | "emerg" | "emergency" => Some(LogLevel::Error),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_the_api_timestamp() {
        let (time, text) = split_timestamp("2026-11-09T11:42:07.112345678Z GET /healthz 200");
        assert_eq!(
            time.unwrap(),
            "2026-11-09T11:42:07.112345678Z"
                .parse::<Timestamp>()
                .unwrap()
        );
        assert_eq!(text, "GET /healthz 200");
    }

    #[test]
    fn a_line_without_a_timestamp_is_kept_whole() {
        assert_eq!(split_timestamp("hello world"), (None, "hello world"));
        let (time, text) = split_timestamp("2026-11-09T11:42:07Z");
        assert!(time.is_some());
        assert_eq!(text, "");
    }

    #[test]
    fn clean_strips_colour_codes_and_line_endings() {
        assert_eq!(
            clean(b"\x1b[32mINFO\x1b[0m ready\r\n"),
            "INFO ready",
            "CSI colour codes go"
        );
        assert_eq!(clean(b"\x1b]0;title\x07after"), "after", "OSC with BEL");
        assert_eq!(clean(b"\x1b]8;;http://x\x1b\\link"), "link", "OSC with ST");
        assert_eq!(clean(b"a\x00b\x08c"), "abc", "other controls go");
    }

    #[test]
    fn clean_expands_tabs_to_columns() {
        assert_eq!(clean(b"ab\tc"), "ab  c");
        assert_eq!(clean(b"\tx"), "    x");
    }

    #[test]
    fn clean_survives_bad_utf8_and_cuts_long_lines() {
        assert_eq!(clean(b"ok \xff here"), "ok \u{fffd} here");
        let long = vec![b'x'; MAX_LINE_BYTES * 2];
        let cut = clean(&long);
        assert!(cut.ends_with('…'));
        assert!(cut.len() <= MAX_LINE_BYTES + '…'.len_utf8());
    }

    #[test]
    fn clock_uses_the_given_zone() {
        let time: Timestamp = "2026-11-09T11:42:07.112Z".parse().unwrap();
        let tz = TimeZone::fixed(jiff::tz::offset(13));
        assert_eq!(clock(time, &tz).to_string(), "00:42:07.112");
        assert_eq!(clock(time, &TimeZone::UTC).to_string(), "11:42:07.112");
    }

    #[test]
    fn levels_from_common_formats() {
        let cases = [
            (
                "I1109 11:42:07.902    1 main.go:42] started",
                Some(LogLevel::Info),
            ),
            (
                "W1109 11:42:07.902    1 main.go:42] slow",
                Some(LogLevel::Warn),
            ),
            (
                "E1109 11:42:07.902    1 main.go:42] boom",
                Some(LogLevel::Error),
            ),
            (
                r#"{"level":"warn","msg":"heap high"}"#,
                Some(LogLevel::Warn),
            ),
            (
                r#"{"msg":"error handling","severity":"INFO"}"#,
                Some(LogLevel::Info),
            ),
            (
                "time=12:00 level=error msg=\"db down\"",
                Some(LogLevel::Error),
            ),
            ("lvl=dbg msg=tick", Some(LogLevel::Debug)),
            (
                "2026-11-09T11:42:07.123Z\tinfo\tserver/main.go:12\tlistening",
                Some(LogLevel::Info),
            ),
            (
                "2026-11-09T11:42:07Z  WARN engine: retrying",
                Some(LogLevel::Warn),
            ),
            (
                "[main] some prefix text ERROR db: refused",
                Some(LogLevel::Error),
            ),
            ("INFO:root:started", Some(LogLevel::Info)),
            ("fatal error: runtime: out of memory", Some(LogLevel::Error)),
            ("panic: nil map", Some(LogLevel::Error)),
            ("[debug] cache miss", Some(LogLevel::Debug)),
        ];
        for (text, want) in cases {
            assert_eq!(detect_level(text), want, "{text}");
        }
    }

    #[test]
    fn plain_text_has_no_level() {
        let cases = [
            "GET /v1/cart/8812 200 14ms",
            "starting checkout-api v2.31.0 on :8080",
            "we failed to fetch info from upstream",
            "reticulating errors_total metric",
            "",
        ];
        for text in cases {
            assert_eq!(detect_level(text), None, "{text}");
        }
    }

    #[test]
    fn level_keys_need_a_word_boundary() {
        assert_eq!(keyed("sublevel=error x"), None);
        assert_eq!(keyed("my_level=error x"), None);
        assert_eq!(keyed("x level=error"), Some(LogLevel::Error));
    }

    #[test]
    fn long_multibyte_lines_do_not_split_characters() {
        let text = "é".repeat(LEVEL_SCAN_BYTES);
        assert_eq!(detect_level(&text), None);
    }
}
