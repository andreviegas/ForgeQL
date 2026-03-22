# Changelog

All notable changes to ForgeQL will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
ForgeQL uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.27.0] - 2026-03-22

### Fixed

- **`SHOW body` failed for bare member names** — `SHOW body OF 'loadSignalCode'`
  returned "function definition not found" when the symbol was a class member
  declaration (`field_declaration`) rather than the out-of-line
  `function_definition`.  The new `MemberEnricher` now stamps a `body_symbol`
  field on member method declarations during indexing (e.g.
  `body_symbol = "SignalSequencer::loadSignalCode"`), and `show_body` /
  `show_callees` follow the redirect — completely language-agnostic.

- **Class/struct member declarations were not indexed** — tree-sitter C++ uses
  `field_declaration` for members inside class bodies, but the indexer only
  handled `declaration` nodes.  Added a `("cpp", "field_declaration")` arm to
  `extract_name()` and `"field_identifier"` to `find_function_name()` so that
  member function prototypes and data members are now visible in the symbol
  index.

### Added

- **`MemberEnricher`** — new enrichment pass
  (`crates/forgeql-core/src/ast/enrich/member.rs`) that populates `body_symbol`
  on `field_declaration` nodes containing a `function_declarator`, linking the
  member declaration to its qualified definition.

- **`body_symbol` enrichment field** — queryable via
  `FIND symbols WHERE body_symbol = 'Class::method'`.

## [0.26.0] - 2026-03-21

### Fixed

- **`IN` / `EXCLUDE` glob matched too broadly** — `IN 'kernel/**'` also
  matched files under `tests/kernel/` because glob patterns floated across
  all path segments.  Now patterns without a leading `**` are anchored at
  the start of the relative path (worktree root is stripped before matching).
  Use `**/kernel/**` for the old floating behaviour.

- **Stack overflow on large codebases** — `collect_nodes` (the AST indexer
  invoked by `USE source.branch`) used recursive depth-first traversal,
  causing a stack overflow on deeply nested files in large projects like
  Zephyr RTOS.  Converted to iterative traversal using `TreeCursor`
  navigation (`goto_first_child` / `goto_next_sibling` / `goto_parent`).

- **Condition skeleton letter overflow** — `skeleton_walk` had only 26 slots
  (a-z) for unique leaf terms; after exhaustion every new term collapsed to
  `z`, producing unreadable noise.  Extended to 52 slots (a-z, A-Z) with `$`
  for any remaining overflow, plus truncation at 120 chars with `…` suffix.

- **Condition skeleton dropped operators** — the catch-all branch in
  `skeleton_walk` only visited named AST children, silently skipping unnamed
  operator tokens (`|`, `&`, `=`, `?`, `:`, etc.).  Conditions like
  `a | b & c` rendered as `abc` with no operators.  Now visits all children
  so bitwise, ternary, and assignment operators are preserved.

- **Quadratic post-pass enrichment** — `ControlFlowEnricher::post_pass()`
  and `RedundancyEnricher::post_pass()` scanned all rows for every function
  definition (O(N×F)), making indexing collapse to a single core for minutes
  on large codebases.  Replaced with a file-grouped binary-search approach
  (O(N log F)) that runs in milliseconds.

### Changed

- **Parallel file indexing** — `SymbolTable::build()` now uses `rayon` to
  parse and enrich files across all CPU cores.  Each thread creates its own
  `Parser` and enricher set, producing a per-file `SymbolTable` that is
  merged via tree-reduction so merges also run in parallel.

- **Zero-copy cache persistence** — `CachedIndex::from_table()` now takes
  ownership of the `SymbolTable` instead of cloning all rows and usages,
  eliminating a full copy of the index (millions of rows) before
  serialization.

- **Query log `elapsed_ms` column** — every CSV log row now includes the
  wall-clock milliseconds the command took to execute, making performance
  analysis on large codebases straightforward.  `CREATE SOURCE` commands are
  now logged with the correct source name (previously went to `unknown.csv`).

- **FIND symbols pre-filtering** — `FIND symbols` now applies WHERE
  predicates directly on `IndexRow` before materializing `SymbolMatch`,
  using the `kind_index` for O(1) row selection when `node_kind = 'value'`
  is present.  On large codebases this avoids cloning millions of rows that
  would be discarded by filters, reducing query time from seconds to
  milliseconds.

- **Early LIMIT short-circuit** — when a `FIND symbols` query has `LIMIT`
  but no `ORDER BY` or `GROUP BY`, materialization stops as soon as enough
  rows are collected, avoiding a full scan of millions of candidates.

- **Comment name compaction** — multi-line comment names (e.g. copyright
  blocks) are now displayed as `len:N` in both the compact CSV and pipe
  `Display` formats, preventing huge comment text from flooding output.
  Single-line names longer than 120 chars are truncated with `…`.

