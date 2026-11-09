//! Following one container's log, across restarts.
//!
//! A follower keeps a log stream open while the container has logs. When
//! the stream ends it opens a new one with `since_time` set to the last
//! line's timestamp and skips the overlap, so nothing is lost or repeated.
//! When the pod watch shows a new container in place of the old one, the
//! follower adds a restart marker at the new container's start time.

use std::sync::Arc;
use std::time::Duration;

use futures::io::AsyncBufReadExt as _;
use futures::{AsyncBufRead, Stream, StreamExt as _};
use jiff::tz::TimeZone;
use jiff::{RoundMode, SignedDuration, Timestamp, TimestampRound, Unit};
use k8s_openapi::api::core::v1::{ContainerState, ContainerStatus, Pod};
use kube::Api;
use kube::api::LogParams;
use model::{LineKind, LogLine, LogOptions, LogSource};
use tokio::sync::{mpsc, watch};

use super::line;

/// First wait before reopening a stream that ended while the container
/// still looked alive. Pod status lags the kubelet by a second or so.
const MIN_BACKOFF: Duration = Duration::from_millis(250);

/// Longest wait between reopen attempts for a container that looks alive.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Raw lines are cut at this many bytes while reading, and the rest of a
/// long line is read and dropped, never kept. One huge line (a minified JSON
/// blob, binary output) would otherwise grow a buffer without bound. Four
/// times the pane's own cut, so cleaning out colour codes leaves enough.
const MAX_RAW_LINE_BYTES: usize = 64 * 1024;

/// What the pod watch last said about one container.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerSeen {
    /// The current container's ID. Kubernetes keeps the last ID while a
    /// crashed container waits to restart, so a new ID means a new container.
    pub id: Option<String>,
    /// Restarts so far.
    pub restarts: u32,
    /// Whether the current container is running, waiting or terminated.
    pub phase: Phase,
    /// When the current container started.
    pub started_at: Option<Timestamp>,
    /// Why the container is waiting, like `ContainerCreating`.
    pub waiting_reason: Option<String>,
    /// Whether a previous, terminated container exists. The kubelet serves
    /// its log while the current one waits.
    pub has_previous: bool,
}

/// A container's state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// No status yet.
    #[default]
    Unknown,
    /// Waiting to start or restart.
    Waiting,
    /// Running.
    Running,
    /// Exited.
    Terminated,
}

impl ContainerSeen {
    /// Reads a container status from the pod.
    pub fn from_status(status: &ContainerStatus) -> Self {
        let state = status.state.as_ref();
        let phase = match state {
            Some(ContainerState {
                running: Some(_), ..
            }) => Phase::Running,
            Some(ContainerState {
                terminated: Some(_),
                ..
            }) => Phase::Terminated,
            Some(ContainerState {
                waiting: Some(_), ..
            }) => Phase::Waiting,
            _ => Phase::Unknown,
        };
        let started_at = state.and_then(|s| {
            s.running
                .as_ref()
                .and_then(|r| r.started_at.as_ref())
                .or_else(|| s.terminated.as_ref().and_then(|t| t.started_at.as_ref()))
                .map(|t| t.0)
        });
        let waiting_reason = state
            .and_then(|s| s.waiting.as_ref())
            .and_then(|w| w.reason.clone());
        let has_previous = status
            .last_state
            .as_ref()
            .and_then(|s| s.terminated.as_ref())
            .is_some_and(|t| t.container_id.as_deref().is_some_and(|id| !id.is_empty()));
        Self {
            id: status.container_id.clone().filter(|id| !id.is_empty()),
            restarts: u32::try_from(status.restart_count).unwrap_or(0),
            phase,
            started_at,
            waiting_reason,
            has_previous,
        }
    }

    /// Whether the kubelet can serve a log for this container now.
    fn loggable(&self) -> bool {
        let current = matches!(self.phase, Phase::Running | Phase::Terminated) && self.id.is_some();
        current || self.has_previous
    }
}

