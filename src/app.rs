//! Top-level TUI wiring: frame layout, pure input routing, and event
//! application.
//!
//! # State access
//!
//! This module **never** locks anything and never holds a state handle. It
//! reads a single consistent snapshot per frame (a cheap `Clone` of
//! `AppState`) and performs every mutation through the domain actions in
//! [`crate::state`]. Input handling is split into a *pure* routing function
//! ([`route_key`]) that maps `(Focus, key)` to an [`Action`] with no side
//! effects — so key handling is unit-testable without a terminal — and an
//! [`apply_action`] step that turns an [`Action`] into the corresponding state
//! action call.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Span,
    widgets::Paragraph,
    Frame,
};

use crate::{
    state::{self, AnalysisMode, AppState, Direction, Edit, Focus, FocusMove, Nav, StateError},
    widgets,
};

/// One frame's worth of layout hit-test rectangles that the event loop needs to
/// remember between draws (the footer buttons, for mouse clicks).
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameHits {
    /// Screen rect occupied by the "always allows" mode button, returned by
    /// [`widgets::policy_input::render`], used to hit-test mouse clicks.
    pub always_allows_button: Rect,
    /// Screen rect occupied by the "never allows" mode button.
    pub never_allows_button: Rect,
    /// Screen rect occupied by the clear button, returned by
    /// [`widgets::policy_input::render`], used to hit-test mouse clicks.
    pub clear_button: Rect,
}

/// A fully-decoded, side-effect-free description of what a key press means in a
/// given focus context. Produced by [`route_key`] and consumed by
/// [`apply_action`]; keeping the two separate makes routing unit-testable
/// without a live terminal or shared state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Quit the application.
    Quit,
    /// Move keyboard focus to the next widget (`Tab`).
    FocusNext,
    /// Move keyboard focus to the previous widget (`Shift+Tab`).
    FocusPrev,
    /// Move the schema-list cursor.
    MoveSchemaCursor(Direction),
    /// Move the request-env-list cursor.
    MoveEnvCursor(Direction),
    /// Scroll the output pane.
    ScrollOutput(Direction),
    /// Load the schema currently under the schema cursor (`Enter`).
    LoadSelectedSchema,
    /// Clear the policy input and output.
    ClearPolicy,
    /// Switch the analysis mode used by the encoder.
    SetMode(AnalysisMode),
    /// Insert a typed character into the policy buffer.
    PolicyChar(char),
    /// Insert a newline into the policy buffer.
    PolicyNewline,
    /// Delete the last character of the policy buffer.
    PolicyBackspace,
}

/// Pure routing: map a `(focus, key)` pair to an optional [`Action`].
///
/// Returns `None` for keys that are not bound in the given context. This
/// function performs **no** state mutation and touches no lock, so it can be
/// exercised directly in unit tests. Global bindings (quit, focus cycling,
/// output paging, clear shortcut) are handled first; the remainder dispatch on
/// the focused widget.
pub fn route_key(focus: Focus, key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // ---- Global bindings (context-independent) --------------------------
    match key.code {
        // Ctrl+C / Ctrl+Q always quit.
        KeyCode::Char('c') | KeyCode::Char('q') if ctrl => return Some(Action::Quit),
        // Ctrl+L clears the policy from anywhere.
        KeyCode::Char('l') if ctrl => return Some(Action::ClearPolicy),
        // Tab / Shift+Tab (crossterm delivers Shift+Tab as BackTab) cycle focus.
        KeyCode::Tab if shift => return Some(Action::FocusPrev),
        KeyCode::Tab => return Some(Action::FocusNext),
        KeyCode::BackTab => return Some(Action::FocusPrev),
        // PageUp/PageDown scroll the output pane regardless of focus.
        KeyCode::PageUp => return Some(Action::ScrollOutput(Direction::Up)),
        KeyCode::PageDown => return Some(Action::ScrollOutput(Direction::Down)),
        // Esc quits UNLESS the policy editor is focused, so it can never destroy
        // a paste/edit in progress (documented in the footer).
        KeyCode::Esc if !matches!(focus, Focus::PolicyInput) => return Some(Action::Quit),
        _ => {}
    }

    // ---- Focus-specific bindings ----------------------------------------
    match focus {
        Focus::SchemaSelector => match key.code {
            KeyCode::Up | KeyCode::Char('k') => Some(Action::MoveSchemaCursor(Direction::Up)),
            KeyCode::Down | KeyCode::Char('j') => Some(Action::MoveSchemaCursor(Direction::Down)),
            KeyCode::Enter => Some(Action::LoadSelectedSchema),
            _ => None,
        },
        Focus::EnvSelector => match key.code {
            KeyCode::Up | KeyCode::Char('k') => Some(Action::MoveEnvCursor(Direction::Up)),
            KeyCode::Down | KeyCode::Char('j') => Some(Action::MoveEnvCursor(Direction::Down)),
            _ => None,
        },
        Focus::OutputPane => match key.code {
            KeyCode::Up | KeyCode::Char('k') => Some(Action::ScrollOutput(Direction::Up)),
            KeyCode::Down | KeyCode::Char('j') => Some(Action::ScrollOutput(Direction::Down)),
            _ => None,
        },
        Focus::ClearButton => match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => Some(Action::ClearPolicy),
            _ => None,
        },
        Focus::AlwaysAllowsButton => match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => {
                Some(Action::SetMode(AnalysisMode::AlwaysAllows))
            }
            _ => None,
        },
        Focus::NeverAllowsButton => match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => Some(Action::SetMode(AnalysisMode::NeverAllows)),
            _ => None,
        },
        Focus::PolicyInput => match key.code {
            KeyCode::Enter => Some(Action::PolicyNewline),
            KeyCode::Backspace => Some(Action::PolicyBackspace),
            // Only take *unmodified* chars so bindings like Ctrl+L are not
            // swallowed as literal text.
            KeyCode::Char(c) if !ctrl => Some(Action::PolicyChar(c)),
            _ => None,
        },
    }
}

