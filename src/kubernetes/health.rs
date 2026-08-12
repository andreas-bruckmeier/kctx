//! Interpreting Kubernetes objects: counts plus transparent observations.
//!
//! Everything here is a pure function from resource objects to summaries, so it is testable from
//! fixtures without a cluster — and so it can never accidentally issue a request.
//!
//! There is deliberately no health score. Each observation states one fact the user can check
//! themselves ("2 Pods not Ready"), because an opaque number hides more than it explains.

use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::core::v1::{Namespace, Node, Pod};
use serde::Serialize;

/// How much attention an observation deserves. Used for colouring only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    /// Worth knowing, not wrong.
    Info,
    /// Possibly degraded.
    Warning,
    /// Something is clearly not working.
    Problem,
}

/// One plainly worded statement about the inspected objects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Observation {
    /// Attention level.
    pub severity: Severity,
    /// Human-readable statement, e.g. `2 Pods not Ready`.
    pub text: String,
}

impl Observation {
    /// An informational observation.
    fn info(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Info,
            text: text.into(),
        }
    }

    /// A possible degradation.
    fn warning(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            text: text.into(),
        }
    }

    /// A clear problem.
    fn problem(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Problem,
            text: text.into(),
        }
    }
}

/// At most this many individually named objects per observation kind; the rest are counted.
const NAME_LIMIT: usize = 3;

/// Container waiting reasons that always mean something is broken rather than starting up.
const FATAL_WAITING_REASONS: &[&str] = &[
    "CrashLoopBackOff",
    "ImagePullBackOff",
    "ErrImagePull",
    "CreateContainerConfigError",
    "CreateContainerError",
    "InvalidImageName",
    "RunContainerError",
];

/// Restart count above which a pod is called out.
const RESTART_THRESHOLD: i32 = 5;

/// Node-level summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NodeSummary {
    /// Nodes returned by the API.
    pub total: usize,
    /// Nodes whose `Ready` condition is `True`.
    pub ready: usize,
    /// Nodes carrying a control-plane role label.
    pub control_plane: usize,
    /// Distinct kubelet versions, sorted.
    pub versions: Vec<String>,
    /// Distinct `os/arch` combinations, sorted.
    pub platforms: Vec<String>,
    /// True when more nodes exist than were listed.
    pub truncated: bool,
}

/// Namespace-level summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NamespaceSummary {
    /// Namespace name.
    pub name: String,
    /// `status.phase`, e.g. `Active` or `Terminating`.
    pub phase: Option<String>,
}

/// Pod counts for one namespace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PodStats {
    /// Pods returned by the API.
    pub total: usize,
    /// Pods whose `Ready` condition is `True`.
    pub ready: usize,
    /// Pods in `Running` phase.
    pub running: usize,
    /// Pods in `Pending` phase.
    pub pending: usize,
    /// Pods in `Succeeded` phase.
    pub succeeded: usize,
    /// Pods in `Failed` phase.
    pub failed: usize,
    /// True when more pods exist than were listed.
    pub truncated: bool,
}

/// Replica counts aggregated over Deployments or StatefulSets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReplicaStats {
    /// Objects returned by the API.
    pub total: usize,
    /// Sum of desired replicas.
    pub desired: i32,
    /// Sum of available replicas.
    pub available: i32,
    /// Objects with fewer available replicas than desired.
    pub degraded: usize,
    /// True when more objects exist than were listed.
    pub truncated: bool,
}

/// DaemonSet scheduling counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DaemonSetStats {
    /// Objects returned by the API.
    pub total: usize,
    /// Sum of desired scheduled pods.
    pub desired: i32,
    /// Sum of ready pods.
    pub ready: i32,
    /// Objects with unavailable or misscheduled pods.
    pub degraded: usize,
    /// True when more objects exist than were listed.
    pub truncated: bool,
}

/// Job outcome counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct JobStats {
    /// Objects returned by the API.
    pub total: usize,
    /// Jobs with running pods.
    pub active: usize,
    /// Jobs with at least one failed pod.
    pub failed: usize,
    /// Jobs that reached `Complete`.
    pub complete: usize,
    /// True when more objects exist than were listed.
    pub truncated: bool,
}

