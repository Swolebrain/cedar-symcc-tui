//! Central application state and the *only* place in the app that touches the
//! state lock.
//!
//! # Encapsulation (hard rules)
//!
//! * The whole application state lives behind a single, **private**
//!   process-global [`std::sync::Mutex`] wrapped in a [`std::sync::LazyLock`]
//!   ([`STATE`]). There is exactly one mutex and it never escapes this module —
//!   no function takes or returns a state handle.
//! * **No module outside `state` may call `.lock()`.** Callers mutate the world
//!   only through the high-level *domain actions* in this module (each of which
//!   returns [`Result`]), and *read* the world through one [`snapshot`] per
//!   frame — a cheap `Clone` of [`AppState`] taken under the lock and rendered
//!   without holding it.
//! * **The lock is NEVER held across an `.await`.** Async work (the debounced
//!   SMTLib encode) snapshots its inputs *off* the lock, runs the analysis, and
//!   writes the result back under the lock, guarded by the generation epoch.
//! * A poisoned lock (a panic while some other closure held it) is recovered
//!   with `unwrap_or_else(|e| e.into_inner())` in the private [`lock`] accessor
//!   so a single panic can never deadlock the UI thread.
//!
//! State is modeled so that invalid combinations are unrepresentable: there are
//! no bare `bool` flags and no `Option` where an enum carries the meaning more
//! precisely (for example the request-env list only exists inside
//! [`SchemaLoad::Loaded`]).
//!
//! # Cheap snapshots
//!
//! [`snapshot`] clones [`AppState`] every frame, so all heavy data is behind an
//! [`Arc`] and the clone is a handful of refcount bumps: the schema file list
//! ([`Arc<[SchemaFile]>`]), the loaded schema ([`Arc<Schema>`]) and its request
//! envs ([`Arc<[RequestEnvEntry]>`]), and the analysis output text / error
//! lists ([`Arc<str>`] / [`Arc<[String]>`]). Only `policy_input` is a bare
//! `String`: it is user-typed and small, so a real copy per frame is cheap and
//! not worth an `Arc`.
//!
//! # Debounce + generation epoch
//!
//! Every action that changes an analysis input (policy text, env selection,
//! schema load, mode) bumps a monotonic `generation` and schedules an analysis
//! via the private [`schedule_analysis`]: it `tokio::spawn`s a task that sleeps
//! [`DEBOUNCE`], then — only if `generation` is unchanged — snapshots the inputs
//! off the lock, runs the encode, and writes the result back only if the
//! generation *still* matches. A newer edit bumps `generation` and schedules its
//! own task, so every superseded task drops out and exactly one analysis (the
//! latest) is ever applied.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, MutexGuard},
    time::Duration,
};

use cedar_policy::{RequestEnv, Schema};

/// The on-disk source format of a discovered schema file.
///
/// Determined purely from the file name: `*.cedarschema` is the human-readable
/// Cedar schema syntax ([`Cedar`](SchemaFormat::Cedar)); `*.cedar.json` is the
/// JSON schema syntax ([`Json`](SchemaFormat::Json)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaFormat {
    /// Human-readable `.cedarschema` syntax (parsed via `Schema` `FromStr`).
    Cedar,
    /// JSON `.cedar.json` syntax (parsed via `Schema::from_json_str`).
    Json,
}

impl SchemaFormat {
    /// Infer the format from a path's file name, if it is a recognized schema
    /// extension. Returns `None` for any other file.
    ///
    /// `.cedar.json` is checked before `.cedarschema` so the double extension
    /// is never misclassified.
    pub fn from_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        if name.ends_with(".cedar.json") {
            Some(SchemaFormat::Json)
        } else if name.ends_with(".cedarschema") {
            Some(SchemaFormat::Cedar)
        } else {
            None
        }
    }
}

/// A schema file discovered on disk, before any attempt to load/parse it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaFile {
    /// Absolute or relative path to the file.
    pub path: PathBuf,
    /// Label shown in the selector. For files found by
    /// [`discover_schema_files`](crate::analysis::discover_schema_files) this is
    /// the path RELATIVE to the discovery root (e.g.
    /// `examples/foo/policies.cedarschema`), so nested files with identical
    /// names stay distinguishable. [`SchemaFile::from_path`] instead uses the
    /// bare file name.
    pub display: String,
    /// Which parser to use, inferred from the extension.
    pub format: SchemaFormat,
}

impl SchemaFile {
    /// Build a [`SchemaFile`] from a path, inferring [`display`](Self::display)
    /// from the file name and [`format`](Self::format) from the extension.
    /// Returns `None` if the path is not a recognized schema file.
    ///
    /// Production discovery builds [`SchemaFile`]s inline (with a root-relative
    /// `display`); this bare-name constructor is retained as public API and is
    /// used by the unit tests.
    #[allow(dead_code)]
    pub fn from_path(path: PathBuf) -> Option<Self> {
        let format = SchemaFormat::from_path(&path)?;
        let display = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_owned())
            .unwrap_or_else(|| path.display().to_string());
        Some(SchemaFile {
            path,
            display,
            format,
        })
    }
}

/// One entry in the request-environment selector: a `RequestEnv` plus a
/// pre-rendered human-readable label.
#[derive(Debug, Clone)]
pub struct RequestEnvEntry {
    /// The Cedar request environment (principal type, action, resource type).
    pub env: RequestEnv,
    /// `"principal, action, resource"` rendering, precomputed for display.
    pub label: String,
}

impl RequestEnvEntry {
    /// Build an entry, rendering `label` as `"<principal>, <action>,
    /// <resource>"` from the environment's `principal()`, `action()` and
    /// `resource()`.
    pub fn new(env: RequestEnv) -> Self {
        let label = format!("{}, {}, {}", env.principal(), env.action(), env.resource());
        RequestEnvEntry { env, label }
    }
}

