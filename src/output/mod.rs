//! Machine-readable and human-readable renderers for the CLI.
//!
//! Everything produced here goes to stdout and is therefore part of kctx's contract with shell
//! scripts. The TUI and all diagnostics use stderr instead.

use serde::Serialize;

use crate::kubeconfig::{AuthMethod, ContextCatalog, ContextEntry};
use crate::kubernetes::ConnectionState;
use crate::kubernetes::health::ReplicaStats;
use crate::kubernetes::inspection::{Availability, Inspection};

/// Marker used for "not configured" in tabular output. `-` is not a legal Kubernetes name,
/// so it can never be confused with real data.
const UNSET: &str = "-";

/// A context as exposed by `--json`. Deliberately contains no credential-derived field.
#[derive(Debug, Serialize)]
pub struct ContextJson<'a> {
    /// Context name.
    pub name: &'a str,
    /// Cluster name referenced by the context.
    pub cluster: &'a str,
    /// User name referenced by the context, if any.
    pub user: Option<&'a str>,
    /// Namespace configured on the context, if any.
    pub namespace: Option<&'a str>,
    /// API server URL, if the cluster is defined.
    pub server: Option<&'a str>,
    /// Absolute path of the kubeconfig file defining the context.
    pub source: String,
    /// Whether this is the effective context.
    pub active: bool,
    /// Whether the defining file marks it as its own `current-context`.
    pub current_in_source: bool,
    /// Whether the name is defined by more than one discovered file.
    pub ambiguous: bool,
    /// Authentication method descriptor.
    pub auth: &'a AuthMethod,
}

impl<'a> From<&'a ContextEntry> for ContextJson<'a> {
    fn from(entry: &'a ContextEntry) -> Self {
        Self {
            name: &entry.name,
            cluster: &entry.cluster,
            user: entry.user.as_deref(),
            namespace: entry.namespace.as_deref(),
            server: entry.server.as_deref(),
            source: entry.source.display().to_string(),
            active: entry.active,
            current_in_source: entry.current_in_source,
            ambiguous: entry.ambiguous,
            auth: &entry.auth_method,
        }
    }
}

/// `kctx list` default output: one tab-separated record per context.
///
/// Columns are `name`, `cluster`, `namespace`, `source`, `active` — stable, unpadded and free of
/// headers so `cut -f1` and `awk -F'\t'` work directly.
pub fn list_tsv(catalog: &ContextCatalog) -> String {
    let mut out = String::new();
    for entry in &catalog.entries {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            entry.name,
            entry.cluster,
            entry.namespace.as_deref().unwrap_or(UNSET),
            entry.source.display(),
            if entry.active { "*" } else { UNSET },
        ));
    }
    out
}

/// `kctx list --json` output.
pub fn list_json(catalog: &ContextCatalog) -> serde_json::Result<String> {
    let contexts: Vec<ContextJson<'_>> = catalog.entries.iter().map(ContextJson::from).collect();
    serde_json::to_string_pretty(&serde_json::json!({ "contexts": contexts }))
}

/// `kctx current --json` output.
pub fn current_json(entry: Option<&ContextEntry>, name: &str) -> serde_json::Result<String> {
    match entry {
        Some(entry) => serde_json::to_string_pretty(&ContextJson::from(entry)),
        // The chain names a context that no discovered file defines.
        None => {
            serde_json::to_string_pretty(&serde_json::json!({ "name": name, "defined": false }))
        }
    }
}

/// Render a snapshot as indented plain text.
///
/// Deliberately free of ANSI colour: this goes to stdout, where it may be piped into a file or
/// another program. The TUI does its own styling.
pub fn inspection_text(snapshot: &Inspection) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}\n\n", snapshot.context));

    out.push_str("Context\n");
    row(&mut out, "Cluster", &snapshot.cluster);
    row(
        &mut out,
        "Server",
        snapshot.server.as_deref().unwrap_or("unknown"),
    );
    row(&mut out, "Namespace", &snapshot.namespace);
    // The authentication *method*, never the credential.
    row(&mut out, "Auth", &snapshot.identity.to_string());
    row(&mut out, "Status", &status_line(snapshot));
    match &snapshot.version {
        Availability::Ok(version) => {
            row(
                &mut out,
                "Kubernetes",
                &format!("{} ({})", version.git_version, version.platform),
            );
        }
        other => row(&mut out, "Kubernetes", &other.describe()),
    }
    if let Some(latency) = snapshot.latency {
        row(&mut out, "Latency", &format!("{} ms", latency.as_millis()));
    }

    out.push_str("\nNodes\n");
    match &snapshot.nodes {
        Availability::Ok(nodes) => {
            row(
                &mut out,
                "Count",
                &format!(
                    "{} total, {} Ready, {} control plane",
                    nodes.total, nodes.ready, nodes.control_plane
                ),
            );
            if !nodes.versions.is_empty() {
                row(&mut out, "Versions", &nodes.versions.join(", "));
            }
            if !nodes.platforms.is_empty() {
                row(&mut out, "Platforms", &nodes.platforms.join(", "));
            }
        }
        other => note(&mut out, &other.describe()),
    }

    out.push_str(&format!("\nNamespace {}\n", snapshot.namespace));
    match &snapshot.namespace_status {
        Availability::Ok(namespace) => {
            row(
                &mut out,
                "Phase",
                namespace.phase.as_deref().unwrap_or(UNSET),
            );
        }
        other => row(&mut out, "Phase", &other.describe()),
    }
    workload_rows(&mut out, snapshot);

    out.push_str("\nProblems\n");
    if snapshot.observations.is_empty() {
        note(&mut out, problems_placeholder(snapshot));
    } else {
        for observation in &snapshot.observations {
            out.push_str(&format!("  {}\n", observation.text));
        }
    }

    out
}

