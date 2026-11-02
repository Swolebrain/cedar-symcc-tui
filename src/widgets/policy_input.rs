//! Multi-line policy editor widget with an embedded clear button.

use ratatui::{
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, Paragraph},
    Frame,
};

use crate::state::{AnalysisMode, AppState, Focus};
use crate::widgets::focus_border_style;

/// Label drawn for the clear button in the footer row.
const CLEAR_LABEL: &str = " [ Clear ] ";
/// Label drawn for the "always allows" analysis-mode button (before Clear).
const ALWAYS_ALLOWS_LABEL: &str = " [ always allows ] ";
/// Label drawn for the "never allows" analysis-mode button (before Clear).
const NEVER_ALLOWS_LABEL: &str = " [ never allows ] ";

/// Render the bordered multi-line policy editor into `area` and return the
/// screen `Rect` occupied by the clear button.
///
/// Reads [`AppState::policy_input`] for the text and [`AppState::focus`] for
/// both the border highlight and the caret/button emphasis. Newlines in the
/// buffer are preserved (no wrapping), and the paragraph is vertically scrolled
/// so the *tail* of the text stays visible once it exceeds the inner height. A
/// caret is drawn at the end of the buffer (via
/// [`Frame::set_cursor_position`]) only while [`Focus::PolicyInput`] holds
/// focus. The clear button is rendered on a one-line footer inside this
/// widget's area, reversed when [`Focus::ClearButton`] holds focus.
///
/// The footer also carries two analysis-mode buttons BEFORE the clear button
/// (to its left): `[ always allows ]` and `[ never allows ]`. The button
/// matching the current [`AppState::mode`] is highlighted as active, and any of
/// the three buttons is emphasised while it holds focus.
///
/// The returned `Rect`s are how the buttons are exposed for hit-testing: the
/// event loop stores them and tests mouse-click coordinates against them, since
/// widgets here never handle input themselves. The tuple is
/// `(always_allows, never_allows, clear)`.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState) -> (Rect, Rect, Rect) {
    // Reserve the last line of the widget's area for the clear button.
    let [text_area, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

    let editor_focused = matches!(state.focus(), Focus::PolicyInput);
    let block = Block::bordered()
        .title("Policy")
        .border_style(focus_border_style(editor_focused));
    let inner = block.inner(text_area);

    // Line bookkeeping: split on '\n' so the count matches the rendered rows
    // (an empty buffer is one empty line).
    let lines: Vec<&str> = state.policy_input().split('\n').collect();
    let total_lines = lines.len();
    let inner_height = inner.height as usize;

    // Scroll so the tail stays visible: keep the bottom `inner_height` lines.
    let scroll_y = total_lines.saturating_sub(inner_height.max(1)) as u16;

    let paragraph = Paragraph::new(state.policy_input())
        .block(block)
        .scroll((scroll_y, 0));
    frame.render_widget(paragraph, text_area);

    // Caret at the very end of the buffer, translated by the scroll offset.
    if editor_focused && inner.height > 0 && inner.width > 0 {
        let last_row = (total_lines - 1) as u16;
        let caret_col = lines.last().map(|l| l.chars().count()).unwrap_or(0) as u16;
        let cursor_y = inner.y + last_row.saturating_sub(scroll_y);
        let cursor_x = (inner.x + caret_col).min(inner.x + inner.width - 1);
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
    }

    // Footer row: two analysis-mode buttons ("always allows", "never allows")
    // followed by the clear button, laid out left-to-right so the mode buttons
    // PRECEDE Clear. Each rect is clamped to what remains of the footer width.
    let mut cursor_x = footer_area.x;
    let footer_right = footer_area.x.saturating_add(footer_area.width);

    // Helper: carve a button rect of `label` width starting at `cursor_x`,
    // clamped to the remaining footer width, and advance the cursor.
    let mut place = |label: &str| -> Rect {
        let remaining = footer_right.saturating_sub(cursor_x);
        let width = (label.chars().count() as u16).min(remaining);
        let rect = Rect {
            x: cursor_x,
            y: footer_area.y,
            width,
            height: 1,
        };
        cursor_x = cursor_x.saturating_add(width);
        rect
    };

    let always_rect = place(ALWAYS_ALLOWS_LABEL);
    let never_rect = place(NEVER_ALLOWS_LABEL);
    let clear_rect = place(CLEAR_LABEL);

    // "always allows" button: active when the current mode is AlwaysAllows.
    frame.render_widget(
        Paragraph::new(Span::styled(
            ALWAYS_ALLOWS_LABEL,
            mode_button_style(
                matches!(state.mode(), AnalysisMode::AlwaysAllows),
                matches!(state.focus(), Focus::AlwaysAllowsButton),
            ),
        )),
        always_rect,
    );

    // "never allows" button: active when the current mode is NeverAllows.
    frame.render_widget(
        Paragraph::new(Span::styled(
            NEVER_ALLOWS_LABEL,
            mode_button_style(
                matches!(state.mode(), AnalysisMode::NeverAllows),
                matches!(state.focus(), Focus::NeverAllowsButton),
            ),
        )),
        never_rect,
    );

    // Clear button on the footer row.
    let button_focused = matches!(state.focus(), Focus::ClearButton);
    let button_style = if button_focused {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Yellow)
    };
    frame.render_widget(
        Paragraph::new(Span::styled(CLEAR_LABEL, button_style)),
        clear_rect,
    );

    (always_rect, never_rect, clear_rect)
}

/// Style for an analysis-mode button.
///
/// The button for the currently-selected mode (`active`) gets a distinct green
/// highlight so the selected mode is always visible; a `focused` button is
/// additionally underlined (and, when not active, drawn in the same yellow as
/// the Clear button) so keyboard focus is visible even on the active button.
fn mode_button_style(active: bool, focused: bool) -> Style {
    let mut style = if active {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else if focused {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    if focused {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}
