//! Application state shared by the TUI and the non-interactive commands.
//!
//! This module owns *what the user is looking at*: the catalog, the current query, the visible
//! selection. It deliberately depends on neither `ratatui` nor `kube`, so both the terminal
//! layer and the Kubernetes layer stay replaceable and testable.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::filter;
use crate::kubeconfig::{ContextCatalog, ContextEntry};
use crate::kubernetes::inspection::Inspection;

/// How long a snapshot stays usable before it is fetched again.
///
/// Long enough that moving up and down the list does not hammer an API server, short enough
/// that what is on screen is still recognisably current.
const CACHE_TTL: Duration = Duration::from_secs(30);

/// Identifies the snapshot of one context: the same name in two kubeconfigs, or the same
/// context viewed with a different namespace, are different things.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InspectionKey {
    /// Context name.
    pub context: String,
    /// Defining kubeconfig.
    pub source: PathBuf,
    /// Namespace the snapshot covers.
    pub namespace: String,
}

impl InspectionKey {
    /// The key for a context inspected with its own namespace.
    pub fn of(entry: &ContextEntry) -> Self {
        Self {
            context: entry.name.clone(),
            source: entry.source.to_path_buf(),
            namespace: entry.effective_namespace().to_string(),
        }
    }
}

/// What the details pane should show for the current selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectionView<'a> {
    /// Nothing requested yet: only local kubeconfig facts are known.
    Idle,
    /// A request is in flight.
    Connecting,
    /// A snapshot is available.
    Ready(&'a Inspection),
}

/// A cached snapshot and when it was taken.
#[derive(Debug)]
struct CacheEntry {
    fetched: Instant,
    snapshot: Inspection,
}

/// Whether keystrokes navigate the list or extend the search query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    /// Single-letter commands: `j`/`k` move, `q` quits, any other letter starts a search.
    #[default]
    Normal,
    /// Printable characters are appended to the query.
    Search,
}

/// Interactive state over a loaded [`ContextCatalog`].
#[derive(Debug)]
pub struct AppState {
    /// Everything discovered locally.
    pub catalog: ContextCatalog,
    /// Current search query.
    query: String,
    /// Indices into `catalog.entries`, best match first.
    visible: Vec<usize>,
    /// Cursor position within `visible`.
    cursor: usize,
    /// Whether keystrokes are search text or commands.
    mode: InputMode,
    /// Whether the details pane is shown.
    pub details_open: bool,
    /// Recently fetched snapshots, keyed by context.
    inspections: HashMap<InspectionKey, CacheEntry>,
    /// Requests currently in flight, so the same context is not fetched twice at once.
    in_flight: HashSet<InspectionKey>,
}

impl AppState {
    /// Start with every context visible and the cursor on the active one.
    pub fn new(catalog: ContextCatalog) -> Self {
        let visible = filter::filter(&catalog.entries, "")
            .into_iter()
            .map(|m| m.index)
            .collect();
        let mut state = Self {
            catalog,
            query: String::new(),
            visible,
            cursor: 0,
            mode: InputMode::Normal,
            details_open: false,
            inspections: HashMap::new(),
            in_flight: HashSet::new(),
        };
        if let Some(active) = state.catalog.active_index {
            state.select_entry(active);
        }
        state
    }

    /// The current query.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Whether keystrokes are currently search text or commands.
    pub fn mode(&self) -> InputMode {
        self.mode
    }

    /// Switch between command and search input.
    pub fn set_mode(&mut self, mode: InputMode) {
        self.mode = mode;
    }

    /// Contexts currently passing the filter, best match first.
    pub fn visible(&self) -> impl Iterator<Item = &ContextEntry> {
        self.visible
            .iter()
            .map(|index| &self.catalog.entries[*index])
    }

    /// How many contexts pass the filter.
    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    /// Cursor position within the visible list.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The context under the cursor, if any context is visible.
    pub fn selected(&self) -> Option<&ContextEntry> {
        self.visible
            .get(self.cursor)
            .map(|index| &self.catalog.entries[*index])
    }

