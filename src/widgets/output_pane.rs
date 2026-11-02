//! Scrollable analysis-output pane widget.

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Paragraph, Wrap},
    Frame,
};

use crate::{
    state::{AnalysisOutput, AppState, Focus},
    widgets::focus_border_style,
};

/// Render the bordered, scrollable output pane into `area`.
///
/// Reads [`AppState::output`] to decide *what* to show, [`AppState::output_scroll`]
/// for the vertical scroll offset, and [`AppState::focus`] for the border
/// highlight. The block title names the active mode, e.g. `Output — parse
/// errors`. Rendering per [`AnalysisOutput`]:
///
/// * [`Empty`](AnalysisOutput::Empty) → a short usage hint.
/// * [`Running`](AnalysisOutput::Running) → an in-progress line.
/// * [`ParseErrors`](AnalysisOutput::ParseErrors) /
///   [`ValidationErrors`](AnalysisOutput::ValidationErrors) → a wrapped,
///   one-entry-per-error list.
/// * [`Encoded`](AnalysisOutput::Encoded) → the SMTLib text with **no wrapping**
///   so s-expression line structure is preserved, scrolled by `output_scroll`.
/// * [`EncodeError`](AnalysisOutput::EncodeError) → the wrapped error message.
///
/// A `line X/Y` indicator is drawn in the bottom border showing the scroll
/// position relative to the body's total line count.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let focused = matches!(state.focus(), Focus::OutputPane);

    // Per-variant: title suffix, body text, whether to wrap, and body colour.
    let (mode, body, wrap, color): (&str, String, bool, Color) = match state.output() {
        AnalysisOutput::Empty => (
            "empty",
            "No output yet — pick a schema/env and type a policy.".to_owned(),
            true,
            Color::DarkGray,
        ),
        AnalysisOutput::Running => ("running", "Analyzing…".to_owned(), true, Color::Yellow),
        AnalysisOutput::ParseErrors(errs) => {
            ("parse errors", number_errors(errs), true, Color::Red)
        }
        AnalysisOutput::ValidationErrors(errs) => {
            ("validation errors", number_errors(errs), true, Color::Red)
        }
        AnalysisOutput::Encoded { smtlib } => ("SMTLib", smtlib.to_string(), false, Color::Reset),
        AnalysisOutput::EncodeError(err) => ("encode error", err.to_string(), true, Color::Red),
    };

    let total_lines = body.split('\n').count().max(1);
    // Clamp the scroll offset to the last line so the indicator is honest.
    let scroll = state.output_scroll().min((total_lines - 1) as u16);
    let current = (scroll as usize + 1).min(total_lines);

    let block = Block::bordered()
        .title(format!("Output — {mode}"))
        .title_bottom(format!("line {current}/{total_lines}"))
        .border_style(focus_border_style(focused));

    let mut paragraph = Paragraph::new(body)
        .block(block)
        .style(Style::default().fg(color))
        .scroll((scroll, 0));
    if wrap {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }
    frame.render_widget(paragraph, area);
}

/// Render a list of error strings as one numbered entry per error, e.g.
/// `1. <msg>`. Kept separate so parse and validation errors format identically.
fn number_errors(errs: &[String]) -> String {
    errs.iter()
        .enumerate()
        .map(|(i, e)| format!("{}. {}", i + 1, e))
        .collect::<Vec<_>>()
        .join("\n")
}
