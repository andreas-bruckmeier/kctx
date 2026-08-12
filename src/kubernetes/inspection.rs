//! Collecting a read-only snapshot of a context's cluster.
//!
//! Two properties matter here:
//!
//! * **Nothing happens until asked.** Building the context list never calls this module, so
//!   startup stays local and instant.
//! * **Partial results are normal.** Every section is fetched independently and degrades to
//!   [`Availability::Denied`] or [`Availability::Unavailable`] on its own, so read-only
//!   credentials that cannot list Nodes still get Pod information.

use std::time::{Duration, Instant};

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::core::v1::{Namespace, Node, Pod};
use serde::Serialize;

use super::client::Connection;
use super::health::{
    CronJobStats, DaemonSetStats, JobStats, NamespaceSummary, NodeSummary, Observation, PodStats,
    ReplicaStats, Severity,
};
use super::{ConnectionState, InspectError, classify, client, health};
use crate::kubeconfig::{AuthMethod, ContextEntry};

/// Upper bound on objects fetched per kind. Inspection is a glance, not an inventory.
const LIST_LIMIT: u32 = 500;

/// Whether a section of the snapshot could be filled in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "kebab-case")]
pub enum Availability<T> {
    /// The data was read successfully.
    Ok(T),
    /// The credentials are not allowed to read it (HTTP 403).
    Denied,
    /// The read failed for another reason.
    Unavailable(String),
    /// Not attempted, because the cluster could not be reached at all.
    #[default]
    NotChecked,
}

impl<T> Availability<T> {
    /// Classify a failed read: permission problems are expected and reported as such.
    fn from_error(error: &kube::Error) -> Self {
        let classified = classify(error);
        if classified.is_permission_denied() {
            Self::Denied
        } else {
            Self::Unavailable(classified.to_string())
        }
    }

    /// The value, if it was read.
    ///
    /// Renderers match on the whole enum so they can say *why* something is missing; this is
    /// the convenience the tests need.
    #[cfg(test)]
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Ok(value) => Some(value),
            _ => None,
        }
    }

    /// How to describe this section to a reader — shared by the CLI and the TUI so both explain
    /// a missing section the same way.
    pub fn describe(&self) -> String {
        match self {
            Self::Ok(_) => "available".to_string(),
            Self::Denied => "unavailable (permission denied)".to_string(),
            Self::Unavailable(message) => format!("unavailable ({message})"),
            Self::NotChecked => "not checked".to_string(),
        }
    }
}

/// Server version as reported by `/version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerVersion {
    /// e.g. `v1.30.2`.
    pub git_version: String,
    /// e.g. `linux/amd64`.
    pub platform: String,
}

/// Per-kind workload summaries for the inspected namespace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Workloads {
    /// Pods.
    pub pods: Availability<PodStats>,
    /// Deployments.
    pub deployments: Availability<ReplicaStats>,
    /// StatefulSets.
    pub statefulsets: Availability<ReplicaStats>,
    /// DaemonSets.
    pub daemonsets: Availability<DaemonSetStats>,
    /// Jobs.
    pub jobs: Availability<JobStats>,
    /// CronJobs.
    pub cronjobs: Availability<CronJobStats>,
}

/// A read-only snapshot of one context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Inspection {
    /// Context name.
    pub context: String,
    /// Cluster name from the kubeconfig.
    pub cluster: String,
    /// API server URL.
    pub server: Option<String>,
    /// Namespace the workload sections refer to.
    pub namespace: String,
    /// How the context authenticates (never a credential itself).
    pub identity: AuthMethod,
    /// Outcome of the connection attempt.
    pub state: ConnectionState,
    /// Detail for a non-`Connected` state.
    pub message: Option<String>,
    /// Round-trip time of the `/version` request.
    #[serde(serialize_with = "serialize_millis")]
    pub latency: Option<Duration>,
    /// Kubernetes version of the API server.
    pub version: Availability<ServerVersion>,
    /// Cluster nodes.
    pub nodes: Availability<NodeSummary>,
    /// The inspected namespace itself.
    pub namespace_status: Availability<NamespaceSummary>,
    /// Workloads in the inspected namespace.
    pub workloads: Workloads,
    /// Everything notable that was observed, most severe first.
    pub observations: Vec<Observation>,
}

/// Latency is far more readable as milliseconds than as a duration struct.
fn serialize_millis<S: serde::Serializer>(
    value: &Option<Duration>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(duration) => serializer.serialize_some(&duration.as_millis()),
        None => serializer.serialize_none(),
    }
}

