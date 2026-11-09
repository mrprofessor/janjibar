//! A root shell on a node, through a short-lived privileged pod.
//!
//! The helper pod is pinned to the node, shares the host's PID, network
//! and IPC namespaces, and only sleeps. The shell is an `exec` into it that
//! runs `nsenter` on PID 1, the host's init, so it gets the host's mount,
//! UTS, IPC, network and PID namespaces: a shell on the node itself. The
//! pod is deleted when the shell ends, and `activeDeadlineSeconds` ends it
//! anyway if the app dies first.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::StreamExt as _;
use k8s_openapi::api::core::v1::{Container, Pod, PodSpec, SecurityContext, Toleration};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, PostParams};
use kube::runtime::watcher;
use kube::{Api, Client, ResourceExt as _};
use model::{NodeShellOptions, ShellError, ShellEvent, ShellInput, brand};

use super::exec::{Exec, shell_script};
use super::{ShellIo, shell_error};

/// The helper pod's only container.
const CONTAINER: &str = "shell";

/// The longest a helper pod lives, even if nobody deletes it: the safety
/// net for an app that quit or crashed before it could. Four hours, about
/// what Lens and Freelens give theirs.
const POD_DEADLINE_SECONDS: i64 = 4 * 60 * 60;

/// How long the helper pod may take to start, image pull included.
const START_TIMEOUT: Duration = Duration::from_secs(120);

/// Waiting reasons that will not fix themselves.
const STUCK: &[&str] = &[
    "ErrImagePull",
    "ImagePullBackOff",
    "InvalidImageName",
    "ErrImageNeverPull",
    "CreateContainerConfigError",
    "CreateContainerError",
];

/// Starts a helper pod on `node`, runs a shell through it, and deletes it.
pub(super) async fn run(
    client: Client,
    node: &str,
    options: &NodeShellOptions,
    helpers: &HelperPods,
    mut io: ShellIo,
) {
    let pods: Api<Pod> = Api::namespaced(client.clone(), &options.namespace);
    let progress = format!(
        "Starting a node shell on {node}: a privileged pod in {} with {}",
        options.namespace, options.image
    );
    if !io.send(ShellEvent::Progress(progress)).await {
        return;
    }
    let created = match pods
        .create(&PostParams::default(), &helper_pod(node, options))
        .await
    {
        Ok(created) => created,
        Err(err) => {
            io.fail(create_error(&err, &options.namespace)).await;
            return;
        }
    };
    let helper = helpers.register(&pods, created.name_any());
    tracing::info!(%node, pod = %helper.name, "node shell pod created");

    let started = tokio::time::timeout(
        START_TIMEOUT,
        wait_until_running(&pods, &helper.name, &mut io),
    )
    .await;
    let pending = match started {
        Ok(Ok(Some(pending))) => pending,
        // The terminal went away while the pod started.
        Ok(Ok(None)) => {
            helper.delete().await;
            return;
        }
        Ok(Err(err)) => {
            io.fail(err).await;
            helper.delete().await;
            return;
        }
        Err(_) => {
            io.fail(ShellError::Timeout(format!(
                "the node shell pod on {node} did not start within {} seconds",
                START_TIMEOUT.as_secs()
            )))
            .await;
            helper.delete().await;
            return;
        }
    };

    let shell = Exec {
        namespace: &options.namespace,
        pod: &helper.name,
        container: Some(CONTAINER),
        command: node_command(),
        pending,
    };
    shell.run(client, io).await;
    helper.delete().await;
}

/// Maps a failed create, with a hint when Pod Security admission refused
/// the privileged pod.
fn create_error(err: &kube::Error, namespace: &str) -> ShellError {
    match shell_error(err) {
        ShellError::Forbidden(message) if message.contains("violates PodSecurity") => {
            ShellError::Forbidden(format!(
                "{message}. A node shell needs a namespace that allows privileged pods; \
                 `{namespace}` does not. Use kube-system or another privileged namespace."
            ))
        }
        other => other,
    }
}

