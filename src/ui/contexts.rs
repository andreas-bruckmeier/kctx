//! The context list: a searchable table plus a status/hint line.
//!
//! The body area is shared with the details pane rendered by [`super::inspection`], so opening
//! it is a layout change rather than a different screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Row, Table, TableState};

use crate::app::{AppState, InputMode};
use crate::kubeconfig::ContextEntry;
use crate::kubernetes::inspection::Inspection;
use crate::paths;

/// Marker shown for values a context does not configure.
const UNSET: &str = "-";

/// Persistent view state: scroll offsets belong to the view, not to [`AppState`].
#[derive(Debug, Default)]
pub struct ContextsView {
    table: TableState,
    /// First visible line of the details pane.
    details_scroll: u16,
    /// Largest useful scroll offset, as reported by the last render.
    details_overflow: u16,
    /// Context the pane last rendered, so scrolling resets when the selection moves.
    details_context: String,
}

impl ContextsView {
    /// Render the whole screen.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) {
        let [header, body, search, hints] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);

        self.draw_header(frame, header, state);

        // The details pane shares the body area; the table simply gets narrower.
        if state.details_open {
            let [list, details] =
                Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .areas(body);
            self.draw_table(frame, list, state);

            let selected = state
                .selected()
                .map(|entry| entry.name.clone())
                .unwrap_or_default();
            if selected != self.details_context {
                self.details_context = selected;
                self.details_scroll = 0;
            }
            self.details_overflow = super::inspection::draw(
                frame,
                details,
                state.selected(),
                state.inspection_view(),
                self.details_scroll,
            );
        } else {
            self.draw_table(frame, body, state);
        }

        draw_search(frame, search, state);
        draw_hints(frame, hints, state);
    }

    /// Scroll the details pane, clamped to what the last render said was useful.
    pub fn scroll_details(&mut self, delta: i32) {
        let target = i32::from(self.details_scroll) + delta;
        self.details_scroll = target.clamp(0, i32::from(self.details_overflow)) as u16;
    }

    /// Title line with the match count.
    fn draw_header(&self, frame: &mut Frame, area: Rect, state: &AppState) {
        let total = state.catalog.entries.len();
        let shown = state.visible_len();
        let counter = if shown == total {
            format!("{total}")
        } else {
            format!("{shown}/{total}")
        };

        let line = Line::from(vec![
            Span::styled(" Kubernetes Contexts ", Style::new().bold()),
            Span::styled(format!("({counter}) "), Style::new().dim()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    /// The context table itself.
    fn draw_table(&mut self, frame: &mut Frame, area: Rect, state: &AppState) {
        let rows: Vec<Row<'_>> = state
            .visible()
            .map(|entry| {
                // Once a context has been inspected, surface trouble in the list itself.
                let flagged = state
                    .cached_snapshot(entry)
                    .is_some_and(Inspection::has_problems);
                row(entry, flagged)
            })
            .collect();
        let empty = rows.is_empty();

        let table = Table::new(
            rows,
            [
                Constraint::Length(2),
                Constraint::Min(16),
                Constraint::Min(14),
                Constraint::Min(10),
                Constraint::Fill(1),
            ],
        )
        .header(
            Row::new(vec!["", "CONTEXT", "CLUSTER", "NAMESPACE", "KUBECONFIG"])
                .style(Style::new().dim()),
        )
        .column_spacing(2)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        );

        self.table
            .select(if empty { None } else { Some(state.cursor()) });
        frame.render_stateful_widget(table, area, &mut self.table);

        if empty {
            // Draw the hint inside the bordered area.
            let inner = area.inner(ratatui::layout::Margin::new(2, 2));
            frame.render_widget(
                Paragraph::new(Line::from("no context matches this search").dim()),
                inner,
            );
        }
    }
}

/// One table row for a context.
///
/// The marker column is two characters wide: `*` for the effective context and `!` when the
/// last inspection of it observed a warning or a problem.
fn row(entry: &ContextEntry, flagged: bool) -> Row<'_> {
    let marker = vec![
        if entry.active {
            Span::styled("*", Style::new().fg(Color::Green).bold())
        } else {
            Span::raw(" ")
        },
        if flagged {
            Span::styled("!", Style::new().fg(Color::Red).bold())
        } else {
            Span::raw(" ")
        },
    ];
    let name = if entry.active {
        Span::styled(entry.name.clone(), Style::new().fg(Color::Green))
    } else {
        Span::raw(entry.name.clone())
    };

    Row::new(vec![
        Line::from(marker),
        Line::from(name),
        Line::from(entry.cluster.clone()),
        Line::from(entry.namespace.clone().unwrap_or_else(|| UNSET.to_string())),
        Line::from(Span::styled(
            paths::shorten_home(&entry.source),
            Style::new().dim(),
        )),
    ])
}

/// Search line, shown as a prompt while typing.
fn draw_search(frame: &mut Frame, area: Rect, state: &AppState) {
    let line = match state.mode() {
        InputMode::Search => Line::from(vec![
            Span::styled(" / ", Style::new().fg(Color::Cyan).bold()),
            Span::raw(state.query().to_string()),
            Span::styled("▏", Style::new().fg(Color::Cyan)),
        ]),
        InputMode::Normal if !state.query().is_empty() => Line::from(vec![
            Span::styled(" filter ", Style::new().dim()),
            Span::raw(state.query().to_string()),
        ]),
        InputMode::Normal => Line::default(),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Key hints, adapted to the current mode.
fn draw_hints(frame: &mut Frame, area: Rect, state: &AppState) {
    let hints = match (state.mode(), state.details_open) {
        (InputMode::Search, _) => {
            " type to filter   ↑/↓ move   Enter select   Esc clear   Ctrl-C quit ".to_string()
        }
        (InputMode::Normal, true) => {
            " ↑/↓ jk move   / search   Enter select   i close   r refresh   ^D/^U scroll   q quit "
                .to_string()
        }
        (InputMode::Normal, false) => {
            " ↑/↓ jk move   / search   Enter select   i inspect   q quit ".to_string()
        }
    };
    frame.render_widget(Paragraph::new(Line::from(hints).dim()), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;
    use crate::kubeconfig::{AuthMethod, ContextCatalog};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::Path;
    use std::sync::Arc;

    fn entry(name: &str, cluster: &str, namespace: Option<&str>, active: bool) -> ContextEntry {
        ContextEntry {
            name: name.to_string(),
            cluster: cluster.to_string(),
            user: Some("user".to_string()),
            namespace: namespace.map(str::to_string),
            server: Some("https://example.com".to_string()),
            cluster_missing: false,
            source: Arc::from(Path::new("/k/config")),
            current_in_source: active,
            active,
            ambiguous: false,
            auth_method: AuthMethod::Token,
        }
    }

    fn state() -> AppState {
        AppState::new(ContextCatalog {
            entries: vec![
                entry("production-eu", "prod-cluster", Some("payments"), true),
                entry("staging", "stg-cluster", None, false),
            ],
            active_name: Some("production-eu".to_string()),
            active_index: Some(0),
            ..ContextCatalog::default()
        })
    }

    /// Render into an off-screen buffer and return it as lines of text.
    fn render(state: &AppState) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(90, 14)).unwrap();
        let mut view = ContextsView::default();
        terminal
            .draw(|frame| view.draw(frame, frame.area(), state))
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn renders_every_column_of_every_context() {
        let lines = render(&state());
        let screen = lines.join("\n");

        assert!(screen.contains("Kubernetes Contexts (2)"), "{screen}");
        assert!(screen.contains("CONTEXT"), "{screen}");
        assert!(screen.contains("production-eu"), "{screen}");
        assert!(screen.contains("prod-cluster"), "{screen}");
        assert!(screen.contains("payments"), "{screen}");
        assert!(screen.contains("/k/config"), "{screen}");
        assert!(screen.contains("staging"), "{screen}");
    }

    #[test]
    fn columns_are_aligned_across_rows() {
        let lines = render(&state());
        let production = lines
            .iter()
            .find(|line| line.contains("production-eu "))
            .unwrap();
        let staging = lines.iter().find(|line| line.contains("staging")).unwrap();

        // Each row puts its cluster cell at the same column.
        let expected = production.find("prod-cluster").unwrap();
        assert_eq!(
            staging.find("stg-cluster"),
            Some(expected),
            "{production}\n{staging}"
        );
    }

    #[test]
    fn the_active_context_is_marked_and_unset_namespaces_show_a_dash() {
        let lines = render(&state());
        let production = lines
            .iter()
            .find(|line| line.contains("production-eu "))
            .unwrap();
        let staging = lines
            .iter()
            .find(|line| line.contains("stg-cluster"))
            .unwrap();

        assert!(production.contains('*'), "{production}");
        assert!(!staging.contains('*'), "{staging}");
        // The namespace column of the staging row holds the unset marker.
        let namespace_column = production.find("payments").unwrap();
        assert_eq!(
            staging[namespace_column..namespace_column + 1].to_string(),
            UNSET
        );
    }

    #[test]
    fn hints_follow_the_input_mode() {
        let mut state = state();
        assert!(render(&state).join("\n").contains("q quit"));

        state.set_mode(InputMode::Search);
        state.push_query_char('p');
        let screen = render(&state).join("\n");
        assert!(screen.contains("type to filter"), "{screen}");
        assert!(screen.contains("/ p"), "{screen}");
        assert!(screen.contains("(1/2)"), "{screen}");
    }

    #[test]
    fn the_details_pane_shows_local_facts_and_no_credentials() {
        let mut state = state();
        state.details_open = true;
        let screen = render(&state).join("\n");

        assert!(screen.contains("Details"), "{screen}");
        assert!(screen.contains("https://example.com"), "{screen}");
        assert!(
            screen.contains("token"),
            "auth method should be described: {screen}"
        );
    }

    #[test]
    fn an_empty_result_set_says_so() {
        let mut state = state();
        state.set_query("zzzz");
        let screen = render(&state).join("\n");

        assert!(screen.contains("no context matches"), "{screen}");
        assert!(screen.contains("(0/2)"), "{screen}");
    }
}
