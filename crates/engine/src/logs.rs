//! Log tabs: one pod or every pod behind a selector, merged by time.
//!
//! A tab watches the pods it targets. Each followed container gets a
//! [`follow::Follower`] task that streams its log and picks it up again after
//! restarts. Pods that appear get followers; pods that go away lose them.
//! Lines from every follower meet in the tab task, which sorts each batch by
//! time and sends it to the UI at most once per [`SNAPSHOT_WINDOW`].

mod follow;
mod line;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::StreamExt as _;
use jiff::tz::TimeZone;
use k8s_openapi::api::core::v1::{ContainerStatus, Pod};
use kube::config::Kubeconfig;
use kube::runtime::{WatchStreamExt as _, watcher};
use kube::{Api, Client, ResourceExt as _};
use model::{
    Containers, Event, LogChunk, LogLine, LogOptions, LogRequest, LogSource, LogState, LogTabId,
    LogTarget,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::client::ClientPool;
use crate::errors::connect_error_from_watch;
use crate::hub::SNAPSHOT_WINDOW;
use crate::task::AbortOnDrop;
use follow::{ContainerSeen, Follower};

/// Lines in flight from followers to the tab task. When the tab falls
/// behind, followers wait, and the API server's stream waits with them.
const LINE_QUEUE: usize = 8192;

/// Opens a log tab: connects to the context, then streams until dropped.
pub(crate) fn open(
    request: LogRequest,
    clients: ClientPool,
    kubeconfig: Arc<Kubeconfig>,
    events: mpsc::UnboundedSender<Event>,
) -> AbortOnDrop<()> {
    AbortOnDrop(tokio::spawn(async move {
        let tab = request.tab;
        let send = |state| {
            // A send error means the UI is gone; the engine stops soon.
            let _ = events.send(Event::LogState { tab, state });
        };
        send(LogState::Starting);
        match clients.get(&request.context, &kubeconfig).await {
            Ok(client) => stream_logs(request, client, events.clone()).await,
            Err(err) => {
                tracing::warn!(context = %request.context, "log tab could not connect: {err}");
                send(LogState::Failed(err.to_string()));
            }
        }
    }))
}

/// Streams a log tab's lines and state to `events` until the receiver is
/// gone, the task is dropped, or a one-shot read ends.
pub async fn stream_logs(
    request: LogRequest,
    client: Client,
    events: mpsc::UnboundedSender<Event>,
) {
    let LogRequest {
        tab: id,
        context,
        target,
        containers,
        options,
    } = request;
    let api: Api<Pod> = Api::namespaced(client, target.namespace());
    let config = match &target {
        LogTarget::Pod { name, .. } => {
            watcher::Config::default().fields(&format!("metadata.name={name}"))
        }
        LogTarget::Selector { selector, .. } => watcher::Config::default().labels(selector),
    };
    let pods = watcher(api.clone(), config)
        .default_backoff()
        .modify(|pod: &mut Pod| pod.managed_fields_mut().clear());
    let mut pods = std::pin::pin!(pods);

    let (lines_tx, mut lines_rx) = mpsc::channel(LINE_QUEUE);
    let mut tab = Tab {
        api,
        containers,
        options: options.clone(),
        tz: local_zone().await,
        lines: lines_tx,
        followers: JoinSet::new(),
        pods: HashMap::new(),
        relisting: None,
    };
    let one_shot = !options.follow || options.previous;
    let mut ready = false;
    let mut failure: Option<String> = None;
    let mut sent_state: Option<LogState> = None;
    let mut pending: Vec<LogLine> = Vec::new();
    let mut flush_at: Option<Instant> = None;
    let mut last_flush: Option<Instant> = None;

    loop {
        // Every branch is cancel safe: `StreamExt::next`, `mpsc::Receiver::recv`,
        // `sleep_until` and `JoinSet::join_next`.
        tokio::select! {
            event = pods.next() => match event {
                Some(Ok(event)) => {
                    if matches!(event, watcher::Event::InitDone) {
                        ready = true;
                        failure = None;
                    }
                    tab.on_watch(event);
                }
                Some(Err(err)) => {
                    tracing::warn!(%context, tab = id.get(), "log tab pod watch error: {err}");
                    if !ready {
                        failure = Some(connect_error_from_watch(&err).to_string());
                    }
                }
                None => return,
            },
            Some(line) = lines_rx.recv() => {
                pending.push(line);
                while let Ok(line) = lines_rx.try_recv() {
                    pending.push(line);
                }
                if flush_at.is_none() {
                    let now = Instant::now();
                    flush_at = Some(last_flush.map_or(now, |t| (t + SNAPSHOT_WINDOW).max(now)));
                }
            }
            () = sleep_until(flush_at), if flush_at.is_some() => {
                flush_at = None;
                last_flush = Some(Instant::now());
                if !flush(id, &mut pending, &events) {
                    return;
                }
            }
            Some(_) = tab.followers.join_next(), if !tab.followers.is_empty() => {}
        }

        let finished = one_shot && ready && tab.followers.is_empty();
        if finished {
            // Followers are done, so every line they sent is queued already.
            while let Ok(line) = lines_rx.try_recv() {
                pending.push(line);
            }
            if !flush(id, &mut pending, &events) {
                return;
            }
        }
        let state = match &failure {
            Some(message) => LogState::Failed(message.clone()),
            None if finished => LogState::Done,
            None => tab.state(ready),
        };
        if sent_state.as_ref() != Some(&state) {
            if events
                .send(Event::LogState {
                    tab: id,
                    state: state.clone(),
                })
                .is_err()
            {
                return;
            }
            sent_state = Some(state);
        }
        if finished {
            return;
        }
    }
}

/// Sends pending lines as one chunk, sorted by time. The sort is stable, so
/// lines from one container keep their order. Returns false if the UI is gone.
fn flush(tab: LogTabId, pending: &mut Vec<LogLine>, events: &mpsc::UnboundedSender<Event>) -> bool {
    if pending.is_empty() {
        return true;
    }
    pending.sort_by_key(|line| line.time);
    let lines = std::mem::take(pending);
    events.send(Event::Logs(LogChunk { tab, lines })).is_ok()
}

async fn sleep_until(at: Option<Instant>) {
    if let Some(at) = at {
        tokio::time::sleep_until(at).await;
    }
}

/// The system time zone for display clocks. Reading it touches files.
async fn local_zone() -> TimeZone {
    tokio::task::spawn_blocking(TimeZone::system)
        .await
        .unwrap_or(TimeZone::UTC)
}

struct Tab {
    api: Api<Pod>,
    containers: Containers,
    options: LogOptions,
    tz: TimeZone,
    lines: mpsc::Sender<LogLine>,
    /// Aborted together when the tab is dropped.
    followers: JoinSet<()>,
    /// Followed pods by UID.
    pods: HashMap<String, FollowedPod>,
    /// UIDs listed since the watcher started a re-list.
    relisting: Option<HashSet<String>>,
}

struct FollowedPod {
    /// Each followed container's status. Dropping a sender tells its
    /// follower the pod is gone.
    containers: HashMap<Arc<str>, watch::Sender<ContainerSeen>>,
}

impl Tab {
    fn on_watch(&mut self, event: watcher::Event<Pod>) {
        match event {
            watcher::Event::Init => self.relisting = Some(HashSet::new()),
            watcher::Event::InitApply(pod) => {
                if let (Some(listed), Some(uid)) = (self.relisting.as_mut(), pod.uid()) {
                    listed.insert(uid);
                }
                self.apply(&pod);
            }
            watcher::Event::InitDone => {
                if let Some(listed) = self.relisting.take() {
                    self.pods.retain(|uid, _| listed.contains(uid));
                }
            }
            watcher::Event::Apply(pod) => self.apply(&pod),
            watcher::Event::Delete(pod) => {
                if let Some(uid) = pod.uid() {
                    self.pods.remove(&uid);
                }
            }
        }
    }

    /// Starts followers for new containers and passes status to the others.
    fn apply(&mut self, pod: &Pod) {
        let Some(uid) = pod.uid() else {
            return;
        };
        let name: Arc<str> = pod.name_any().into();
        let wanted = wanted_containers(pod, &self.containers);
        let followed = self.pods.entry(uid).or_insert_with(|| FollowedPod {
            containers: HashMap::new(),
        });
        for (container, seen) in wanted {
            if let Some(tx) = followed.containers.get(&container) {
                tx.send_if_modified(|old| {
                    let changed = *old != seen;
                    if changed {
                        *old = seen;
                    }
                    changed
                });
                continue;
            }
            let (tx, rx) = watch::channel(seen);
            let follower = Follower {
                api: self.api.clone(),
                source: Arc::new(LogSource {
                    pod: Arc::clone(&name),
                    container: Arc::clone(&container),
                }),
                options: self.options.clone(),
                seen: rx,
                out: self.lines.clone(),
                tz: self.tz.clone(),
            };
            self.followers.spawn(follower.run());
            followed.containers.insert(container, tx);
        }
    }

    fn state(&self, ready: bool) -> LogState {
        if !ready {
            return LogState::Starting;
        }
        if self.pods.is_empty() {
            return LogState::NoPods;
        }
        let containers: usize = self.pods.values().map(|p| p.containers.len()).sum();
        LogState::Streaming {
            pods: u32::try_from(self.pods.len()).unwrap_or(u32::MAX),
            containers: u32::try_from(containers).unwrap_or(u32::MAX),
        }
    }
}

/// The containers to follow in a pod, init containers first, with their
/// status. Containers without a status yet get a default one and wait.
fn wanted_containers(pod: &Pod, filter: &Containers) -> Vec<(Arc<str>, ContainerSeen)> {
    let Some(spec) = pod.spec.as_ref() else {
        return Vec::new();
    };
    let status = pod.status.as_ref();
    let init_statuses = status
        .and_then(|s| s.init_container_statuses.as_deref())
        .unwrap_or_default();
    let statuses = status
        .and_then(|s| s.container_statuses.as_deref())
        .unwrap_or_default();
    let find = |list: &[ContainerStatus], name: &str| {
        list.iter()
            .find(|s| s.name == name)
            .map(ContainerSeen::from_status)
            .unwrap_or_default()
    };
    let init = spec
        .init_containers
        .iter()
        .flatten()
        .map(|c| (c.name.as_str(), find(init_statuses, &c.name)));
    let main = spec
        .containers
        .iter()
        .map(|c| (c.name.as_str(), find(statuses, &c.name)));
    init.chain(main)
        .filter(|(name, _)| match filter {
            Containers::All => true,
            Containers::Only(only) => **only == **name,
        })
        .map(|(name, seen)| (Arc::from(name), seen))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        Container, ContainerState, ContainerStateRunning, PodSpec, PodStatus,
    };
    use model::{Clock, LineKind};

    fn container(name: &str) -> Container {
        Container {
            name: name.to_owned(),
            ..Container::default()
        }
    }

    fn pod() -> Pod {
        Pod {
            spec: Some(PodSpec {
                init_containers: Some(vec![container("migrate")]),
                containers: vec![container("app"), container("envoy")],
                ..PodSpec::default()
            }),
            status: Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    name: "app".to_owned(),
                    container_id: Some("containerd://a".to_owned()),
                    state: Some(ContainerState {
                        running: Some(ContainerStateRunning::default()),
                        ..ContainerState::default()
                    }),
                    ..ContainerStatus::default()
                }]),
                ..PodStatus::default()
            }),
            ..Pod::default()
        }
    }

    #[test]
    fn follows_init_containers_first_then_the_rest() {
        let names: Vec<String> = wanted_containers(&pod(), &Containers::All)
            .into_iter()
            .map(|(name, _)| name.to_string())
            .collect();
        assert_eq!(names, ["migrate", "app", "envoy"]);
    }

    #[test]
    fn a_named_container_is_followed_alone_with_its_status() {
        let wanted = wanted_containers(&pod(), &Containers::Only("app".into()));
        assert_eq!(wanted.len(), 1);
        assert_eq!(&*wanted[0].0, "app");
        assert_eq!(wanted[0].1.phase, follow::Phase::Running);
        assert_eq!(wanted[0].1.id.as_deref(), Some("containerd://a"));
    }

    #[test]
    fn containers_without_status_wait() {
        let wanted = wanted_containers(&pod(), &Containers::Only("envoy".into()));
        assert_eq!(wanted[0].1, ContainerSeen::default());
    }

    fn line(second: i64, pod: &str) -> LogLine {
        LogLine {
            time: jiff::Timestamp::from_second(1_800_000_000 + second).unwrap(),
            clock: Clock::default(),
            source: Arc::new(LogSource {
                pod: pod.into(),
                container: "app".into(),
            }),
            kind: LineKind::Text(None),
            text: format!("{pod}{second}").into(),
        }
    }

    #[test]
    fn a_flush_merges_streams_by_time_and_keeps_each_streams_order() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Arrival order: pod a's batch, then pod b's, with a tie at 3.
        let mut pending = vec![
            line(1, "a"),
            line(3, "a"),
            line(5, "a"),
            line(2, "b"),
            line(3, "b"),
        ];
        assert!(flush(LogTabId::new(7), &mut pending, &tx));
        assert_eq!(pending.len(), 0);
        let Some(Event::Logs(chunk)) = rx.try_recv().ok() else {
            panic!("expected a chunk");
        };
        assert_eq!(chunk.tab, LogTabId::new(7));
        let texts: Vec<&str> = chunk.lines.iter().map(|l| &*l.text).collect();
        assert_eq!(texts, ["a1", "b2", "a3", "b3", "a5"]);
    }

    #[test]
    fn an_empty_flush_sends_nothing() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(flush(LogTabId::new(1), &mut Vec::new(), &tx));
        assert!(rx.try_recv().is_err());
    }
}