/// The helper pod for `node`.
fn helper_pod(node: &str, options: &NodeShellOptions) -> Pod {
    let labels = BTreeMap::from([
        ("app.kubernetes.io/name".to_owned(), "node-shell".to_owned()),
        (
            "app.kubernetes.io/managed-by".to_owned(),
            brand::FIELD_MANAGER.to_owned(),
        ),
    ]);
    Pod {
        metadata: ObjectMeta {
            generate_name: Some(generate_name(node)),
            namespace: Some(options.namespace.clone()),
            labels: Some(labels),
            ..ObjectMeta::default()
        },
        spec: Some(PodSpec {
            node_name: Some(node.to_owned()),
            host_pid: Some(true),
            host_network: Some(true),
            host_ipc: Some(true),
            restart_policy: Some("Never".to_owned()),
            termination_grace_period_seconds: Some(0),
            active_deadline_seconds: Some(POD_DEADLINE_SECONDS),
            automount_service_account_token: Some(false),
            // Run on tainted nodes too, control planes included.
            tolerations: Some(vec![Toleration {
                operator: Some("Exists".to_owned()),
                ..Toleration::default()
            }]),
            containers: vec![Container {
                name: CONTAINER.to_owned(),
                image: Some(options.image.clone()),
                command: Some(vec!["sleep".to_owned(), POD_DEADLINE_SECONDS.to_string()]),
                security_context: Some(SecurityContext {
                    privileged: Some(true),
                    ..SecurityContext::default()
                }),
                ..Container::default()
            }],
            ..PodSpec::default()
        }),
        status: None,
    }
}

/// `node-shell-<node>-`, cut so the server's five random characters still
/// fit a 63-character DNS label.
fn generate_name(node: &str) -> String {
    let node: String = node
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let node = node.trim_matches('-');
    if node.is_empty() {
        "node-shell-".to_owned()
    } else {
        format!("node-shell-{node}-")
    }
}