/// CronJob counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CronJobStats {
    /// Objects returned by the API.
    pub total: usize,
    /// CronJobs with `spec.suspend: true`.
    pub suspended: usize,
    /// CronJobs that have never run.
    pub never_scheduled: usize,
    /// True when more objects exist than were listed.
    pub truncated: bool,
}

/// Summarise nodes and report readiness problems, notable conditions and version skew.
pub fn observe_nodes(nodes: &[Node], truncated: bool) -> (NodeSummary, Vec<Observation>) {
    let mut summary = NodeSummary {
        total: nodes.len(),
        truncated,
        ..NodeSummary::default()
    };
    let mut versions = BTreeSet::new();
    let mut platforms = BTreeSet::new();
    let mut not_ready = Vec::new();
    let mut pressured: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for node in nodes {
        let name = object_name(node.metadata.name.as_deref());

        if is_control_plane(node) {
            summary.control_plane += 1;
        }
        if condition_is(node_conditions(node), "Ready", "True") {
            summary.ready += 1;
        } else {
            not_ready.push(name.clone());
        }

        // Any of these being True is a genuine node problem, unlike Ready which is inverted.
        for condition in [
            "MemoryPressure",
            "DiskPressure",
            "PIDPressure",
            "NetworkUnavailable",
        ] {
            if condition_is(node_conditions(node), condition, "True") {
                pressured
                    .entry(condition.to_string())
                    .or_default()
                    .push(name.clone());
            }
        }

        if let Some(info) = node
            .status
            .as_ref()
            .and_then(|status| status.node_info.as_ref())
        {
            versions.insert(info.kubelet_version.clone());
            platforms.insert(format!("{}/{}", info.operating_system, info.architecture));
        }
    }

    summary.versions = versions.into_iter().collect();
    summary.platforms = platforms.into_iter().collect();

    let mut observations = Vec::new();
    if !not_ready.is_empty() {
        observations.push(Observation::problem(format!(
            "{} of {} Nodes not Ready ({})",
            not_ready.len(),
            summary.total,
            join_names(&not_ready)
        )));
    }
    for (condition, names) in &pressured {
        observations.push(Observation::warning(format!(
            "{} Node(s) report {condition} ({})",
            names.len(),
            join_names(names)
        )));
    }
    if summary.versions.len() > 1 {
        observations.push(Observation::info(format!(
            "Nodes run {} different kubelet versions: {}",
            summary.versions.len(),
            summary.versions.join(", ")
        )));
    }
    if truncated {
        observations.push(Observation::info(format!(
            "only the first {} Nodes were inspected",
            summary.total
        )));
    }

    (summary, observations)
}

/// Summarise a namespace object.
pub fn observe_namespace(namespace: &Namespace) -> (NamespaceSummary, Vec<Observation>) {
    let summary = NamespaceSummary {
        name: object_name(namespace.metadata.name.as_deref()),
        phase: namespace
            .status
            .as_ref()
            .and_then(|status| status.phase.clone()),
    };

    let mut observations = Vec::new();
    if summary
        .phase
        .as_deref()
        .is_some_and(|phase| phase != "Active")
    {
        observations.push(Observation::warning(format!(
            "Namespace {} is {}",
            summary.name,
            summary.phase.as_deref().unwrap_or("in an unknown phase")
        )));
    }

    (summary, observations)
}

