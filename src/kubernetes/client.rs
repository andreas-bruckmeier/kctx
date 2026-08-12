//! Turning a discovered context into a configured, read-only Kubernetes client.
//!
//! Credentials are handled entirely by `kube`: certificate loading, bearer tokens, exec
//! credential plugins and TLS all use the established client implementation rather than anything
//! hand-rolled here. kctx only chooses *which* context to load and imposes timeouts.

use std::time::Duration;

use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};

use super::InspectError;
use crate::kubeconfig::ContextEntry;

/// Timeouts applied to every cluster interaction.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// TCP + TLS connection establishment.
    pub connect: Duration,
    /// Waiting for a single response.
    pub request: Duration,
    /// Upper bound for a whole inspection, however many requests it makes.
    pub overall: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(3),
            request: Duration::from_secs(5),
            overall: Duration::from_secs(8),
        }
    }
}

impl Timeouts {
    /// Scale the defaults to an overall budget, keeping the per-request limits proportionate.
    pub fn with_overall(overall: Duration) -> Self {
        let default = Self::default();
        Self {
            connect: default.connect.min(overall),
            request: default.request.min(overall),
            overall,
        }
    }
}

/// A configured client plus the facts kctx wants to display about it.
///
/// `kube::Client` deliberately hides its configuration, so the server URL and the resolved
/// namespace are captured here while they are still available.
pub struct Connection {
    /// The client itself.
    pub client: Client,
    /// API server URL the client will talk to.
    pub server: String,
    /// Namespace the context resolved to.
    pub namespace: String,
}

/// Build a client for `entry` from the kubeconfig file that defines it.
///
/// Only that one file is read, so a context is always resolved against the clusters and users it
/// was written next to, independent of whatever else is on `$KUBECONFIG`.
pub async fn connect(entry: &ContextEntry, timeouts: Timeouts) -> Result<Connection, InspectError> {
    let kubeconfig = Kubeconfig::read_from(entry.source.as_ref())
        .map_err(|error| InspectError::Config(error.to_string()))?;

    let options = KubeConfigOptions {
        context: Some(entry.name.clone()),
        cluster: None,
        user: None,
    };
    let mut config = Config::from_custom_kubeconfig(kubeconfig, &options)
        .await
        .map_err(|error| InspectError::Config(error.to_string()))?;

    config.connect_timeout = Some(timeouts.connect);
    config.read_timeout = Some(timeouts.request);
    config.write_timeout = Some(timeouts.request);
    // Retrying transient failures would multiply the time a broken cluster keeps the user
    // waiting; the inspection reports the failure instead.
    config.default_retry = false;

    let server = config.cluster_url.to_string();
    let namespace = config.default_namespace.clone();
    tracing::debug!(
        context = %entry.name,
        %server,
        %namespace,
        "built client configuration"
    );

    let client = Client::try_from(config).map_err(|error| super::classify(&error))?;
    Ok(Connection {
        client,
        server,
        namespace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubeconfig::parser;
    use std::path::Path;

    fn write(dir: &Path, contents: &str) -> std::path::PathBuf {
        let path = dir.join("config");
        std::fs::write(&path, contents).unwrap();
        path
    }

    const CONFIG: &str = r#"
apiVersion: v1
kind: Config
current-context: other
clusters:
  - name: c
    cluster:
      server: https://cluster.example.com:6443
      insecure-skip-tls-verify: true
users:
  - name: u
    user:
      token: secret-token
contexts:
  - name: wanted
    context:
      cluster: c
      user: u
      namespace: chosen
  - name: other
    context:
      cluster: c
      user: u
"#;

    /// Building a client must not contact anything, so this is safe offline.
    #[tokio::test]
    async fn uses_the_requested_context_not_the_files_current_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), CONFIG);
        let loaded = parser::load(&path).unwrap();
        let wanted = loaded
            .entries
            .iter()
            .find(|entry| entry.name == "wanted")
            .unwrap();

        let connection = connect(wanted, Timeouts::default()).await.unwrap();

        assert_eq!(connection.namespace, "chosen");
        assert_eq!(connection.server, "https://cluster.example.com:6443/");
        assert_eq!(connection.client.default_namespace(), "chosen");
    }

    #[tokio::test]
    async fn a_context_with_an_undefined_cluster_fails_as_a_config_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "apiVersion: v1\nkind: Config\ncontexts:\n  - name: broken\n    context:\n      cluster: missing\n",
        );
        let loaded = parser::load(&path).unwrap();

        let error = match connect(&loaded.entries[0], Timeouts::default()).await {
            Ok(_) => panic!("a context without a cluster must not produce a client"),
            Err(error) => error,
        };

        assert!(matches!(error, InspectError::Config(_)), "{error:?}");
    }

    #[test]
    fn overall_budget_never_exceeds_the_per_request_limits() {
        let timeouts = Timeouts::with_overall(Duration::from_secs(1));
        assert_eq!(timeouts.connect, Duration::from_secs(1));
        assert_eq!(timeouts.request, Duration::from_secs(1));
        assert_eq!(timeouts.overall, Duration::from_secs(1));
    }
}
