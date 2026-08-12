//! Terminal user interface.
//!
//! Two rules shape this module:
//!
//! * **The TUI renders to stderr.** stdout is the machine-readable channel a shell wrapper
//!   captures, so drawing there would corrupt the result. `ratatui::init()` hard-codes stdout,
//!   so the terminal is constructed by hand over [`std::io::stderr`].
//! * **No Kubernetes logic lives here.** The UI reads [`AppState`] and renders it; anything
//!   that talks to a cluster happens elsewhere and arrives as plain data.

pub mod contexts;
pub mod inspection;

use std::io::{IsTerminal, Stderr, stderr};
use std::sync::OnceLock;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};
use tokio::sync::mpsc;

use crate::app::{AppState, InputMode, InspectionKey};
use crate::kubeconfig::ContextEntry;
use crate::kubernetes::client::Timeouts;
use crate::kubernetes::inspection::{Inspection, inspect};

/// What a key press asks the application to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Keep going.
    Continue,
    /// Scroll the details pane by this many lines.
    ScrollDetails(i32),
    /// Take the context under the cursor.
    Select,
    /// Leave without selecting anything.
    Cancel,
}

/// Run the interactive selector, returning the chosen context or `None` if cancelled.
pub async fn select(state: &mut AppState) -> anyhow::Result<Option<ContextEntry>> {
    if !stderr().is_terminal() {
        anyhow::bail!(
            "stderr is not a terminal, so the interactive selector cannot be shown; \
             pass a context name instead (`kctx select <context>`)"
        );
    }

    let mut terminal = TerminalGuard::new()?;
    let mut events = input_events();
    let mut view = contexts::ContextsView::default();
    // Snapshots arrive here from background tasks; the loop never waits on a cluster.
    let (snapshots, mut results) = mpsc::unbounded_channel::<(InspectionKey, Inspection)>();

    loop {
        request_inspection(state, &snapshots);
        terminal.draw(&mut view, state)?;

        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else {
                    // The input thread ended (stdin closed); treat it as a cancellation.
                    return Ok(None);
                };
                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                {
                    match handle_key(state, key) {
                        Action::Continue => {}
                        Action::ScrollDetails(delta) => view.scroll_details(delta),
                        Action::Select => return Ok(state.selected().cloned()),
                        Action::Cancel => return Ok(None),
                    }
                }
                // Anything else (resize, mouse, focus) is handled by redrawing.
            }
            Some((key, snapshot)) = results.recv() => {
                tracing::debug!(context = %key.context, state = %snapshot.state, "snapshot received");
                state.store_inspection(snapshot, key);
            }
        }
    }
}

/// Start a background inspection if the details pane needs one.
///
/// Called on every iteration: [`AppState::inspection_due`] answers `None` unless the pane is
/// open, the selection has no fresh snapshot, and nothing is already in flight.
fn request_inspection(
    state: &mut AppState,
    snapshots: &mpsc::UnboundedSender<(InspectionKey, Inspection)>,
) {
    let Some(entry) = state.inspection_due().cloned() else {
        return;
    };
    state.mark_in_flight(&entry);

    let sender = snapshots.clone();
    let key = InspectionKey::of(&entry);
    tokio::spawn(async move {
        let snapshot = inspect(&entry, None, Timeouts::default()).await;
        // A closed channel just means the user has already left the selector.
        let _ = sender.send((key, snapshot));
    });
}

/// Translate a key press into an [`Action`], mutating the state as needed.
///
/// Two modes keep single-letter commands and free-text search from fighting over the keyboard:
/// in `Normal` mode `j`/`k` navigate and any other printable character starts a search; in
/// `Search` mode every printable character is literal text.
fn handle_key(state: &mut AppState, key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Action::Cancel,
            KeyCode::Char('n') => nav(state, 1),
            KeyCode::Char('p') => nav(state, -1),
            // Scrolling the details pane, which can be taller than the screen.
            KeyCode::Char('d') => Action::ScrollDetails(5),
            KeyCode::Char('u') => Action::ScrollDetails(-5),
            _ => Action::Continue,
        };
    }

    match key.code {
        KeyCode::Enter => Action::Select,
        KeyCode::Up => nav(state, -1),
        KeyCode::Down => nav(state, 1),
        KeyCode::PageUp => nav(state, -10),
        KeyCode::PageDown => nav(state, 10),
        KeyCode::Home => {
            state.cursor_to_start();
            Action::Continue
        }
        KeyCode::End => {
            state.cursor_to_end();
            Action::Continue
        }
        KeyCode::Esc => {
            // First Esc drops the filter, a second one leaves.
            if state.clear_query() {
                state.set_mode(InputMode::Normal);
                Action::Continue
            } else {
                Action::Cancel
            }
        }
        KeyCode::Backspace => {
            state.pop_query_char();
            if state.query().is_empty() {
                state.set_mode(InputMode::Normal);
            }
            Action::Continue
        }
        KeyCode::Char(character) => match state.mode() {
            InputMode::Search => {
                state.push_query_char(character);
                Action::Continue
            }
            InputMode::Normal => normal_mode_char(state, character),
        },
        _ => Action::Continue,
    }
}

/// Handle a printable character in normal mode.
fn normal_mode_char(state: &mut AppState, character: char) -> Action {
    match character {
        'q' => Action::Cancel,
        'j' => nav(state, 1),
        'k' => nav(state, -1),
        'g' => {
            state.cursor_to_start();
            Action::Continue
        }
        'G' => {
            state.cursor_to_end();
            Action::Continue
        }
        'i' => {
            state.details_open = !state.details_open;
            Action::Continue
        }
        'r' => {
            // Only meaningful with the pane open, where the next loop iteration refetches.
            state.invalidate_selected();
            Action::Continue
        }
        '/' => {
            state.set_mode(InputMode::Search);
            Action::Continue
        }
        // Anything else starts a search with that character, so users can just type.
        _ => {
            state.set_mode(InputMode::Search);
            state.push_query_char(character);
            Action::Continue
        }
    }
}