/// Result of loading the currently selected schema.
///
/// The request-env list only exists in the `Loaded` case, so it is impossible
/// to hold envs for a schema that failed to load. The heavy `Loaded` payload is
/// behind [`Arc`]s so cloning a [`snapshot`] is cheap.
#[derive(Debug, Clone)]
pub enum SchemaLoad {
    /// No schema has been selected/loaded yet.
    NoneSelected,
    /// The schema parsed successfully; `envs` are its enumerated request envs.
    Loaded {
        /// The file this schema was loaded from.
        file: SchemaFile,
        /// The parsed schema (shared so snapshots clone only a refcount).
        schema: Arc<Schema>,
        /// Enumerated request environments from `schema.request_envs()`.
        envs: Arc<[RequestEnvEntry]>,
    },
    /// The schema failed to parse; `error` is a rendered message.
    LoadFailed {
        /// The file we tried to load.
        file: SchemaFile,
        /// Rendered parse/schema error.
        error: String,
    },
}

impl SchemaLoad {
    /// Path of the file currently reflected by this load state, if any.
    /// Used to detect a re-selection of the same file.
    pub fn loaded_path(&self) -> Option<&Path> {
        match self {
            SchemaLoad::NoneSelected => None,
            SchemaLoad::Loaded { file, .. } | SchemaLoad::LoadFailed { file, .. } => {
                Some(file.path.as_path())
            }
        }
    }

    /// Number of request envs available (0 unless successfully `Loaded`).
    pub fn env_count(&self) -> usize {
        match self {
            SchemaLoad::Loaded { envs, .. } => envs.len(),
            _ => 0,
        }
    }
}

/// What the output pane should display, in priority order. Text payloads are
/// behind [`Arc`] so a per-frame [`snapshot`] clone is cheap even for a large
/// SMTLib body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnalysisOutput {
    /// Nothing to show.
    Empty,
    /// An analysis is in flight; a result may replace this.
    Running,
    /// Policy failed to parse; one rendered message per error.
    ParseErrors(Arc<[String]>),
    /// Policy parsed but failed validation; one message per error.
    ValidationErrors(Arc<[String]>),
    /// Successful SMTLib encoding.
    Encoded {
        /// The SMTLib text read out of the writer solver.
        smtlib: Arc<str>,
    },
    /// Encoding failed (a symcc error other than the expected `SolverUnknown`).
    EncodeError(Arc<str>),
}

/// Which symbolic-analysis question [`analyze`](crate::analysis::analyze) asks.
///
/// [`NeverAllows`](AnalysisMode::NeverAllows) is the DEFAULT and preserves the
/// original behaviour (the per-policy `cedar-policy-symcc` `never_matches`
/// encoding); [`AlwaysAllows`](AnalysisMode::AlwaysAllows) selects the
/// whole-policy-set `always_allows` encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisMode {
    /// Per-policy `never_matches` analysis (the default).
    NeverAllows,
    /// Whole-policy-set `always_allows` analysis.
    AlwaysAllows,
}

/// Which widget currently owns keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The schema file list.
    SchemaSelector,
    /// The request-env list.
    EnvSelector,
    /// The multi-line policy editor.
    PolicyInput,
    /// The "always allows" analysis-mode button.
    AlwaysAllowsButton,
    /// The "never allows" analysis-mode button.
    NeverAllowsButton,
    /// The "clear" button.
    ClearButton,
    /// The scrollable output pane.
    OutputPane,
}

impl Focus {
    /// The next focus target for `Tab`, cycling back to the start after the
    /// last variant.
    pub fn next(self) -> Focus {
        match self {
            Focus::SchemaSelector => Focus::EnvSelector,
            Focus::EnvSelector => Focus::PolicyInput,
            Focus::PolicyInput => Focus::AlwaysAllowsButton,
            Focus::AlwaysAllowsButton => Focus::NeverAllowsButton,
            Focus::NeverAllowsButton => Focus::ClearButton,
            Focus::ClearButton => Focus::OutputPane,
            Focus::OutputPane => Focus::SchemaSelector,
        }
    }

    /// The previous focus target for `Shift+Tab`, cycling to the last variant
    /// from the first.
    pub fn prev(self) -> Focus {
        match self {
            Focus::SchemaSelector => Focus::OutputPane,
            Focus::EnvSelector => Focus::SchemaSelector,
            Focus::PolicyInput => Focus::EnvSelector,
            Focus::AlwaysAllowsButton => Focus::PolicyInput,
            Focus::NeverAllowsButton => Focus::AlwaysAllowsButton,
            Focus::ClearButton => Focus::NeverAllowsButton,
            Focus::OutputPane => Focus::ClearButton,
        }
    }
}

/// A cursor-movement direction shared by the list selectors and the output
/// scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Toward the start of the list / top of the pane.
    Up,
    /// Toward the end of the list / bottom of the pane.
    Down,
}

/// A navigation intent handed to [`navigate`].
///
/// `Up`/`Down` are single-line moves that act on the *focused* pane (schema
/// cursor, env cursor, or output scroll); `PageUp`/`PageDown` always scroll the
/// output pane regardless of focus. None of these change an analysis input, so
/// `navigate` never bumps the generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    /// Move one line up in the focused pane.
    Up,
    /// Move one line down in the focused pane.
    Down,
    /// Scroll the output pane up (any focus).
    PageUp,
    /// Scroll the output pane down (any focus).
    PageDown,
}

/// A single policy-editor edit handed to [`edit_policy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// Insert a typed character at the end of the buffer.
    Char(char),
    /// Insert a newline at the end of the buffer.
    Newline,
    /// Delete the last character of the buffer.
    Backspace,
}

/// Which way [`cycle_focus`] advances keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusMove {
    /// Advance to the next widget (`Tab`).
    Next,
    /// Go back to the previous widget (`Shift+Tab`).
    Prev,
}