/// Remembers the last line read, so a reopened stream neither drops nor
/// repeats lines.
///
/// The log API rounds `sinceTime` down to the second, so a reopened stream
/// replays part of a second. Lines older than the last one are skipped, and
/// so are as many lines with exactly its timestamp as were already read.
#[derive(Clone, Debug, Default)]
pub struct Resume {
    last: Option<Timestamp>,
    /// Lines read with exactly the `last` timestamp.
    at_last: usize,
    /// Set by [`Resume::reopen`] until the first new line.
    skipping: bool,
    skip_at_last: usize,
}

impl Resume {
    /// The `since_time` for a reopened stream: the last line's second.
    pub fn since(&self) -> Option<Timestamp> {
        let last = self.last?;
        let round = TimestampRound::new()
            .smallest(Unit::Second)
            .mode(RoundMode::Floor);
        Some(last.round(round).unwrap_or(last))
    }

    /// Call when a new stream opens.
    pub fn reopen(&mut self) {
        self.skipping = self.last.is_some();
        self.skip_at_last = self.at_last;
    }

    /// Whether a line with this timestamp is new. Records it if so.
    pub fn admit(&mut self, time: Timestamp) -> bool {
        if self.skipping
            && let Some(last) = self.last
        {
            if time < last {
                return false;
            }
            if time == last && self.skip_at_last > 0 {
                self.skip_at_last -= 1;
                return false;
            }
            self.skipping = false;
        }
        match self.last {
            Some(last) if time == last => self.at_last += 1,
            Some(last) if time < last => {}
            _ => {
                self.last = Some(time);
                self.at_last = 1;
            }
        }
        true
    }

    /// The last line's timestamp.
    pub fn last(&self) -> Option<Timestamp> {
        self.last
    }
}

/// Notices when a container was replaced, so the log can mark the gap.
#[derive(Clone, Debug, Default)]
pub struct Restarts {
    id: Option<String>,
}

/// A restart to mark in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Restart {
    /// The restart count after it.
    pub restarts: u32,
    /// When the new container started, if known.
    pub at: Option<Timestamp>,
}

impl Restarts {
    /// Returns a restart if `seen` shows a different container from the
    /// one seen before. The first container seen is not a restart.
    pub fn observe(&mut self, seen: &ContainerSeen) -> Option<Restart> {
        let id = seen.id.as_ref()?;
        let changed = self.id.as_ref().is_some_and(|old| old != id);
        if self.id.as_ref() != Some(id) {
            self.id = Some(id.clone());
        }
        changed.then_some(Restart {
            restarts: seen.restarts,
            at: seen.started_at,
        })
    }
}

/// The log request parameters for one stream.
pub fn params(
    container: &str,
    options: &LogOptions,
    resume: Option<Timestamp>,
    first: bool,
) -> LogParams {
    let mut params = LogParams {
        container: Some(container.to_owned()),
        follow: options.follow && !options.previous,
        previous: options.previous,
        // Always on: merging and reconnecting both need each line's time.
        timestamps: true,
        ..LogParams::default()
    };
    if first {
        params.tail_lines = options.tail_lines.map(i64::from);
        if params.tail_lines.is_none() {
            params.since_time = options.since_time;
        }
    } else {
        params.since_time = resume;
    }
    params
}

/// Everything a follower needs.
pub struct Follower {
    /// Pods in the target's namespace.
    pub api: Api<Pod>,
    /// The pod and container.
    pub source: Arc<LogSource>,
    /// How to read.
    pub options: LogOptions,
    /// The container's status from the pod watch. Closed when the pod goes.
    pub seen: watch::Receiver<ContainerSeen>,
    /// Where lines go.
    pub out: mpsc::Sender<LogLine>,
    /// For display clocks.
    pub tz: TimeZone,
}

