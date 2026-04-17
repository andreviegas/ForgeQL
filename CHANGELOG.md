# Changelog

All notable changes to ForgeQL will be documented in this file.

ForgeQL uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

---
---

## [Unreleased]

### Bug Fixes

- **Bug 1.1**: `FIND files` without `IN` clause now defaults depth correctly instead of returning 0 results.
- **Bug 1.3 / Imp 2.7**: `ORDER BY` now accepts known enrichment fields (e.g. `lines`, `param_count`, `is_recursive`, `has_cast`, `cast_count`, `is_exported`, `cast_safety`).
- **Bug 1.4**: `is_exported` now correctly detects Rust `pub fn` functions via `visibility_modifier` AST node in `ScopeEnricher`.
- **Bug 1.5**: Cast enrichment exposed at function level via `CastEnricher::enrich_row` — adds `has_cast` and `cast_count` fields.
- **Bug 1.6 / Imp 2.2**: Naming convention (`has_`/`is_`/`_count`) documented in `doc/syntax.md`.

### Improved

- **Imp 2.1**: `USE` parse errors now include a hint suggesting `USE source.branch AS alias` format.
- **Imp 2.3**: `FIND globals` changed from `node_kind="declaration"` to `fql_kind="variable"` for language-agnostic behavior.
- **Imp 2.5**: `GROUP BY` on custom enrichment fields now renders field-value groups in compact output. Added `group_by_field` to `QueryResult`.
- **Imp 2.6**: Stale worktree validation — `CachedIndex` stores and validates `source_name` on resume.
### Changed

- **ForgeQL agent local filesystem access** — `forgeql.agent.md` now includes
  `read`, `edit`, and `search` tools alongside ForgeQL MCP tools, enabling
  local filesystem access for non-source tasks (writing `HINTS.md`, reading
  workspace configuration, creating output files). Source code access remains
  ForgeQL-exclusive.

## [0.36.0] — 2026-04-16

### Changed

- **Alias is now the session key** — `USE source.branch AS 'alias'` now uses
  the alias directly as the `session_id` instead of generating an opaque
  time-based token. The `session_id` returned by `USE` always equals the alias
  the caller supplied, making it trivially reconstructable without persisting
  any external state. LLM clients that forget to forward the session_id can
  recover by re-issuing the original `USE` command or simply by passing the
  alias they already chose.
- **Session resume is O(1)** — the internal session lookup on reconnect changed
  from an O(n) linear scan to a direct hash-map lookup keyed by alias.
- **MCP tool description updated** — `run_fql` description and `with_instructions`
  now explicitly state that the alias from `AS '...'` equals the `session_id`.
- **`generate_session_id()`** is now test-only; production sessions no longer
  generate opaque time-based IDs.
- **Auto-reconnect after server restart** — when a client passes a `session_id`
  that is no longer in memory but whose worktree still exists on disk, the
  engine transparently re-creates the session by deriving `source_name` and
  `branch` from the worktree directory name and git metadata.  No `.forgeql-meta`
  sidecar file is needed; the existing filesystem layout is sufficient.

### Added — Guard Enrichment: Phases 1–5 (cache v22)

- **Guard enrichment fields** — every symbol inside a C/C++ `#ifdef`/`#if`/`#elif`/`#else` block is now tagged with seven guard fields injected by `collect_nodes()`:
  - `guard` — raw guard condition text (e.g. `"defined(CONFIG_SMP)"`, `"!X"`, `"Y && X"`)
  - `guard_defines` — comma-separated symbols that must be defined for this branch
  - `guard_negates` — comma-separated symbols that must be undefined for this branch
  - `guard_mentions` — all symbols mentioned in the condition (superset of defines + negates)
  - `guard_group_id` — unique u64 identifying the `#ifdef`/`#if` block; all arms share the same ID
  - `guard_branch` — ordinal within the group: `0` = if, `1` = first elif/else, `2` = second, …
  - `guard_kind` — `"preprocessor"` | `"attribute"` | `"heuristic"`

- **Rust `#[cfg(...)]` attribute guards (Phase 2)** — `guard_kind = "attribute"` for `#[cfg(test)]`, `#[cfg(feature = "...")]`, etc. Extracts condition, defines, and mentions from Rust attribute syntax.

- **Python heuristic guards (Phase 3)** — `guard_kind = "heuristic"` for `TYPE_CHECKING`, `sys.platform`, and similar runtime platform-conditional patterns. Infrastructure via `env_guard_patterns` + `build_env_guard_frame`.

- **Guard-aware ShadowEnricher (Task 1.3)** — `walk_scopes_iterative` maintains a mini guard stack; declarations in opposite `#ifdef`/`#else` arms (same `guard_group_id`, different `guard_branch`) no longer produce false-positive shadow reports. Scope maps changed from `BTreeSet<String>` to `HashMap<String, Option<GuardInfo>>`.

- **Guard-aware DeclDistanceEnricher (Task 1.4)** — dead-store detection uses structural `guard_group_id`/`guard_branch` exclusivity checks. Writes in exclusive `#ifdef`/`#else` branches no longer trigger `has_unused_reassign = "true"`.

- **`LanguageConfig` guards section** — `block_guard_kinds`, `elif_kinds`, `else_kinds`, `condition_field`, `name_field`, `negate_ifdef_variant` with accessor methods `has_guard_support()`, `is_block_guard_kind()`, `is_elif_kind()`, `is_else_kind()`, `guard_condition_field()`, `guard_name_field()`, `negate_ifdef_variant()`.

