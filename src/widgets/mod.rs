//! Pure render widgets for the CedarSymCCTui UI.
//!
//! Every function in this module and its submodules is a **pure render
//! function**: it borrows an immutable [`AppState`](crate::state::AppState)
//! snapshot and draws into a [`ratatui::Frame`], never touching the state lock
//! (no `.lock()`, no shared handle) and never handling input events. The event
//! loop (a later phase) is the sole owner of mutation and input; it hands each
//! widget the exact [`Rect`](ratatui::layout::Rect) to draw into and reads back
//! only the hit-test rectangles the widgets return (see
//! [`policy_input::render`]).
//!
//! Focus indication is uniform across widgets: the block border is drawn with
//! [`focus_border_style`] so the focused widget is visually highlighted, driven
//! entirely by [`AppState::focus`](crate::state::AppState::focus).

// The render functions are consumed by the event loop (a later phase) that is
// not wired yet; mirror the `state`/`analysis` modules and allow them to exist
// without a non-test caller until the UI draw loop lands.
#![allow(dead_code)]

pub mod env_selector;
pub mod output_pane;
pub mod policy_input;
pub mod schema_selector;

use ratatui::style::{Color, Modifier, Style};

/// Border style for a widget's block, brightened and bold when `focused`.
///
/// This is the single source of the focus indication shared by all widgets:
/// the focused widget gets a bold cyan border, everything else a dim gray one.
pub(crate) fn focus_border_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Truncate `s` to at most `max` display columns, appending `…` when it was cut.
///
/// Used by the side-by-side env selector (and anywhere a single-line label must
/// fit a narrow rect) so long labels are shortened rather than wrapped. Width is
/// counted in `char`s, which is adequate for the ASCII-ish Cedar type names
/// shown here.
pub(crate) fn truncate_label(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_owned();
    }
    if max == 1 {
        return "…".to_owned();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}