/// The full application state. Fields are private; render code reads a
/// consistent [`snapshot`] through the getters below, and all *mutation* goes
/// through the domain actions in this module.
///
/// Cheap to `Clone`: every heavy field is behind an [`Arc`] (see the module
/// docs), so a per-frame snapshot is a few refcount bumps plus a small
/// `policy_input` copy.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Discovered schema files.
    schemas: Arc<[SchemaFile]>,
    /// Highlighted row in the schema list (not necessarily loaded).
    schema_cursor: usize,
    /// Load state of the currently selected schema.
    schema_load: SchemaLoad,
    /// Highlighted row in the request-env list.
    env_cursor: usize,
    /// Multi-line Cedar policy text. Kept as a bare `String` (not `Arc`): it is
    /// user-typed and small, so a per-frame clone is cheap.
    policy_input: String,
    /// What the output pane should render.
    output: AnalysisOutput,
    /// Vertical scroll offset of the output pane.
    output_scroll: u16,
    /// Rendered line count of the currently-applied output, used to clamp
    /// [`output_scroll`](Self::output_scroll) so scrolling past the end cannot
    /// strand the view far below the text. Derived by the analysis scheduler
    /// from the applied [`AnalysisOutput`]; starts at `u16::MAX` (unbounded)
    /// until the first analysis result is applied. The output widget keeps its
    /// own independent display clamp.
    output_line_count: u16,
    /// Which widget has focus.
    focus: Focus,
    /// Which analysis the debounced encode runs. Starts as
    /// [`AnalysisMode::NeverAllows`] so the original behaviour is preserved.
    mode: AnalysisMode,
    /// Monotonic epoch bumped on every input change; async results carry the
    /// epoch they were spawned with and are dropped if it has moved on.
    generation: u64,
}

impl AppState {
    // --- Read-only accessors ------------------------------------------------
    // Render code reads a consistent snapshot through these; the fields stay
    // private so all mutation is funnelled through the domain actions.

    /// Discovered schema files.
    pub fn schemas(&self) -> &[SchemaFile] {
        &self.schemas
    }

    /// Highlighted row in the schema list (not necessarily loaded).
    pub fn schema_cursor(&self) -> usize {
        self.schema_cursor
    }

    /// Load state of the currently selected schema.
    pub fn schema_load(&self) -> &SchemaLoad {
        &self.schema_load
    }

    /// Highlighted row in the request-env list.
    pub fn env_cursor(&self) -> usize {
        self.env_cursor
    }

    /// Multi-line Cedar policy text.
    pub fn policy_input(&self) -> &str {
        &self.policy_input
    }

    /// What the output pane should render.
    pub fn output(&self) -> &AnalysisOutput {
        &self.output
    }

    /// Vertical scroll offset of the output pane.
    pub fn output_scroll(&self) -> u16 {
        self.output_scroll
    }

    /// Which widget has focus.
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// Which analysis the debounced encode runs.
    pub fn mode(&self) -> AnalysisMode {
        self.mode
    }
}

/// An error returned by a domain action.
///
/// Every variant is returned by at least one action; the event loop treats them
/// as benign (the UI already reflects the resulting state) and ignores them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    /// No schemas are available to load (the discovered list is empty).
    NoSchemas,
    /// Reading/parsing the selected schema failed; the rendered error text.
    SchemaLoadFailed(String),
    /// Discovering schema files under the root failed; the rendered I/O error.
    DiscoveryFailed(String),
    /// A delete edit (backspace) was requested on an empty policy buffer.
    NothingToDelete,
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateError::NoSchemas => write!(f, "no schemas are available to load"),
            StateError::SchemaLoadFailed(err) => write!(f, "schema load failed: {err}"),
            StateError::DiscoveryFailed(err) => write!(f, "schema discovery failed: {err}"),
            StateError::NothingToDelete => write!(f, "nothing to delete"),
        }
    }
}

impl std::error::Error for StateError {}

/// Quiescence window: after the last input-generation change the scheduler waits
/// this long before running an analysis, so encoding does not run on every
/// keystroke.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// The single, private, process-global application state.
///
/// It never escapes this module: there is no accessor that hands out the
/// `Mutex` or an `AppState` reference. All reads go through [`snapshot`] and all
/// writes through the domain actions.
static STATE: LazyLock<Mutex<AppState>> = LazyLock::new(|| Mutex::new(initial_state()));

/// The initial application state. Shared by [`STATE`]'s initializer and (in
/// tests) the [`test_guard`] reset, so both start from exactly the same values.
fn initial_state() -> AppState {
    AppState {
        schemas: Arc::from(Vec::<SchemaFile>::new()),
        schema_cursor: 0,
        schema_load: SchemaLoad::NoneSelected,
        env_cursor: 0,
        policy_input: String::new(),
        output: AnalysisOutput::Empty,
        output_scroll: 0,
        output_line_count: u16::MAX,
        focus: Focus::SchemaSelector,
        mode: AnalysisMode::NeverAllows,
        generation: 0,
    }
}

/// Acquire the global lock, recovering from poisoning.
///
/// This is the ONLY place in the crate that locks the state mutex. A poisoned
/// lock (a panic while another closure held it) is recovered with
/// `unwrap_or_else(|e| e.into_inner())` so a single panicking scope can never
/// deadlock the UI. The returned guard MUST NOT be held across an `.await`.
fn lock() -> MutexGuard<'static, AppState> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A cheap, detached clone of the current state, taken under the lock.
///
/// The event loop calls this exactly once per frame and renders every widget
/// from the returned `&AppState`, so the lock is never held during rendering and
/// the widgets can never observe a torn state. Heavy fields are behind [`Arc`],
/// so the clone is a few refcount bumps plus a small `policy_input` copy.
pub fn snapshot() -> AppState {
    lock().clone()
}

// ---------------------------------------------------------------------------
// Domain actions. Each acquires the lock itself, validates, mutates, and (when
// it changes an analysis input) bumps the generation and schedules a debounced
// analysis. The lock is never held across the (async) analysis; scheduling only
// hands a generation to a spawned task.
// ---------------------------------------------------------------------------

