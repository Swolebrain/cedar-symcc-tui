# State design

This document explains how application state is owned, read, and changed in
cedar-symcc-tui, and why the public mutation API looks the way it does. The
README describes what the tool does; this file describes the rules the code
follows and the reasons for them.

## Contents

1. [Layers and what each owns](#1-layers-and-what-each-owns)
2. [Core rules](#2-core-rules)
3. [Why state actions are coarse and user-facing](#3-why-state-actions-are-coarse-and-user-facing)
4. [`Action` carries the state layer's own types](#4-action-carries-the-state-layers-own-types)
5. [Adding a new action](#5-adding-a-new-action)
6. [Known deviations](#6-known-deviations)

## 1. Layers and what each owns

| Layer | Owns | Never does |
|-------|------|------------|
| `src/main.rs` | Terminal lifecycle (raw mode, alternate screen, bracketed paste, mouse capture, panic hook), the event loop, mouse hit-testing against `app::FrameHits` | Hold state, take a lock, run timers, track generations |
| `src/app.rs` | Frame layout (`draw`), pure `route_key(Focus, KeyEvent) -> Option<Action>`, and `apply_action`, which maps each `Action` to exactly one state call | Lock anything or hold a state handle |
| `src/widgets/*.rs` | Rendering one `&AppState` each (`schema_selector`, `env_selector`, `policy_input`, `output_pane`) | Handle input or mutate state |
| `src/state/mod.rs` | All application state, its invariants, the debounce, and the generation epoch | Hand out the mutex or a `&mut AppState` |
| `src/analysis/mod.rs` | Schema discovery, `load_schema`, parse, strict validation, `analyze` (SMT-LIB encode) | Touch application state; it takes owned inputs and returns values |

Data flows one way in each direction:

```
render:  state::snapshot() ──► app::draw(frame, &AppState) ──► widgets::*::render
input:   KeyEvent ──► app::route_key ──► Action ──► app::apply_action ──► state::<action>
```

`policy_input::render` returns the screen rects of its three buttons. That is
the only thing a widget hands back, and it is layout, not state: `main.rs`
keeps the rects in `FrameHits` to hit-test the next mouse click.

## 2. Core rules

### Encapsulated global

The whole state is one private `static STATE: LazyLock<Mutex<AppState>>` in
`src/state/mod.rs`. The private function `lock()` is the only caller of
`.lock()` in the crate. It recovers a poisoned mutex with
`unwrap_or_else(|e| e.into_inner())`, so one panic cannot deadlock the UI.

`AppState`'s fields are private. Outside the module there are only read-only
getters (`schemas()`, `schema_cursor()`, `schema_load()`, `env_cursor()`,
`policy_input()`, `output()`, `output_scroll()`, `focus()`, `mode()`). A write
from another module is a compile error, not a convention someone has to
remember.

### Snapshot per frame

`snapshot()` clones `AppState` under the lock and returns the clone. The lock is
released before rendering starts. Two things follow:

- A frame is drawn from one consistent state, so widgets never see a half-applied
  update.
- A slow render never blocks a background analysis that wants to write its
  result back.

The clone is cheap because heavy data sits behind `Arc`: the schema list
(`Arc<[SchemaFile]>`), the loaded schema (`Arc<Schema>`), its request envs
(`Arc<[RequestEnvEntry]>`), and the analysis output (`Arc<str>` /
`Arc<[String]>`). Only `policy_input` is a real `String` copy. It is user-typed
and small.

### Invalid states are unrepresentable

Request envs exist only inside `SchemaLoad::Loaded { envs, .. }`. The three
variants of `SchemaLoad` are each a real situation:

- `NoneSelected`: no schema has been chosen yet.
- `Loaded { file, schema, envs }`: the schema parsed and its envs are known.
- `LoadFailed { file, error }`: the parse failed; the env pane shows `error`.

There is no way to hold envs for a schema that failed to load.

The output pane's content is one enum, `AnalysisOutput`, with the variants
`Empty`, `Running`, `ParseErrors`, `ValidationErrors`, `Encoded`, and
`EncodeError`. It replaces what would otherwise be a set of flags and optional
fields that could disagree with each other. `Focus`, `Nav`, `Edit`,
`FocusMove`, and `AnalysisMode` are closed enums too. `AppState` has no bare
`bool` fields.

### Never hold the lock across `.await`; generation epoch

Every action that changes an analysis input bumps `generation` and calls the
private `schedule_analysis(generation)`. That function `tokio::spawn`s a task
that:

1. sleeps `DEBOUNCE` (300 ms);
2. in `run_scheduled`, takes the lock, returns if `generation` has moved on,
   otherwise clones the inputs and releases the lock;
3. runs `analysis::analyze` off the lock;
4. takes the lock again and writes the result only if `generation` still
   matches.

One counter gives both debounce (a newer keystroke makes the older task a
no-op) and latest-wins (a stale result is never applied). The event loop knows
nothing about any of it; it redraws every 50 ms tick and picks up whatever the
task wrote.

## 3. Why state actions are coarse and user-facing

The public mutation API is a short list of user verbs:

`init`, `load_schema_under_cursor`, `navigate`, `edit_policy`, `handle_paste`,
`clear_policy`, `cycle_focus`, `set_analysis_mode`.

There is no `set_env_cursor`, no `set_output`, no `bump_generation`. That is
deliberate.

### Worked example: `load_schema_under_cursor`

1. Under the lock, read `schema_cursor` and the schema list. Return
   `Err(NoSchemas)` if there is nothing at the cursor, and return `Ok(())`
   without changes if that file is already the loaded one.
2. Release the lock, then read and parse the file with `analysis::load_schema`.
   File I/O never runs under the mutex.
3. Under one lock: reset `env_cursor` to 0, set `output` to `Empty`, install
   the new `schema_load`, and bump `generation`.
4. On success, call `schedule_analysis`. On failure, the state already holds
   `SchemaLoad::LoadFailed` (so the env pane shows the error), and the action
   also returns `Err(StateError::SchemaLoadFailed(..))`.

### What breaks if the UI composed small setters instead

**Torn intermediate states become observable.** If step 3 were four separate
setter calls, a background task or the next frame could run between them. It
might see the new schema with the old `env_cursor`, which can be out of bounds
for the new env list, or the new schema next to the old schema's SMT-LIB.

**Every caller must know the recipe.** A schema load is triggered by `Enter` in
the schema list and by `init` auto-loading a sole schema, and a mouse click
could trigger it later. Each would have to repeat the same steps in the same
order. Forgetting `bump_generation` in one of them lets a stale in-flight
result silently overwrite the new schema's output.

**Validation separates from mutation.** `edit_policy(Edit::Backspace)` on an
empty buffer returns `Err(NothingToDelete)` and does not bump the generation or
schedule anything. With setters, the "is there anything to delete" decision
would move into the UI, next to the key handling.

**Errors lose meaning.** Domain actions return user-level errors. The current
`StateError` variants are `NoSchemas`, `SchemaLoadFailed(String)`,
`DiscoveryFailed(String)`, and `NothingToDelete`. A setter can only report that
it set a value. Because each action leaves the state consistent and already
reflects the outcome on screen, `main.rs` can treat every `StateError` as benign
and ignore it.

**The UI stays simple and testable.** `route_key` is pure, so the tests in
`src/app.rs` check key bindings with no terminal and no global state.
`apply_action` is a flat table. The rules live in one module and are tested
there as user-level sequences.

The boundary sits at user intent. The UI reports what happened; the state layer
decides what that means for every field, the generation, and the scheduler.
Actions are complex inside so that callers cannot get the order wrong.

The other actions follow the same shape. `edit_policy`, `handle_paste`, and
`set_analysis_mode` mutate under the lock, bump `generation`, set `output` to
`Running`, release the lock, and schedule. `clear_policy` does the same but sets
`output` to `Empty`. `set_analysis_mode` and `handle_paste` return early without
bumping when nothing would change (same mode, empty paste). `navigate` and
`cycle_focus` never bump; that decision is made inside the state module, not by
the caller.

## 4. `Action` carries the state layer's own types

`Action::SetMode(AnalysisMode)` becomes `state::set_analysis_mode(mode)` and
`Action::Navigate(Nav)` becomes `state::navigate(nav)`. Both are
pass-throughs. There is no UI-side enum that has to be translated into the
state's enum, so there is no translation layer that could drop or distort
intent.

`Action` still exists, rather than `route_key` calling state directly, because
it keeps `route_key` pure. A function that returns a value can be tested
without global state; a function that mutates the global cannot.

## 5. Adding a new action

- Name it after the user's intent (`load_schema_under_cursor`), not after the
  field it touches.
- Do all related field updates under one `lock()`.
- Do file or other blocking I/O off the lock, between lock scopes.
- Bump `generation` if and only if an analysis input changed, then call
  `schedule_analysis` after releasing the lock.
- Return a `StateError` variant with a user-level meaning. Keep the rule that
  every variant is returned by some action; add a variant only when an action
  needs it.
- Add an `Action` variant that carries the state layer's types, a `route_key`
  binding, a one-line arm in `apply_action`, and a `route_key` test in
  `src/app.rs`.

## 6. Known deviations

**Mouse clicks bypass `Action`.** In `run` in `src/main.rs`, a left click on a
button calls `state::set_analysis_mode(AnalysisMode::AlwaysAllows)`,
`state::set_analysis_mode(AnalysisMode::NeverAllows)`, or
`state::clear_policy()` directly instead of producing an `Action` and going
through `apply_action`. These are still domain actions, so the invariants hold.
The input path is just not uniform: keyboard input goes through the pure router
and mouse input does not. Pastes also call `state::handle_paste` directly.

**Moving the env cursor does not schedule an analysis.** `run_scheduled` reads
`envs[env_cursor]`, so the env cursor is an analysis input. `navigate` changes
`env_cursor` but never bumps `generation` or schedules, so the output keeps
showing the previous env's encoding until another input changes (an edit,
paste, clear, mode switch, or schema load). The module docs in
`src/state/mod.rs` list "env selection" among the inputs that bump the
generation, which the code does not currently do for cursor moves.
