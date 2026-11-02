//! Schema-file selector widget.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState},
    Frame,
};

use crate::state::AppState;
use crate::widgets::focus_border_style;

/// Marker shown on the row whose schema is actually loaded.
const LOADED_MARKER: &str = "● ";
/// Blank prefix (same width as [`LOADED_MARKER`]) for non-loaded rows, so all
/// labels align.
const BLANK_MARKER: &str = "  ";

/// Render the bordered schema-file list into `area`.
///
/// Reads [`AppState::schemas`] for the rows (each shown via
/// [`SchemaFile::display`](crate::state::SchemaFile::display)),
/// [`AppState::schema_cursor`] for the highlighted row,
/// [`AppState::schema_load`] to mark the row that is *actually loaded*, and
/// [`AppState::focus`] for the border highlight.
///
/// The caller passes a `Rect` of exactly 10 content lines (12 including the
/// border). Scrolling is delegated to ratatui's [`List`] + [`ListState`]:
/// rendering via [`Frame::render_stateful_widget`] with the cursor row selected
/// makes the list auto-scroll its viewport so the selected entry stays visible
/// when there are more schemas than fit. The cursor row is drawn reversed; the
/// loaded row is prefixed with a green `●` marker so it is distinguishable from
/// a merely-hovered cursor row.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let focused = matches!(state.focus(), crate::state::Focus::SchemaSelector);
    let loaded_path = state.schema_load().loaded_path();

    let items: Vec<ListItem> = state
        .schemas()
        .iter()
        .map(|file| {
            let is_loaded = loaded_path == Some(file.path.as_path());
            if is_loaded {
                Line::from(vec![
                    Span::styled(
                        LOADED_MARKER,
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        file.display.clone(),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::raw(BLANK_MARKER),
                    Span::raw(file.display.clone()),
                ])
            }
        })
        .map(ListItem::new)
        .collect();

    let block = Block::bordered()
        .title("Schemas")
        .border_style(focus_border_style(focused));

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::BOLD),
    );

    // ListState drives the viewport: selecting the cursor row makes the List
    // scroll so that row is visible within the (10-line) inner area.
    let mut list_state = ListState::default();
    if !state.schemas().is_empty() {
        list_state.select(Some(state.schema_cursor().min(state.schemas().len() - 1)));
    }

    frame.render_stateful_widget(list, area, &mut list_state);
}
