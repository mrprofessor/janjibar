//! Shells through the engine's public handle, against a fake exec server.
//! No cluster needed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

mod support {
    pub mod fake_exec;
}

use std::time::Duration;

use engine::{EngineHandle, KubeconfigSources};
use model::{
    ConnectError, ContextName, Event, ShellError, ShellEvent, ShellExit, ShellInput, ShellSession,
    ShellTarget,
};
use support::fake_exec::FakeExec;
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

/// An engine whose only context, `fake`, points at `server`. The
/// kubeconfig file lives as long as the returned guard.
async fn engine_for(server: &FakeExec) -> (EngineHandle, tempfile::TempPath) {
    let kubeconfig = format!(
        "apiVersion: v1
kind: Config
current-context: fake
clusters:
- name: fake
  cluster:
    server: {}
users:
- name: fake
  user:
    token: not-checked
contexts:
- name: fake
  context:
    cluster: fake
    user: fake
",
        server.url()
    );
    let path = tempfile::Builder::new()
        .suffix(".yaml")
        .tempfile()
        .unwrap()
        .into_temp_path();
    std::fs::write(&path, kubeconfig).unwrap();
    let sources = KubeconfigSources::new(vec![path.to_path_buf()]);
    let (handle, mut events) = engine::start(&tokio::runtime::Handle::current(), sources);
    // Requests are served in order, so once the catalog is in, shells for
    // `fake` find their context.
    wait_for_catalog(&mut events).await;
    tokio::spawn(async move { while events.recv().await.is_some() {} });
    (handle, path)
}

async fn wait_for_catalog(events: &mut mpsc::UnboundedReceiver<Event>) {
    timeout(WAIT, async {
        loop {
            if let Some(Event::Catalog(_)) = events.recv().await {
                return;
            }
        }
    })
    .await
    .expect("no catalog");
}

fn pod(name: &str) -> ShellTarget {
    ShellTarget::Pod {
        context: ContextName::new("fake"),
        namespace: "shop".to_owned(),
        pod: name.to_owned(),
        container: Some("app".to_owned()),
    }
}

/// Reads events until the output so far contains `text`.
async fn wait_for_output(session: &mut ShellSession, text: &str) -> String {
    let mut output = String::new();
    timeout(WAIT, async {
        while !output.contains(text) {
            match session.events.recv().await {
                Some(ShellEvent::Output(bytes)) => {
                    output.push_str(&String::from_utf8_lossy(&bytes));
                }
                Some(ShellEvent::Progress(_)) => {}
                other => panic!("expected output, got {other:?}; output so far: {output:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {text:?} in {output:?}"));
    output
}

/// The event that ends the session, skipping output.
async fn last_event(session: &mut ShellSession) -> ShellEvent {
    timeout(WAIT, async {
        loop {
            match session
                .events
                .recv()
                .await
                .expect("the session closed without an end")
            {
                ShellEvent::Output(_) | ShellEvent::Progress(_) => {}
                end => return end,
            }
        }
    })
    .await
    .expect("the session did not end")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pod_shell_echoes_resizes_and_reports_its_exit_code() {
    let server = FakeExec::start().await.unwrap();
    let (engine, _kubeconfig) = engine_for(&server).await;
    let mut session = engine.open_shell(pod("web-0"));

    session
        .input
        .send(ShellInput::Resize {
            columns: 100,
            rows: 30,
        })
        .unwrap();
    wait_for_output(&mut session, "[size 100x30]").await;

    session
        .input
        .send(ShellInput::Bytes(b"echo hi\r".to_vec()))
        .unwrap();
    wait_for_output(&mut session, "echo hi").await;

    session
        .input
        .send(ShellInput::Bytes(b"exit 7\r".to_vec()))
        .unwrap();
    assert_eq!(
        last_event(&mut session).await,
        ShellEvent::Exited(ShellExit {
            code: Some(7),
            message: None
        })
    );

    let url = server.urls().pop().unwrap();
    assert!(
        url.starts_with("/api/v1/namespaces/shop/pods/web-0/exec?"),
        "{url}"
    );
    for part in [
        "container=app",
        "stdin=true",
        "stdout=true",
        "tty=true",
        "command=sh",
    ] {
        assert!(url.contains(part), "{part} missing from {url}");
    }
    assert!(!url.contains("stderr=true"), "a TTY merges stderr: {url}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_exit_is_code_zero() {
    let server = FakeExec::start().await.unwrap();
    let (engine, _kubeconfig) = engine_for(&server).await;
    let mut session = engine.open_shell(pod("web-0"));
    wait_for_output(&mut session, "$ ").await;
    session
        .input
        .send(ShellInput::Bytes(b"exit 0\r".to_vec()))
        .unwrap();
    assert_eq!(
        last_event(&mut session).await,
        ShellEvent::Exited(ShellExit {
            code: Some(0),
            message: None
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_become_errors_the_user_can_act_on() {
    let server = FakeExec::start().await.unwrap();
    let (engine, _kubeconfig) = engine_for(&server).await;

    let mut forbidden = engine.open_shell(pod("secret"));
    assert!(matches!(
        last_event(&mut forbidden).await,
        ShellEvent::Failed(ShellError::Forbidden(message)) if message.contains("pods/exec")
    ));

    let mut missing = engine.open_shell(pod("missing"));
    assert!(matches!(
        last_event(&mut missing).await,
        ShellEvent::Failed(ShellError::NotFound(_))
    ));

    let mut distroless = engine.open_shell(pod("distroless"));
    assert!(matches!(
        last_event(&mut distroless).await,
        ShellEvent::Failed(ShellError::NoShell(_))
    ));

    let mut unknown = engine.open_shell(ShellTarget::Pod {
        context: ContextName::new("gone"),
        namespace: "shop".to_owned(),
        pod: "web-0".to_owned(),
        container: None,
    });
    assert_eq!(
        last_event(&mut unknown).await,
        ShellEvent::Failed(ShellError::Connect(ConnectError::UnknownContext))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_input_ends_the_shell() {
    let server = FakeExec::start().await.unwrap();
    let (engine, _kubeconfig) = engine_for(&server).await;
    let ShellSession { input, mut events } = engine.open_shell(pod("web-0"));
    timeout(WAIT, events.recv()).await.unwrap().unwrap();
    drop(input);
    // The engine stops and drops its end; nothing but leftovers follow.
    timeout(WAIT, async {
        while let Some(event) = events.recv().await {
            assert!(matches!(event, ShellEvent::Output(_)), "{event:?}");
        }
    })
    .await
    .expect("the engine kept the shell open");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_kubeconfig_holds_the_context_and_goes_away_when_dropped() {
    let server = FakeExec::start().await.unwrap();
    let (engine, _kubeconfig) = engine_for(&server).await;
    let pinned = timeout(WAIT, engine.pin_kubeconfig(ContextName::new("fake")))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let path = pinned.path().to_owned();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(&server.url()), "{text}");
    assert!(text.contains("current-context: fake"), "{text}");

    drop(pinned);
    timeout(WAIT, async {
        while path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the file outlived its PinnedKubeconfig");

    let unknown = timeout(WAIT, engine.pin_kubeconfig(ContextName::new("gone")))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(unknown, Err(ShellError::NotFound(_))));
}