/// Enters every namespace of the host's PID 1, then starts a shell there.
fn node_command() -> Vec<String> {
    [
        "nsenter", "-t", "1", "-m", "-u", "-i", "-n", "-p", "--", "sh", "-c",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain([shell_script().to_owned()])
    .collect()
}

/// Where a starting helper pod stands.
#[derive(Debug, PartialEq, Eq)]
enum Startup {
    /// Still starting, with the container's waiting reason if any.
    Waiting(Option<String>),
    /// The container runs.
    Running,
    /// It will not start.
    Failed(String),
}

fn startup(pod: &Pod) -> Startup {
    let Some(status) = pod.status.as_ref() else {
        return Startup::Waiting(None);
    };
    if let Some(phase @ ("Failed" | "Succeeded")) = status.phase.as_deref() {
        let why = status
            .message
            .clone()
            .or_else(|| status.reason.clone())
            .unwrap_or_else(|| format!("the pod is {phase}"));
        return Startup::Failed(why);
    }
    let container = status
        .container_statuses
        .iter()
        .flatten()
        .find(|container| container.name == CONTAINER);
    let Some(state) = container.and_then(|container| container.state.as_ref()) else {
        return Startup::Waiting(None);
    };
    if state.running.is_some() {
        return Startup::Running;
    }
    if let Some(terminated) = &state.terminated {
        let why = terminated
            .message
            .clone()
            .or_else(|| terminated.reason.clone())
            .unwrap_or_else(|| format!("exited with code {}", terminated.exit_code));
        return Startup::Failed(why);
    }
    let reason = state
        .waiting
        .as_ref()
        .and_then(|waiting| waiting.reason.clone());
    if let Some(stuck) = reason.as_deref().filter(|reason| STUCK.contains(reason)) {
        let message = state
            .waiting
            .as_ref()
            .and_then(|waiting| waiting.message.clone())
            .unwrap_or_default();
        return Startup::Failed(
            format!("{stuck}: {message}")
                .trim_end_matches(": ")
                .to_owned(),
        );
    }
    Startup::Waiting(reason)
}

/// Watches the helper pod until its container runs. Returns the input the
/// terminal sent meanwhile, to replay to the shell, or `None` when the
/// terminal went away.
async fn wait_until_running(
    pods: &Api<Pod>,
    name: &str,
    io: &mut ShellIo,
) -> Result<Option<Vec<ShellInput>>, ShellError> {
    let mut updates = std::pin::pin!(watcher::watch_object(pods.clone(), name));
    let mut pending = Vec::new();
    let mut last_reason = None;
    loop {
        tokio::select! {
            update = updates.next() => {
                let pod = match update {
                    Some(Ok(Some(pod))) => pod,
                    Some(Ok(None)) => {
                        return Err(ShellError::PodFailed("the pod was deleted".to_owned()));
                    }
                    Some(Err(err)) => {
                        return Err(ShellError::PodFailed(format!("watching the pod failed: {err}")));
                    }
                    None => {
                        return Err(ShellError::PodFailed("the pod watch ended".to_owned()));
                    }
                };
                match startup(&pod) {
                    Startup::Running => return Ok(Some(pending)),
                    Startup::Failed(why) => return Err(ShellError::PodFailed(why)),
                    Startup::Waiting(reason) => {
                        if reason.is_some() && reason != last_reason {
                            let note = format!("Waiting for the pod: {}", reason.as_deref().unwrap_or_default());
                            if !io.send(ShellEvent::Progress(note)).await {
                                return Ok(None);
                            }
                            last_reason = reason;
                        }
                    }
                }
            }
            input = io.input.recv() => match input {
                // Keys typed early and the first grid size go to the shell
                // once it runs.
                Some(input) => pending.push(input),
                None => return Ok(None),
            },
        }
    }
}

/// The helper pods that are up, so an app that quits can delete them
/// before its runtime stops. Cheap to clone; clones share the list.
#[derive(Clone, Debug, Default)]
pub(crate) struct HelperPods {
    live: Arc<Mutex<HashMap<String, Api<Pod>>>>,
}

impl HelperPods {
    fn register(&self, pods: &Api<Pod>, name: String) -> HelperPod {
        self.lock().insert(name.clone(), pods.clone());
        HelperPod {
            pods: pods.clone(),
            name,
            list: self.clone(),
        }
    }

    /// Deletes every helper pod that is up.
    pub(crate) async fn delete_all(&self) {
        let live: Vec<(String, Api<Pod>)> = self.lock().drain().collect();
        let deletes = live.iter().map(|(name, pods)| delete(pods, name));
        futures::future::join_all(deletes).await;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Api<Pod>>> {
        // The map stays consistent even if a holder panicked mid-insert.
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A helper pod to delete when the shell is done.
struct HelperPod {
    pods: Api<Pod>,
    name: String,
    list: HelperPods,
}

impl HelperPod {
    /// Deletes the pod, then takes it off the list. In that order: if the
    /// app quits while the delete is in flight, `delete_all` still sees it.
    async fn delete(self) {
        delete(&self.pods, &self.name).await;
        self.list.lock().remove(&self.name);
    }
}

/// Deletes a helper pod at once, without a grace period.
async fn delete(pods: &Api<Pod>, name: &str) {
    let params = DeleteParams {
        grace_period_seconds: Some(0),
        ..DeleteParams::background()
    };
    match pods.delete(name, &params).await {
        Ok(_) => tracing::info!(pod = %name, "node shell pod deleted"),
        // Deleted already, by the shell's own cleanup or by someone else.
        Err(kube::Error::Api(status)) if status.code == 404 => {}
        Err(err) => tracing::warn!(
            pod = %name,
            "could not delete the node shell pod; activeDeadlineSeconds ends it later: {err}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateRunning, ContainerStateWaiting, ContainerStatus, PodStatus,
    };

    use super::*;

    fn with_state(state: ContainerState) -> Pod {
        Pod {
            status: Some(PodStatus {
                phase: Some("Pending".to_owned()),
                container_statuses: Some(vec![ContainerStatus {
                    name: CONTAINER.to_owned(),
                    state: Some(state),
                    ..ContainerStatus::default()
                }]),
                ..PodStatus::default()
            }),
            ..Pod::default()
        }
    }

    fn waiting(reason: &str, message: &str) -> Pod {
        with_state(ContainerState {
            waiting: Some(ContainerStateWaiting {
                reason: Some(reason.to_owned()),
                message: Some(message.to_owned()),
            }),
            ..ContainerState::default()
        })
    }

    #[test]
    fn a_pod_security_refusal_says_which_namespace_to_use() {
        let err = kube::Error::Api(
            kube::core::Status::failure(
                "pods \"node-shell-w-abcde\" is forbidden: violates PodSecurity \"restricted:latest\"",
                "Forbidden",
            )
            .with_code(403)
            .boxed(),
        );
        let ShellError::Forbidden(message) = create_error(&err, "apps") else {
            panic!("not forbidden");
        };
        assert!(message.contains("`apps` does not"), "{message}");
        assert!(message.contains("kube-system"), "{message}");
    }

    #[test]
    fn the_helper_pod_is_privileged_on_the_node_in_the_host_namespaces() {
        let options = NodeShellOptions {
            image: "example.com/tools:1".to_owned(),
            namespace: "ops".to_owned(),
        };
        let pod = helper_pod("worker-1", &options);
        assert_eq!(pod.metadata.namespace.as_deref(), Some("ops"));
        assert_eq!(
            pod.metadata.generate_name.as_deref(),
            Some("node-shell-worker-1-")
        );
        let spec = pod.spec.unwrap();
        assert_eq!(spec.node_name.as_deref(), Some("worker-1"));
        assert_eq!(spec.host_pid, Some(true));
        assert_eq!(spec.host_network, Some(true));
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
        assert_eq!(spec.active_deadline_seconds, Some(POD_DEADLINE_SECONDS));
        assert_eq!(spec.automount_service_account_token, Some(false));
        let container = &spec.containers[0];
        assert_eq!(container.image.as_deref(), Some("example.com/tools:1"));
        assert_eq!(
            container.security_context.as_ref().unwrap().privileged,
            Some(true)
        );
    }

    #[test]
    fn long_or_dotted_node_names_still_make_a_valid_pod_name() {
        let name = generate_name("ip-10-0-1-23.eu-west-1.compute.internal");
        assert_eq!(name, "node-shell-ip-10-0-1-23-eu-west-1-compute-internal-");
        let long = generate_name(&"n".repeat(80));
        assert_eq!(long, format!("node-shell-{}-", "n".repeat(40)));
        assert!(long.len() + 5 <= 63, "the random suffix still fits a label");
        assert_eq!(generate_name("..."), "node-shell-");
    }

    #[test]
    fn the_shell_enters_the_hosts_namespaces_through_pid_1() {
        let command = node_command();
        assert_eq!(command[..3], ["nsenter", "-t", "1"]);
        for flag in ["-m", "-u", "-i", "-n", "-p"] {
            assert!(command.iter().any(|arg| arg == flag), "{flag}");
        }
        assert_eq!(command.last().map(String::as_str), Some(shell_script()));
    }

    #[test]
    fn startup_reads_running_stuck_and_waiting_states() {
        let running = with_state(ContainerState {
            running: Some(ContainerStateRunning::default()),
            ..ContainerState::default()
        });
        assert_eq!(startup(&running), Startup::Running);
        assert_eq!(
            startup(&waiting("ContainerCreating", "")),
            Startup::Waiting(Some("ContainerCreating".to_owned()))
        );
        assert_eq!(
            startup(&waiting(
                "ImagePullBackOff",
                "Back-off pulling image \"nope\""
            )),
            Startup::Failed("ImagePullBackOff: Back-off pulling image \"nope\"".to_owned())
        );
        assert_eq!(startup(&Pod::default()), Startup::Waiting(None));
        let failed = Pod {
            status: Some(PodStatus {
                phase: Some("Failed".to_owned()),
                reason: Some("OutOfcpu".to_owned()),
                ..PodStatus::default()
            }),
            ..Pod::default()
        };
        assert_eq!(startup(&failed), Startup::Failed("OutOfcpu".to_owned()));
    }
}