/// Summarise pods and report readiness, waiting reasons and restart counts.
pub fn observe_pods(pods: &[Pod], truncated: bool) -> (PodStats, Vec<Observation>) {
    let mut stats = PodStats {
        total: pods.len(),
        truncated,
        ..PodStats::default()
    };
    let mut not_ready = Vec::new();
    let mut waiting: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut restarting: Vec<(String, i32)> = Vec::new();

    for pod in pods {
        let name = object_name(pod.metadata.name.as_deref());
        let phase = pod
            .status
            .as_ref()
            .and_then(|status| status.phase.as_deref())
            .unwrap_or("");

        match phase {
            "Running" => stats.running += 1,
            "Pending" => stats.pending += 1,
            "Succeeded" => stats.succeeded += 1,
            "Failed" => stats.failed += 1,
            _ => {}
        }

        let ready = condition_is(pod_conditions(pod), "Ready", "True");
        if ready {
            stats.ready += 1;
        } else if phase != "Succeeded" {
            // Completed pods are not "not Ready" in any interesting sense.
            not_ready.push(name.clone());
        }

        for container in container_statuses(pod) {
            if let Some(reason) = container
                .state
                .as_ref()
                .and_then(|state| state.waiting.as_ref())
                .and_then(|waiting| waiting.reason.as_deref())
                && FATAL_WAITING_REASONS.contains(&reason)
            {
                waiting
                    .entry(reason.to_string())
                    .or_default()
                    .push(name.clone());
            }
        }

        let restarts = container_statuses(pod)
            .map(|status| status.restart_count)
            .max()
            .unwrap_or(0);
        if restarts > RESTART_THRESHOLD {
            restarting.push((name.clone(), restarts));
        }
    }

    let mut observations = Vec::new();
    if !not_ready.is_empty() {
        observations.push(Observation::problem(format!(
            "{} Pod{} not Ready ({})",
            not_ready.len(),
            plural(not_ready.len()),
            join_names(&not_ready)
        )));
    }
    if stats.pending > 0 {
        observations.push(Observation::warning(format!(
            "{} Pod{} Pending",
            stats.pending,
            plural(stats.pending)
        )));
    }
    for (reason, names) in &waiting {
        // Deduplicate: a pod with several broken containers is still one pod.
        let mut unique = names.clone();
        unique.dedup();
        observations.push(Observation::problem(format!(
            "{} Pod{} in {reason} ({})",
            unique.len(),
            plural(unique.len()),
            join_names(&unique)
        )));
    }
    if stats.failed > 0 {
        observations.push(Observation::problem(format!(
            "{} Pod{} Failed",
            stats.failed,
            plural(stats.failed)
        )));
    }
    if !restarting.is_empty() {
        let worst = restarting
            .iter()
            .map(|(_, count)| *count)
            .max()
            .unwrap_or(0);
        observations.push(Observation::warning(format!(
            "{} Pod{} with more than {RESTART_THRESHOLD} container restarts (highest {worst})",
            restarting.len(),
            plural(restarting.len())
        )));
    }
    if truncated {
        observations.push(Observation::info(format!(
            "only the first {} Pods were inspected",
            stats.total
        )));
    }

    (stats, observations)
}

/// Summarise Deployments; report any with fewer available replicas than desired.
pub fn observe_deployments(
    deployments: &[Deployment],
    truncated: bool,
) -> (ReplicaStats, Vec<Observation>) {
    let counted = deployments.iter().map(|deployment| {
        let desired = deployment
            .spec
            .as_ref()
            .and_then(|spec| spec.replicas)
            .unwrap_or(1);
        let available = deployment
            .status
            .as_ref()
            .and_then(|status| status.available_replicas)
            .unwrap_or(0);
        (
            object_name(deployment.metadata.name.as_deref()),
            desired,
            available,
        )
    });
    replica_observations("Deployment", "available", counted, truncated)
}

/// Summarise StatefulSets; report any with fewer ready replicas than desired.
pub fn observe_statefulsets(
    statefulsets: &[StatefulSet],
    truncated: bool,
) -> (ReplicaStats, Vec<Observation>) {
    let counted = statefulsets.iter().map(|statefulset| {
        let desired = statefulset
            .spec
            .as_ref()
            .and_then(|spec| spec.replicas)
            .unwrap_or(1);
        let ready = statefulset
            .status
            .as_ref()
            .and_then(|status| status.ready_replicas)
            .unwrap_or(0);
        (
            object_name(statefulset.metadata.name.as_deref()),
            desired,
            ready,
        )
    });
    replica_observations("StatefulSet", "ready", counted, truncated)
}

/// Shared aggregation for the two replica-shaped controllers.
fn replica_observations(
    kind: &str,
    quality: &str,
    objects: impl Iterator<Item = (String, i32, i32)>,
    truncated: bool,
) -> (ReplicaStats, Vec<Observation>) {
    let mut stats = ReplicaStats {
        truncated,
        ..ReplicaStats::default()
    };
    let mut degraded = Vec::new();

    for (name, desired, healthy) in objects {
        stats.total += 1;
        stats.desired += desired;
        stats.available += healthy;
        if healthy < desired {
            stats.degraded += 1;
            degraded.push(format!("{name} {healthy}/{desired}"));
        }
    }

    let mut observations = Vec::new();
    for entry in degraded.iter().take(NAME_LIMIT) {
        let (name, ratio) = entry.split_once(' ').unwrap_or((entry.as_str(), ""));
        observations.push(Observation::problem(format!(
            "{} {name} has {ratio} {quality} replicas",
            kind.to_lowercase()
        )));
    }
    if degraded.len() > NAME_LIMIT {
        observations.push(Observation::problem(format!(
            "{} further {kind}s are degraded",
            degraded.len() - NAME_LIMIT
        )));
    }
    if truncated {
        observations.push(Observation::info(format!(
            "only the first {} {kind}s were inspected",
            stats.total
        )));
    }

    (stats, observations)
}

