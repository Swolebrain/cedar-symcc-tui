//! CedarSymCCTui binary: terminal lifecycle + the tokio async event loop.
//!
//! This module owns the terminal (raw mode, alternate screen, bracketed paste,
//! mouse capture, and a panic hook that restores all of it) and the event loop
//! that routes input through [`app`] into the `state` domain actions. It holds
//! **no** application state of its own: there is no mutex and no generation
//! bookkeeping here. All of that — including the debounced, generation-guarded
//! async SMTLib analysis — lives inside `state`, behind a private
//! process-global.
//!
//! # State access
//!
//! `main` never locks anything. Each frame it takes one [`state::snapshot`] (a
//! cheap `Clone`), renders every widget from it, and mutates the world only
//! through `state::` domain actions (each returning `Result`). Analysis is
//! scheduled *inside* those actions, so the event loop no longer runs a debounce
//! timer.
//!
//! # Redraw
//!
//! The loop polls input with a short [`TICK`] timeout and redraws every tick, so
//! an analysis result written by a background task appears within one tick
//! without needing an extra keypress. That periodic tick is the redraw
//! mechanism; no separate notification channel is needed.

use std::io::{self, IsTerminal, Stdout};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Position;
use ratatui::Terminal;

use crate::state::{AnalysisMode, Focus};

mod analysis;
mod app;
mod state;
mod widgets;

/// The concrete terminal type: a ratatui `Terminal` over the crossterm backend
/// writing to stdout.
type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Input poll timeout; doubles as the redraw tick so `Running` and then the
/// analysis result appear without requiring an extra keypress.
const TICK: Duration = Duration::from_millis(50);

#[tokio::main]
async fn main() -> io::Result<()> {
    // Bail out early on a non-interactive terminal. A piped or redirected
    // invocation (e.g. `cedar-symcc-tui > out.txt`, `< /dev/null`, or launched
    // from a context with no controlling TTY) has no terminal to drive; without
    // this check `enable_raw_mode` fails deep in setup with the opaque
    // `Error: Os { code: 6, kind: Uncategorized, message: "No such device or
    // address" }`. Detect it up front and exit non-zero with a clear message.
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        eprintln!("cedar-symcc-tui must be run in an interactive terminal");
        std::process::exit(1);
    }

    // Install the panic hook BEFORE entering raw mode so any panic (including
    // one during setup) restores the terminal before printing.
    install_panic_hook();

    // Discover schemas in the CWD the binary was launched from and install
    // them (auto-loading the sole schema). Discovery / auto-load failures are
    // non-fatal: the UI still starts, showing an empty list or the load error.
    if let Ok(cwd) = std::env::current_dir() {
        let _ = state::init(&cwd);
    }

    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal).await;
    // Restore on the normal-exit path too (the panic hook covers crashes).
    restore_terminal();
    result
}

/// Enter raw mode + alternate screen and enable bracketed paste (multi-line
/// pastes arrive as one [`Event::Paste`]) and mouse capture (so the clear
/// button is clickable). Returns the ready-to-draw terminal.
fn setup_terminal() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // From here on raw mode is enabled: if ANY subsequent step fails, restore
    // the terminal before propagating the error, so an error return can never
    // leave the user's terminal stuck in raw mode. (The panic hook only covers
    // panics, not returned `Err`s.)
    if let Err(err) = execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    ) {
        restore_terminal();
        return Err(err);
    }
    let backend = CrosstermBackend::new(stdout);
    match Terminal::new(backend) {
        Ok(terminal) => Ok(terminal),
        Err(err) => {
            restore_terminal();
            Err(err)
        }
    }
}

/// Reverse [`setup_terminal`]: disable mouse capture + bracketed paste, leave
/// the alternate screen, and disable raw mode. Best-effort — errors are ignored
/// because this runs on the exit/panic path where there is nothing useful to do
/// with them.
fn restore_terminal() {
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
}

/// Install a panic hook that restores the terminal before delegating to the
/// previous hook, so a crash can never leave the user's terminal in raw mode
/// with the alternate screen / mouse capture / bracketed paste still on.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

/// The async event loop: draw a frame from a fresh snapshot, poll input with a
/// short timeout, and route it through [`app`] into the `state` domain actions.
/// Returns when the user quits.
async fn run(terminal: &mut Tui) -> io::Result<()> {
    // Hit-test rectangles from the last frame (the footer buttons, for clicks).
    let mut hits = app::FrameHits::default();

    loop {
        // One immutable snapshot per frame; the lock is not held during
        // rendering. Reused for focus-based key routing below.
        let snapshot = state::snapshot();
        terminal.draw(|frame| {
            hits = app::draw(frame, &snapshot);
        })?;

        // Poll with a short timeout so the loop keeps ticking (and redrawing)
        // while a background analysis runs; its result then appears on the next
        // tick without needing another keypress.
        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Some(action) = app::route_key(snapshot.focus(), key) {
                        if matches!(action, app::Action::Quit) {
                            break;
                        }
                        // Errors are benign: the UI already reflects state (e.g.
                        // a backspace rejected on an empty buffer). Ignore them.
                        let _ = app::apply_action(action);
                    }
                }
                Event::Paste(text) => {
                    // Bracketed paste: append the whole multi-line chunk in one
                    // shot (newlines preserved) when the editor is focused.
                    if matches!(snapshot.focus(), Focus::PolicyInput) {
                        let _ = state::handle_paste(&text);
                    }
                }
                Event::Mouse(mouse) => {
                    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                        let pos = Position::new(mouse.column, mouse.row);
                        // Benign Results ignored: the click either lands on a
                        // button (applied) or nowhere (no-op).
                        if hits.always_allows_button.contains(pos) {
                            let _ = state::set_analysis_mode(AnalysisMode::AlwaysAllows);
                        } else if hits.never_allows_button.contains(pos) {
                            let _ = state::set_analysis_mode(AnalysisMode::NeverAllows);
                        } else if hits.clear_button.contains(pos) {
                            let _ = state::clear_policy();
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use cedar_policy::PolicySet;

    /// Phase 0 smoke test (preserved): proves the third-party crate graph links
    /// by touching one symbol from cedar-policy and one from cedar-policy-symcc.
    #[test]
    fn dependency_graph_links() {
        let policies = PolicySet::new();
        assert_eq!(policies.policies().count(), 0);
        assert!(!std::any::type_name::<cedar_policy_symcc::err::Error>().is_empty());
    }
}
