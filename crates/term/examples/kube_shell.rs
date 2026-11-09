//! A Kubernetes shell in a window: `exec` into a pod, a root shell on a
//! node, or a local shell pinned to one context. It wires the engine to the
//! terminal pane the way the app will.
//!
//! ```sh
//! # A shell in a pod; the container is optional.
//! nix develop -c cargo run -p term --example kube_shell -- --context kind-platform pod NAMESPACE/POD [CONTAINER]
//! # A root shell on a node, through a privileged helper pod.
//! nix develop -c cargo run -p term --example kube_shell -- --context kind-platform node NODE [--image IMAGE] [--namespace NAMESPACE]
//! # Your login shell with KUBECONFIG pinned to the context.
//! nix develop -c cargo run -p term --example kube_shell -- --context kind-platform local
//! ```
//!
//! The window stays open after a pod or node shell ends, so its status
//! stays readable. Closing it ends the shell and deletes a node shell's pod;
//! on quit the app waits for that delete, as the app itself must.

#[path = "support/pane.rs"]
mod pane;

use std::time::Duration;

use anyhow::{Context as _, bail};
use engine::{EngineHandle, KubeconfigSources};
use model::{ContextName, NodeShellOptions, ShellTarget};
use pane::OnExit;
use term::{
    Engine, EngineOptions, LocalTerminalOptions, SessionSource, TerminalState, WorkingDirectory,
};

/// How long quitting waits for node shell pods to be deleted.
const QUIT_CLEANUP: Duration = Duration::from_secs(3);

const USAGE: &str = "usage: kube_shell --context NAME (pod NAMESPACE/POD [CONTAINER] | \
                     node NODE [--image IMAGE] [--namespace NAMESPACE] | local)";

/// What to open.
enum Mode {
    Shell(ShellTarget),
    Local(ContextName),
}

fn parse(args: &[String]) -> anyhow::Result<Mode> {
    let [flag, context, kind, rest @ ..] = args else {
        bail!(USAGE);
    };
    if flag != "--context" {
        bail!(USAGE);
    }
    let context = ContextName::new(context.as_str());
    match (kind.as_str(), rest) {
        ("pod", [pod, container @ ..]) if container.len() <= 1 => {
            let (namespace, pod) = pod
                .split_once('/')
                .context("name the pod as NAMESPACE/POD")?;
            Ok(Mode::Shell(ShellTarget::Pod {
                context,
                namespace: namespace.to_owned(),
                pod: pod.to_owned(),
                container: container.first().cloned(),
            }))
        }
        ("node", [node, flags @ ..]) => {
            let mut options = NodeShellOptions::default();
            let mut flags = flags.iter();
            while let Some(flag) = flags.next() {
                let value = flags
                    .next()
                    .with_context(|| format!("{flag} needs a value"))?;
                match flag.as_str() {
                    "--image" => options.image.clone_from(value),
                    "--namespace" => options.namespace.clone_from(value),
                    _ => bail!(USAGE),
                }
            }
            Ok(Mode::Shell(ShellTarget::Node {
                context,
                node: node.clone(),
                options,
            }))
        }
        ("local", []) => Ok(Mode::Local(context)),
        _ => bail!(USAGE),
    }
}

/// On quit, deletes the node shells' helper pods before the runtime stops.
/// GPUI gives quit handlers 200 ms, so this blocks the quit instead, for at
/// most [`QUIT_CLEANUP`].
fn close_shells_on_quit(
    engine: EngineHandle,
    runtime: tokio::runtime::Handle,
) -> impl FnOnce(&mut gpui_kit::App) {
    move |cx| {
        cx.on_app_quit(move |_| {
            let closed = engine.close_shells();
            if runtime
                .block_on(tokio::time::timeout(QUIT_CLEANUP, closed))
                .is_err()
            {
                tracing::warn!("node shell pods may outlive the app; their deadline ends them");
            }
            async {}
        })
        .detach();
    }
}

fn main() -> anyhow::Result<()> {
    pane::init_logging();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = parse(&args)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("engine")
        .enable_all()
        .build()?;
    // The engine reads the catalog before it serves any request, so the
    // context is known by the time the shell starts. The runtime stays here,
    // alive until the process exits, so a closed window's cleanup can run.
    let (engine, _events) = engine::start(runtime.handle(), KubeconfigSources::from_env());
    let on_quit = close_shells_on_quit(engine.clone(), runtime.handle().clone());

    match mode {
        Mode::Shell(target) => {
            let title = match &target {
                ShellTarget::Pod {
                    context,
                    namespace,
                    pod,
                    ..
                } => format!("{context}: {namespace}/{pod}"),
                ShellTarget::Node { context, node, .. } => format!("{context}: node {node}"),
            };
            let session = engine.open_shell(target);
            pane::run_with(
                title,
                OnExit::Stay,
                Box::new(engine),
                on_quit,
                move |config, window, cx| {
                    let source = Engine::new(SessionSource::new(session), EngineOptions::default());
                    TerminalState::new(source, config, window, cx)
                },
            );
        }
        Mode::Local(context) => {
            let pinned = runtime
                .block_on(engine.pin_kubeconfig(context.clone()))
                .context("the engine stopped")??;
            let options = LocalTerminalOptions::default()
                .with_env("KUBECONFIG", pinned.path().to_string_lossy())
                .with_cwd(WorkingDirectory::Home);
            let title = format!("{context}: local shell");
            // The pinned file is deleted when `pinned` drops with the window.
            let keep = Box::new((engine, pinned));
            pane::run_with(
                title,
                OnExit::Close,
                keep,
                on_quit,
                move |config, window, cx| TerminalState::local(options, config, window, cx),
            );
        }
    }
    Ok(())
}