- **`guard_utils.rs`** — `GuardFrame`, `GuardInfo`, `NEXT_GUARD_GROUP_ID`, `inject_guard_fields()`, `guard_info_from_fields()`, `guard_info_from_stack()`, `build_guard_frame()`, `decompose_condition()`, `parse_condition_text()`, `static_guard_kind()`, `are_guards_exclusive()`.

- **`EnrichContext` guard stack** — now carries `guard_stack: &[GuardFrame]` for use by enrichers.

### Added — Macro Expansion Pipeline (Phase 4–5)

- **MacroExpandEnricher (Phase 4, Task 4.4)** — enriches `macro_call` rows with `macro_def_file`, `macro_def_line`, `macro_arity`, `macro_expansion` fields. Graceful failure reporting via `expansion_failed` and `expansion_failure_reason`.

- **C++ MacroExpander (Phase 4)** — shared macro infrastructure (`MacroDef`, `MacroTable`, `MacroExpander`, `resolve_macro`), two-pass macro collection pipeline, `CachedIndex` macro persistence.

- **C++ `call_expression` re-tagging (Task 4.2)** — `collect_nodes()` re-tags `call_expression` → `macro_call` via `MacroTable` lookup when `extract_name` returns `None`.

- **DeclDistanceEnricher macro expansion (Task 4.4)** — scans expanded text for local variable reads using `contains_word()` to suppress false dead-store positives.

- **EscapeEnricher macro expansion (Task 4.5)** — detects `&local` patterns in expanded macro text as address-of escapes (tier 2).

- **Extended MacroExpandEnricher (Task 4.7)** — `expanded_reads`, `expanded_has_escape`, `expansion_depth` fields for successful expansions.

- **RustMacroExpander (Phase 5)** — `macro_rules!` extraction and expansion for Rust: `extract_def()`, `extract_args()`, `substitute()`, `wrap_for_reparse()`.

### Changed

- **`cpp.json`** — `guards` block added; `preproc_else` and `preproc_elif` removed from `skip_node_kinds` so all guard branches are now traversed and indexed.
- **`rust.json`** — added `"macros"` section and `"macro_invocation": "macro_call"` to `kind_map`.
- **`RustLanguage::extract_name()`** — handles `macro_invocation` via `child_by_field_name("macro")`.
- **Cache version** bumped through v17 → v18 → v19 → v20 → v21 → v22 across all phases.

### Fixed

- **Negation operator NULL semantics** — `!=`, `NOT LIKE`, and `NOT MATCHES` now return `false` when the field does not exist on a row, matching documented NULL semantics. Previously `is_none_or()` returned `true` for missing fields, causing false positives.
- **`RustLanguageInline.extract_name`** — synced with production `RustLanguage`: added `"macro_invocation"` arm and `"scoped_identifier"` early return guard.
- **`CppLanguageInline.extract_name`** — synced with production `CppLanguage`: added `"macro_invocation"` arm.
- **C++ `macro_invocation` nodes** now indexed as `macro_call` rows.

### Tests

- `rust_macro_invocation_indexed_as_macro_call`
- `rust_cfg_attribute_ast_structure`
- `rust_cfg_attribute_guard_indexed`
- `cpp_config_is_consistent` updated for guard traversal
- `query_methods_kind_membership` updated: `preproc_else` is no longer a skip kind

---

## [0.34.0] — 2026-04-12

### Added

- **Qualified name resolution** (`SHOW body OF 'CachedIndex::save'`):
  - New `enclosing_type` enrichment field on function nodes inside owner
    containers (impl blocks, classes, traits).
  - `resolve_symbol()` now splits qualified names on `::` (Rust/C++) or
    `.` (Python) and filters by `enclosing_type`.
  - Falls through to `body_symbol` redirect for C++ out-of-line definitions.
  - Language-agnostic: driven by `owner_container_kinds` in JSON config +
    `LanguageSupport::extract_name()`.

- **IN auto-glob bare paths** — `IN 'src'` and `IN 'crates/'` now
  automatically expand to `IN 'src/**'` and `IN 'crates/**'`.
  Implemented via `normalize_glob()` in `query.rs`, benefiting all callers
  of `glob_matches()` and `relative_glob_matches()`.

- **SHOW LINES n-m bypasses implicit 40-line cap** — explicit line ranges
  are user-specified and should not be blocked by the implicit
  `DEFAULT_SHOW_LINE_LIMIT`. Only `SHOW body` and `SHOW context`
  (unbounded output) remain subject to the cap.

- **Actionable error messages** — symbol-not-found errors now suggest
  similar names from the index (`suggest_similar()`) and provide
  `FIND symbols WHERE name LIKE` guidance.  Filter-eliminated errors
  report which clauses (IN, EXCLUDE, WHERE) removed candidates.

- **DEPTH 0 enrichment metadata** — `SHOW body OF 'func' DEPTH 0`
  now includes a `metadata` row in compact output with selected
  enrichment fields (lines, param_count, branch_count, is_recursive,
  etc.) so the agent can make informed decisions without a separate
  FIND query.

- **FIND files recursive default with IN** — when `IN` is specified
  without an explicit `DEPTH`, defaults to full depth instead of 0,
  showing individual files rather than collapsed directories.

### Changed files

