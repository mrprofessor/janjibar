//! A fake cluster for running the app without Kubernetes.
//!
//! Serves pods and a custom resource (`crontabs.stable.example.com`) over
//! plain HTTP, writes a kubeconfig that points at it, and adds then deletes a
//! pod every few seconds so the UI has something to show. Pods write log
//! lines all the time, and `shop-app-0000` crash-loops: it dies every 15
//! seconds and comes back 3 seconds later.
//!
//! ```sh
//! nix develop -c cargo run -p engine --example fake_cluster -- --pods 41 --kubeconfig /tmp/fake.yaml
//! KUBECONFIG=/tmp/fake.yaml nix develop -c cargo run -p app -- --context fake
//! ```

#![expect(
    clippy::print_stdout,
    reason = "a dev tool that tells the user where it listens"
)]

#[path = "../tests/support/fake_api.rs"]
mod fake_api;

use std::path::PathBuf;
use std::time::Duration;

use fake_api::FakeApi;
use k8s_openapi::jiff::{Timestamp, ToSpan as _};

/// How often some pod writes a log line.
const LOG_EVERY: Duration = Duration::from_millis(200);
/// How often the crash-looping pod dies.
const CRASH_EVERY: Duration = Duration::from_secs(15);
/// How long it stays down.
const RESTART_AFTER: Duration = Duration::from_secs(3);
/// The pod that crash-loops.
const CRASHER: &str = "shop-app-0000";

/// A plausible log line, with a level now and then.
fn message(pod: usize, n: usize) -> String {
    let id = 7000 + (pod * 131 + n * 17) % 3000;
    let ms = 3 + (pod * 7 + n * 13) % 90;
    match n % 11 {
        0 => format!("INFO cache refreshed in {ms}ms, {id} keys"),
        3 => format!("level=warn msg=\"slow upstream\" upstream=payments latency={ms}ms"),
        7 => format!(
            r#"{{"level":"error","msg":"request failed","order":"ord_{id}","err":"timeout"}}"#
        ),
        9 => format!("DEBUG pool size=8 idle={}", n % 8),
        _ => format!("GET /v1/cart/{id} 200 {ms}ms"),
    }
}

struct Args {
    port: u16,
    pods: usize,
    churn: Duration,
    kubeconfig: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        port: 18_443,
        pods: 41,
        churn: Duration::from_secs(3),
        kubeconfig: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--port" => args.port = value()?.parse().map_err(|e| format!("--port: {e}"))?,
            "--pods" => args.pods = value()?.parse().map_err(|e| format!("--pods: {e}"))?,
            "--churn-secs" => {
                let secs: u64 = value()?.parse().map_err(|e| format!("--churn-secs: {e}"))?;
                args.churn = Duration::from_secs(secs);
            }
            "--kubeconfig" => args.kubeconfig = Some(PathBuf::from(value()?)),
            other => {
                return Err(format!(
                    "unknown argument `{other}`. Flags: --port N --pods N --churn-secs N --kubeconfig PATH"
                ));
            }
        }
    }
    Ok(args)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    let args = parse_args()?;
    let api = FakeApi::start(&format!("127.0.0.1:{}", args.port))
        .await
        .map_err(|e| format!("could not listen on port {}: {e}", args.port))?;

    let namespaces = ["shop", "payments", "platform", "kube-system"];
    for i in 0..args.pods {
        let ns = namespaces[i % namespaces.len()];
        let age = 60 * i64::try_from(i * 97 % 6000).unwrap_or(0);
        let name = format!("{ns}-app-{i:04}");
        api.add_pod_with(ns, &name, age, &[("app", ns)], &["app"]);
        // A backlog, so a new log tab has a tail to show.
        let now = Timestamp::now();
        for j in 0..40i64 {
            let at = now - (40 - j).seconds() - 250.milliseconds();
            api.log_at(
                ns,
                &name,
                "app",
                at,
                &message(i, usize::try_from(j).unwrap_or(0)),
            );
        }
    }
    api.add_crontab("batch-jobs", "nightly", "0 2 * * *", Some(1), 5 * 24 * 3600);
    api.add_crontab("batch-jobs", "every-minute", "* * * * */1", None, 120);

    let kubeconfig = format!(
        "apiVersion: v1\nkind: Config\ncurrent-context: fake\nclusters:\n  - name: fake\n    cluster:\n      server: \"{}\"\nusers:\n  - name: fake\n    user: {{}}\ncontexts:\n  - name: fake\n    context:\n      cluster: fake\n      user: fake\n",
        api.url()
    );
    if let Some(path) = &args.kubeconfig {
        std::fs::write(path, &kubeconfig)
            .map_err(|e| format!("could not write {}: {e}", path.display()))?;
        println!("kubeconfig written to {}", path.display());
    }
    println!(
        "fake cluster at {} with {} pods",
        api.url(),
        api.pod_count()
    );

    // Add a pod, then delete it on the next tick, forever. Meanwhile pods
    // log and one pod crash-loops.
    let mut tick = tokio::time::interval(args.churn);
    let mut log_tick = tokio::time::interval(LOG_EVERY);
    let mut crash_tick = tokio::time::interval(CRASH_EVERY);
    crash_tick.tick().await;
    let mut n = 0u64;
    let mut live: Option<String> = None;
    let mut lines = 0usize;
    let mut crashed_at: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = log_tick.tick() => {
                lines += 1;
                let i = lines % args.pods.clamp(1, 8);
                let ns = namespaces[i % namespaces.len()];
                api.log(ns, &format!("{ns}-app-{i:04}"), "app", &message(i, lines));
                if crashed_at.is_some_and(|t| t.elapsed() >= RESTART_AFTER) {
                    crashed_at = None;
                    api.start_container("shop", CRASHER, "app");
                    api.log("shop", CRASHER, "app", "starting app v2.31.0 on :8080");
                    println!("restarted shop/{CRASHER}");
                }
                continue;
            }
            _ = crash_tick.tick() => {
                api.log("shop", CRASHER, "app", "fatal error: runtime: out of memory");
                api.crash("shop", CRASHER, "app");
                crashed_at = Some(tokio::time::Instant::now());
                println!("crashed shop/{CRASHER}");
                continue;
            }
        }
        if let Some(name) = live.take() {
            api.delete_pod("shop", &name);
            println!("deleted shop/{name} ({} pods)", api.pod_count());
        } else {
            n += 1;
            let name = format!("churn-{n}");
            api.add_pod("shop", &name, 0);
            println!("added shop/{name} ({} pods)", api.pod_count());
            live = Some(name);
        }
    }
}
