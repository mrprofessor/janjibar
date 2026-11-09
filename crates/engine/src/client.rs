//! One kube-rs client per context, built on first use.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};
use model::{ConnectError, ContextName};
use tokio::sync::OnceCell;

use crate::errors::connect_error_from_client;

/// How long to wait for a TCP and TLS connection to the API server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Caches one client per context.
///
/// Clients are cheap to clone and safe to share. Each context has a
/// once-cell, so concurrent callers wait for one build instead of running
/// the exec plugin twice. The cache is cleared when the kubeconfig is
/// reloaded, since a context's credentials may have changed.
#[derive(Clone, Default)]
pub struct ClientPool {
    clients: Arc<Mutex<HashMap<ContextName, Arc<OnceCell<Client>>>>>,
}

impl ClientPool {
    /// Returns the cached client for `context`, or builds one.
    ///
    /// Building runs on a blocking thread, because `Client::try_from` runs
    /// exec auth plugins synchronously and they can take seconds. A failed
    /// build is not cached; the next call tries again.
    pub async fn get(
        &self,
        context: &ContextName,
        kubeconfig: &Kubeconfig,
    ) -> Result<Client, ConnectError> {
        let cell = Arc::clone(self.lock().entry(context.clone()).or_default());
        cell.get_or_try_init(|| build(context, kubeconfig))
            .await
            .cloned()
    }

    /// Drops every cached client.
    pub fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ContextName, Arc<OnceCell<Client>>>> {
        // The map stays consistent even if a holder panicked mid-insert.
        self.clients.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

async fn build(context: &ContextName, kubeconfig: &Kubeconfig) -> Result<Client, ConnectError> {
    let options = KubeConfigOptions {
        context: Some(context.as_str().to_owned()),
        ..KubeConfigOptions::default()
    };
    let exec_command = exec_command(kubeconfig, context);
    let mut config = Config::from_custom_kubeconfig(kubeconfig.clone(), &options)
        .await
        .map_err(|e| ConnectError::Config(e.to_string()))?;
    config.connect_timeout = Some(CONNECT_TIMEOUT);
    tracing::debug!(%context, "building client");
    tokio::task::spawn_blocking(move || Client::try_from(config))
        .await
        .map_err(|e| ConnectError::Other(format!("client setup was interrupted: {e}")))?
        .map_err(|e| connect_error_from_client(&e, exec_command.as_deref()))
}

/// The exec plugin command of the context's user, for error messages.
fn exec_command(kubeconfig: &Kubeconfig, context: &ContextName) -> Option<String> {
    let user = kubeconfig
        .contexts
        .iter()
        .find(|c| c.name == context.as_str())?
        .context
        .as_ref()?
        .user
        .clone()?;
    kubeconfig
        .auth_infos
        .iter()
        .find(|a| a.name == user)?
        .auth_info
        .as_ref()?
        .exec
        .as_ref()?
        .command
        .clone()
}
