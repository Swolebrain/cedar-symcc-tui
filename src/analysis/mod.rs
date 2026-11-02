//! Pure analysis pipeline: schema discovery, schema loading, policy parsing,
//! validation, and async SMTLib encoding via `cedar-policy-symcc`.
//!
//! # Purity / lock discipline
//!
//! This module is **pure** with respect to application state: every function
//! takes owned inputs and *returns* a value (a [`Vec<SchemaFile>`], a
//! [`SchemaLoad`], or an [`AnalysisOutput`]). It never acquires the state lock
//! and never holds anything across an `.await`. The `state` module's domain
//! actions call into it and install the results into the global state.

// These public functions are consumed by a later phase (the event loop / async
// spawner) that is not wired yet; they are exercised by the in-file unit tests.
// Mirror the `state` module and allow them to exist without a non-test caller
// until the UI lands.
#![allow(dead_code)]

use std::{path::Path, str::FromStr};

use cedar_policy::{PolicySet, RequestEnv, Schema, ValidationMode, Validator};
use cedar_policy_symcc::{
    always_allows_asserts, err::Error as SymccError, never_matches_asserts, solver::WriterSolver,
    CedarSymCompiler, CompiledPolicy, CompiledPolicySet, WellFormedAsserts,
};

use crate::state::{
    AnalysisMode, AnalysisOutput, RequestEnvEntry, SchemaFile, SchemaFormat, SchemaLoad,
};

/// Maximum directory depth to descend below the discovery root. The root's own
/// entries are at depth 0, so files are found up to this many directory levels
/// beneath the root. Bounds worst-case work so a run from a deep tree cannot
/// hang the UI.
const MAX_DEPTH: usize = 8;
/// Hard cap on the number of discovered schema files. Together with
/// [`MAX_DEPTH`] this bounds a run started from a huge directory: once the cap
/// is hit the walk stops and returns what it has so far.
const MAX_RESULTS: usize = 2000;
/// Directory names (exact match) pruned from the walk, in addition to any
/// dot-prefixed directory (`.git`, `.kiro`, ...). These are large, generated,
/// or environment trees that never hold hand-authored schemas and would only
/// slow discovery down in a large workspace.
const PRUNED_DIRS: &[&str] = &["build", "target", "env", "node_modules"];

/// Discover schema files under `root`, RECURSIVELY (this is the CWD the binary
/// was launched from).
///
/// Walks `root` and its subdirectories with a hand-rolled [`std::fs::read_dir`]
/// recursion (no `walkdir`/extra dependency), collecting every regular file
/// whose name ends in `.cedarschema` or `.cedar.json`. The double extension
/// `.cedar.json` is matched on the full name (handled by
/// [`SchemaFormat::from_path`], which checks it before `.cedarschema`).
/// Unrelated files are ignored.
///
/// # Pruning and limits
///
/// - Directories whose name starts with `.` plus [`PRUNED_DIRS`] are skipped.
/// - Symlinked directories are NOT followed, guarding against symlink cycles.
/// - Recursion stops at [`MAX_DEPTH`] levels below the root; results are capped
///   at [`MAX_RESULTS`].
///
/// # Errors
///
/// A failure to read the ROOT itself is propagated as [`std::io::Error`].
/// Failures deeper in the tree (e.g. an unreadable subdirectory) are swallowed
/// silently so one bad directory does not abort the whole walk.
///
/// Results are sorted by [`SchemaFile::display`], the path relative to `root`,
/// so the list is stable and grouped by directory.
pub fn discover_schema_files(root: &Path) -> std::io::Result<Vec<SchemaFile>> {
    let mut files = Vec::new();
    // Reading the ROOT is allowed to fail (propagated); read it here so that
    // failure surfaces, then hand the entries to the recursive helper, which
    // swallows read failures for everything below the root.
    let root_entries = std::fs::read_dir(root)?;
    walk_dir(root, root_entries, 0, &mut files);
    files.sort_by(|a, b| a.display.cmp(&b.display));
    Ok(files)
}