impl Follower {
    /// Follows the container until the pod goes away, the tab closes, or a
    /// one-shot read ends.
    pub async fn run(mut self) {
        let mut resume = Resume::default();
        let mut restarts = Restarts::default();
        let mut backoff = MIN_BACKOFF;
        let mut first = true;
        let mut last_notice: Option<String> = None;
        let mut pod_gone = false;

        loop {
            // Wait until the kubelet has a log to serve.
            loop {
                let seen = self.seen.borrow_and_update().clone();
                if let Some(restart) = restarts.observe(&seen)
                    && !self.restarted(restart, resume.last()).await
                {
                    return;
                }
                if seen.loggable() {
                    break;
                }
                let reason = seen.waiting_reason.as_deref().unwrap_or("not started");
                let notice = format!(
                    "container \"{}\" is waiting to start: {reason}",
                    self.source.container
                );
                if !self.notice(&mut last_notice, notice).await || self.one_shot() {
                    return;
                }
                if self.seen.changed().await.is_err() {
                    return;
                }
            }

            let params = params(&self.source.container, &self.options, resume.since(), first);
            first = false;
            resume.reopen();
            // A container that is not running serves its last run's log, so
            // every line this stream reads predates a restart seen meanwhile.
            // A running one may already be the new container.
            let serves_old = self.seen.borrow().phase != Phase::Running;
            let last_before_open = resume.last();
            match self.api.log_stream(&self.source.pod, &params).await {
                Ok(reader) => {
                    last_notice = None;
                    let mut lines = std::pin::pin!(raw_lines(reader, MAX_RAW_LINE_BYTES));
                    loop {
                        // Both branches are cancel safe: the line stream owns
                        // its buffer, and `changed` loses nothing.
                        tokio::select! {
                            next = lines.next() => match next {
                                Some(Ok(raw)) => {
                                    let Some(line) = self.parse(&raw, &mut resume) else {
                                        continue;
                                    };
                                    backoff = MIN_BACKOFF;
                                    if self.out.send(line).await.is_err() {
                                        return;
                                    }
                                }
                                Some(Err(err)) => {
                                    tracing::debug!(pod = %self.source.pod, container = %self.source.container, "log stream broke: {err}");
                                    break;
                                }
                                None => break,
                            },
                            changed = self.seen.changed(), if !pod_gone => {
                                if changed.is_err() {
                                    // Keep reading: the last lines may still be on the way.
                                    pod_gone = true;
                                    continue;
                                }
                                let seen = self.seen.borrow_and_update().clone();
                                let floor = if serves_old { resume.last() } else { last_before_open };
                                if let Some(restart) = restarts.observe(&seen)
                                    && !self.restarted(restart, floor).await
                                {
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    let message = match &err {
                        kube::Error::Api(status) => status.message.clone(),
                        other => other.to_string(),
                    };
                    tracing::debug!(pod = %self.source.pod, container = %self.source.container, "log stream failed: {message}");
                    if !self.notice(&mut last_notice, message).await {
                        return;
                    }
                }
            }

            if self.one_shot() || pod_gone {
                return;
            }
            // A running container means the stream broke or the status is
            // behind: retry soon. Otherwise wait for the status to change.
            if self.seen.borrow().phase == Phase::Running {
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    changed = self.seen.changed() => if changed.is_err() { return },
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            } else if self.seen.changed().await.is_err() {
                return;
            }
        }
    }

    /// A single read with no reconnects.
    fn one_shot(&self) -> bool {
        !self.options.follow || self.options.previous
    }

    fn parse(&self, raw: &RawLine, resume: &mut Resume) -> Option<LogLine> {
        let mut text = line::clean(&raw.bytes);
        if raw.cut {
            line::mark_cut(&mut text);
        }
        let (time, text) = line::split_timestamp(&text);
        // Every line carries a timestamp because we ask for them. If one
        // does not, place it after the last line instead of dropping it.
        let time = time
            .or_else(|| resume.last())
            .unwrap_or_else(Timestamp::now);
        if !resume.admit(time) {
            return None;
        }
        Some(LogLine {
            time,
            clock: line::clock(time, &self.tz),
            source: Arc::clone(&self.source),
            kind: LineKind::Text(line::detect_level(text)),
            text: text.into(),
        })
    }

    /// Sends a restart marker. Returns false if the tab is gone.
    async fn restarted(&self, restart: Restart, floor: Option<Timestamp>) -> bool {
        let time = restart_time(restart, floor);
        let line = LogLine {
            time,
            clock: line::clock(time, &self.tz),
            source: Arc::clone(&self.source),
            kind: LineKind::Restarted {
                restarts: restart.restarts,
            },
            text: format!(
                "container restarted ({}), stream picked up again",
                restart.restarts
            )
            .into(),
        };
        self.out.send(line).await.is_ok()
    }

    /// Sends a notice unless it repeats the last one. Returns false if the
    /// tab is gone.
    async fn notice(&self, last: &mut Option<String>, text: String) -> bool {
        if last.as_deref() == Some(text.as_str()) {
            return true;
        }
        let time = Timestamp::now();
        let line = LogLine {
            time,
            clock: line::clock(time, &self.tz),
            source: Arc::clone(&self.source),
            kind: LineKind::Notice,
            text: text.as_str().into(),
        };
        *last = Some(text);
        self.out.send(line).await.is_ok()
    }
}

/// Where a restart marker sits in time: when the new container started,
/// which is before the new container's first line (`startedAt` has whole
/// seconds, so it never lands after it).
///
/// `floor` is the old container's last line, when that is certain. The
/// marker then also goes after it, which matters when a container restarts
/// within the second of its last line. While a stream to a running
/// container is open, followers pass the last line from before it opened,
/// since the stream may already be reading the new container.
pub fn restart_time(restart: Restart, floor: Option<Timestamp>) -> Timestamp {
    let after = floor.map(|f| f.checked_add(SignedDuration::from_nanos(1)).unwrap_or(f));
    match (restart.at, after) {
        (Some(at), Some(after)) => at.max(after),
        (Some(at), None) => at,
        (None, Some(after)) => after,
        (None, None) => Timestamp::now(),
    }
}

/// One line as read, before cleaning.
#[derive(Debug, PartialEq, Eq)]
struct RawLine {
    /// The bytes, with the line ending when the line fit.
    bytes: Vec<u8>,
    /// The line was longer than the cap and its end was dropped.
    cut: bool,
}

/// Splits a byte stream into lines without failing on bad UTF-8. Lines
/// longer than `cap` bytes are cut (see [`MAX_RAW_LINE_BYTES`]).
///
/// The stream owns the reader and the buffer, so dropping a pending `next()`
/// loses nothing. An I/O error ends the stream after it is reported.
fn raw_lines(
    reader: impl AsyncBufRead + Unpin,
    cap: usize,
) -> impl Stream<Item = std::io::Result<RawLine>> {
    futures::stream::unfold(Some(reader), move |reader| async move {
        let mut reader = reader?;
        match read_line(&mut reader, cap).await {
            Ok(None) => None,
            Ok(Some(line)) => Some((Ok(line), Some(reader))),
            Err(err) => Some((Err(err), None)),
        }
    })
}

/// Reads one line, keeping at most `cap` bytes of it. `None` at the end of
/// the stream.
async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    cap: usize,
) -> std::io::Result<Option<RawLine>> {
    let mut line = RawLine {
        bytes: Vec::new(),
        cut: false,
    };
    let mut read_any = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // A last line without a line ending still counts.
            return Ok(read_any.then(|| finish(line)));
        }
        read_any = true;
        let newline = available.iter().position(|&b| b == b'\n');
        // Bytes of this chunk that belong to the line, line ending included.
        let used = newline.map_or(available.len(), |i| i + 1);
        if !line.cut {
            // The line ending does not count toward the cap.
            let text = newline.unwrap_or(used);
            let room = cap.saturating_sub(line.bytes.len());
            if text > room {
                line.bytes.extend_from_slice(&available[..room]);
                line.cut = true;
            } else {
                line.bytes.extend_from_slice(&available[..used]);
            }
        }
        reader.consume_unpin(used);
        if newline.is_some() {
            return Ok(Some(finish(line)));
        }
    }
}

/// Drops a character the cut split in two, so a cut line does not end in a
/// replacement character.
fn finish(mut line: RawLine) -> RawLine {
    if line.cut
        && let Err(err) = std::str::from_utf8(&line.bytes)
        && err.error_len().is_none()
    {
        line.bytes.truncate(err.valid_up_to());
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn running(id: &str, restarts: u32, started: &str) -> ContainerSeen {
        ContainerSeen {
            id: Some(id.to_owned()),
            restarts,
            phase: Phase::Running,
            started_at: Some(ts(started)),
            ..ContainerSeen::default()
        }
    }

    #[test]
    fn reopen_skips_the_replayed_part_of_a_second() {
        let mut resume = Resume::default();
        for t in ["2026-11-09T11:42:07.100Z", "2026-11-09T11:42:07.500Z"] {
            assert!(resume.admit(ts(t)));
        }
        assert_eq!(resume.since(), Some(ts("2026-11-09T11:42:07Z")));

        resume.reopen();
        // The server replays from 11:42:07.000.
        assert!(!resume.admit(ts("2026-11-09T11:42:07.100Z")));
        assert!(!resume.admit(ts("2026-11-09T11:42:07.500Z")));
        assert!(resume.admit(ts("2026-11-09T11:42:07.900Z")));
        assert!(resume.admit(ts("2026-11-09T11:42:08.000Z")));
    }

    #[test]
    fn reopen_keeps_new_lines_with_the_same_timestamp() {
        let mut resume = Resume::default();
        let t = ts("2026-11-09T11:42:07.5Z");
        assert!(resume.admit(t));
        assert!(resume.admit(t));
        resume.reopen();
        assert!(!resume.admit(t), "first repeat");
        assert!(!resume.admit(t), "second repeat");
        assert!(resume.admit(t), "a third line at the same instant is new");
    }

    #[test]
    fn a_first_stream_keeps_every_line_even_out_of_order() {
        let mut resume = Resume::default();
        assert!(resume.admit(ts("2026-11-09T11:42:08Z")));
        assert!(resume.admit(ts("2026-11-09T11:42:07Z")));
        assert_eq!(resume.last(), Some(ts("2026-11-09T11:42:08Z")));
        assert_eq!(resume.since(), Some(ts("2026-11-09T11:42:08Z")));
    }

    #[test]
    fn nothing_read_means_no_since_time() {
        let mut resume = Resume::default();
        assert_eq!(resume.since(), None);
        resume.reopen();
        assert!(resume.admit(ts("2026-11-09T11:42:07Z")));
    }

    #[test]
    fn a_new_container_id_is_a_restart() {
        let mut restarts = Restarts::default();
        let first = running("containerd://a", 3, "2026-11-09T11:40:00Z");
        assert_eq!(
            restarts.observe(&first),
            None,
            "the first container is not a restart"
        );
        assert_eq!(restarts.observe(&first), None, "the same container again");

        // Crashed: waiting, but Kubernetes keeps the old ID.
        let crashed = ContainerSeen {
            phase: Phase::Waiting,
            waiting_reason: Some("CrashLoopBackOff".to_owned()),
            has_previous: true,
            started_at: None,
            ..first.clone()
        };
        assert_eq!(restarts.observe(&crashed), None);

        let back = running("containerd://b", 4, "2026-11-09T11:42:13Z");
        assert_eq!(
            restarts.observe(&back),
            Some(Restart {
                restarts: 4,
                at: Some(ts("2026-11-09T11:42:13Z"))
            })
        );
        assert_eq!(restarts.observe(&back), None, "marked once");
    }

    #[test]
    fn a_container_without_an_id_is_not_a_restart() {
        let mut restarts = Restarts::default();
        let creating = ContainerSeen {
            phase: Phase::Waiting,
            waiting_reason: Some("ContainerCreating".to_owned()),
            ..ContainerSeen::default()
        };
        assert_eq!(restarts.observe(&creating), None);
        let up = running("containerd://a", 0, "2026-11-09T11:40:00Z");
        assert_eq!(restarts.observe(&up), None);
    }

    #[test]
    fn restart_markers_sit_between_old_and_new_lines() {
        let old_last = ts("2026-11-09T11:42:07.902Z");
        let restart = Restart {
            restarts: 14,
            at: Some(ts("2026-11-09T11:42:13Z")),
        };
        assert_eq!(
            restart_time(restart, Some(old_last)),
            ts("2026-11-09T11:42:13Z")
        );
        assert_eq!(restart_time(restart, None), ts("2026-11-09T11:42:13Z"));

        // Restarted within the second of the last line: startedAt is
        // rounded down to 11:42:07, before that line.
        let quick = Restart {
            restarts: 1,
            at: Some(ts("2026-11-09T11:42:07Z")),
        };
        let marked = restart_time(quick, Some(old_last));
        assert!(marked > old_last);
        assert!(marked < ts("2026-11-09T11:42:07.903Z"));

        let unknown_start = Restart {
            restarts: 1,
            at: None,
        };
        assert!(restart_time(unknown_start, Some(old_last)) > old_last);
    }

    #[test]
    fn loggable_states() {
        let mut seen = ContainerSeen::default();
        assert!(!seen.loggable(), "no status yet");
        seen.phase = Phase::Waiting;
        assert!(!seen.loggable(), "creating, never ran");
        seen.has_previous = true;
        assert!(seen.loggable(), "crash loop: the last run's log is served");
        let seen = running("containerd://a", 0, "2026-11-09T11:40:00Z");
        assert!(seen.loggable());
    }

    #[test]
    fn reads_container_statuses() {
        let status = ContainerStatus {
            name: "app".to_owned(),
            container_id: Some("containerd://abc".to_owned()),
            restart_count: 14,
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("CrashLoopBackOff".to_owned()),
                    ..ContainerStateWaiting::default()
                }),
                ..ContainerState::default()
            }),
            last_state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    container_id: Some("containerd://abc".to_owned()),
                    exit_code: 1,
                    ..ContainerStateTerminated::default()
                }),
                ..ContainerState::default()
            }),
            ..ContainerStatus::default()
        };
        let seen = ContainerSeen::from_status(&status);
        assert_eq!(seen.phase, Phase::Waiting);
        assert_eq!(seen.restarts, 14);
        assert_eq!(seen.id.as_deref(), Some("containerd://abc"));
        assert_eq!(seen.waiting_reason.as_deref(), Some("CrashLoopBackOff"));
        assert!(seen.has_previous);

        let status = ContainerStatus {
            name: "app".to_owned(),
            container_id: Some("containerd://def".to_owned()),
            state: Some(ContainerState {
                running: Some(ContainerStateRunning {
                    started_at: Some(Time(ts("2026-11-09T11:42:13Z"))),
                }),
                ..ContainerState::default()
            }),
            ..ContainerStatus::default()
        };
        let seen = ContainerSeen::from_status(&status);
        assert_eq!(seen.phase, Phase::Running);
        assert_eq!(seen.started_at, Some(ts("2026-11-09T11:42:13Z")));
        assert!(!seen.has_previous);
    }

    #[test]
    fn first_request_uses_tail_or_since_and_later_ones_resume() {
        let options = LogOptions::default();
        let p = params("app", &options, None, true);
        assert!(p.follow && p.timestamps && !p.previous);
        assert_eq!(p.tail_lines, Some(1000));
        assert_eq!(p.container.as_deref(), Some("app"));

        let since = ts("2026-11-09T11:00:00Z");
        let options = LogOptions {
            tail_lines: None,
            since_time: Some(since),
            ..LogOptions::default()
        };
        let p = params("app", &options, None, true);
        assert_eq!(p.since_time, Some(since));
        assert_eq!(p.tail_lines, None);

        let resume = ts("2026-11-09T11:42:07Z");
        let p = params("app", &LogOptions::default(), Some(resume), false);
        assert_eq!(p.since_time, Some(resume));
        assert_eq!(p.tail_lines, None, "a reopened stream must not tail");

        let previous = LogOptions {
            previous: true,
            ..LogOptions::default()
        };
        let p = params("app", &previous, None, true);
        assert!(
            p.previous && !p.follow,
            "the previous container cannot be followed"
        );
    }

    /// Reads `data` in chunks of `chunk` bytes, cutting lines at `cap`.
    async fn read_all(data: &[u8], chunk: usize, cap: usize) -> Vec<RawLine> {
        let reader = futures::io::BufReader::with_capacity(chunk, futures::io::Cursor::new(data));
        raw_lines(reader, cap).map(Result::unwrap).collect().await
    }

    fn raw(bytes: &[u8], cut: bool) -> RawLine {
        RawLine {
            bytes: bytes.to_vec(),
            cut,
        }
    }

    #[tokio::test]
    async fn raw_lines_split_on_newlines_and_keep_a_last_partial_line() {
        let data: &[u8] = b"one\ntwo\r\n\xffthree";
        let lines: Vec<Vec<u8>> = raw_lines(futures::io::Cursor::new(data), MAX_RAW_LINE_BYTES)
            .map(|line| line.unwrap().bytes)
            .collect()
            .await;
        assert_eq!(
            lines,
            [
                b"one\n".to_vec(),
                b"two\r\n".to_vec(),
                b"\xffthree".to_vec()
            ]
        );
    }

    #[tokio::test]
    async fn a_long_line_is_cut_and_the_rest_skipped() {
        // Three-byte reads make the long line span many chunks.
        let lines = read_all(b"abcdefghij\nok\n", 3, 4).await;
        assert_eq!(lines, [raw(b"abcd", true), raw(b"ok\n", false)]);
    }

    #[tokio::test]
    async fn a_line_that_just_fits_is_kept_whole() {
        let lines = read_all(b"abcd\r\nabcd\n", 2, 5).await;
        assert_eq!(lines, [raw(b"abcd\r\n", false), raw(b"abcd\n", false)]);
    }

    #[tokio::test]
    async fn a_last_line_without_an_ending_is_cut_too() {
        let lines = read_all(b"abcdefgh", 8, 4).await;
        assert_eq!(lines, [raw(b"abcd", true)]);
    }

    #[tokio::test]
    async fn a_cut_never_splits_a_character() {
        // 'é' is two bytes; a cap of 2 falls between them.
        let lines = read_all("aéb\nok\n".as_bytes(), 16, 2).await;
        assert_eq!(lines, [raw(b"a", true), raw(b"ok\n", false)]);
    }

    #[tokio::test]
    async fn a_huge_line_keeps_only_the_cap() {
        let mut data = vec![b'x'; 10 * MAX_RAW_LINE_BYTES];
        data.extend_from_slice(b"\nafter\n");
        let lines = read_all(&data, 8 * 1024, MAX_RAW_LINE_BYTES).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].bytes.len(), MAX_RAW_LINE_BYTES);
        assert!(lines[0].cut);
        assert_eq!(lines[1], raw(b"after\n", false));
    }
}