/// Summarise DaemonSets; report unavailable or misscheduled pods.
pub fn observe_daemonsets(
    daemonsets: &[DaemonSet],
    truncated: bool,
) -> (DaemonSetStats, Vec<Observation>) {
    let mut stats = DaemonSetStats {
        truncated,
        ..DaemonSetStats::default()
    };
    let mut observations = Vec::new();
    let mut degraded = Vec::new();

    for daemonset in daemonsets {
        stats.total += 1;
        let name = object_name(daemonset.metadata.name.as_deref());
        let Some(status) = &daemonset.status else {
            continue;
        };

        stats.desired += status.desired_number_scheduled;
        stats.ready += status.number_ready;

        if status.number_ready < status.desired_number_scheduled {
            stats.degraded += 1;
            degraded.push(format!(
                "daemonset {name} has {}/{} Pods ready",
                status.number_ready, status.desired_number_scheduled
            ));
        }
        if status.number_misscheduled > 0 {
            observations.push(Observation::warning(format!(
                "daemonset {name} has {} misscheduled Pod(s)",
                status.number_misscheduled
            )));
        }
    }

    for text in degraded.iter().take(NAME_LIMIT) {
        observations.push(Observation::problem(text.clone()));
    }
    if degraded.len() > NAME_LIMIT {
        observations.push(Observation::problem(format!(
            "{} further DaemonSets are degraded",
            degraded.len() - NAME_LIMIT
        )));
    }
    if truncated {
        observations.push(Observation::info(format!(
            "only the first {} DaemonSets were inspected",
            stats.total
        )));
    }

    (stats, observations)
}

/// Summarise Jobs; report failures.
pub fn observe_jobs(jobs: &[Job], truncated: bool) -> (JobStats, Vec<Observation>) {
    let mut stats = JobStats {
        total: jobs.len(),
        truncated,
        ..JobStats::default()
    };
    let mut failed = Vec::new();

    for job in jobs {
        let name = object_name(job.metadata.name.as_deref());
        let Some(status) = &job.status else { continue };

        if status.active.unwrap_or(0) > 0 {
            stats.active += 1;
        }
        if status.succeeded.unwrap_or(0) > 0 || job_condition_true(job, "Complete") {
            stats.complete += 1;
        }
        if status.failed.unwrap_or(0) > 0 || job_condition_true(job, "Failed") {
            stats.failed += 1;
            failed.push(name);
        }
    }

    let mut observations = Vec::new();
    if !failed.is_empty() {
        observations.push(Observation::problem(format!(
            "{} Job{} failed ({})",
            failed.len(),
            plural(failed.len()),
            join_names(&failed)
        )));
    }
    if truncated {
        observations.push(Observation::info(format!(
            "only the first {} Jobs were inspected",
            stats.total
        )));
    }

    (stats, observations)
}

/// Summarise CronJobs; report suspended schedules.
pub fn observe_cronjobs(cronjobs: &[CronJob], truncated: bool) -> (CronJobStats, Vec<Observation>) {
    let mut stats = CronJobStats {
        total: cronjobs.len(),
        truncated,
        ..CronJobStats::default()
    };
    let mut suspended = Vec::new();

    for cronjob in cronjobs {
        let name = object_name(cronjob.metadata.name.as_deref());
        if cronjob
            .spec
            .as_ref()
            .and_then(|spec| spec.suspend)
            .unwrap_or(false)
        {
            stats.suspended += 1;
            suspended.push(name);
        }
        if cronjob
            .status
            .as_ref()
            .is_none_or(|status| status.last_schedule_time.is_none())
        {
            stats.never_scheduled += 1;
        }
    }

    let mut observations = Vec::new();
    if !suspended.is_empty() {
        observations.push(Observation::info(format!(
            "{} CronJob{} suspended ({})",
            suspended.len(),
            if suspended.len() == 1 { " is" } else { "s are" },
            join_names(&suspended)
        )));
    }
    if truncated {
        observations.push(Observation::info(format!(
            "only the first {} CronJobs were inspected",
            stats.total
        )));
    }

    (stats, observations)
}

