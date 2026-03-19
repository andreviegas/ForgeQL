# Changelog

All notable changes to ForgeQL will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
ForgeQL uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

---

## [0.20.0] - 2026-03-19

### Changed

- **Transactions redesigned as checkpoint-based model** (breaking change).
  `BEGIN TRANSACTION 'name'` is now a **standalone statement** that creates a
  named git checkpoint (records the current HEAD OID after auto-committing any
  dirty working-tree state).  `COMMIT MESSAGE 'msg'` is now a **standalone
  statement** that stages all changes and creates a git commit.  `ROLLBACK
  [TRANSACTION 'name']` reverts to a named checkpoint via `git reset --hard`.
  Each command executes independently and returns its own result, giving AI
  agents full per-step visibility and decision-making control.

  **Before (0.19.x):** `BEGIN TRANSACTION ... COMMIT` was a single compound
  grammar block.  All inner operations were planned and applied atomically.
  VERIFY auto-rolled back on failure.

  **After (0.20.0):** Each statement is sent individually.  The AI sees every
  result and decides whether to proceed, verify, commit, or rollback.

  ```sql
  BEGIN TRANSACTION 'rename-api'
  CHANGE FILES 'src/**/*.cpp' MATCHING 'oldName' WITH 'newName'
  VERIFY build 'test'
  COMMIT MESSAGE 'rename oldName to newName'
  ```

- **`ROLLBACK` now uses `git reset --hard`** instead of restoring in-memory
  file snapshots.  Session checkpoints are stored as `(label, git_oid)` pairs
  on a stack.  `ROLLBACK TRANSACTION 'name'` also removes all checkpoints
  created after the named one.

---

## [0.19.7] - 2026-03-19

### Fixed

- **`VERIFY` via MCP now requires `session_id`** — previously, calling
  `VERIFY build '<step>'` through the MCP `run_fql` tool without a
  `session_id` silently fell back to a filesystem search rooted at the
  engine's data directory, which never found `.forgeql.yaml` and always
  returned *"step not found"*.
  `VERIFY` now calls `require_session_id` exactly like `FIND`, `SHOW`, and
  mutations do — a missing `session_id` produces a clear error:
  *"session_id required — run USE <source>.<branch> first"*.
  Pass the `session_id` returned by `use_source` (or `USE` via `run_fql`).

- **Multi-statement `run_fql` now executes all operations, not just the first**.
  When an agent sends multiple FQL statements in a single `run_fql` call
  (separated by `\n` or real newlines), all of them are now executed in
  sequence.  Previously only the first was executed and the rest were silently
  dropped.

- **Query log gets one row per statement** (both MCP and CLI).  The log
  previously wrote one row for the entire input string, which was truncated
  at 80 chars and mixed all statements together, making `source_lines` and
  token counts meaningless for multi-statement inputs.  Each executed
  operation now produces its own log row with a compact label derived from
  the parsed IR (e.g. `FIND symbols`, `SHOW body OF 'Foo::bar'`,
  `CHANGE FILE 'src/f.cpp' LINES 10-20`).

---

## [0.19.6] - 2026-03-19

### Changed

- **`source_lines` replaces `lines_returned` in the query log**: the CSV log
  column now counts the number of raw **source-code lines** actually returned
  by each operation, not the number of result rows.
  - `SHOW LINES 61-130` → `70`
  - `SHOW body` / `SHOW context` → number of lines in the rendered body
  - `FIND symbols`, `FIND usages`, mutations, source ops → `0` (no source code
    was disclosed)

  This is tracked to measure how much of a codebase the AI agent has
  inspected during a session.

### Fixed

- **`SHOW LINES` line count in the query log**: `SHOW LINES` results were
  always logged as `source_lines=1` because the previous approach parsed the
  serialised JSON output and did not recognise the `"lines"` array key.
  Replaced the JSON-parsing `count_result_rows` function entirely with
  `ForgeQLResult::source_lines_count()`, which works directly on the typed
  result and handles all current and future result variants correctly.

