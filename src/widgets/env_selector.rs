//! Request-environment selector widget, rendered side-by-side with the schema
//! selector.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::state::{AppState, Focus, SchemaLoad};
use crate::widgets::{focus_border_style, truncate_label};

/// Render the bordered request-environment list into `area`.
///
/// Reads [`AppState::schema_load`] and [`AppState::env_cursor`] plus
/// [`AppState::focus`] for the border highlight. Behaviour by load state:
///
/// * [`SchemaLoad::NoneSelected`] → a `no schema selected` placeholder.
/// * [`SchemaLoad::Loading`] → a `loading…` placeholder while the load is in
///   flight.
/// * [`SchemaLoad::LoadFailed`] → the (wrapped) load error message.
/// * [`SchemaLoad::Loaded`] → a scrolling [`List`] of the envs' `label`s, with
///   the [`env_cursor`](AppState::env_cursor) row highlighted.
///
/// This widget is drawn in a *narrow* Rect beside the schema selector, so each
/// label is truncated with an ellipsis to the inner width rather than wrapped.
/// Scrolling uses ratatui's [`ListState`] (via
/// [`Frame::render_stateful_widget`]) so the selected env stays visible when
/// the list overflows the pane.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let focused = matches!(state.focus(), Focus::EnvSelector);
    let block = Block::bordered()
        .title("Request envs")
        .border_style(focus_border_style(focused));

    match state.schema_load() {
        SchemaLoad::NoneSelected => {
            let p = Paragraph::new("no schema selected")
                .block(block)
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true });
            frame.render_widget(p, area);
        }
        SchemaLoad::Loading { .. } => {
            let p = Paragraph::new("loading…")
                .block(block)
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true });
            frame.render_widget(p, area);
        }
        SchemaLoad::LoadFailed { error, .. } => {
            let p = Paragraph::new(error.as_str())
                .block(block)
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: true });
            frame.render_widget(p, area);
        }
        SchemaLoad::Loaded { envs, .. } => {
            // Inner width available for a label = area width minus the two
            // border columns. Truncate to it so long labels never wrap.
            let inner_width = area.width.saturating_sub(2) as usize;
            let items: Vec<ListItem> = envs
                .iter()
                .map(|e| ListItem::new(Line::from(truncate_label(&e.label, inner_width))))
                .collect();

            let list = List::new(items).block(block).highlight_style(
                Style::default()
                    .add_modifier(Modifier::REVERSED)
                    .add_modifier(Modifier::BOLD),
            );

            let mut list_state = ListState::default();
            if !envs.is_empty() {
                list_state.select(Some(state.env_cursor().min(envs.len() - 1)));
            }
            frame.render_stateful_widget(list, area, &mut list_state);
        }
    }
}