/// `s` when a count needs a plural.
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Names of unnamed objects should still read sensibly.
fn object_name(name: Option<&str>) -> String {
    name.unwrap_or("<unnamed>").to_string()
}

/// Join up to [`NAME_LIMIT`] names, counting the rest.
fn join_names(names: &[String]) -> String {
    if names.len() <= NAME_LIMIT {
        return names.join(", ");
    }
    format!(
        "{}, +{} more",
        names[..NAME_LIMIT].join(", "),
        names.len() - NAME_LIMIT
    )
}

/// True if a control-plane role label is present.
fn is_control_plane(node: &Node) -> bool {
    node.metadata.labels.as_ref().is_some_and(|labels| {
        labels.contains_key("node-role.kubernetes.io/control-plane")
            || labels.contains_key("node-role.kubernetes.io/master")
    })
}

/// `(type, status)` pairs of a node's conditions.
fn node_conditions(node: &Node) -> impl Iterator<Item = (&str, &str)> {
    node.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .into_iter()
        .flatten()
        .map(|condition| (condition.type_.as_str(), condition.status.as_str()))
}

/// `(type, status)` pairs of a pod's conditions.
fn pod_conditions(pod: &Pod) -> impl Iterator<Item = (&str, &str)> {
    pod.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .into_iter()
        .flatten()
        .map(|condition| (condition.type_.as_str(), condition.status.as_str()))
}

/// Container statuses of a pod, including init containers.
fn container_statuses(
    pod: &Pod,
) -> impl Iterator<Item = &k8s_openapi::api::core::v1::ContainerStatus> {
    let status = pod.status.as_ref();
    let containers = status.and_then(|status| status.container_statuses.as_ref());
    let init = status.and_then(|status| status.init_container_statuses.as_ref());
    containers
        .into_iter()
        .flatten()
        .chain(init.into_iter().flatten())
}

/// Whether a named condition currently has the given status.
fn condition_is<'a>(
    conditions: impl Iterator<Item = (&'a str, &'a str)>,
    name: &str,
    status: &str,
) -> bool {
    conditions
        .into_iter()
        .any(|(type_, value)| type_ == name && value == status)
}

