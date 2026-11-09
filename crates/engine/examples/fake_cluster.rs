//! A fake cluster for running the app without Kubernetes.
//!
//! Serves pods and a custom resource (`crontabs.stable.example.com`) over
//! plain HTTP, writes a kubeconfig that points at it, and adds then deletes a
//! pod every few seconds so the UI has something to show.
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
        api.add_pod(ns, &format!("{ns}-app-{i:04}"), age);
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

    // Add a pod, then delete it on the next tick, forever.
    let mut tick = tokio::time::interval(args.churn);
    let mut n = 0u64;
    let mut live: Option<String> = None;
    loop {
        tick.tick().await;
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
