//! Polling the metrics API while views need it, and sending snapshots.
//!
//! A [`UsagePoller`] runs one task per context. Views hold
//! [`UsageSubscription`]s; while at least one is held the task polls every
//! scope they need on one shared tick, so a round ends in one snapshot. A
//! scope a view starts needing is polled at once, unless it was polled
//! within the interval. With no subscriptions the task sleeps and keeps the
//! history it has.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::future::join_all;
use model::{
    ContextName, Usage, UsageInterest, UsageObject, UsageScope, UsageSnapshot, UsageStatus,
};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};

use crate::fetch;
use crate::history::History;

/// How the poller behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollConfig {
    /// Time between polls. metrics-server scrapes every 15 seconds by
    /// default, so polling faster only repeats samples.
    pub interval: Duration,
    /// How long one list request may take before the round moves on.
    pub timeout: Duration,
    /// How much history each pod and node keeps.
    pub window: Duration,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(15),
            timeout: Duration::from_secs(10),
            window: Duration::from_hours(1),
        }
    }
}

/// Polls one context's metrics API. Cheap to clone; the task stops when the
/// last clone is dropped.
#[derive(Clone, Debug)]
pub struct UsagePoller {
    inbox: mpsc::UnboundedSender<Message>,
    next_id: Arc<AtomicU64>,
    _task: Arc<AbortOnDrop>,
}

/// Keeps polling for one interest until dropped.
#[derive(Debug)]
pub struct UsageSubscription {
    id: u64,
    inbox: mpsc::UnboundedSender<Message>,
}

impl Drop for UsageSubscription {
    fn drop(&mut self) {
        // A send error means the poller is gone, so there is nothing to stop.
        let _ = self.inbox.send(Message::Unsubscribe(self.id));
    }
}

#[derive(Debug)]
enum Message {
    Subscribe(u64, UsageInterest),
    Unsubscribe(u64),
}

#[derive(Debug)]
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl UsagePoller {
    /// Starts polling `context` through `client`, on the current tokio
    /// runtime. Nothing is requested until the first subscription.
    ///
    /// `send` gets every snapshot and returns false once nobody listens,
    /// which stops the poller.
    ///
    /// # Panics
    ///
    /// Outside a tokio runtime.
    pub fn start(
        client: kube::Client,
        context: ContextName,
        config: PollConfig,
        send: impl FnMut(UsageSnapshot) -> bool + Send + 'static,
    ) -> Self {
        let (inbox, messages) = mpsc::unbounded_channel();
        let state = State::new(context, config.window);
        let task = tokio::spawn(run(client, config, state, messages, send));
        Self {
            inbox,
            next_id: Arc::new(AtomicU64::new(0)),
            _task: Arc::new(AbortOnDrop(task)),
        }
    }

    /// Asks for `interest` until the returned subscription is dropped.
    pub fn subscribe(&self, interest: UsageInterest) -> UsageSubscription {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // A send error means the task ended; the subscription is then inert.
        let _ = self.inbox.send(Message::Subscribe(id, interest));
        UsageSubscription {
            id,
            inbox: self.inbox.clone(),
        }
    }
}

