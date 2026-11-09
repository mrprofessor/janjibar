//! Lists any kind through discovery and server-side tables, from a terminal.
//!
//! Reads only. Useful to check the generic path against a real cluster.
//!
//! ```sh
//! nix develop -c cargo run -p engine --example table -- --context kind-platform pods
//! nix develop -c cargo run -p engine --example table -- --context kind-platform -n faults po --watch
//! ```

#![expect(
    clippy::print_stdout,
    reason = "a dev tool that prints tables to the terminal"
)]

use std::fmt::Write as _;

use engine::discovery::discover;
use engine::tables::{TableQuery, watch_table};
use kube::config::KubeConfigOptions;
use model::{ConnectionState, Event, TableSnapshot};
use tokio::sync::mpsc;

struct Args {
    context: Option<String>,
    namespace: Option<String>,
    kind: String,
    watch: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut context = None;
    let mut namespace = None;
    let mut kind = None;
    let mut watch = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--context" => context = Some(value()?),
            "-n" | "--namespace" => namespace = Some(value()?),
            "--watch" | "-w" => watch = true,
            other if !other.starts_with('-') => kind = Some(other.to_owned()),
            other => {
                return Err(format!(
                    "unknown argument `{other}`. Usage: [--context NAME] [-n NAMESPACE] [--watch] KIND"
                ));
            }
        }
    }
    Ok(Args {
        context,
        namespace,
        kind: kind.ok_or("name a kind, like `pods` or `deploy`")?,
        watch,
    })
}

fn print(snapshot: &TableSnapshot) {
    let mut out = String::new();
    let namespaced =
        snapshot.namespace.is_none() && snapshot.rows.iter().any(|r| r.namespace.is_some());
    if namespaced {
        out.push_str("NAMESPACE\t");
    }
    let header: Vec<String> = snapshot
        .columns
        .iter()
        .map(|c| c.name.to_uppercase())
        .collect();
    out.push_str(&header.join("\t"));
    for row in snapshot.rows.iter() {
        out.push('\n');
        if namespaced {
            let _ = write!(out, "{}\t", row.namespace.as_deref().unwrap_or(""));
        }
        let cells: Vec<&str> = row.cells.iter().map(AsRef::as_ref).collect();
        out.push_str(&cells.join("\t"));
    }
    println!("{out}\n({} rows)\n", snapshot.rows.len());
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let args = parse_args()?;
    let options = KubeConfigOptions {
        context: args.context.clone(),
        ..KubeConfigOptions::default()
    };
    let config = kube::Config::from_kubeconfig(&options)
        .await
        .map_err(|e| format!("kubeconfig: {e}"))?;
    // Exec auth plugins run inside `try_from`; keep them off the async workers.
    let client = tokio::task::spawn_blocking(move || kube::Client::try_from(config))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("client: {e}"))?;
    let context = args.context.unwrap_or_else(|| "current".to_owned());

    let catalog = discover(&context.as_str().into(), &client)
        .await
        .map_err(|e| e.to_string())?;
    println!(
        "{} kinds in {} groups; {} problems",
        catalog.kinds().count(),
        catalog.groups.len(),
        catalog.problems.len()
    );
    for problem in &catalog.problems {
        println!("  {}: {}", problem.group_version, problem.message);
    }
    let kind = catalog
        .find(&args.kind)
        .ok_or(format!("no kind named `{}`", args.kind))?;
    println!("listing {} ({})\n", kind.kind, kind.resource);

    let query = match &args.namespace {
        Some(ns) => TableQuery::in_namespace(kind, ns.as_str()),
        None => TableQuery::all(kind),
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let watch = tokio::spawn(watch_table(context.as_str().into(), client, query, tx));
    while let Some(event) = rx.recv().await {
        let Event::Table(snapshot) = event else {
            continue;
        };
        match &snapshot.state {
            ConnectionState::Connecting => continue,
            ConnectionState::Failed(err) => return Err(err.to_string()),
            ConnectionState::Retrying(err) => println!("retrying: {err}"),
            ConnectionState::Live => print(&snapshot),
        }
        if !args.watch {
            break;
        }
    }
    watch.abort();
    Ok(())
}