impl Inspection {
    /// A snapshot that has not been attempted yet, used for the `Connecting...` state.
    pub fn pending(entry: &ContextEntry, namespace: &str) -> Self {
        Self {
            context: entry.name.clone(),
            cluster: entry.cluster.clone(),
            server: entry.server.clone(),
            namespace: namespace.to_string(),
            identity: entry.auth_method.clone(),
            state: ConnectionState::Connecting,
            message: None,
            latency: None,
            version: Availability::NotChecked,
            nodes: Availability::NotChecked,
            namespace_status: Availability::NotChecked,
            workloads: Workloads::default(),
            observations: Vec::new(),
        }
    }

    /// True when at least one observation is a warning or a problem.
    pub fn has_problems(&self) -> bool {
        self.observations
            .iter()
            .any(|observation| observation.severity >= Severity::Warning)
    }
}

/// Inspect `entry`, reading only what the credentials allow.
///
/// This never fails: an unreachable or forbidden cluster is a *result*, described by
/// [`Inspection::state`], so the caller (CLI or TUI) always has something to show.
pub async fn inspect(
    entry: &ContextEntry,
    namespace_override: Option<&str>,
    timeouts: client::Timeouts,
) -> Inspection {
    let namespace = namespace_override
        .unwrap_or_else(|| entry.effective_namespace())
        .to_string();
    let mut snapshot = Inspection::pending(entry, &namespace);

    match tokio::time::timeout(
        timeouts.overall,
        gather(entry, namespace_override, timeouts, &mut snapshot),
    )
    .await
    {
        Ok(Ok(())) => snapshot.state = ConnectionState::Connected,
        Ok(Err(error)) => {
            snapshot.state = error.state();
            snapshot.message = Some(error.to_string());
        }
        Err(_elapsed) => {
            let error = InspectError::TimedOut(timeouts.overall);
            snapshot.state = error.state();
            snapshot.message = Some(error.to_string());
        }
    }

    // Most severe first; equal severities keep the order they were produced in.
    snapshot
        .observations
        .sort_by_key(|observation| std::cmp::Reverse(observation.severity));
    snapshot
}

/// Connect, then fill in every section that the credentials permit.
async fn gather(
    entry: &ContextEntry,
    namespace_override: Option<&str>,
    timeouts: client::Timeouts,
    snapshot: &mut Inspection,
) -> Result<(), InspectError> {
    let connection = client::connect(entry, timeouts).await?;
    snapshot.server = Some(connection.server.clone());
    // Without an override, the namespace `kube` resolved for the context is authoritative.
    if namespace_override.is_none() {
        snapshot.namespace = connection.namespace.clone();
    }
    let namespace = snapshot.namespace.clone();
    let namespace = namespace.as_str();

    // `/version` doubles as the reachability probe and the latency sample.
    let started = Instant::now();
    match connection.server_version().await {
        Ok(info) => {
            snapshot.latency = Some(started.elapsed());
            snapshot.version = Availability::Ok(ServerVersion {
                git_version: info.git_version,
                platform: info.platform,
            });
        }
        Err(error) => {
            let classified = classify(&error);
            if !classified.is_permission_denied() {
                // Unreachable, unauthenticated or broken TLS: nothing else can succeed either.
                return Err(classified);
            }
            // Reachable and authenticated, just not allowed to read the version.
            snapshot.latency = Some(started.elapsed());
            snapshot.version = Availability::Denied;
        }
    }

    // Every remaining read runs concurrently: one slow or forbidden section cannot hold up
    // the others.
    let (nodes, namespace_status, pods, deployments, statefulsets, daemonsets, jobs, cronjobs) = tokio::join!(
        read_nodes(&connection),
        read_namespace(&connection, namespace),
        read_pods(&connection, namespace),
        read_deployments(&connection, namespace),
        read_statefulsets(&connection, namespace),
        read_daemonsets(&connection, namespace),
        read_jobs(&connection, namespace),
        read_cronjobs(&connection, namespace),
    );

    let observations = &mut snapshot.observations;
    snapshot.nodes = collect(nodes, observations);
    snapshot.namespace_status = collect(namespace_status, observations);
    snapshot.workloads = Workloads {
        pods: collect(pods, observations),
        deployments: collect(deployments, observations),
        statefulsets: collect(statefulsets, observations),
        daemonsets: collect(daemonsets, observations),
        jobs: collect(jobs, observations),
        cronjobs: collect(cronjobs, observations),
    };

    Ok(())
}

