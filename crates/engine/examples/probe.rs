//! Runs the engine headless against a real context and prints what the UI
//! would show: a row count and a status summary for every kind, then the
//! inspector's view of one pod with its owner chain. Read-only.
//!
//! ```sh
//! nix develop -c cargo run -p engine --example probe -- kind-platform [namespace/pod]
//! ```

#![expect(clippy::print_stdout, reason = "a dev tool that prints its findings")]

use std::collections::BTreeMap;
use std::time::Duration;

use engine::KubeconfigSources;
use model::resource::{InspectTarget, Kind, ObjectKey};
use model::{Command, ConnectionState, Event, Health, SubscriptionId};

#[tokio::main]
async fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let context: model::ContextName = args
        .next()
        .ok_or("usage: probe CONTEXT [namespace/pod]")?
        .as_str()
        .into();
    let pod = args.next();

    let (engine, mut events) = engine::start(
        &tokio::runtime::Handle::current(),
        KubeconfigSources::from_env(),
    );
    engine.send(Command::SelectContext(context.clone()));
    for (i, kind) in Kind::ALL.into_iter().enumerate() {
        engine.send(Command::Subscribe {
            id: SubscriptionId(i as u64),
            context: context.clone(),
            resource: kind.resource(),
            namespace: None,
        });
    }

    let mut seen: BTreeMap<Kind, String> = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while seen.len() < Kind::ALL.len() {
        let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await else {
            break;
        };
        match event {
            Event::Connection { state, .. } => println!("connection: {state:?}"),
            Event::Kinds(catalog) => {
                let all = catalog.kinds().count();
                let untyped = catalog
                    .kinds()
                    .filter(|k| Kind::from_resource(&k.resource).is_none())
                    .count();
                println!("discovery: {all} kinds, {untyped} without a typed view (issue #19)");
            }
            Event::Table(t) => {
                let Some(kind) = Kind::from_resource(&t.resource) else {
                    continue;
                };
                match &t.state {
                    ConnectionState::Failed(err) => {
                        seen.insert(kind, format!("FAILED: {err}"));
                        continue;
                    }
                    ConnectionState::Connecting => continue,
                    ConnectionState::Live | ConnectionState::Retrying(_) => {}
                }
                let mut by_health: BTreeMap<String, usize> = BTreeMap::new();
                for row in t.rows.iter() {
                    if row.health != Health::Unknown {
                        *by_health.entry(format!("{:?}", row.health)).or_default() += 1;
                    }
                }
                let bad: Vec<String> = t
                    .rows
                    .iter()
                    .filter(|r| r.health == Health::Bad)
                    .take(4)
                    .map(|r| {
                        let ns = r.namespace.as_deref().unwrap_or("-");
                        format!("{ns}/{} ({})", r.name, r.cells.join(" | "))
                    })
                    .collect();
                let line = if bad.is_empty() {
                    format!("{} rows {by_health:?}", t.rows.len())
                } else {
                    format!(
                        "{} rows {by_health:?}\n      bad: {}",
                        t.rows.len(),
                        bad.join("\n           ")
                    )
                };
                seen.insert(kind, line);
            }
            _ => {}
        }
    }
    for kind in Kind::ALL {
        let line = seen.get(&kind).map_or("no snapshot", String::as_str);
        println!("{:<16} {line}", kind.plural());
    }

    if let Some(pod) = pod {
        let (ns, name) = pod.split_once('/').ok_or("pod must be namespace/name")?;
        let target = InspectTarget {
            context,
            kind: Kind::Pod,
            namespace: None,
            key: ObjectKey::namespaced(ns, name),
        };
        engine.send(Command::Inspect(Some(target)));
        let mut got = (false, false);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !(got.0 && got.1) {
            let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await else {
                break;
            };
            match event {
                Event::Details { details, .. } if !got.0 => {
                    got.0 = true;
                    match details {
                        Some(d) => {
                            println!(
                                "\ninspect {pod}: {} {:?} {:?}",
                                d.status, d.health, d.badges
                            );
                            for f in &d.fields {
                                println!("  {:<16} {}", f.label, f.value);
                            }
                            for c in &d.containers {
                                println!(
                                    "  container {} [{}] {} restarts={}",
                                    c.name, c.image, c.state, c.restarts
                                );
                            }
                            for s in &d.sections {
                                println!("  {}:", s.title);
                                for f in &s.fields {
                                    println!("    {:<20} {}", f.label, f.value);
                                }
                            }
                        }
                        None => println!("\ninspect {pod}: not found"),
                    }
                }
                Event::Owners { chain, .. } if !got.1 => {
                    got.1 = true;
                    let chain: Vec<String> = chain
                        .iter()
                        .map(|o| format!("{} {}", o.kind, o.name))
                        .collect();
                    println!("  owners: {}", chain.join(" > "));
                }
                _ => {}
            }
        }
    }
    Ok(())
}
