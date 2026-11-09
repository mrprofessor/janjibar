//! Log tabs: what they follow, the lines the engine sends, and the buffer
//! a tab keeps.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use jiff::Timestamp;

use crate::ContextName;

/// How many lines a log tab keeps before dropping the oldest.
///
/// 100k lines of about 120 bytes is roughly 20 MB per tab, which keeps a
/// few open tabs well inside the app's memory budget.
pub const DEFAULT_LOG_CAPACITY: usize = 100_000;

/// How many lines each container sends first when a tab opens.
pub const DEFAULT_TAIL_LINES: u32 = 1000;

/// Identifies one open log tab. The UI picks it; the engine echoes it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogTabId(u64);

impl LogTabId {
    /// Wraps a tab number.
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The tab number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// What a log tab follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogTarget {
    /// One pod.
    Pod {
        /// The pod's namespace.
        namespace: Arc<str>,
        /// The pod's name.
        name: Arc<str>,
    },
    /// Every pod in a namespace that matches a label selector, such as the
    /// pods of a Deployment. Pods are picked up and dropped as they come and go.
    Selector {
        /// The namespace to look in.
        namespace: Arc<str>,
        /// A label selector, like `app=checkout,tier=api`.
        selector: Arc<str>,
        /// What the UI calls the workload, like `deploy/checkout-api`.
        title: Arc<str>,
    },
}

impl LogTarget {
    /// The namespace the target lives in.
    pub fn namespace(&self) -> &str {
        match self {
            Self::Pod { namespace, .. } | Self::Selector { namespace, .. } => namespace,
        }
    }

    /// A short name for headers, like `pod/web-0` or `deploy/checkout-api`.
    pub fn title(&self) -> String {
        match self {
            Self::Pod { name, .. } => format!("pod/{name}"),
            Self::Selector { title, .. } => title.to_string(),
        }
    }
}

/// Which containers of each pod a tab follows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Containers {
    /// Every container, init containers first.
    #[default]
    All,
    /// Only the container with this name.
    Only(Arc<str>),
}

/// How a tab reads logs. These map onto the Kubernetes log API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogOptions {
    /// Keep streams open, and pick them up again after a container restarts.
    pub follow: bool,
    /// Read the previous, terminated container instead of the current one.
    /// Implies a one-shot read.
    pub previous: bool,
    /// Start with this many lines per container. `None` reads everything.
    pub tail_lines: Option<u32>,
    /// Start at this time instead. Used when `tail_lines` is `None`.
    pub since_time: Option<Timestamp>,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            follow: true,
            previous: false,
            tail_lines: Some(DEFAULT_TAIL_LINES),
            since_time: None,
        }
    }
}

/// Asks the engine to open a log tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRequest {
    /// The tab, chosen by the UI.
    pub tab: LogTabId,
    /// The context to read from.
    pub context: ContextName,
    /// The pod or pods to follow.
    pub target: LogTarget,
    /// Which containers to follow in each pod.
    pub containers: Containers,
    /// How to read.
    pub options: LogOptions,
}

/// Where a line came from. Shared by every line of one container.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LogSource {
    /// The pod's name.
    pub pod: Arc<str>,
    /// The container's name.
    pub container: Arc<str>,
}

impl LogSource {
    /// The pod's tag in a merged view: the last dash-separated part of its
    /// name, which is the random suffix of pods that a controller made.
    pub fn short_pod(&self) -> &str {
        self.pod.rsplit('-').next().unwrap_or(&self.pod)
    }
}

/// A log level found in a line's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LogLevel {
    /// Trace.
    Trace,
    /// Debug.
    Debug,
    /// Info, notice.
    Info,
    /// Warnings.
    Warn,
    /// Errors, fatal and panic.
    Error,
}

impl LogLevel {
    /// The lowercase name shown in the level column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// What a line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// Output from the container, with its level if one was found.
    Text(Option<LogLevel>),
    /// The container restarted and the stream picked up after it. Lines
    /// before this one came from the old container.
    Restarted {
        /// The container's restart count after the restart.
        restarts: u32,
    },
    /// A message from the app, not the container: a container that is
    /// waiting to start, or a stream that failed.
    Notice,
}

/// A local wall-clock time, already converted for display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Clock {
    /// Hour, 0 to 23.
    pub hour: u8,
    /// Minute, 0 to 59.
    pub minute: u8,
    /// Second, 0 to 59.
    pub second: u8,
    /// Millisecond, 0 to 999.
    pub millis: u16,
}