/// Split an analysed section into its summary and its observations.
fn collect<T>(
    section: Availability<(T, Vec<Observation>)>,
    observations: &mut Vec<Observation>,
) -> Availability<T> {
    match section {
        Availability::Ok((summary, found)) => {
            observations.extend(found);
            Availability::Ok(summary)
        }
        Availability::Denied => Availability::Denied,
        Availability::Unavailable(message) => Availability::Unavailable(message),
        Availability::NotChecked => Availability::NotChecked,
    }
}

/// Cluster nodes, if the credentials may list them.
async fn read_nodes(connection: &Connection) -> Availability<(NodeSummary, Vec<Observation>)> {
    match connection.cluster::<Node>().list(LIST_LIMIT).await {
        Ok(listing) => Availability::Ok(health::observe_nodes(&listing.items, listing.truncated)),
        Err(error) => Availability::from_error(&error),
    }
}

/// The inspected namespace object itself.
async fn read_namespace(
    connection: &Connection,
    namespace: &str,
) -> Availability<(NamespaceSummary, Vec<Observation>)> {
    match connection.cluster::<Namespace>().get(namespace).await {
        Ok(object) => Availability::Ok(health::observe_namespace(&object)),
        Err(error) => Availability::from_error(&error),
    }
}

/// Pods in the inspected namespace.
async fn read_pods(
    connection: &Connection,
    namespace: &str,
) -> Availability<(PodStats, Vec<Observation>)> {
    match connection
        .namespaced::<Pod>(namespace)
        .list(LIST_LIMIT)
        .await
    {
        Ok(listing) => Availability::Ok(health::observe_pods(&listing.items, listing.truncated)),
        Err(error) => Availability::from_error(&error),
    }
}

/// Deployments in the inspected namespace.
async fn read_deployments(
    connection: &Connection,
    namespace: &str,
) -> Availability<(ReplicaStats, Vec<Observation>)> {
    match connection
        .namespaced::<Deployment>(namespace)
        .list(LIST_LIMIT)
        .await
    {
        Ok(listing) => Availability::Ok(health::observe_deployments(
            &listing.items,
            listing.truncated,
        )),
        Err(error) => Availability::from_error(&error),
    }
}

/// StatefulSets in the inspected namespace.
async fn read_statefulsets(
    connection: &Connection,
    namespace: &str,
) -> Availability<(ReplicaStats, Vec<Observation>)> {
    match connection
        .namespaced::<StatefulSet>(namespace)
        .list(LIST_LIMIT)
        .await
    {
        Ok(listing) => Availability::Ok(health::observe_statefulsets(
            &listing.items,
            listing.truncated,
        )),
        Err(error) => Availability::from_error(&error),
    }
}

/// DaemonSets in the inspected namespace.
async fn read_daemonsets(
    connection: &Connection,
    namespace: &str,
) -> Availability<(DaemonSetStats, Vec<Observation>)> {
    match connection
        .namespaced::<DaemonSet>(namespace)
        .list(LIST_LIMIT)
        .await
    {
        Ok(listing) => Availability::Ok(health::observe_daemonsets(
            &listing.items,
            listing.truncated,
        )),
        Err(error) => Availability::from_error(&error),
    }
}

/// Jobs in the inspected namespace.
async fn read_jobs(
    connection: &Connection,
    namespace: &str,
) -> Availability<(JobStats, Vec<Observation>)> {
    match connection
        .namespaced::<Job>(namespace)
        .list(LIST_LIMIT)
        .await
    {
        Ok(listing) => Availability::Ok(health::observe_jobs(&listing.items, listing.truncated)),
        Err(error) => Availability::from_error(&error),
    }
}

