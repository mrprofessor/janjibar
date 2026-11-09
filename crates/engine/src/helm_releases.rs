//! Helm releases for the UI.
//!
//! While the UI shows releases, a metadata-only watch follows Helm's
//! Secrets and `ConfigMaps` in the selected context, so no payload moves to
//! keep the list live. Each release's latest revision is decoded in the
//! background, a few at a time, to fill the chart columns. One revision's
//! full details are decoded only when the UI asks.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::StreamExt as _;
use k8s_openapi::NamespaceResourceScope;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use k8s_openapi::serde::de::DeserializeOwned;
use kube::core::PartialObjectMeta;
use kube::runtime::{WatchStreamExt as _, reflector, watcher};
use kube::{Api, Client, Resource};
use model::{
    ContextName, Event, HelmChartInfo, HelmCommand, HelmEvent, HelmHistoryEntry, HelmReleaseKey,
    HelmRevision, HelmRow, HelmSnapshot, HelmStatus, HelmStorage,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior};

use crate::age::human_age;
use crate::pods::{AbortOnDrop, SNAPSHOT_WINDOW};

/// The selected context's client, once it has connected.
pub(crate) type Connected = Option<(ContextName, Client)>;

/// Payloads decoded at once to fill chart columns. Each one is a GET of up
/// to 1 MiB plus a decode on the blocking pool, so a cluster with hundreds
/// of releases fills in steadily without a burst.
const MAX_DECODES: usize = 4;

/// How often ages are re-checked when nothing else changes. Release ages
/// are rarely younger than minutes, so this is slower than the pod tick.
const AGE_TICK: Duration = Duration::from_secs(5);

/// The Helm side of the engine: the watch while the UI shows releases, and
/// the latest details request.
#[derive(Default)]
pub(crate) struct HelmFeed {
    watch: Option<AbortOnDrop<()>>,
    details: Option<AbortOnDrop<()>>,
}

impl HelmFeed {
    /// Carries out one command from the UI.
    pub(crate) fn command(
        &mut self,
        command: HelmCommand,
        connected: &watch::Sender<Connected>,
        events: &mpsc::UnboundedSender<Event>,
    ) {
        match command {
            HelmCommand::Open => {
                if self.watch.is_none() {
                    let task = follow(connected.subscribe(), events.clone());
                    self.watch = Some(AbortOnDrop(tokio::spawn(task)));
                }
            }
            HelmCommand::Close => {
                self.watch = None;
                self.details = None;
            }
            HelmCommand::LoadDetails {
                context,
                release,
                revision,
            } => {
                let task = load_details(
                    connected.subscribe(),
                    events.clone(),
                    context,
                    release,
                    revision,
                );
                // Replacing the handle aborts a request still in flight.
                self.details = Some(AbortOnDrop(tokio::spawn(task)));
            }
        }
    }
}