/// Recursively visit the already-opened `entries` of one directory at `depth`
/// (root == 0), pushing schema files into `files` and descending into
/// non-pruned subdirectories. Errors below the root are swallowed.
fn walk_dir(root: &Path, entries: std::fs::ReadDir, depth: usize, files: &mut Vec<SchemaFile>) {
    for entry in entries {
        if files.len() >= MAX_RESULTS {
            return;
        }
        // A single unreadable entry is skipped, not fatal.
        let Ok(entry) = entry else { continue };
        // `DirEntry::file_type` does NOT traverse symlinks, so a symlink to a
        // directory reports `is_symlink() == true` / `is_dir() == false` and is
        // therefore never recursed into — this is our symlink-cycle guard.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();

        if file_type.is_file() {
            // `SchemaFile::from_path`'s display would be the bare file name; we
            // instead build the file with a display RELATIVE to `root` so that
            // nested files with identical names remain distinguishable. `path`
            // stays the full joined path so loading still works.
            if let Some(format) = SchemaFormat::from_path(&path) {
                files.push(SchemaFile {
                    display: relative_display(root, &path),
                    path,
                    format,
                });
            }
        } else if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Prune dot-directories and the well-known generated/env trees.
            if name.starts_with('.') || PRUNED_DIRS.contains(&name.as_ref()) {
                continue;
            }
            // Enforce the depth cap: the subdirectory's entries would live at
            // `depth + 1`, so only descend while that stays within MAX_DEPTH.
            if depth + 1 > MAX_DEPTH {
                continue;
            }
            // Skip unreadable subdirectories silently (permission errors deep in
            // the tree must not abort the whole walk).
            if let Ok(sub_entries) = std::fs::read_dir(&path) {
                walk_dir(root, sub_entries, depth + 1, files);
            }
        }
        // Anything else (symlinks, sockets, ...) is ignored.
    }
}

/// Render `path` relative to `root` for display, falling back to the full path
/// if `path` is somehow not under `root`.
fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Read `file` from disk and build a [`cedar_policy::Schema`], then enumerate
/// its request environments.
///
/// JSON schemas (`SchemaFormat::Json`) are parsed with `Schema::from_json_str`;
/// human-readable schemas (`SchemaFormat::Cedar`) with `Schema::from_str`. On
/// success returns [`SchemaLoad::Loaded`] with the parsed schema and its
/// `request_envs()` rendered into [`RequestEnvEntry`] labels; on any I/O or
/// parse failure returns [`SchemaLoad::LoadFailed`] with the rendered error.
pub fn load_schema(file: &SchemaFile) -> SchemaLoad {
    let text = match std::fs::read_to_string(&file.path) {
        Ok(text) => text,
        Err(err) => {
            return SchemaLoad::LoadFailed {
                file: file.clone(),
                error: err.to_string(),
            }
        }
    };

    // The two parsers return different error types (`SchemaError` vs
    // `CedarSchemaError`), so render each arm's error to a `String` in place.
    let parsed: Result<Schema, String> = match file.format {
        SchemaFormat::Json => Schema::from_json_str(&text).map_err(|e| e.to_string()),
        SchemaFormat::Cedar => Schema::from_str(&text).map_err(|e| e.to_string()),
    };
    let schema = match parsed {
        Ok(schema) => schema,
        Err(error) => {
            return SchemaLoad::LoadFailed {
                file: file.clone(),
                error,
            }
        }
    };

    let envs: Vec<RequestEnvEntry> = schema.request_envs().map(RequestEnvEntry::new).collect();
    SchemaLoad::Loaded {
        file: file.clone(),
        schema: schema.into(),
        envs: envs.into(),
    }
}

