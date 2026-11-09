//! Polls a context's metrics API and prints usage, like `kubectl top`.
//!
//! ```sh
//! cargo run -p resource-metrics --example top -- --context kind-platform
//! cargo run -p resource-metrics --example top -- --context NAME --namespace shop --rounds 5
//! ```
//!
//! Prints each snapshot: the status of each scope, node usage, the busiest
//! pods, and how much history the first node has built up.

#![allow(clippy::print_stdout, reason = "an example prints its findings")]

use std::error::Error;
use std::time::Duration;

use kube::config::KubeConfigOptions;
use model::{ContextName, Usage, UsageInterest, UsageObject, UsageScope, UsageSnapshot};
use resource_metrics::{PollConfig, UsagePoller};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut context = None;
    let mut namespace = None;
    let mut rounds = 3;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
        match arg.as_str() {
            "--context" => context = Some(value),
            "--namespace" => namespace = Some(value),
            "--rounds" => rounds = value.parse()?,
            other => return Err(format!("unknown flag {other}").into()),
        }
    }

    let options = KubeConfigOptions {
        context: context.clone(),
        ..KubeConfigOptions::default()
    };
    let config = kube::Config::from_kubeconfig(&options).await?;
    // Exec auth plugins run while the client is built; keep them off the runtime.
    let client = tokio::task::spawn_blocking(move || kube::Client::try_from(config)).await??;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let name = ContextName::new(context.unwrap_or_else(|| "current".to_owned()));
    let poller = UsagePoller::start(client, name, PollConfig::default(), move |snapshot| {
        tx.send(snapshot).is_ok()
    });
    let _nodes = poller.subscribe(UsageInterest::Latest(UsageScope::Nodes));
    let _pods = poller.subscribe(UsageInterest::Latest(UsageScope::Pods(
        namespace.map(Into::into),
    )));

    let mut chart = None;
    for round in 1..=rounds {
        let Some(snapshot) = rx.recv().await else {
            break;
        };
        // Follow the first node's history once it is known.
        if chart.is_none()
            && let Some(node) = snapshot
                .latest
                .keys()
                .find(|o| matches!(o, UsageObject::Node(_)))
        {
            chart = Some(poller.subscribe(UsageInterest::History(node.clone())));
        }
        print(round, &snapshot);
    }
    Ok(())
}

fn print(round: u32, snapshot: &UsageSnapshot) {
    println!("\nround {round}");
    for (scope, status) in &snapshot.status {
        println!("  {scope:?}: {status:?}");
    }
    let mut pods: Vec<(&UsageObject, &Usage)> = Vec::new();
    for (object, usage) in &snapshot.latest {
        match object {
            UsageObject::Node(name) => println!("  node {name}: {}", format_usage(usage)),
            UsageObject::Pod { .. } => pods.push((object, usage)),
        }
    }
    pods.sort_by(|a, b| b.1.cpu.value.total_cmp(&a.1.cpu.value));
    println!("  {} pods, busiest:", pods.len());
    for (object, usage) in pods.iter().take(5) {
        if let UsageObject::Pod { namespace, name } = object {
            println!("    {namespace}/{name}: {}", format_usage(usage));
        }
    }
    for history in &snapshot.history {
        let span = match (history.cpu.samples.first(), history.cpu.samples.last()) {
            (Some(first), Some(last)) => last
                .time
                .duration_since(first.time)
                .unwrap_or(Duration::ZERO),
            _ => Duration::ZERO,
        };
        println!(
            "  history {:?}: {} samples over {} s",
            history.object,
            history.cpu.samples.len(),
            span.as_secs()
        );
    }
}

fn format_usage(usage: &Usage) -> String {
    format!(
        "{:.0}m CPU, {:.0}Mi memory",
        usage.cpu.value * 1000.0,
        usage.memory.value / 1024.0 / 1024.0
    )
}