async fn run(
    client: kube::Client,
    config: PollConfig,
    mut state: State,
    mut messages: mpsc::UnboundedReceiver<Message>,
    mut send: impl FnMut(UsageSnapshot) -> bool,
) {
    let mut ticker = tokio::time::interval(config.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // New scopes are polled when they appear; the tick is for repeats.
    ticker.reset();
    loop {
        let active = state.is_active();
        let polled = tokio::select! {
            message = messages.recv() => {
                let Some(message) = message else { return };
                state.handle(message);
                // A view often asks for several things at once; take them all
                // before polling so they share one round.
                while let Ok(message) = messages.try_recv() {
                    state.handle(message);
                }
                if !active && state.is_active() {
                    ticker.reset();
                }
                let due = state.due(Instant::now(), config.interval);
                poll(&client, config.timeout, &mut state, due).await
            }
            _ = ticker.tick(), if active => {
                let scopes = state.scopes().into_iter().collect();
                poll(&client, config.timeout, &mut state, scopes).await
            }
        };
        if polled > 0 {
            tracing::trace!(scopes = polled, "metrics API polled");
        }
        if let Some(snapshot) = state.changed_snapshot()
            && !send(snapshot)
        {
            tracing::debug!("usage snapshots have no listener; poller stopped");
            return;
        }
    }
}

/// Lists every scope at once and records the results. Returns how many
/// scopes were polled.
async fn poll(
    client: &kube::Client,
    timeout: Duration,
    state: &mut State,
    scopes: Vec<UsageScope>,
) -> usize {
    let count = scopes.len();
    let results = join_all(scopes.into_iter().map(|scope| async move {
        let result = match tokio::time::timeout(timeout, fetch::fetch(client, &scope)).await {
            Ok(Ok(items)) => Ok(items),
            Ok(Err(err)) => Err(fetch::status_from(&err)),
            Err(_) => Err(UsageStatus::Failed(format!(
                "no answer within {} s",
                timeout.as_secs()
            ))),
        };
        (scope, result)
    }))
    .await;
    let now = Instant::now();
    for (scope, result) in results {
        state.apply(scope, result, now);
    }
    count
}

/// What the poller knows about one scope.
#[derive(Debug)]
struct ScopeState {
    polled_at: Instant,
    status: UsageStatus,
    /// Items from the last poll that worked.
    items: Vec<(UsageObject, Usage)>,
}

/// Everything but the I/O, so the rules can be tested directly.
#[derive(Debug)]
struct State {
    context: ContextName,
    subscriptions: HashMap<u64, UsageInterest>,
    history: History,
    scopes: HashMap<UsageScope, ScopeState>,
    last_sent: Option<UsageSnapshot>,
}

impl State {
    fn new(context: ContextName, window: Duration) -> Self {
        Self {
            context,
            subscriptions: HashMap::new(),
            history: History::new(window),
            scopes: HashMap::new(),
            last_sent: None,
        }
    }

    fn is_active(&self) -> bool {
        !self.subscriptions.is_empty()
    }

    fn handle(&mut self, message: Message) {
        match message {
            Message::Subscribe(id, interest) => {
                self.subscriptions.insert(id, interest);
            }
            Message::Unsubscribe(id) => {
                self.subscriptions.remove(&id);
            }
        }
    }

    /// Every scope views asked for, directly or through a history.
    fn wanted(&self) -> BTreeSet<UsageScope> {
        self.subscriptions
            .values()
            .map(|interest| match interest {
                UsageInterest::Latest(scope) => scope.clone(),
                UsageInterest::History(object) => UsageScope::of(object),
            })
            .collect()
    }

    /// The scopes to list: [`wanted`](Self::wanted), with namespaces left
    /// out when every pod is listed anyway.
    fn scopes(&self) -> BTreeSet<UsageScope> {
        let wanted = self.wanted();
        let all_pods = wanted.contains(&UsageScope::Pods(None));
        wanted
            .into_iter()
            .filter(|scope| !(all_pods && matches!(scope, UsageScope::Pods(Some(_)))))
            .collect()
    }

    /// The listed scope that covers `scope`.
    fn covering(scope: &UsageScope, scopes: &BTreeSet<UsageScope>) -> UsageScope {
        if matches!(scope, UsageScope::Pods(Some(_))) && scopes.contains(&UsageScope::Pods(None)) {
            UsageScope::Pods(None)
        } else {
            scope.clone()
        }
    }

    /// Scopes to list now: never listed, or not within `interval`.
    fn due(&self, now: Instant, interval: Duration) -> Vec<UsageScope> {
        self.scopes()
            .into_iter()
            .filter(|scope| {
                self.scopes
                    .get(scope)
                    .is_none_or(|s| now.duration_since(s.polled_at) >= interval)
            })
            .collect()
    }

    /// Records one scope's poll.
    fn apply(
        &mut self,
        scope: UsageScope,
        result: Result<Vec<(UsageObject, Usage)>, UsageStatus>,
        now: Instant,
    ) {
        let previous = self.scopes.remove(&scope);
        let previous_status = previous.as_ref().map(|s| s.status.clone());
        let (status, items) = match result {
            Ok(items) => {
                for (object, usage) in &items {
                    self.history.record(object, usage);
                }
                if let Some(newest) = items.iter().map(|(_, u)| u.cpu.time).max() {
                    self.history.trim(newest);
                }
                (UsageStatus::Live, items)
            }
            // Keep the last values on screen; the status says they are old.
            Err(status) => (status, previous.map(|s| s.items).unwrap_or_default()),
        };
        if previous_status.as_ref() != Some(&status) {
            match &status {
                UsageStatus::Live => {
                    tracing::debug!(context = %self.context, ?scope, "metrics API live");
                }
                other => tracing::warn!(context = %self.context, ?scope, "metrics API: {other:?}"),
            }
        }
        self.scopes.insert(
            scope,
            ScopeState {
                polled_at: now,
                status,
                items,
            },
        );
    }

    /// The snapshot for what views want now.
    fn snapshot(&self) -> UsageSnapshot {
        let scopes = self.scopes();
        let status: BTreeMap<UsageScope, UsageStatus> = self
            .wanted()
            .into_iter()
            .filter_map(|wanted| {
                let polled = self.scopes.get(&Self::covering(&wanted, &scopes))?;
                Some((wanted, polled.status.clone()))
            })
            .collect();
        let latest: BTreeMap<UsageObject, Usage> = scopes
            .iter()
            .filter_map(|scope| self.scopes.get(scope))
            .flat_map(|s| s.items.iter().cloned())
            .collect();
        let objects: BTreeSet<&UsageObject> = self
            .subscriptions
            .values()
            .filter_map(|interest| match interest {
                UsageInterest::History(object) => Some(object),
                UsageInterest::Latest(_) => None,
            })
            .collect();
        let history = objects
            .into_iter()
            .map(|o| self.history.series(o))
            .collect();
        UsageSnapshot {
            context: self.context.clone(),
            status,
            latest,
            history,
        }
    }

    /// The snapshot, if views want one and it differs from the last sent.
    fn changed_snapshot(&mut self) -> Option<UsageSnapshot> {
        if !self.is_active() {
            return None;
        }
        let snapshot = self.snapshot();
        if self.last_sent.as_ref() == Some(&snapshot) {
            return None;
        }
        self.last_sent = Some(snapshot.clone());
        Some(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use model::Sample;

    use super::*;

    fn pod(namespace: &str, name: &str) -> UsageObject {
        UsageObject::Pod {
            namespace: namespace.into(),
            name: name.into(),
        }
    }

    fn usage(secs: u64, cpu: f64) -> Usage {
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        Usage {
            cpu: Sample { time, value: cpu },
            memory: Sample { time, value: 1.0 },
        }
    }

    fn state(interests: &[UsageInterest]) -> State {
        let mut state = State::new(ContextName::new("test"), Duration::from_hours(1));
        for (id, interest) in interests.iter().enumerate() {
            state.handle(Message::Subscribe(id as u64, interest.clone()));
        }
        state
    }

    fn ns(name: &str) -> UsageScope {
        UsageScope::Pods(Some(name.into()))
    }

    #[test]
    fn all_pods_cover_every_namespace() {
        let state = state(&[
            UsageInterest::Latest(ns("shop")),
            UsageInterest::Latest(UsageScope::Pods(None)),
            UsageInterest::History(pod("billing", "api-1")),
            UsageInterest::History(UsageObject::Node("n1".into())),
        ]);
        assert_eq!(
            state.scopes().into_iter().collect::<Vec<_>>(),
            [UsageScope::Nodes, UsageScope::Pods(None)]
        );
    }

    #[test]
    fn a_history_polls_its_namespace() {
        let state = state(&[UsageInterest::History(pod("shop", "web-1"))]);
        assert_eq!(state.scopes().into_iter().collect::<Vec<_>>(), [ns("shop")]);
    }

    #[test]
    fn only_stale_scopes_are_due() {
        let mut state = state(&[
            UsageInterest::Latest(UsageScope::Nodes),
            UsageInterest::Latest(ns("shop")),
        ]);
        let start = Instant::now();
        let interval = Duration::from_secs(15);
        assert_eq!(state.due(start, interval).len(), 2);
        state.apply(UsageScope::Nodes, Ok(Vec::new()), start);
        assert_eq!(state.due(start, interval), [ns("shop")]);
        assert_eq!(state.due(start + interval, interval).len(), 2);
    }

    #[test]
    fn status_is_reported_for_each_wanted_scope() {
        let mut state = state(&[
            UsageInterest::Latest(ns("shop")),
            UsageInterest::Latest(UsageScope::Pods(None)),
            UsageInterest::Latest(UsageScope::Nodes),
        ]);
        let now = Instant::now();
        state.apply(UsageScope::Pods(None), Ok(Vec::new()), now);
        state.apply(
            UsageScope::Nodes,
            Err(UsageStatus::Forbidden("nodes".to_owned())),
            now,
        );
        let status = state.snapshot().status;
        assert_eq!(status[&ns("shop")], UsageStatus::Live);
        assert_eq!(status[&UsageScope::Pods(None)], UsageStatus::Live);
        assert_eq!(
            status[&UsageScope::Nodes],
            UsageStatus::Forbidden("nodes".to_owned())
        );
    }

    #[test]
    fn a_failed_poll_keeps_the_last_values() {
        let mut state = state(&[UsageInterest::Latest(ns("shop"))]);
        let now = Instant::now();
        state.apply(
            ns("shop"),
            Ok(vec![(pod("shop", "web-1"), usage(100, 0.5))]),
            now,
        );
        state.apply(
            ns("shop"),
            Err(UsageStatus::Failed("timeout".to_owned())),
            now,
        );
        let snapshot = state.snapshot();
        assert_eq!(snapshot.latest[&pod("shop", "web-1")].cpu.value, 0.5);
        assert_eq!(
            snapshot.status[&ns("shop")],
            UsageStatus::Failed("timeout".to_owned())
        );
    }

    #[test]
    fn history_grows_only_for_new_samples() {
        let web = pod("shop", "web-1");
        let mut state = state(&[UsageInterest::History(web.clone())]);
        let now = Instant::now();
        state.apply(ns("shop"), Ok(vec![(web.clone(), usage(100, 0.1))]), now);
        state.apply(ns("shop"), Ok(vec![(web.clone(), usage(100, 0.1))]), now);
        state.apply(ns("shop"), Ok(vec![(web.clone(), usage(115, 0.2))]), now);
        let history = state.snapshot().history;
        assert_eq!(history.len(), 1);
        let values: Vec<f64> = history[0].cpu.samples.iter().map(|s| s.value).collect();
        assert_eq!(values.len(), 2);
        assert!((values[1] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn unchanged_snapshots_are_not_sent_twice() {
        let mut state = state(&[UsageInterest::Latest(UsageScope::Nodes)]);
        let now = Instant::now();
        state.apply(UsageScope::Nodes, Ok(Vec::new()), now);
        assert!(state.changed_snapshot().is_some());
        state.apply(UsageScope::Nodes, Ok(Vec::new()), now);
        assert!(state.changed_snapshot().is_none());
    }

    #[test]
    fn nothing_is_sent_without_subscribers() {
        let mut state = state(&[]);
        assert!(state.changed_snapshot().is_none());
        assert!(state.scopes().is_empty());
    }
}
