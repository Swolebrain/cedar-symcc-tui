# cedar-symcc-tui

Terminal UI for inspecting the symbolic
compilation of Cedar policies. It parses a policy, strictly validates it against
a schema and request environment, and shows the SMT-LIB encoding of the policy's
condition produced by `cedar-policy-symcc` — the declarations and asserts for the
policy's scope plus its `when`/`unless` condition, in the chosen request
environment. No external SMT solver binary is required: the SMT-LIB is captured
from a `WriterSolver` (which only writes the script, never solves), so the tool
records the encoding rather than solving it.

## Build and run

Requires a recent stable Rust toolchain (install via [rustup](https://rustup.rs)).
Build from the package directory with `cargo build --release`; the binary lands
at `target/release/cedar-symcc-tui`. Alternatively install it onto your `PATH`
with `cargo install --path .`. On startup it discovers `.cedarschema` and
`.cedar.json` files in the **current working directory and its subdirectories**
(recursively, up to 8 levels deep). It prunes dot-directories (`.git`, ...) and
the `build`, `target`, `env`, and `node_modules` trees, does not follow
directory symlinks, and caps the list at 2000 files. Each entry is shown by its
path relative to the launch directory, so run it from the directory holding (or
containing) your schemas:

```
cd path/to/my-schemas
/path/to/cedar-symcc-tui/target/release/cedar-symcc-tui
```

If exactly one schema is found it is auto-selected and loaded; otherwise pick one
and press `Enter`. Discovery failures never abort — the tool starts empty.

It must be run in an interactive terminal. A piped or redirected invocation (no
controlling TTY) exits non-zero with `cedar-symcc-tui must be run in an
interactive terminal` rather than failing with an opaque OS error.

## Panes (top to bottom)

- **Schemas** (top-left) — discovered files; a green `●` marks the loaded one,
  overflow scrolls within a fixed 10-line viewport.
- **Request envs** (top-right) — request environments (`principal, action,
  resource`) from the loaded schema; a placeholder before load, error on failure.
- **Policy** — multi-line Cedar editor (newlines and pastes preserved) with an
  embedded `[ Clear ]` button.
- **Output** — parse/validation/encoding result, with a `line X/Y` indicator in
  the bottom border.

## Key / mouse bindings

| Key | Context | Action |
|-----|---------|--------|
| `Ctrl+C`, `Ctrl+Q` | any | Quit |
| `Esc` | any except Policy editor | Quit |
| `Ctrl+L` | any | Clear policy + output |
| `Tab` / `Shift+Tab` | any | Focus next / previous pane |
| `PageUp` / `PageDown` | any | Scroll output pane |
| `↑`/`k`, `↓`/`j` | Schemas | Move cursor |
| `Enter` | Schemas | Load schema under cursor |
| `↑`/`k`, `↓`/`j` | Request envs | Move cursor |
| `↑`/`k`, `↓`/`j` | Output | Scroll |
| `Enter` / `Space` | Clear button | Clear policy + output |
| char, `Enter`, `Backspace` | Policy editor | Insert char / newline, delete |
| Left-click | Clear button | Clear policy + output |

Focus cycles Schemas → Request envs → Policy → Clear button → Output. In the
Policy editor `Esc`, `j`, `k` are literal text.

## Output semantics

The pipeline shows the first stage that produces a result, in priority order:
**parse errors** → **validation errors** (strict) → **SMTLib encoding**.

The encoding is the SMT-LIB declarations and asserts for each policy's encoded
condition — the policy's scope plus its `when`/`unless` condition, in the
selected request environment. It is produced by capturing the script the crate
writes to a solver, so no solver binary is invoked. Each policy's block begins
with an SMT-LIB comment header giving the policy id and the request environment,
e.g. `; policy policy0 @ User, Action::"view", Photo`.

A policy's condition can still fold to a constant: a trivial
`permit(principal, action, resource);` matches *every* request in the
environment, so its condition is constant-true and symcc writes no assertions.
In that case the tool keeps the header and appends an explanatory comment that
the condition folds to a constant (rather than showing a blank pane). A policy
with a non-constant condition — for a schema where `principal` has a `Long`
attribute `age`, `permit(principal, action, resource) when { principal.age
+ 1 > 19 };` — matches some requests and not others, so it does not fold and the
encoding emits declarations, `(assert …)`, and `(check-sat)`.

## Architecture

- `main.rs` — terminal lifecycle (raw mode, alternate screen, mouse/paste
  capture, panic-hook restore) and the async event loop. It holds no state of
  its own: each frame it takes one `state::snapshot()` and mutates only through
  `state::` domain actions.
- `app.rs` — layout and pure `route_key` routing (`(Focus, key)` → `Action`),
  applied via `apply_action`, which calls exactly one state action per action.
- `state/mod.rs` — a **private process-global** `AppState` behind a single
  `LazyLock<Mutex<…>>` that never escapes the module. Callers read via
  `snapshot()` and mutate via high-level **domain actions** (`init`,
  `load_schema_under_cursor`, `navigate`,
  `edit_policy`, `handle_paste`, `clear_policy`, `cycle_focus`,
  `set_analysis_mode`) that each return `Result<(), StateError>`. Enum-modeled
  state makes invalid combinations unrepresentable.
- `analysis/mod.rs` — schema discovery, `load_schema`, parse, validate, async
  SMTLib encode.
- `widgets/{schema_selector,env_selector,policy_input,output_pane}.rs` — render
  one `AppState` snapshot each; they never handle input.

Four ideas hold the design together. **Encapsulated global:** the mutex is
private to `state`; no function takes or returns a state handle, and the lock is
never held across an `.await`. **Snapshot-per-frame rendering:** the event loop
clones one `AppState` per frame and renders every widget from it without holding
the lock; the clone is cheap because all heavy data (the loaded schema, request
envs, schema file list, analysis output text and error lists) lives behind an
`Arc`, so a snapshot is a handful of refcount bumps plus a small `policy_input`
copy. **In-state debounce:** every action that changes an analysis input bumps a
monotonic `generation` and schedules a background task that sleeps 300 ms, then
runs the encode off the lock and writes the result back — the scheduler lives
inside `state`, not the event loop. **Generation epoch:** async results are
applied only if `generation` still matches when they land, so a newer edit
supersedes any in-flight analysis and exactly one result (the latest) is ever
shown.