/// Move the cursor and keep going.
fn nav(state: &mut AppState, delta: isize) -> Action {
    state.move_cursor(delta);
    Action::Continue
}

/// Read terminal events on a dedicated thread and forward them to the async loop.
///
/// `event::read` blocks, which would stall the runtime; a thread keeps the loop free to react
/// to background work. The thread is intentionally not joined — it is blocked on stdin and the
/// process exits immediately after the selector returns.
fn input_events() -> mpsc::UnboundedReceiver<Event> {
    let (sender, receiver) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("kctx-input".to_string())
        .spawn(move || {
            while let Ok(event) = event::read() {
                if sender.send(event).is_err() {
                    break;
                }
            }
        })
        .expect("spawning the input thread");
    receiver
}

/// Owns the terminal and restores it on drop, including on panic.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stderr>>,
}

impl TerminalGuard {
    /// Enter raw mode and the alternate screen on stderr.
    fn new() -> anyhow::Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        execute!(stderr(), EnterAlternateScreen, cursor::Hide)?;
        Ok(Self {
            terminal: Terminal::new(CrosstermBackend::new(stderr()))?,
        })
    }

    /// Render one frame.
    fn draw(&mut self, view: &mut contexts::ContextsView, state: &AppState) -> anyhow::Result<()> {
        self.terminal
            .draw(|frame| view.draw(frame, frame.area(), state))?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Undo everything [`TerminalGuard::new`] did. Safe to call more than once.
fn restore_terminal() {
    let _ = execute!(stderr(), LeaveAlternateScreen, cursor::Show);
    let _ = disable_raw_mode();
}

/// Make sure a panic cannot leave the user with a broken terminal.
fn install_panic_hook() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubeconfig::{AuthMethod, ContextCatalog};
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

    fn press(state: &mut AppState, code: KeyCode) -> Action {
        handle_key(state, KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_char(state: &mut AppState, character: char) -> Action {
        press(state, KeyCode::Char(character))
    }

    #[test]
    fn arrows_and_jk_navigate_in_normal_mode() {
        let mut state = state();

        assert_eq!(press(&mut state, KeyCode::Down), Action::Continue);
        assert_eq!(state.selected().unwrap().name, "beta");
        type_char(&mut state, 'j');
        assert_eq!(state.selected().unwrap().name, "gamma");
        type_char(&mut state, 'k');
        assert_eq!(state.selected().unwrap().name, "beta");
        press(&mut state, KeyCode::Up);
        assert_eq!(state.selected().unwrap().name, "alpha");
    }

    #[test]
    fn q_quits_in_normal_mode_but_is_text_in_search_mode() {
        let mut state = state();
        assert_eq!(type_char(&mut state, 'q'), Action::Cancel);

        let mut state = self::state();
        type_char(&mut state, '/');
        assert_eq!(type_char(&mut state, 'q'), Action::Continue);
        assert_eq!(state.query(), "q");
    }

    #[test]
    fn typing_a_letter_starts_a_search() {
        let mut state = state();

        type_char(&mut state, 'b');
        assert_eq!(state.mode(), InputMode::Search);
        assert_eq!(state.query(), "b");
        assert_eq!(state.selected().unwrap().name, "beta");

        // In search mode j and k are literal text, not navigation.
        type_char(&mut state, 'j');
        assert_eq!(state.query(), "bj");
        assert_eq!(state.visible_len(), 0);
    }

    #[test]
    fn slash_enters_search_mode_without_adding_a_character() {
        let mut state = state();
        type_char(&mut state, '/');

        assert_eq!(state.mode(), InputMode::Search);
        assert_eq!(state.query(), "");
        assert_eq!(state.visible_len(), 3);
    }

    #[test]
    fn escape_clears_the_filter_then_cancels() {
        let mut state = state();
        type_char(&mut state, 'b');

        assert_eq!(press(&mut state, KeyCode::Esc), Action::Continue);
        assert_eq!(state.query(), "");
        assert_eq!(state.mode(), InputMode::Normal);
        assert_eq!(press(&mut state, KeyCode::Esc), Action::Cancel);
    }

    #[test]
    fn backspacing_out_of_a_query_returns_to_normal_mode() {
        let mut state = state();
        type_char(&mut state, 'b');

        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.query(), "");
        assert_eq!(state.mode(), InputMode::Normal);
        // Normal-mode commands work again.
        assert_eq!(type_char(&mut state, 'q'), Action::Cancel);
    }

    #[test]
    fn enter_selects_and_ctrl_c_cancels() {
        let mut state = state();
        assert_eq!(press(&mut state, KeyCode::Enter), Action::Select);

        let action = handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert_eq!(action, Action::Cancel);
    }

    #[test]
    fn i_toggles_the_details_pane() {
        let mut state = state();
        assert!(!state.details_open);

        type_char(&mut state, 'i');
        assert!(state.details_open);
        type_char(&mut state, 'i');
        assert!(!state.details_open);
    }

    #[test]
    fn home_end_and_g_jump_to_the_ends_of_the_list() {
        let mut state = state();

        press(&mut state, KeyCode::End);
        assert_eq!(state.selected().unwrap().name, "gamma");
        press(&mut state, KeyCode::Home);
        assert_eq!(state.selected().unwrap().name, "alpha");
        type_char(&mut state, 'G');
        assert_eq!(state.selected().unwrap().name, "gamma");
        type_char(&mut state, 'g');
        assert_eq!(state.selected().unwrap().name, "alpha");
    }
}