- `crates/forgeql-core/src/ast/query.rs` — `normalize_glob()` auto-appends `/**` to bare paths
- `crates/forgeql-core/src/ast/index.rs` — `suggest_similar()` for fuzzy name suggestions
- `crates/forgeql-core/src/ast/show.rs` — metadata extraction on DEPTH 0
- `crates/forgeql-core/src/engine.rs` — `apply_show_lines_cap()` bypass for explicit ranges, actionable errors in `resolve_symbol()`, recursive depth default for FIND files
- `crates/forgeql-core/src/result.rs` — `metadata` field on `ShowResult`
- `crates/forgeql-core/src/compact.rs` — metadata rendering in compact output
- `crates/forgeql-lang-rust/config/rust.json` — added `owner_container_kinds`
- `crates/forgeql-lang-cpp/config/cpp.json` — added `owner_container_kinds`
- `crates/forgeql-lang-python/config/python.json` — added `owner_container_kinds`
- `crates/forgeql-core/src/ast/lang_json.rs` — `owner_container_kinds` in `DefinitionsSection`
- `crates/forgeql-core/src/ast/lang.rs` — `owner_container_raw_kinds` field + accessor
- `crates/forgeql-core/src/ast/enrich/member.rs` — `enclosing_type` enrichment + `enclosing_owner_name()`

---

## [0.33.0] — 2026-04-09

### Added

- **Proportional mutation recovery** — mutations now earn budget back at a 1:1
  ratio for every source line written, bypassing the rolling-window halving.
  `CHANGE`, `COPY`, and `MOVE` all report `lines_written` in the response and
  grant that exact amount as budget recovery (capped at ceiling).  Deletions
  (`LINES n-m WITH NOTHING`, `WITH ''`) correctly yield `lines_written: 0`.

- **Anti-pattern fragmentation tip** — the session tracks the last 5
  `SHOW LINES` reads.  When 3 or more sequential reads target the same file
  with adjacent or overlapping ranges (≤ 20-line gap), a hint is injected:
  *"Use `SHOW body OF 'function_name'` to read an entire function in one
  operation, or use a single wider `SHOW LINES` range."*  Switching to a
  different file resets the sequence.

- **`lines_written` field in mutation results** — `MutationResult` now includes
  `lines_written: usize`, surfaced in both JSON and compact output for all
  mutation types (`change_content`, `copy_lines`, `move_lines`).

### Changed

- **Line-budget config defaults retuned** — defaults adjusted based on
  real-world agent session analysis (bulk comment-translation workloads):

  | Parameter | Old | New | Rationale |
  |---|---|---|---|
  | `initial` | 200 | 1000 | Agents ran out too quickly on medium files |
  | `ceiling` | 2000 | 3000 | Higher headroom for long sessions |
  | `recovery_base` | 20 | 50 | Faster recovery between read bursts |
  | `recovery_window_secs` | 60 | 30 | Shorter halving window, less punishing |
  | `warning_threshold` | 40 | 250 | Earlier warning gives agents more time to adapt |
  | `critical_threshold` | 10 | 50 | More buffer before hard-cap kicks in |
  | `critical_max_lines` | 10 | 20 | Usable reads even in critical state |
  | `idle_reset_secs` | 300 | 200 | Faster stale-budget cleanup |

- **Mutation budget accounting** — mutations now call `session.reward_budget()`
  instead of `session.deduct_budget(0)`.  The old path gave only flat
  rolling-window recovery; the new path grants proportional recovery first,
  then applies rolling-window recovery on top.

---

## [0.32.0] — 2026-04-06

### Added

- **Line-budget system** — configurable per-session budget that limits how many
  source lines an agent can read.  Configured via `line_budget` section in
  `.forgeql.yaml`.  Features:
  - Rolling budget with diminishing-returns recovery within time windows
  - Warning state (below threshold) and critical state (caps SHOW LINES output)
  - Budget status (`remaining/ceiling (delta)`) included in every MCP
    response via `line_budget` metadata field
  - Persisted to `.budgets/{source}@{branch}.json` under the `ForgeQL` data dir
  - Budget file key uses the **feature branch name**, not the worktree alias:
    `USE src.main AS feat` → `src@feat.json`; `USE src.feat AS feat2` → `src@feat.json`
  - `USE src.X AS X` (alias equals branch) is rejected with a clear error
  - `idle_reset_secs` (default 300): expired files are auto-deleted on next `USE`
    via `sweep_expired()` — restores full budget after an idle gap, no cron needed
  - Budget delta reflects recovery on every command, including non-consuming ones
  - Warning and critical states include actionable token-saving tips in
    `status_line()` surfaced directly in each MCP response
  - Admin commands (`CreateSource`, `RefreshSource`, `ShowSources`, `ShowBranches`)
    are exempt from budget deduction and recovery

- **Relaxed DSL quoting** —
  - `string_literal` now accepts **double-quoted** strings (`"value"`) in
    addition to the existing single-quoted form (`'value'`), everywhere the DSL
    accepts a string.
  - New `bare_value` terminal: accepts unquoted alphanumeric tokens (plus
    underscores, colons, hyphens, dots, and forward-slashes) as string values
    wherever quoting is optional.
  - New `any_value` rule (`string_literal | bare_value`) is used in all
    positions where quoting is optional: `WHERE` predicates, `OF` targets
    (SHOW / FIND usages), `IN`, `EXCLUDE`, `MATCHING` patterns, COPY/MOVE file
    paths, and BEGIN/ROLLBACK/VERIFY step names.
  - `CHANGE … MATCHING` and `COMMIT MESSAGE` still require explicit quoting
    (content that may contain spaces).
  - `file_list` (CHANGE FILE/FILES path list) still requires explicit quoting
    for safety on mutations.