/// Apply a routed [`Action`] to the global state via the domain actions.
///
/// Exactly one `state::` action per [`Action`] variant. Returns the action's
/// [`Result`]; the event loop treats any [`StateError`] as benign (the UI
/// already reflects the resulting state) and ignores it. [`Action::Quit`] is
/// handled by the event loop *before* this is called, so its arm here is an
/// inert `Ok`.
///
/// Navigation maps onto [`state::navigate`]: schema/env cursor moves become
/// [`Nav::Up`]/[`Nav::Down`] (dispatched on the focused pane), and output scroll
/// — which also fires from `PageUp`/`PageDown` in any focus — becomes
/// [`Nav::PageUp`]/[`Nav::PageDown`] so it always targets the output pane.
pub fn apply_action(action: Action) -> Result<(), StateError> {
    match action {
        // Quit is intercepted by the event loop; nothing to do at the state layer.
        Action::Quit => Ok(()),
        Action::FocusNext => state::cycle_focus(FocusMove::Next),
        Action::FocusPrev => state::cycle_focus(FocusMove::Prev),
        Action::MoveSchemaCursor(dir) | Action::MoveEnvCursor(dir) => state::navigate(match dir {
            Direction::Up => Nav::Up,
            Direction::Down => Nav::Down,
        }),
        Action::ScrollOutput(dir) => state::navigate(match dir {
            Direction::Up => Nav::PageUp,
            Direction::Down => Nav::PageDown,
        }),
        Action::ClearPolicy => state::clear_policy(),
        Action::SetMode(mode) => state::set_analysis_mode(mode),
        Action::PolicyChar(c) => state::edit_policy(Edit::Char(c)),
        Action::PolicyNewline => state::edit_policy(Edit::Newline),
        Action::PolicyBackspace => state::edit_policy(Edit::Backspace),
        Action::LoadSelectedSchema => state::load_schema_under_cursor(),
    }
}

/// Draw one full frame from an [`AppState`] snapshot and return the frame's
/// hit-test rectangles.
///
/// Layout (top to bottom):
/// * row 1 — fixed 12 lines (10 content + border), split 50/50 horizontally
///   into the schema selector | request-env selector;
/// * row 2 — the policy input editor, flexible, minimum 8 lines;
/// * row 3 — the scrollable output pane, taking all remaining space;
/// * row 4 — a one-line help/status footer with the key bindings.
///
/// The caller passes a single [`state::snapshot`] taken once per frame, so the
/// lock is not held during rendering and the widgets can never observe a torn
/// state.
pub fn draw(frame: &mut Frame, s: &AppState) -> FrameHits {
    let [top, policy_area, output_area, footer_area] = Layout::vertical([
        Constraint::Length(12), // 10 content lines + top/bottom border
        Constraint::Min(8),     // policy editor, flexible with an 8-line floor
        Constraint::Min(0),     // output pane takes the rest
        Constraint::Length(1),  // help/status footer
    ])
    .areas(frame.area());

    // Row 1: schema selector | env selector, side by side 50/50.
    let [schema_area, env_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top);

    widgets::schema_selector::render(frame, schema_area, s);
    widgets::env_selector::render(frame, env_area, s);
    let (always_allows_button, never_allows_button, clear_button) =
        widgets::policy_input::render(frame, policy_area, s);
    widgets::output_pane::render(frame, output_area, s);
    render_footer(frame, footer_area, s);

    FrameHits {
        always_allows_button,
        never_allows_button,
        clear_button,
    }
}