impl fmt::Display for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02}:{:02}:{:02}.{:03}",
            self.hour, self.minute, self.second, self.millis
        )
    }
}

/// One log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    /// When the container wrote it, from the log API's timestamp.
    pub time: Timestamp,
    /// `time` in local time, for display.
    pub clock: Clock,
    /// The pod and container it came from.
    pub source: Arc<LogSource>,
    /// Container output, a restart marker, or a notice.
    pub kind: LineKind,
    /// The text, without the timestamp, colour codes or line ending.
    pub text: Arc<str>,
}

/// New lines for one tab, sorted by time.
#[derive(Clone, Debug)]
pub struct LogChunk {
    /// The tab.
    pub tab: LogTabId,
    /// The lines, oldest first.
    pub lines: Vec<LogLine>,
}

/// Where a log tab stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogState {
    /// Connecting, or waiting for the first list of pods.
    Starting,
    /// Following these pods.
    Streaming {
        /// Pods that match the target.
        pods: u32,
        /// Containers being followed across those pods.
        containers: u32,
    },
    /// No pod matches the target right now. For a selector, pods that appear
    /// later are picked up.
    NoPods,
    /// A one-shot read finished.
    Done,
    /// The tab could not start: the context failed or the target is invalid.
    Failed(String),
}

/// What a [`LogBuffer::push`] changed, so a view can update search results
/// and scroll state without rescanning everything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogChange {
    /// Lines dropped from the front to stay within capacity.
    pub evicted: usize,
    /// Index of the first line that is new or moved, counted after the
    /// eviction. Lines before it only shifted down by `evicted`.
    pub first_changed: usize,
    /// Lines added.
    pub inserted: usize,
}

/// The lines of one log tab, kept in time order and capped in size.
///
/// Chunks usually belong at the end and are appended. A line from a slower
/// stream can be older than lines already held; it is inserted in its place,
/// after any lines with the same timestamp, so a merged view stays ordered.
/// When the buffer is full the oldest lines go first.
#[derive(Clone, Debug)]
pub struct LogBuffer {
    lines: VecDeque<LogLine>,
    capacity: usize,
    evicted: u64,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_LOG_CAPACITY)
    }
}