### Changed

- **MCP surface collapsed to a single `run_fql` tool** — `use_source`, `find_symbols`,
  `find_usages`, `show_body`, and `disconnect` tool definitions removed. All ForgeQL
  operations go through `run_fql` with raw FQL syntax. One tool, one mental model.
  - `run_fql` now extracts `session_id` from `USE` responses and prepends an
    `⚠️ IMPORTANT: Pass session_id "..." in ALL subsequent run_fql calls.` hint.

- **Composite worktree key: `branch.alias` on disk, `fql/branch/alias` in git** —
  `USE source.main AS 'fix-comments'` now creates worktree directory
  `main.fix-comments` and git branch `fql/main/fix-comments`. Previously both were
  just `fix-comments`, meaning two agents using the same alias on different base
  branches (`main` vs `dev`) would silently share a worktree. Now each
  `(base-branch, alias)` pair is a distinct, collision-free identity:
  - Filesystem: `data_dir/worktrees/main.fix-comments/` (flat, no nesting)
  - Git branch: `fql/main/fix-comments` (under `fql/` namespace, visible in `SHOW BRANCHES`)
  - The `fql/` prefix avoids a git loose-ref collision: `refs/heads/main` already
    exists as a file, so `refs/heads/main/fix-comments` is impossible without it.
  - On resume: the same `USE source.main AS 'fix-comments'` reconnects to the
    same worktree — uncommitted changes are preserved across server restarts.
  - On collision (same alias, same base): a warning is returned in `message` so
    agents know they may be resuming another agent's uncommitted work.

- **`USE` requires `AS 'branch-name'` (breaking change)** — `USE source.branch`
  without an `AS` clause is now a parse error. Every `USE` command must supply a
  human-readable branch alias, e.g. `USE forgeql-pub.main AS 'my-feature-branch'`.

### Removed

- **`DISCONNECT` command eliminated** — sessions are now fully managed by a server-side
  48-hour TTL. Worktrees persist across server restarts and are shared between agents.
  Multiple agents can reconnect to the same branch with `USE source.branch AS 'alias'`
  at any time — uncommitted changes are preserved. There is no explicit session-end
  ceremony; `COMMIT` is the natural terminal action.

### Fixed

- **`.forgeql-index` leaks into squash commits after BEGIN → ROLLBACK cycles** —
  Fixed by clearing `last_clean_oid` to `None` when the checkpoint stack becomes
  empty after rollback.

- **`CHANGE FILE LINES n-m WITH NOTHING` parse error** — made the `WITH` keyword
  optional so both `LINES 3-5 NOTHING` and `LINES 3-5 WITH NOTHING` are accepted.

- **USE hyphenated branch** — `use_stmt` grammar: the **branch** position now uses
  `source_name` (allows hyphens) instead of `identifier`.
  `USE forgeql-pub.line-budget AS 'lb2'` now parses correctly.  The AS target also
  accepts `any_value` so bare branch names work without quotes.

- **Budget reward display** — `BudgetState::deduct()` now captures `before` **before**
  `try_recover()` so the reported delta reflects the full net change.

- **`dup_logic` false positive with `*p++` in conditions** — fixed by using a
  position-unique key for side-effectful expressions in `skeleton_walk`.

- **`has_repeated_condition_calls` false positive with `isdigit(*p++)`** — fixed by
  using a per-position unique key for calls containing `++`/`--` operators.

### Security

- **Path traversal in `SHOW LINES`, `CHANGE FILE`, `COPY LINES`, `MOVE LINES`** —
  `Workspace::safe_path()` rejects absolute paths and normalises `..` components
  before checking the result still starts with the worktree root.  All four entry
  points are now guarded.

## [0.31.2] - 2026-03-29

### Added

- **README video links** — two YouTube videos added near the top of README.md:
  an overview video and a live demo of an AI agent querying the VLC source
  code (~600 K LOC).

### Fixed

- **COMMIT does not advance branch ref in linked worktrees** —
  `exec_commit` now uses a new `squash_commit_on_branch()` helper that
  resolves `HEAD → refs/heads/<branch>` before committing and updates
  the branch ref by name with an explicit parent OID.  Previously, the
  squash path called `soft_reset` followed by `repo.commit(Some("HEAD"))`;
  in linked worktrees (libgit2 1.8.1) `soft_reset` can detach HEAD,
  causing the commit to update a detached pointer instead of the branch
  ref — leaving the commit as a dangling object invisible to `git log`.

- **Compact diff shows file header/tail instead of actual edited region** —
  `compact_diff_plan` now uses a new `edit_based_change_ranges()` function
  that converts byte-range edits directly to line-level change ranges via
  binary search on a line-start-offsets table — O(edits × log(lines)).
  Previously, the compact diff path relied on an O(m×n) LCS algorithm
  with a 4 M-cell cap; any file over ~2 000 lines exceeded the cap,
  causing LCS to return no matches and the diff to collapse into a single
  range spanning the entire file, which was then elided to the first and
  last lines.