/// Discover schema files under `root` and install them, auto-loading the sole
/// schema when exactly one is found.
///
/// Moves the startup logic out of `main`. On success the schema list is
/// installed; if it contains exactly one schema that schema is loaded (as by
/// [`load_schema_under_cursor`]).
///
/// # Errors
///
/// * [`StateError::DiscoveryFailed`] if reading `root` fails.
/// * [`StateError::SchemaLoadFailed`] if the sole schema is auto-loaded and its
///   parse fails (the failure is still recorded in state so the pane shows it).
pub fn init(root: &Path) -> Result<(), StateError> {
    let files = crate::analysis::discover_schema_files(root)
        .map_err(|e| StateError::DiscoveryFailed(e.to_string()))?;
    let single = files.len() == 1;
    {
        let mut s = lock();
        // Clamp the cursor into the new list (or 0 when empty), then install.
        if files.is_empty() {
            s.schema_cursor = 0;
        } else if s.schema_cursor >= files.len() {
            s.schema_cursor = files.len() - 1;
        }
        s.schemas = Arc::from(files);
    }
    if single {
        // Auto-select and load the sole schema so its request envs populate.
        load_schema_under_cursor()?;
    }
    Ok(())
}

/// Load the schema currently under the schema cursor.
///
/// Replaces the old select-then-async-load dance: it reads + parses the file
/// synchronously (schema files are small and local), records the result in
/// state so the pane shows it, and — on success — schedules an analysis for the
/// current policy against the new schema. Re-selecting the already-active
/// schema is a no-op.
///
/// # Errors
///
/// * [`StateError::NoSchemas`] if there is no schema at the cursor.
/// * [`StateError::SchemaLoadFailed`] if the read/parse fails. The failure is
///   *also* recorded as [`SchemaLoad::LoadFailed`] so the request-env pane
///   renders the error, and `Err` is returned as well.
pub fn load_schema_under_cursor() -> Result<(), StateError> {
    // (1) Pick the file to load under the lock. A re-selection of the already
    //     active schema (same path) is a pure no-op.
    let file = {
        let s = lock();
        if s.schemas.is_empty() {
            return Err(StateError::NoSchemas);
        }
        let Some(file) = s.schemas.get(s.schema_cursor).cloned() else {
            return Err(StateError::NoSchemas);
        };
        if s.schema_load.loaded_path() == Some(file.path.as_path()) {
            return Ok(()); // already the active schema; nothing to reload
        }
        file
    };

    // (2) Read + parse the schema OFF the lock: blocking file I/O is never held
    //     under the mutex.
    let load = crate::analysis::load_schema(&file);
    // Capture the failure text (if any) before `load` is moved into state.
    let error = match &load {
        SchemaLoad::LoadFailed { error, .. } => Some(error.clone()),
        _ => None,
    };

    // (3) Install the result and bump the generation. The single bump supersedes
    //     any analysis spawned against the previous schema, so its write-back is
    //     dropped by the generation guard. The load happened synchronously
    //     above with no analysis scheduled mid-load, so one bump suffices.
    let generation = {
        let mut s = lock();
        s.env_cursor = 0; // highlight the first env of the new list
        s.output = AnalysisOutput::Empty; // wipe stale output on the switch
        s.schema_load = load;
        s.generation += 1;
        s.generation
    };

    if let Some(error) = error {
        // The failed load is recorded in state (the pane shows it) AND reported.
        return Err(StateError::SchemaLoadFailed(error));
    }
    // (4) Success: schedule an analysis for the current policy against the new
    //     schema now that its envs are available.
    schedule_analysis(generation);
    Ok(())
}

/// Move a cursor or scroll the output pane.
///
/// `Nav::Up`/`Nav::Down` move one line in the *focused* pane (schema cursor, env
/// cursor, or output scroll), clamped to bounds; `Nav::PageUp`/`Nav::PageDown`
/// scroll the output pane regardless of focus. Navigation changes no analysis
/// input, so it never bumps the generation and never schedules analysis. A move
/// against a boundary (or a focus with nothing to move) is a silent no-op that
/// still returns `Ok`.
///
/// # Errors
///
/// Never returns `Err`; the `Result` is uniform with the other actions.
pub fn navigate(nav: Nav) -> Result<(), StateError> {
    let mut s = lock();
    match nav {
        Nav::Up | Nav::Down => {
            let up = matches!(nav, Nav::Up);
            match s.focus {
                Focus::SchemaSelector => {
                    let len = s.schemas.len();
                    if len != 0 {
                        s.schema_cursor = if up {
                            s.schema_cursor.saturating_sub(1)
                        } else {
                            (s.schema_cursor + 1).min(len - 1)
                        };
                    }
                }
                Focus::EnvSelector => {
                    let len = s.schema_load.env_count();
                    if len != 0 {
                        s.env_cursor = if up {
                            s.env_cursor.saturating_sub(1)
                        } else {
                            (s.env_cursor + 1).min(len - 1)
                        };
                    }
                }
                Focus::OutputPane => {
                    let max = s.output_line_count.saturating_sub(1);
                    s.output_scroll = if up {
                        s.output_scroll.saturating_sub(1)
                    } else {
                        s.output_scroll.saturating_add(1).min(max)
                    };
                }
                // Buttons and the policy editor have nothing to move on Up/Down.
                _ => {}
            }
        }
        Nav::PageUp | Nav::PageDown => {
            // PageUp/PageDown scroll the output pane regardless of focus,
            // clamped to the last content line so scrolling past the end cannot
            // strand the offset near u16::MAX.
            let max = s.output_line_count.saturating_sub(1);
            s.output_scroll = if matches!(nav, Nav::PageUp) {
                s.output_scroll.saturating_sub(1)
            } else {
                s.output_scroll.saturating_add(1).min(max)
            };
        }
    }
    Ok(())
}