/// Run the full pipeline for `policy_src` against `schema`/`env`, returning what
/// the output pane should display.
///
/// Priority order:
/// 1. **Parse.** `PolicySet::from_str`; on failure →
///    [`AnalysisOutput::ParseErrors`], one readable string per error.
/// 2. **Validate.** `Validator::validate(.., ValidationMode::Strict)`; if it did
///    not pass → [`AnalysisOutput::ValidationErrors`] from `validation_errors()`.
/// 3. **Encode.** Branches on `mode`.
///
///    For [`NeverAllows`](AnalysisMode::NeverAllows) (default), for each policy:
///    `CompiledPolicy::compile` → `never_matches_asserts` → `CedarSymCompiler`
///    over a [`WriterSolver`] → `check_unsat`. Each policy's block is prefixed
///    with an SMT-LIB comment header identifying the policy id and env.
///
///    For [`AlwaysAllows`](AnalysisMode::AlwaysAllows), ONE
///    `CompiledPolicySet::compile` for the whole set → `always_allows_asserts`
///    → the SAME `CedarSymCompiler`/`WriterSolver`/`check_unsat` path, producing
///    a single block for the whole set (not per-policy).
///
///    The concatenated SMTLib text is returned in [`AnalysisOutput::Encoded`].
///
/// Why a query builder at all: the crate emits SMT-LIB only as a side effect of
/// building a solver query — its `Encoder` is private and `CompiledPolicy`
/// exposes no Term/SymEnv, so there is no lower-level "just encode" entry point.
/// Every single-policy builder emits the same `enforce(...) ++ compiled term`
/// body and differs only in a final wrapper assert; `never_matches_asserts` adds
/// the least (`eq(term, some(true))`), so it is used to stay closest to the bare
/// encoding.
pub async fn analyze(
    policy_src: String,
    schema: Schema,
    env: RequestEnv,
    mode: AnalysisMode,
) -> AnalysisOutput {
    // (1) Parse the policy text into a PolicySet.
    let pset = match PolicySet::from_str(&policy_src) {
        Ok(pset) => pset,
        Err(errs) => {
            return AnalysisOutput::ParseErrors(errs.iter().map(|e| e.to_string()).collect())
        }
    };

    // (2) Strict validation against the schema.
    let result = Validator::new(schema.clone()).validate(&pset, ValidationMode::Strict);
    if !result.validation_passed() {
        return AnalysisOutput::ValidationErrors(
            result.validation_errors().map(|e| e.to_string()).collect(),
        );
    }

    // (3) Encode to SMTLib text. The query used depends on `mode`.
    // `principal, action, resource` rendering of the request env for the header.
    let env_label = format!("{}, {}, {}", env.principal(), env.action(), env.resource());
    let mut smtlib = String::new();

    match mode {
        // DEFAULT: per-policy `never_matches` query — preserves the original
        // behaviour exactly.
        AnalysisMode::NeverAllows => {
            let policies: Vec<&cedar_policy::Policy> = pset.policies().collect();
            for policy in policies {
                let compiled = match CompiledPolicy::compile(policy, &env, &schema) {
                    Ok(compiled) => compiled,
                    Err(err) => return AnalysisOutput::EncodeError(err.to_string().into()),
                };
                let asserts = never_matches_asserts(&compiled);
                let body = match encode_smtlib(&asserts).await {
                    Ok(body) => body,
                    Err(out) => return out,
                };

                // SMT-LIB comment header identifying which policy this block
                // encodes and against which request environment. Always
                // emitted, even when the body below folds away, so the pane is
                // never blank or ambiguous.
                smtlib.push_str(&format!("; policy {} @ {}\n", policy.id(), env_label));

                if body.trim().is_empty() {
                    // The encoded condition can still legitimately produce
                    // NOTHING when it folds to a constant in this request
                    // environment — e.g. `permit(principal, action, resource);`
                    // matches every request, so its match term is constant-true
                    // and symcc short-circuits without writing any script.
                    // Surface an explanatory comment rather than an empty block.
                    smtlib.push_str(
                        "; This policy's condition folds to a constant, so no assertions were emitted.\n\
                         ; (The policy trivially matches, or trivially cannot match, every request.)\n",
                    );
                } else {
                    smtlib.push_str(&body);
                }
                // Guarantee a separating newline between per-policy blocks.
                if !smtlib.ends_with('\n') {
                    smtlib.push('\n');
                }
            }

            // A policy set with no policies (e.g. empty input that still parsed)
            // yields no blocks at all; keep a non-empty, explanatory result
            // rather than a blank pane.
            if smtlib.trim().is_empty() {
                smtlib = "; No policies to encode (the policy set is empty).\n".to_string();
            }
        }

        // The `always_allows` query is about the WHOLE policy set, so we compile
        // one `CompiledPolicySet` and emit a single block (not per-policy).
        AnalysisMode::AlwaysAllows => {
            let compiled_set = match CompiledPolicySet::compile(&pset, &env, &schema) {
                Ok(set) => set,
                Err(err) => return AnalysisOutput::EncodeError(err.to_string().into()),
            };
            let asserts = always_allows_asserts(&compiled_set);
            let body = match encode_smtlib(&asserts).await {
                Ok(body) => body,
                Err(out) => return out,
            };

            // Header for the single whole-set block, naming the query and env.
            smtlib.push_str(&format!("; always_allows @ {}\n", env_label));

            if body.trim().is_empty() {
                // As with a single policy, the always-allows condition can fold
                // to a constant in this request environment (e.g. the set
                // trivially allows, or trivially cannot allow, every request),
                // so symcc writes no script. Surface an explanatory comment
                // rather than an empty block.
                smtlib.push_str(
                    "; The policy set's always-allows condition folds to a constant, so no assertions were emitted.\n\
                     ; (The set trivially allows, or trivially cannot allow, every request.)\n",
                );
            } else {
                smtlib.push_str(&body);
            }
            if !smtlib.ends_with('\n') {
                smtlib.push('\n');
            }
        }
    }

    AnalysisOutput::Encoded {
        smtlib: smtlib.into(),
    }
}