- **COMMIT fails with "current tip is not the first parent"** —
  `squash_commit_on_branch()` now creates the commit without a ref update
  (`repo.commit(None, …)`) and then force-updates the branch ref via
  `repo.reference()`.  Previously it passed the branch ref name to
  `repo.commit(Some(ref))`, which triggers libgit2's compare-and-swap
  check — since the branch tip had advanced past `last_clean_oid` during
  `BEGIN TRANSACTION`'s checkpoint commit, the CAS always failed.

## [0.31.1] - 2026-03-28

### Fixed

- **Symbol resolution picks wrong definition for ambiguous names** —
  `resolve_symbol` now prefers rows with a non-empty `fql_kind` (actual
  definitions) over reference-only index rows such as `scoped_identifier`
  nodes.  Previously, `SHOW body OF 'new'` could resolve to an unrelated
  function that merely *called* `new`, because the last-write-wins
  tie-breaker did not distinguish definitions from references.  All five
  symbol-targeted SHOW commands (`body`, `callees`, `context`, `signature`,
  `members`) are affected.

- **Recursion enrichment false positives on qualified calls** —
  `extract_callee_name` now returns the full qualified callee text (e.g.
  `Vec::new`) instead of stripping it to the bare name (`new`).
  `count_self_calls` compares qualified calls exactly and unqualified calls
  with an `ends_with` fallback for C++ out-of-line definitions.  This
  eliminates false `is_recursive = true` on every Rust `new()`, `default()`,
  `from()`, etc. that calls another type's constructor.

- **Recursion enrichment false negatives on C++ qualified self-calls** —
  `void Foo::bar() { Foo::bar(); }` is now correctly detected as recursive.
  Previously the qualified callee `Foo::bar` was stripped to `bar` and
  compared against the full name `Foo::bar`, always producing a mismatch.

- **Rust `scoped_identifier` nodes polluting the name index** —
  `RustLanguage::extract_name` now skips `scoped_identifier` nodes (e.g.
  `Vec::new` in a call expression), matching the existing C++ guard for
  `qualified_identifier`.  This prevents hundreds of reference-only rows
  from entering the name index and reduces the ambiguity that triggered the
  resolution bug above.
## [0.31.0] - 2026-03-27

### Added

- **`COPY LINES n-m OF 'src' TO 'dst' [AT LINE k]`** — copies a 1-based
  inclusive line range from one file to another (or the same file).  When
  `AT LINE k` is omitted the lines are appended at the end of the destination
  file.  The source file is left untouched.

- **`MOVE LINES n-m OF 'src' TO 'dst' [AT LINE k]`** — identical to `COPY`
  but also deletes `src` lines `n..=m` after the insertion.  For same-file
  moves the insert and delete are applied in reverse byte order so the result
  is correct regardless of move direction (up or down).

- **Heredoc `WITH <<TAG...TAG` syntax for CHANGE commands** — all three
  `WITH` forms (`CHANGE FILE LINES n-m WITH`, `CHANGE FILE WITH`, and
  `CHANGE FILE MATCHING ... WITH`) now accept a heredoc block in addition
  to the existing single-quoted string literal.  The heredoc tag must be
  all-uppercase (e.g. `RUST`, `CODE`, `END`); the closing tag must appear
  on its own line with no leading whitespace and must match the opening tag.
  The body may contain any characters — single quotes, double quotes,
  embedded ForgeQL keywords — without escaping.  This eliminates the
  single-quote quoting problem for code edits involving Rust char literals,
  lifetimes, and C-style string escapes.

- **`fql_kind` fast-path index lookup** — `FIND symbols WHERE fql_kind = '...'`
  now resolves through a dedicated `fql_kind` index instead of a full symbol
  scan, matching the performance of the existing `node_kind` power-user path.

- **Sidecar `.forgeql.yaml` config outside the repo** — ForgeQL now discovers
  and loads a `.forgeql.yaml` configuration file placed next to (but outside)
  the repository root, enabling per-project settings without touching the
  tracked tree.

### Fixed

- **`GROUP BY` count column now shows the real aggregate count** — previously
  the last column in grouped `FIND` results always displayed `0` (it was
  rendering the per-symbol `usages` field instead of the group count).
  `HAVING count >= N` filtering was always correct; only the display was wrong.

- **`.forgeql-session` and `.forgeql-index` excluded from all commits** —
  ForgeQL runtime control files are now filtered out of both internal
  checkpoint commits and user-visible `COMMIT` output, so they never
  appear in repository history.

### Changed

- **`SHOW BRANCHES` is now session-scoped** — the `OF <source>` argument
  has been removed.  `SHOW BRANCHES` now requires an active session and
  returns the branches for that session source.  Passing `OF <source>` is
  a grammar error.

## [0.30.0] - 2026-03-24

### Added

- **Rust language support** — new `forgeql-lang-cpp` sibling crate
  `forgeql-lang-rust` adds first-class Rust indexing via `tree-sitter-rust`.
  All `fql_kind` values (`function`, `struct`, `enum`, `class` for `impl`,
  `namespace` for `mod`, `variable`, `import`, `macro`, etc.) are mapped
  and enrichment fields work across both languages without query changes.

- **SMS (State Model Search) combinatorial test engine** — Phase C adds an
  automated combinatorial harness that exercises every `WHERE`, `ORDER BY`,
  `GROUP BY`, `LIMIT`, and `OFFSET` clause combination against real index
  data, verifying invariants (ordering, limit bounds, filter correctness)
  for each permutation.  Catches regressions in the clause pipeline that
  unit tests would miss.

### Changed