- **CSV `count` column header is now `line` for `FIND usages`**: when
  `FIND usages OF 'symbol'` is used without `GROUP BY`, each result row is
  one call site and the 4th CSV column contains the line number (not a count).
  The header now says `"line"` instead of `"count"` for this operation so
  callers are not confused.  All other operations (`FIND symbols`,
  `COUNT … GROUP BY`, etc.) continue to use `"count"`.

---

## [0.19.5] - 2026-03-19

### Fixed

- **`REFRESH SOURCE` now visible to open sessions**: after `REFRESH SOURCE`,
  the next `USE source.branch` call detects that the bare repo's branch HEAD
  has moved past the session's indexed commit and automatically evicts the
  stale in-memory session.  A fresh session is then created from the updated
  HEAD, triggering a re-index.  Previously, the stale in-memory session was
  returned unconditionally even when new commits had been fetched.

- **`fetch_all` uses an explicit refspec**: `REFRESH SOURCE` now passes
  `+refs/heads/*:refs/heads/*` to the remote fetch instead of an empty
  refspec.  An empty refspec relied on the bare repo's configured remote
  mapping, which in some libgit2 bare-clone setups maps to
  `refs/remotes/origin/*` rather than `refs/heads/*`.  With the explicit
  refspec, local branch refs are always updated and `worktree::create` can
  reliably find the new commits via `find_branch(Local)`.

---

## [0.19.4] - 2026-03-19

### Security

- **`CHANGE` commands cannot modify `.forgeql.yaml`**: the mutation planner
  now rejects any file target whose filename is `.forgeql.yaml` before any
  I/O is performed.  This closes a command-injection vector where an AI agent
  could use a `CHANGE` command to overwrite the config file and then trigger
  `VERIFY build` to execute the tampered shell command.

- **`verify_steps` are frozen at session start**: when `USE source.branch` is
  executed, the engine reads `.forgeql.yaml` once and stores the `verify_steps`
  in the session.  Both `VERIFY build` (standalone) and `VERIFY build` inside
  a transaction now use these frozen steps instead of re-reading the file from
  disk.  Changes to `.forgeql.yaml` after a session is opened have no effect
  on which commands `VERIFY` will execute—mirroring how CI systems work.

### Fixed

- **`CHANGE FILES` now expands glob patterns**: the `file_list` entries
  (e.g. `'src/**/*.cpp'`) were treated as literal paths instead of being
  expanded against the workspace.  Globs are now resolved using the same
  matching engine as `IN` / `EXCLUDE` clauses.  A glob that matches no files
  returns an error.

- **`MATCHING` is tolerant of glob-expanded files missing the pattern**:
  when `CHANGE FILES` uses glob patterns, files that do not contain the
  `MATCHING` text are silently skipped instead of aborting the whole
  transaction.  An error is still raised when *no* glob-matched file
  contains the pattern, or when a literal (non-glob) path is missing it.

---

## [0.19.3] - 2026-03-18

### Fixed

- **C/C++ variable declarations are now indexed**: the tree-sitter
  `declaration` node kind (e.g. `int x = 5;`, `static Foo bar;`) is now
  processed by the indexer via a language-specific extraction rule in
  `extract_name`.  Previously these nodes were silently skipped because they
  lack a direct `name` grammar field.  `FIND symbols WHERE node_kind =
  'declaration'` now returns results.

- **`FIND globals` now works**: the parser predicate was changed from
  `kind = 'Variable'` (a non-existent node kind that always matched nothing)
  to `node_kind = 'declaration'` with `scope = 'file'`.  `FIND globals` is
  now a convenience alias for
  `FIND symbols WHERE node_kind = 'declaration' WHERE scope = 'file'`,
  returning only file-scope variable declarations.

- **`VERIFY build` now runs in the correct directory**: `run_standalone` and
  `run_step` were executing the shell command without setting a working
  directory, so relative paths like `./scripts/Build.sh` failed with
  "not found".  Both functions now receive the workspace root (derived from
  the `.forgeql.yaml` location) and pass it via `.current_dir()`.

### Added