/// Watches whichever context is connected, starting over when it changes.
async fn follow(mut connected: watch::Receiver<Connected>, events: mpsc::UnboundedSender<Event>) {
    loop {
        let current = connected.borrow_and_update().clone();
        let Some((context, client)) = current else {
            if connected.changed().await.is_err() {
                return;
            }
            continue;
        };
        // Both branches are cancel safe. Dropping the watch future aborts
        // its drivers and decodes.
        tokio::select! {
            () = watch_releases(context, client, events.clone()) => return,
            changed = connected.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
    }
}

/// Waits for `context` to be connected, then fetches and decodes one
/// revision and sends the result.
async fn load_details(
    mut connected: watch::Receiver<Connected>,
    events: mpsc::UnboundedSender<Event>,
    context: ContextName,
    release: HelmReleaseKey,
    revision: u32,
) {
    let client = {
        let Ok(slot) = connected
            .wait_for(|c| c.as_ref().is_some_and(|(ctx, _)| *ctx == context))
            .await
        else {
            return;
        };
        slot.as_ref().map(|(_, client)| client.clone())
    };
    let Some(client) = client else { return };
    let record = record_of(&release, revision);
    let result = helm::get(&client, &record)
        .await
        .map_err(|err| error_chain(&err));
    if let Err(message) = &result {
        tracing::warn!(%context, release = %release.name, revision, "{message}");
    }
    // A send error means the UI is gone.
    let _ = events.send(Event::Helm(HelmEvent::Details {
        context,
        release,
        revision,
        result,
    }));
}

/// What the drivers report to the snapshot loop.
enum Signal {
    /// The store changed.
    Changed(HelmStorage),
    /// A full list arrived.
    Loaded(HelmStorage),
    /// The watch failed; it retries on its own.
    Failed(HelmStorage, watcher::Error),
}

/// Starts a metadata-only watch on one storage kind and returns its store.
/// The watch runs until the returned guard is dropped.
fn drive<K>(
    client: Client,
    storage: HelmStorage,
    signals: mpsc::UnboundedSender<Signal>,
) -> (reflector::Store<PartialObjectMeta<K>>, AbortOnDrop<()>)
where
    K: Resource<Scope = NamespaceResourceScope, DynamicType = ()>
        + Clone
        + DeserializeOwned
        + Debug
        + Send
        + Sync
        + 'static,
{
    // `Api<PartialObjectMeta<K>>` asks the server for metadata only.
    let api: Api<PartialObjectMeta<K>> = Api::all(client);
    let (store, writer) = reflector::store();
    let config = watcher::Config::default()
        .labels(helm::OWNER_SELECTOR)
        .page_size(500);
    let stream = watcher(api, config).default_backoff().reflect(writer);
    let task = tokio::spawn(async move {
        let mut stream = std::pin::pin!(stream);
        while let Some(item) = stream.next().await {
            let signal = match item {
                Ok(watcher::Event::InitDone) => Signal::Loaded(storage),
                Ok(watcher::Event::Apply(_) | watcher::Event::Delete(_)) => {
                    Signal::Changed(storage)
                }
                Ok(_) => continue,
                Err(err) => Signal::Failed(storage, err),
            };
            if signals.send(signal).is_err() {
                return;
            }
        }
    });
    (store, AbortOnDrop(task))
}

/// One storage kind being watched.
struct Source<K: Resource<DynamicType = ()> + Clone + 'static> {
    store: reflector::Store<PartialObjectMeta<K>>,
    loaded: bool,
    _driver: AbortOnDrop<()>,
}

impl<K: Resource<DynamicType = ()> + Clone + 'static> Source<K> {
    fn revisions(&self, storage: HelmStorage, out: &mut Vec<HelmRevision>) {
        out.extend(
            self.store
                .state()
                .iter()
                .filter_map(|meta| helm::revision_from_meta(&meta.metadata, storage)),
        );
    }
}