- **`SHOW outline` and `FIND symbols` now return `fql_kind` values** —
  the `kind` field in `SHOW outline` results and the group keys in `FIND
  symbols` CSV output are now `fql_kind` values (e.g. `function`, `class`,
  `macro`) rather than raw tree-sitter `node_kind` strings (e.g.
  `function_definition`, `class_specifier`, `preproc_def`).  A fallback to
  `node_kind` applies only when `fql_kind` is empty (unmapped nodes such as
  `compound_assignment`).  Queries using `WHERE kind = 'function'` now work
  identically across C++ and Rust.

- **`node_kind` deprecated for agent queries** — `node_kind` remains in the
  index for internal use and backwards compatibility, but all documentation,
  examples, and agent instructions now exclusively reference `fql_kind`.

- **`kind` alias removed — `fql_kind` is now the sole kind field** — the
  `kind` alias that previously routed `WHERE kind = '...'` to raw `node_kind`
  values on `FIND symbols` has been dropped.  `SHOW outline` and `SHOW
  members` now expose `fql_kind` in both WHERE predicates and JSON result
  objects (`OutlineEntry.fql_kind`, `MemberEntry.fql_kind`).  Compact CSV
  schema headers change from `"kind"` to `"fql_kind"`.  Power-users needing
  raw tree-sitter precision can still use `WHERE node_kind =
  'function_definition'`.

### Fixed

- **Compact diff: single oversized hunk now uses head/tail elision** —
  when a mutation produced a single hunk exceeding the K-line budget the
  renderer now shows a proportional K/2 head + `(… N lines elided …)` +
  K/2 tail instead of emitting lines until the budget ran out.

- **Cross-language symbol ambiguity in SHOW commands** — `SHOW body`,
  `SHOW signature`, `SHOW context`, and `SHOW callees` no longer return
  spurious results when two symbols from different languages share a name.

## [0.29.0] - 2026-03-24

### Added

- **Compact diff preview in CHANGE responses** — successful mutations now
  return a compact, token-bounded diff preview in the `diff` field of
  `MutationResult`.  The preview is computed in memory before applying
  edits, showing exactly what changed.  Parameters are configurable via
  `CompactDiffConfig` (defaults: K=14 content lines per file, W=40 chars
  per line, C=2 context-after lines).  Long lines are truncated with `…`;
  multi-hunk changes show the first and last hunks with elision of middle
  hunks.  Previously the response only confirmed `applied: true` with a
  file count, requiring a separate `SHOW LINES` to verify.

- **Disk-persisted session TTL via sentinel file** — each worktree now
  writes a `.forgeql-session` sentinel file containing the Unix epoch
  timestamp of its last activity.  `prune_orphaned_worktrees()` reads this
  sentinel before deleting a worktree, so server restarts and short-lived
  CLI invocations no longer lose the 48 h TTL timer.

- **Background session eviction in MCP mode** — a `tokio::spawn` interval
  task runs `evict_idle_sessions()` every 5 minutes while the MCP server
  is alive.  Previously the eviction function existed but was never
  called from a background loop, so idle sessions would accumulate
  indefinitely in long-running server processes.

### Changed

- **Engine shared via `Arc<Mutex>` in MCP** — `ForgeQlMcp` now wraps the
  engine in `Arc<Mutex<ForgeQLEngine>>` (was `Mutex<ForgeQLEngine>`),
  allowing the background eviction task to share access with the MCP
  handler.

- **`SESSION_TTL_SECS` is now `pub const`** — exposed so the background
  eviction task in the binary crate can reference it.

### Fixed

- **`CHANGE FILE LINES` trailing-newline bug** — `CHANGE FILE … LINES x-y
  WITH 'text'` no longer merges the last replacement line with the next
  existing line.  Since LINES is a line-oriented command and the replaced
  byte range includes the trailing newline, the replacement text must also
  end with one.  `resolve_lines()` now auto-appends `\n` when the content
  is non-empty and does not already end with one.

- **Transaction commits no longer pollute branch history** — `BEGIN
  TRANSACTION` checkpoint commits are now squashed into a single clean
  commit by `COMMIT MESSAGE`.  Previously every `BEGIN TRANSACTION`
  created a visible commit on the session branch, and `COMMIT` added yet
  another on top, leaving the history littered with internal
  `forgeql: checkpoint '…'` entries.  The new flow:
  - `BEGIN TRANSACTION` records a `pre_txn_oid` (the HEAD before the
    checkpoint) and tracks it in a new `Checkpoint` struct.
  - `COMMIT` soft-resets to `last_clean_oid` (the base before any
    checkpoints in the current cycle) then creates one squashed commit.
  - `ROLLBACK` updates `last_clean_oid` to the checkpoint's `pre_txn_oid`
    so subsequent commits squash from the correct base.
  Multi-cycle workflows (`BEGIN … COMMIT … BEGIN … COMMIT … ROLLBACK TO
  first`) are fully supported — rollback across multiple commit boundaries
  works correctly.

- **`.forgeql-index` excluded from user-facing commits** — a new
  `stage_and_commit_clean()` git helper stages all files except the binary
  index cache.  `COMMIT MESSAGE` uses it so the index file never appears
  in branch history.  Checkpoint commits still include the index (enabling
  fast cache-hit rollback via `resume_index()`).

- **Rollback uses `resume_index()` before full rebuild** — after
  `git reset --hard`, the engine now tries the on-disk index cache first.
  When the checkpoint commit included `.forgeql-index` the cache matches
  HEAD, giving an O(ms) restore instead of a full tree-sitter reparse.