- **`scope` and `storage` dynamic fields** for C/C++ `declaration` nodes:
  - `scope`: `"file"` when the declaration's parent is the translation unit,
    `"local"` when inside a function body.
  - `storage`: the storage class specifier text (`"static"`, `"extern"`) when
    present; absent for default linkage.
  - Use `WHERE storage != 'static'` to exclude internal-linkage variables, or
    `WHERE scope = 'local'` to find only local variable declarations.

- **Function forward declaration filtering**: `declaration` nodes whose
  declarator tree contains a `function_declarator` (e.g. `void foo(int);`)
  are now skipped during indexing so they don't pollute variable results.

- **`declaration` in the `node_kind` table** (syntax.md): documented alongside
  the other common C/C++ node kinds.

- **Integration tests**: `find_globals_returns_declarations`,
  `find_symbols_where_node_kind_declaration`, and
  `find_symbols_group_by_node_kind` verify the new indexing end-to-end.

### Changed

- **Known Limitations**: the "Scope filtering" note now reflects that `scope`
  and `storage` dynamic fields are available for filtering.

---

## [0.19.2] - 2026-03-17

### Fixed

- **`FIND files` now honours all universal clauses**: `WHERE`, `ORDER BY`,
  `LIMIT`, and `OFFSET` were silently ignored on `FIND files` results because
  `apply_clauses()` was never called.  The engine now builds typed `FileEntry`
  values, runs the full clause pipeline, and only then performs depth-grouping.

### Added

- **`extension` and `size` fields on `FileEntry`**: `FIND files` results now
  expose `extension` (string, without the leading `.`) and `size` (bytes,
  integer) as filterable, sortable fields — e.g.
  `FIND files WHERE extension NOT LIKE 'cpp' WHERE extension NOT LIKE 'h'`.

---

## [0.19.1] - 2026-03-17

### Fixed

- **`--data-dir` tilde expansion**: paths like `~/forgeql-data` passed with
  single quotes (e.g. in MCP host configs or scripts) were not expanded by the
  shell. ForgeQL now resolves `~` internally via the `dirs` crate, which handles
  `$HOME` on Linux/macOS and `USERPROFILE`/`FOLDERID_Profile` on Windows.
- **Lexical `..` normalization**: `--data-dir '~/../../some/path'` and similar
  traversals are now collapsed to a clean absolute path before the engine starts,
  making logs and error messages unambiguous.

### Added

- **`path_utils` module** (`crates/forgeql/src/path_utils.rs`): new internal
  module with `resolve_data_dir`, `expand_tilde`, and `normalize_lexically`
  helpers, covered by 5 unit tests.

---

## [0.19.0] - 2026-03-17

### Added

- **Standalone `VERIFY build 'step'`**: `VERIFY build` is now a top-level
  statement (not just a `BEGIN TRANSACTION … COMMIT` clause).  Run any verify
  step defined in `.forgeql.yaml` on demand — outside a transaction — to check
  the current state of the worktree.

- **`VerifyBuildResult`**: new result type exposed in the MCP / programmatic API
  with `step`, `success`, and `output` fields.

---

## [0.18.0] - 2026-03-17

Initial public release.

### Highlights

- **17-command surface**: `FIND symbols` / `FIND usages OF` / `FIND callees OF` /
  `FIND files` / 6 `SHOW` commands / `CHANGE` with `MATCHING`, `LINES`, `WITH`,
  `WITH NOTHING` / session management / `BEGIN TRANSACTION … COMMIT`

- **Universal clause system**: `WHERE`, `HAVING`, `IN`, `EXCLUDE`, `ORDER BY`,
  `GROUP BY`, `LIMIT`, `OFFSET`, `DEPTH` — works identically on every command

- **Flat index model**: every tree-sitter AST node is an `IndexRow` with dynamic
  `fields` extracted from the grammar — no hardcoded type hierarchies

- **MCP server mode**: connects to AI agents (GitHub Copilot, Claude, etc.) via
  the Model Context Protocol over stdio

- **Interpreter mode**: pipe any FQL statement to the binary for scripting and
  quick lookups

- **C/C++ support**: tree-sitter grammars for `.c`, `.h`, `.cpp`, `.hpp`, `.cc`,
  `.cxx`, `.ino` files

- **257 tests**, zero `clippy::pedantic` warnings

---


