//! Turning a kubeconfig file into [`ContextEntry`] values.
//!
//! Parsing is delegated to `kube`'s [`Kubeconfig`], which handles multi-document YAML and
//! rewrites relative certificate/token/exec paths to absolute ones. We never read or write
//! kubeconfig bytes ourselves, and we never touch the file.

use std::path::Path;
use std::sync::Arc;

use kube::config::{AuthInfo, Kubeconfig, KubeconfigError as KubeError};

use super::{AuthMethod, ContextEntry, KubeconfigError};

/// One successfully parsed kubeconfig file.
#[derive(Debug)]
pub struct LoadedKubeconfig {
    /// The file it was read from.
    pub path: Arc<Path>,
    /// `current-context` as set by this file, if any.
    pub current_context: Option<String>,
    /// Contexts defined by this file.
    pub entries: Vec<ContextEntry>,
}

/// Read and parse a single kubeconfig file.
pub fn load(path: &Path) -> Result<LoadedKubeconfig, KubeconfigError> {
    let kubeconfig = Kubeconfig::read_from(path).map_err(|error| classify(path, error))?;

    if kubeconfig.contexts.is_empty() && kubeconfig.clusters.is_empty() {
        return Err(KubeconfigError::NotKubeconfig {
            path: path.to_path_buf(),
        });
    }

    let path: Arc<Path> = Arc::from(path);
    Ok(LoadedKubeconfig {
        entries: entries_from(&kubeconfig, &path),
        current_context: kubeconfig.current_context.clone(),
        path,
    })
}

/// Map a `kube` parse failure onto our error taxonomy.
fn classify(path: &Path, error: KubeError) -> KubeconfigError {
    match error {
        KubeError::ReadConfig(source, _) => KubeconfigError::Inaccessible {
            path: path.to_path_buf(),
            source,
        },
        // `Parse` and the merge errors all mean "this file is not usable as written".
        other => KubeconfigError::Invalid {
            path: path.to_path_buf(),
            message: summarise(&other.to_string()),
        },
    }
}