/// Watches every release in a context and sends snapshots until the UI is
/// gone or the future is dropped.
///
/// Snapshots go out at most once per [`SNAPSHOT_WINDOW`], and only when
/// something the UI shows changed: rows, ages, charts, loading or errors.
pub async fn watch_releases(
    context: ContextName,
    client: Client,
    events: mpsc::UnboundedSender<Event>,
) {
    let (signals_tx, mut signals) = mpsc::unbounded_channel();
    let (store, driver) = drive::<Secret>(client.clone(), HelmStorage::Secret, signals_tx.clone());
    let mut secrets = Source {
        store,
        loaded: false,
        _driver: driver,
    };
    let (store, driver) = drive::<ConfigMap>(client.clone(), HelmStorage::ConfigMap, signals_tx);
    let mut config_maps = Some(Source {
        store,
        loaded: false,
        _driver: driver,
    });

    let mut charts = Charts::new(client);
    let mut ages = tokio::time::interval(AGE_TICK);
    ages.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The failing storage kind and what to tell the user.
    let mut problem: Option<(HelmStorage, Arc<str>)> = None;
    let mut last: Option<HelmSnapshot> = None;
    let mut last_sent: Option<Instant> = None;

    loop {
        let loaded = secrets.loaded && config_maps.as_ref().is_none_or(|s| s.loaded);
        // All branches are cancel safe: `mpsc::Receiver::recv`,
        // `JoinSet::join_next` and `Interval::tick`.
        let first = tokio::select! {
            signal = signals.recv() => {
                let Some(signal) = signal else { return };
                Some(signal)
            }
            Some(done) = charts.decodes.join_next(), if !charts.decodes.is_empty() => {
                charts.finish(done);
                None
            }
            _ = ages.tick(), if loaded => None,
        };
        // Coalesce: wait out the rest of the window so a burst of changes
        // becomes one snapshot.
        if let Some(sent) = last_sent {
            let wait = SNAPSHOT_WINDOW.saturating_sub(sent.elapsed());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
        let pending = std::iter::from_fn(|| signals.try_recv().ok());
        for signal in first.into_iter().chain(pending) {
            match signal {
                // News from a storage kind means its watch works again.
                Signal::Changed(storage) => {
                    if problem.as_ref().is_some_and(|(s, _)| *s == storage) {
                        problem = None;
                    }
                }
                Signal::Loaded(storage) => {
                    if problem.as_ref().is_some_and(|(s, _)| *s == storage) {
                        problem = None;
                    }
                    match storage {
                        HelmStorage::Secret => secrets.loaded = true,
                        HelmStorage::ConfigMap => {
                            if let Some(source) = &mut config_maps {
                                source.loaded = true;
                            }
                        }
                    }
                }
                Signal::Failed(HelmStorage::ConfigMap, err) if is_forbidden(&err) => {
                    // `ConfigMap` storage is rare. A role that cannot list
                    // `ConfigMaps` should still see Secret-backed releases.
                    tracing::warn!(%context, "skipping ConfigMap-stored Helm releases: {err}");
                    config_maps = None;
                    if problem
                        .as_ref()
                        .is_some_and(|(s, _)| *s == HelmStorage::ConfigMap)
                    {
                        problem = None;
                    }
                }
                Signal::Failed(storage, err) => {
                    tracing::warn!(%context, ?storage, "Helm release watch error: {err}");
                    problem = Some((storage, problem_text(storage, &err).into()));
                }
            }
        }

        let mut revisions = Vec::new();
        secrets.revisions(HelmStorage::Secret, &mut revisions);
        if let Some(source) = &config_maps {
            source.revisions(HelmStorage::ConfigMap, &mut revisions);
        }
        let loaded = secrets.loaded && config_maps.as_ref().is_none_or(|s| s.loaded);
        let rows = project(revisions, &mut charts, SystemTime::now());
        charts.start();
        last_sent = Some(Instant::now());

        let snapshot = HelmSnapshot {
            context: context.clone(),
            rows: if loaded { rows.into() } else { Arc::from([]) },
            loaded,
            problem: problem.as_ref().map(|(_, text)| Arc::clone(text)),
        };
        // Before the first list there is nothing to show but an error.
        if !loaded && snapshot.problem.is_none() {
            continue;
        }
        if last.as_ref() == Some(&snapshot) {
            continue;
        }
        last = Some(snapshot.clone());
        tracing::debug!(%context, rows = snapshot.rows.len(), "Helm snapshot sent");
        if events
            .send(Event::Helm(HelmEvent::Releases(snapshot)))
            .is_err()
        {
            return;
        }
    }
}

/// Which revision of which release.
type RevisionKey = (HelmReleaseKey, u32);

/// Chart info per latest revision, decoded in the background.
struct Charts {
    client: Client,
    known: HashMap<RevisionKey, HelmChartInfo>,
    queue: VecDeque<RevisionKey>,
    decodes: JoinSet<(RevisionKey, HelmChartInfo)>,
}

impl Charts {
    fn new(client: Client) -> Self {
        Self {
            client,
            known: HashMap::new(),
            queue: VecDeque::new(),
            decodes: JoinSet::new(),
        }
    }

    /// The chart info for `key`, queueing a decode the first time.
    fn lookup(&mut self, key: &RevisionKey) -> HelmChartInfo {
        if let Some(info) = self.known.get(key) {
            return info.clone();
        }
        self.known.insert(key.clone(), HelmChartInfo::Loading);
        self.queue.push_back(key.clone());
        HelmChartInfo::Loading
    }

    /// Forgets revisions that are no longer any release's latest.
    fn retain(&mut self, latest: &HashSet<RevisionKey>) {
        self.known.retain(|key, _| latest.contains(key));
        self.queue.retain(|key| latest.contains(key));
    }

    /// Starts queued decodes up to [`MAX_DECODES`] at once.
    fn start(&mut self) {
        while self.decodes.len() < MAX_DECODES {
            let Some(key) = self.queue.pop_front() else {
                return;
            };
            let client = self.client.clone();
            self.decodes.spawn(async move {
                let record = record_of(&key.0, key.1);
                let info = match helm::get(&client, &record).await {
                    Ok(details) => HelmChartInfo::Known(details.chart),
                    Err(err) => {
                        let message = error_chain(&err);
                        tracing::debug!(release = %key.0.name, revision = key.1, "{message}");
                        HelmChartInfo::Unreadable(message.into())
                    }
                };
                (key, info)
            });
        }
    }

    /// Records a finished decode, unless the revision was pruned meanwhile.
    fn finish(&mut self, done: Result<(RevisionKey, HelmChartInfo), tokio::task::JoinError>) {
        match done {
            Ok((key, info)) => {
                if let Some(slot) = self.known.get_mut(&key) {
                    *slot = info;
                }
            }
            Err(err) => tracing::warn!("a Helm chart decode did not finish: {err}"),
        }
        self.start();
    }
}

/// Groups revisions into rows sorted by namespace, name and storage, with
/// history newest first and chart info from `charts`.
fn project(revisions: Vec<HelmRevision>, charts: &mut Charts, now: SystemTime) -> Vec<HelmRow> {
    let mut by_release: BTreeMap<HelmReleaseKey, Vec<HelmRevision>> = BTreeMap::new();
    for revision in revisions {
        let key = HelmReleaseKey {
            namespace: Arc::clone(&revision.namespace),
            name: Arc::clone(&revision.name),
            storage: revision.storage,
        };
        by_release.entry(key).or_default().push(revision);
    }
    let mut latest_keys = HashSet::with_capacity(by_release.len());
    let mut rows = Vec::with_capacity(by_release.len());
    for (key, mut revisions) in by_release {
        revisions.sort_unstable_by_key(|r| std::cmp::Reverse(r.revision));
        let history: Arc<[HelmHistoryEntry]> = revisions
            .iter()
            .map(|r| HelmHistoryEntry {
                revision: r.revision,
                status: r.status,
                age: age(r.updated, now),
            })
            .collect();
        let Some(latest) = history.first().cloned() else {
            continue;
        };
        let revision_key = (key.clone(), latest.revision);
        let chart = charts.lookup(&revision_key);
        latest_keys.insert(revision_key);
        rows.push(HelmRow {
            key,
            revision: latest.revision,
            status: latest.status,
            age: latest.age,
            chart,
            history,
        });
    }
    charts.retain(&latest_keys);
    rows
}

fn age(updated: Option<SystemTime>, now: SystemTime) -> Arc<str> {
    let Some(updated) = updated else {
        return Arc::from("-");
    };
    let seconds = match now.duration_since(updated) {
        Ok(elapsed) => i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
        Err(ahead) => -i64::try_from(ahead.duration().as_secs()).unwrap_or(i64::MAX),
    };
    human_age(seconds).into()
}

/// A revision record for `helm::get`, which only reads its identity.
fn record_of(release: &HelmReleaseKey, revision: u32) -> HelmRevision {
    HelmRevision {
        namespace: Arc::clone(&release.namespace),
        name: Arc::clone(&release.name),
        revision,
        status: HelmStatus::Unknown,
        updated: None,
        storage: release.storage,
    }
}

fn is_forbidden(err: &watcher::Error) -> bool {
    match err {
        watcher::Error::InitialListFailed(kube::Error::Api(status))
        | watcher::Error::WatchStartFailed(kube::Error::Api(status))
        | watcher::Error::WatchFailed(kube::Error::Api(status))
        | watcher::Error::WatchError(status) => status.is_forbidden(),
        _ => false,
    }
}

/// What the UI says when a watch fails.
fn problem_text(storage: HelmStorage, err: &watcher::Error) -> String {
    let kind = match storage {
        HelmStorage::Secret => "Secrets",
        HelmStorage::ConfigMap => "ConfigMaps",
    };
    if is_forbidden(err) {
        format!("not allowed to watch {kind} across all namespaces")
    } else {
        format!("watching Helm {kind} failed: {}", error_chain(err))
    }
}

/// An error and its sources, joined like `a: b: c`.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        // kube's errors often repeat their source in their own message.
        if !text.contains(&message) {
            text.push_str(": ");
            text.push_str(&message);
        }
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revision(ns: &str, name: &str, rev: u32, status: HelmStatus, age_secs: u64) -> HelmRevision {
        HelmRevision {
            namespace: ns.into(),
            name: name.into(),
            revision: rev,
            status,
            updated: Some(now() - Duration::from_secs(age_secs)),
            storage: HelmStorage::Secret,
        }
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_794_182_417)
    }

    fn charts() -> Charts {
        let config = kube::Config::new("http://127.0.0.1:9".parse().unwrap());
        Charts::new(Client::try_from(config).unwrap())
    }

    fn key(ns: &str, name: &str) -> HelmReleaseKey {
        HelmReleaseKey {
            namespace: ns.into(),
            name: name.into(),
            storage: HelmStorage::Secret,
        }
    }

    #[tokio::test]
    async fn rows_group_revisions_newest_first() {
        let mut charts = charts();
        let rows = project(
            vec![
                revision("shop", "web", 1, HelmStatus::Superseded, 3 * 86_400),
                revision("data", "pg", 4, HelmStatus::Deployed, 600),
                revision("shop", "web", 2, HelmStatus::Deployed, 7_200),
            ],
            &mut charts,
            now(),
        );
        let summary: Vec<_> = rows
            .iter()
            .map(|r| (&*r.key.name, r.revision, r.status, &*r.age, r.history.len()))
            .collect();
        assert_eq!(
            summary,
            [
                ("pg", 4, HelmStatus::Deployed, "10m", 1),
                ("web", 2, HelmStatus::Deployed, "120m", 2),
            ]
        );
        let web: Vec<_> = rows[1]
            .history
            .iter()
            .map(|h| (h.revision, &*h.age))
            .collect();
        assert_eq!(web, [(2, "120m"), (1, "3d")]);
    }

    #[tokio::test]
    async fn each_latest_revision_is_decoded_once() {
        let mut charts = charts();
        let revisions = vec![
            revision("shop", "web", 1, HelmStatus::Superseded, 60),
            revision("shop", "web", 2, HelmStatus::Deployed, 60),
            revision("data", "pg", 1, HelmStatus::Deployed, 60),
        ];
        let rows = project(revisions.clone(), &mut charts, now());
        assert!(rows.iter().all(|r| r.chart == HelmChartInfo::Loading));
        let queued: Vec<_> = charts
            .queue
            .iter()
            .map(|(k, rev)| (&*k.name, *rev))
            .collect();
        assert_eq!(queued, [("pg", 1), ("web", 2)]);

        // Projecting again queues nothing new.
        project(revisions, &mut charts, now());
        assert_eq!(charts.queue.len(), 2);
    }

    #[tokio::test]
    async fn decoded_charts_fill_rows_and_upgrades_decode_again() {
        let mut charts = charts();
        let web2 = (key("shop", "web"), 2);
        project(
            vec![revision("shop", "web", 2, HelmStatus::Deployed, 60)],
            &mut charts,
            now(),
        );
        charts.queue.clear();
        let chart = model::HelmChart {
            name: "web".into(),
            version: "1.0.0".into(),
            app_version: "2.0".into(),
        };
        charts
            .known
            .insert(web2.clone(), HelmChartInfo::Known(chart.clone()));
        let rows = project(
            vec![revision("shop", "web", 2, HelmStatus::Deployed, 60)],
            &mut charts,
            now(),
        );
        assert_eq!(rows[0].chart, HelmChartInfo::Known(chart));

        // An upgrade makes revision 3 the latest: it is queued, and the old
        // entry is forgotten.
        let rows = project(
            vec![
                revision("shop", "web", 2, HelmStatus::Superseded, 60),
                revision("shop", "web", 3, HelmStatus::PendingUpgrade, 5),
            ],
            &mut charts,
            now(),
        );
        assert_eq!(rows[0].chart, HelmChartInfo::Loading);
        assert!(!charts.known.contains_key(&web2));
        assert_eq!(charts.queue.len(), 1);
    }

    #[test]
    fn ages_handle_missing_and_future_times() {
        assert_eq!(&*age(None, now()), "-");
        assert_eq!(
            &*age(Some(now() + Duration::from_secs(30)), now()),
            "<invalid>"
        );
        assert_eq!(&*age(Some(now() - Duration::from_secs(45)), now()), "45s");
    }
}