/// Render the one-line key-binding/status footer.
fn render_footer(frame: &mut Frame, area: Rect, s: &AppState) {
    // Esc's behaviour depends on focus, so describe it honestly.
    let esc = if matches!(s.focus(), Focus::PolicyInput) {
        "Esc:type"
    } else {
        "Esc:quit"
    };
    let help = format!(
        "Tab/S-Tab:focus  ↑↓/kj:move  Enter:load  \
         PgUp/PgDn:scroll  Ctrl+L:clear  Ctrl+C/Ctrl+Q:quit  {esc}"
    );
    let footer = Paragraph::new(Span::styled(
        help,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(footer, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn key_mod(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn global_quit_bindings_win_in_every_focus() {
        for focus in [
            Focus::SchemaSelector,
            Focus::EnvSelector,
            Focus::PolicyInput,
            Focus::ClearButton,
            Focus::OutputPane,
        ] {
            assert_eq!(
                route_key(focus, key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                Some(Action::Quit),
                "Ctrl+C must quit from {focus:?}"
            );
            assert_eq!(
                route_key(focus, key_mod(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Some(Action::Quit),
                "Ctrl+Q must quit from {focus:?}"
            );
            assert_eq!(
                route_key(focus, key_mod(KeyCode::Char('l'), KeyModifiers::CONTROL)),
                Some(Action::ClearPolicy),
                "Ctrl+L must clear from {focus:?}"
            );
        }
    }

    #[test]
    fn esc_quits_everywhere_except_policy_input() {
        assert_eq!(
            route_key(Focus::SchemaSelector, key(KeyCode::Esc)),
            Some(Action::Quit)
        );
        // In the policy editor, Esc is NOT quit (so it can't destroy a paste).
        assert_eq!(route_key(Focus::PolicyInput, key(KeyCode::Esc)), None);
    }

    #[test]
    fn tab_and_backtab_cycle_focus() {
        assert_eq!(
            route_key(Focus::SchemaSelector, key(KeyCode::Tab)),
            Some(Action::FocusNext)
        );
        assert_eq!(
            route_key(Focus::SchemaSelector, key(KeyCode::BackTab)),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            route_key(
                Focus::SchemaSelector,
                key_mod(KeyCode::Tab, KeyModifiers::SHIFT)
            ),
            Some(Action::FocusPrev)
        );
    }

    #[test]
    fn pageup_pagedown_scroll_output_from_any_focus() {
        assert_eq!(
            route_key(Focus::PolicyInput, key(KeyCode::PageUp)),
            Some(Action::ScrollOutput(Direction::Up))
        );
        assert_eq!(
            route_key(Focus::EnvSelector, key(KeyCode::PageDown)),
            Some(Action::ScrollOutput(Direction::Down))
        );
    }

    #[test]
    fn list_navigation_uses_arrows_and_kj() {
        assert_eq!(
            route_key(Focus::SchemaSelector, key(KeyCode::Down)),
            Some(Action::MoveSchemaCursor(Direction::Down))
        );
        assert_eq!(
            route_key(Focus::SchemaSelector, key(KeyCode::Char('k'))),
            Some(Action::MoveSchemaCursor(Direction::Up))
        );
        assert_eq!(
            route_key(Focus::EnvSelector, key(KeyCode::Char('j'))),
            Some(Action::MoveEnvCursor(Direction::Down))
        );
        assert_eq!(
            route_key(Focus::SchemaSelector, key(KeyCode::Enter)),
            Some(Action::LoadSelectedSchema)
        );
        assert_eq!(route_key(Focus::EnvSelector, key(KeyCode::Char(' '))), None);
    }

    #[test]
    fn policy_input_typing_maps_to_edit_actions() {
        assert_eq!(
            route_key(Focus::PolicyInput, key(KeyCode::Char('a'))),
            Some(Action::PolicyChar('a'))
        );
        assert_eq!(
            route_key(Focus::PolicyInput, key(KeyCode::Enter)),
            Some(Action::PolicyNewline)
        );
        assert_eq!(
            route_key(Focus::PolicyInput, key(KeyCode::Backspace)),
            Some(Action::PolicyBackspace)
        );
        // 'k'/'j' are literal text in the editor, not navigation.
        assert_eq!(
            route_key(Focus::PolicyInput, key(KeyCode::Char('k'))),
            Some(Action::PolicyChar('k'))
        );
        // A Ctrl-modified char is not inserted as literal text.
        assert_eq!(
            route_key(
                Focus::PolicyInput,
                key_mod(KeyCode::Char('l'), KeyModifiers::CONTROL)
            ),
            Some(Action::ClearPolicy),
            "Ctrl+L is the clear shortcut, not a typed 'l'"
        );
    }

    #[test]
    fn analysis_trigger_classification() {
        // The pure edit actions all dirty the input (bump generation); the
        // navigation actions do not. This mirrors what the debounce observes.
        assert!(matches!(
            route_key(Focus::PolicyInput, key(KeyCode::Char('x'))),
            Some(Action::PolicyChar('x'))
        ));
        assert_eq!(route_key(Focus::EnvSelector, key(KeyCode::Enter)), None);
        assert!(matches!(
            route_key(Focus::SchemaSelector, key(KeyCode::Down)),
            Some(Action::MoveSchemaCursor(Direction::Down))
        ));
    }
}