/// Apply one edit to the policy buffer, then bump the generation, mark the
/// output [`AnalysisOutput::Running`], and schedule a debounced re-analysis.
///
/// # Errors
///
/// * [`StateError::NothingToDelete`] if [`Edit::Backspace`] is applied to an
///   empty buffer (no generation bump, nothing scheduled).
pub fn edit_policy(edit: Edit) -> Result<(), StateError> {
    let generation = {
        let mut s = lock();
        match edit {
            Edit::Char(c) => s.policy_input.push(c),
            Edit::Newline => s.policy_input.push('\n'),
            Edit::Backspace => {
                if s.policy_input.pop().is_none() {
                    return Err(StateError::NothingToDelete);
                }
            }
        }
        s.generation += 1;
        s.output = AnalysisOutput::Running;
        s.generation
    };
    schedule_analysis(generation);
    Ok(())
}

/// Append a pasted chunk to the policy buffer, preserving all newlines, then
/// bump the generation, mark the output [`AnalysisOutput::Running`], and
/// schedule a debounced re-analysis. An empty paste is a no-op.
///
/// # Errors
///
/// Never returns `Err`; the `Result` is uniform with the other actions.
pub fn handle_paste(text: &str) -> Result<(), StateError> {
    if text.is_empty() {
        return Ok(()); // nothing pasted; do not churn the generation
    }
    let generation = {
        let mut s = lock();
        s.policy_input.push_str(text);
        s.generation += 1;
        s.output = AnalysisOutput::Running;
        s.generation
    };
    schedule_analysis(generation);
    Ok(())
}

/// Clear the policy editor: empty the buffer, reset the output to
/// [`AnalysisOutput::Empty`], bump the generation, and schedule (which, with an
/// empty policy, resolves to `Empty` again).
///
/// # Errors
///
/// Never returns `Err`; the `Result` is uniform with the other actions.
pub fn clear_policy() -> Result<(), StateError> {
    let generation = {
        let mut s = lock();
        s.policy_input.clear();
        s.output = AnalysisOutput::Empty;
        s.generation += 1;
        s.generation
    };
    schedule_analysis(generation);
    Ok(())
}

/// Advance keyboard focus to the next or previous widget.
///
/// Focus is not an analysis input, so this never bumps the generation.
///
/// # Errors
///
/// Never returns `Err`; the `Result` is uniform with the other actions.
pub fn cycle_focus(dir: FocusMove) -> Result<(), StateError> {
    let mut s = lock();
    s.focus = match dir {
        FocusMove::Next => s.focus.next(),
        FocusMove::Prev => s.focus.prev(),
    };
    Ok(())
}

/// Switch the analysis mode used by the encoder.
///
/// A no-op (no generation bump, nothing scheduled) if `mode` already matches the
/// current mode. Otherwise sets the mode, bumps the generation, marks the output
/// [`AnalysisOutput::Running`], and schedules a debounced re-analysis.
///
/// # Errors
///
/// Never returns `Err`; the `Result` is uniform with the other actions.
pub fn set_analysis_mode(mode: AnalysisMode) -> Result<(), StateError> {
    let generation = {
        let mut s = lock();
        if s.mode == mode {
            return Ok(()); // already in this mode
        }
        s.mode = mode;
        s.generation += 1;
        s.output = AnalysisOutput::Running;
        s.generation
    };
    schedule_analysis(generation);
    Ok(())
}

// ---------------------------------------------------------------------------
// The debounced analysis scheduler (the one shared private helper besides the
// lock accessor). Every input-changing action funnels through here.
// ---------------------------------------------------------------------------

/// Spawn a debounced analysis task for `generation`.
///
/// The task sleeps [`DEBOUNCE`] and then runs [`run_scheduled`]. A newer edit
/// bumps the generation and schedules its own task; when this one wakes it finds
/// the generation moved on and applies nothing, so exactly one analysis (the
/// latest) is ever applied. The lock is only ever taken *inside* the task, never
/// across the sleep.
fn schedule_analysis(generation: u64) {
    tokio::spawn(async move {
        // Debounce: wait for the input to be quiescent before doing any work.
        tokio::time::sleep(DEBOUNCE).await;
        run_scheduled(generation).await;
    });
}

/// Run the analysis for `generation`: snapshot inputs off the lock, encode, and
/// write the result back only if the generation still matches.
///
/// Split out from [`schedule_analysis`] so it can be driven deterministically in
/// tests (no timer). The generation is checked twice — before doing any work and
/// again before writing — so a result that a newer edit has superseded is
/// dropped and never displayed.
async fn run_scheduled(generation: u64) {
    // A local plan so the (heavy) inputs are cloned once, under the lock, and
    // the async encode below runs entirely OFF the lock.
    // The `Run` variant carries an owned `Schema` (large) while `Skip` is
    // tiny; the size gap is immaterial for a value built once per scheduled
    // analysis, so silence the enum-size lint rather than boxing.
    #[allow(clippy::large_enum_variant)]
    enum Plan {
        /// Preconditions unmet (no schema/env or empty policy): show `Empty`.
        Skip,
        /// Ready to encode.
        Run {
            src: String,
            schema: Schema,
            env: RequestEnv,
            mode: AnalysisMode,
        },
    }

    // (1) Under the lock: bail if superseded, else snapshot the inputs.
    let plan = {
        let s = lock();
        if s.generation != generation {
            return; // superseded during the debounce window
        }
        match &s.schema_load {
            SchemaLoad::Loaded { schema, envs, .. } if !s.policy_input.trim().is_empty() => {
                match envs.get(s.env_cursor) {
                    Some(entry) => Plan::Run {
                        src: s.policy_input.clone(),
                        // One real Schema clone, taken under the lock exactly as
                        // the old spawn_analysis did.
                        schema: Schema::clone(schema),
                        env: entry.env.clone(),
                        mode: s.mode,
                    },
                    None => Plan::Skip,
                }
            }
            _ => Plan::Skip,
        }
    };

    // (2) Run the (async) encode OFF the lock.
    let output = match plan {
        Plan::Skip => AnalysisOutput::Empty,
        Plan::Run {
            src,
            schema,
            env,
            mode,
        } => crate::analysis::analyze(src, schema, env, mode).await,
    };

    // (3) Under the lock: write back only if still current (stale-drop guard).
    // On accept, `output_line_count` is set to the result's rendered line count
    // (so `navigate` can clamp the scroll) and `output_scroll` is reset to 0.
    let mut s = lock();
    if s.generation != generation {
        return; // a newer input superseded this analysis while it ran
    }
    // Rendered line count, matching how the output widget splits the body on
    // '\n'; saturated into a u16 and floored at 1 (an empty body is one line).
    let line_count = match &output {
        AnalysisOutput::Empty | AnalysisOutput::Running => 1,
        AnalysisOutput::ParseErrors(errs) | AnalysisOutput::ValidationErrors(errs) => {
            errs.len().max(1)
        }
        AnalysisOutput::Encoded { smtlib } => smtlib.split('\n').count().max(1),
        AnalysisOutput::EncodeError(err) => err.split('\n').count().max(1),
    };
    s.output_line_count = line_count.min(u16::MAX as usize) as u16;
    s.output = output;
    s.output_scroll = 0;
}

