//! Runs one port-forward through the engine, from a terminal.
//!
//! Useful to check forwarding against a real cluster. Nothing is saved.
//!
//! ```sh
//! nix develop -c cargo run -p engine --example forward -- --context kind-platform gateway/svc/gateway 8080:80
//! curl http://127.0.0.1:8080/
//! ```

#![expect(
    clippy::print_stdout,
    reason = "a dev tool that prints the forward's state to the terminal"
)]

use engine::KubeconfigSources;
use model::{Command, ContextName, Event, ForwardSpec};

fn parse_args() -> Result<(ContextName, String), String> {
    let mut context = None;
    let mut rest = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        if arg == "--context" {
            context = Some(it.next().ok_or("--context needs a value")?);
        } else {
            rest.push(arg);
        }
    }
    let context =
        context.ok_or("usage: --context NAME [namespace/][pod/|svc/]name [local:]port")?;
    Ok((context.as_str().into(), rest.join(" ")))
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let (context, forward) = parse_args()?;
    let spec = ForwardSpec::parse(&forward, &context, "default").map_err(|e| e.to_string())?;
    let (engine, mut events) = engine::start(
        &tokio::runtime::Handle::current(),
        KubeconfigSources::from_env(),
    );
    let mut sent = false;
    while let Some(event) = events.recv().await {
        match event {
            Event::Catalog(_) if !sent => {
                sent = true;
                engine.send(Command::StartForward(spec.clone()));
            }
            Event::Forwards(rows) => {
                for row in rows.iter() {
                    let addr = row
                        .local_addr
                        .map_or_else(|| "-".to_owned(), |a| a.to_string());
                    let last = row
                        .last_error
                        .as_deref()
                        .map(|e| format!(" error: {e}"))
                        .or_else(|| row.last_pod.as_deref().map(|p| format!(" last pod: {p}")))
                        .unwrap_or_default();
                    println!(
                        "{addr} -> {}/{} {}  {:?}  open {} total {}{last}",
                        row.spec.namespace,
                        row.spec.target,
                        row.spec.port,
                        row.state,
                        row.active,
                        row.total
                    );
                }
            }
            _ => {}
        }
    }
    Ok(())
}
