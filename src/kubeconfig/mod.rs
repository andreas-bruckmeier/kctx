//! Local kubeconfig discovery, parsing and the context model.
//!
//! This layer is entirely local: it reads and parses files and never opens a socket.
//! It is also strictly read-only with respect to the user's files — nothing here writes.

pub mod discovery;
pub mod parser;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use discovery::DiscoverySources;

/// How a context authenticates, as far as it can be told from the kubeconfig.
///
/// Only non-sensitive descriptors are kept: the auth-provider name and the exec command's
/// file name. Tokens, passwords, certificate data and exec args/env are never captured.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "kebab-case")]
pub enum AuthMethod {
    /// No credentials configured for this user entry (or no user entry at all).
    Unspecified,
    /// Inline bearer token.
    Token,
    /// Bearer token read from a file.
    TokenFile,
    /// Client certificate / key pair.
    ClientCertificate,
    /// HTTP basic auth.
    BasicAuth,
    /// Legacy `auth-provider` plugin, by provider name.
    AuthProvider(String),
    /// `exec` credential plugin, by command file name only.
    Exec(String),
}

impl std::fmt::Display for AuthMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unspecified => f.write_str("none"),
            Self::Token => f.write_str("token"),
            Self::TokenFile => f.write_str("token file"),
            Self::ClientCertificate => f.write_str("client certificate"),
            Self::BasicAuth => f.write_str("basic auth"),
            Self::AuthProvider(name) => write!(f, "auth provider ({name})"),
            Self::Exec(command) => write!(f, "exec plugin ({command})"),
        }
    }
}

/// A single context, resolved against the clusters and users of its own kubeconfig file.
#[derive(Debug, Clone)]
pub struct ContextEntry {
    /// Context name as written in the kubeconfig.
    pub name: String,
    /// Referenced cluster name.
    pub cluster: String,
    /// Referenced user name, if the context has one.
    pub user: Option<String>,
    /// Namespace configured on the context, if any.
    pub namespace: Option<String>,
    /// API server URL of the referenced cluster, if that cluster is defined in the same file.
    pub server: Option<String>,
    /// True when the context references a cluster that the file does not define.
    pub cluster_missing: bool,
    /// The kubeconfig file this context came from.
    pub source: Arc<Path>,
    /// True when this context is the `current-context` of its own file.
    pub current_in_source: bool,
    /// True when this is the effective context of the whole discovered chain.
    pub active: bool,
    /// True when another discovered file defines a context with the same name.
    pub ambiguous: bool,
    /// Authentication method descriptor (never contains secrets).
    pub auth_method: AuthMethod,
}

impl ContextEntry {
    /// The namespace requests should default to: the configured one, else `default`.
    pub fn effective_namespace(&self) -> &str {
        self.namespace.as_deref().unwrap_or("default")
    }
}

/// Something that went wrong with one kubeconfig file. Never fatal: other files still load.
#[derive(Debug, thiserror::Error)]
pub enum KubeconfigError {
    /// The file could not be opened or read.
    #[error("cannot read kubeconfig {}: {source}", .path.display())]
    Inaccessible {
        /// Offending file.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file could be read but is not valid kubeconfig YAML.
    #[error("invalid kubeconfig {}: {message}", .path.display())]
    Invalid {
        /// Offending file.
        path: PathBuf,
        /// Parser message (never contains file contents).
        message: String,
    },
    /// The file parsed as YAML but has neither contexts nor clusters.
    #[error("{} is not a kubeconfig", .path.display())]
    NotKubeconfig {
        /// Offending file.
        path: PathBuf,
    },
}

/// Everything discovered locally: contexts, which one is effective, and per-file problems.
#[derive(Debug, Default)]
pub struct ContextCatalog {
    /// All contexts, in discovery order.
    pub entries: Vec<ContextEntry>,
    /// Effective `current-context` name for the discovered chain, if any file sets one.
    pub active_name: Option<String>,
    /// Index into [`Self::entries`] of the effective context, if it is actually defined.
    pub active_index: Option<usize>,
    /// Files that failed to load. One broken file never blocks the others.
    pub problems: Vec<KubeconfigError>,
    /// Files that loaded successfully, in discovery order.
    pub sources: Vec<Arc<Path>>,
}

impl ContextCatalog {
    /// Load every discoverable kubeconfig and resolve the effective context.
    ///
    /// Follows client-go's merge semantics for `current-context`: the first file in the chain
    /// that sets it wins, and the first definition of a duplicated context name wins.
    pub fn load(sources: &DiscoverySources) -> Self {
        let mut catalog = Self::default();

        for path in sources.candidates() {
            match parser::load(&path) {
                Ok(loaded) => {
                    if catalog.active_name.is_none() {
                        catalog.active_name = loaded.current_context.clone();
                    }
                    catalog.sources.push(Arc::clone(&loaded.path));
                    catalog.entries.extend(loaded.entries);
                }
                Err(problem) => {
                    tracing::debug!(path = %path.display(), error = %problem, "skipping kubeconfig");
                    catalog.problems.push(problem);
                }
            }
        }

        catalog.mark_ambiguous();
        catalog.resolve_active();
        catalog
    }

    /// Flag every context name that occurs in more than one file.
    fn mark_ambiguous(&mut self) {
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for entry in &self.entries {
            *counts.entry(entry.name.as_str()).or_default() += 1;
        }
        let duplicated: std::collections::HashSet<String> = counts
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(name, _)| name.to_string())
            .collect();
        for entry in &mut self.entries {
            entry.ambiguous = duplicated.contains(&entry.name);
        }
    }

    /// Point `active_index` at the first definition of the effective context name.
    fn resolve_active(&mut self) {
        let Some(name) = self.active_name.clone() else {
            return;
        };
        self.active_index = self.entries.iter().position(|entry| entry.name == name);
        if let Some(index) = self.active_index {
            self.entries[index].active = true;
        }
    }

    /// All entries with the given context name, in discovery order.
    pub fn find_all(&self, name: &str) -> Vec<&ContextEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.name == name)
            .collect()
    }

    /// The effective context, if one is set and defined.
    pub fn active(&self) -> Option<&ContextEntry> {
        self.active_index.map(|index| &self.entries[index])
    }

    /// Context names similar enough to `name` to suggest as a typo correction.
    pub fn suggestions(&self, name: &str) -> Vec<&str> {
        let needle = name.to_lowercase();
        self.entries
            .iter()
            .map(|entry| entry.name.as_str())
            .filter(|candidate| {
                let candidate = candidate.to_lowercase();
                candidate.contains(&needle) || needle.contains(&candidate)
            })
            .collect()
    }
}
