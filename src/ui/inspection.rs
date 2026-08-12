//! The details pane: local kubeconfig facts, plus live cluster information once it arrives.
//!
//! This module renders whatever [`InspectionView`] it is handed and nothing more — no requests,
//! no analysis, no knowledge of `kube`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use crate::app::InspectionView;
use crate::kubeconfig::ContextEntry;
use crate::kubernetes::ConnectionState;
use crate::kubernetes::health::{Observation, Severity};
use crate::kubernetes::inspection::{Availability, Inspection};
use crate::paths;

/// Marker for values that are not configured.
const UNSET: &str = "-";

/// Width of the label column inside the pane. Wide enough for `StatefulSets` plus a gap.
const LABEL_WIDTH: usize = 14;

/// Render the details pane for `entry`, enriched with whatever `view` provides.
///
/// `scroll` is the first line to show; the return value is the largest offset that still shows
/// content, so the caller can clamp its scrolling without duplicating the layout maths.
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    entry: Option<&ContextEntry>,
    view: InspectionView<'_>,
    scroll: u16,
) -> u16 {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);

    let Some(entry) = entry else {
        frame.render_widget(
            Paragraph::new(Line::from("nothing selected").dim()).block(block.title(" Details ")),
            area,
        );
        return 0;
    };

    let mut lines = vec![
        Line::from(Span::styled(entry.name.clone(), Style::new().bold())),
        Line::default(),
    ];
    lines.extend(local_facts(entry));
    lines.push(Line::default());

    match view {
        InspectionView::Idle => {
            lines.push(Line::from(Span::styled(
                "press i to inspect the cluster",
                Style::new().dim(),
            )));
        }
        InspectionView::Connecting => {
            lines.push(status_line(ConnectionState::Connecting, None));
        }
        InspectionView::Ready(snapshot) => lines.extend(live_facts(snapshot)),
    }

    // Long snapshots (many problems) do not fit; report how far they can be scrolled. Wrapped
    // lines count as one here, so the estimate errs towards allowing one screen too little.
    let visible = area.height.saturating_sub(2);
    let overflow = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_sub(visible);
    let offset = scroll.min(overflow);

    let mut title = " Details ".to_string();
    if overflow > 0 {
        title = format!(" Details  {}/{} ", offset + 1, overflow + 1);
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((offset, 0))
            .block(block.title(title)),
        area,
    );
    overflow
}

/// Everything known without touching the network.
fn local_facts(entry: &ContextEntry) -> Vec<Line<'static>> {
    let mut lines = vec![
        field("Cluster", &entry.cluster),
        field("Server", entry.server.as_deref().unwrap_or("unknown")),
        field("Namespace", entry.namespace.as_deref().unwrap_or(UNSET)),
        field("User", entry.user.as_deref().unwrap_or(UNSET)),
        // The authentication *method* only: kctx never shows credentials.
        field("Auth", &entry.auth_method.to_string()),
        field("Source", &paths::shorten_home(&entry.source)),
    ];

    if entry.cluster_missing {
        lines.push(warn(format!(
            "cluster {:?} is not defined in this file",
            entry.cluster
        )));
    }
    if entry.ambiguous {
        lines.push(warn("another kubeconfig defines this context name too"));
    }
    lines
}

/// Everything read from the cluster.
fn live_facts(snapshot: &Inspection) -> Vec<Line<'static>> {
    let mut lines = vec![status_line(snapshot.state, snapshot.message.as_deref())];

    match &snapshot.version {
        Availability::Ok(version) => {
            lines.push(field(
                "Kubernetes",
                &format!("{} ({})", version.git_version, version.platform),
            ));
        }
        other => lines.push(field("Kubernetes", &other.describe())),
    }
    if let Some(latency) = snapshot.latency {
        lines.push(field("Latency", &format!("{} ms", latency.as_millis())));
    }

    lines.push(Line::default());
    lines.push(heading("Nodes"));
    match &snapshot.nodes {
        Availability::Ok(nodes) => {
            lines.push(field(
                "Ready",
                &format!(
                    "{}/{} ({} control plane)",
                    nodes.ready, nodes.total, nodes.control_plane
                ),
            ));
            if !nodes.versions.is_empty() {
                lines.push(field("Versions", &nodes.versions.join(", ")));
            }
            if !nodes.platforms.is_empty() {
                lines.push(field("Platforms", &nodes.platforms.join(", ")));
            }
        }
        other => lines.push(dimmed(&other.describe())),
    }

    lines.push(Line::default());
    lines.push(heading(&format!("Namespace {}", snapshot.namespace)));
    match &snapshot.namespace_status {
        Availability::Ok(namespace) => {
            lines.push(field("Phase", namespace.phase.as_deref().unwrap_or(UNSET)));
        }
        other => lines.push(field("Phase", &other.describe())),
    }
    lines.extend(workload_lines(snapshot));

    lines.push(Line::default());
    lines.push(heading("Problems"));
    if snapshot.observations.is_empty() {
        lines.push(dimmed(crate::output::problems_placeholder(snapshot)));
    } else {
        lines.extend(snapshot.observations.iter().map(observation_line));
    }

    lines
}