- **Session TTL increased to 48 h** — prevents premature eviction during
  long development sessions (was 2 h).

- **`escape_count` / `escape_kinds` fields missing** — `EscapeEnricher` now
  emits all 5 documented fields.  Previously only `has_escape`,
  `escape_tier`, and `escape_vars` were emitted; `escape_count` and
  `escape_kinds` were documented but never implemented, causing
  `WHERE escape_count >= 1` to return 0 rows.

- **`has_assignment_in_condition` false positive on `>=` operator** —
  tree-sitter-cpp mis-parses `addr < 0 || addr >= 100` as a template
  expression followed by an assignment (`= 100`).  The enricher now
  detects this tree-sitter misparse pattern and skips it.

- **`duplicate_condition` too aggressive on simple guards** — trivial
  condition skeletons (≤ 4 chars, e.g. `(a)`, `(!a)`, `(a<b)`, `(a==b)`)
  are no longer flagged.  These simple guards repeat naturally in
  functions and produced noise rather than actionable findings.

- **Enrichment field → node kind optimisation** — all enricher field names
  (`escape_*`, `shadow_*`, `unused_param*`, `fallthrough_*`, `recursion_*`,
  `todo_*`, `decl_distance`, `decl_far_count`, `has_unused_reassign`) are
  now mapped in `field_to_kinds()`, enabling the query planner to skip
  non-function rows early.

### Added

- **`git::soft_reset()` helper** — equivalent of `git reset --soft <oid>`,
  used by `COMMIT` to squash checkpoint commits into a single clean commit.

- **`git::stage_and_commit_clean()` helper** — stages all files except
  `.forgeql-index`, ensuring the binary cache never leaks into user-facing
  commits.

- **`Checkpoint` struct** — replaces the previous `(String, String)` tuple
  in the checkpoint stack.  Tracks `name`, `oid`, and `pre_txn_oid` to
  support squash-on-commit and correct rollback across commit boundaries.

- **`Session::last_clean_oid` field** — records the base OID for the next
  `COMMIT` squash cycle.  Set on first `BEGIN TRANSACTION`, updated on
  each `COMMIT` and `ROLLBACK`.

- **`MATCHES` / `NOT MATCHES` operators** — regex filtering in WHERE
  predicates via the `regex` crate.  Works on any string field:
  `WHERE name MATCHES '^(get|set)_'`,
  `WHERE text MATCHES '(?i)TODO|FIXME'`.

- **Universal WHERE on SHOW commands** — WHERE predicates now work on:
  - `SHOW body`, `SHOW lines`, `SHOW context` — filter source lines by
    `text` (content) or `line` (number).  Example:
    `SHOW body OF 'func' DEPTH 99 WHERE text MATCHES 'return' LIMIT 100`
  - `SHOW callees` — filter call graph entries by `name`, `path`, `line`.
    Enables single-query recursion detection:
    `SHOW callees OF 'fn' WHERE name = 'fn'`

- **`ClauseTarget` for `SourceLine`** — fields: `text` (content),
  `line` (number), `marker`.

- **`ClauseTarget` for `CallGraphEntry`** — fields: `name`, `path`/`file`,
  `line`.

- **`DeclDistanceEnricher`** — new enricher adding three fields to function
  rows:
  - `decl_distance`: sum of (first-use − declaration) line distances for
    locals with distance ≥ 2.
  - `decl_far_count`: count of local variables with distance ≥ 2.
  - `has_unused_reassign`: `"true"` when a local is reassigned before its
    previous value was read (dead store detection).
  Excludes parameters, globals, and member variables.  Fully language-agnostic
  via `LanguageConfig` fields.

- **`LanguageConfig` expansion** — six new fields for language-agnostic
  data-flow analysis: `parameter_list_raw_kind`, `identifier_raw_kind`,
  `assignment_raw_kinds`, `update_raw_kinds`, `init_declarator_raw_kind`,
  `block_raw_kind`.

- **`EscapeEnricher`** — detects functions that return addresses of
  stack-local variables (dangling pointer risk).  Three detection tiers:
  - Tier 1 (`escape_tier=1`): direct `return &local` — 100% certain.
  - Tier 2 (`escape_tier=2`): array decay `return local_array` — 100% certain.
  - Tier 3 (`escape_tier=3`): indirect alias `ptr = &local; return ptr`.
  Fields: `has_escape`, `escape_tier`, `escape_vars`.
  Excludes `static` locals (safe).  Fully language-agnostic via
  `LanguageConfig` — five new fields: `return_statement_raw_kind`,
  `address_of_expression_raw_kind`, `address_of_operator`,
  `array_declarator_raw_kind`, `static_storage_keywords`.

- **`ShadowEnricher`** — detects functions where an inner scope
  redeclares a variable name that already exists in an outer scope
  (parameter or enclosing block).  Fields: `has_shadow`, `shadow_count`,
  `shadow_vars`.  Handles nested blocks, for-loop initializer
  declarations, and multi-level nesting.  Fully language-agnostic via
  existing `LanguageConfig` fields.

- **`UnusedParamEnricher`** — detects function parameters that are never
  referenced in the function body.  Fields: `has_unused_param`,
  `unused_param_count`, `unused_params`.  Fully language-agnostic via
  existing `LanguageConfig` fields.