    /// Replace the query, keeping the cursor on the same context where possible.
    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
        let previous = self.visible.get(self.cursor).copied();
        self.visible = filter::filter(&self.catalog.entries, &self.query)
            .into_iter()
            .map(|m| m.index)
            .collect();
        self.cursor = previous
            .and_then(|entry| self.visible.iter().position(|index| *index == entry))
            .unwrap_or(0);
    }

    /// Append a character typed by the user.
    pub fn push_query_char(&mut self, character: char) {
        let mut query = std::mem::take(&mut self.query);
        query.push(character);
        self.set_query(query);
    }

    /// Delete the last character of the query.
    pub fn pop_query_char(&mut self) {
        let mut query = std::mem::take(&mut self.query);
        query.pop();
        self.set_query(query);
    }

    /// Clear the query. Returns true if anything changed.
    pub fn clear_query(&mut self) -> bool {
        if self.query.is_empty() {
            return false;
        }
        self.set_query(String::new());
        true
    }

    /// Move the cursor by `delta`, clamped to the visible list.
    pub fn move_cursor(&mut self, delta: isize) {
        if self.visible.is_empty() {
            self.cursor = 0;
            return;
        }
        let last = self.visible.len() - 1;
        let target = self.cursor as isize + delta;
        self.cursor = target.clamp(0, last as isize) as usize;
    }

    /// Put the cursor on the first visible context.
    pub fn cursor_to_start(&mut self) {
        self.cursor = 0;
    }

    /// Put the cursor on the last visible context.
    pub fn cursor_to_end(&mut self) {
        self.cursor = self.visible.len().saturating_sub(1);
    }

    /// The cached snapshot for `entry`, if one was taken recently enough to still show.
    pub fn cached_snapshot(&self, entry: &ContextEntry) -> Option<&Inspection> {
        self.inspections
            .get(&InspectionKey::of(entry))
            .filter(|cached| cached.fetched.elapsed() < CACHE_TTL)
            .map(|cached| &cached.snapshot)
    }

    /// What the details pane should render for the context under the cursor.
    pub fn inspection_view(&self) -> InspectionView<'_> {
        let Some(entry) = self.selected() else {
            return InspectionView::Idle;
        };

        if let Some(snapshot) = self.cached_snapshot(entry) {
            return InspectionView::Ready(snapshot);
        }
        if self.in_flight.contains(&InspectionKey::of(entry)) {
            return InspectionView::Connecting;
        }
        InspectionView::Idle
    }

    /// The context that should be inspected next, if any.
    ///
    /// Returns `None` when the pane is closed, nothing is selected, a fresh snapshot is already
    /// cached, or a request is already running — so callers can simply ask on every keystroke.
    pub fn inspection_due(&self) -> Option<&ContextEntry> {
        if !self.details_open {
            return None;
        }
        match self.inspection_view() {
            InspectionView::Idle => self.selected(),
            InspectionView::Connecting | InspectionView::Ready(_) => None,
        }
    }

    /// Record that a request for `entry` has started.
    pub fn mark_in_flight(&mut self, entry: &ContextEntry) {
        self.in_flight.insert(InspectionKey::of(entry));
    }

    /// Store a completed snapshot, replacing any older one.
    pub fn store_inspection(&mut self, snapshot: Inspection, key: InspectionKey) {
        self.in_flight.remove(&key);
        self.inspections.insert(
            key,
            CacheEntry {
                fetched: Instant::now(),
                snapshot,
            },
        );
    }

    /// Drop the cached snapshot for the current selection so it is fetched again.
    pub fn invalidate_selected(&mut self) {
        if let Some(entry) = self.selected() {
            let key = InspectionKey::of(entry);
            self.inspections.remove(&key);
        }
    }

    /// Move the cursor onto a specific catalog entry if it is visible.
    fn select_entry(&mut self, entry_index: usize) {
        if let Some(position) = self.visible.iter().position(|index| *index == entry_index) {
            self.cursor = position;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubeconfig::AuthMethod;
    use std::path::Path;
    use std::sync::Arc;

    fn entry(name: &str) -> ContextEntry {
        ContextEntry {
            name: name.to_string(),
            cluster: format!("{name}-cluster"),
            user: None,
            namespace: None,
            server: None,
            cluster_missing: false,
            source: Arc::from(Path::new("/k/config")),
            current_in_source: false,
            active: false,
            ambiguous: false,
            auth_method: AuthMethod::Unspecified,
        }
    }

    fn state() -> AppState {
        AppState::new(ContextCatalog {
            entries: vec![entry("alpha"), entry("beta"), entry("gamma")],
            ..ContextCatalog::default()
        })
    }

    #[test]
    fn starts_on_the_active_context() {
        let state = AppState::new(ContextCatalog {
            entries: vec![entry("alpha"), entry("beta")],
            active_name: Some("beta".to_string()),
            active_index: Some(1),
            ..ContextCatalog::default()
        });

        assert_eq!(state.selected().unwrap().name, "beta");
    }

    #[test]
    fn cursor_is_clamped_to_the_visible_list() {
        let mut state = state();

        state.move_cursor(-5);
        assert_eq!(state.cursor(), 0);
        state.move_cursor(99);
        assert_eq!(state.cursor(), 2);
        assert_eq!(state.selected().unwrap().name, "gamma");
    }

    #[test]
    fn typing_narrows_the_list() {
        let mut state = state();

        state.push_query_char('b');
        assert_eq!(state.visible_len(), 1);
        assert_eq!(state.selected().unwrap().name, "beta");

        state.pop_query_char();
        assert_eq!(state.visible_len(), 3);
    }

    #[test]
    fn cursor_follows_the_selected_context_while_filtering() {
        let mut state = state();
        state.move_cursor(2);
        assert_eq!(state.selected().unwrap().name, "gamma");

        // "gamma" still matches "amm", so the cursor must stay on it rather than reset.
        state.set_query("amm");
        assert_eq!(state.selected().unwrap().name, "gamma");
    }

    #[test]
    fn cursor_resets_when_the_selected_context_is_filtered_away() {
        let mut state = state();
        state.move_cursor(2);

        // "alp" matches nothing in "beta" or "gamma", so the cursor cannot stay where it was.
        state.set_query("alp");
        assert_eq!(state.visible_len(), 1);
        assert_eq!(state.cursor(), 0);
        assert_eq!(state.selected().unwrap().name, "alpha");
    }

    #[test]
    fn nothing_is_selected_when_nothing_matches() {
        let mut state = state();
        state.set_query("zzz");

        assert_eq!(state.visible_len(), 0);
        assert!(state.selected().is_none());
        state.move_cursor(1);
        assert_eq!(state.cursor(), 0);
    }

    fn snapshot(entry: &ContextEntry) -> Inspection {
        Inspection::pending(entry, entry.effective_namespace())
    }

    #[test]
    fn nothing_is_inspected_while_the_details_pane_is_closed() {
        let state = state();

        assert!(!state.details_open);
        assert!(state.inspection_due().is_none());
        assert_eq!(state.inspection_view(), InspectionView::Idle);
    }

    #[test]
    fn opening_the_pane_makes_an_inspection_due_exactly_once() {
        let mut state = state();
        state.details_open = true;

        let entry = state
            .inspection_due()
            .expect("the selection should be due")
            .clone();
        assert_eq!(entry.name, "alpha");

        state.mark_in_flight(&entry);
        assert!(
            state.inspection_due().is_none(),
            "must not fetch the same context twice"
        );
        assert_eq!(state.inspection_view(), InspectionView::Connecting);
    }

    #[test]
    fn a_stored_snapshot_is_served_from_the_cache() {
        let mut state = state();
        state.details_open = true;
        let entry = state.selected().unwrap().clone();

        state.mark_in_flight(&entry);
        state.store_inspection(snapshot(&entry), InspectionKey::of(&entry));

        assert!(matches!(state.inspection_view(), InspectionView::Ready(_)));
        assert!(
            state.inspection_due().is_none(),
            "a fresh snapshot must not be refetched"
        );
    }

    #[test]
    fn refreshing_drops_the_cached_snapshot() {
        let mut state = state();
        state.details_open = true;
        let entry = state.selected().unwrap().clone();
        state.store_inspection(snapshot(&entry), InspectionKey::of(&entry));

        state.invalidate_selected();

        assert_eq!(state.inspection_view(), InspectionView::Idle);
        assert!(state.inspection_due().is_some());
    }

    #[test]
    fn each_context_is_cached_separately() {
        let mut state = state();
        state.details_open = true;
        let alpha = state.selected().unwrap().clone();
        state.store_inspection(snapshot(&alpha), InspectionKey::of(&alpha));

        state.move_cursor(1);
        assert_eq!(state.selected().unwrap().name, "beta");
        assert_eq!(
            state.inspection_view(),
            InspectionView::Idle,
            "beta has no snapshot yet"
        );

        state.move_cursor(-1);
        assert!(matches!(state.inspection_view(), InspectionView::Ready(_)));
    }

    #[test]
    fn identical_context_names_from_different_files_are_distinct_cache_entries() {
        let mut first = entry("dup");
        first.source = Arc::from(Path::new("/k/a.yaml"));
        let mut second = entry("dup");
        second.source = Arc::from(Path::new("/k/b.yaml"));

        assert_ne!(InspectionKey::of(&first), InspectionKey::of(&second));

        let mut state = AppState::new(ContextCatalog {
            entries: vec![first.clone(), second],
            ..ContextCatalog::default()
        });
        state.details_open = true;
        state.store_inspection(snapshot(&first), InspectionKey::of(&first));

        assert!(matches!(state.inspection_view(), InspectionView::Ready(_)));
        state.move_cursor(1);
        assert_eq!(state.inspection_view(), InspectionView::Idle);
    }

    #[test]
    fn clearing_reports_whether_it_changed_anything() {
        let mut state = state();
        assert!(!state.clear_query());

        state.push_query_char('b');
        assert!(state.clear_query());
        assert_eq!(state.query(), "");
        assert_eq!(state.visible_len(), 3);
    }
}