/// One line per workload kind, including the ones that could not be read.
fn workload_lines(snapshot: &Inspection) -> Vec<Line<'static>> {
    let workloads = &snapshot.workloads;
    let mut lines = Vec::new();

    lines.push(field(
        "Pods",
        &match &workloads.pods {
            Availability::Ok(pods) => format!("{} total, {} Ready", pods.total, pods.ready),
            other => other.describe(),
        },
    ));
    lines.push(field(
        "Deployments",
        &match &workloads.deployments {
            Availability::Ok(stats) => {
                format!(
                    "{} total, {}/{} available",
                    stats.total, stats.available, stats.desired
                )
            }
            other => other.describe(),
        },
    ));
    lines.push(field(
        "StatefulSets",
        &match &workloads.statefulsets {
            Availability::Ok(stats) => {
                format!(
                    "{} total, {}/{} ready",
                    stats.total, stats.available, stats.desired
                )
            }
            other => other.describe(),
        },
    ));
    lines.push(field(
        "DaemonSets",
        &match &workloads.daemonsets {
            Availability::Ok(stats) => {
                format!(
                    "{} total, {}/{} ready",
                    stats.total, stats.ready, stats.desired
                )
            }
            other => other.describe(),
        },
    ));
    lines.push(field(
        "Jobs",
        &match &workloads.jobs {
            Availability::Ok(stats) => format!("{} total, {} failed", stats.total, stats.failed),
            other => other.describe(),
        },
    ));
    lines.push(field(
        "CronJobs",
        &match &workloads.cronjobs {
            Availability::Ok(stats) => {
                format!("{} total, {} suspended", stats.total, stats.suspended)
            }
            other => other.describe(),
        },
    ));

    lines
}

/// `Status  Connected` with a colour that matches the outcome.
fn status_line(state: ConnectionState, message: Option<&str>) -> Line<'static> {
    let colour = match state {
        ConnectionState::Connected => Color::Green,
        ConnectionState::Connecting => Color::Cyan,
        ConnectionState::PermissionDenied | ConnectionState::TimedOut => Color::Yellow,
        _ => Color::Red,
    };

    let mut spans = vec![
        Span::styled(format!("{:<LABEL_WIDTH$}", "Status"), Style::new().dim()),
        Span::styled(state.to_string(), Style::new().fg(colour)),
    ];
    if let Some(message) = message {
        spans.push(Span::styled(format!(" ({message})"), Style::new().dim()));
    }
    Line::from(spans)
}

/// An observation, coloured by severity.
fn observation_line(observation: &Observation) -> Line<'static> {
    let colour = match observation.severity {
        Severity::Problem => Color::Red,
        Severity::Warning => Color::Yellow,
        Severity::Info => Color::Blue,
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(observation.text.clone(), Style::new().fg(colour)),
    ])
}

/// `label   value`, label dimmed.
fn field(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<LABEL_WIDTH$}"), Style::new().dim()),
        Span::raw(value.to_string()),
    ])
}

/// A section heading.
fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::new().bold()))
}

/// Dimmed free text.
fn dimmed(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::new().dim()))
}