/// Drive one [`WellFormedAsserts`] bundle through a [`WriterSolver`]-backed
/// [`CedarSymCompiler`] and return the emitted, normalised SMT-LIB text.
///
/// Shared by both analysis modes so the compiler / `check_unsat` / buffer
/// read-out path stays identical. On a genuine encoding failure it returns
/// `Err(AnalysisOutput::EncodeError(..))` so the caller can return it as-is;
/// the caller is responsible for the header and the constant-fold empty case.
async fn encode_smtlib(asserts: &WellFormedAsserts<'_>) -> Result<String, AnalysisOutput> {
    let mut compiler = match CedarSymCompiler::new(WriterSolver {
        w: Vec::<u8>::new(),
    }) {
        Ok(compiler) => compiler,
        Err(err) => return Err(AnalysisOutput::EncodeError(err.to_string().into())),
    };

    // IMPORTANT (looks like a bug, but is not): `WriterSolver::check_sat`
    // unconditionally reports `Decision::Unknown` — it only writes the
    // SMTLib script, it never actually solves. So `check_unsat` ALWAYS
    // returns `Err(Error::SolverUnknown)` even though encoding fully
    // succeeded. We therefore treat `Err(SolverUnknown)` as the SUCCESS
    // path and read the emitted text out of the writer's buffer. Every
    // OTHER error variant is a genuine encoding failure.
    match compiler.check_unsat(asserts).await {
        Ok(_) | Err(SymccError::SolverUnknown) => {}
        Err(err) => return Err(AnalysisOutput::EncodeError(err.to_string().into())),
    }

    // Take the accumulated bytes out of the writer (`WriterSolver.w` is a
    // public field; we reach it through the compiler's `solver_mut()`
    // accessor and `mem::take` the `Vec<u8>` so the compiler can drop).
    let buf = std::mem::take(&mut compiler.solver_mut().w);
    let text = match String::from_utf8(buf) {
        Ok(text) => text,
        Err(err) => {
            return Err(AnalysisOutput::EncodeError(
                format!("SMTLib output was not valid UTF-8: {err}").into(),
            ))
        }
    };

    Ok(normalize_smtlib(&text))
}