/// CronJobs in the inspected namespace.
async fn read_cronjobs(
    connection: &Connection,
    namespace: &str,
) -> Availability<(CronJobStats, Vec<Observation>)> {
    match connection
        .namespaced::<CronJob>(namespace)
        .list(LIST_LIMIT)
        .await
    {
        Ok(listing) => {
            Availability::Ok(health::observe_cronjobs(&listing.items, listing.truncated))
        }
        Err(error) => Availability::from_error(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubeconfig::parser;

    fn context(server: &str) -> (tempfile::TempDir, ContextEntry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            format!(
                "apiVersion: v1\nkind: Config\nclusters:\n  - name: c\n    cluster:\n      server: {server}\n      insecure-skip-tls-verify: true\nusers:\n  - name: u\n    user:\n      token: t\ncontexts:\n  - name: ctx\n    context:\n      cluster: c\n      user: u\n      namespace: apps\n"
            ),
        )
        .unwrap();
        let entry = parser::load(&path).unwrap().entries.remove(0);
        (dir, entry)
    }

    #[test]
    fn a_pending_snapshot_describes_the_context_without_touching_the_network() {
        let (_dir, entry) = context("https://cluster.example.com:6443");
        let snapshot = Inspection::pending(&entry, entry.effective_namespace());

        assert_eq!(snapshot.context, "ctx");
        assert_eq!(snapshot.namespace, "apps");
        assert_eq!(snapshot.state, ConnectionState::Connecting);
        assert_eq!(snapshot.version, Availability::NotChecked);
        assert!(!snapshot.has_problems());
    }

    /// Nothing listens on this port, so the connection is refused immediately.
    #[tokio::test]
    async fn an_unreachable_cluster_yields_a_snapshot_rather_than_an_error() {
        let (_dir, entry) = context("https://127.0.0.1:1");
        let snapshot = inspect(&entry, None, client::Timeouts::default()).await;

        assert_eq!(snapshot.state, ConnectionState::Unavailable);
        assert!(snapshot.message.is_some());
        // Nothing was attempted beyond the probe.
        assert_eq!(snapshot.nodes, Availability::NotChecked);
        assert_eq!(snapshot.workloads.pods, Availability::NotChecked);
    }

    #[tokio::test]
    async fn an_unroutable_address_times_out_within_the_budget() {
        // 203.0.113.0/24 is reserved for documentation and is not routable.
        let (_dir, entry) = context("https://203.0.113.1:6443");
        let timeouts = client::Timeouts::with_overall(Duration::from_millis(300));

        let started = Instant::now();
        let snapshot = inspect(&entry, None, timeouts).await;

        assert_eq!(
            snapshot.state,
            ConnectionState::TimedOut,
            "{:?}",
            snapshot.message
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn the_namespace_override_wins_over_the_context_namespace() {
        let (_dir, entry) = context("https://127.0.0.1:1");
        let snapshot = inspect(&entry, Some("kube-system"), client::Timeouts::default()).await;

        assert_eq!(snapshot.namespace, "kube-system");
    }

    #[test]
    fn availability_serialises_with_a_status_tag() {
        let denied: Availability<PodStats> = Availability::Denied;
        let value = serde_json::to_value(&denied).unwrap();
        assert_eq!(value["status"], "denied");

        let ok = Availability::Ok(PodStats {
            total: 2,
            ready: 2,
            ..PodStats::default()
        });
        let value = serde_json::to_value(&ok).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["detail"]["total"], 2);
    }

    #[test]
    fn availability_reports_permission_problems_separately_from_failures() {
        let error = kube::Error::Api(Box::new(kube::core::Status {
            code: 403,
            message: "nodes is forbidden".to_string(),
            ..kube::core::Status::default()
        }));
        assert_eq!(
            Availability::<PodStats>::from_error(&error),
            Availability::Denied
        );

        let error = kube::Error::Service(Box::new(std::io::Error::other("boom")));
        assert!(matches!(
            Availability::<PodStats>::from_error(&error),
            Availability::Unavailable(_)
        ));
    }

    #[test]
    fn latency_is_serialised_as_milliseconds() {
        let (_dir, entry) = context("https://example.com");
        let mut snapshot = Inspection::pending(&entry, "default");
        snapshot.latency = Some(Duration::from_millis(34));

        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(value["latency"], 34);
        assert_eq!(value["identity"]["kind"], "token");
    }

    #[test]
    fn observations_are_sorted_most_severe_first() {
        let mut observations = [
            Observation {
                severity: Severity::Info,
                text: "info".into(),
            },
            Observation {
                severity: Severity::Problem,
                text: "problem".into(),
            },
            Observation {
                severity: Severity::Warning,
                text: "warning".into(),
            },
        ];
        observations.sort_by_key(|observation| std::cmp::Reverse(observation.severity));

        let texts: Vec<&str> = observations.iter().map(|o| o.text.as_str()).collect();
        assert_eq!(texts, vec!["problem", "warning", "info"]);
    }
}

/// End-to-end tests against a loopback stand-in for the API server.
///
/// These drive the real `kube` client over real HTTP, so they cover request paths, response
/// decoding and — most importantly — what happens when only *some* reads are permitted.
#[cfg(test)]
mod api_tests {
    use super::*;
    use crate::kubeconfig::parser;
    use crate::kubernetes::fake_api::{FakeApiServer, Route};
    use serde_json::json;

    fn version_body() -> serde_json::Value {
        json!({
            "major": "1", "minor": "30", "gitVersion": "v1.30.2",
            "gitCommit": "abc", "gitTreeState": "clean", "buildDate": "2026-05-01T00:00:00Z",
            "goVersion": "go1.24", "compiler": "gc", "platform": "linux/amd64"
        })
    }

    fn list(kind: &str, items: serde_json::Value) -> serde_json::Value {
        json!({"kind": kind, "apiVersion": "v1", "metadata": {}, "items": items})
    }

    /// A namespace where Nodes are forbidden, CronJobs are broken and Pods are unhealthy.
    fn routes() -> Vec<Route> {
        vec![
            Route::ok("/version", version_body()),
            // Read-only credentials very often cannot list cluster-scoped Nodes.
            Route::failure(
                "/api/v1/nodes",
                403,
                "nodes is forbidden: cannot list resource \"nodes\"",
            ),
            Route::ok(
                "/api/v1/namespaces/apps",
                json!({"kind": "Namespace", "apiVersion": "v1", "metadata": {"name": "apps"},
                       "status": {"phase": "Active"}}),
            ),
            Route::ok(
                "/api/v1/namespaces/apps/pods",
                list(
                    "PodList",
                    json!([
                        {"metadata": {"name": "web-1"},
                         "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]}},
                        {"metadata": {"name": "web-2"},
                         "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "False"}],
                                    "containerStatuses": [{"name": "c", "ready": false, "restartCount": 31,
                                                            "image": "i", "imageID": "",
                                                            "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}}
                    ]),
                ),
            ),
            Route::ok(
                "/apis/apps/v1/namespaces/apps/deployments",
                list(
                    "DeploymentList",
                    json!([{"metadata": {"name": "web"},
                            "spec": {"replicas": 3, "selector": {}, "template": {}},
                            "status": {"replicas": 3, "availableReplicas": 2}}]),
                ),
            ),
            Route::ok(
                "/apis/apps/v1/namespaces/apps/statefulsets",
                list("StatefulSetList", json!([])),
            ),
            Route::ok(
                "/apis/apps/v1/namespaces/apps/daemonsets",
                list("DaemonSetList", json!([])),
            ),
            Route::ok(
                "/apis/batch/v1/namespaces/apps/jobs",
                list("JobList", json!([])),
            ),
            Route::failure(
                "/apis/batch/v1/namespaces/apps/cronjobs",
                500,
                "etcd is unhappy",
            ),
        ]
    }

    fn context_for(server: &str) -> (tempfile::TempDir, ContextEntry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            format!(
                "apiVersion: v1\nkind: Config\nclusters:\n  - name: c\n    cluster:\n      server: {server}\nusers:\n  - name: u\n    user:\n      token: t\ncontexts:\n  - name: ctx\n    context:\n      cluster: c\n      user: u\n      namespace: apps\n"
            ),
        )
        .unwrap();
        let entry = parser::load(&path).unwrap().entries.remove(0);
        (dir, entry)
    }

    #[tokio::test]
    async fn insufficient_permissions_degrade_one_section_not_the_whole_snapshot() {
        let server = FakeApiServer::start(routes());
        let (_dir, entry) = context_for(&server.url);

        let snapshot = inspect(&entry, None, client::Timeouts::default()).await;

        // The cluster answered, so the snapshot is Connected despite the forbidden Nodes.
        assert_eq!(
            snapshot.state,
            ConnectionState::Connected,
            "{:?}",
            snapshot.message
        );
        assert_eq!(snapshot.namespace, "apps");
        assert!(snapshot.latency.is_some());

        let version = snapshot
            .version
            .value()
            .expect("version should be readable");
        assert_eq!(version.git_version, "v1.30.2");
        assert_eq!(version.platform, "linux/amd64");

        // Forbidden vs. broken are reported differently.
        assert_eq!(snapshot.nodes, Availability::Denied);
        assert_eq!(snapshot.nodes.describe(), "unavailable (permission denied)");
        assert!(
            matches!(snapshot.workloads.cronjobs, Availability::Unavailable(_)),
            "{:?}",
            snapshot.workloads.cronjobs
        );

        // Everything readable is still there.
        let pods = snapshot
            .workloads
            .pods
            .value()
            .expect("pods should be readable");
        assert_eq!(pods.total, 2);
        assert_eq!(pods.ready, 1);
        let deployments = snapshot.workloads.deployments.value().unwrap();
        assert_eq!(deployments.degraded, 1);
        assert_eq!(
            snapshot.namespace_status.value().unwrap().phase.as_deref(),
            Some("Active")
        );
    }

    #[tokio::test]
    async fn problems_are_reported_as_plain_observations() {
        let server = FakeApiServer::start(routes());
        let (_dir, entry) = context_for(&server.url);

        let snapshot = inspect(&entry, None, client::Timeouts::default()).await;
        let texts: Vec<&str> = snapshot
            .observations
            .iter()
            .map(|o| o.text.as_str())
            .collect();

        assert!(texts.contains(&"1 Pod not Ready (web-2)"), "{texts:?}");
        assert!(
            texts.contains(&"1 Pod in CrashLoopBackOff (web-2)"),
            "{texts:?}"
        );
        assert!(
            texts.contains(&"deployment web has 2/3 available replicas"),
            "{texts:?}"
        );
        assert!(
            texts.contains(&"1 Pod with more than 5 container restarts (highest 31)"),
            "{texts:?}"
        );
        assert!(snapshot.has_problems());
        // Problems come before informational notes.
        assert_eq!(snapshot.observations[0].severity, Severity::Problem);
    }

    #[tokio::test]
    async fn a_rejected_token_is_reported_as_an_authentication_failure() {
        let server = FakeApiServer::start(vec![Route::failure("/version", 401, "Unauthorized")]);
        let (_dir, entry) = context_for(&server.url);

        let snapshot = inspect(&entry, None, client::Timeouts::default()).await;

        assert_eq!(snapshot.state, ConnectionState::AuthenticationFailed);
        assert_eq!(snapshot.workloads.pods, Availability::NotChecked);
    }

    #[tokio::test]
    async fn a_namespace_override_changes_the_paths_that_are_read() {
        let mut routes = routes();
        routes.push(Route::ok(
            "/api/v1/namespaces/kube-system/pods",
            list("PodList", json!([{"metadata": {"name": "kube-proxy"},
                                    "status": {"phase": "Running",
                                               "conditions": [{"type": "Ready", "status": "True"}]}}])),
        ));
        let server = FakeApiServer::start(routes);
        let (_dir, entry) = context_for(&server.url);

        let snapshot = inspect(&entry, Some("kube-system"), client::Timeouts::default()).await;

        assert_eq!(snapshot.namespace, "kube-system");
        assert_eq!(snapshot.workloads.pods.value().unwrap().total, 1);
    }

    #[tokio::test]
    async fn a_snapshot_of_a_healthy_namespace_has_nothing_to_report() {
        let routes = vec![
            Route::ok("/version", version_body()),
            Route::ok("/api/v1/nodes", list("NodeList", json!([]))),
            Route::ok(
                "/api/v1/namespaces/apps",
                json!({"kind": "Namespace", "apiVersion": "v1", "metadata": {"name": "apps"},
                       "status": {"phase": "Active"}}),
            ),
            Route::ok("/api/v1/namespaces/apps/pods", list("PodList", json!([]))),
            Route::ok(
                "/apis/apps/v1/namespaces/apps/deployments",
                list("DeploymentList", json!([])),
            ),
            Route::ok(
                "/apis/apps/v1/namespaces/apps/statefulsets",
                list("StatefulSetList", json!([])),
            ),
            Route::ok(
                "/apis/apps/v1/namespaces/apps/daemonsets",
                list("DaemonSetList", json!([])),
            ),
            Route::ok(
                "/apis/batch/v1/namespaces/apps/jobs",
                list("JobList", json!([])),
            ),
            Route::ok(
                "/apis/batch/v1/namespaces/apps/cronjobs",
                list("CronJobList", json!([])),
            ),
        ];
        let server = FakeApiServer::start(routes);
        let (_dir, entry) = context_for(&server.url);

        let snapshot = inspect(&entry, None, client::Timeouts::default()).await;

        assert_eq!(snapshot.state, ConnectionState::Connected);
        assert!(
            snapshot.observations.is_empty(),
            "{:?}",
            snapshot.observations
        );
        assert!(!snapshot.has_problems());
    }
}