/// A yellow caution line.
fn warn(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), Style::new().fg(Color::Yellow)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubeconfig::AuthMethod;
    use crate::kubernetes::health::{NodeSummary, PodStats};
    use crate::kubernetes::inspection::ServerVersion;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
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
            current_in_source: false,
            active: false,
            ambiguous: false,
            // An exec plugin: only the command name may ever appear on screen.
            auth_method: AuthMethod::Exec("aws".to_string()),
        }
    }

    fn snapshot(entry: &ContextEntry) -> Inspection {
        let mut snapshot = Inspection::pending(entry, "payments");
        snapshot.state = ConnectionState::Connected;
        snapshot.latency = Some(Duration::from_millis(34));
        snapshot.version = Availability::Ok(ServerVersion {
            git_version: "v1.30.2".to_string(),
            platform: "linux/amd64".to_string(),
        });
        snapshot.nodes = Availability::Ok(NodeSummary {
            total: 5,
            ready: 4,
            control_plane: 1,
            versions: vec!["v1.30.2".to_string()],
            platforms: vec!["linux/amd64".to_string()],
            truncated: false,
        });
        snapshot.workloads.pods = Availability::Ok(PodStats {
            total: 42,
            ready: 39,
            ..PodStats::default()
        });
        // A read-only account that cannot list Deployments must still see the rest.
        snapshot.workloads.deployments = Availability::Denied;
        snapshot.observations = vec![Observation {
            severity: Severity::Problem,
            text: "2 Pods not Ready (api-1, api-2)".to_string(),
        }];
        snapshot
    }

    /// Render the pane on its own and return it as text.
    fn render(entry: Option<&ContextEntry>, view: InspectionView<'_>) -> String {
        render_scrolled(entry, view, 0, 34)
    }

    /// Render at a given scroll offset and height.
    fn render_scrolled(
        entry: Option<&ContextEntry>,
        view: InspectionView<'_>,
        scroll: u16,
        height: u16,
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(52, height)).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, frame.area(), entry, view, scroll);
            })
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<String>>()
            .join("\n")
    }

    #[test]
    fn the_idle_pane_shows_local_facts_and_invites_inspection() {
        let entry = entry();
        let screen = render(Some(&entry), InspectionView::Idle);

        assert!(screen.contains("production-eu"), "{screen}");
        assert!(screen.contains("prod-cluster"), "{screen}");
        assert!(screen.contains("https://prod.example.com"), "{screen}");
        assert!(screen.contains("payments"), "{screen}");
        assert!(screen.contains("press i to inspect"), "{screen}");
        // Nothing live has been read, so no cluster sections appear.
        assert!(!screen.contains("Kubernetes"), "{screen}");
        assert!(!screen.contains("Problems"), "{screen}");
    }

    #[test]
    fn only_the_auth_method_is_shown_never_a_credential() {
        let entry = entry();
        let screen = render(Some(&entry), InspectionView::Idle);

        assert!(screen.contains("exec plugin (aws)"), "{screen}");
        for forbidden in ["token", "get-token", "certificate-data", "password"] {
            assert!(
                !screen.contains(forbidden),
                "{forbidden} leaked into {screen}"
            );
        }
    }

    #[test]
    fn an_in_flight_request_says_it_is_connecting() {
        let entry = entry();
        let screen = render(Some(&entry), InspectionView::Connecting);

        assert!(screen.contains("Connecting..."), "{screen}");
    }

    #[test]
    fn a_ready_snapshot_shows_live_information() {
        let entry = entry();
        let snapshot = snapshot(&entry);
        let screen = render(Some(&entry), InspectionView::Ready(&snapshot));

        assert!(screen.contains("Connected"), "{screen}");
        assert!(screen.contains("v1.30.2"), "{screen}");
        assert!(screen.contains("34 ms"), "{screen}");
        assert!(screen.contains("4/5"), "{screen}");
        assert!(screen.contains("42 total, 39 Ready"), "{screen}");
        assert!(screen.contains("2 Pods not Ready"), "{screen}");
    }

    #[test]
    fn forbidden_sections_say_why_they_are_empty() {
        let entry = entry();
        let snapshot = snapshot(&entry);
        let screen = render(Some(&entry), InspectionView::Ready(&snapshot));

        assert!(screen.contains("permission denied"), "{screen}");
        // ...while the sections that were readable are still shown.
        assert!(screen.contains("42 total"), "{screen}");
    }

    #[test]
    fn a_failed_connection_reports_the_reason() {
        let entry = entry();
        let mut snapshot = Inspection::pending(&entry, "payments");
        snapshot.state = ConnectionState::TimedOut;
        snapshot.message = Some("timed out after 8.0s".to_string());

        let screen = render(Some(&entry), InspectionView::Ready(&snapshot));

        assert!(screen.contains("Timed out"), "{screen}");
        assert!(screen.contains("timed out after 8.0s"), "{screen}");
        assert!(screen.contains("not checked"), "{screen}");
        // An unread cluster has no *observed* problems, which is not the same as being healthy.
        assert!(screen.contains("not determined"), "{screen}");
        assert!(!screen.contains("none observed"), "{screen}");
    }

    #[test]
    fn kubeconfig_oddities_are_pointed_out() {
        let mut entry = entry();
        entry.cluster_missing = true;
        entry.ambiguous = true;

        let screen = render(Some(&entry), InspectionView::Idle);

        assert!(screen.contains("not defined in this file"), "{screen}");
        assert!(screen.contains("defines this context name too"), "{screen}");
    }

    #[test]
    fn a_pane_too_short_for_its_content_can_be_scrolled() {
        let entry = entry();
        let snapshot = snapshot(&entry);

        // Twelve rows cannot show everything, so the problems are below the fold...
        let top = render_scrolled(Some(&entry), InspectionView::Ready(&snapshot), 0, 12);
        assert!(top.contains("Cluster"), "{top}");
        assert!(!top.contains("2 Pods not Ready"), "{top}");
        // ...and the title says how far the pane can be scrolled.
        assert!(top.contains("Details  1/"), "{top}");

        // Scrolling down reaches them, and over-scrolling is clamped to the last screenful
        // rather than showing an empty pane.
        let scrolled = render_scrolled(Some(&entry), InspectionView::Ready(&snapshot), 999, 12);
        assert!(scrolled.contains("2 Pods not Ready"), "{scrolled}");
        assert!(scrolled.contains("Problems"), "{scrolled}");
    }

    #[test]
    fn short_content_is_not_marked_scrollable() {
        let entry = entry();
        let screen = render_scrolled(Some(&entry), InspectionView::Idle, 0, 34);

        assert!(screen.contains(" Details "), "{screen}");
        assert!(!screen.contains("Details  1/"), "{screen}");
    }

    #[test]
    fn an_empty_selection_does_not_panic() {
        let screen = render(None, InspectionView::Idle);
        assert!(screen.contains("nothing selected"), "{screen}");
    }
}