- **`FallthroughEnricher`** — detects switch/case statements where a
  non-empty case falls through to the next case without `break` or
  `return`.  Empty cases (intentional grouping like `case 1: case 2:`)
  are not flagged.  Fields: `has_fallthrough`, `fallthrough_count`.
  Two new `LanguageConfig` fields: `case_statement_raw_kind`,
  `break_statement_raw_kind`.

- **`RecursionEnricher`** — detects direct (single-function) self-recursion.
  Fields: `is_recursive`, `recursion_count`.  One new `LanguageConfig`
  field: `call_expression_raw_kind`.

- **`TodoEnricher`** — detects TODO, FIXME, HACK, and XXX markers in
  comments inside function bodies.  Word-boundary-aware matching avoids
  false positives.  Fields: `has_todo`, `todo_count`, `todo_tags`.
  Uses existing `comment_raw_kind` from `LanguageConfig`.

- **Shared data-flow utilities** (`data_flow_utils.rs`) — extracted common
  local-variable collection, declarator walking, write-context detection,
  and AST helpers from `DeclDistanceEnricher` for reuse by `EscapeEnricher`
  and future enrichers.

### Changed

- **`use_source` MCP response now includes a prominent session_id reminder** —
  the tool response prepends a dedicated text block:
  `⚠️ IMPORTANT: Pass session_id "…" in ALL subsequent tool calls (find_symbols, find_usages, show_body, run_fql, disconnect).`
  The tool description was also updated to state the session_id `MUST` be
  passed to every subsequent call.

- **Agent instruction files expanded to self-contained references** —
  `forgeql.agent.md` and `CLAUDE.md` now inline all syntax, `fql_kind`
  table, enrichment fields, and recipes. No external `references/` files
  needed per workspace.

- **README.md (agents)** — clarified deployment: one file per workspace,
  `references/` folder is human documentation only.

- **WHERE on source lines runs before line cap** — the implicit
  `DEFAULT_SHOW_LINE_LIMIT` truncation now runs after WHERE filtering,
  so queries search the full function body, not just the first N lines.

## [0.28.0] - 2026-03-22

### Added

- **Language-agnostic architecture** — `forgeql-core` no longer contains any
  language-specific code. All language knowledge is provided via the
  `LanguageSupport` trait, `LanguageConfig` struct, and `LanguageRegistry`.
  Adding a new language requires only a new crate — zero changes to core.

- **`forgeql-lang-cpp` crate** — C++ language support extracted into its own
  crate (`crates/forgeql-lang-cpp/`). Contains `CppLanguage`, `CPP_CONFIG`,
  `map_kind()`, and `cpp_registry()`.

- **`fql_kind` field** — universal kind on every `IndexRow`: `function`, `class`,
  `struct`, `enum`, `variable`, `field`, `comment`, `import`, `macro`,
  `type_alias`, `namespace`, `number`, `cast`, `operator`. Query with
  `WHERE fql_kind = 'function'` for language-agnostic filtering.

- **`language` field** — every `IndexRow` carries the language name (e.g. `cpp`).
  Query with `WHERE language = 'cpp'`.

- **New enrichment fields**:
  - `suffix_meaning` — semantic meaning of number suffixes (e.g. `unsigned`)
  - `catch_all_kind` — kind of catch-all branch in switch (e.g. `default`)
  - `for_style` — `traditional` or `range` for loops
  - `operator_category` — `increment`, `arithmetic`, `bitwise`, or `shift`
  - `throw_count` — count of throw statements in functions
  - `cast_safety` — `safe`, `moderate`, or `unsafe` for cast expressions
  - `binding_kind` — `function` or `variable` for declarations
  - `is_exported` — `true` for file-scope non-static declarations
  - `member_kind` — `method` or `field` for class/struct members
  - `owner_kind` — raw kind of enclosing type for members
  - `is_override`, `is_final` — modifier flags for virtual method specifiers

- **`MemberEnricher`** — enrichment pass that populates `body_symbol`,
  `member_kind`, and `owner_kind` on `field_declaration` nodes.

- **`body_symbol` enrichment field** — queryable via
  `FIND symbols WHERE body_symbol = 'Class::method'`.

### Changed

- **`has_default` renamed to `has_catch_all`** — the switch enrichment field
  uses language-agnostic terminology. Queries using `has_default` must be
  updated to `has_catch_all`.

- **All enrichers are now config-driven** — enrichers read from
  `LanguageConfig` instead of hardcoding C++ node kinds. This is an internal
  change with no effect on query results for C++ code.

### Fixed

- **`SHOW body` failed for bare member names** — `SHOW body OF 'loadSignalCode'`
  returned "function definition not found" when the symbol was a class member
  declaration (`field_declaration`) rather than the out-of-line
  `function_definition`.  The `MemberEnricher` now stamps a `body_symbol`
  field on member method declarations during indexing (e.g.
  `body_symbol = "SignalSequencer::loadSignalCode"`), and `show_body` /
  `show_callees` follow the redirect — completely language-agnostic.

- **Class/struct member declarations were not indexed** — tree-sitter C++ uses
  `field_declaration` for members inside class bodies, but the indexer only
  handled `declaration` nodes.  Added a `("cpp", "field_declaration")` arm to
  `extract_name()` and `"field_identifier"` to `find_function_name()` so that
  member function prototypes and data members are now visible in the symbol
  index.

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
  | **ControlFlowEnricher** | `condition_tests`, `paren_depth`, `has_catch_all`, `has_assignment_in_condition`, `mixed_logic`, `branch_count` |
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