/// One `Kind  summary` line per workload kind, including the unavailable ones.
fn workload_rows(out: &mut String, snapshot: &Inspection) {
    let workloads = &snapshot.workloads;

    let pods = match &workloads.pods {
        Availability::Ok(pods) => {
            let mut text = format!("{} total, {} Ready", pods.total, pods.ready);
            if pods.pending > 0 {
                text.push_str(&format!(", {} Pending", pods.pending));
            }
            if pods.failed > 0 {
                text.push_str(&format!(", {} Failed", pods.failed));
            }
            text
        }
        other => other.describe(),
    };
    row(out, "Pods", &pods);

    row(
        out,
        "Deployments",
        &replicas(&workloads.deployments, "available"),
    );
    row(
        out,
        "StatefulSets",
        &replicas(&workloads.statefulsets, "ready"),
    );

    let daemonsets = match &workloads.daemonsets {
        Availability::Ok(stats) => {
            format!(
                "{} total, {}/{} Pods ready",
                stats.total, stats.ready, stats.desired
            )
        }
        other => other.describe(),
    };
    row(out, "DaemonSets", &daemonsets);

    let jobs = match &workloads.jobs {
        Availability::Ok(stats) => format!(
            "{} total, {} active, {} complete, {} failed",
            stats.total, stats.active, stats.complete, stats.failed
        ),
        other => other.describe(),
    };
    row(out, "Jobs", &jobs);

    let cronjobs = match &workloads.cronjobs {
        Availability::Ok(stats) => {
            format!("{} total, {} suspended", stats.total, stats.suspended)
        }
        other => other.describe(),
    };
    row(out, "CronJobs", &cronjobs);
}

/// `n total, a/d available replicas` for the replica-shaped controllers.
fn replicas(section: &Availability<ReplicaStats>, quality: &str) -> String {
    match section {
        Availability::Ok(stats) => format!(
            "{} total, {}/{} {quality} replicas",
            stats.total, stats.available, stats.desired
        ),
        other => other.describe(),
    }
}

/// Connection state plus its detail, if any.
fn status_line(snapshot: &Inspection) -> String {
    match &snapshot.message {
        Some(message) => format!("{} ({message})", snapshot.state),
        None => snapshot.state.to_string(),
    }
}

/// `  label   value`, with the label padded to a fixed width.
fn row(out: &mut String, label: &str, value: &str) {
    out.push_str(&format!("  {label:<13}{value}\n"));
}

/// An indented line with no label column.
fn note(out: &mut String, text: &str) {
    out.push_str(&format!("  {text}\n"));
}

/// What to say when there are no observations.
///
/// "none observed" would be a lie for a cluster that was never read — the absence of problems is
/// only meaningful when something actually answered.
pub fn problems_placeholder(snapshot: &Inspection) -> &'static str {
    if snapshot.state == ConnectionState::Connected {
        "none observed"
    } else {
        "not determined: nothing could be read"
    }
}