/// Normalise emitted SMTLib for readable display.
///
/// The writer already emits newline-delimited s-expressions (`(reset)`,
/// `(set-logic ALL)`, declarations, `(assert ..)`, `(check-sat)`), so we do NOT
/// re-indent or otherwise restructure it — that would risk corrupting the
/// s-expressions. We only normalise CRLF → LF and trim trailing whitespace from
/// each line. Returns text without a forced trailing newline (the caller adds a
/// block separator).
fn normalize_smtlib(text: &str) -> String {
    text.replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A minimal but valid Cedar schema (human syntax) with one action and a
    /// `Long` attribute, enough to enumerate a request env AND to make a
    /// `never_matches` encoding non-trivial (see the encode test).
    const SCHEMA_SRC: &str = r#"
        entity User { age: Long };
        entity Photo;
        action view appliesTo { principal: User, resource: Photo };
    "#;

    /// Create a unique temp directory under `std::env::temp_dir()`. Caller must
    /// remove it (see `TempDir`).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            let unique = format!(
                "symcc-analysis-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            path.push(unique);
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
        fn child(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
        /// Create a (possibly nested) subdirectory under the temp root and
        /// return its path.
        fn subdir(&self, rel: &str) -> PathBuf {
            let dir = self.path.join(rel);
            std::fs::create_dir_all(&dir).expect("create subdir");
            dir
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn discover_finds_both_extensions_and_ignores_others() {
        let dir = TempDir::new("discover");
        std::fs::write(dir.child("a.cedarschema"), "entity User;").unwrap();
        std::fs::write(dir.child("b.cedar.json"), "{}").unwrap();
        std::fs::write(dir.child("notes.txt"), "ignore me").unwrap();
        std::fs::write(dir.child("policy.cedar"), "permit(...);").unwrap();

        let found = discover_schema_files(&dir.path).expect("read dir");
        let names: Vec<&str> = found.iter().map(|f| f.display.as_str()).collect();
        assert_eq!(
            names,
            vec!["a.cedarschema", "b.cedar.json"],
            "sorted, only schema files"
        );
        assert_eq!(found[0].format, SchemaFormat::Cedar);
        assert_eq!(
            found[1].format,
            SchemaFormat::Json,
            ".cedar.json matched on double extension"
        );
        // Root-level files keep a bare-name display (nothing to make relative).
        assert!(found
            .iter()
            .all(|f| f.path.is_absolute() || f.path.starts_with(&dir.path)));
    }

    #[test]
    fn discover_finds_schema_nested_two_dirs_deep() {
        let dir = TempDir::new("nested");
        let deep = dir.subdir("examples/foo");
        std::fs::write(deep.join("policies.cedarschema"), "entity User;").unwrap();

        let found = discover_schema_files(&dir.path).expect("read dir");
        let displays: Vec<&str> = found.iter().map(|f| f.display.as_str()).collect();
        let expected = PathBuf::from("examples")
            .join("foo")
            .join("policies.cedarschema")
            .to_string_lossy()
            .into_owned();
        assert_eq!(displays, vec![expected.as_str()], "nested schema is found");
        // `path` stays the full joined path so loading still works.
        assert_eq!(found[0].path, deep.join("policies.cedarschema"));
    }

    #[test]
    fn discover_display_is_relative_path() {
        let dir = TempDir::new("relative");
        let sub = dir.subdir("nested");
        std::fs::write(sub.join("s.cedar.json"), "{}").unwrap();

        let found = discover_schema_files(&dir.path).expect("read dir");
        let expected = PathBuf::from("nested")
            .join("s.cedar.json")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            found[0].display, expected,
            "display is the path relative to root"
        );
        assert_ne!(
            found[0].display, "s.cedar.json",
            "not just the bare file name"
        );
    }

    #[test]
    fn discover_prunes_generated_and_dot_directories() {
        let dir = TempDir::new("prune");
        // One legitimate schema at the root so the walk has something to find.
        std::fs::write(dir.child("keep.cedarschema"), "entity User;").unwrap();
        // A schema hidden inside each pruned directory: none must be found.
        for pruned in [".git", "build", "target", "node_modules", "env"] {
            let sub = dir.subdir(pruned);
            std::fs::write(sub.join("hidden.cedarschema"), "entity User;").unwrap();
        }

        let found = discover_schema_files(&dir.path).expect("read dir");
        let displays: Vec<&str> = found.iter().map(|f| f.display.as_str()).collect();
        assert_eq!(
            displays,
            vec!["keep.cedarschema"],
            "pruned dirs contribute nothing"
        );
    }

    #[test]
    fn discover_ignores_unrelated_extensions_recursively() {
        let dir = TempDir::new("exts");
        let sub = dir.subdir("deeper");
        std::fs::write(sub.join("readme.md"), "# doc").unwrap();
        std::fs::write(sub.join("policy.cedar"), "permit(...);").unwrap();
        std::fs::write(sub.join("data.json"), "{}").unwrap();

        let found = discover_schema_files(&dir.path).expect("read dir");
        assert!(found.is_empty(), "no recognized schema files anywhere");
    }

    #[test]
    fn discover_depth_cap_stops_the_walk() {
        let dir = TempDir::new("depth");
        // Build a chain strictly deeper than MAX_DEPTH levels below the root and
        // place a schema at the very bottom. It sits below the cap and so must
        // NOT be found. Root entries are depth 0, so the deepest reachable file
        // lives at MAX_DEPTH; put ours one level beyond that.
        let mut rel = PathBuf::new();
        for i in 0..(MAX_DEPTH + 1) {
            rel.push(format!("d{i}"));
        }
        let too_deep = dir.subdir(rel.to_str().unwrap());
        std::fs::write(too_deep.join("buried.cedarschema"), "entity User;").unwrap();
        // A shallow schema (depth 0) that must still be found.
        std::fs::write(dir.child("top.cedarschema"), "entity User;").unwrap();

        let found = discover_schema_files(&dir.path).expect("read dir");
        let displays: Vec<&str> = found.iter().map(|f| f.display.as_str()).collect();
        assert_eq!(
            displays,
            vec!["top.cedarschema"],
            "the too-deep schema is pruned by the depth cap"
        );
    }

    #[test]
    fn load_schema_succeeds_and_reports_failure() {
        let dir = TempDir::new("load");
        let good = dir.child("good.cedarschema");
        std::fs::write(&good, SCHEMA_SRC).unwrap();
        let good_file = SchemaFile::from_path(good).unwrap();
        match load_schema(&good_file) {
            SchemaLoad::Loaded { envs, .. } => assert!(!envs.is_empty(), "schema yields >=1 env"),
            other => panic!("expected Loaded, got {other:?}"),
        }

        let bad = dir.child("bad.cedarschema");
        std::fs::write(&bad, "this is not a valid schema {{{").unwrap();
        let bad_file = SchemaFile::from_path(bad).unwrap();
        match load_schema(&bad_file) {
            SchemaLoad::LoadFailed { error, .. } => assert!(!error.is_empty()),
            other => panic!("expected LoadFailed, got {other:?}"),
        }
    }

    /// Parse the inline schema and grab its first request env for encode tests.
    fn schema_and_env() -> (Schema, RequestEnv) {
        let schema: Schema = SCHEMA_SRC.parse().expect("inline schema parses");
        let env = schema
            .request_envs()
            .next()
            .expect("at least one request env");
        (schema, env)
    }

    #[tokio::test]
    async fn analyze_returns_parse_errors_for_broken_policy() {
        let (schema, env) = schema_and_env();
        let out = analyze(
            "this is not cedar at all".to_string(),
            schema,
            env,
            AnalysisMode::NeverAllows,
        )
        .await;
        match out {
            AnalysisOutput::ParseErrors(errs) => assert!(!errs.is_empty()),
            other => panic!("expected ParseErrors, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn analyze_returns_validation_errors_for_unknown_entity_type() {
        let (schema, env) = schema_and_env();
        // `Admin` is not declared in the schema -> strict validation fails.
        let src = r#"permit(principal == Admin::"root", action, resource);"#.to_string();
        let out = analyze(src, schema, env, AnalysisMode::NeverAllows).await;
        match out {
            AnalysisOutput::ValidationErrors(errs) => assert!(!errs.is_empty()),
            other => panic!("expected ValidationErrors, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn analyze_encodes_valid_policy_to_smtlib() {
        let (schema, env) = schema_and_env();
        // `principal.age + 1 > 19` is a NON-CONSTANT match condition (it holds
        // for some ages and not others), so the `never_matches` query does not
        // fold away and real SMTLib carrying the policy body is emitted. (A
        // trivial `permit(principal, action, resource);` matches every request
        // in the env, so its match term is constant and symcc short-circuits.)
        let src =
            "permit(principal, action, resource) when { principal.age + 1 > 19 };".to_string();
        let out = analyze(src, schema, env, AnalysisMode::NeverAllows).await;
        match out {
            AnalysisOutput::Encoded { smtlib } => {
                assert!(!smtlib.is_empty(), "smtlib must be non-empty");
                assert!(
                    smtlib.contains("(check-sat)"),
                    "smtlib must contain (check-sat):\n{smtlib}"
                );
                assert!(
                    smtlib.contains("; policy "),
                    "output must carry the per-policy header:\n{smtlib}"
                );
            }
            other => panic!("expected Encoded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn analyze_trivial_policy_yields_explanatory_encoded() {
        let (schema, env) = schema_and_env();
        // A trivial `permit(principal, action, resource);` matches EVERY request
        // in the env, so its match term is constant-true and `never_matches`
        // folds to a constant: symcc short-circuits WITHOUT writing any SMTLib.
        // We must still surface the header plus an explanatory `Encoded` block,
        // never an empty string.
        let out = analyze(
            "permit(principal, action, resource);".to_string(),
            schema,
            env,
            AnalysisMode::NeverAllows,
        )
        .await;
        match out {
            AnalysisOutput::Encoded { smtlib } => {
                assert!(!smtlib.is_empty(), "explanatory text must not be empty");
                assert!(
                    smtlib.contains("; policy "),
                    "output must still carry the per-policy header:\n{smtlib}"
                );
                assert!(
                    smtlib.contains("folds to a constant"),
                    "should explain the constant fold rather than be blank:\n{smtlib}"
                );
            }
            other => panic!("expected Encoded, got {other:?}"),
        }
    }

    /// Truncate `s` to at most `max` chars (on a char boundary), appending `…`
    /// when it was shortened, and flatten newlines so a single error renders on
    /// one table row.
    fn truncate(s: &str, max: usize) -> String {
        let flat = s.replace('\n', " ");
        match flat.char_indices().nth(max) {
            None => flat,
            Some((idx, _)) => format!("{}…", &flat[..idx]),
        }
    }

    /// End-to-end test over the DOWNLOADED example corpus. Drives the REAL
    /// pipeline (`discover_schema_files` → `load_schema` → `analyze`) over the 8
    /// example directories under `examples/`, one `#[tokio::test]` iterating
    /// serially, and prints a per-example results table (run with
    /// `--nocapture` to see it).
    ///
    /// The corpus is located RELATIVE TO THE CRATE via `CARGO_MANIFEST_DIR`
    /// joined with `examples` — never the process CWD and never a hard-coded
    /// absolute path.
    ///
    /// If `examples/` is absent (someone checked out the crate without the
    /// downloaded corpus), the test SKIPs cleanly with an early return so the
    /// build does not break for them.
    #[tokio::test]
    async fn examples_corpus_drives_real_pipeline_end_to_end() {
        let examples_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
        // SKIP cleanly when the corpus is not present, rather than failing.
        if !examples_root.is_dir() {
            eprintln!("examples/ absent at {examples_root:?}; skipping corpus e2e test");
            return;
        }

        let start = std::time::Instant::now();

        // (a) Recursively discover the example schemas via the real function.
        let schema_files = discover_schema_files(&examples_root).expect("discover examples/");
        // HARD: discovery must find all 8 `.cedarschema` files under examples/.
        assert_eq!(
            schema_files.len(),
            8,
            "expected 8 schema files under examples/, found {}: {:?}",
            schema_files.len(),
            schema_files
                .iter()
                .map(|f| f.display.as_str())
                .collect::<Vec<_>>(),
        );

        println!("\n=== examples corpus end-to-end (first request env) ===");
        println!(
            "{:<18} | {:<6} | {:<4} | {:<16} | smtlib bytes / first error (<=100 chars)",
            "example", "loaded", "envs", "outcome"
        );
        println!("{}", "-".repeat(96));

        for file in &schema_files {
            // (b) Load each schema via the real function. HARD: must load.
            let (schema, envs) = match load_schema(file) {
                SchemaLoad::Loaded { schema, envs, .. } => (schema, envs),
                other => panic!("schema {} did not load: {other:?}", file.display),
            };
            // HARD: every loaded schema must yield at least one request env.
            assert!(
                !envs.is_empty(),
                "schema {} yielded zero request envs",
                file.display
            );

            // (c) Read the sibling `policies.cedar` next to the schema.
            let policy_path = file
                .path
                .parent()
                .expect("schema file has a parent dir")
                .join("policies.cedar");
            let policy_src = std::fs::read_to_string(&policy_path)
                .unwrap_or_else(|e| panic!("read {}: {e}", policy_path.display()));

            // Use the FIRST request env from the schema (not one cherry-picked
            // to make results look good).
            let first_env = envs[0].env.clone();
            let out = analyze(
                policy_src,
                (*schema).clone(),
                first_env,
                AnalysisMode::NeverAllows,
            )
            .await;

            // HARD: upstream test data must parse — ParseErrors is a real bug.
            if let AnalysisOutput::ParseErrors(errs) = &out {
                panic!("example {} produced ParseErrors: {:?}", file.display, errs);
            }

            // Human-friendly example name = parent directory name.
            let name = file
                .path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or(file.display.as_str());

            // Validation/Encode outcomes are collected and printed, NOT
            // hard-failed: a policy set legitimately need not type-check against
            // every request env (the first env may target an unrelated action).
            let (outcome, detail) = match &out {
                AnalysisOutput::Encoded { smtlib } => {
                    ("Encoded", format!("{} bytes", smtlib.len()))
                }
                AnalysisOutput::ValidationErrors(errs) => (
                    "ValidationErrors",
                    truncate(errs.first().map(String::as_str).unwrap_or("(none)"), 100),
                ),
                AnalysisOutput::EncodeError(e) => ("EncodeError", truncate(e, 100)),
                AnalysisOutput::Empty => ("Empty", String::new()),
                AnalysisOutput::Running => ("Running", String::new()),
                AnalysisOutput::ParseErrors(_) => unreachable!("hard-asserted above"),
            };
            println!(
                "{name:<18} | {:<6} | {:<4} | {outcome:<16} | {detail}",
                "yes",
                envs.len(),
            );
        }

        println!("{}", "-".repeat(96));
        println!("elapsed: {:?}", start.elapsed());
    }
}