- **Enrichment-to-kind inference** — `FIND symbols` queries that filter on
  enrichment fields (e.g. `WHERE cast_style = 'c_style'`) now automatically
  infer the target `node_kind`(s) and use the `kind_index` for fast lookup,
  even without an explicit `node_kind =` predicate.  This turns queries that
  previously scanned all rows into sub-second lookups.

- **`dup_logic` enrichment field** — control-flow rows (`if_statement`,
  `while_statement`, `for_statement`, `do_statement`) now include a
  `dup_logic` field set to `"true"` when the condition contains duplicate
  sub-expressions in `&&` / `||` chains (e.g. `a & FLAG || a & FLAG`).
  Catches copy-paste bugs where an operand was duplicated instead of changed.

- **Skeleton `pointer_expression` fix** — `skeleton_walk` now treats
  `pointer_expression` (`*ptr`) as a distinct leaf instead of dropping the
  dereference operator.  This means `ptr != NULL && *ptr != 0` correctly
  produces `a!=b&&c!=d` (two distinct terms) instead of `a!=b&&a!=b`.

- **Skeleton arithmetic operators preserved** — added `+`, `-`, `*`, `/`,
  `%`, `<<`, `>>` to the operator set kept in condition skeletons.  Without
  this, `x - 1` and `x + 1` both collapsed to `ab`, causing false
  `dup_logic` positives on expressions like `(match-1) == ticks || (match+1) == ticks`.

- **Skeleton opaque catch-all for unknown AST nodes** — `skeleton_walk` now
  maps any unrecognised named node as a single opaque leaf instead of
  recursing into its children.  This prevents the C++ `operator` keyword
  from being silently dropped in member-access expressions like
  `bt_hf->operator`, which was causing a `dup_logic` false positive on
  `bt_hf && bt_hf->operator`.  Transparent wrapper nodes (`condition_clause`,
  `cast_expression`, `comma_expression`) are still recursed through.

---

## [0.25.0] - 2026-03-21

### Added

- **SHOW output guardrail** — SHOW commands that return source lines (body,
  lines, context) are now capped at 40 lines when no explicit `LIMIT` is
  provided.  Exceeding the cap returns **zero lines** plus a guidance hint
  directing the agent to use `FIND symbols WHERE` → `SHOW LINES n-m` instead
  of brute-force pagination.  When the agent consciously adds `LIMIT N`, the
  value is honored.

- **AI agent integration package** (`doc/agents/`) — distributable Custom
  Agent definitions that lock AI tools to ForgeQL MCP and prevent drift to
  local grep/find/cat:
  - `forgeql.agent.md` — VS Code Copilot Custom Agent with `tools: [forgeql/*]`
  - `AGENTS.md` — platform-agnostic workspace instructions
  - `claude-code/CLAUDE.md` — Claude Code adapter
  - `cursor/.cursorrules` — Cursor adapter
  - `references/query-strategy.md` — decision tree and anti-patterns
  - `references/recipes.md` — 8 workflow templates
  - `references/syntax-quick-ref.md` — condensed command/field reference with
    verified Known Limitations table
  - `README.md` — installation guide for all platforms

- **Expanded MCP `with_instructions()`** — the instruction text injected into
  the agent system prompt during the MCP `initialize` handshake now includes
  three structured sections (Critical Rules, Query Strategy, Efficiency) with
  inlined default constants (`DEFAULT_QUERY_LIMIT=20`,
  `DEFAULT_BODY_DEPTH=0`, `DEFAULT_CONTEXT_LINES=5`,
  `DEFAULT_SHOW_LINE_LIMIT=40`).

### Changed

- **`ShowResult` extended** — `total_lines: Option<usize>` and
  `hint: Option<String>` fields added.  Compact CSV renderer appends
  `truncated` and `hint` rows when present.

### Removed

- **`doc/FORGEQL_AGENT_GUIDE.md`** — superseded by the `doc/agents/` package.
  All unique content (Known Limitations table) migrated to
  `doc/agents/references/syntax-quick-ref.md`.

---

## [0.24.0] - 2026-03-20

### Added

- **metric_hint in compact output** — FIND symbols queries that filter or sort
  by an enrichment metric (e.g. `WHERE member_count > 10`,
  `ORDER BY lines DESC`) now display that metric as the last column in compact
  CSV instead of the default `usages`.  The schema row reflects the active
  metric: `[name,path,line,member_count]`.

### Fixed

- **member_count over-counting nested members** — `member_count` walked the
  entire AST subtree recursively, which double-counted members of nested
  structs/classes.  Now counts only direct children of the
  `field_declaration_list` (fields, methods, declarations) plus those inside
  `access_specifier` sections.

---

## [0.23.1] - 2026-03-20

### Fixed

- **WHERE clauses on SHOW outline / SHOW members** — WHERE predicates were
  silently ignored; only LIMIT/OFFSET were applied.  Now the full clause
  pipeline (WHERE, ORDER BY, LIMIT, OFFSET) runs on outline and member
  entries via `ClauseTarget` implementations for `OutlineEntry` and
  `MemberEntry`.

---