/// `kctx inspect --json` output.
pub fn inspection_json(snapshot: &Inspection) -> serde_json::Result<String> {
    serde_json::to_string_pretty(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubeconfig::ContextEntry;
    use std::path::Path;
    use std::sync::Arc;

    fn entry(name: &str, namespace: Option<&str>, active: bool) -> ContextEntry {
        ContextEntry {
            name: name.to_string(),
            cluster: format!("{name}-cluster"),
            user: Some(format!("{name}-user")),
            namespace: namespace.map(str::to_string),
            server: Some("https://example.com".to_string()),
            cluster_missing: false,
            source: Arc::from(Path::new("/home/u/.kube/config")),
            current_in_source: active,
            active,
            ambiguous: false,
            auth_method: AuthMethod::Token,
        }
    }

    fn catalog() -> ContextCatalog {
        ContextCatalog {
            entries: vec![
                entry("prod", Some("payments"), true),
                entry("staging", None, false),
            ],
            active_name: Some("prod".to_string()),
            active_index: Some(0),
            ..ContextCatalog::default()
        }
    }

    #[test]
    fn tsv_has_stable_columns_and_no_header() {
        let rendered = list_tsv(&catalog());
        let lines: Vec<&str> = rendered.lines().collect();

        assert_eq!(
            lines[0],
            "prod\tprod-cluster\tpayments\t/home/u/.kube/config\t*"
        );
        assert_eq!(
            lines[1],
            "staging\tstaging-cluster\t-\t/home/u/.kube/config\t-"
        );
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn json_shape_is_stable() {
        let rendered = list_json(&catalog()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let first = &value["contexts"][0];

        assert_eq!(first["name"], "prod");
        assert_eq!(first["namespace"], "payments");
        assert_eq!(first["active"], true);
        assert_eq!(first["auth"]["kind"], "token");
        assert_eq!(value["contexts"][1]["namespace"], serde_json::Value::Null);
    }

    #[test]
    fn current_json_reports_undefined_contexts() {
        let rendered = current_json(None, "ghost").unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["name"], "ghost");
        assert_eq!(value["defined"], false);
    }
}

#[cfg(test)]
mod inspection_tests {
    use super::*;
    use crate::kubeconfig::{AuthMethod, ContextEntry};
    use crate::kubernetes::health::{NodeSummary, Observation, PodStats, Severity};
    use crate::kubernetes::inspection::ServerVersion;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    fn entry() -> ContextEntry {
        ContextEntry {
            name: "production-eu".to_string(),
            cluster: "prod-cluster".to_string(),
            user: Some("prod-user".to_string()),
            namespace: Some("payments".to_string()),
            server: Some("https://prod.example.com".to_string()),
            cluster_missing: false,
            source: Arc::from(Path::new("/k/prod.yaml")),
            current_in_source: true,
            active: true,
            ambiguous: false,
            auth_method: AuthMethod::Exec("aws".to_string()),
        }
    }

    fn connected(entry: &ContextEntry) -> Inspection {
        let mut snapshot = Inspection::pending(entry, "payments");
        snapshot.state = ConnectionState::Connected;
        snapshot.latency = Some(Duration::from_millis(34));
        snapshot.version = Availability::Ok(ServerVersion {
            git_version: "v1.30.2".to_string(),
            platform: "linux/amd64".to_string(),
        });
        snapshot.nodes = Availability::Ok(NodeSummary {
            total: 5,
            ready: 5,
            control_plane: 1,
            versions: vec!["v1.30.2".to_string()],
            platforms: vec!["linux/amd64".to_string()],
            truncated: false,
        });
        snapshot.workloads.pods = Availability::Ok(PodStats {
            total: 42,
            ready: 39,
            pending: 1,
            ..PodStats::default()
        });
        snapshot.workloads.deployments = Availability::Denied;
        snapshot.observations = vec![Observation {
            severity: Severity::Problem,
            text: "2 Pods not Ready (api-1, api-2)".to_string(),
        }];
        snapshot
    }

    #[test]
    fn inspection_text_covers_every_section() {
        let entry = entry();
        let rendered = inspection_text(&connected(&entry));

        for expected in [
            "production-eu",
            "  Cluster      prod-cluster",
            "  Server       https://prod.example.com",
            "  Auth         exec plugin (aws)",
            "  Status       Connected",
            "  Kubernetes   v1.30.2 (linux/amd64)",
            "  Latency      34 ms",
            "  Count        5 total, 5 Ready, 1 control plane",
            "Namespace payments",
            "  Pods         42 total, 39 Ready, 1 Pending",
            "  StatefulSets not checked",
            "Problems",
            "  2 Pods not Ready (api-1, api-2)",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in:\n{rendered}"
            );
        }
    }

    #[test]
    fn forbidden_sections_are_labelled_rather_than_omitted() {
        let entry = entry();
        let rendered = inspection_text(&connected(&entry));

        assert!(
            rendered.contains("  Deployments  unavailable (permission denied)"),
            "{rendered}"
        );
    }

    #[test]
    fn plain_text_output_carries_no_ansi_escapes() {
        let entry = entry();
        let rendered = inspection_text(&connected(&entry));

        assert!(
            !rendered.contains('\u{1b}'),
            "stdout output must stay pipe-friendly"
        );
    }

    #[test]
    fn an_unreached_cluster_does_not_claim_to_be_problem_free() {
        let entry = entry();
        let mut snapshot = Inspection::pending(&entry, "payments");
        snapshot.state = ConnectionState::Unavailable;
        snapshot.message = Some("cluster is unreachable: connection refused".to_string());

        let rendered = inspection_text(&snapshot);

        assert!(rendered.contains("Status       Unavailable"), "{rendered}");
        assert!(rendered.contains("connection refused"), "{rendered}");
        assert!(rendered.contains("not determined"), "{rendered}");
        assert!(!rendered.contains("none observed"), "{rendered}");
    }

    #[test]
    fn json_output_is_machine_readable_and_credential_free() {
        let entry = entry();
        let rendered = inspection_json(&connected(&entry)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["state"], "connected");
        assert_eq!(value["latency"], 34);
        assert_eq!(value["identity"]["kind"], "exec");
        assert_eq!(value["identity"]["detail"], "aws");
        assert_eq!(value["nodes"]["status"], "ok");
        assert_eq!(value["nodes"]["detail"]["ready"], 5);
        assert_eq!(value["workloads"]["deployments"]["status"], "denied");
        assert_eq!(value["observations"][0]["severity"], "problem");
    }
}
