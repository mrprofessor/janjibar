//! A kubeconfig pinned to one context, for a local cluster shell.
//!
//! The file holds that context, its cluster and its user, copied from the
//! merged kubeconfig, with the context current. `kubectl`, `helm` and the
//! rest in a shell with `KUBECONFIG` set to it reach that cluster only.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use kube::config::Kubeconfig;
use model::{ContextName, PinnedKubeconfig, ShellError};
use tokio::sync::oneshot;

/// Writes the pinned file, sends it back, and deletes it once the
/// [`PinnedKubeconfig`] is dropped.
pub(super) async fn pin(
    merged: Arc<Kubeconfig>,
    context: ContextName,
    reply: oneshot::Sender<Result<PinnedKubeconfig, ShellError>>,
) {
    let pinned = match pinned(&merged, &context) {
        Ok(pinned) => pinned,
        Err(err) => {
            // The requester may have given up; then nobody is waiting.
            let _ = reply.send(Err(err));
            return;
        }
    };
    // Writing a file is blocking I/O; keep it off the async workers.
    let written = tokio::task::spawn_blocking(move || write(&pinned)).await;
    let file = match written {
        Ok(Ok(file)) => file,
        Ok(Err(err)) => {
            let _ = reply.send(Err(ShellError::Other(format!(
                "could not write the shell's kubeconfig: {err}"
            ))));
            return;
        }
        Err(err) => {
            let _ = reply.send(Err(ShellError::Other(format!(
                "writing the shell's kubeconfig was interrupted: {err}"
            ))));
            return;
        }
    };
    let (release, released) = oneshot::channel();
    let path = PathBuf::from(file.as_os_str());
    if reply
        .send(Ok(PinnedKubeconfig::new(path, context, release)))
        .is_err()
    {
        // Nobody took the file; `file` deletes it on the way out.
        return;
    }
    // Resolves when the PinnedKubeconfig is dropped. If the runtime shuts
    // down first, this task is dropped, and so is `file`.
    let _ = released.await;
    if let Err(err) = file.close() {
        tracing::warn!("could not delete a shell's kubeconfig: {err}");
    }
}

/// The parts of `merged` that `context` needs, with `context` current.
fn pinned(merged: &Kubeconfig, context: &ContextName) -> Result<Kubeconfig, ShellError> {
    let named = merged
        .contexts
        .iter()
        .find(|named| named.name == context.as_str())
        .ok_or_else(|| ShellError::NotFound(format!("context `{context}` is not in kubeconfig")))?;
    let entry = named
        .context
        .as_ref()
        .ok_or_else(|| ShellError::Other(format!("context `{context}` has no cluster or user")))?;
    let cluster = merged
        .clusters
        .iter()
        .find(|cluster| cluster.name == entry.cluster)
        .ok_or_else(|| {
            ShellError::NotFound(format!(
                "cluster `{}` of context `{context}` is not in kubeconfig",
                entry.cluster
            ))
        })?;
    let user = match &entry.user {
        Some(user) => vec![
            merged
                .auth_infos
                .iter()
                .find(|info| &info.name == user)
                .ok_or_else(|| {
                    ShellError::NotFound(format!(
                        "user `{user}` of context `{context}` is not in kubeconfig"
                    ))
                })?
                .clone(),
        ],
        None => Vec::new(),
    };
    Ok(Kubeconfig {
        api_version: Some("v1".to_owned()),
        kind: Some("Config".to_owned()),
        preferences: merged.preferences.clone(),
        clusters: vec![cluster.clone()],
        auth_infos: user,
        contexts: vec![named.clone()],
        current_context: Some(context.as_str().to_owned()),
        ..Kubeconfig::default()
    })
}

/// Writes `kubeconfig` as YAML to a new file that only the user can read.
fn write(kubeconfig: &Kubeconfig) -> std::io::Result<tempfile::TempPath> {
    let yaml = serde_saphyr::to_string(kubeconfig).map_err(std::io::Error::other)?;
    // New temporary files are created with mode 0600.
    let mut file = tempfile::Builder::new()
        .prefix("kubeconfig-")
        .suffix(".yaml")
        .tempfile()?;
    file.write_all(yaml.as_bytes())?;
    file.flush()?;
    Ok(file.into_temp_path())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const MERGED: &str = r"
apiVersion: v1
kind: Config
current-context: dev
clusters:
- name: dev-cluster
  cluster:
    server: https://dev.example:6443
- name: prod-cluster
  cluster:
    server: https://prod.example:6443
users:
- name: dev-user
  user:
    token: dev-token
- name: prod-user
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1
      command: kubelogin
      args: [get-token]
contexts:
- name: dev
  context:
    cluster: dev-cluster
    user: dev-user
- name: prod
  context:
    cluster: prod-cluster
    user: prod-user
    namespace: payments
";

    fn merged() -> Kubeconfig {
        Kubeconfig::from_yaml(MERGED).unwrap()
    }

    #[test]
    fn only_the_pinned_context_its_cluster_and_its_user_are_kept() {
        let pinned = pinned(&merged(), &ContextName::new("prod")).unwrap();
        assert_eq!(pinned.current_context.as_deref(), Some("prod"));
        let names = |items: Vec<&str>| items.join(",");
        assert_eq!(
            names(pinned.contexts.iter().map(|c| c.name.as_str()).collect()),
            "prod"
        );
        assert_eq!(
            names(pinned.clusters.iter().map(|c| c.name.as_str()).collect()),
            "prod-cluster"
        );
        assert_eq!(
            names(pinned.auth_infos.iter().map(|u| u.name.as_str()).collect()),
            "prod-user"
        );
        let context = pinned.contexts[0].context.as_ref().unwrap();
        assert_eq!(context.namespace.as_deref(), Some("payments"));
    }

    #[test]
    fn an_unknown_context_or_a_dangling_reference_is_an_error() {
        assert!(matches!(
            pinned(&merged(), &ContextName::new("nope")),
            Err(ShellError::NotFound(_))
        ));
        let mut broken = merged();
        broken.clusters.retain(|c| c.name != "dev-cluster");
        assert!(matches!(
            pinned(&broken, &ContextName::new("dev")),
            Err(ShellError::NotFound(_))
        ));
    }

    #[test]
    fn the_file_reads_back_with_its_credentials_and_only_the_user_can_read_it() {
        let file = write(&pinned(&merged(), &ContextName::new("dev")).unwrap()).unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let read = Kubeconfig::read_from(&file).unwrap();
        assert_eq!(read.current_context.as_deref(), Some("dev"));
        assert_eq!(read.auth_infos.len(), 1);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("dev-token"),
            "the token is written out:\n{text}"
        );
    }

    #[tokio::test]
    async fn dropping_the_pinned_kubeconfig_deletes_the_file() {
        let (reply, receiver) = oneshot::channel();
        let task = tokio::spawn(pin(Arc::new(merged()), ContextName::new("dev"), reply));
        let pinned = receiver.await.unwrap().unwrap();
        let path = pinned.path().to_owned();
        assert!(path.exists());
        drop(pinned);
        task.await.unwrap();
        assert!(!path.exists());
    }
}