impl LogBuffer {
    /// An empty buffer that holds at most `capacity` lines (at least one).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            lines: VecDeque::with_capacity(capacity.min(4096)),
            capacity,
            evicted: 0,
        }
    }

    /// The most lines the buffer holds.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Lines held now.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the buffer holds no lines.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The line at `index`, oldest first.
    pub fn get(&self, index: usize) -> Option<&LogLine> {
        self.lines.get(index)
    }

    /// Every line, oldest first.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &LogLine> + DoubleEndedIterator {
        self.lines.iter()
    }

    /// Lines dropped since the buffer was made. Adding it to an index gives
    /// a position that stays put while old lines are dropped.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Adds lines, keeping time order, then drops the oldest lines beyond
    /// capacity.
    pub fn push(&mut self, lines: impl IntoIterator<Item = LogLine>) -> LogChange {
        let old_len = self.lines.len();
        let mut first_changed = old_len;
        let mut inserted = 0;
        for line in lines {
            let at = match self.lines.back() {
                Some(last) if last.time > line.time => {
                    self.lines.partition_point(|l| l.time <= line.time)
                }
                _ => self.lines.len(),
            };
            first_changed = first_changed.min(at);
            self.lines.insert(at, line);
            inserted += 1;
        }
        let evicted = self.lines.len().saturating_sub(self.capacity);
        self.lines.drain(..evicted);
        self.evicted += evicted as u64;
        LogChange {
            evicted,
            first_changed: first_changed.saturating_sub(evicted),
            inserted,
        }
    }

    /// Drops every line.
    pub fn clear(&mut self) {
        self.evicted += self.lines.len() as u64;
        self.lines.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(pod: &str) -> Arc<LogSource> {
        Arc::new(LogSource {
            pod: pod.into(),
            container: "app".into(),
        })
    }

    fn line(second: i64, pod: &str, text: &str) -> LogLine {
        LogLine {
            time: Timestamp::from_second(1_800_000_000 + second).unwrap(),
            clock: Clock::default(),
            source: source(pod),
            kind: LineKind::Text(None),
            text: text.into(),
        }
    }

    fn texts(buffer: &LogBuffer) -> Vec<&str> {
        buffer.iter().map(|l| &*l.text).collect()
    }

    #[test]
    fn appends_in_order() {
        let mut buffer = LogBuffer::new(10);
        let change = buffer.push([line(1, "a", "one"), line(2, "a", "two")]);
        assert_eq!(texts(&buffer), ["one", "two"]);
        assert_eq!(
            change,
            LogChange {
                evicted: 0,
                first_changed: 0,
                inserted: 2
            }
        );

        let change = buffer.push([line(3, "a", "three")]);
        assert_eq!(change.first_changed, 2, "only the new line changed");
    }

    #[test]
    fn late_lines_from_another_pod_are_merged_by_time() {
        let mut buffer = LogBuffer::new(10);
        buffer.push([line(1, "a", "a1"), line(3, "a", "a3"), line(5, "a", "a5")]);
        // Pod b's stream was slower; its lines are older than a5.
        let change = buffer.push([line(2, "b", "b2"), line(4, "b", "b4")]);
        assert_eq!(texts(&buffer), ["a1", "b2", "a3", "b4", "a5"]);
        assert_eq!(change.first_changed, 1);
    }

    #[test]
    fn equal_times_keep_arrival_order() {
        let mut buffer = LogBuffer::new(10);
        buffer.push([line(1, "a", "first"), line(2, "a", "later")]);
        buffer.push([line(1, "b", "second")]);
        buffer.push([line(1, "c", "third")]);
        assert_eq!(texts(&buffer), ["first", "second", "third", "later"]);
    }

    #[test]
    fn drops_the_oldest_lines_beyond_capacity() {
        let mut buffer = LogBuffer::new(3);
        buffer.push([line(1, "a", "1"), line(2, "a", "2"), line(3, "a", "3")]);
        let change = buffer.push([line(4, "a", "4"), line(5, "a", "5")]);
        assert_eq!(texts(&buffer), ["3", "4", "5"]);
        assert_eq!(buffer.evicted(), 2);
        assert_eq!(
            change,
            LogChange {
                evicted: 2,
                first_changed: 1,
                inserted: 2
            }
        );
    }

    #[test]
    fn a_line_older_than_a_full_buffer_falls_off_at_once() {
        let mut buffer = LogBuffer::new(2);
        buffer.push([line(5, "a", "5"), line(6, "a", "6")]);
        buffer.push([line(1, "b", "ancient")]);
        assert_eq!(texts(&buffer), ["5", "6"]);
        assert_eq!(buffer.evicted(), 1);
    }

    #[test]
    fn capacity_is_at_least_one() {
        let mut buffer = LogBuffer::new(0);
        buffer.push([line(1, "a", "1"), line(2, "a", "2")]);
        assert_eq!(texts(&buffer), ["2"]);
    }

    #[test]
    fn clear_counts_as_eviction() {
        let mut buffer = LogBuffer::new(5);
        buffer.push([line(1, "a", "1"), line(2, "a", "2")]);
        buffer.clear();
        assert!(buffer.is_empty());
        assert_eq!(buffer.evicted(), 2);
    }

    #[test]
    fn short_pod_is_the_last_name_part() {
        let s = LogSource {
            pod: "checkout-api-7d9f8c6b5-x2kqp".into(),
            container: "api".into(),
        };
        assert_eq!(s.short_pod(), "x2kqp");
        let s = LogSource {
            pod: "solo".into(),
            container: "api".into(),
        };
        assert_eq!(s.short_pod(), "solo");
    }

    #[test]
    fn clock_pads_fields() {
        let c = Clock {
            hour: 9,
            minute: 5,
            second: 7,
            millis: 12,
        };
        assert_eq!(c.to_string(), "09:05:07.012");
    }

    #[test]
    fn target_titles() {
        let pod = LogTarget::Pod {
            namespace: "shop".into(),
            name: "web-0".into(),
        };
        assert_eq!(pod.title(), "pod/web-0");
        assert_eq!(pod.namespace(), "shop");
        let workload = LogTarget::Selector {
            namespace: "shop".into(),
            selector: "app=web".into(),
            title: "deploy/web".into(),
        };
        assert_eq!(workload.title(), "deploy/web");
    }
}