/// Reduce a parser message to its first line.
///
/// The YAML parser helpfully appends an excerpt of the offending document, which may quote a
/// line holding a token or a certificate. Position information is kept, file content is not.
fn summarise(message: &str) -> String {
    message
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Resolve every context in `kubeconfig` against its clusters and users.
pub fn entries_from(kubeconfig: &Kubeconfig, path: &Arc<Path>) -> Vec<ContextEntry> {
    kubeconfig
        .contexts
        .iter()
        .filter_map(|named| {
            let context = named.context.as_ref()?;
            let cluster = kubeconfig
                .clusters
                .iter()
                .find(|candidate| candidate.name == context.cluster)
                .and_then(|candidate| candidate.cluster.as_ref());
            let auth_info = context.user.as_ref().and_then(|user| {
                kubeconfig
                    .auth_infos
                    .iter()
                    .find(|candidate| candidate.name == *user)
                    .and_then(|candidate| candidate.auth_info.as_ref())
            });

            Some(ContextEntry {
                name: named.name.clone(),
                cluster: context.cluster.clone(),
                user: context.user.clone(),
                namespace: context.namespace.clone().filter(|ns| !ns.is_empty()),
                server: cluster.and_then(|cluster| cluster.server.clone()),
                cluster_missing: cluster.is_none(),
                source: Arc::clone(path),
                current_in_source: kubeconfig.current_context.as_deref()
                    == Some(named.name.as_str()),
                active: false,
                ambiguous: false,
                auth_method: auth_method(auth_info),
            })
        })
        .collect()
}

/// Describe how a user entry authenticates, without capturing any secret material.
///
/// The precedence mirrors what a client actually uses: exec and auth-provider plugins take
/// over credential acquisition, then bearer tokens, then client certificates, then basic auth.
fn auth_method(auth_info: Option<&AuthInfo>) -> AuthMethod {
    let Some(auth_info) = auth_info else {
        return AuthMethod::Unspecified;
    };

    if let Some(exec) = &auth_info.exec {
        // Only the command's file name is kept: args and env routinely carry secrets.
        let command = exec
            .command
            .as_deref()
            .map(|command| {
                Path::new(command).file_name().map_or_else(
                    || command.to_string(),
                    |name| name.to_string_lossy().into_owned(),
                )
            })
            .unwrap_or_else(|| "unknown".to_string());
        return AuthMethod::Exec(command);
    }
    if let Some(provider) = &auth_info.auth_provider {
        return AuthMethod::AuthProvider(provider.name.clone());
    }
    if auth_info.token.is_some() {
        return AuthMethod::Token;
    }
    if auth_info.token_file.is_some() {
        return AuthMethod::TokenFile;
    }
    if auth_info.client_certificate.is_some() || auth_info.client_certificate_data.is_some() {
        return AuthMethod::ClientCertificate;
    }
    if auth_info.username.is_some() {
        return AuthMethod::BasicAuth;
    }
    AuthMethod::Unspecified
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    const MULTI: &str = r#"
apiVersion: v1
kind: Config
current-context: prod
clusters:
  - name: prod-cluster
    cluster:
      server: https://prod.example.com
  - name: staging-cluster
    cluster:
      server: https://staging.example.com
users:
  - name: prod-user
    user:
      token: super-secret-token
  - name: staging-user
    user:
      exec:
        apiVersion: client.authentication.k8s.io/v1beta1
        command: /usr/local/bin/aws
        args: ["eks", "get-token", "--role", "arn:secret"]
contexts:
  - name: prod
    context:
      cluster: prod-cluster
      user: prod-user
      namespace: payments
  - name: staging
    context:
      cluster: staging-cluster
      user: staging-user
  - name: orphan
    context:
      cluster: gone-cluster
      user: prod-user
"#;

    #[test]
    fn parses_multiple_contexts_clusters_and_users() {
        let (_dir, path) = write(MULTI);
        let loaded = load(&path).unwrap();

        assert_eq!(loaded.current_context.as_deref(), Some("prod"));
        assert_eq!(loaded.entries.len(), 3);

        let prod = &loaded.entries[0];
        assert_eq!(prod.name, "prod");
        assert_eq!(prod.cluster, "prod-cluster");
        assert_eq!(prod.user.as_deref(), Some("prod-user"));
        assert_eq!(prod.namespace.as_deref(), Some("payments"));
        assert_eq!(prod.server.as_deref(), Some("https://prod.example.com"));
        assert!(prod.current_in_source);
        assert_eq!(prod.auth_method, AuthMethod::Token);
    }

    #[test]
    fn contexts_without_namespace_default_lazily() {
        let (_dir, path) = write(MULTI);
        let loaded = load(&path).unwrap();
        let staging = &loaded.entries[1];

        assert_eq!(staging.namespace, None);
        assert_eq!(staging.effective_namespace(), "default");
        assert!(!staging.current_in_source);
    }

    #[test]
    fn exec_auth_records_only_the_command_file_name() {
        let (_dir, path) = write(MULTI);
        let loaded = load(&path).unwrap();

        assert_eq!(
            loaded.entries[1].auth_method,
            AuthMethod::Exec("aws".into())
        );
        // Nothing derived from the entry may leak the exec args.
        let rendered = format!("{:?}", loaded.entries[1]);
        assert!(!rendered.contains("arn:secret"), "{rendered}");
        assert!(!rendered.contains("eks"), "{rendered}");
    }

    #[test]
    fn secrets_never_reach_the_context_model() {
        let (_dir, path) = write(MULTI);
        let loaded = load(&path).unwrap();
        let rendered = format!("{:?}", loaded.entries);
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
    }

    #[test]
    fn missing_cluster_reference_is_flagged_not_fatal() {
        let (_dir, path) = write(MULTI);
        let loaded = load(&path).unwrap();
        let orphan = &loaded.entries[2];

        assert!(orphan.cluster_missing);
        assert_eq!(orphan.server, None);
    }

    #[test]
    fn malformed_yaml_is_reported_as_invalid() {
        let (_dir, path) = write("clusters: [\n  - name: broken\n    cluster: {server\n");
        let error = load(&path).unwrap_err();

        match error {
            KubeconfigError::Invalid { path: reported, .. } => assert_eq!(reported, path),
            other => panic!("expected an invalid-kubeconfig error, got {other:?}"),
        }
    }

    #[test]
    fn parse_errors_never_quote_the_file_contents() {
        // The YAML parser appends an excerpt around the failure, which can sit right next to a
        // credential. The reported message must keep the position and drop the excerpt.
        let (_dir, path) = write(
            "apiVersion: v1\nkind: Config\nusers:\n  - name: u\n    user:\n      token: SUPER-SECRET-TOKEN\n      broken: [\n",
        );
        let error = load(&path).unwrap_err();
        let rendered = format!("{error}");

        assert!(!rendered.contains("SUPER-SECRET-TOKEN"), "{rendered}");
        assert!(
            !rendered.contains('\n'),
            "message must stay a single line: {rendered}"
        );
        assert!(
            rendered.contains("line"),
            "position information is still useful: {rendered}"
        );
    }

    #[test]
    fn missing_file_is_reported_as_inaccessible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent");
        let error = load(&path).unwrap_err();

        assert!(
            matches!(error, KubeconfigError::Inaccessible { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn yaml_without_contexts_or_clusters_is_not_a_kubeconfig() {
        let (_dir, path) = write("some: unrelated\nyaml: document\n");
        let error = load(&path).unwrap_err();

        assert!(
            matches!(error, KubeconfigError::NotKubeconfig { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn multi_document_files_are_merged() {
        let (_dir, path) = write(
            r#"
apiVersion: v1
kind: Config
clusters:
  - name: one
    cluster:
      server: https://one.example.com
contexts:
  - name: first
    context:
      cluster: one
---
apiVersion: v1
kind: Config
contexts:
  - name: second
    context:
      cluster: one
"#,
        );
        let loaded = load(&path).unwrap();
        let names: Vec<&str> = loaded
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();

        assert_eq!(names, vec!["first", "second"]);
    }

    #[test]
    fn relative_certificate_paths_are_absolutised_by_kube() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            "apiVersion: v1\nkind: Config\nclusters:\n  - name: c\n    cluster:\n      server: https://x\n      certificate-authority: ca.crt\ncontexts:\n  - name: ctx\n    context:\n      cluster: c\n",
        )
        .unwrap();

        let kubeconfig = Kubeconfig::read_from(&path).unwrap();
        let ca = kubeconfig.clusters[0]
            .cluster
            .as_ref()
            .unwrap()
            .certificate_authority
            .clone()
            .unwrap();
        assert!(Path::new(&ca).is_absolute(), "{ca}");
    }
}