#[cfg(test)]
mod tests {
    // The `test_guard()` serialization guard is deliberately held for a test's
    // whole duration — including across `.await` in the async tests — so global
    // state cannot be reset or mutated by another test mid-run. That is exactly
    // what this lint warns about, so allow it here. It is safe: the guard is a
    // plain test-only serialization mutex, never touched by production code.
    #![allow(clippy::await_holding_lock)]

    use super::*;

    /// A minimal Cedar schema with a single action, enough to enumerate at least
    /// one request env.
    const SCHEMA_SRC: &str = r#"
        entity User;
        entity Photo;
        action view appliesTo { principal: User, resource: Photo };
    "#;

    /// Serializes tests that touch the process-global [`STATE`] and resets it to
    /// [`initial_state`] before each. Every test that reads or writes the global
    /// acquires this first and holds it for the test's duration.
    ///
    /// Tests are a child module of `state`, so they may reach the private
    /// [`STATE`]/[`lock`]/[`run_scheduled`] and `AppState`'s private fields to
    /// establish preconditions (mirroring the old setters that seeded state);
    /// the domain action under test is still invoked through its public API.
    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn test_guard() -> MutexGuard<'static, ()> {
        let guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        *lock() = initial_state();
        guard
    }

    /// A temp directory for tests that exercise the real disk load path.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "symcc-state-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
        fn write(&self, name: &str, contents: &str) {
            std::fs::write(self.path.join(name), contents).expect("write temp file");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn sf(name: &str) -> SchemaFile {
        SchemaFile::from_path(PathBuf::from(name)).expect("recognized schema extension")
    }

    /// Seed a `Loaded` schema directly (no disk), mirroring the old
    /// `apply_schema_load` setup. Used to establish preconditions for actions
    /// under test.
    fn seed_loaded() {
        let schema: Schema = SCHEMA_SRC.parse().expect("inline schema parses");
        let envs: Vec<RequestEnvEntry> = schema.request_envs().map(RequestEnvEntry::new).collect();
        assert!(!envs.is_empty(), "schema should yield at least one env");
        let file = sf("seed.cedarschema");
        let mut s = lock();
        s.schemas = Arc::from(vec![file.clone()]);
        s.schema_cursor = 0;
        s.env_cursor = 0;
        s.schema_load = SchemaLoad::Loaded {
            file,
            schema: Arc::new(schema),
            envs: Arc::from(envs),
        };
    }

    #[test]
    fn schema_format_inferred_from_extension() {
        assert_eq!(sf("x.cedarschema").format, SchemaFormat::Cedar);
        assert_eq!(sf("x.cedar.json").format, SchemaFormat::Json);
        assert!(SchemaFile::from_path(PathBuf::from("x.txt")).is_none());
    }

    #[test]
    fn focus_next_cycles_through_all_variants() {
        let seq = [
            Focus::SchemaSelector,
            Focus::EnvSelector,
            Focus::PolicyInput,
            Focus::AlwaysAllowsButton,
            Focus::NeverAllowsButton,
            Focus::ClearButton,
            Focus::OutputPane,
        ];
        let mut f = Focus::SchemaSelector;
        for expected_next in seq.iter().skip(1).chain(std::iter::once(&seq[0])) {
            f = f.next();
            assert_eq!(f, *expected_next);
        }
        assert_eq!(f, Focus::SchemaSelector, "cycled back to the start");
        // prev is the inverse of next.
        assert_eq!(Focus::SchemaSelector.prev(), Focus::OutputPane);
    }

    #[test]
    fn cycle_focus_advances_and_reverses() {
        let _g = test_guard();
        assert_eq!(snapshot().focus(), Focus::SchemaSelector);
        cycle_focus(FocusMove::Next).unwrap();
        assert_eq!(snapshot().focus(), Focus::EnvSelector);
        cycle_focus(FocusMove::Prev).unwrap();
        assert_eq!(snapshot().focus(), Focus::SchemaSelector);
    }

    #[test]
    fn navigate_schema_cursor_clamps_at_both_ends() {
        let _g = test_guard();
        {
            let mut s = lock();
            s.schemas = Arc::from(vec![
                sf("a.cedarschema"),
                sf("b.cedarschema"),
                sf("c.cedarschema"),
            ]);
            s.focus = Focus::SchemaSelector;
        }
        for _ in 0..10 {
            navigate(Nav::Down).unwrap();
        }
        assert_eq!(snapshot().schema_cursor(), 2);
        for _ in 0..10 {
            navigate(Nav::Up).unwrap();
        }
        assert_eq!(snapshot().schema_cursor(), 0);
    }

    #[test]
    fn navigate_output_scroll_saturates_at_zero() {
        let _g = test_guard();
        {
            let mut s = lock();
            s.focus = Focus::OutputPane;
        }
        navigate(Nav::Up).unwrap(); // already 0, stays 0
        assert_eq!(snapshot().output_scroll(), 0);
        navigate(Nav::Down).unwrap();
        navigate(Nav::Down).unwrap();
        assert_eq!(snapshot().output_scroll(), 2);
        navigate(Nav::Up).unwrap();
        assert_eq!(snapshot().output_scroll(), 1);
    }

    #[test]
    fn navigate_output_scroll_clamps_to_content() {
        let _g = test_guard();
        {
            let mut s = lock();
            s.focus = Focus::OutputPane;
            s.output = AnalysisOutput::Encoded {
                smtlib: "a\nb\nc".into(),
            };
            s.output_line_count = 3;
            s.output_scroll = 0;
        }
        // Scroll down far past the end; it must clamp to the last line (index 2).
        for _ in 0..50 {
            navigate(Nav::Down).unwrap();
        }
        assert_eq!(snapshot().output_scroll(), 2);
        // A single Up now moves the view by exactly one line.
        navigate(Nav::Up).unwrap();
        assert_eq!(snapshot().output_scroll(), 1);
    }

    #[tokio::test]
    async fn clear_policy_clears_both_fields() {
        let _g = test_guard();
        {
            let mut s = lock();
            s.policy_input = "permit(principal, action, resource);".into();
            s.output = AnalysisOutput::Encoded { smtlib: "x".into() };
        }
        clear_policy().unwrap();
        let snap = snapshot();
        assert!(snap.policy_input().is_empty(), "policy input cleared");
        assert_eq!(*snap.output(), AnalysisOutput::Empty, "output cleared");
    }

    #[tokio::test]
    async fn edit_policy_char_appends_and_marks_running() {
        let _g = test_guard();
        edit_policy(Edit::Char('p')).unwrap();
        edit_policy(Edit::Char('q')).unwrap();
        let snap = snapshot();
        assert_eq!(snap.policy_input(), "pq");
        assert_eq!(*snap.output(), AnalysisOutput::Running);
    }

    #[test]
    fn edit_policy_backspace_on_empty_errs() {
        let _g = test_guard();
        // The error path returns before scheduling, so no runtime is needed.
        assert_eq!(
            edit_policy(Edit::Backspace),
            Err(StateError::NothingToDelete)
        );
    }

    #[tokio::test]
    async fn edit_policy_backspace_removes_last_char() {
        let _g = test_guard();
        edit_policy(Edit::Char('a')).unwrap();
        edit_policy(Edit::Char('b')).unwrap();
        edit_policy(Edit::Backspace).unwrap();
        assert_eq!(snapshot().policy_input(), "a");
    }

    #[tokio::test]
    async fn edit_policy_newline_inserts_newline() {
        let _g = test_guard();
        edit_policy(Edit::Char('a')).unwrap();
        edit_policy(Edit::Newline).unwrap();
        edit_policy(Edit::Char('b')).unwrap();
        assert_eq!(snapshot().policy_input(), "a\nb");
    }

    #[tokio::test]
    async fn handle_paste_appends_multiline_preserving_newlines() {
        let _g = test_guard();
        {
            let mut s = lock();
            s.policy_input = "x".into();
        }
        handle_paste("a\nb\nc").unwrap();
        let snap = snapshot();
        assert_eq!(snap.policy_input(), "xa\nb\nc");
        assert_eq!(*snap.output(), AnalysisOutput::Running);
    }

    #[test]
    fn handle_paste_empty_is_noop() {
        let _g = test_guard();
        {
            let mut s = lock();
            s.policy_input = "keep".into();
            s.output = AnalysisOutput::Empty;
        }
        handle_paste("").unwrap(); // returns Ok without scheduling
        let snap = snapshot();
        assert_eq!(snap.policy_input(), "keep");
        assert_eq!(*snap.output(), AnalysisOutput::Empty);
    }

    #[test]
    fn load_schema_under_cursor_no_schemas_errs() {
        let _g = test_guard();
        // Errors before any load/schedule, so no runtime is needed.
        assert_eq!(load_schema_under_cursor(), Err(StateError::NoSchemas));
    }

    #[tokio::test]
    async fn load_schema_under_cursor_bad_file_records_loadfailed_and_errs() {
        let _g = test_guard();
        let dir = TempDir::new("badload");
        dir.write("bad.cedarschema", "this is not a valid schema {{{");
        let files = crate::analysis::discover_schema_files(&dir.path).unwrap();
        {
            let mut s = lock();
            s.schemas = Arc::from(files);
            s.schema_cursor = 0;
        }
        match load_schema_under_cursor() {
            Err(StateError::SchemaLoadFailed(msg)) => assert!(!msg.is_empty()),
            other => panic!("expected SchemaLoadFailed, got {other:?}"),
        }
        assert!(
            matches!(snapshot().schema_load(), SchemaLoad::LoadFailed { .. }),
            "the failure is recorded in state so the pane can show it"
        );
    }

    #[tokio::test]
    async fn load_schema_reselect_noop_and_switch_wipes_output() {
        let _g = test_guard();
        let dir = TempDir::new("switch");
        dir.write("a.cedarschema", SCHEMA_SRC);
        dir.write("b.cedarschema", SCHEMA_SRC);
        // Two schemas => no auto-load; cursor starts at 0 (a).
        init(&dir.path).unwrap();
        assert_eq!(snapshot().schemas().len(), 2);

        // Load schema 0 (a), then dirty the policy + output.
        load_schema_under_cursor().unwrap();
        assert!(matches!(
            snapshot().schema_load(),
            SchemaLoad::Loaded { .. }
        ));
        edit_policy(Edit::Char('x')).unwrap();
        {
            let mut s = lock();
            s.output = AnalysisOutput::Encoded {
                smtlib: "cached".into(),
            };
        }
        let gen_before = lock().generation;

        // Re-selecting the already-loaded schema (cursor still 0) is a no-op.
        load_schema_under_cursor().unwrap();
        assert_eq!(
            lock().generation,
            gen_before,
            "re-selecting the loaded schema must not bump generation"
        );
        assert!(
            matches!(snapshot().output(), AnalysisOutput::Encoded { .. }),
            "reselect leaves output untouched"
        );

        // Switch to schema 1 (b): move the cursor, then load. Output is wiped;
        // the load is synchronous so the resting state is Loaded.
        {
            let mut s = lock();
            s.focus = Focus::SchemaSelector;
        }
        navigate(Nav::Down).unwrap();
        load_schema_under_cursor().unwrap();
        let snap = snapshot();
        assert_eq!(snap.schema_cursor(), 1);
        assert_eq!(snap.env_cursor(), 0);
        assert_eq!(*snap.output(), AnalysisOutput::Empty, "switch wipes output");
        assert!(matches!(snap.schema_load(), SchemaLoad::Loaded { .. }));
        assert_eq!(
            snap.policy_input(),
            "x",
            "switch must not clear policy input"
        );
    }

    #[tokio::test]
    async fn analysis_from_before_switch_is_dropped() {
        // Regression test for the schema-switch generation race: an analysis
        // snapshotted before a schema swap must never be applied after it.
        let _g = test_guard();
        let dir = TempDir::new("race");
        dir.write("a.cedarschema", SCHEMA_SRC);
        dir.write("b.cedarschema", SCHEMA_SRC);
        init(&dir.path).unwrap();
        load_schema_under_cursor().unwrap(); // load a
        edit_policy(Edit::Char('p')).unwrap();
        let stale_gen = lock().generation; // an analysis is "spawned" at this gen

        // User switches to schema b before that analysis returns.
        {
            let mut s = lock();
            s.focus = Focus::SchemaSelector;
        }
        navigate(Nav::Down).unwrap();
        load_schema_under_cursor().unwrap(); // installs b, bumps generation again

        // The stale analysis task wakes carrying the pre-switch generation, so
        // run_scheduled must drop it and write nothing.
        let output_before = snapshot().output().clone();
        run_scheduled(stale_gen).await;
        assert_eq!(
            *snapshot().output(),
            output_before,
            "an analysis snapshotted before the switch must never survive it"
        );
    }

    #[tokio::test]
    async fn scheduler_drops_stale_and_accepts_current() {
        let _g = test_guard();
        seed_loaded();
        {
            let mut s = lock();
            s.generation = 5;
            s.policy_input = "permit(principal, action, resource) when { 1 == 1 };".into();
            s.output = AnalysisOutput::Running;
        }
        // A superseded generation writes nothing.
        run_scheduled(4).await;
        assert_eq!(
            *snapshot().output(),
            AnalysisOutput::Running,
            "stale generation result is dropped"
        );
        // The current generation's result is applied and the scroll is reset.
        run_scheduled(5).await;
        assert!(
            matches!(snapshot().output(), AnalysisOutput::Encoded { .. }),
            "current generation result is accepted"
        );
        assert_eq!(snapshot().output_scroll(), 0, "scroll reset on accept");
    }

    #[tokio::test]
    async fn two_rapid_edits_apply_only_latest_generation() {
        let _g = test_guard();
        seed_loaded();
        {
            let mut s = lock();
            s.policy_input = "permit(principal, action, resource);".into();
        }
        // Two rapid edits: append a space, then delete it. Both keep the policy
        // valid but bump the generation twice.
        edit_policy(Edit::Char(' ')).unwrap();
        let stale = lock().generation;
        edit_policy(Edit::Backspace).unwrap();
        let current = lock().generation;
        assert_ne!(stale, current);

        // The first debounce task wakes with a superseded generation → nothing.
        run_scheduled(stale).await;
        assert_eq!(
            *snapshot().output(),
            AnalysisOutput::Running,
            "the superseded edit applies no analysis"
        );
        // The latest debounce task applies exactly one analysis.
        run_scheduled(current).await;
        assert!(
            matches!(snapshot().output(), AnalysisOutput::Encoded { .. }),
            "only the latest generation's analysis is applied"
        );
    }

    #[test]
    fn snapshot_is_an_independent_clone() {
        let _g = test_guard();
        {
            let mut s = lock();
            s.policy_input = "before".into();
        }
        let snap = snapshot();
        {
            let mut s = lock();
            s.policy_input = "after".into();
        }
        assert_eq!(
            snap.policy_input(),
            "before",
            "snapshot is a detached clone"
        );
        assert_eq!(snapshot().policy_input(), "after");
    }

    #[tokio::test]
    async fn init_single_schema_autoloads() {
        let _g = test_guard();
        let dir = TempDir::new("single");
        dir.write("only.cedarschema", SCHEMA_SRC);
        init(&dir.path).unwrap();
        let snap = snapshot();
        assert_eq!(snap.schemas().len(), 1);
        assert!(
            matches!(snap.schema_load(), SchemaLoad::Loaded { .. }),
            "the sole schema is auto-loaded"
        );
    }

    #[test]
    fn init_multiple_schemas_do_not_autoload() {
        let _g = test_guard();
        let dir = TempDir::new("multi");
        dir.write("a.cedarschema", SCHEMA_SRC);
        dir.write("b.cedarschema", SCHEMA_SRC);
        init(&dir.path).unwrap();
        let snap = snapshot();
        assert_eq!(snap.schemas().len(), 2);
        assert!(
            matches!(snap.schema_load(), SchemaLoad::NoneSelected),
            "multiple schemas are not auto-loaded"
        );
    }

    #[test]
    fn init_discovery_failure_errs() {
        let _g = test_guard();
        let dir = TempDir::new("nodir");
        let missing = dir.path.join("does-not-exist");
        match init(&missing) {
            Err(StateError::DiscoveryFailed(msg)) => assert!(!msg.is_empty()),
            other => panic!("expected DiscoveryFailed, got {other:?}"),
        }
    }
}
