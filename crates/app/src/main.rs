//! The desktop app binary.
//!
//! Startup order matters:
//! 1. Copy PATH from the login shell, while this is the only thread.
//! 2. Start the tokio runtime and the engine on it.
//! 3. Start GPUI on the main thread and open the window.

mod shell_env;

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{
    App, Bounds, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions, point, prelude::*,
    px, size,
};
use model::{Command, ContextName, brand};
use tracing_subscriber::EnvFilter;

struct Args {
    /// Connect to this context at startup.
    context: Option<String>,
    /// Open logs for this pod (`namespace/name`) at startup.
    logs: Option<(String, String)>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        context: None,
        logs: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--context" => {
                args.context = Some(it.next().ok_or("--context needs a context name")?);
            }
            "--logs" => {
                let pod = it.next().ok_or("--logs needs NAMESPACE/POD")?;
                let (ns, name) = pod.split_once('/').ok_or("--logs needs NAMESPACE/POD")?;
                args.logs = Some((ns.to_owned(), name.to_owned()));
            }
            other => {
                if let Some(name) = other.strip_prefix("--context=") {
                    args.context = Some(name.to_owned());
                } else {
                    return Err(format!(
                        "unknown argument `{other}`. Usage: [--context NAME [--logs NAMESPACE/POD]]"
                    ));
                }
            }
        }
    }
    Ok(args)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = parse_args().map_err(anyhow::Error::msg)?;

    shell_env::sync_path_from_login_shell();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("engine")
        .enable_all()
        .build()?;
    let (engine, events) = engine::start(runtime.handle(), engine::KubeconfigSources::from_env());
    let handle = runtime.handle().clone();

    gpui_kit::application()
        .with_assets(ui::assets::Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            ui::init(cx);
            bridge::gpui_tokio::init_from_handle(cx, handle);

            cx.on_action(|_: &ui::Quit, cx| cx.quit());
            cx.set_menus(vec![Menu {
                name: brand::APP_NAME.into(),
                items: vec![
                    MenuItem::action("Reload kubeconfig", ui::ReloadKubeconfig),
                    MenuItem::separator(),
                    MenuItem::action(format!("Quit {}", brand::APP_NAME), ui::Quit),
                ],
                disabled: false,
            }]);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let send: ui::CommandSink = Rc::new(move |command: Command| {
                if !engine.send(command) {
                    tracing::error!("the engine has stopped; command dropped");
                }
            });
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1360.0), px(840.0)),
                    cx,
                ))),
                window_min_size: Some(size(px(800.0), px(480.0))),
                titlebar: Some(TitlebarOptions {
                    title: Some(brand::APP_NAME.into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(16.0), px(16.0))),
                }),
                app_id: Some(brand::BUNDLE_ID.to_owned()),
                ..WindowOptions::default()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| ui::Workspace::new(brand::APP_NAME, send, events, window, cx))
            });
            match opened {
                Ok((window, workspace)) => {
                    let weak = workspace.downgrade();
                    cx.on_action(move |_: &ui::ReloadKubeconfig, cx| {
                        // A failed update means the window is gone.
                        let _ = weak.update(cx, ui::Workspace::reload);
                    });
                    if let Some(name) = args.context {
                        workspace.update(cx, |w, cx| w.select(ContextName::new(name), cx));
                        if let Some((ns, pod)) = args.logs {
                            let opened = window.update(cx, |_, window, cx| {
                                workspace.update(cx, |w, cx| w.open_pod_logs(ns, pod, window, cx));
                            });
                            if let Err(err) = opened {
                                tracing::warn!("could not open logs at startup: {err:#}");
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::error!("could not open the main window: {err:#}");
                    cx.quit();
                }
            }
            cx.activate(true);
        });

    runtime.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}