/// Whether a job condition of the given type is `True`.
fn job_condition_true(job: &Job, name: &str) -> bool {
    job.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|condition| condition.type_ == name && condition.status == "True")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a typed resource from JSON, exactly as the API server would return it.
    fn object<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
        serde_json::from_value(value).expect("fixture must deserialise")
    }

    fn node(name: &str, ready: &str, version: &str, control_plane: bool) -> Node {
        let mut labels = serde_json::Map::new();
        if control_plane {
            labels.insert("node-role.kubernetes.io/control-plane".into(), json!(""));
        }
        object(json!({
            "metadata": {"name": name, "labels": labels},
            "status": {
                "conditions": [{"type": "Ready", "status": ready}],
                "nodeInfo": {
                    "kubeletVersion": version,
                    "architecture": "amd64",
                    "operatingSystem": "linux",
                    // The remaining nodeInfo fields are required by the schema.
                    "bootID": "", "containerRuntimeVersion": "", "kernelVersion": "",
                    "kubeProxyVersion": "", "machineID": "", "osImage": "", "systemUUID": ""
                }
            }
        }))
    }

    fn pod(name: &str, phase: &str, ready: &str) -> Pod {
        object(json!({
            "metadata": {"name": name},
            "status": {"phase": phase, "conditions": [{"type": "Ready", "status": ready}]}
        }))
    }

    fn pod_with_container(name: &str, phase: &str, waiting: Option<&str>, restarts: i32) -> Pod {
        let state = match waiting {
            Some(reason) => json!({"waiting": {"reason": reason}}),
            None => json!({"running": {}}),
        };
        object(json!({
            "metadata": {"name": name},
            "status": {
                "phase": phase,
                "conditions": [{"type": "Ready", "status": "False"}],
                "containerStatuses": [{
                    "name": "app", "ready": false, "restartCount": restarts,
                    "image": "img", "imageID": "", "state": state
                }]
            }
        }))
    }

    fn deployment(name: &str, desired: i32, available: i32) -> Deployment {
        object(json!({
            "metadata": {"name": name},
            "spec": {"replicas": desired, "selector": {}, "template": {}},
            "status": {"availableReplicas": available, "replicas": desired}
        }))
    }

    #[test]
    fn healthy_objects_produce_no_observations() {
        let (nodes, node_observations) = observe_nodes(
            &[
                node("a", "True", "v1.30.1", true),
                node("b", "True", "v1.30.1", false),
            ],
            false,
        );
        let (pods, pod_observations) = observe_pods(
            &[pod("x", "Running", "True"), pod("y", "Running", "True")],
            false,
        );
        let (deployments, deployment_observations) =
            observe_deployments(&[deployment("api", 3, 3)], false);

        assert_eq!(nodes.total, 2);
        assert_eq!(nodes.ready, 2);
        assert_eq!(nodes.control_plane, 1);
        assert_eq!(nodes.versions, vec!["v1.30.1"]);
        assert_eq!(nodes.platforms, vec!["linux/amd64"]);
        assert_eq!(pods.ready, 2);
        assert_eq!(deployments.desired, 3);
        assert_eq!(deployments.available, 3);

        assert!(node_observations.is_empty(), "{node_observations:?}");
        assert!(pod_observations.is_empty(), "{pod_observations:?}");
        assert!(
            deployment_observations.is_empty(),
            "{deployment_observations:?}"
        );
    }

    #[test]
    fn not_ready_nodes_are_named() {
        let (summary, observations) = observe_nodes(
            &[
                node("a", "True", "v1.30.1", false),
                node("b", "False", "v1.30.1", false),
            ],
            false,
        );

        assert_eq!(summary.ready, 1);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].severity, Severity::Problem);
        assert_eq!(observations[0].text, "1 of 2 Nodes not Ready (b)");
    }

    #[test]
    fn node_pressure_conditions_are_reported() {
        let node: Node = object(json!({
            "metadata": {"name": "a"},
            "status": {"conditions": [
                {"type": "Ready", "status": "True"},
                {"type": "MemoryPressure", "status": "True"},
                {"type": "DiskPressure", "status": "False"}
            ]}
        }));
        let (_, observations) = observe_nodes(&[node], false);

        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].text, "1 Node(s) report MemoryPressure (a)");
        assert_eq!(observations[0].severity, Severity::Warning);
    }

    #[test]
    fn kubelet_version_skew_is_noted() {
        let (summary, observations) = observe_nodes(
            &[
                node("a", "True", "v1.30.1", false),
                node("b", "True", "v1.29.4", false),
            ],
            false,
        );

        assert_eq!(summary.versions, vec!["v1.29.4", "v1.30.1"]);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].severity, Severity::Info);
        assert!(
            observations[0]
                .text
                .contains("2 different kubelet versions")
        );
    }

    #[test]
    fn many_broken_nodes_are_summarised_not_enumerated() {
        let nodes: Vec<Node> = (0..6)
            .map(|i| node(&format!("n{i}"), "False", "v1.30.1", false))
            .collect();
        let (_, observations) = observe_nodes(&nodes, false);

        assert_eq!(
            observations[0].text,
            "6 of 6 Nodes not Ready (n0, n1, n2, +3 more)"
        );
    }

    #[test]
    fn pod_phases_and_readiness_are_counted() {
        let pods = vec![
            pod("running", "Running", "True"),
            pod("pending", "Pending", "False"),
            pod("done", "Succeeded", "False"),
            pod("failed", "Failed", "False"),
        ];
        let (stats, observations) = observe_pods(&pods, false);

        assert_eq!(stats.total, 4);
        assert_eq!(stats.ready, 1);
        assert_eq!(stats.running, 1);
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.succeeded, 1);
        assert_eq!(stats.failed, 1);

        let texts: Vec<&str> = observations.iter().map(|o| o.text.as_str()).collect();
        // "done" completed successfully, so it must not be counted as not-Ready.
        assert!(
            texts.contains(&"2 Pods not Ready (pending, failed)"),
            "{texts:?}"
        );
        assert!(texts.contains(&"1 Pod Pending"), "{texts:?}");
        assert!(texts.contains(&"1 Pod Failed"), "{texts:?}");
    }

    #[test]
    fn crashloop_and_image_pull_failures_are_reported_by_reason() {
        let pods = vec![
            pod_with_container("a", "Running", Some("CrashLoopBackOff"), 0),
            pod_with_container("b", "Pending", Some("ImagePullBackOff"), 0),
            pod_with_container("c", "Pending", Some("ErrImagePull"), 0),
            // A benign waiting reason must not be reported.
            pod_with_container("d", "Pending", Some("ContainerCreating"), 0),
        ];
        let (_, observations) = observe_pods(&pods, false);
        let texts: Vec<&str> = observations.iter().map(|o| o.text.as_str()).collect();

        assert!(
            texts.contains(&"1 Pod in CrashLoopBackOff (a)"),
            "{texts:?}"
        );
        assert!(
            texts.contains(&"1 Pod in ImagePullBackOff (b)"),
            "{texts:?}"
        );
        assert!(texts.contains(&"1 Pod in ErrImagePull (c)"), "{texts:?}");
        assert!(
            !texts.iter().any(|text| text.contains("ContainerCreating")),
            "{texts:?}"
        );
    }

    #[test]
    fn excessive_restarts_are_reported_as_a_bounded_count() {
        let pods = vec![
            pod_with_container("a", "Running", None, 47),
            pod_with_container("b", "Running", None, 12),
            // At the threshold, not above it.
            pod_with_container("c", "Running", None, 5),
        ];
        let (_, observations) = observe_pods(&pods, false);
        let restarts = observations
            .iter()
            .find(|o| o.text.contains("restarts"))
            .unwrap();

        assert_eq!(
            restarts.text,
            "2 Pods with more than 5 container restarts (highest 47)"
        );
        assert_eq!(restarts.severity, Severity::Warning);
    }

    #[test]
    fn init_container_failures_are_seen_too() {
        let pod: Pod = object(json!({
            "metadata": {"name": "init-broken"},
            "status": {
                "phase": "Pending",
                "conditions": [{"type": "Ready", "status": "False"}],
                "initContainerStatuses": [{
                    "name": "init", "ready": false, "restartCount": 0, "image": "img", "imageID": "",
                    "state": {"waiting": {"reason": "ImagePullBackOff"}}
                }]
            }
        }));
        let (_, observations) = observe_pods(&[pod], false);

        assert!(
            observations
                .iter()
                .any(|o| o.text.contains("ImagePullBackOff")),
            "{observations:?}"
        );
    }

    #[test]
    fn degraded_deployments_are_named_with_their_ratio() {
        let (stats, observations) =
            observe_deployments(&[deployment("api", 3, 2), deployment("web", 2, 2)], false);

        assert_eq!(stats.total, 2);
        assert_eq!(stats.degraded, 1);
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].text,
            "deployment api has 2/3 available replicas"
        );
        assert_eq!(observations[0].severity, Severity::Problem);
    }

    #[test]
    fn deployments_without_status_count_as_fully_unavailable() {
        let deployment: Deployment = object(json!({
            "metadata": {"name": "fresh"},
            "spec": {"replicas": 2, "selector": {}, "template": {}}
        }));
        let (stats, observations) = observe_deployments(&[deployment], false);

        assert_eq!(stats.available, 0);
        assert_eq!(
            observations[0].text,
            "deployment fresh has 0/2 available replicas"
        );
    }

    #[test]
    fn many_degraded_deployments_are_capped() {
        let deployments: Vec<Deployment> =
            (0..5).map(|i| deployment(&format!("d{i}"), 2, 1)).collect();
        let (_, observations) = observe_deployments(&deployments, false);

        assert_eq!(observations.len(), NAME_LIMIT + 1);
        assert_eq!(
            observations.last().unwrap().text,
            "2 further Deployments are degraded"
        );
    }

    #[test]
    fn statefulsets_report_ready_replicas() {
        let statefulset: StatefulSet = object(json!({
            "metadata": {"name": "db"},
            "spec": {"replicas": 3, "selector": {}, "template": {}, "serviceName": "db"},
            "status": {"replicas": 3, "readyReplicas": 1, "availableReplicas": 1}
        }));
        let (stats, observations) = observe_statefulsets(&[statefulset], false);

        assert_eq!(stats.degraded, 1);
        assert_eq!(
            observations[0].text,
            "statefulset db has 1/3 ready replicas"
        );
    }

    #[test]
    fn daemonsets_report_missing_and_misscheduled_pods() {
        let daemonset: DaemonSet = object(json!({
            "metadata": {"name": "agent"},
            "spec": {"selector": {}, "template": {}},
            "status": {
                "desiredNumberScheduled": 5, "numberReady": 3, "numberMisscheduled": 1,
                "currentNumberScheduled": 4, "numberAvailable": 3, "numberUnavailable": 2
            }
        }));
        let (stats, observations) = observe_daemonsets(&[daemonset], false);
        let texts: Vec<&str> = observations.iter().map(|o| o.text.as_str()).collect();

        assert_eq!(stats.desired, 5);
        assert_eq!(stats.ready, 3);
        assert_eq!(stats.degraded, 1);
        assert!(
            texts.contains(&"daemonset agent has 3/5 Pods ready"),
            "{texts:?}"
        );
        assert!(
            texts.contains(&"daemonset agent has 1 misscheduled Pod(s)"),
            "{texts:?}"
        );
    }

    #[test]
    fn failed_jobs_are_reported_from_counts_or_conditions() {
        let by_count: Job = object(json!({
            "metadata": {"name": "import"}, "spec": {"template": {}},
            "status": {"failed": 1}
        }));
        let by_condition: Job = object(json!({
            "metadata": {"name": "export"}, "spec": {"template": {}},
            "status": {"conditions": [{"type": "Failed", "status": "True"}]}
        }));
        let healthy: Job = object(json!({
            "metadata": {"name": "ok"}, "spec": {"template": {}},
            "status": {"succeeded": 1}
        }));

        let (stats, observations) = observe_jobs(&[by_count, by_condition, healthy], false);

        assert_eq!(stats.total, 3);
        assert_eq!(stats.failed, 2);
        assert_eq!(stats.complete, 1);
        assert_eq!(observations[0].text, "2 Jobs failed (import, export)");
    }

    #[test]
    fn suspended_cronjobs_are_noted_as_information() {
        let suspended: CronJob = object(json!({
            "metadata": {"name": "nightly"},
            "spec": {"schedule": "0 0 * * *", "suspend": true, "jobTemplate": {"spec": {"template": {}}}}
        }));
        let active: CronJob = object(json!({
            "metadata": {"name": "hourly"},
            "spec": {"schedule": "0 * * * *", "jobTemplate": {"spec": {"template": {}}}},
            "status": {"lastScheduleTime": "2026-08-12T10:00:00Z"}
        }));

        let (stats, observations) = observe_cronjobs(&[suspended, active], false);

        assert_eq!(stats.total, 2);
        assert_eq!(stats.suspended, 1);
        assert_eq!(stats.never_scheduled, 1);
        assert_eq!(observations[0].text, "1 CronJob is suspended (nightly)");
        assert_eq!(observations[0].severity, Severity::Info);
    }

    #[test]
    fn a_terminating_namespace_is_a_warning() {
        let namespace: Namespace =
            object(json!({"metadata": {"name": "doomed"}, "status": {"phase": "Terminating"}}));
        let (summary, observations) = observe_namespace(&namespace);

        assert_eq!(summary.phase.as_deref(), Some("Terminating"));
        assert_eq!(observations[0].text, "Namespace doomed is Terminating");
    }

    #[test]
    fn an_active_namespace_is_silent() {
        let namespace: Namespace =
            object(json!({"metadata": {"name": "default"}, "status": {"phase": "Active"}}));
        let (_, observations) = observe_namespace(&namespace);

        assert!(observations.is_empty(), "{observations:?}");
    }

    #[test]
    fn truncated_listings_say_so_instead_of_pretending_to_be_complete() {
        let (_, observations) = observe_pods(&[pod("a", "Running", "True")], true);
        assert_eq!(
            observations.last().unwrap().text,
            "only the first 1 Pods were inspected"
        );

        let (_, observations) = observe_nodes(&[node("a", "True", "v1.30.1", false)], true);
        assert_eq!(
            observations.last().unwrap().text,
            "only the first 1 Nodes were inspected"
        );
    }

    #[test]
    fn objects_without_names_do_not_panic() {
        let nameless: Pod = object(json!({"status": {"phase": "Pending"}}));
        let (stats, observations) = observe_pods(&[nameless], false);

        assert_eq!(stats.pending, 1);
        assert!(
            observations[0].text.contains("<unnamed>"),
            "{observations:?}"
        );
    }
}