## [0.23.0] - 2026-03-20

### Added

- **Compact output module** (`compact.rs`) — token-efficient CSV format that
  deduplicates repeated fields by grouping rows that share a key.  Now the
  default for MCP `run_fql` (CSV mode).

  - FIND symbols: grouped by `node_kind` — kind appears once per group.
  - FIND usages: grouped by file — line numbers collapsed per file.
  - SHOW outline: grouped by kind, comments compressed to `len:N`.
  - SHOW members: grouped by kind.
  - SHOW callees/callers: grouped by file.
  - SHOW body/lines/context: 2-column `line,text` with line range spans.
  - SHOW signature: single flat row.
  - FIND files: 2-column `path,size` (dropped `depth`, `extension`).
  - Mutations, transactions, source ops: fall back to JSON (already small).

- **CLI `--format` flag** — `text` (default), `compact`, or `json`.
  Available globally across REPL, pipe, and one-shot modes.

- **`tokens_approx` for compact output** — appended as a final CSV row
  (`"tokens_approx",N`) when output is compact; spliced into JSON when
  output is JSON.

### Changed

- MCP `run_fql` default output changed from JSON-wrapped flat arrays to
  compact grouped CSV.  Pass `format=JSON` to get full structured JSON.

---

## [0.22.0] - 2026-03-20

### Added

- **Enrichment pipeline** — 9 trait-based `NodeEnricher` implementations that
  compute ~30 new metadata fields at index time, queryable with `WHERE` just
  like dynamic fields.  Enrichers run in a single pass over the AST plus a
  post-pass for cross-row aggregations (e.g. `branch_count`,
  `duplicate_condition`).

  | Enricher | Key fields |
  |---|---|
  | **ScopeEnricher** | `scope` (`file`/`local`), `storage` (`static`/`extern`) |
  | **NamingEnricher** | `naming` (camelCase, PascalCase, snake_case, UPPER_SNAKE, flatcase), `name_length` |
  | **CommentEnricher** | `comment_style` (doc_line, doc_block, block, line), `has_doc` |
  | **NumberEnricher** | `num_format`, `is_magic`, `num_value`, `num_suffix`, `has_separator` |
  | **ControlFlowEnricher** | `condition_tests`, `paren_depth`, `has_default`, `has_assignment_in_condition`, `mixed_logic`, `branch_count` |
  | **OperatorEnricher** | `increment_style`, `compound_op`, `shift_direction`, `shift_amount` |
  | **MetricsEnricher** | `lines`, `param_count`, `return_count`, `goto_count`, `string_count`, `member_count`, `is_const`, `is_static`, `is_inline` |
  | **CastEnricher** | `cast_style`, `cast_target_type` |
  | **RedundancyEnricher** | `has_repeated_condition_calls`, `repeated_condition_calls`, `null_check_count`, `duplicate_condition` |

- **`field_num()` fallback** — `SymbolMatch` and `IndexRow` now parse dynamic
  string fields as integers on the fly, so `ORDER BY lines DESC` works on
  enrichment fields without dedicated numeric columns.

- **Enrichment integration tests** — 104 new tests in
  `enrichment_integration.rs` covering all 9 enrichers, cross-enricher
  queries, and `field_num()` fallback.

- **`doc/syntax.md` updated** — full Enrichment Fields reference with per-
  enricher tables, example queries, and 7 Known Limitations entries.

---

## [0.21.0] - 2026-03-19

### Added

- **`QueryLogger` moved to `forgeql-core`** — the query logger is now a public
  module (`forgeql_core::query_logger`) in the core library, making it
  available for integration testing and downstream consumers. Zero new
  dependencies; the CLI binary now re-exports from core.

- **Comprehensive syntax-coverage test suite** — 156 new integration tests in
  `syntax_coverage.rs` covering every ForgeQL command, clause, and operator
  combination documented in `doc/syntax.md`:
  - FIND symbols with every WHERE operator (`=`, `!=`, `LIKE`, `NOT LIKE`,
    `>`, `>=`, `<`, `<=`), dynamic fields, ORDER BY, LIMIT, OFFSET, IN,
    EXCLUDE, GROUP BY, and multi-WHERE combinations.
  - FIND usages, callees, files, and globals with all clause variants.
  - SHOW body (depth 0/1/99), signature, outline, members, context, callees,
    and LINES ranges.
  - CHANGE + ROLLBACK round-trips: MATCHING, LINES WITH, WITH content,
    LINES NOTHING, WITH NOTHING, and multi-file glob.
  - Transaction lifecycle: BEGIN/ROLLBACK named and anonymous, nested
    transactions.
  - Error cases: malformed FQL, missing sessions, nonexistent
    symbols/files/checkpoints.
  - Parser-only coverage for every clause combination and command variant.
  - QueryLogger integration: CSV creation, multi-row append, source-name
    sanitization.
  - Display and serialization: `to_json` roundtrip, `to_csv`, `Display`.

  Total workspace tests: **427** (was 271).

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


