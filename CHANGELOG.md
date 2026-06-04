# Changelog

All notable changes to ForgeQL will be documented in this file.

ForgeQL uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.57.0] — 2026-06-04 — Phase B: FIND NODE command

### Added

- `FIND NODE id` command — resolves a `node_id` to its current location,
  `rev`, and navigation links:
  - O(log N) segment lookup via `seg_idx_for_node_id_prefix` binary search
  - Linear `col_ordinal` scan within the matched segment (zero heap)
  - Returns `fql_kind`, `name`, `path`, `line`, `rev`, and four nav links:
    `parent_node_id`, `first_child_node_id`, `next/prev_sibling_node_id`
- `FindNodeResult` struct and `ForgeQLResult::FindNode` variant.
- Compact output: header row, schema row, data row, `node_nav` footer row.
- `node_not_found` error response with `suggested_next` hint.
- `StorageEngine::find_node` trait method with `Ok(None)` default impl.

## [0.56.0] — 2026-06-04 — B-prep: pre-computed navigation and rev columns in segment

### Added

- Five new typed segment columns computed at index time, zero heap at query time:
  - `col_parent_ordinal` (u32): ordinal of the nearest indexed ancestor; `u32::MAX` for
    top-level nodes. Replaces the `parent_ordinal` enrichment string field.
  - `col_rev` (u64, raw 8-byte LE): first 8 bytes of SHA-256 of the node byte span.
    Enables `IF REV` safety checks in node-addressed mutations without a file read.
  - `col_first_child_ordinal` (u32): ordinal of the first addressable child, filled by a
    post-DFS pass in `ShadowWriter`.
  - `col_next_sibling_ordinal` (u32): ordinal of the next addressable sibling.
  - `col_prev_sibling_ordinal` (u32): ordinal of the previous addressable sibling.
- `SegmentReader` accessors: `parent_ordinal_of`, `rev_of`, `first_child_ordinal_of`,
  `next_sibling_ordinal_of`, `prev_sibling_ordinal_of` — all `cast_slice` + index reads,
  zero heap allocation.
- `format_rev(u64) -> String` helper in `node_id.rs` (format: `h{:016x}`).

### Changed

- `IndexRow` gains two typed fields: `parent_ordinal: u32` and `rev: u64`, set directly
  in the `file_indexer` DFS (SHA-256 computed inline from source bytes). The
  `parent_ordinal` enrichment string is no longer written to the fields map.
- `OrdinalHint` construction in the reindex path now reads `row.parent_ordinal` directly
  instead of parsing from the enrichment string map.
- `ShadowWriter` nav post-pass: after emitting all rows for a file, groups addressable
  rows by parent ordinal, sorts by ordinal (DFS order), and fills first-child and sibling
  links across the file in a single O(N) pass.
- `RowId` inner field made `pub` to allow construction in `ShadowWriter` post-pass.
- `ENRICH_VER` bumped to 13 to force reindex of all segments onto the new layout.

## [0.55.8] — 2026-06-04 — Fix misleading parse error for unterminated WITH strings

### Fixed

- `CHANGE FILE ... WITH '...'` now emits a targeted hint when the closing
  quote is missing, instead of the cryptic `expected content_value` error
  pointing at the opening quote. Root cause: pest reports the position where
  a rule was attempted, not where the string ran out of input.
- The hint also documents two already-supported alternatives for content
  containing single quotes (e.g. Rust lifetimes): double-quoted strings
  (`WITH "pub x: &'a T,"`) and HEREDOC blocks (`WITH <<TAG
  content TAG`). Both were in the grammar but undocumented.

### Added

- `HINTS.md` at the repo root — documents the stable node_id ordinal

   invariant, correct ordinal-access patterns for columnar and live-index
  paths, key file locations, and CHANGE FILE quoting guidelines.

## [0.55.7] — 2026-06-04 — Thread node_id through SymbolMatch and ordinal through SymbolLocation/ShowRequest

### Changed

- **AC-1**: Added `node_id: Option<String>` field to `SymbolMatch`; populated it in `materialize_rows`, `materialize_one_row`, and both `resolve_impl` symbol match constructions from `ordinal_of(row)` + `make_node_id`. Replaced the broken `fields.get("ordinal")` block in `SymbolRow::from_match_with_ctx` with `row.node_id.clone()`.

- **AC-2**: Added `ordinal: Option<u32>` field to `SymbolLocation`; populated it in `location_for_row` (seg.ordinal_of), the dirty-overlay `SymbolLocation` in `resolve_impl` (ds.reader.ordinal_of), and `row_to_location` (row.ordinal).

- **AC-3**: Added `ordinal: Option<u32>` field to `ShowRequest`; passed `loc.ordinal` in `exec_show_body` and `None` in the three other `ShowRequest` constructors. Replaced the broken `enrichment.get("ordinal")` block in `show_body` with `req.ordinal.map(|ord| make_node_id(&path_str, ord))`.

- Every `SymbolMatch` construction in legacy, test, and compact code updated with `node_id: None`.
- Every `ShowRequest` construction in tests updated with `ordinal: None`.
- Restored accidentally trimmed `show_outline_for_file` trait method in `storage/mod.rs`.

## [0.55.6] — 2026-06-03 — Fix dirty-overlay path disambiguation in resolve_impl

### Fixed

- `resolve_impl` Stage 1 (dirty overlay) now applies `IN`/`EXCLUDE` glob path
  filters before considering dirty segments. Previously, `SHOW body OF 'name'
  IN 'file.rs'` could return a symbol from an unrelated file if multiple files
  in the dirty overlay contained functions with the same name. Mirrors the
  `segments_passing_path_filter` logic already used in Stage 2.
- `resolve_impl` Stage 1 now sorts dirty candidates by path alphabetically
  before selecting the last entry, matching the deterministic tie-breaking used
  by Stage 2 (persistent segments). Previously, the most-recently-edited file
  in the transaction would win ambiguous name resolution instead of the
  alphabetically-last path.
- Bug was introduced in `baa983e` (PhaseFT1) which wired dirty segments into
  `resolve_impl` for the first time but omitted path-filter propagation and
  stable tie-breaking.
- Added two regression tests:
  `dirty_overlay_resolve_respects_in_glob_filter` and
  `dirty_overlay_resolve_uses_alphabetical_not_insertion_order`.

## [0.55.5] — 2026-06-03 — Eliminate global lock from node_id computation

### Changed

- Moved SHA-256 path hashing and shortest-prefix computation from query time to
  `Overlay::open()` time. All segments in an overlay are hashed together in a
  single pass, so each `node_id` emission at query time costs only a struct
  field read and a string format — no SHA-256, no lock, no allocation beyond
  the returned `String`.
- Added `sha256: [u8; 32]` and `prefix_len: u8` to `SegmentMeta`, with
  convenience methods `segment_id()` and `node_id(ordinal)` that read the
  pre-computed values.
- Added `seg_id_index` to `Overlay`: a `Vec<([u8; 32], u32)>` sorted by SHA-256
  bytes, shared via `Arc<Overlay>` across all concurrent sessions. Enables O(log N)
  reverse lookup from a `node_id` hex prefix to a segment index with zero heap
  allocation — groundwork for node-addressed queries.
- Restored `node_id::make_node_id(path, ordinal)` as a thin helper for call
  sites (e.g. `SHOW body`, `SHOW members`) that have a path string but no
  `SegmentMeta`. Uses a single SHA-256 + default 12-char prefix; no global state.

### Fixed

- Two syntax errors in `overlay.rs` introduced by the previous commit: a stray
  closing brace that ended `impl Overlay` prematurely, and a duplicate
  `start..end` expression in `row_range_for_path_range`.
- Three call sites in `ast/show/body.rs`, `ast/show/members.rs`, and `result.rs`
  that were left referencing the deleted `make_node_id` function, causing a
  build failure.
- Clippy lints in `node_id.rs` and `overlay.rs`: `cast_possible_truncation`
  (changed constant type to `u8`, used `usize::from` for indexing,
  `filter_map`+`try_from` for the index cast), `doc_markdown` (added backticks),
  and `manual_is_multiple_of`.

## [0.55.4] — 2026-05-31 — Addressable node_id policy and regression coverage

### Changed

- Restricted ordinal and `node_id` assignment to addressable `fql_kind` rows so analysis-only rows (such as number literals) no longer surface stable node handles.
- Preserved stable ordinals for addressable extra rows such as control-flow nodes, fixing missing `node_id` values on `if`/`while` outline entries.
- Bumped `ENRICH_VER` from `11` to `12` to force rebuilds onto the updated addressable-only node-id policy.

### Tests

- Added focused integration coverage for addressable-vs-analysis-only `node_id` behavior in `engine_integration`.
- Added a new `NID*` golden test block covering baseline node-id projection, addressable policy enforcement, mutation stability, rename visibility, and rollback restoration.

## [0.55.3] — 2026-05-31 — Stable node addressing improvements

### Changed

- Migrated node ordinals from enrichment text to a dedicated `col_ordinal` `u32` column in `.fqsf` segments.
- Added `IndexRow.ordinal: Option<u32>` and threaded ordinal writes through `file_indexer`, `build_context`, and `shadow_writer`.
- Removed `skip_serializing_if` on `IndexRow.ordinal` to keep `CachedIndex` bincode round-trips stable.
- Added `SegmentBuilder::set_ordinal` and `SegmentReader::ordinal_of`, and switched outline node-id projection to typed ordinal reads.
- Added `node_id: Option<String>` to `SourceLine` and parser-side extraction in result conversion.
- Updated `show_body` to emit `node_id` on the function start line when ordinal metadata is present.
- Switched `segment_id()` to SHA-256 normalized-path hashing with minimum unambiguous hex-prefix expansion.
- Added ordinal remapping support (`OrdinalRemapper`/`OrdinalHint`) so reindexing can preserve stable ordinals across edits.
- Implemented layered rematch resolution using symbol identity, guard metadata, statement fingerprint, and content hash.
- Bumped `ENRICH_VER` from `10` to `11` to force rebuilds that populate the new ordinal column.
## [0.55.2] — 2026-05-30 — Addressable node IDs in results

### Added

- `crates/forgeql-core/src/node_id.rs` **(new)**:
  - Stable node-handle helpers for `segment_id(path)` and `make_node_id(path, ordinal)`.

### Changed

- `crates/forgeql-core/src/ast/index/file_indexer.rs`:
  - Added per-file DFS ordinal assignment (`ordinal`) for named indexed rows.
  - Added `parent_ordinal_stack` traversal state to mirror parent ancestry during DFS walk.
- `crates/forgeql-core/src/result.rs`:
  - Added optional `node_id` to `SymbolRow` and `OutlineEntry`.
- Outline paths now emit `node_id` when available:
  - `crates/forgeql-core/src/storage/columnar/columnar_storage/query.rs`
  - `crates/forgeql-core/src/ast/show/members.rs`
  - `crates/forgeql-core/src/engine/convert.rs`
  - `crates/forgeql-core/src/compact.rs` (compact schema/rows updated for node_id-aware output)
- `crates/forgeql-core/src/storage/columnar/mod.rs`:
  - `ENRICH_VER` bumped to `10` to force reindex and populate ordinal-enriched rows.

### Notes

- This release introduces stable `node_id` values in existing query and outline outputs, with automatic reindex migration (`ENRICH_VER = 10`).
- Additional robustness and coverage improvements for node addressing will ship in follow-up releases.

## [0.55.1] — 2026-05-30 — Golden expectations updated after frozen-branch reindex

### Changed

- Golden test baselines were adapted after reindexing the two frozen golden sources:
  - `zephyr-andre.zephyr-main`
  - `pytorch-andre.pytorch-frozen`
- `crates/forgeql/tests/golden.json`:
  - Refreshed affected expected rows/counts to match post-reindex canonical ordering and metrics.
  - Kept markdown paragraph probes (`LIKE`/`MATCHES`) in place as regression coverage for `.md` content queries.

## [0.55.0] — 2026-05-30 — Markdown language support + golden session isolation

### Added

- `crates/forgeql-lang-markdown` **(new crate)**:
  - Markdown `LanguageSupport` implementation backed by `tree-sitter-md`
  - Embedded config at `crates/forgeql-lang-markdown/config/md.json`
  - Kind mappings for `heading`, `section`, `code_block`, `list_item`, `paragraph`,
    `table`, `block_quote`, and `import` (`link_definition`)
  - `.md` and `.mdx` extension support

### Changed

- Workspace wiring:
  - Added `crates/forgeql-lang-markdown` to workspace members and dependencies
  - Registered `MarkdownLanguage` in `forgeql` binary startup registry
- `crates/forgeql/tests/zephyr_golden.rs`:
  - USE aliases are now run-scoped (`<alias>-g<pid>`) to avoid resuming stale/dirty
    sessions from interrupted prior runs in mutation-heavy golden tests.

## [0.54.19] — 2026-05-25 — P2-F: externalize corpus/golden/syntax test data

### Changed

- `crates/forgeql/tests/corpus.json` **(new)**: 201-entry JSON array extracted from the
  616-line inline `corpus()` function in `parity_find.rs`. Loaded at compile-time with
  `include_str!`; `corpus()` reduced to a 5-line JSON loader.
- `crates/forgeql/tests/zephyr_golden.rs`: `golden_values()` switches read path from
  `std::fs::read_to_string` to `include_str!("golden.json")` — `fixture_path` kept for
  update-mode write-back; both `from_str` call-sites fixed for `needless_borrow`.
- `crates/forgeql-core/tests/sms_integration.rs`: `load_syntax()` switches read path from
  `fs::read_to_string` to `include_str!("../../../tests/fixtures/syntax.json")`.

## [0.54.18] — 2026-05-25 — P2-E: split ast/index.rs into module folder

### Changed

- `crates/forgeql-core/src/ast/index.rs` trimmed to root file: type aliases, structs
  (`SegmentBuildCtx`, `IndexRow`, `UsageSite`, `IndexStats`, `MemEstimate`, `SymbolTable`,
  `RowRef`), `reassign_intern_ids`, `node_text`, module declarations, tests.
- `crates/forgeql-core/src/ast/index/build.rs` **(new)**: `SecondaryIndexBuilder` and full
  `impl SymbolTable` block (build, merge, incremental reindex, query, purge methods, ~730 lines).
- `crates/forgeql-core/src/ast/index/file_indexer.rs` **(new)**: per-file parse pass —
  `collect_macro_defs_for_file`, `IndexContext`, `index_file`, `collect_nodes`,
  `extract_fields` (~430 lines).

## [0.54.17] — 2026-05-25 — P2-C fix + P2-D: split lang.rs into module folder

### Changed

- `columnar_storage.rs`: removed stale `#![allow(clippy::redundant_pub_crate)]` (all
  `pub(super)` items live in sub-modules, lint never fires in root).
- `crates/forgeql-core/src/ast/lang.rs` trimmed to root file (constants, `LanguageConfig`
  struct, `MacroDef`, traits, `LanguageRegistry`, module declarations, tests).
- `crates/forgeql-core/src/ast/lang/config.rs` **(new)**: `impl LanguageConfig` block
  (923-line query/accessor methods).
- `crates/forgeql-core/src/ast/lang/inline.rs` **(new, cfg-gated)**: test-only inline
  C++, Rust, and Python implementations; `include_bytes!` paths corrected for new depth.
## [0.54.16] — 2026-05-25 — P2-C: split columnar_storage.rs into module folder

### Changed

- `crates/forgeql-core/src/storage/columnar/columnar_storage.rs` trimmed to root
  module file (struct + `new()` + `mod` declarations + tests).
- `columnar_storage/fast_paths.rs` — fast-path `impl ColumnarStorage` methods
  and module-level helper free functions (18 items made `pub(super)`).
- `columnar_storage/query.rs` — resolve helpers + `StorageEngine` trait impl;
  imports fast-path symbols via `use super::fast_paths::…`.
- `columnar_storage/commit.rs` — overlay orchestration, dirty/delta helpers,
  and commit logic (3 methods made `pub(super)`).

## [0.54.15] — 2026-05-25 — P2-B: split build_and_persist into private step methods

### Changed

- **`crates/forgeql-core/src/storage/columnar/overlay_builder.rs`** — extracted the 486-line,
  12-step `build_and_persist` body into 10 private methods; orchestrator is now 89 lines.
  Methods without `self` access are associated functions (`Self::`) to avoid `unused_self`:
  - `step1_open_segments` — parallel mmap open of all segments (uses `self`)
  - `step25_collect_file_only` — workspace files not in any segment (uses `self`)
  - `step34_build_row_index` — cumulative row offsets + `global_row_table`
  - `step45_dedup_segments` — per-segment canonical row sets (parallel dedup)
  - `step5_build_kind_postings` — merge kind bitmaps across segments
  - `step55_build_enrich_bitmaps` — three-phase enrichment bitmap pipeline
  - `step6_build_name_fst` — merge name FST, postings, and trigram index
  - `step75_build_index_files` — cached file sizes array (uses `self`)
  - `step76_build_file_entries` — file-only entries blob (uses `self`)
  - `step8_write_overlay` — atomic temp-file → fsync → rename write
  - `#[expect(clippy::too_many_lines)]` moved from `build_and_persist` to `step55_build_enrich_bitmaps`
  - `#[expect(clippy::type_complexity)]` added to `step6_build_name_fst` for its triple return
## [0.54.14] — 2026-05-25 — P2-A: split exec_show match arms into private methods

### Changed

- **`crates/forgeql-core/src/engine/exec_show.rs`** — extracted every `match op { … }` arm
  of `exec_show` (397 lines) into a dedicated private method:
  - `exec_show_context` — resolves symbol + calls `show::show_context`
  - `exec_show_signature` — resolves symbol + calls `show::show_signature`
  - `exec_show_outline` — delegates to `engine.show_outline_for_file`
  - `exec_show_members` — resolves type symbol + calls `show::show_members`
  - `exec_show_body` — resolves body symbol + calls `show::show_body`
  - `exec_show_callees` — resolves body symbol + calls `show::show_callees`
  - `exec_show_lines` — delegates to `show::show_lines`
  - `exec_show_find_files` — full FindFiles clause pipeline (fast-path + filesystem walk);
    returns `Result<serde_json::Value>`; annotated `#[expect(clippy::too_many_lines)]`
  - Four methods that do not access `self` are associated functions (`Self::` call sites);
    four that call `get_or_parse_for_show` / `lang_registry` remain `&self` methods.
  - `exec_show` itself is now a 27-line dispatcher; `#[expect(clippy::too_many_lines)]`
    attribute and unused `let root = workspace.root()` binding removed.
  - Added `storage::StorageEngine` to imports for parameter typing in the new methods.

## [0.54.13] — 2026-05-25 — P1-F: replace field_to_kinds_for_config match with OnceLock HashMap

### Changed

- **`crates/forgeql-core/src/storage/legacy/prefilter.rs`** — eliminated the
  214-line `match field { … }` in `field_to_kinds_for_config`. Replaced by:
  - `type FieldKindFn` / `type FieldKindMap` type aliases
  - `cast_kinds` and `qualifier_kinds` named helpers for the two non-trivial arms
  - `FIELD_KIND_MAP: OnceLock<FieldKindMap>` static populated once by `get_field_kind_map()`
  - `field_to_kinds_for_config` reduced to a single `HashMap::get` + `map` call

## [0.54.12] — 2026-05-25 — P1-E: add Session::from_coords factory

### Changed

- **`crates/forgeql-core/src/session/mod.rs`** — `Session::from_coords` convenience
  constructor added: takes `&SessionCoords`, `PathBuf`, and `&Arc<LanguageRegistry>`,
  delegates to `Session::new` mapping `coords.alias→id`, `coords.user→user_id`,
  `coords.source→source_name`, `coords.branch→branch`.
- **`crates/forgeql-core/src/engine/exec_source.rs`** — `use_source` call site updated:
  7-line `Session::new(…)` block replaced by single `Session::from_coords(&coords, …)` call.

## [0.54.11] — 2026-05-25 — P1-D: introduce EscapeLocals + EscapeAccumulator structs

### Changed

- **`crates/forgeql-core/src/ast/enrich/escape.rs`** — `check_expr_escape` reduced
  from 9 parameters to 4 by bundling the read-only inputs into `EscapeLocals<'a>`
  and the three mutable accumulation fields (`escaping`, `best_tier`, `kinds_seen`)
  into `EscapeAccumulator`. `EscapeAccumulator::new()` initialises the accumulator.
- `#[allow(clippy::too_many_arguments)]` on `check_expr_escape` removed.
- Phase 5 walk in `enrich_row` and Phase 5b macro-expansion closure updated to
  use `acc.*` fields in place of the three separate `mut` variables.

## [0.54.10] — 2026-05-25 — P1-C: introduce SecondaryIndexBuilder struct

### Changed

- **`crates/forgeql-core/src/ast/index.rs`** — `SecondaryIndexBuilder<'a>` struct
  replaces the 8-parameter free function `index_row_into_secondaries`. Holds disjoint
  `&mut` borrows of the five secondary-index fields plus an immutable `&ColumnarTable`
  borrow; exposes a single `insert(&mut self, row: &IndexRow, idx: u32)` method.
- **`merge`**, **`push_row`**, **`rebuild_indexes_from_rows`** — the three call sites
  now construct a `SecondaryIndexBuilder` inline and call `.insert()`.
- `#[allow(clippy::too_many_arguments)]` on `index_row_into_secondaries` removed.
## [0.54.9] — 2026-05-25 — P1-B: introduce IndexContext struct

### Changed

- **`crates/forgeql-core/src/ast/index.rs`** — `IndexContext<'a>` struct bundles
  `path`, `language`, `enrichers`, `macro_table`, and `table`; the five parameters
  shared by `collect_nodes` and `index_file`.
- **`collect_nodes`** — signature reduced from 8 parameters to 4
  (`source`, `ctx: &mut IndexContext<'_>`, `cursor`, `ts_language`);
  `#[allow(clippy::too_many_arguments)]` removed.
- **`index_file`** — signature reduced from 7 parameters to 3
  (`parser`, `ctx: &mut IndexContext<'_>`, `seg_ctx`);
  `#[allow(clippy::too_many_arguments)]` removed.
- **`SymbolTable::build`** (×2) and **`reindex_files`** — call sites updated to construct
  `IndexContext` before calling `index_file`.
- **`columnar_storage.rs`**, **`columnar_filter.rs`**, **`columnar_range.rs`**,
  **`segment_parity.rs`**, **`overlay_parity.rs`**, **`lang_coverage_integration.rs`** —
  all external call sites updated to the new `IndexContext` API.
## [0.54.8] — 2026-05-25 — P1-A: introduce ShowRequest struct

### Changed

- **`crates/forgeql-core/src/ast/show/request.rs`** (new) — `ShowRequest<'a>` struct
  bundles the 7 parameters shared by all four `show_*` symbol functions.
- **`show_body`**, **`show_callees`**, **`show_signature`**, **`show_members`** —
  signatures reduced from 5–9 individual parameters to `req: &ShowRequest<'_>`
  (plus function-specific extras); `#[allow(clippy::too_many_arguments)]` removed.
- **`exec_show.rs`** — each call site now builds one `ShowRequest` from the resolved
  `SymbolLocation` and passes it by reference, eliminating 28 duplicate parameter lines.
- **`overlay_parity.rs`** — all direct `show_*` test call sites updated to the new API.

## [0.54.7] — 2026-05-25 — Refactoring roadmap: parameter clustering + file splitting

### Added

- **`TODO.md`** — comprehensive refactoring roadmap covering all 13 steps across
  three phases: parameter clustering (P1-A through P1-F), file splitting
  (P2-A through P2-F), and regression prevention (P3-A).  Each step is a
  self-contained commit on the `code-refactore` branch.

## [0.54.6] — 2026-05-25 — Lang crates, CLI, MCP, session lint cleanup

### Fixed

- **`forgeql-lang-{python,cpp,rust,c}/src/lib.rs`** — `#[allow(expect_used)]` on `*_config()` → `#[expect(..., reason = "embedded JSON validated at test time")]`; test module allow-lists replaced with precise `#[expect(...)]` per-crate (python: `unwrap_used` only; cpp: `expect_used` only; rust: both; c: removed entirely).
- **`forgeql-lang-{cpp,rust}/src/macro_expand.rs`** — `#[allow(redundant_pub_crate)]` on struct → `#[expect(...)]` (lint fires); test module `#[allow(unwrap_used, expect_used)]` → `#[expect(...)]` (both fire).
- **`forgeql/src/cli.rs`** — test module `#[allow(clippy::panic)]` → `#[expect(...)]` (`panic!` is used in tests).
- **`forgeql/src/mcp.rs`** — `#[allow(dead_code)]` on `tool_router` field → `#[expect(dead_code, reason = "rmcp ToolRouter macro")]`; two `#[allow(needless_pass_by_value)]` → `#[expect(...)]` (`map_err` requires ownership); test module `unwrap_in_result` suppression removed (lint never fires).
- **`forgeql/src/session.rs`** — test module `unwrap_in_result` suppression removed (lint never fires).

## [0.54.5] — 2026-05-25 — Columnar/AST/engine/transforms lint cleanup

### Fixed

- **`manifest.rs`** — test module `#![allow(unwrap_used, expect_used)]` → `#![expect(unwrap_used)]` (expect_used lint never fires in that test module).
- **`overlay_lock.rs`** — `lock_path` field `#[allow(dead_code)]` → `#[expect(dead_code, reason=...)]`; test module `#[allow(unwrap_used, expect_used)]` → `#[expect(unwrap_used, expect_used)]`.
- **`segment_reader.rs`** — `#[allow(unsafe_code)]` → `#[expect(unsafe_code, reason=...)]`; two `#[allow(cast_possible_truncation)]` on masked `u64→usize` casts → `usize::try_from(...).unwrap_or(usize::MAX)`; `#[allow(indexing_slicing)]` → `#[expect(indexing_slicing, reason=...)]`; test module allow-list pruned (panic/items_after_statements/wildcard_imports never fire).
- **`overlay_builder.rs`** — dead `#![allow(redundant_pub_crate)]` removed; `#[allow(too_many_lines)]` → `#[expect(..., reason=...)]`; inline `const` moved to module scope (removes `items_after_statements`); all `#[allow(cast_possible_truncation)]` replaced with `try_from().unwrap_or()`; `#[allow(indexing_slicing)]` → `#[expect(indexing_slicing, reason=...)]`.
- **`query_logger.rs`** — `#[allow(many_single_char_names)]` and `#[allow(cast_possible_truncation)]` → `#[expect(...)]` with documented reasons (Howard Hinnant date algorithm; bounded values).
- **`storage/legacy/resolve.rs`** — spurious `#[allow(too_many_lines)]` removed (lint never fired); `#[allow(expect_used)]` × 2 → `#[expect(...)]` with invariant reasons; remaining `expect()` on non-empty slice replaced with `.ok_or_else(|| anyhow!(...))` for proper error propagation.
- **`engine/exec_show.rs`** — `#[allow(too_many_lines)]` → `#[expect(...)]`; `#[allow(unwrap_used)]` → `#[expect(...)]` (fast_path_ext invariant documented).
- **`ast/lang.rs`** — `#[allow(struct_excessive_bools)]` → `#[expect(...)]` on `LanguageConfig`; three test-helper functions' `#[allow(expect_used)]` → `#[expect(expect_used, reason = "embedded JSON is always valid")]`.
- **`ast/intern.rs`** — `#[allow(expect_used)]` → `#[expect(...)]` (overflow = programming error); two `#[allow(cast_possible_truncation)]` on `id as usize` → `usize::try_from(id).unwrap_or(usize::MAX)`.
- **`ast/enrich/numbers.rs`** — `#[allow(cast_possible_truncation)]` → `#[expect(...)]` (intentional `f64→i64` truncation documented).
- **`ast/index.rs`** — `#[allow(cast_possible_truncation)]` on `field_count as u16` → `u16::try_from(field_count).unwrap_or(u16::MAX)`; test module `#![allow(unwrap_used, expect_used)]` → `#![expect(unwrap_used, expect_used, reason = "test code")]`.
- **`transforms/diff.rs`** — four `#[allow(cast_possible_wrap, cast_sign_loss)]` blocks on byte-shift arithmetic → `isize::try_from(...).unwrap_or(isize::MAX)` / `usize::try_from(...).unwrap_or(0)` with named temporaries for clarity.

## [0.54.4] — 2026-05-24 — Columnar storage lint cleanup & `SymbolRow` API

### Fixed

- **`columnar_storage.rs`: eliminated all `#[allow]` suppressions** — proper fixes
  for each:
  - `unnecessary_wraps` on `fast_group_by_file` / `fast_group_by_kind`: changed
    return type from `Result<Vec<SymbolMatch>>` to `Vec<SymbolMatch>`; call sites
    wrapped in `Ok(...)` to match the outer `Result` context.
  - `cast_possible_truncation` (×2): replaced `as usize` / `as u32` with
    `try_from(...).unwrap_or(MAX)` — overflow is unreachable for real source files
    but now made explicit.
  - `too_many_lines` on `reindex_files`: suppression removed (function is under
    the threshold).
  - `too_many_lines` on `resolve_impl`, `find_symbols`, `warm_or_open`: replaced
    with `#[expect(..., reason = "...")]` documenting why splitting would harm
    readability.

- **`segment_builder.rs`: eliminated all remaining `#[allow]` suppressions** —
  proper fixes for each:
  - `missing_const_for_fn` on `Col::len()`: added `const`; `Vec::len()` has been
    const-stable since Rust 1.63 so the old workaround comment was outdated.
  - `cast_possible_truncation` on `cid_len`: replaced `as u8` with
    `u8::try_from(content_id.len().min(32)).unwrap_or(32u8)`; value is capped at
    32 so `try_from` always succeeds.
  - `cast_possible_truncation` on `row_count`: replaced with `#[expect(...,
    reason = "...")]`; `TryFrom` is not const-stable so the cast is required.
  - `too_many_arguments` on `emit_row` / `add_row`: replaced with `#[expect]` —
    superseded in the next commit by the `SymbolRow` refactor.
  - `too_many_lines` on `flush`: replaced with `#[expect(..., reason = "...")]`.
  - `expect_used` on `intern`: replaced with `#[expect(..., reason = "...")]`;
    panic on 4-billion-string overflow is intentional sentinel behaviour.

### Changed

- **`SegmentBuilder::emit_row` / `add_row` now accept `SymbolRow`** instead of
  7 positional arguments. The named struct makes call sites self-documenting,
  eliminates the `too_many_arguments` lint naturally, and ensures future column
  additions only touch the struct definition and its construction sites.
  All 10 affected files updated (`segment_builder.rs`, `build_context.rs`,
  `columnar_storage.rs`, `shadow_writer.rs`, `segment_reader.rs`, `mod.rs`,
  `segment_parity.rs`, `overlay_parity.rs`, `columnar_filter.rs`,
  `columnar_range.rs`).

## [0.54.3] — 2026-05-24 — Heredoc in all string positions; overlay safety hardening

### Added

- **Heredoc syntax now accepted in every `any_value` position** — previously
  `<<TAG...TAG` blocks were only valid on the `WITH` (replacement) side of
  `CHANGE` commands.  After this change heredoc works anywhere a string is
  accepted: `MATCHING` patterns, `WHERE`/`HAVING` predicate values, `IN` /
  `EXCLUDE` globs, `OF` symbol targets, aliases, etc.

  Example — match a multi-line pattern:
  ```sql
  CHANGE FILE 'src/lib.rs' MATCHING <<OLD
  fn foo() {
      todo!()
  }
  OLD WITH <<NEW
  fn foo() -> u32 { 42 }
  NEW
  ```

  Example — complex regex predicate without escaping:
  ```sql
  FIND symbols WHERE name MATCHES <<RE
  ^(get|set)_[a-z_]{3,}
  RE
  ```

### Fixed

- **`overlay_writer.rs`: silent `as` casts replaced with checked conversions**
  — removed all 5 `#[allow(clippy::cast_possible_truncation)]` suppressions.
  Added private `to_u32()`/`to_u16()` helpers that use `u{32,16}::try_from()`
  and return `io::Error(InvalidData)` on overflow, so corrupt or oversized data
  is rejected at write time instead of silently truncating.  `compute_blobs()`
  now returns `io::Result<ComputedBlobs>` and all callers propagate errors with
  `?`.  On-disk header constants are expressed as `u32` literals backed by
  compile-time `assert!` macros to keep them in sync with the `usize` originals
  in `overlay.rs`.

- **`overlay.rs`: removed 52 `#[allow]` suppressions** — replaced every blanket
  lint suppression with proper safe code:
  - 43 `indexing_slicing` → bounds-checked `.get()` with explicit error handling
  - 2 `cast_possible_truncation` → `u32::try_from()`
  - 2 `dead_code` → items removed or actually used
  - 1 `unsafe_code` → narrowed to `#[expect(unsafe_code)]` on the one call site
  - 1 `too_many_lines` → helper functions extracted
  - 1 `unwrap_used`/`expect_used` in test module → safe alternatives

### Implementation

- Grammar (`forgeql.pest`): `any_value` rule extended to
  `heredoc_literal | string_literal | bare_value`.
- Parser (`helpers.rs`): new `unwrap_any_value()` helper dispatches all three
  variants; `next_str()` delegates to it (one canonical extraction path).
- Parser (`clauses.rs`): `parse_predicate`, `in_clause`, and `exclude_clause`
  updated to use `unwrap_any_value` instead of the raw `unquote` call.

## [0.54.2] — 2026-05-24 — Python (PyTorch) golden test suite GP1–GP25

### Tests

- **Added Python/PyTorch golden test suite GP1–GP25** (`tests/golden.json`) — 25 new
  data-driven tests against a new `pytorch-andre.pytorch-frozen` source
  (2 953 280 symbols indexed).  Coverage includes:
  - Enrichment metrics on Python functions: `param_count`, `lines`, `branch_count`,
    `string_count`, `todo_count`, `unused_param_count`, `decl_far_count`,
    `return_count`, `recursion_count`, `name_length`, `condition_tests`
  - Pattern predicates: `MATCHES`, `LIKE` on function names (dunder methods,
    `__init__` family)
  - Numeric enrichments for Python: `num_format = 'hex'` / `'scientific'`,
    `shift_direction = 'left'`
  - Navigation: `SHOW outline`, `SHOW LINES`, `SHOW members`, `SHOW callees`
  - Aggregate queries: `GROUP BY file`, `GROUP BY fql_kind` within `torch/nn/**`
    and `torch/**` subtrees

## [0.54.1] — 2026-05-24 — `FIND files DEPTH` pipeline fix & MATCHES performance

### Fixed

- **`FIND files DEPTH N ORDER BY size DESC` returned wrong results** (`exec_show.rs`,
  `ast/query.rs`) — `ORDER BY` + `LIMIT` were applied *before* `group_files_by_depth`,
  so the pipeline selected a handful of large individual files from a single deep
  directory, computed `common_prefix_depth` on that tiny set, and then showed them all
  as shallow individual files instead of collapsing them into directory summaries.
  The fix moves `ORDER BY` / `OFFSET` / `LIMIT` to run on the already-grouped result.
  Directory summary JSON entries now also carry a `"size"` field (mirroring
  `total_size`) so numeric sort applies uniformly to both individual files and
  directory summaries.

- **`WHERE condition_text MATCHES '.{150,}'` (and similar `MATCHES` / `NOT MATCHES`
  predicates) caused severe CPU saturation** (`filter.rs`) — the regex was compiled
  inside the per-item retain closure, triggering millions of redundant compilations
  on large symbol tables (e.g. Linux kernel with 29 M+ symbols, 849 s wall time).
  The fix compiles the regex once per predicate before the retain loop.
  Pure min-length patterns (`.{N,}`) additionally bypass the regex engine entirely
  with a cheap `len >= N` byte-count check, yielding a further ~10× speedup for
  that common pattern class.

### Tests

- Added golden tests **`GFF8_depth1_top5_dirs_by_size`** and
  **`GFF9_depth2_top5_dirs_by_size`** (`tests/golden.json`) — assert that
  `FIND files DEPTH 1 ORDER BY size DESC LIMIT 5` and the DEPTH 2 variant return
  directory summaries (paths ending with `/`) with correct sizes, directly
  exercising the regression that was fixed above.

## [0.54.0] — 2026-05-23 — `FIND files` overlay fast path (all workspace files)

### Added

- **All workspace files tracked in the overlay** (`overlay_builder.rs`, `overlay_writer.rs`,
  `overlay.rs`) — FQOV schema bumped to **v8**, adding a `file_entries` blob that enumerates
  every regular workspace file that does **not** already have a symbol segment (images, docs,
  CMake scripts, Kconfig files, build artefacts, `.elf`/`.bin`/`.png` outputs, …).  Each
  file-only entry stores `(relative_path, file_size_bytes)` — no symbol rows, no AST data —
  so they cost approximately 20–30 bytes per file in the overlay.

  Impact on large repos (one-time per commit, paid at index-build time):

  | Repo         | Source files | Added file-only | Total overlay entries |
  |--------------|-------------|-----------------|----------------------|
  | Zephyr main  | 14 240       | 45 250          | 59 490               |
  | Linux main   | 64 083       | 29 614          | 93 697               |

  `RowPtr.segment_idx` values and the `ColumnarStorage.segments` alignment are
  unaffected — file-only entries live in their own blob separate from `segment_metas`.
  Old overlays (v7) are invalidated by the version bump and rebuilt once on the next
  query against a registered source.

- **`FIND files` overlay fast path — extended to all file types** (`exec_show.rs`,
  `columnar_storage.rs`, `storage/mod.rs`) —
  `FIND files WHERE extension = 'X' …` now resolves from the overlay for **any** extension
  once the overlay is (re)built with the current code.  On Zephyr this reduces latency from
  ~1–2 s to < 5 ms for queries like:

  ```sql
  FIND files WHERE extension = 'cmake' LIMIT 5
  FIND files WHERE extension = 'elf'   IN 'build/**'
  FIND files WHERE extension = 'png'   ORDER BY size DESC LIMIT 10
  FIND files WHERE extension = 'rst'
  ```

  The guard in `exec_show.rs` is backward-compatible: for an overlay built with **older**
  code (source files only), any extension absent from the overlay falls back to the
  filesystem walk automatically.  No `SCHEMA_VERSION` bump is required.

  Queries with no extension predicate (`ORDER BY size DESC`, exact-path lookups, `WHERE size > N`)
  continue to use the filesystem walk to remain correct with old overlays.

- **`StorageEngine::indexed_files()`** (`storage/mod.rs`) — new optional trait method (default
  `None`) that returns all indexed source files as typed `FileEntry` rows.

- **`ColumnarStorage::indexed_files()`** (`columnar_storage.rs`) — implementation that reads
  per-segment file sizes from the `index_files` mmap blob (zero syscalls) and patches dirty
  overlay segments (one `stat` per mutated file).  Now includes file-only entries automatically
  since it iterates all `overlay.segments()`.

### Tests

- All 8 GFF golden tests (`GFF1`–`GFF8`) confirmed correct:
  - `GFF1–GFF3`, `GFF7`, `GFF8` — indexed extensions → overlay fast path.
  - `GFF4` (`WHERE size > 50000`), `GFF5` (exact path) — no extension predicate → filesystem walk.
  - `GFF6` (`WHERE extension = 'rst'`) — on new overlays, fast path; on old overlays, fallback.

### Notes

- Future work: add `forgeql-lang-cmake`, `forgeql-lang-json`, `forgeql-lang-yaml` crates
  (backed by `tree-sitter-cmake` / `-json` / `-yaml`) to graduate those file types from
  file-only entries to full AST-indexed symbol segments.

## [0.53.4] — 2026-05-23 — Fix enrichment staleness in columnar storage after `CHANGE FILE`

### Fixed

- **RWTE / `ColumnarStorage::reindex_files`** (`columnar_storage.rs`) — After a `CHANGE FILE`
  mutation, `branch_count` and `max_condition_tests` were always absent in the next query result
  when the columnar backend was active (the default).  `reindex_files` was calling `index_file`
  on a fresh per-file `SymbolTable` but never invoking `post_pass()`, so `ControlFlowEnricher`
  never had a chance to compute and write its post-walk fields before the segment was serialised.
  A `post_pass` loop is now executed immediately after `index_file` inside `reindex_files`,
  mirroring what `SymbolTable::reindex_files` (legacy backend) already did correctly.

### Tests

- **RWTE00–RWTE30** — 31 new read/write transaction tests covering every enrichment field:
  `lines`, `param_count`, `return_count`, `goto_count`, `string_count`, `branch_count`,
  `max_condition_tests`, `has_todo`, `is_static`, `is_inline`, `is_recursive`, `has_cast`,
  `has_unused_param`.  Each test records a numeric or boolean baseline before mutation,
  applies two `CHANGE FILE` edits that trigger every enricher, asserts all 13 post-mutation
  values, then rolls back and verifies the baseline is restored.  RWTE27 and RWTE28
  (`branch_count` / `max_condition_tests`) were the TDD anchor tests that exposed the bug.
  All 31 pass.

## [0.53.3] — 2026-05-23 — Four query-correctness fixes + `LanguageConfig`-driven AST checks

### Fixed

- **GSB4 / SHOW body** (`body.rs`) — Body was clipped at the wrong end-line when the stored
  `enrichment["lines"]` value was stale (e.g. `k_sys_work_q_init` truncated to 3 lines instead of 15).
  `body.rs` now calls `first_absorbed_toplevel_in_compound()` live on the already-parsed AST node
  instead of trusting the indexed value.

- **GSMB2 / SHOW members** (`members.rs`) — Member classification for structs and classes was
  mis-labelling methods as fields and missing enumerators. Extracted `classify_member()` helper;
  `is_method_declaration` now uses `config.function_declarator()` instead of a hardcoded string.

- **GSC2 / SHOW callees** (`exec_show.rs`, `callees.rs`, `show.rs`) — Callee results were sorted
  lexicographically by default (`K_KERNEL_STACK_SIZEOF` before `k_work_queue_start`). The engine
  now injects `ORDER BY line ASC` when no explicit `ORDER BY` is given for `SHOW callees`, matching
  natural call-site order. `collect_callees_walk` returns `Vec<(String, usize)>` (name + 1-based
  call-site line) so each result carries its source location.

- **GFF8 / FIND files depth** (`exec_show.rs`) — `FileEntry.depth` was computed relative to the
  `IN` glob path instead of the repository root. It is now derived from
  `path.components().count()`, making `WHERE depth = N` consistent with the root-relative depth
  shown in results.

### Changed

- **`LanguageConfig`-driven kind checks** (`metrics.rs`, `members.rs`, `body.rs`) — Hardcoded
  tree-sitter node-kind strings (`"function_definition"`, `"compound_statement"`,
  `"field_declaration"`, `"function_declarator"`, `"init_declarator"`, `"declaration"`) replaced
  with `config.is_function_kind()`, `config.is_block_kind()`, `config.is_field_kind()`,
  `config.function_declarator()`, `config.is_init_declarator_kind()`, and
  `config.is_declaration_kind()`. C-specific literals (`"initializer_list"`,
  `"field_designator"`, `"storage_class_specifier"`) are retained with explanatory comments where
  no language-agnostic config equivalent exists.

### Tests

- Golden test suite expanded to 129 tests: `GFF1–GFF8` (FIND files), `GSL1–GSL5` (SHOW LINES),
  `GSB1–GSB4` (SHOW body), `GSCX1` (SHOW context), `GSO1` (SHOW outline), `GSC1–GSC2`
  (SHOW callees), `GSMB1–GSMB2` (SHOW members), `GSS1` (SHOW signature), `GST42–GST52`
  (enrichment / triage flags). All 129 pass.
- Bug-exercise regression tests added for the four query-correctness bugs above.

## [0.53.2] — 2026-05-23 — `forgeql-lang-c`: dedicated C language crate with `tree-sitter-c`

### Added

- **`forgeql-lang-c` crate** — New language crate for C source files (`.c`, `.h`) backed by `tree-sitter-c`.
  Previously, all C and C++ files were parsed by `tree-sitter-cpp`, which treats `class`, `template`,
  `namespace`, and other C++ keywords as reserved — causing `tree-sitter-cpp` to catastrophically mis-parse
  any C file that uses them as ordinary identifiers (GBUG11: `class` parameter in `hci_driver.c` turned a
  valid `switch` statement into a phantom anonymous class body, corrupting all symbols from that point).

- **`CLanguage` struct** implements `LanguageSupport` for C with:
  - `tree-sitter-c` grammar (no C++ keyword conflicts)
  - `c.json` configuration: C-only kind map (no templates, no OOP visibility, no named casts, no range `for`)
  - `CMacroExpander` for two-pass `#define` expansion
  - Full test suite: 7 unit tests covering `map_kind`, extension resolution, and negative assertions

- **`tree-sitter-c = "0.23"` workspace dependency** added.

### Fixed

- **GBUG11** — `.c` and `.h` files now route through `tree-sitter-c` instead of `tree-sitter-cpp`, eliminating
  the class-keyword parse corruption in Zephyr's `hci_driver.c` and any similar C file that uses C++ keywords
  as valid C identifiers.

### Changed

- **`forgeql-lang-cpp` extensions** — Removed `.c` and `.h` from `CppLanguage::extensions()` and `cpp.json`.
  C++ grammar now covers only `["cpp", "cc", "cxx", "hpp", "hxx", "ino"]`.

- **`ts-debug` tool** — `.c`/`.h` files now parsed with `tree-sitter-c`; `.cpp`/`.cc`/`.cxx`/`.hpp`/`.hxx`
  continue to use `tree-sitter-cpp`.

## [0.53.1] — 2026-05-22 — Enrichment bug fixes: `mixed_logic` MISRA semantics, negative-hex suffix, `fql_kind` for operator rows

### Fixed

- **`mixed_logic` now uses MISRA Rule 12.1 semantics** (`control_flow.rs`) — The previous check (`skeleton.contains("&&") && skeleton.contains("||")`)
  produced false positives whenever both operators appeared anywhere in the condition, even when one was fully parenthesised (e.g. `((a > b) || ((a == b) && !c))`).
  The new `detect_mixed_logic()` function uses `strip_outer_parens` + `split_top_level` to flag only the case where `&&` and `||` appear as *top-level operators*
  without explicit parentheses separating them (MISRA Rule 12.1). Six dedicated unit tests added.

- **Negative-hex literals no longer reported as float-suffixed** (`numbers.rs`) — `is_hex_digit_suffix` checked `lower.starts_with("0x")`,
  which fails for negative literals such as `-0xff` (starts with `"-"`). A leading `-` is now stripped before the `"0x"` prefix test,
  so `num_suffix` is no longer incorrectly set to `"f"` for values like `-0xff`, `-0x0007FFFF`, etc. Unit test added.

- **`fql_kind` populated for `compound_assignment` and `shift_expression` operator rows** (`cpp.json`) — `OperatorEnricher` already
  created `ExtraRow`s with `node_kind = "compound_assignment"` / `"shift_expression"`, but both were absent from the C++ `kind_map`,
  so `fql_kind` was always `""`. Added `"compound_assignment": "compound_assignment"` and `"shift_expression": "shift_expression"`
  to the `kind_map`; `FIND symbols WHERE fql_kind = 'compound_assignment'` and `fql_kind = 'shift_expression'` now return results.
  `map_kind` unit-test assertions added for both kinds.

## [0.53.0] — 2026-05-22 — Enrichment bitmaps; O(1) predicate prefiltering; DESC streaming; `index_files` overlay; zero-alloc FST

### Added

- **Phase 5: FQOV v7 Global Enrichment Bitmaps** — Upgraded the overlay format to schema version 7 (TOC count: 11). A new `enrich_bitmaps` blob stores `RoaringBitmap`s keyed by `"field=value"` for all enrichment attributes, built at overlay-write time by `overlay_builder.rs`. `prefilter_global` now intersects enrichment bitmaps for Eq/Bool/Gte/Gt/Lte/Lt predicates, shrinking the candidate set from 37k+ rows to ~50–500 rows before segment materialisation. Numeric fields use lexicographic-scan + parse; string/bool fields use exact key lookup.
- **Phase 4: `index_files` Table in Overlay (FQOV v6)** — Upgraded the overlay format to schema version 6 (TOC count: 10). A flat `u32` file-size array (`index_files_bytes`) is serialised alongside segment metadata, eliminating expensive disk-based directory walks for file-system query acceleration. Automated version up-conversion and runtime validation included.
- **Phase 3: Bounded DESC Streaming Fast-Path** — `stream_names_desc` and `stream_names_desc_kind_filtered` on `Overlay` use an in-memory bounded min-heap (`BinaryHeap<HeapEntry>`) over a forward FST walk to retain only the alphabetically largest N names in O(K) footprint — no segment files opened.
- **Phase 2: Zero-Allocation FST Stream Filtering** — Replaced per-name `RoaringBitmap` heap allocation with a zero-copy `&[u32]` slice via `decode_postings_slice` inside `stream_names_asc` and `stream_names_asc_kind_filtered`, eliminating thousands of heap allocations per query.
- **15 Strategic Golden Queries (GST1–GST15)** — Expanded `golden.json` with queries targeting deep AST attributes, data-flow metrics, unused parameters, shadow variables, duplicate conditions, recursive logic, and alphabetical limits.

## [0.52.0] — 2026-05-22 — `GROUP BY file` fast-path operational; internal constant hygiene
### Fixed

- **`GROUP BY file` fast-path predicate evaluation** — WHERE predicates were left in `no_group` and evaluated against grouped results (which lack per-symbol fields), causing golden tests G13, G17, G19 to return 0 rows. Predicates are now cleared before `apply_clauses` runs on the grouped output.
- **`GROUP BY file` fast-path dispatch** — `find_symbols` now dispatches to `fast_group_by_file` when `group_by_file_fast_path_eligible` is true, enabling the sub-second GROUP BY path introduced in Phase 1.

### Changed

- Internal filenames `.forgeql-columnar-delta` and `.forgeql-staging` are now referenced via `storage::columnar::DELTA_FILE_NAME` and `STAGING_DIR_NAME` module constants rather than hardcoded string literals in `git/mod.rs`.

## [0.51.0] — 2026-05-21 — Path acceleration fast-paths; GROUP BY sub-second; bounded top-K

### Added

- **Path-prefix segment skip (Phases 2–6)** — `FIND … IN 'path/**'` queries now skip all
  segments outside the matching path prefix.  Phase 2 sorts segments by `source_path` at
  build time (FQOV v4) so rows from each path prefix occupy a contiguous global row-ID
  range.  Phase 3 adds an O(1) `segment_row_range` lookup.  Phase 4 adds `path_seg_range`
  and `path_row_range` via binary search (O(log N), no FST blob needed).  Phase 5 restricts
  the segment loop to the matching range.  Phase 6 passes the row range into
  `prefilter_global` to clamp the kind/name bitmap intersection before any segment is
  opened.

- **`ORDER BY name ASC LIMIT N` FST stream fast-path (Phase 1)** — bare name-sorted queries
  with no WHERE predicates stream names directly from the in-memory FST; no segments are
  opened.  Phase 9 extends this to `WHERE fql_kind = X` queries via
  `stream_names_asc_kind_filtered`.

- **`GROUP BY file` and `GROUP BY fql_kind` sub-second fast-paths (Phases 0, 7, 9, 9b)** —
  `GROUP BY file` reads only `dedup_row_count` from segment metadata (zero segment I/O);
  `GROUP BY fql_kind` sums per-kind deduplicated counts from the kind bitmaps.  Whole-repo
  GROUP BY queries that previously took ~82 s now complete in under a second.

- **Deduplicated row counts in overlay (Phase 9b, FQOV v5)** — `SegmentRecord` gains
  `dedup_row_count: u32` computed at build time via canonical (name, fql_kind, line) set
  intersection.  Kind bitmaps are also deduplicated, eliminating the 17–18% overcounting
  from tree-sitter intra-file duplicate AST nodes.  `SCHEMA_VERSION` bumped 3 → 4 → 5;
  old overlays are detected and rebuilt automatically on first use.

- **Bounded top-K materialization (Phase 8)** — `ORDER BY field LIMIT K` queries (K ≤ 1000)
  use introselect (`slice::select_nth_unstable_by`, O(N) average) instead of a full sort.
  A running trim in `materialize_all` bounds peak memory to O(K) via `TOPK_OVER_FETCH = 4`.

### Fixed

- `exec_source.rs` warm-path now verifies `Overlay::open().is_ok()` before skipping the
  cold-rebuild path; a schema-version mismatch no longer silently loads a stale overlay.

- `apply_clauses` was re-applying `in_glob`/`exclude_glob` to synthetic `SymbolMatch`
  results (path = None) from GROUP BY fast-paths, dropping all rows when an IN clause was
  present.  Fast-path methods now strip those clauses from the `no_group` clone.
 — 2026-05-18 — Bug fix: LIMIT with enrichment/LIKE queries returned 0

### Fixed

- **`FIND … WHERE <enrichment> = '…' LIMIT N` returned 0 results** (`columnar_storage.rs`) —
  `materialize_all` applied a `fetch_cap = LIMIT+1` early-exit that counted raw
  materialized rows *before* `apply_clauses` ran.  Two scenarios triggered this:

  1. **Enrichment-only predicates** — segments without a posting blob for the
     queried field (e.g. `postings_is_recursive`) let ALL their rows pass
     through `prefilter_enrichment_postings`.  Those rows filled the cap
     immediately; `apply_clauses` then filtered them all away → 0 results even
     though matching rows existed in later segments.

  2. **`name LIKE` / `name MATCHES` with trigram false positives** — the trigram
     prefilter returns every row whose name *contains* the literal (e.g.
     `"alloc"` matches `memalloc_*` names), not just rows that satisfy the full
     LIKE pattern.  False positives from the first alphabetical segment exhausted
     the budget before genuine matches were reached.

  Fixed by applying the WHERE predicate filter *inside* the segment loop, before
  truncating to the remaining capacity.  The cap now counts only rows that
  actually pass the WHERE predicates, so `LIMIT N` reliably returns up to N
  matching results regardless of segment order.

## [0.50.12] — 2026-05-17 — Bug fixes: CSV enrichment string output and SHOW body line clipping

### Fixed

- **CSV `ORDER BY` enrichment string field showed `0`** (`compact.rs`, `result.rs`) —
  when the last sort column was a non-numeric enrichment string (e.g.
  `ORDER BY cast_style`, which yields values like `"c_style"`), the compact
  CSV renderer called `metric().to_string()`.  `metric()` tried to parse
  `metric_value` as `usize`, failed silently, and fell back to
  `usages.unwrap_or(0)` — always printing `0`.  Fixed by replacing `metric()`
  with a new `metric_str()` method that returns `metric_value` verbatim when
  set, then falls back to the `count` (GROUP BY) or `usages` integer only when
  no string value is present.

- **`SHOW body` returned only 3 lines for functions containing C99 subscript-designator local arrays**
  (`ast/enrich/metrics.rs`) — `first_absorbed_toplevel_in_compound` is a
  heuristic that detects when tree-sitter has mis-parsed a function and absorbed
  a subsequent file-scope declaration into the function body; when it fires it
  clips the enriched `lines` value to exclude the absorbed node.  The heuristic
  incorrectly fired on functions that contain a *legitimate* local variable
  declared as a `static const T arr[] = { [ENUM] = value, … }` C99
  subscript-designator array (e.g. `__get_dwarf_regnum_for_perf_regnum_powerpc`
  in the Linux kernel's `dwarf-regs-powerpc.c`), because that declaration has a
  multi-line `initializer_list` that superficially looks like an absorbed
  file-scope driver table.

  The guard condition `declaration_has_initializer_list` now requires the
  `initializer_list` to contain at least one `field_designator` node
  (`.member = value` struct member syntax).  Arrays initialised with
  subscript designators (`[N] = value`) or plain value lists no longer trigger
  the heuristic.  A new `initializer_list_has_field_designator` DFS helper
  handles arrays-of-structs where the `field_designator` is nested one level
  deeper.

### Tests

- **`metrics_lines_not_clipped_for_c99_designator_array`** — new integration test
  in `enrichment_integration.rs` backed by a `withC99DesignatorArray` fixture
  function in `tests/fixtures/enrichment_patterns.cpp`.  Asserts that a function
  whose body contains a C99 subscript-designator static array reports `lines >= 10`
  (the function has 12 lines; without the fix it reported `lines = 3`).

## [0.50.11] — 2026-05-17 — FQOV v3: zero-copy TOC-based overlay format

### Performance

- **FQOV v3 overlay format** (`crates/forgeql-core/src/storage/columnar/overlay_writer.rs`,
  `overlay_builder.rs`, `overlay.rs`) — the overlay file format was completely
  rewritten from bincode serialization to a hand-crafted, zero-copy binary layout:

  - **Header** (20 bytes): 4-byte magic `FQOV`, 4-byte schema version (`1`), 8-byte
    generation counter, 4-byte TOC entry count.
  - **TOC** (36 bytes × 9 entries): each entry has a 28-byte zero-padded name,
    4-byte offset, and 4-byte length — allowing random access to any blob without
    parsing the rest of the file.
  - **9 named blobs** laid out after the TOC: `row_table`, `kind_strings`,
    `kind_index`, `bitmap_data`, `trigram_index`, `name_fst`, `name_postings`,
    `segments`, `segment_strings`.

  `Overlay::open` now reads the header and TOC from the mmap, then wraps each blob
  as a range into the existing mmap — no heap copies, no bincode decode.
  `FstMap` and the name postings are served directly from the mmap via `MmapSlice`.

### Internal

- **`WriteV3Params` struct** (`overlay_writer.rs`) — groups the 9 write parameters
  to satisfy the ≤7 argument clippy limit and keep call sites readable.  The
  `write_v3` function now takes `params: &WriteV3Params<'_>`.
- **`compute_blobs` extracted** from `write_v3` — splits the blob-building logic
  into a separate function, keeping each function under 100 lines.
- **`HEADER_V3_LEN_U32` / `TOC_COUNT_U32` module-level consts** — replace inline
  `as u32` casts that triggered `clippy::cast_possible_truncation`.
- **Helper functions extracted from `Overlay::open`**:
  `parse_toc_entries`, `find_blob_ranges`, `validate_blob_layout`,
  `decode_segment_metas` — each under 30 lines; `open` itself is now ~68 lines.
- **`MmapSlice::new` declared `const fn`** (`segment_reader.rs`).

### Tests

- **Zephyr golden test: data-driven refactor + 14-query expansion** —
  `zephyr_golden.rs` was a hardcoded 4-query test; it is now a generic
  data-driven runner that reads `crates/forgeql/tests/golden.json` and
  executes each entry as a first-class assertion, making it trivial to add
  new golden cases without touching Rust.

  Coverage expanded from 4 → 14 queries:
  - `FIND symbols` with `ORDER BY`, `LIMIT`, exact-match `WHERE`, and
    enrichment filters (`param_count`, `language`, `name MATCHES`)
  - `SHOW LINES` plain, `WHERE text LIKE`, and `WHERE text MATCHES`
  - `FIND symbols GROUP BY fql_kind` with and without `HAVING`
  - `FIND symbols GROUP BY file`
  - `FIND files WHERE extension = …`

  All slow queries were scoped with `IN 'subdir/**'` to limit the candidate
  set; total suite runtime dropped from **~596 s → 27 s** (G11 and G12 each
  fell from 5+ minutes to under one second).

### Cache Invalidation

- **`ENRICH_VER` bumped from 7 to 8** (`crates/forgeql-core/src/storage/columnar/mod.rs`):
  The FQOV v3 binary layout is incompatible with the old bincode-serialized
  overlay files.  Existing v7 overlay caches are automatically invalidated and
  rebuilt on first use.

## [0.50.10] — 2026-05-17 — Overlay mmap quick wins (Phase 1)

### Performance

- **Overlay open no longer heap-copies the raw file bytes** — `Overlay::open` previously
  called `std::fs::read()`, allocating a heap `Vec<u8>` equal to the full overlay file
  (up to hundreds of MB on large repos).  It now uses `memmap2::MmapOptions::new().map()`
  instead; the OS demand-pages only the bytes touched by the bincode deserialiser and
  releases the mapping immediately after the payload is decoded.  Multiple sessions on
  the same commit SHA share OS page-cache pages rather than each holding a private copy.

- **Overlay FST constructed without cloning bytes** — after bincode deserialises
  `OverlayPayload`, the previous code called `FstMap::new(payload.name_fst_bytes.clone())`
  creating a second heap copy of the FST bytes.  The payload is now declared `mut` and
  `std::mem::take` moves the bytes directly into the FST, eliminating the extra allocation.
  The same pattern is applied to `name_postings_bytes` and the other payload fields.

- **SegmentReader FST is now zero-copy** — `SegmentReader::open` previously called
  `blob_slice(...).to_vec()` to allocate a heap buffer for the FST bytes before
  constructing the `FstMap`.  A new `MmapSlice` newtype (`pub(crate) struct MmapSlice`
  holding `Arc<Mmap>` + `start/end` range, implementing `AsRef<[u8]>`) allows
  `FstMap<MmapSlice>` to read FST data directly from the segment's existing mmap —
  zero extra heap allocation per segment on open.

## [0.50.9] — 2026-05-17 — Lazy session restore, checkpoint fix, and zephyr golden test

### Fixed

- **ROLLBACK checkpoint empty-stack bug** — after a full `ROLLBACK` (last checkpoint
  popped, `last_clean_oid = None`) the engine previously called `checkpoint_file::save()`
  with an empty stack, persisting a file where `expected = None`.  On the next server
  start `try_restore` compared `expected=None` against the real HEAD OID and emitted a
  spurious `"checkpoint file HEAD mismatch — discarding stale stack"` warning for every
  restored session.  Fixed: `exec_rollback` now calls `checkpoint_file::remove()` when
  the stack is fully drained, keeping the on-disk state consistent with the in-memory
  state.

### Performance

- **Lazy session restore at MCP startup** — `restore_sessions_from_disk()` previously
  called `use_source()` for every live worktree on disk, loading the full columnar index
  into RAM before the first request.  On a shared server with many developers this could
  exhaust all available memory at startup.  The function now only reads each worktree's
  `.forgeql-session` sentinel file and records a lightweight `PendingSession` entry
  (user, source, branch, alias, worktree name) — no index is loaded.  The columnar index
  is loaded lazily the first time the agent issues a `USE` command for that session.
  `session_count()` includes both active and pending sessions.  The pass-2 git metadata
  sweep was updated to protect pending worktrees from accidental pruning.

### Tests

- **Zephyr golden integration test** (`crates/forgeql/tests/zephyr_golden.rs`) — new
  Phase 0a test that opens a real MCP session against the frozen `zephyr-andre.zephyr-main`
  branch and asserts four golden values recorded on 2026-05-17:
  - Total `symbols_indexed = 2 720 018`
  - First 5 functions in `kernel/sched.c` ordered by line (thread\_runq→51,
    curr\_cpu\_runq→71, runq\_add→80, runq\_remove→88, runq\_yield→96)
  - `k_mutex_lock` → exactly 1 result: `field`, line 3525, `include/zephyr/kernel.h`
  - First function alphabetically → `AGC_IRQHandler`, line 64,
    `modules/hal_silabs/simplicity_sdk/src/blob_stubs.c`
  - Gated on `FORGEQL_DATA_DIR` env var; skips gracefully when unset.
  - Activate: `FORGEQL_DATA_DIR=/path/to/data cargo test --package forgeql --test zephyr_golden`

## [0.50.8] — 2026-05-16 — Bug fixes and dead-code removal

### Fixed

- **`mcp.rs` double-prepend bug** — `resolve_source()` and the budget map-key lookup in `exec_engine()` were manually prepending `user_id:` to a `session_id` that is already the full four-field token, producing a five-segment key that never matched any session entry. Both were silent: `resolve_source` always returned `"unknown"` (wrong log-file routing) and `budget_snap` was always `None` (missing budget lines). Fixed by using `session_id` directly as the map key.
- Stale user-visible strings in the MCP tool description and `⚠️ IMPORTANT` session hint referred to `session_id` as "the alias you chose" — updated to describe it as an opaque token to store verbatim.

### Internal

- **`RequestContext` removed** (`context.rs` deleted, `pub mod context` removed from `lib.rs`) — this was a dead abstraction for a planned Phase E permission system that is no longer on the roadmap. Every call site used `RequestContext::admin()` and every receiving parameter was `_ctx` (explicitly ignored). No production code read any field of the struct.
  - `ChangeFiles::plan()` and `plan_from_ir()` signatures simplified (drop the unused `ctx` parameter).

## [0.50.7] — 2026-05-17 — Self-describing session tokens and `execute()` takes `Option<&SessionCoords>`

### Internal

- **`SessionCoords::to_session_id()`** — encodes all four identity fields (`user:source:branch:alias`) into an opaque token; the single encoding point for session identity.
- **`SessionCoords::from_session_id()`** — decodes a token back into `SessionCoords`; uses `splitn(4, ':')` so alias may contain `':'`.
- **`SessionCoords::map_key()`** now delegates to `to_session_id()` — map key and external token are always the same value, making the `HashMap<String, Session>` fully self-describing.
- **`ForgeQLEngine::execute()`** signature changed from `session_id: Option<&str>` to `coords: Option<&SessionCoords>` — the engine receives the full identity struct and never reconstructs it from raw strings.
- Entry-point callers (`mcp.rs::exec_engine`, `execute.rs::execute_and_print`) now decode the incoming session token via `from_session_id()` before calling `execute()`.
- Test helpers in `exec_session.rs` (`register_local_session`, `register_local_session_for`, `register_local_session_with_columnar`) build `SessionCoords` directly and return `coords.to_session_id()` instead of bare aliases.
- Lookup helpers (`init_session_budget`, `install_columnar_for_session`, `session_has_columnar`, `session_index_stats_rows`) now use `session_id` directly as the map key (it is the full token).
- **Fixes the "alias already bound to source X" error**: `map_key()` previously encoded only `user:alias`, making the same alias across different sources collide. The four-field key makes each `(user, source, branch, alias)` tuple unique.
- All integration and unit tests updated to pass `Option<&SessionCoords>` to `execute()` and to decode session tokens before use.
- `budget_status()` call sites updated to pass the full token directly instead of manually constructing `"user:alias"`.
- **`SessionCoords::anonymous()` removed** — the migration it was guarding has happened; all construction sites now call `SessionCoords::new(auth(AuthContext::Tester), ...)`. Tests in `coords.rs` updated accordingly; hardcoded `"anonymous"` strings in expected values replaced with `auth(AuthContext::Tester)`.
- Fixed stale doc-table in `coords.rs` module comment (`Session map key` column now shows the correct four-field format).

## [0.50.6] — 2026-05-16 — Introduce `auth()` as single source of truth for user identity

### Internal

- **New `forgeql_core::auth` module** (`crates/forgeql-core/src/auth.rs`):

  Introduces `AuthContext` (enum: `Mcp`, `Cli`, `Session`, `Tester`) and
  `pub const fn auth(context: AuthContext) -> &'static str`.  The string
  `"anonymous"` now appears **exactly once** in the entire codebase — as the
  return value of `auth()` for production contexts.  `"fql_tester"` is
  returned for `AuthContext::Tester`, making test sessions completely
  distinguishable from production sessions in logs and on disk.

- **Entry-point birth points** (`crates/forgeql/src/mcp.rs`,
  `crates/forgeql/src/execute.rs`, `crates/forgeql/src/session.rs`):

  Each entry point now calls `auth(AuthContext::X)` exactly once and passes
  the resulting `user_id` variable everywhere else.  No `"anonymous"` literal
  appears outside of `auth()`.  When real authentication is added, only
  `auth()` needs to change — the rest of the call graph is already wired.

- **Test helpers use `AuthContext::Tester`**
  (`crates/forgeql-core/src/engine/exec_session.rs`):

  `register_local_session`, `register_local_session_with_columnar`,
  `init_session_budget`, `install_columnar_for_session`, `session_has_columnar`,
  `session_index_stats_rows` — all test helpers now compute the session map key
  via `auth(AuthContext::Tester)` = `"fql_tester"` instead of a hardcoded
  `"anonymous"` literal.  A new `register_local_session_for(user_id, path)`
  helper is added for tests that exercise a specific entry-point auth context
  (e.g. the MCP unit tests).

  Session restore fallback in `restore_sessions_from_disk()` uses
  `auth(AuthContext::Session)` for old sentinels that pre-date the `user=`
  field.

- **All integration and unit tests updated** to import
  `forgeql_core::auth::{auth, AuthContext}` and call
  `engine.execute(auth(AuthContext::Tester), ...)` instead of the literal
  `"anonymous"`.  `budget_status` key format strings updated to match.

- **Clippy fixes**: `auth()` is declared `const fn`; redundant `.clone()` on
  `session_id` in `exec_source.rs` removed.

## [0.50.5] — 2026-05-16 — Wire `SessionCoords` into `exec_source.rs`

### Internal

- **`SessionCoords` now drives all session identity derivations in `use_source()`**
  (`crates/forgeql-core/src/engine/exec_source.rs`):

  - **Validation** (`alias ≠ branch`): delegated to `SessionCoords::validate()` instead of an
    inline `if as_branch == branch` check.
  - **Budget-branch key**: delegated to `SessionCoords::budget_branch()` (trunk branches key
    by alias; feature branches key by branch name).
  - **`"anonymous"` user**: the hardcoded literal in `Session::new()` is replaced by
    `&coords.user`, so the single migration touch-point is `SessionCoords::anonymous()` at
    construction time.
  - **Worktree dir name, git branch, worktree path**: derived exclusively through
    `coords.worktree_dir()`, `coords.git_branch()`, and
    `SessionCoords::worktrees_root(&data_dir).join(&wt_name)`.  The inline
    `safe_source / safe_branch / safe_alias / format!(...)` block is removed.
    The git-branch format is now `fql/{user}/{source}/{branch}/{alias}` (was
    `fql/{branch}/{alias}`); the additional segments make it globally unique
    across users and sources.

- **Cross-source alias collision is now a hard error** instead of a silent eviction.
  `USE src-b.main AS 'r'` while alias `r` is already bound to `src-a` now returns
  `ForgeError::InvalidInput` with a clear message directing the agent to pick a
  different alias or run `DROP SESSION 'r'` first.

- **All 5 ad-hoc `data_dir.join("worktrees")` call-sites replaced** with
  `SessionCoords::worktrees_root(&data_dir)` across:
  - `src/engine/exec_source.rs` (worktree path construction)
  - `src/engine.rs` (`ForgeQLEngine::new()` mkdir)
  - `src/engine/warm.rs` (background warmer worktree path)
  - `src/engine/tests.rs` (unit test assertion)
  - `tests/reconnect_dirty.rs` (integration test setup)

## [0.50.4] — 2026-05-16 — Eager session restore at startup: replace `prune_orphaned_worktrees` + `try_auto_reconnect`

### Internal

- **`restore_sessions_from_disk()` replaces `prune_orphaned_worktrees()` + `try_auto_reconnect()`**
  (`crates/forgeql-core/src/engine/exec_session.rs`, `crates/forgeql/src/runner/mcp_stdio.rs`,
  `crates/forgeql-core/src/engine.rs`):

  The previous architecture had two problems:
  1. `prune_orphaned_worktrees` contained a latent bug in its live-session guard: it built
     `live_ids` from session map keys (bare alias strings) but compared them against git
     worktree directory names (`source.branch.alias`) and `wt.name` values — these never
     match, so in-memory sessions were never protected from accidental pruning.
  2. `try_auto_reconnect` ran on every request for an unknown session ID, triggering a
     full disk scan and git-repo traversal on first use after a server restart.

  The replacement is a single `restore_sessions_from_disk(&mut self)` called **once** at
  MCP server startup (before the engine is wrapped in `Arc<Mutex>` and before accepting
  requests).  It scans `<data_dir>/worktrees/`, prunes TTL-expired worktrees (using correct
  `live_wt_names` built from `sessions.values().map(|s| s.worktree_name.as_str())`), and
  restores all warm sessions into the in-memory map via `use_source()` — the same path taken
  by an explicit `USE` command.  After startup, `require_session` is a pure O(1) map lookup.

  A private `prune_single_worktree()` helper is extracted to avoid duplicating the
  remove-worktree-dir + remove-git-metadata sequence.

- **Extended `.forgeql-session` sentinel file** (`crates/forgeql-core/src/session/mod.rs`):

  The sentinel file written by `Session::touch()` into each worktree directory previously
  stored a bare Unix timestamp integer on a single line.  It now uses a `key=value` format:

  ```
  timestamp=1747123456
  source=pisco-firmware
  branch=main
  alias=refactor
  user=anonymous
  ```

  The old bare-integer format is still accepted (backward compat: the parser falls back to
  treating a non-`key=value` line as the timestamp when no `timestamp=` key has been seen).

  A new public `SessionSentinel` struct and `read_sentinel()` function replace the old
  `read_last_active()`.  `restore_sessions_from_disk` uses the `source`/`branch`/`alias`
  fields to restore sessions without git-repo traversal or directory-name parsing.

## [0.50.3] — 2026-05-16 — Introduce `SessionCoords`: single source of truth for session identity

### Internal

- **New `SessionCoords` struct** (`crates/forgeql-core/src/session/coords.rs`):
  All session identity derivations — the session map key, git session-branch name,
  worktree directory name, and worktree filesystem path — are now computed from a
  single `SessionCoords { user, source, branch, alias }` value.

  Previously these four strings were derived independently at each call-site
  (`exec_source.rs`, `exec_session.rs`, `engine.rs`, `warm.rs`) with slightly
  different formatting rules, making it easy for them to diverge silently.

  Key methods:
  - `SessionCoords::anonymous(source, branch, alias)` — default constructor
    (`user = "anonymous"`); change only this call-site when real auth lands.
  - `map_key()` → `"{user}:{alias}"` — future session `HashMap` key (scopes alias
    per user, eliminating cross-user collisions).
  - `git_branch()` → `"fql/{user}/{source}/{branch}/{alias}"` — globally unique
    git branch name (adds `user` and `source` segments missing from the old format).
  - `worktree_dir()` → `"{source}.{safe_branch}.{alias}"` (slashes in branch
    names replaced with dashes to keep the directory flat).
  - `worktree_path(data_dir)` → `data_dir/worktrees/{user}/{worktree_dir}`.
  - `worktrees_root(data_dir)` / `user_worktrees_root(data_dir, user)` — typed
    accessors replacing five ad-hoc `data_dir.join("worktrees")` call-sites.
  - `is_sha_ref()` — heuristic predicate to distinguish branch names from short
    SHA prefixes; gates the `revparse_single` code path in `worktree::create`.
  - `budget_branch()` — trunk-vs-feature budget logic extracted from
    `exec_source.rs`.
  - `validate()` — alias ≠ branch guard (alias must differ from the branch name).
  - `from_dir_name()` — inverse parse of `worktree_dir()` used by
    `try_auto_reconnect`.

  32 unit tests cover all methods including SHA detection, slash-to-dash
  replacement, cross-user isolation, cross-source isolation, and roundtrip parsing.

  This is a prerequisite for PR 2 (wiring `SessionCoords` into `exec_source.rs`
  to harden the existing silent session alias collision bug).

## [0.50.2] — 2026-05-15 — Fix `is_magic` false positives: blanket 0/1/-1 exclusion removed; numbers in string literals excluded

### Bug Fixes

- **`is_magic` no longer blanket-excludes `0`, `1`, and `-1`**
  (`crates/forgeql-core/src/ast/enrich/numbers.rs`):
  The previous implementation unconditionally suppressed `is_magic` for values in
  `{-1, 0, 1}`, even in fully semantic comparison contexts such as
  `if (status == 1)` or `return -1`. These are classic magic numbers and must be
  flagged. The blanket exclusion is removed. The only remaining exemptions are:
  - **Named-constant context** (`init_declarator`, `enumerator`, `preproc_def`):
    the literal is defining a constant, not using an opaque value.
  - **Zero in a subscript expression** (`array[0]`): first-element access is a
    universal structural idiom with no domain-specific meaning.

- **Numbers inside string literals are no longer indexed**
  (`crates/forgeql-core/src/ast/index.rs`, `crates/forgeql-core/src/ast/enrich/mod.rs`,
  `crates/forgeql-core/src/ast/enrich/numbers.rs`, `crates/forgeql-core/src/ast/lang.rs`):
  tree-sitter-cpp can emit phantom `number_literal` nodes (and `unary_expression`
  wrapping them) for digit sequences inside string content — e.g.
  `"0 for layer 2 (default), 1 for layer 3+4"` produced spurious `is_magic='true'`
  rows for every digit. The fix introduces a reusable `inside_literal: bool` field
  in `EnrichContext`, maintained O(1) by a `literal_depth` counter in
  `collect_nodes` that increments on descent into an opaque string or comment node
  and decrements on ascent. `NumberEnricher` checks `ctx.inside_literal` as its
  first guard; other enrichers with similar needs can use the same flag.
  `LanguageConfig` gains an `is_opaque_string_kind()` predicate that returns `true`
  only when `string_content_raw_kind` is set (C/C++, Rust), ensuring Python
  f-string interpolations — which embed real expressions inside `string` nodes —
  are not affected.

### Cache Invalidation

- **`ENRICH_VER` bumped from 6 to 7** (`crates/forgeql-core/src/storage/columnar/mod.rs`):
  The `is_magic` field semantics changed (values that were `'false'` are now
  `'true'` in comparison/argument contexts). Existing v6 segment caches are
  automatically invalidated and rebuilt on first use.

## [0.50.1] — 2026-05-15 — Fix `cast_safety` always emitting `'unsafe'` for named C++ casts and Rust `as`-casts

### Bug Fixes

- **`cast_safety` now correctly classifies named C++ casts and Rust `as`-casts**
  (`crates/forgeql-lang-cpp/config/cpp.json`, `crates/forgeql-lang-rust/config/rust.json`,
  `crates/forgeql-core/src/ast/enrich/casts.rs`, `crates/forgeql-core/src/ast/lang.rs`,
  `crates/forgeql-core/src/ast/lang_json.rs`):
  Previously every cast — including `static_cast<T>()`, `dynamic_cast<T>()`, and
  Rust `as`-casts — was reported as `cast_safety='unsafe'`. Three root causes were fixed:

  1. **Named C++ casts not detected at all**: tree-sitter-cpp 0.23 parses
     `static_cast<T>(x)` as a `call_expression` containing a `template_function`
     node, not as a dedicated `static_cast_expression` node. The `CastEnricher`
     only walked raw cast nodes and therefore never saw named casts. A new
     `named_casts` map in `LanguageConfig` (populated from `cpp.json`) and a
     companion `detect_named_cast_row()` path in `CastEnricher` now recognise
     `call_expression` + `template_function` pairs whose function name matches a
     known cast keyword (`static_cast`, `dynamic_cast`, `const_cast`,
     `reinterpret_cast`) and emit a synthetic cast enrichment row with the correct
     `cast_style` and `cast_safety`.

  2. **Incorrect safety for Rust `as`-casts**: the Rust config mapped the `as`
     cast kind to `'unsafe'`. Rust `as` is a checked, non-panicking numeric
     coercion that is never unsafe in safe code; it is now classified as
     `'moderate'` (may truncate or lose precision, but does not violate memory
     safety).

  3. **Prefilter not covering `call_expression`**: the storage-layer prefilter that
     maps `cast_safety` filter values to candidate node kinds was updated to include
     `call_expression` alongside the existing `cast_expression` and `as_expression`
     kinds, so `WHERE cast_safety='safe'` index scans now reach named-cast nodes.

  **Classification after fix:**

  | Cast form | `cast_style` | `cast_safety` |
  |---|---|---|
  | C-style `(T)x` | `c_style` | `unsafe` |
  | `reinterpret_cast<T>()` | `reinterpret_cast` | `unsafe` |
  | `const_cast<T>()` | `const_cast` | `moderate` |
  | `static_cast<T>()` | `static_cast` | `safe` |
  | `dynamic_cast<T>()` | `dynamic_cast` | `safe` |
  | Rust `x as T` | `as_cast` | `moderate` |

  Verified against `pisco-firmware`: 61 `safe` and 96 `unsafe` casts correctly
  classified; previously all 157 were reported as `unsafe`.

## [0.50.0] — 2026-05-15 — Single-file `.fqsf` segment format (65× fewer files, 25× fewer VMAs)

### Breaking Changes

- **Segment storage format v6**: Columnar segments are now stored as single `.fqsf`
  binary files (`<segments>/<provider>-v6/<2c>/<hex[2:]>.fqsf`) instead of per-file
  directories containing ~65 individual `.bin` files. `ENRICH_VER` bumped from 5 to 6;
  existing v5 segment caches are automatically invalidated and rebuilt on first use.

### Performance

- **65× fewer files**: replaces ~4.5 M per-segment directories (≈65 `.bin` files each)
  with ~70 K `.fqsf` files on a Zephyr RTOS repository index
- **25× fewer VMAs**: one `Arc<Mmap>` per segment file instead of ~25 separate mmaps
  per segment, substantially reducing `/proc/<pid>/maps` pressure
- **Atomic writes**: segments are written to a `.tmp.<stem>.<pid>.fqsf` file and then
  renamed into place; concurrent writers safely race without corruption
- **4-byte blob alignment**: all blobs within `.fqsf` files are padded to 4-byte
  boundaries, enabling zero-copy `bytemuck::cast_slice` on mmap data

### Implementation Notes

- New format wire layout: `FQSF` magic (4 bytes), version `u32`, entry_count `u32`,
  TOC (entry_count × 64 bytes: 56-byte name + `u32` offset + `u32` length), then
  4-byte-aligned blob data sections
- `promote_segment` simplified from recursive `copy_dir_all` to `std::fs::copy`
- Staging GC (`gc_orphaned_staging`) updated to match `.fqsf`-suffixed filenames
- `encode_zone_maps` simplified to return `Vec` directly (was `Result<Vec, _>`)

### Bug Fixes

- **`SHOW body` now rejects non-function symbols in both legacy and columnar backends**: previously, `SHOW body OF 'some_struct'` could silently return a random enclosing function instead of an error. Both `resolve_body_symbol` paths now filter candidates to function-like kinds (`function`, `method`, `constructor`, `destructor`, `macro`). Member declarations (`fql_kind="field"`) that carry a `body_symbol` redirect (C++ out-of-line definitions set by `MemberEnricher`) continue to work. Non-function names now produce an actionable error: `'X' is not a function (found fql_kind: [struct]). Use FIND symbols WHERE name = 'X' to locate the definition, then SHOW LINES n-m OF 'file' to read it.`
- **Cross-language ambiguity check extended to `SHOW body`**: `resolve_body_symbol` in the legacy backend now applies the same cross-language guard that `resolve_symbol` has — if a name exists in multiple languages, an explicit `WHERE language = '...'` or `IN '*.ext'` clause is required.

## [0.49.10] — 2026-05-14 — Fix inflated `lines`, `return_count`, `goto_count`, `string_count`, `throw_count` for misparsed C/C++ functions

### Bug Fixes

- **Inflated metrics for tree-sitter-c misparsed function bodies**: the same
  tree-sitter-c brace-imbalance misparse documented in 0.49.9 (Bug 4) also
  inflated several numeric enrichment fields for the affected functions.
  Twelve driver functions in Zephyr RTOS were confirmed across two absorption
  patterns:

  | Function | File | Old `lines` | New `lines` | Factor |
  |---|---|---|---|---|
  | `uart_ns16550_init` | `drivers/serial/uart_ns16550.c` | 1084 | 102 | ×11 |
  | `process_events` | (various) | 982 | ~97 | ×10 |
  | `gpio_pca_series_debug_dump` | `drivers/gpio/gpio_pca_series.c` | 1032 | 108 | ×10 |
  | `i2c_mchp_isr` | `drivers/i2c/i2c_mchp_sercom_g1.c` | 921 | 40 | ×23 |
  | `spi_max32_transceive` | `drivers/spi/spi_max32.c` | 884 | 203 | ×4 |
  | `flash_stm32_check_status` | `drivers/flash/flash_stm32h7x.c` | 590 | 86 | ×7 |
  | `dma_esp32_config_descriptor` | `drivers/dma/dma_esp32_gdma.c` | 515 | ~91 | ×6 |
  | `adc_max32_start_channel` | `drivers/adc/adc_max32.c` | 475 | 30 | ×16 |
  | `tcan4x5x_reset` | `drivers/can/can_tcan4x5x.c` | 342 | 46 | ×7 |
  | `virtconsole_poll_in` | `drivers/serial/uart_virtio_console.c` | 247 | 46 | ×5 |

  **Root cause**: tree-sitter-c/C++ evaluates all branches of `#if`/`#elif`/`#else`
  simultaneously; a brace imbalance in one branch causes a function body to absorb
  sibling function definitions and/or file-scope driver-table declarations that
  follow in the same translation unit.

  **Fix — `return_count`, `goto_count`, `string_count`, `throw_count`**
  (`crates/forgeql-lang-cpp/config/cpp.json`):
  Added `"function_definition"` to `nested_function_body_kinds`. The bounded DFS
  (`count_descendants_by_kind_bounded`) already stops at every entry in this list;
  adding `function_definition` makes it stop at absorbed siblings exactly as it
  stops at lambdas. No Rust code change required for these fields.

  **Fix — `lines`** (`crates/forgeql-core/src/ast/enrich/metrics.rs`):
  Added `first_absorbed_toplevel_in_compound()`. For each `function_definition`,
  the helper DFS-walks the AST subtree and clips `end_row` at the first absorbed
  file-scope node. Three node kinds are detected as absorbed:

  1. **`function_definition`** direct child of a `compound_statement`, or found
     inside a `preproc_ifdef` / preprocessor block anywhere in the subtree —
     the "swallowed sibling function" pattern.  This covers both the simple case
     (sibling functions directly in the outer `compound_statement`) and the
     common Zephyr pattern where sibling functions live inside an
     `#ifdef CONFIG_…` block that itself became a child of the misparsed body.
     When a `function_definition` is encountered in the recursion it is recorded
     (its start row contributes to the minimum clip point) but its body is not
     descended, preventing false positives from the sibling's own content.

  2. **`declaration`** direct child of a `compound_statement` that spans multiple
     lines and contains an `initializer_list` — the "swallowed struct initializer"
     pattern for correctly-parsed declarations (`static const struct foo_driver_api
     api = { .poll_in = bar, … };`). Single-line local declarations are excluded
     by the multi-line guard.

  3. **`ERROR`** node with `storage_class_specifier` as its first named child —
     tree-sitter-cpp 0.23.x fails to parse macro-as-type declarations such as
     `static DEVICE_API(gpio, name) = { … }` and emits `ERROR` instead of
     `declaration` (the macro call in type position confuses the grammar).
     Guard: the ERROR must span multiple lines and its first named child must be
     `storage_class_specifier` (`static`, `extern`, etc.), which uniquely
     identifies this pattern in practice within a function body.

  Regression test `metrics_lines_not_clipped_for_clean_function` verifies that
  a correctly-parsed function (`multiReturn`, 5 lines) retains its exact line
  count (i.e. `first_absorbed_toplevel_in_compound` returns `None`).

  **`branch_count` is unaffected**: the `ControlFlowEnricher` binary-search
  post-pass correctly attributes control-flow nodes to their real enclosing
  function even for misparsed bodies.

- **`SHOW body` `end_line` now uses enriched `lines` as single source of truth**
  (`crates/forgeql-core/src/ast/show/body.rs`):
  Previously `show_body()` derived `end_line` from `fn_node.end_position().row + 1`
  (the raw tree-sitter span) independently of the enrichment pipeline, so even
  after the `lines` fix above, `SHOW body OF 'gpio_pca_series_debug_dump' DEPTH 0`
  still reported the header `798-1829` while `metadata.lines=108`.
  `show_body()` now reads `enrichment["lines"]` and computes
  `end_line = fn_start_line + lines_count`, falling back to the raw span only
  when no enrichment is available. The emitted lines array is also clipped to
  this boundary. For clean functions `fn_start_line + enriched_lines ==
  fn_node.end_position().row + 1` exactly, so all existing tests are unaffected.

- **Cache invalidation**: `ENRICH_VER` bumped 3 → 5 (via intermediate 4 during
  development). The columnar segment namespace changes to `*-v5/`, forcing all
  segments to be rebuilt on the next `USE` command. Old `*-v3/` and `*-v4/`
  directories are orphaned and can be removed manually. (`CURRENT_VERSION` for
  the legacy `.forgeql-index` is left unchanged — that file is no longer written
  when a columnar build context is active.)

## [0.49.9] — 2026-05-14 — Fix RecursionEnricher false positives for non-recursive functions

### Bug Fixes

- **`RecursionEnricher` false positives — Bug 4**: `count_self_calls` now stops
  at nested `function_definition` nodes instead of recursing into them.  This
  fixes two overlapping scenarios:
  1. **Genuine nested functions** (GNU C, Python, closures): a call from an
     inner function back to the outer one is mutual recursion, not direct
     self-recursion, and was incorrectly counted as a self-call.
  2. **tree-sitter misparse**: certain C files with a `#if`/`#elif`/`#else`
     block containing a `goto` label cause tree-sitter-c to extend a
     `function_definition` body beyond its real closing `}`.  The inflated body
     contained several sibling function definitions (which are themselves correct
     separate nodes), each calling the outer function — all were wrongly counted
     as self-calls.  Confirmed in `drivers/spi/spi_max32.c` from Zephyr RTOS:
     `spi_max32_transceive` (200 lines, not recursive) was reported as
     `is_recursive = true` with `recursion_count = 4`.

  Added regression test `recursion_called_by_many` with a fixture function
  (`calledByMany`) that is called by three other functions in the same file and
  must not be flagged as recursive.



### Performance

- **`ScopeEnricher` O(sibling_count) → O(1)**: `enrich_row()` called
  `ctx.node.parent().is_some_and(|p| is_root_kind(p.kind()))` to distinguish
  file-scope from local-scope declarations. Replaced with
  `ctx.language_config.is_root_kind(ctx.parent_kind)` — a direct read from the
  cursor-walk stack added in 0.49.7. Semantics are identical (`parent_kind` is
  `""` when there is no parent, and `is_root_kind("")` returns false).

### Docs / internal

- Added a **performance contract** doc comment to the `NodeEnricher::extra_rows`
  default method explicitly prohibiting `ctx.node.parent()` calls inside that
  hot path and pointing implementors to `ctx.parent_kind` as the safe alternative.
- Audited all 18 enrichers for O(n²) exposure: the only remaining `.parent()`
  calls are in `enrich_row()` (not `extra_rows()`), bounded to named/recognized
  nodes that do not appear as 150k-wide siblings in real code.

## [0.49.7] — 2026-05-14 — Eliminate sequential SymbolTable merge, ShadowWriter double-read, and O(n²) NumberEnricher

### Performance

- **O(n²) → O(n) `NumberEnricher` fix — 6× cold-build speedup on Zephyr RTOS**:
  `NumberEnricher.extra_rows()` called `ctx.node.parent()` for every
  `number_literal` node to check whether the literal lived inside a named-constant
  context. In tree-sitter 0.25 `ts_node_parent` scans all preceding siblings by
  byte position — O(sibling_count) per call. A single `initializer_list` in
  `model.h` has ~150 000 children; summing 0…150 000 yields ~11 billion sibling
  scans → 213 seconds blocked in that one file. Fix: a `parent_kind_stack:
  Vec<&'static str>` maintained inside `collect_nodes()` tracks the cursor-walk
  parent kind O(1) (push on `goto_first_child`, pop on `goto_parent`). The stack
  head is exposed as `EnrichContext::parent_kind`, and `NumberEnricher` now reads
  `ctx.parent_kind` instead of calling `ctx.node.parent()`. Result on Zephyr RTOS
  (14 234 C files): **cold build 4 m 07 s → 0 m 41 s (6× faster)**; `model.h`
  alone 213 447 ms → 1 083 ms (197× faster).

- **Columnar inline fast-path** eliminates two sequential CPU/I/O bottlenecks
  that were visible on the CPU/disk monitor as a long flat single-core period
  followed by a second disk read burst:
  - **Sequential `SymbolTable` merge** — previously `SymbolTable::build` merged
    14 000+ per-file tables into one via `.reduce()`, running `reassign_intern_ids`
    and rebuilding all secondary indexes sequentially (~2 min wall time on Zephyr
    RTOS). The columnar fast-path now takes a `par_iter().for_each()` branch that
    runs per-file `post_pass` enrichment inline (control-flow, redundancy — both
    intra-file, so quality is identical) and writes the segment to disk via the
    `SegmentBuildCtx` emit-fn, then drops the per-file table without merging.
  - **ShadowWriter double-read** — `warm_or_open` previously called
    `ShadowWriter::new(table, …)` which re-read all 14 000+ source files from
    disk to compute content-IDs and write segments, duplicating the I/O already
    done during `SymbolTable::build`. With the inline fast-path the
    `prebuilt_segment_map` is propagated from `LegacyMemoryStorage` directly to
    `OverlayBuilder`, skipping `ShadowWriter` entirely (no second disk burst).
  - **Measured on Zephyr RTOS (14 234 C files, ~2.7 M rows)**: cold rebuild
    real time 3 m 28 s vs 3 m 45 s before (−17 s), CPU user time 6 m 3 s vs
    7 m 10 s before (−67 s). CPU graph shows ONE multi-core burst instead of
    burst → single-core flat → second burst.

### Internal

- `SegmentBuildCtx.provider_id` widened from `&'static str` to `String`.
- `InlineCtxState` struct added to `columnar/build_context.rs` (holds shared
  `Mutex<HashMap<PathBuf, Vec<u8>>>` segment map and `Mutex<BTreeSet<String>>`
  column set, both populated by the rayon parallel loop).
- `LegacyMemoryStorage.prebuilt_segment_map` field added for passing the inline
  segment map from `build_index` to `warm_or_open`.

## [0.49.6] — 2026-05-14 — Fix stale segments, stale worktree metadata, and skip legacy index write

### Fixed

- **`is_valid_segment`** previously only checked the FQSG magic bytes, allowing
  stale segments from older builds (same `ENRICH_VER` path, different column
  layout) to pass the guard. `ShadowWriter` kept them intact; `OverlayBuilder`
  then failed to open them with "mmapping col_fql_kind_id.bin". The check now
  also validates `SCHEMA_VERSION` and verifies that every core column file is
  exactly `row_count × 4` bytes, so mismatched segments are always overwritten.
- **`worktree::create`** failed with `"failed to make directory '…/worktrees/<name>': directory exists"`
  when a previous session's git-internal worktree metadata directory was left
  behind after the checkout path was deleted (e.g. via `git worktree remove
  --force` without pruning, or a Ctrl-C during teardown). `create()` now calls
  `repo.find_worktree(name)?.prune()` before the `repo.worktree()` add call,
  clearing orphaned metadata so the worktree can be recreated cleanly.
- **`overlay_builder` warning** now uses `{e:#}` (full anyhow error chain)
  instead of `{e}` when logging skipped unreadable segments.

### Changed

- **`Session::build_index`** no longer writes `.forgeql-index` when a columnar
  build context is configured. The legacy `SymbolTable` was already a transient
  artefact freed by `drop_legacy_index()` immediately after `warm_or_open`
  completes; persisting it to disk wasted I/O and produced a cache file that is
  never read on subsequent sessions (the warm path skips `resume_index()` when
  an overlay exists).
- **`exec_rollback`** no longer calls `resume_index()` for columnar sessions.
  The columnar state is fully restored by `reload_dirty_from_delta()` alone;
  the previous `resume_index()` call triggered an expensive and unused full
  rebuild of the legacy `SymbolTable` because `.forgeql-index` is no longer
  present on disk. Legacy-only sessions are unchanged.

## [0.49.5] — 2026-05-14 — Fix `shadow_writer` writing segments to unversioned unsharded path

### Fixed

- **`ShadowWriter::run` was writing segments to `segments/<provider_id>/<hex>/`** —
  the versioned provider dir (`{provider_id}-v{ENRICH_VER}`) and 2-char SHA prefix
  sharding were applied in `build_context.rs` / `overlay_builder.rs` / `warm.rs`
  but not in `shadow_writer.rs`, which is the main cold-build path.  Segments were
  therefore landing in the old flat layout, the overlay builder then looked in the
  versioned sharded layout and found nothing, and rebuilt from scratch on every USE.
  - `provider_dir` changed from `segments_base.join(provider_id)` to
    `segments_base.join(format!("{}-v{}", provider_id, ENRICH_VER))`.
  - `target_dir` changed from `provider_dir.join(&hex)` to
    `provider_dir.join(&hex[..2]).join(&hex[2..])`.
  - Unit tests `writes_one_segment_per_file` and `enrichment_fields_written_to_extra_columns`
    updated to expect the versioned + sharded layout.

## [0.49.4] — 2026-05-14 — Path-based enrichment versioning (`ENRICH_VER`)

### Added

- **`ENRICH_VER` constant (`mod.rs`)** — single compile-time `u32` that tracks the
  enrichment logic revision.  Bumping it automatically orphans all stale columnar
  cache dirs on the next `USE`; no manual cache deletion is ever required.
  - History: 1 = initial (v0.49.0), 2 = `condition_tests` fix (v0.49.1),
    3 = `has_fallthrough` annotation fix (v0.49.3).  Current value: **3**.
- **Versioned + sharded storage paths** (`build_context.rs`) — segments, overlays,
  and manifests are now stored under `<provider>-v<N>/` namespaces with git-style
  2-char SHA fan-out:
  - Segments: `segments/git-sha1-v3/<hex[0..2]>/<hex[2..]>/`
  - Overlays: `overlays/git-sha1-v3/<hex[0..2]>/<hex[2..]>.bin`
  - Manifest: `manifest-git-sha1-v3.json` (fresh column registry per version;
    fixes stale field accumulation from the additive-only `extend` in `manifest.rs`)
- **`ColumnarBuildContext::versioned_provider()`** and **`manifest_path()`** helpers.

### Changed

- `overlay_builder.rs`, `warm.rs`, `shadow_writer.rs` updated to use the new
  versioned + sharded paths (no header bytes changed).

## [0.49.3] — 2026-05-12 — Fix `has_fallthrough` ignores explicit annotations

### Fixed

- **`has_fallthrough` false-positive for annotated fallthroughs (`fallthrough.rs`)** —
  `__fallthrough;` (Zephyr/GCC/Clang), `[[fallthrough]]` (C++17), and
  `/* FALLTHROUGH */`-style comments were not recognised, causing every annotated
  intentional fallthrough to be incorrectly flagged.
  - New `is_fallthrough_statement()` helper matches known annotation keywords by node
    text (case-insensitive, semicolon-stripped).
  - New `is_fallthrough_comment()` helper matches standalone FALLTHROUGH comments
    (exact content match to avoid false-positives on descriptive comments).
  - `check_switch_cases()` now also scans siblings in the switch body between cases,
    because tree-sitter may place trailing comments there rather than inside the
    `case_statement` node.
  - Added `attributed_statement` to `statement_boundary_kinds` in `cpp.json` so that
    `[[fallthrough]];` is correctly classified as a statement.
  - Three new enrichment integration tests: `fallthrough_annotated_zephyr_style`,
    `fallthrough_annotated_cpp17_attr`, `fallthrough_annotated_comment`.

---

## [0.49.2] — 2026-05-12 — Fix session alias cross-source collision

### Fixed

- **Session alias not scoped to source (`exec_source.rs`)** — `USE vlc.master AS 'bench'`
  would resume an existing in-memory session named `'bench'` even if it belonged to a
  different source (e.g. `forgeql-pub`), returning wrong symbol counts and stale data.
  The eviction guard now checks both `source_name` and `user_id` against the requesting
  call before deciding to resume.  The `user_id` guard uses `"anonymous"` today and is
  wired for the future user system via a single `TODO(users)` change point.

---

## [0.49.1] — 2026-05-11 — Fix `condition_tests` clause counting

### Fixed

- **`condition_tests` over-count (`control_flow.rs`)** — The enricher previously
  counted every comparison *and* logical operator in the AST (`>`, `!=`, `&&`,
  `||`, …), producing `2N − 1` for a flat N-term `||` chain.  It now counts only
  `&&` / `||` / `and` / `or` operators and adds 1, giving the number of
  independent clauses the condition tests.

  | Example | Before | After |
  |---|---|---|
  | `a > 0` | 1 | **1** |
  | `a > 0 && b != 0` | 3 | **2** |
  | `a > 0 && b < 10 \|\| c == 5` | 5 | **3** |
  | 14-clause `\|\|` chain (VLC `input.c:2718`) | 15 | **8** |

---

## [0.49.0] — 2026-05-10 — Warm-Path Columnar Reconnect

### Changed

- **Warm-path optimisation (`exec_source.rs`)** — When the columnar overlay
  already exists on disk for the current HEAD commit, `USE source.branch` now
  skips `resume_index()` entirely and calls
  `ColumnarStorage::warm_or_open(ctx, None)` directly.  Previously every
  reconnect loaded the full legacy `SymbolTable` (~2–3 GB for Zephyr) only to
  discard it immediately after the overlay was opened.

  Measured improvement on `zephyr-andre.main` (2.7 M symbols):
  - Cold path (no overlay): ~236 s (unchanged — shadow-write still runs)
  - Warm path (overlay exists): ~15 s (≈15× faster)

  The cold path is preserved exactly: if no overlay exists, `resume_index()`
  runs first so the legacy `SymbolTable` is available for `ShadowWriter` to
  build segments and create the overlay.

  Fallback safety: if `warm_or_open` fails on the warm path, `resume_index()`
  is called as recovery so the session always has a usable index.

---

## [0.48.15] — 2026-05-10 — PhaseFT7: Git-Diff Reindex on Reconnect

### Added

- **`git::diff_head_to_worktree` (PhaseFT7)** — New function that returns the
  list of tracked files modified in the worktree relative to HEAD, as absolute
  paths. Uses `git2::StatusOptions` with `include_untracked(false)` so only
  committed-but-modified files are returned. Excludes all ForgeQL internal
  control files (same set as `CLEAN_COMMIT_EXCLUDED`).

- **Reconnect dirty reindex (`exec_source.rs`)** — After `resume_index` /
  `load_delta` and FT6 checkpoint restore, `diff_head_to_worktree` is called
  for existing worktrees (`wt_existed = true`). Any dirty files are reindexed
  via `session.reindex_files()` before the session is handed back to the
  caller. Non-fatal: git diff failures and reindex failures are logged as
  warnings and the cached index is used as-is (graceful degradation).

- **Gate tests — `tests/reconnect_dirty.rs`** — Three tests covering:
  `reconnect_reindexes_dirty_files`, `reconnect_does_not_reindex_clean_files`,
  `reconnect_after_begin_does_not_double_index`.

- **Unit tests in `git/mod.rs`** — Four inline tests covering:
  clean repo returns empty list, modified tracked file is detected, untracked
  file is excluded, ForgeQL control file is excluded.

### Fixed

- **Stale index after server restart mid-session** — Previously, `CHANGE FILE`
  edits made after the last checkpoint (or with no `BEGIN TRANSACTION`) were
  lost on reconnect: `resume_index` restored the pre-change cache and the
  in-memory delta was gone. FT7 detects and reindexes these files automatically.

---

## [0.48.14] — 2026-05-10 — PhaseFT6: Checkpoint Stack Persistence

### Added

- **`session::checkpoint_file` (PhaseFT6)** — New module that persists the
  in-memory checkpoint stack to `.forgeql-checkpoints` in the worktree using
  `bincode` serialization. The file is written atomically after every
  `BEGIN TRANSACTION`, updated on `ROLLBACK`, and deleted on `COMMIT`.

- **`CheckpointFile` / `PersistedCheckpoint`** — Serializable counterparts to
  `Session::checkpoints`. Version-stamped (`FILE_VERSION = 1`) so future
  format changes can gracefully discard stale files.

- **`checkpoint_file::try_restore`** — Validates the stored HEAD against the
  current worktree HEAD before restoring. Uses `checkpoints.last().oid` when
  the stack is non-empty, falling back to `last_clean_oid` for sessions with
  no open transaction. Silently discards stale or corrupt files.

- **`Session::get_head_oid` (public-crate)** — Extracted as a standalone
  `pub(crate)` method so `exec_source.rs` can obtain the current HEAD without
  going through a full `git2::Repository` open.

- **`exec_source.rs` reconnect restore** — After `load_delta` / `resume_index`
  in the `USE` path, `try_restore` is called to re-hydrate the checkpoint stack
  into a reconnecting session. Graceful on missing file (empty stack = same
  behaviour as pre-FT6).

- **Gate tests — `tests/checkpoint_persist.rs`** — Four tests covering:
  `checkpoint_survives_restart`, `stale_checkpoint_file_is_discarded`,
  `commit_clears_checkpoint_file`, `nested_checkpoints_rollback`.

### Changed

- **`git::CLEAN_COMMIT_EXCLUDED`** — Added `.forgeql-checkpoints`. The file
  is never included in user-facing commits (squashed away at `COMMIT`).
  It is intentionally **not** in `CHECKPOINT_EXCLUDED` so that `git reset
  --hard` on `ROLLBACK` restores the pre-transaction snapshot including the
  checkpoint file.

- **`session/mod.rs`** — `checkpoint_file` declared as a `pub mod` so the
  module is reachable from engine layers and gate tests.

### Fixed

- **`exec_transaction.rs` `BEGIN`** — `checkpoint_file::save` is called
  *after* `session.checkpoints.push(...)` so the file always reflects the
  full live stack (including the newly-pushed entry).

- **`exec_transaction.rs` `ROLLBACK`** — `checkpoint_file::save` is called
  *after* `git reset --hard`, not before. This overwrites whatever the git
  restore left on disk with the correct in-memory state (post-pop stack).

---

## [0.48.13] — 2026-05-10 — PhaseFT5: Route Flip + Drop Legacy RAM

### Changed

- **`BackendSet::default_engine` / `default_engine_mut` (PhaseFT5)** — Route
  flip: the default engine is now columnar when installed, falling back to
  legacy. Queries issued without a `USING` clause are served by columnar on
  sessions that have it.

- **`BackendSet::engine_for`** — Split `Backend::Default | Backend::Legacy`
  into two separate arms. `Backend::Legacy` remains an explicit escape-hatch
  that always targets the legacy engine regardless of the default routing.

### Added

- **`IndexStats::rows: usize`** — New field on `IndexStats` (zero-cost
  `Default::default()` for legacy) so columnar sessions can expose their row
  count through the same `index_stats()` path as legacy.

- **`ColumnarStorage::stats: IndexStats`** — Pre-computed stats field
  populated in `ColumnarStorage::new()` from `overlay.row_count()`. Returned
  by `index_stats()` (previously `None`).

- **`ColumnarStorage::locate_definition`** — Implemented via `resolve_impl`
  (previously inherited the default `None`).

- **`Session::drop_legacy_index()`** — Frees the legacy `SymbolTable` from
  memory. Called immediately after `install_columnar` in `exec_source.rs` so
  the legacy RAM is released once columnar is the default engine.

- **`ForgeQLEngine::session_index_stats_rows` (test-helper)** — Returns
  `index_stats().rows` for the session's default engine. Used by FT5 gate
  tests.

### Fixed

- **`Session::build_index` / `resume_index` / `save_index`** — Now target
  `legacy_storage_mut()` explicitly. Previously called
  `default_engine_mut().build/load/persist` which, after the route flip,
  would have routed to the no-op columnar implementations.

- **`Session::reindex_files`** — Legacy arm is non-fatal (`tracing::warn`)
  when called after `drop_legacy_index()` (table is `None`). Columnar arm
  remains a separate non-fatal warning.

- **`Session::flush_if_dirty`** — Skips `save_index` for columnar sessions;
  the delta file is managed at `BEGIN TRANSACTION` time and does not need an
  explicit flush.

- **`exec_source.rs` `show_stats`** — Two-arm `filter_map`: columnar sessions
  now appear in `SHOW SOURCES` with `rows` populated from
  `index_stats().rows`; legacy-specific memory fields are zeroed.

- **`exec_source.rs` `symbols_indexed`** — Fixed at two call-sites to prefer
  `engine().index_stats().rows` with legacy table fallback so columnar
  sessions report a non-zero count in the `USE` response.

### Tests

- **`ft5_columnar_index_stats_rows_match_overlay`** — Gate test: verifies
  `ColumnarStorage::index_stats()` returns `Some` and `rows ==
  overlay.row_count()`.

- **`ft5_session_has_columnar_after_install`** — Gate test: verifies
  `session_has_columnar() == true` and `session_index_stats_rows() ==
  overlay.row_count()` after `install_columnar_for_session`.

## [0.48.12] — 2026-05-10 — PhaseFT4: Overlay Manifest Merge at COMMIT

### Added

- **`OverlayBuilder::from_merge` (PhaseFT4)** — New constructor that builds a
  merged `segment_map` from a base overlay (excluding segments shadowed by
  `dirty.removed_hex_ids`) and the dirty-added segments. All segment readers are
  re-opened fresh from the bare-repo after promotion, avoiding mmap/inode issues
  on cross-device or OS-specific paths.

- **`ColumnarStorage::commit_dirty_inner`** — Core FT4 operation: promotes all
  staging segments to the bare-repo segment store via `promote_segment`, builds a
  new overlay with `OverlayBuilder::from_merge`, swaps the live `overlay` and
  `segments` fields, resets `dirty` to a fresh `DirtyOverlay`, clears the staging
  dir via `clear_staging_dir`, and removes the delta file.

- **`promote_segment` (private)** — Idempotent segment promotion: `dst.exists()`
  early-return guard; `rename`-first for same-device moves; lost-race re-check on
  rename failure; `copy_dir_all` fallback for cross-device.

- **`clear_staging_dir` (private)** — Deletes all entries inside the staging dir
  while keeping the directory itself (avoids `create_dir_all` on next reindex).

- **`StorageEngine::commit_dirty` (trait)** — Default no-op added to the
  `StorageEngine` trait, overridden by `ColumnarStorage` to delegate to
  `commit_dirty_inner`.

- **`exec_commit` integration** — After a successful git commit, `exec_commit`
  calls `columnar.commit_dirty(commit_hash, &ctx)` non-fatally: on error a
  `warn!` is emitted and the stale overlay is retained until the next FT7
  recovery path.

### Tests

- **`commit_promotes_segments_and_builds_new_overlay`** — Gate test: reindexes a
  file into staging, calls `commit_dirty`, asserts staging dir is empty, promoted
  segment is in the bare-repo store, the new overlay file exists, the overlay
  segment list is correct (old hex gone, new hex and unchanged hex present), and
  live queries return updated symbols.

- **`new_session_hits_promoted_overlay_cache`** — Gate test: verifies that a
  second session opening the promoted overlay via `Overlay::open` succeeds (cache
  hit), and that the session sees only the committed symbols.

## [0.48.11] — 2026-05-09 — PhaseFT3: Delta File Persistence

### Added

- **`DeltaFile` + `StagedEntry` (PhaseFT3)** — New module
  `crates/forgeql-core/src/storage/columnar/delta_file.rs` serialises the
  `DirtyOverlay` to `.forgeql-columnar-delta` using `bincode`. `DeltaFile::save`
  performs an atomic write-then-rename; `DeltaFile::load` rebuilds the overlay
  from staging segment directories; `DeltaFile::gc_orphaned_staging` removes
  staging dirs not referenced by the current delta file; `DeltaFile::read_valid_hexes`
  returns the hex IDs from a delta file non-fatally (empty on missing/corrupt).

- **`ColumnarStorage::delta_path`** — New `PathBuf` field pointing to
  `<worktree>/.forgeql-columnar-delta`. `save_delta`, `load_delta`, and
  `reload_delta_after_rollback` methods added to `ColumnarStorage`.

- **`Session::columnar_storage_mut()`** — New public method delegating to
  `backends.columnar_engine_mut()`, providing safe external access to the
  columnar backend without exposing the private `backends` field.

- **`StorageEngine::flush_delta` / `reload_dirty_from_delta`** — Two new
  default no-op trait methods, overridden by `ColumnarStorage` to save/restore
  the delta file. Enables `exec_begin_transaction` and `exec_rollback` to drive
  delta persistence through the trait interface.

### Changed

- **`warm_or_open`** — Calls `load_delta()` at all three return points so the
  dirty overlay is restored on session reconnect.

- **`reindex_files` + `purge_file`** — Both now call `save_delta()` after each
  mutation so the delta file is always up to date.

- **`exec_begin_transaction`** — Flushes the columnar delta before
  `stage_and_commit` so the checkpoint commit captures the current overlay state.

- **`exec_rollback`** — After `git reset --hard`, calls
  `reload_delta_after_rollback()` which GCs orphaned staging dirs then reloads
  the restored delta into RAM.

- **`git/mod.rs`** — `.forgeql-columnar-delta` added to `CLEAN_COMMIT_EXCLUDED`
  so it is never included in user-facing `COMMIT MESSAGE` history.

### Tests

- `delta_file_roundtrip` — bincode save/load round-trip for `DeltaFile`
- `delta_survives_simulated_restart` — dirty state persists across session drop/reconnect
- `rollback_gcs_orphaned_staging_segments` — orphaned staging dirs GC'd on rollback
- `nested_rollback_restores_correct_delta` — nested BEGIN/ROLLBACK restores correct state

---

## [0.48.10] — 2026-05-09 — PhaseFT1 + PhaseFT2: DirtyOverlay + reindex_files/purge_file

### Added

- **`DirtyOverlay` (PhaseFT1)** — New per-session in-RAM mutation layer in
  `crates/forgeql-core/src/storage/columnar/dirty_overlay.rs`. Tracks changed
  and deleted files via `DirtySegment` entries (`added: Vec<DirtySegment>`) and
  a `removed_hex_ids: HashSet<String>` that shadows persistent segments.
  `find_symbols`, `find_usages`, and `resolve_symbol` on `ColumnarStorage` now
  union persistent + dirty rows and filter out any persistent segment whose
  `hex_content_id` appears in `removed_hex_ids`. When the overlay is empty the
  new code paths are bypassed entirely (no per-query overhead).

- **`ColumnarStorage::dirty_mut()`** — `pub(crate)` accessor exposing the
  `DirtyOverlay` for direct manipulation in tests and by `reindex_files`.

- **`reindex_files` + `purge_file` (PhaseFT2)** — Full implementation of the
  `StorageEngine::reindex_files` and `StorageEngine::purge_file` trait methods
  on `ColumnarStorage`. `reindex_files` reads modified files from disk, computes
  the `git_blob_sha1` content-ID, builds a `SegmentBuilder`, validates with
  `is_valid_segment` (content-addressed idempotency), flushes to
  `.forgeql-staging/<hex>/`, and calls `dirty.add_segment`. `purge_file` looks
  up the persistent hex via `path_to_hex_content_id`, shadows it in
  `removed_hex_ids`, and evicts any stale dirty entry.

- **`staging_dir` + `lang_registry` fields on `ColumnarStorage`** — `staging_dir`
  is derived as `worktree_root.join(".forgeql-staging")` at construction time.
  `lang_registry: Arc<LanguageRegistry>` is used by `reindex_files` to select
  the correct parser per file extension; unknown extensions are skipped silently.

- **`BackendSet::columnar_engine_mut()`** — New method returning
  `Option<&mut dyn StorageEngine>` for the columnar backend, enabling
  `Session::reindex_files` to call the columnar backend non-fatally alongside
  the legacy backend.

- **`StorageEngine: 'static` supertrait** — Added `+ 'static` to the trait
  declaration in `storage/mod.rs` so `Box<dyn StorageEngine>` satisfies the
  lifetime bound required by `columnar_engine_mut()`.

- **`Session::reindex_files` columnar wiring** — Now calls
  `columnar_engine_mut().reindex_files(paths)` after the legacy backend. Errors
  are logged via `tracing::warn!` and are non-fatal; the legacy result is always
  returned to the caller.

### Tests

- **`dirty_overlay_shadows_and_unions`** (overlay_parity) — PhaseFT1 gate:
  `find_symbols` returns dirty rows and hides shadowed persistent rows.

- **`dirty_overlay_find_usages_shadows_and_unions`** (overlay_parity) —
  PhaseFT1 gate: `find_usages` respects dirty overlay shadowing and union.

- **`dirty_overlay_resolve_symbol_shadows_and_unions`** (overlay_parity) —
  PhaseFT1 gate: `resolve_symbol` returns the dirty row and `None` for a name
  that no longer exists in the dirty overlay.

- **`reindex_updates_dirty_overlay`** (overlay_parity) — PhaseFT2 gate:
  `reindex_files` shadows the old persistent segment and surfaces new symbols
  from the rewritten file while leaving other files' symbols untouched.

- **`purge_removes_file_symbols`** (overlay_parity) — PhaseFT2 gate:
  `purge_file` removes all symbols for the given file while leaving other
  files' symbols untouched.

---

## [0.48.9] — 2026-05-09 — Phase 06d: Zone-map pruning + parallel shadow-writer

### Added

- **Zone-map pruning in `find_symbols` and `resolve_impl`** — Before scanning
  segments, numeric predicates on `line`, `usages_count`, `byte_start`, and
  `byte_end` are evaluated against each segment's pre-computed zone-map
  (`zonemap_<col>.bin`). Segments whose entire value range cannot satisfy the
  predicate are pruned without being opened. `WHERE line < 0` and
  `WHERE line > 99999` drop from ~8 500 ms to ~30 ms (all 14 078 segments
  pruned instantly).

- **`usages` → `usages_count` zone-map alias** — Predicates written as
  `WHERE usages > N` now correctly map to the `usages_count` column when
  consulting zone maps in both `find_symbols` and `resolve_impl`.

- **Impossible-predicate short-circuit** — Before touching zone-map files,
  the engine checks whether the predicate can ever be satisfied on the
  unsigned `u32` storage domain (`Lt val≤0`, `Lte val<0`, `Eq val<0`). If
  not, `by_segment` / `seg_order` is cleared immediately and scanning is
  skipped entirely. The boundary condition `WHERE line < 0` (parsed as
  `val=0, op=Lt`) is correctly detected as impossible.

- **Fast path for enrichment-only + path-filter queries** — When a query
  carries a path glob (`IN 'drivers/serial/**'`) but no indexed predicate
  (`fql_kind=`, `name=`, `name LIKE`, `name MATCHES`), the global prefilter
  bitmap (built from all 500k+ rows) is bypassed. `by_segment` is seeded
  directly from path-filtered segments. Enrichment-only wide queries
  (`WHERE is_recursive = 'true' IN drivers/serial/**`) improve ~2×
  (264 ms → 132 ms). Wide glob queries (`WHERE has_fallthrough = 'true'
  IN drivers/**`) improve ~1.7× (3 114 ms → 1 884 ms).

- **Parallel `ShadowWriter` via rayon** — `ShadowWriter::run` rewrites the
  former build-loop + flusher-thread approach as a `rayon::par_iter()` across
  all files. Each worker independently computes the content-ID, checks
  idempotency, builds a `SegmentBuilder`, and flushes to disk. All 20 cores
  are used; the sequential merge phase (column aggregation, segment map
  assembly) follows. Rebuilding 14 078 zephyr-andre segments after a full
  segment wipe completes in the rayon burst visible in the CPU graph.

### Changed

- **`ShadowWriter::run` signature** — `run(mut self)` → `run(self)` (no
  longer needs `mut` since `pre_computed` is accessed via shared `.get()`
  inside the parallel closure).

- **`.cargo/config.toml` dev profile** — `debug = true` → `debug = false`.
  Strips DWARF symbols from debug builds (rust-analyzer doesn't need them).
  Reduces `/dev/shm/forgeql-target/debug` from ~6 GB to ~400 MB while
  keeping `incremental = true` so IDE responsiveness is unchanged.

### Tests

- **`enrichment_only_fast_path_parity`** (overlay_parity) — Verifies that
  `WHERE has_doc=X IN 'canonical.cpp'` (no indexed predicate, path filter)
  returns the same count via the fast path as the legacy backend.

- **`negative_line_predicate_returns_empty`** (overlay_parity) — Verifies
  that `WHERE line Lt -1`, `WHERE line Lte -1`, `WHERE line Eq -1`, and
  `WHERE line Lt 0` all return zero results (impossible-predicate
  short-circuit, including the `val=0` boundary case).

## [0.48.8] — 2026-05-08 — Phase 06b: ParseCache + SHOW wiring

### Added

- **`ast/parse_cache.rs`** — New `ParseCache` struct: per-session LRU cache
  for tree-sitter parses, keyed by SHA-1 hash of the source bytes. Capacity
  defaults to 32 entries per session. Backed by `VecDeque<[u8; 20]>` (LRU
  order) + `HashMap<[u8; 20], Arc<CachedParse>>`. On cache miss reads the
  file, computes SHA-1, parses with tree-sitter, and inserts the result.
  Repeat reads of the same (unchanged) file are served without disk I/O.

- **`Session::parse_cache`** — New `Mutex<ParseCache>` field on `Session`.
  Allows all SHOW operations in a session to share one parse cache.

- **`ForgeQLEngine::get_or_parse_for_show`** — New helper that acquires the
  session's parse cache on lock, delegates to `ParseCache::get_or_parse`,
  and falls back to a single-use cache when no session is active.

### Changed

- **`show_body`, `show_callees`, `show_members`, `show_signature`** — Now
  accept `&CachedParse` instead of a raw path; they no longer re-parse the
  file themselves. Callers (`exec_show.rs`) call `get_or_parse_for_show`
  once per SHOW invocation. Eliminates redundant file reads and tree-sitter
  parses inside a session.

- **`show_context`** — Now accepts `&[u8]` source bytes instead of a path
  (reads bytes before calling, outside the signature).

- **`ColumnarStorage::show_outline_for_file`** — Replaced the Phase-06 stub
  with a real implementation that iterates segment rows, filters by glob
  pattern, assembles (name, fql_kind, path, line) entries and returns them
  sorted by line number.

- **`ast/parse_cache` visibility**  — `ParseCache`, `CachedParse`,
  `sha1_of_bytes`, and all methods are now `pub` so integration tests and
  downstream crates can use them directly.

### Internal

- **`parse_file` helper removed** from `ast/show.rs` — no longer needed now
  that all SHOW callers receive `CachedParse` from the session cache.

### Tests

- **`parse_cache_hit_and_lru_eviction`** — Verifies `Arc` pointer equality
  on cache hit and that LRU eviction produces a distinct `Arc` after capacity
  overflow (capacity=1 test with two fixture files).

- **`columnar_show_outline_matches_legacy`** — Verifies that
  `ColumnarStorage::show_outline_for_file` returns the same (name, line) set
  as the legacy `show_outline` for `canonical.cpp`.
## [0.48.7] — 2026-05-08 — Phase 06a: Columnar resolve_* implementation

### Changed

- **`SegmentReader::enrichment_for_row`** (new) — collects all enrichment
  column values for a single row into a `HashMap<String, String>`. Mirrors
  the per-row loop inside `materialize_rows`, exposed as `pub(crate)` for
  use by `ColumnarStorage::location_for_row`.

- **`ColumnarStorage::location_for_row`** (new) — converts `(seg_idx,
  local_row)` to a `SymbolLocation` using the `SegmentMeta.source_path`
  already stored in the overlay. No PathMap / git-tree walk needed.
  Uses `fql_kind` as proxy for `node_kind` (segments do not store raw
  tree-sitter node kinds); `language_id` is 0 (no SHOW path reads this).

- **`ColumnarStorage::resolve_impl`** (new) — shared core for all three
  trait methods. Steps: qualified-name split (`::` / `.`), overlay FST name
  lookup, enclosing-type enrichment filter, IN/EXCLUDE glob filter, WHERE
  predicate filter via lightweight `SymbolMatch`, preferred-kind scoring,
  last-write-wins disambiguation. Returns `Option<SymbolLocation>`.

- **`ColumnarStorage::resolve_symbol`** — replaced `Err("requires Phase 06")`
  stub; calls `resolve_impl` with no kind preference.

- **`ColumnarStorage::resolve_type_symbol`** — replaced stub; calls
  `resolve_impl` preferring class/struct/enum/union/type_alias/trait/interface.

- **`ColumnarStorage::resolve_body_symbol`** — replaced stub; calls
  `resolve_impl`, then follows any `body_symbol` enrichment redirect (C++
  out-of-line member function definitions) with a second `resolve_impl` call
  using empty clauses — matching legacy `index.find_def(target)` semantics.

- Two free functions added in `columnar_storage.rs`:
  - `split_qualified_name` — splits `Owner::member` / `Owner.member`.
  - `passes_resolve_glob`  — IN/EXCLUDE glob check on relative paths.

### Verified

- Task 5 audit: zero `SymbolTable` usages in `engine/exec_show*` — all SHOW
  paths remain backend-clean and route through trait methods only.
- All 50 tests pass (unit, parity, SMS regression at budget=5000).

---

## [0.48.6] — 2026-05-08 — Phase 05.6: Engine submodule split + Phase 06 prerequisites

### Changed

- **`crates/forgeql-core/src/engine/`** — `engine.rs` free functions, JSON
  converters, and unit tests extracted into dedicated submodules (Task 1):
  - `engine/helpers.rs` — `load_verify_config`, `generate_session_id` (cfg),
    `require_session_id`, `mutation_op_name`, `detect_metric_hint`,
    `reject_text_filter`
  - `engine/convert.rs` — `convert_suggestions`, `convert_show_json` (+ private
    `convert_show_content`, `extract_source_lines`)
  - `engine/tests.rs` — all `#[cfg(test)]` functions
  - `engine.rs` retains: constants, `ForgeQLEngine` struct + impl, module
    declarations, and `pub(crate) use` re-exports so `exec_*.rs` imports are
    unchanged.
- **Visibility pattern** — `pub mod helpers/convert` (publicly routable module) +
  `pub(crate) fn` items inside — the only combination satisfying both
  `unreachable_pub` (`workspace.lints.rust`) and `redundant_pub_crate`
  (clippy `pedantic`) simultaneously.

### Verified (no-op tasks)

- **Task 2** — Zero deprecated engine shims found; nothing to remove.
- **Task 3** — Columnar code audit clean: no bare `.unwrap()` in production
  paths; `.expect()` confined to test helpers only.
- **Task 5** — Phase 06 gate checks all pass:
  - `ShadowWriter` / `OverlayBuilder` absent from `engine/**` and `session/**`
  - `warm_or_open` confirmed at `columnar_storage.rs:394` (usages=3)
- **Task 4** — Phase 06 enrichment requirements documented in
  `ForgeQL-StorageEngine-Plan/phases/Phase06.md`.

## [0.48.5] — 2026-05-08 — Phase 05.5: Lift inline overlay-build into `ColumnarStorage`

### Changed

- **`ColumnarStorage`** — new inherent methods centralise all overlay
  orchestration that was previously scattered across callers:
  - `warm_or_open(ctx, legacy, worktree_path, commit_sha)` — opens an
    existing overlay (fast path) or builds one via `ShadowWriter` +
    `OverlayBuilder` under `OverlayLock` (slow path), then constructs and
    returns a ready-to-query `ColumnarStorage`.
  - `warm(ctx, legacy, worktree_path, commit_sha)` — thin wrapper around
    `warm_or_open` for background warming where the result is discarded.
  - `open_segments_from_overlay` (private) — opens `SegmentReader`s for
    every segment listed in an `Overlay`; silently skips unreadable ones.

- **`exec_source.rs`** — the 120-line inline overlay-build block replaced
  by a single `ColumnarStorage::warm_or_open` call.  Zero references to
  `ShadowWriter`, `OverlayBuilder`, `OverlayLock`, or `Overlay::open`.

- **`session/mod.rs` (`build_index`)** — `SegmentBuildCtx` wiring,
  inline content-ID cache (`Arc<Mutex<HashMap>>`), `ShadowWriter` run,
  and `OverlayBuilder` call removed.  `build_index` is now legacy-only.
  Zero references to `ShadowWriter`, `OverlayBuilder`, or `OverlayLock`.

- **`warm.rs` (`warm_snapshot`)** — `OverlayLock` acquire + re-check
  block removed (now inside `warm_or_open`).  After `build_index`, calls
  `ColumnarStorage::warm` to delegate segment + overlay construction.
  Zero references to `ShadowWriter`, `OverlayBuilder`, or `OverlayLock`.

## [0.48.4] — 2026-05-07 — Phase 05.4: Remove escape hatches from `StorageEngine` trait

### Changed

- **`StorageEngine` trait** — deleted three legacy/columnar-specific methods:
  `as_legacy_table()`, `as_legacy_table_mut()`, `set_seg_ctx()`.
  The trait now contains zero backend-aware methods; all query paths go
  through the generic interface.
- **`BackendSet`** — stores the legacy backend as a concrete
  `LegacyMemoryStorage` (not `Box<dyn StorageEngine>`).  New accessors:
  `legacy_storage()` / `legacy_storage_mut()` (both `const fn`, returning
  `Option<&LegacyMemoryStorage>` for Phase 09 forward-compatibility).
  `default_engine()` / `default_engine_mut()` auto-coerce to
  `&dyn StorageEngine`.  `BackendSet::new` now takes `LegacyMemoryStorage`
  directly.  Deprecated `legacy()` accessor removed.
- **`LegacyMemoryStorage`** — added three inherent `pub const fn` methods:
  `table()`, `table_mut()`, `install_segment_build_ctx()`.  The trait
  overrides for `as_legacy_table`, `as_legacy_table_mut`, `set_seg_ctx`
  are removed.

### Removed

- `StorageEngine::as_legacy_table()` — use `Session::legacy_storage().and_then(|l| l.table())`
- `StorageEngine::as_legacy_table_mut()` — use `Session::legacy_storage_mut().and_then(|l| l.table_mut())`
- `StorageEngine::set_seg_ctx()` — use `LegacyMemoryStorage::install_segment_build_ctx()`
- `Session::index_mut()` — dead code (zero external callers)

### Added

- `Session::legacy_storage(&self) -> Option<&LegacyMemoryStorage>` — typed
  accessor for exec paths that legitimately need `&SymbolTable`.

## [0.48.3] — 2026-05-07 — Phase 05.3: Introduce `BackendSet`

### Added

- **`crates/forgeql-core/src/storage/backend_set.rs`** — new `BackendSet` struct
  that owns all storage backends for a session:
  - `new(legacy)` — creates a set with only the legacy backend.
  - `with_columnar(columnar)` — builder-style columnar install.
  - `set_columnar(&mut self, ...)` — post-construction install / replace.
  - `has_columnar()` — `true` when a columnar backend is present.
  - `default_engine()` / `default_engine_mut()` — access to the legacy backend.
  - `engine_for(&Backend)` — routes `Default`/`Legacy` to the legacy backend,
    `Columnar` to the optional columnar backend (errors when absent).
  - Deprecated `legacy()` accessor as a Phase 05.4 removal marker.
- `storage/mod.rs`: `pub mod backend_set; pub use backend_set::BackendSet;`
- **`crates/forgeql-core/tests/backend_set.rs`** — 4 unit tests:
  `new_yields_legacy_only`, `with_columnar_round_trip`,
  `engine_for_default_equals_legacy`, `set_columnar_replaces`.

### Changed

- **`session/mod.rs`**: replaced two fields `engine: Box<dyn StorageEngine>` and
  `columnar_engine: Option<Box<dyn StorageEngine>>` with a single
  `backends: BackendSet`.
- **`session/mod.rs`**: `engine()`, `engine_mut()`, `engine_for()` are now thin
  forwarders to `BackendSet`. Added `has_columnar()` and `install_columnar()`
  forwarding methods.
- **`session/mod.rs`** internals (`build_index`, `resume_index`, `save_index`,
  `reindex_files`, `drop_index`, `index`, `index_mut`, `has_index`): all
  `self.engine.*` calls replaced with `self.backends.default_engine[_mut]().*`.
- **`engine/exec_source.rs`**: `session.columnar_engine = Some(...)` →
  `session.install_columnar(...)`.
- **`engine/exec_session.rs`**: `session.columnar_engine = Some(...)` →
  `session.install_columnar(...)`; `s.columnar_engine.is_some()` →
  `Session::has_columnar` method reference.

---

## [0.48.2] — 2026-05-07 — Phase 05.2: Introduce `ColumnarBuildContext`

### Added

- **`crates/forgeql-core/src/storage/columnar/build_context.rs`** — new
  `ColumnarBuildContext` struct that groups the four previously-flat columnar
  configuration fields on `Session` into a single typed value:
  - `segments_dir: PathBuf`
  - `overlays_dir: PathBuf`
  - `provider_id: String`
  - `hash_fn: HashFn`
- Two path-derivation helpers on `ColumnarBuildContext`:
  - `segment_dir_for(hex_content_id)` → `<segments_dir>/<provider_id>/<hex>/`
  - `overlay_path_for(snapshot_hex)` → `<overlays_dir>/<provider_id>/<hex>.bin`
- `ColumnarBuildContext` is re-exported from both `columnar/mod.rs` and
  `storage/mod.rs`.

### Changed

- **`session/mod.rs`**: replaced four flat `columnar_*` fields
  (`columnar_segments_dir`, `columnar_provider_id`, `columnar_hash_fn`,
  `columnar_overlays_dir`) with a single `columnar_build: Option<ColumnarBuildContext>`.
- **`session/mod.rs`**: replaced `set_columnar_segments_dir` with
  `set_columnar_build(ctx: ColumnarBuildContext)` and added a `const`
  `columnar_build()` accessor.
- **`session/mod.rs`** `build_index()`: reads provider ID, hash fn, segment
  dir, and overlay path from `ctx` instead of four separate `Option` fields;
  eliminates the four-way `if let (Some(…), Some(…), …)` guard.
- **`engine/exec_source.rs`**: writer block constructs a `ColumnarBuildContext`
  and calls `set_columnar_build`; reader block calls `ctx.overlay_path_for` and
  `ctx.segment_dir_for`; collapsed the `if needs_build { if let Some(table)`
  nesting into `if needs_build && let Some(table)`.
- **`engine/warm.rs`**: constructs a `ColumnarBuildContext` and calls
  `set_columnar_build`.
- **`engine/exec_session.rs`**: integration-test helper uses
  `set_columnar_build`.

---

## [0.48.1] — 2026-05-07 — Phase 05.1: Move Legacy Resolvers Out of `engine.rs`

### Changed

- **Moved legacy backend internals from `engine.rs` into `storage/legacy/` submodules.**
  - `storage/legacy/helpers.rs` — `passes_glob_filter` (glob path predicate utility).
  - `storage/legacy/prefilter.rs` — `find_symbols_prefilter`, `validate_order_by_field`,
    `field_to_kinds`, `field_to_kinds_for_config`, `infer_kinds_from_fields`,
    `extract_anchored_literal`, `regex_trigram_literal`, `like_trigram_literal`, `find_pred_string`.
  - `storage/legacy/resolve.rs` — `resolve_symbol`, `resolve_type_symbol`, `resolve_body_symbol`,
    `split_qualified_name`.
  - `storage/legacy.rs` now declares `mod helpers; mod prefilter; mod resolve;` and all
    6 `crate::engine::*` call sites updated to `helpers::*` / `prefilter::*` / `resolve::*`.
- **Cleaned up `engine.rs`**: removed ~576 lines of dead code (moved functions + their tests).
  `engine.rs` now owns only `ForgeQLEngine`, session management, conversion helpers,
  `detect_metric_hint`, `reject_text_filter`, and `extract_source_lines`.
- **Validate-order-by tests** moved to `storage/legacy/prefilter.rs` test module.
- Removed now-unused imports from `engine.rs`: `HashSet`, `SymbolTable`, `SymbolMatch`.

## [0.48.0] — 2026-05-06 — Phase 05: Workspace Overlay, Trigram Index, Background Warming

### Added

- **`Overlay` reader (`storage/columnar/overlay.rs`).**
  New `Overlay` struct reads a workspace-level merged index from a binary file
  (format: 24-byte header `FQOV` + bincode-serialised `OverlayPayload`).
  - `Overlay::open(path)` validates magic + schema version, deserialises the payload,
    rebuilds `RoaringBitmap` per `fql_kind`, and re-hydrates the name FST.
  - Query methods: `prefilter_kind`, `lookup_name_bitmap`, `resolve_global`.
  - Exported types: `Overlay`, `RowPtr`, `SegmentMeta`, `OverlayPayload` (all `pub`).

- **`OverlayBuilder` (`storage/columnar/overlay_builder.rs`).**
  Merges N segments into a single `Overlay` file atomically.
  - Takes `provider_id`, `segments_dir`, `worktree_root`, and `segment_map`
    (`HashMap<PathBuf, Vec<u8>>`) from `ShadowWriteResult`.
  - Sorts segments by `hex_content_id` for deterministic global row ordering.
  - Builds merged name FST + name postings; merges `RoaringBitmap`s per `fql_kind`.
  - Writes header + payload via tmp-file + `sync_all` + atomic rename.

- **`ColumnarStorage` (`storage/columnar/columnar_storage.rs`).**
  Implements `StorageEngine` over a set of `SegmentReader`s + an `Overlay`.
  - `find_symbols`: prefilter global bitmap → group by segment → materialize → apply_clauses.
  - `find_usages`: FST name lookup → group by segment → materialize.
  - SHOW methods return a Phase 06 placeholder error.
  - Installed in `Session.columnar_engine` after `USE` when overlay exists on disk.

- **Session wiring (`session/mod.rs`, `engine/exec_source.rs`).**
  After shadow-write, `use_source` sets `columnar_overlays_dir`, calls
  `OverlayBuilder::build_and_persist`, opens the result with `Overlay::open`,
  loads `SegmentReader`s, and installs a `ColumnarStorage` into the session.

- **`WarmPolicy` + `WarmPolicyKind` in `ColumnarConfig` (`config.rs`).**
  `warm_on_create` and `warm_on_refresh` knobs with `WarmPolicyKind` (`off`,
  `default-branch`, `all-branches`, `pinned`).  Both default to `enabled: false`.

- **Parity integration tests (`tests/overlay_parity.rs`).**  7 tests covering:
  - `overlay_find_symbols_matches_legacy_merged` — 2-segment overlay vs merged legacy `(name, fql_kind, line)` set.
  - `overlay_kind_prefilter_matches_legacy` — `WHERE fql_kind='function'` returns only functions.
  - `overlay_exact_name_lookup_matches_legacy` — `WHERE name='foo'` row count + values match legacy.
  - `overlay_like_filter_matches_legacy` — `WHERE name LIKE 'f%'` name set matches legacy.
  - `overlay_order_by_line_asc` — `ORDER BY line ASC` produces non-decreasing lines.
  - `overlay_enrichment_field_filter_matches_legacy` — `WHERE has_doc='true'` count + field presence match legacy.
  - `overlay_lookup_name_spans_segments` — `lookup_name_bitmap('bar')` bitmap spans both canonical fixtures (≥ 2 global row IDs).

- **Public re-exports** — `storage/mod.rs` re-exports `Overlay`, `OverlayBuilder`,
  `ColumnarStorage`, `ShadowWriteResult`; `columnar/mod.rs` re-exports all sub-modules
  and types as `pub`.

- **Background warming on `CREATE SOURCE` and `REFRESH SOURCE`** (task 9).
  New `engine::warm` module exposes `pick_warm_targets`, `warm_snapshot`, and
  `spawn_warmer`.  When `columnar.warm_on_create.enabled` or
  `columnar.warm_on_refresh.enabled` is set in `.forgeql.yaml`, a detached
  background thread builds segments and overlays for the chosen snapshots
  immediately after the source op returns — so the first `USE` pays only the
  columnar load cost (~50–200 ms) instead of the full build (~10–30 s on large
  repos).  `REFRESH SOURCE` only warms branches whose HEAD actually moved,
  preventing CPU drain on no-change polling refreshes.  Both knobs default to
  `enabled: false`.  Five unit tests cover the policy selector for every variant.
- **`Source::branch_heads()` and `Source::default_branch()`.** Public helpers
  used by background warming to compute the moved-set across `REFRESH SOURCE`
  and to resolve the default-branch policy target.
- **Per-overlay advisory file lock** (task 7, R7).  `OverlayLock`
  (`fd-lock`-backed POSIX flock / Windows `LockFileEx`) serialises concurrent
  `USE` calls that land on the same `(source, branch, commit)` on a sibling
  `<commit>.lock` file instead of double-building or racing on the atomic
  rename.  The build path re-checks overlay existence after acquiring the lock
  so a peer that finished while waiting is respected without wasted work.  Two
  unit tests: lock-file lifecycle and serialised-acquire ordering (POSIX-only).
- **Trigram index in workspace overlay** (task 4).  The overlay now persists a
  `name → trigram → RoaringBitmap<global_row_id>` index built from the merged
  name FST, mirroring legacy `TrigramIndex` semantics (ASCII-lowercased,
  deduplicated 3-byte windows).  The columnar prefilter consults it for
  `WHERE name LIKE '…'` and `WHERE name MATCHES '…'`, intersecting per-trigram
  bitmaps for every literal run of ≥3 chars before materialising rows.
  Bumps `OverlayPayload` `SCHEMA_VERSION` from `1` → `2`; existing v1 overlays
  are detected at open time and rebuilt on the next `USE`.
  End-to-end parity runtime: 273 s → 220 s (~19 %).
- **`PARITY_SHORT=1` fast mode for `parity_full_corpus`.**  When set, the
  parity gate keeps only the first 2 queries of each `gNN_` group
  (≈50 queries instead of ≈250), running in ~4.5 min instead of ~16.
  Nightly / pre-release runs leave the variable unset to exercise the full
  corpus.

### Fixed

- **Engine-level parity test (`tests/parity_find.rs`) rewritten.**
  The previous unit-level harness bypassed the parser and `USING 'columnar'`
  dispatch.  The new harness runs real FQL strings through
  `ForgeQLEngine::execute()` including queries with `USING 'columnar'`, covering
  a corpus of 287 distinct `FIND symbols` queries across 40 groups (`g01`–`g40`).
  All 287 query pairs (legacy vs columnar) report zero divergence.

- **`ColumnarStorage::find_symbols` deduplicates on `(name, fql_kind, path, line)`.**
  The legacy backend deduplicates on `(name_id, path_id, node_kind_id, line)` in
  `find_symbols_prefilter`.  Without the equivalent deduplication in
  `ColumnarStorage`, 2 extra rows appeared in the columnar results for the
  canonical fixtures, causing parity divergence.  Dedup is now applied before
  `apply_clauses`.

- **`register_local_session_with_columnar` test-helper path corrected.**
  The overlay was opened at `overlays_dir/unknown/.bin`; the correct path is
  `overlays_dir/test/.bin` (provider_id is `"test"`).  The segment directory is
  likewise `segments_dir/test/{hex_content_id}/`.

- **`LIMIT 1000` normalisation in `parity_full_corpus`.**
  Corpus queries without an explicit `LIMIT` clause now get `LIMIT 1000`
  appended before both legacy and columnar runs.  This prevents the default
  `LIMIT 20` from causing spurious divergence due to different iteration orders
  between the two backends.

- **`overlay_find_symbols_matches_legacy_merged` updated for dedup.**
  The legacy baseline is now built with a per-file `HashSet<(name, fql_kind, path,
  line)>` dedup — matching `ColumnarStorage::find_symbols` — instead of comparing
  against the raw 246-row combined SymbolTable (which included 2 intra-file
  duplicates).

- **`SymbolRow.kind` no longer falls back to deprecated `node_kind`.**  The
  legacy backend populated `kind` from `fql_kind ?? node_kind` while the
  columnar backend never stores `node_kind` — producing parity divergence for
  AST nodes without an `fql_kind` mapping (`preproc_ifdef`, `enumerator`,
  `compound_assignment`, `default_parameter`, `keyword_argument`, …).  Both
  backends now return an empty `kind` for such rows.
- **Deterministic ordering before `LIMIT`/`OFFSET` truncation.**
  `filter::apply_clauses` now applies a stable `(name, line, path)`
  tie-breaker after any user-supplied `ORDER BY`, and uses the same triple as
  the default order when no `ORDER BY` is given.  Eliminates backend-dependent
  row selection that previously caused divergence on `g01`, `g09`, `g13`,
  `g17`, `g20`, `g24`.

- **`session_has_columnar` test-helper** (`engine/exec_session.rs`).
  Returns `true` if the named session has a columnar backend installed; used
  by `parity_full_corpus` to assert the backend was wired up before running
  any queries.

## [0.47.0] — 2026-05-04 — Phase 04: Per-Segment Reader

### Added

- **`SegmentReader` (`storage/columnar/segment_reader.rs`) — mmap-based read path.**
  New `SegmentReader` opens a single columnar segment directory written by
  `SegmentBuilder` and exposes a full `FIND symbols`-equivalent API against
  its on-disk data without loading everything into RAM.

  **Open and validation** (`SegmentReader::open`):
  - Reads `header.bin`, validates the `FQSG` magic and schema version 1.
  - Parses the variable-length column entry table to discover both core and
    enrichment columns.
  - Memory-maps all seven core `col_*.bin` files and any enrichment columns.
  - Builds a `StringPool` with both forward (ID → `&str`) and reverse
    (name → ID) lookups.
  - Deserialises `postings_fql_kind.bin` into
    `HashMap<kind_id, RoaringBitmap>` for O(n/64) prefilter queries.
  - Loads `name.fst` bytes into a `fst::Map<Vec<u8>>` and mmaps
    `name_postings.bin` for FST-backed name lookups.

  **Query pipeline** (`find_symbols`):
  1. *Roaring bitmap prefilter* — `WHERE fql_kind = 'X'` predicates (exact
     equality only) are resolved against the in-memory posting list bitmaps
     using bitwise AND, producing a compact candidate row set without
     touching column data.
  2. *Materialise* — surviving rows are read from the mmap'd column arrays
     and assembled into `Vec<SymbolMatch>` (enrichment fields copied into
     `SymbolMatch.fields`).  `node_kind` is set to `None` (segments do not
     store tree-sitter grammar node kinds; that detail lives in the legacy
     index only).
  3. *`apply_clauses` residual pipeline* — the shared `crate::filter`
     pipeline runs over the materialised results, handling residual WHERE
     predicates, GROUP BY / HAVING, ORDER BY, LIMIT, and OFFSET exactly as
     the legacy backend does — guaranteeing clause-pipeline parity.

  **Row accessors** — `name_of`, `fql_kind_of`, `language_of`, `line_of`,
  `byte_start_of`, `byte_end_of`, `usages_count_of`, `extra_field_str` give
  direct per-row reads without materialising a full `SymbolMatch`.

  **FST name lookup** (`lookup_name`) — O(log n) FST lookup decodes the
  packed `(count | byte_offset << 32)` value from the FST and returns the
  matching row IDs from `name_postings.bin`.

  **9 unit tests** in `segment_reader::tests`:
  `open_segment_written_by_builder`,
  `find_functions_order_by_name`,
  `find_by_enrichment_field`,
  `group_by_kind_having_count`,
  `order_by_line_desc`,
  `limit_and_offset`,
  `lookup_name_via_fst`,
  `roaring_prefilter_returns_empty_for_unknown_kind`,
  `source_path_propagated_to_symbol_match`,
  `round_trip_row_content`,
  `find_symbols_on_empty_segment_returns_empty_vec`,
  `open_nonexistent_dir_returns_err`,
  `open_corrupt_magic_returns_err`,
  `open_nonmonotone_string_pool_returns_err`.

  `SegmentReader` is re-exported from `crate::storage::columnar` and from
  `crate::storage`.
  > **Phase 04 scope**: `SegmentReader` is a standalone library component.
  > It does not wire into `FIND … USING 'columnar'` production queries.
  > Multi-segment overlay queries over a live session are Phase 05.

- **Parity test harness (`crates/forgeql-core/tests/segment_parity.rs`).**
  11 integration tests verifying that `SegmentReader` produces byte-for-byte
  identical results to the legacy `SymbolTable` path on the canonical C++ and
  Rust fixtures:
  `parity_cpp_canonical`, `parity_rust_canonical`,
  `parity_filter_fql_kind_function_cpp`,
  `parity_order_by_line_asc_cpp`, `parity_order_by_line_desc_cpp`,
  `parity_like_name_cpp`, `parity_byte_ranges_cpp`,
  `parity_lookup_name_cpp`, `parity_enrichment_fields_cpp`,
  `memory_budget_fql_kind_prefilter_cpp` (Linux-only; page-fault baseline
  ≈ 232 faults for a cold mmap on the canonical.cpp fixture).

## [0.46.0] — 2026-05-04

### Added

- **Per-segment columnar writer (`storage/columnar`).**
  New `crates/forgeql-core/src/storage/columnar/` module delivers the
  per-segment write path.  Three new dependencies added to the workspace:
  `memmap2 = "0.9"`, `roaring = "0.10"`, `fst = "0.4"`, and
  `bytemuck = { version = "1", features = ["derive"] }`.

- **`git_blob_sha1` standalone function (`git_sha1_provider.rs`).**
  `pub fn git_blob_sha1(content: &[u8]) -> [u8; 20]` hashes a byte slice
  using git's canonical blob object format (`"blob {len}\0{content}"`),
  enabling content-addressed segment filenames without going through the full
  `gix` stack.

- **`ColumnarConfig` in `.forgeql.yaml`.**
  `ForgeConfig` gains a `columnar: ColumnarConfig` section (default-off).
  Setting `columnar.shadow_write = true` enables dual-write mode.  The
  sidecar template includes the commented-out section as documentation.

- **`SegmentBuilder` (`storage/columnar/segment_builder.rs`).**
  Builds one columnar segment from the rows of a single source file.
  Writes an atomic snapshot into a content-addressed directory
  `<segments_base>/git-sha1/<content_hex>/` via a tmp-dir + rename idiom.
  The binary format consists of:
  - `header.bin` — 80-byte preamble (magic `FQSG`, schema version,
    provider-id, content-id, row count, string count, column count).
  - One `col_<name>.bin` per column (`name_id`, `fql_kind_id`, `line`,
    `byte_start`, `byte_end`, `usages_count`, `language_id`) — packed
    `u32` arrays via `bytemuck`.
  - `strings_offsets.bin` + `strings_data.bin` — per-segment string
    intern table.
  - `postings_fql_kind.bin` — `RoaringBitmap` per `fql_kind` string,
    serialised with `roaring`'s portable format.
  - `name.fst` + `name_postings.bin` — `fst` automaton mapping symbol
    name to a `(count, byte_offset)` pair packed into `u64`; the byte
    offset indexes into `name_postings.bin` for row-ID lists.
  - `is_valid_segment(dir)` guard checks for the `FQSG` magic before any
    read attempt.

- **`ShadowWriter` (`storage/columnar/shadow_writer.rs`) — fully redesigned.**
  All six Phase 03 issues closed in commit `488e972`:

  - **Issue 1 — provider decoupling**: `ShadowWriter::new` now accepts
    `provider_id: &str` and `hash_content: &(dyn Fn(&[u8]) -> Vec<u8> + Send + Sync)`.
    The `git_blob_sha1` symbol is no longer referenced inside `ShadowWriter`;
    the concrete hash function is injected by the caller (`exec_source.rs`).

  - **Issue 2 — enrichment fields**: `ShadowWriter::run` calls
    `table.resolve_fields(&row.fields)` for each `IndexRow` and forwards
    every enrichment key/value to `SegmentBuilder::set_field`, so extra
    per-enricher columns are written to every segment.

  - **Issue 3 — double file read**: `ShadowWriter::new` accepts a
    `pre_computed: HashMap<PathBuf, Vec<u8>>` map.  When a file's content ID
    is already in the map (computed inline during `index_file` via
    `SegmentBuildCtx::emit_fn`), the source file is not re-read.
    `Session::build_index` populates this map via a `Mutex`-backed cache
    written to by the `emit_fn` closure.

  - **Issue 4 — background flush**: `run()` spawns a `std::thread` that
    receives `(SegmentBuilder, target_dir)` pairs from a `sync_channel(64)`.
    Flushing happens on the background thread while the main loop builds the
    next segment, overlapping CPU and I/O.

  - **Issue 5 — `Manifest`**: new `storage/columnar/manifest.rs` with
    `Manifest { schema_version, provider_id, column_registry: BTreeSet<String>,
    segment_count }`.  `Manifest::update(path, provider_id, columns, count)`
    atomically merges and saves `<forgeql_dir>/manifest.json` after each run.

  - **Issue 6 — unit tests**: five unit tests added to `shadow_writer.rs`:
    `empty_table_writes_no_segments`, `writes_one_segment_per_file`,
    `enrichment_fields_written_to_extra_columns`, `pre_computed_avoids_file_read`,
    `manifest_written_after_run`.

- **`Session::set_columnar_segments_dir` extended.**
  Now accepts `(dir: PathBuf, provider_id: impl Into<String>, hash_fn: HashFn)`.
  Two new fields added to `Session`: `columnar_provider_id: Option<String>`
  and `columnar_hash_fn: Option<HashFn>`.

- **`Session::build_index` wires `SegmentBuildCtx`.**
  Before `engine.build()`, if shadow-write is configured, `build_index` creates
  a `SegmentBuildCtx` whose `emit_fn` populates an in-memory content-ID cache.
  After the build, the cache is extracted and passed to `ShadowWriter::new` as
  `pre_computed`, avoiding all double file reads.

- **`exec_source.rs` injects `HashFn`.**
  The shadow-write config block now creates
  `Arc::new(|b: &[u8]| git_blob_sha1(b).to_vec())` and passes it together
  with `"git-sha1"` to the updated `set_columnar_segments_dir`.

## [0.45.0] — 2026-05-04

### Added

- **`USING 'backend'` clause for all read-only commands.**
  Optional `USING 'backend'` clause can appear between a command's primary target
  and any `clauses` modifiers on every `FIND` and `SHOW` command.  Accepted
  backend names:
  - `'legacy'` — routes to the existing in-memory `LegacyMemoryStorage` (same
    as omitting `USING`).
  - `'columnar'` — routes to `Session::columnar_engine`; returns
    `"columnar backend is not enabled for this session"` if the slot is `None`.
  - (default, no clause) — equivalent to `'legacy'` in the current implementation.

  `USING` is intentionally not accepted on mutations (`CHANGE`, `COPY`, `MOVE`,
  `BEGIN TRANSACTION`, `COMMIT`, `ROLLBACK`, `VERIFY`) — the grammar rejects it
  at parse time.

- **`Backend` enum (`crates/forgeql-core/src/ir.rs`).**
  Variants: `Default` (serde default), `Legacy`, `Columnar`.
  `Backend::from_clause(s)` maps a string to the enum or returns a
  `ForgeError::DslParse` for unknown names.
  `is_default_backend` is the `serde(skip_serializing_if)` helper, so JSON
  wire format is unchanged for queries that do not supply `USING`.

- **`Session::columnar_engine` slot.**
  `Session` now holds `columnar_engine: Option<Box<dyn StorageEngine>>`,
  initialised to `None`.  `Session::engine_for(&Backend)` dispatches
  `Default`/`Legacy` to the existing engine and `Columnar` to the slot.

- **`require_workspace_and_engine_for` helper (`exec_session.rs`).**
  Read-only `exec_show` and `exec_find` call this instead of
  `require_workspace_and_engine` so that backend routing flows through a
  single chokepoint.

## [0.44.0] — 2026-05-03

### Added

- **`StorageEngine` trait (`forgeql-core::storage`).**
  A new `StorageEngine: Send + Sync` trait abstracts all index read/write operations
  (`find_symbols`, `find_usages`, `resolve_symbol`, `resolve_type_symbol`,
  `resolve_body_symbol`, `stats`, `build`, `reindex_files`, `purge_file`,
  `persist_to_cache`, `load_from_cache`). Every `exec_*` path now goes through the
  trait instead of touching `SymbolTable` directly. Escape hatches `as_legacy_table`
  / `as_legacy_table_mut` are provided for test helpers and debugging tools.

- **`LegacyMemoryStorage` — existing `SymbolTable` behind the trait.**
  The previous in-RAM index is wrapped in `LegacyMemoryStorage`, which implements
  `StorageEngine` with identical behaviour. All query results are byte-for-byte
  equivalent to pre-0.44.0 output. `Session` now owns `Box<dyn StorageEngine>`
  instead of `Option<SymbolTable>` directly; `Session::index()` and
  `Session::index_mut()` are kept as backwards-compatible helpers that downcast
  through `as_legacy_table`.

- **`SourceProvider` trait + `GitSha1Provider`.**
  `SourceProvider` decouples storage from git internals — methods: `hash_content`,
  `read_content`, `current_snapshot`, `walk_snapshot`, `changed_paths`. The
  production implementation `GitSha1Provider` is `gix`-backed and uses git's blob
  SHA-1 algorithm for content addressing. Validated by
  `walk_snapshot_matches_git_ls_tree`, which cross-checks provider output against
  `git ls-tree -r HEAD` on the live repo.

- **`StubColumnarStorage` — trait-shape validation.**
  A throwaway empty `StorageEngine` implementation confirms the trait is implementable
  by a non-legacy backend. Removed once the real columnar engine lands in a future
  phase.

- **`MockProvider` — in-memory `SourceProvider` for unit tests.**
  Supports `insert`, `add_snapshot`, `set_current`, and deterministic content-ID
  hashing; used by all `SourceProvider` shape tests.

- **`storage/README.md`** — documents the `StorageEngine` and `SourceProvider`
  traits and their relationship to `LegacyMemoryStorage`.

### Changed

- **`Session` struct refactored.**
  `index: Option<SymbolTable>` replaced by `engine: Box<dyn StorageEngine>`.
  Public API (`Session::index`, `Session::index_mut`, `Session::engine`,
  `Session::engine_mut`) is fully backwards-compatible.

- **`exec_find`, `exec_show`, `exec_change` go through `StorageEngine`.**
  All three `exec_*` modules now call trait methods instead of concrete
  `SymbolTable` methods. Zero direct `SymbolTable` references remain in
  `crates/forgeql-core/src/engine/**` (one surviving doc-comment in
  `exec_session.rs` is intentional).

- **Cache version unchanged** — storage layer is a pure structural refactor;
  no enrichment field values changed; existing `.forgeql-index` files remain valid.

## [0.43.0] — 2026-04-29

### Fixed

- **BUG-05 / BUG-NEW-01 / BUG-NEW-03: `param_count` and aggregate counts inflated by C++ lambdas.**
  `count_params` previously performed a full DFS of the function subtree, counting every
  `parameter_declaration` node — including those inside lambda bodies embedded in the
  function body. Fixed with `find_param_list_shallow`, which locates the function's own
  `parameter_list` by stopping DFS recursion at `compound_statement` (the body), and
  bounded-DFS variants for `return_count`, `goto_count`, `string_count`, and `throw_count`
  that stop at `lambda_expression` nodes. Regression tests added for `outerNoParams`
  (0 params + lambda with 2), `outerTwoParams` (2 params + lambda with 3), and
  `outerOneReturn` (1 outer return + lambda with 1).

- **BUG-06: `is_magic = true` false-positives for numbers in named-constant contexts (C++).**
  Enum enumerators (`enum E { A = 8 }`) and `const` variable initialisers
  (`const int kBuf = 256`) were incorrectly flagged as magic. Fixed by checking the
  direct parent node against a new config field `constant_def_parent_kinds`
  (`["preproc_def", "enumerator", "init_declarator"]` for C++). Numbers in bare
  expressions (`arr[64]`, `if (x == 42)`) remain magic. Tests added:
  `number_is_magic_false_enumerator`, `number_is_magic_false_const_var`,
  `number_is_magic_true_bare_expr_regression`.

- **BUG-13: `SHOW members OF` fails for types with many reference-only index rows.**
  `resolve_symbol` last-indexed-wins returned a bare identifier reference (no
  `member_count`) for types appearing hundreds of times as pointer arguments. Replaced
  with `resolve_type_symbol`: fast path checks whether the resolved row already has
  `fql_kind = struct/class/enum` and `member_count > 0`; slow path scans all candidates
  via `find_all_defs` and picks the last type definition with members.

### Changed

- **Cache version bump — `CURRENT_VERSION` advanced from 26 → 27.**
  The BUG-05 and BUG-06 fixes alter enrichment field values for existing rows
  (`param_count`, `return_count`, `goto_count`, `string_count`, `throw_count`,
  `is_magic`). Existing `.forgeql-index` files are invalidated and rebuilt on next
  session open.

- **C++ language config: `nested_function_body_kinds` and `constant_def_parent_kinds`.**
  Two new optional arrays in `cpp.json` (both `#[serde(default)]` — empty for Rust and
  Python). `nested_function_body_kinds: ["lambda_expression"]` drives bounded-DFS in
  the metrics enricher. `constant_def_parent_kinds: ["preproc_def", "enumerator",
  "init_declarator"]` drives magic-number suppression in the numbers enricher.

## [0.42.0] — 2026-04-28

### Refactored

- **metrics.rs** — extracted `count_descendants_where` shared DFS closure; `count_descendants_by_kind` and `count_descendants_by_kinds` delegate to it.
- **engine.rs** — extracted `find_pred_string` helper (removes 4 repeated `find_map` blocks); extracted `passes_glob_filter` helper (removes 3 duplicated IN/EXCLUDE glob-check blocks).
- **numbers.rs** — consolidated `detect_format` case pairs using `eq_ignore_ascii_case`/char arrays; extracted `is_hex_digit_suffix` to deduplicate guard shared by `detect_suffix_with_table` and `strip_suffix_with_table`; replaced double `trim_start_matches` chains in `parse_value` with `strip_prefix(...).or_else(...)`.
- **data_flow_utils.rs** — moved `has_descendant_kind` from `member.rs` and `scope.rs` into the shared module; `contains_kind` now delegates to `find_descendant_by_kind` (removes 26-line DFS loop copy); `collect_parameter_names` uses `children()` iterator.
- **member.rs** — `enclosing_type_name` delegates to `enclosing_type_node` (removes duplicated while-loop); `enclosing_owner_name` likewise delegates to `enclosing_type_node`.
- **todo.rs / recursion.rs / fallthrough.rs** — replaced `for i in 0..child_count()` / `node.child(i)` patterns with idiomatic `node.children(&mut cursor)` iterators; `check_switch_cases` converted to `filter+collect`.
- **scope.rs** — two `named_child_count` indexed loops replaced with `named_children().find()` and `named_children().filter().any()`.
- **exec_find.rs** — `find_symbols` fast-path and normal-path used identical QueryResult construction; extracted `make_result` closure. Applied `passes_glob_filter` to `find_usages`.
- **redundancy.rs** — `has_update_descendant` 27-line DFS cursor loop replaced with `contains_kind` delegation (3-line `any()` chain).
## [0.41.0] — 2026-04-27

### Fixed

- **Bug: `UsageSite.path_id` not remapped during parallel-build merge.**
  In the parallel index build, each file is parsed into its own per-file `SymbolTable`
  with its own `PathPool`. Usage sites were added with `path_id` values valid only in
  that per-file pool. During `merge()`, row IDs were correctly remapped via
  `reassign_intern_ids`, but usage site `path_id`s were merged verbatim — making every
  usage site point to whatever happened to be at that numeric slot in the global pool
  (typically the first interned path). Fixed by remapping each `UsageSite.path_id`
  through `other.strings.paths → self.strings.paths` in the merge loop, identical to
  how row IDs are remapped. Caught by live regression testing on zephyr-andre (2.7 M
  symbols, 4.38 M usage sites).

### Changed

- **`UsageSite.path: PathBuf` → `path_id: u32`** — the 4.4 M usage sites on a
  zephyr-scale session each previously owned a full heap-allocated `PathBuf`.  With only
  14,234 distinct paths in the workspace that is a **308× duplication** of path data.
  `path_id` is now an interned ID into the existing `ColumnarTable.paths` pool, which
  already held every unique path for `IndexRow`.  Resolving a site's path at query time
  costs a single array index (`paths.get(path_id)`) with zero allocation.
  - **Cache version bump** — `CURRENT_VERSION` advanced from 25 → 26; existing `.forgeql-index`
    files are invalidated and will be rebuilt on next session open.
  - **`add_usage`** interns the path via `self.strings.paths.intern(path)` before pushing.
  - **`show_callers` byte cache** keyed by `u32` instead of `PathBuf` — eliminating the
    `clone()` per site.
  - **`purge_file`** uses `path_id` comparison instead of `PathBuf` equality.
  - **`mem_estimate`** updated: `UsageSite` is now fully fixed-size; the per-site
    `PathBuf` heap and capacity terms are removed from the usages estimate.
  - Estimated RAM saving on zephyr-andre (4.38 M sites × avg 40-byte path): **~280 MB**.

## [0.40.1] — 2026-04-27

### Changed

- **Option A: `index_row_into_secondaries` free function** — the 12-line secondary-index
  update block that appeared identically in `push_row`, `merge`, and `rebuild_indexes_from_rows`
  is now a single private free function. The free-function design (not a `&mut self` method)
  enables Rust split-borrows: `&self.strings` (immutable) coexists with
  `&mut self.name_index`, `&mut self.kind_index`, `&mut self.fql_kind_index`,
  `&mut self.stats`, and `&mut self.trigram_index` simultaneously.
  **Commands**: `CHANGE FILE ... LINES n-m WITH <<RUST ... RUST`

- **Option B: `IndexStats` u32 keys** — `IndexStats::by_fql_kind` and
  `IndexStats::by_language` changed from `HashMap<String, usize>` to `HashMap<u32, usize>`
  (interned pool IDs). Eliminates two `to_owned()` String heap allocations per row on all
  three hot paths. Added `IndexStats::resolved_by_fql_kind` and
  `IndexStats::resolved_by_language` helpers that convert IDs to strings lazily at
  query-output time only (`exec_find` GROUP BY fast path, `exec_source` SHOW STATS).
  `result.rs`, `compact.rs`, and `cache.rs` unchanged (no cache version bump required —
  `IndexStats` is always rebuilt from rows on cache load, never persisted).
  **Commands**: `CHANGE FILE ... LINES n-m WITH <<RUST ... RUST`

### Fixed

- **`mem_estimate()` now accounts for `field_keys` and `field_values` pool bytes.**
  The two `StringPool`s added in v0.40.0 for field interning were omitted from the
  `strings_bytes` total in `MemEstimate`.  On zephyr-scale sessions they represent
  ~20–30 MB of interned enrichment key/value strings that were previously invisible in
  `SHOW STATS`.

## [0.40.0] — 2026-04-26

### Added

- **jemalloc global allocator** — the binary now uses `tikv-jemallocator` with
  `background_threads` instead of the system glibc malloc. jemalloc's decay
  background thread returns dirty/muzzy pages to the OS via `madvise()` after
  large frees. On zephyr-scale sessions (2.7 M symbols, ~4.9 GB live data) this
  eliminates the post-`ROLLBACK` RSS spike: RSS stays at ~4.8 GB instead of
  climbing to 15+ GB when glibc would hold freed pages as internal free lists.

### Fixed

- Post-`ROLLBACK` RSS bloat on large sessions. `ROLLBACK` calls `drop_index()`
  (frees ~4.7 GB) then `resume_index()` (re-allocates ~4.7 GB); glibc never
  returned the freed pages. jemalloc recovers them within seconds.

## [0.39.0] — 2026-04-26

### Added

- **`SHOW STATS [FOR 'session_id']`** — new FQL command that reports per-session
  internal diagnostics: row counts, distinct name/path counts, usage-site counts,
  trigram index size, and a component-by-component heap-memory estimate (rows,
  usages, secondary indexes, trigram, and intern pools). Includes `by_language`
  and `by_fql_kind` breakdowns. When no `FOR` clause is given, all loaded
  sessions are reported.
- **`SymbolTable::mem_estimate()`** — returns a `MemEstimate` struct with
  approximate heap-byte counts for every major component of the index.
  Uses `size_of` for fixed parts and capacity-based accounting for
  `String` / `Vec` / `HashMap` heap allocations.
- **`PathPool::iter()`** — iterate interned paths in insertion order.
- **`TrigramIndex::posting_iter()` / `posting_len()`** — read-only accessors
  over trigram posting lists, used by `mem_estimate()`.

### Fixed

- **`GROUP BY language` (and `GROUP BY node_kind`, `GROUP BY fql_kind`, `GROUP BY path`)
  returned `"(empty)"` for all rows.** `SymbolRow::from_match_with_ctx` was
  using `row.fields.get(field)` for every group field, but `language` etc.
  are structured `SymbolMatch` fields, not entries in the enrichment `fields`
  HashMap. Fixed by matching on the field name and reading from the correct
  struct field before falling back to the HashMap.

## [0.38.7] - 2025-07-25

### Changed
- **PR-E: Remove String fields from `IndexRow`; use ID-only storage.**
  All five top-level string fields (`name`, `node_kind`, `fql_kind`, `language`,
  `path`) have been removed from `IndexRow`; only the compact `u32` ID fields
  remain. String data lives exclusively in `ColumnarTable` (serialised as
  `CachedIndex.strings`). Resolving a field is now a single pool lookup via
  the new `SymbolTable` accessor methods: `name_of`, `node_kind_of`,
  `fql_kind_of`, `language_of`, `path_of`.
- Cache format version bumped 24 → 25; existing caches are automatically
  invalidated and rebuilt.
- `ColumnarTable`, `StringPool`, and `PathPool` now derive
  `Serialize / Deserialize` so the pool is persisted in `CachedIndex.strings`.
- `RowRef<'t>` wrapper added to implement `ClauseTarget` for `(IndexRow, SymbolTable)`.
- `ExtraRow` transit type added in `enrich/mod.rs` for enricher output.
- All enrichers, show functions, engine query paths, filter impls, and
  integration tests updated to use accessor methods instead of string fields.

## [0.38.6] — 2026-04-26 (string-interning-phase-1)

### Added

- **`ColumnarTable` string interning infrastructure (phase 1 — plumbing only).**
  New file `crates/forgeql-core/src/ast/intern.rs` introduces three types:
  - `StringPool` — append-only string interning pool; O(1) amortised intern/lookup.
  - `PathPool` — same pattern typed for `PathBuf`.
  - `ColumnarTable` — composite pool for all five top-level `IndexRow` string fields
    (`name`, `node_kind`, `fql_kind`, `language`, `path`).

  `IndexRow` gains five `#[serde(skip)]` ID fields (`name_id`, `node_kind_id`,
  `fql_kind_id`, `language_id`, `path_id`).  `SymbolTable` gains a `pub(crate)
  strings: ColumnarTable` field and five zero-copy accessor methods (`name_of`,
  `node_kind_of`, `fql_kind_of`, `language_of`, `path_of`).

  IDs are populated on every call to `push_row` and `merge`.  The existing `String`
  fields on `IndexRow` are **kept** (dual-write approach) for full backward
  compatibility with all existing filter/engine code — see phase 2 below.

  **Note — no memory reduction in this release.**  Because the original `String` /
  `PathBuf` fields on `IndexRow` are still present, per-row heap usage
  *increases* by 20 B (five new `u32` IDs) and `ColumnarTable` is an additional
  allocation.  The projected ~1.4 GB → ~300 MB saving (at 8 M symbols) will only
  be realised in **phase 2**, when the duplicated string fields are removed and all
  consumers are migrated to the `*_of()` accessors.

  Cache format version unchanged — the ID fields are `#[serde(skip)]` and are
  rebuilt in O(N) on every index load.  A future cache-version bump will be
  required once the original string fields are removed in phase 2.
## [0.38.5] — 2026-04-26 (rollback-cleanup)

### Fixed

- **ROLLBACK leaves a spurious checkpoint commit in git history.**
  `BEGIN TRANSACTION` creates a `"forgeql: checkpoint '...'"` commit to
  snapshot the worktree (including `.forgeql-index`).  Previously,
  `ROLLBACK` did `git reset --hard <checkpoint_oid>`, which restored the
  worktree correctly but left the branch tip pointing at the checkpoint
  commit — visible in `git log` and VS Code's Source Control graph.

  Fix: after `reset_hard` + `resume_index`, if `oid != pre_txn_oid`
  (i.e. BEGIN actually created a checkpoint commit), a `git soft_reset`
  to `pre_txn_oid` moves the branch ref back to the commit that existed
  before BEGIN, without touching the worktree.  `.forgeql-index` stays
  on disk for the already-completed `resume_index`.

  Edge cases handled:
  - `oid == pre_txn_oid` (nothing was staged at BEGIN time, no checkpoint
    commit was created) → the `soft_reset` is skipped entirely.
  - `soft_reset` fails (e.g. detached HEAD) → logged as a warning;
    correctness of the index is unaffected.

### Added

- `git::head_commit_message(repo)` — returns the HEAD commit message as
  a `String` (no callers yet; kept for future crash-recovery diagnostics).

### Commands used

- `BEGIN TRANSACTION 'pr-c-rollback-cleanup'`
- `CHANGE FILE 'crates/forgeql-core/src/git/mod.rs'` — added `head_commit_message`
- `CHANGE FILE 'crates/forgeql-core/src/engine/exec_transaction.rs'` —
  renamed `_pre_txn_oid` → `pre_txn_oid`; added `soft_reset` to pop the
  checkpoint commit off the branch tip after ROLLBACK
- `VERIFY build 'test-all-before-commit'`
- `COMMIT MESSAGE 'fix: soft_reset to pre_txn_oid after ROLLBACK to remove spurious checkpoint commit'`
## [0.38.5] — 2026-04-26

### Architecture

- **Restored git-as-source-of-truth for transactional rollback.**  This
  was the original 0.29.0 design, broken by later refactors.  The fix
  reverses the "smart-rollback" approach added earlier in PR-C1 in
  favour of a simpler and provably-correct mechanism:
  - `BEGIN TRANSACTION` now flushes the in-memory index to
    `.forgeql-index` *before* `git::stage_and_commit`, guaranteeing
    that the checkpoint commit captures a cache file matching the
    in-memory state.  The cache file is intentionally included in
    checkpoint commits (see `git::CHECKPOINT_EXCLUDED`) for exactly
    this purpose.
  - `ROLLBACK` reverts to: `git reset --hard <oid>` →
    `Session::drop_index` → `Session::resume_index`.  Because the
    checkpoint commit contains the matching cache, `resume_index`
    cache-hits and restores a guaranteed-correct index in
    O(deserialize) — never falls into a full O(N) rebuild.
  - This is more trustworthy than smart-rollback, which depended on
    `dirty_paths`/`changed_files_between` correctly enumerating every
    affected file.  A single missed path could have silently corrupted
    the in-memory index.  The new approach has one invariant —
    "save before stage in BEGIN" — instead of four.

### Performance

- **`CHANGE FILE` no longer flushes the on-disk cache** after every
  mutation.  The in-memory index is updated, `index_dirty = true` is
  set, and the next BEGIN/COMMIT/eviction-time flush picks it up.  On
  Zephyr (~2.7 M rows) this drops single-file CHANGE from ~17–18s to
  ~1s.
- **`Session::flush_if_dirty`** added — cheap no-op when the index is
  in sync, full `save_index` when it has diverged.
- **`Session::index_dirty`** field added; `reindex_files` sets it,
  `save_index` clears it, `mark_index_dirty` lets `COMMIT` force a
  flush after HEAD movement (since the cache's `commit_hash` becomes
  stale even when no rows changed).
- **`Session::drop_index`** added — clears `index/macro_table/cached_commit`
  without saving, used by `ROLLBACK` so `resume_index` reads the
  freshly-restored cache from disk.

### Removed

- The `Session::has_index` accessor (only existed to support the
  smart-rollback fast path; no remaining callers).
- The `PathBuf` import in `engine/exec_transaction.rs` (no longer
  needed once smart-rollback was removed).
- `git::dirty_paths` and `git::changed_files_between` are kept as
  helpers but are no longer called from `exec_rollback`.  They may be
  reused by a future "crash recovery on USE" feature that reindexes
  uncommitted dirty files after a daemon restart.

### Fixed

- **`COMMIT MESSAGE` now flushes the cache after the commit.**  Since
  `squash_commit_on_branch` moves HEAD, the cache's `commit_hash`
  field becomes stale even when no rows changed.  The new
  `mark_index_dirty` + `flush_if_dirty` sequence ensures the on-disk
  cache matches the new HEAD, so the next `resume_index` (e.g. after
  daemon restart) will cache-hit instead of falling through to a full
  rebuild.

### Notes

- TTL eviction is intentionally *not* a flush point: it deletes the
  worktree (and with it the `.forgeql-index` file), so flushing first
  would be wasted work.  Sessions with ongoing transactions preserve
  their cache via the BEGIN-time checkpoint commits, which live in
  the bare repo and survive worktree removal.
- Crash semantics: a daemon kill mid-transaction loses the in-RAM
  checkpoint stack and `last_clean_oid`, but git refs and any
  committed checkpoints survive.  The next `USE` lands at HEAD =
  most-recent-COMMIT (or most-recent-checkpoint OID if a transaction
  was open) with the matching cache restored from git.

### Commands used

- `BEGIN TRANSACTION 'pr-c1-git-as-truth'`
- `CHANGE FILE 'crates/forgeql-core/src/session/mod.rs'` — added
  `index_dirty` field, `flush_if_dirty`, `mark_index_dirty`,
  `drop_index`; cleared/set the flag in `build_index`/`resume_index`/
  `reindex_files`/`save_index`.
- `CHANGE FILE 'crates/forgeql-core/src/engine/exec_transaction.rs'`
  — flush before BEGIN's stage_and_commit; flush after COMMIT's
  squash; replaced 70-line smart-rollback block with 14-line
  reset+resume_index.
- `CHANGE FILE 'crates/forgeql-core/src/engine/exec_session.rs'` —
  removed save_index from `reindex_session`.
- `VERIFY build 'test-all-before-commit'`
- `COMMIT MESSAGE 'arch: git-as-source-of-truth rollback (PR-C1 step 5)'`
## [0.38.5] — 2026-04-25 (continued)

### Performance

- **Path-scoped `post_pass` for incremental re-indexing.**
  `Session::reindex_files` (used by every `CHANGE FILE` and the
  smart-rollback path) was calling each enricher's `post_pass(&mut table)`
  unconditionally, which walked the entire `SymbolTable.rows` vector twice
  per affected enricher.  On Zephyr (~2.7 M rows) this added ~17 s to
  every single-file CHANGE — a regression introduced when post-pass
  enrichers (`control_flow`, `redundancy`) were folded into the
  incremental path.
  - Changed the `NodeEnricher::post_pass` trait signature to
    `post_pass(&self, table, scope: Option<&HashSet<PathBuf>>)`.  `None`
    preserves the old full-table semantics (used by `SymbolTable::build`);
    `Some(&paths)` filters every row iteration to rows whose `path` is in
    the set.
  - Updated `control_flow::post_pass` and `redundancy::post_pass` to
    apply the filter to all three phases (function lookup, CF row scan,
    output writes).  Both algorithms are intra-function so unchanged
    files cannot affect the result — correctness is preserved.
  - `metrics::post_pass` is a no-op (its work moved into `enrich_row`)
    and accepts the new parameter unchanged.
  - All other enrichers (`escape`, `shadow`, `decl_distance`, `todo`,
    etc.) inherit the trait default, which remains a no-op.
  - On Zephyr this turns CHANGE-time post_pass overhead from O(N)
    into O(P × scope_lookup), reducing it from ~17 s to milliseconds.

### Fixed

- **`ROLLBACK` no longer rewrites the on-disk index cache.**
  After `git reset --hard <checkpoint_oid>` the cached
  `.forgeql-index`'s `commit_hash` no longer matches HEAD anyway, so
  immediately calling `save_index` produced a stale-but-fresh blob at
  the cost of ~17 s on Zephyr.  The cache is now left untouched on
  rollback; the next mutation or session shutdown will rewrite it.
  - Commands: `BEGIN TRANSACTION 'pr-c1-scoped-postpass'`,
    `CHANGE FILE 'crates/forgeql-core/src/engine/exec_transaction.rs'
    LINES 217-223 …` (drop `save_index`),
    `CHANGE FILE 'crates/forgeql-core/src/ast/enrich/mod.rs' …` (trait
    signature), `CHANGE FILE
    'crates/forgeql-core/src/ast/enrich/{control_flow,redundancy,metrics}.rs'
    …` (scoped overrides), `CHANGE FILE
    'crates/forgeql-core/src/ast/index.rs' …` (call sites for build +
    reindex_files), `VERIFY build 'test-all-before-commit'`,
    `COMMIT MESSAGE 'perf: scoped post_pass + skip save_index on
    rollback (PR-C1 step 4)'`.
## [0.38.5] — 2026-04-25 (continued)

### Fixed

- **`ROLLBACK` no longer triggers a full O(N) re-index on large workspaces.**
  The 0.29.0 smart-rollback fast path silently broke when the cached
  `.forgeql-index`'s internal `commit_hash` field could not match the new
  HEAD after `git reset --hard <checkpoint_oid>` (the cache was saved with
  the *pre-checkpoint* HEAD, not the checkpoint OID itself), so
  `resume_index` always fell through to `build_index`. On Zephyr
  (~2.7 M symbols) this caused multi-second stalls and pushed RSS from
  ~11 GB to ~29 GB, large enough to trigger OOM kills.
  - Added `git::dirty_paths` (working-tree status query, excluding
    `FORGEQL_CONTROL_FILES`) and `git::changed_files_between` (tree-to-tree
    diff between two commits, also filtering control files).
  - `exec_rollback` now captures dirty working-tree paths *before*
    `git reset --hard`, computes the set of files committed during the
    transaction via `changed_files_between(pre_reset_oid, oid)`, unions
    them, and dispatches an incremental `Session::reindex_files` covering
    only that set. When the union is empty the in-memory index is already
    correct and no work is performed.
  - Falls back to the pre-existing `resume_index` → `build_index` path
    only when the in-memory index is missing (`!session.has_index()`) or
    when an incremental re-index returns an error.
  - Added `Session::has_index` accessor.
  - On the OOM-reproducing test sequence this turns ROLLBACK from a
    multi-GB full rebuild into an O(P) operation (P = changed files).
  - Commands: `BEGIN TRANSACTION 'pr-c1-smart-rollback'`,
    `CHANGE FILE 'crates/forgeql-core/src/git/mod.rs' LINES …` (added
    `changed_files_between` and `dirty_paths`),
    `CHANGE FILE 'crates/forgeql-core/src/engine/exec_transaction.rs'
    LINES …` (replaced rollback body, added `PathBuf` import),
    `CHANGE FILE 'crates/forgeql-core/src/session/mod.rs' LINES …`
    (added `has_index`), `VERIFY build 'test-all-before-commit'`,
    `COMMIT MESSAGE 'fix: restore smart-rollback fast path (PR-C1 step 3)'`.
## [0.38.5] — 2026-04-25

### Performance

- **Posting-list row IDs shrunk from `usize` to `u32`** in
  `SymbolTable::name_index`, `kind_index`, and `fql_kind_index`
  (`ast/index.rs`). On 64-bit hosts this halves the per-entry footprint
  of the three primary secondary indexes — saving roughly 4 bytes per
  posting-list entry. On Zephyr (~2.7 M rows, ~3 M total posting
  entries) this removes ~12 MB of resident overhead with no change to
  query semantics or public API. A `debug_assert!` boundary in
  `push_row` / `merge` / `purge_file` catches the (currently
  unreachable) `> u32::MAX` row count case in tests; release builds
  saturate to `u32::MAX`. Trigram posting lists and `IndexRow` /
  `UsageSite` line/byte fields are deferred to PR-C2 alongside the
  string-interning refactor.
  - Commands: `BEGIN TRANSACTION 'pr-c1-u32-shrink'`,
    six `CHANGE FILE 'crates/forgeql-core/src/ast/index.rs' LINES …`
    operations covering struct fields, `merge`, `push_row`,
    iterator readers, `purge_file`, and tests,
    `VERIFY build 'test-all-before-commit'`,
    `COMMIT MESSAGE 'perf: u32 row-ids in primary secondary indexes (PR-C1 step 2)'`.

### Fixed

- **`purge_file` now rebuilds `IndexStats`** (`ast/index.rs`). The
  incremental purge path used by `reindex_files` previously left
  `stats.by_fql_kind` and `stats.by_language` stale after files were
  edited, deleted, or renamed within a session. `GROUP BY fql_kind` and
  `GROUP BY language` queries could return counts inflated by the
  pre-edit row population. The rebuild now runs in the same loop that
  rebuilds `name_index`, `kind_index`, `fql_kind_index`, and the
  trigram index — keeping every persisted-or-derived structure
  invalidation hook in one place. Regression test
  `purge_file_rebuilds_index_stats` enforces this for every future
  refactor.
  - Commands: `BEGIN TRANSACTION 'pr-c1-stats-purge'`,
    `CHANGE FILE 'crates/forgeql-core/src/ast/index.rs' LINES 451-481 WITH ...`,
    `VERIFY build 'test-all-before-commit'`,
    `COMMIT MESSAGE 'fix: purge_file rebuilds IndexStats (PR-C1 step 1)'`.

## [0.38.4] — 2026-04-25

### Performance

- **Trigram inverted index for fast `MATCHES` / `LIKE` substring queries**: A new
  `TrigramIndex` (in `ast/trigram.rs`) maps every 3-byte window of each symbol
  name to the set of row indices containing it. Built in O(N) during `push_row`
  / `merge`; not serialized (rebuilt on warm reconnect).
  - `MATCHES '^k_thread_.*$'` — extracts literal `k_thread_`, narrows via
    trigram, then applies the full regex only to those candidates. Was 40 s on
    Zephyr; now < 50 ms.
  - `LIKE '%CONFIG_BT%'` — extracts literal `CONFIG_BT`, narrows via trigram,
    then applies the full LIKE check. Patterns with no extractable literal of
    length ≥ 3 fall through to the existing full-scan path unchanged.

- **`TrigramIndex::insert` dedup O(n) instead of O(n²)**: per-name `seen`
  collection switched from `Vec::contains` to `HashSet`, fixing slow warm-reload
  on large comment nodes (up to ~9 KB names on Zephyr).

### Fixed

- **`LIKE` / `MATCHES` trigram pre-filter ignored ASCII case folding**: The
  trigram index was built over original-case bytes, so `WHERE name LIKE '%MOTOR%'`
  and `WHERE name MATCHES '(?i)Motor'` returned 0 rows. Index now lowercases at
  insert and lookup, restoring `like_match` / `(?i)` semantics.

- **`SymbolTable::purge_file` did not rebuild the trigram index**: After an
  incremental file purge, `trigram_index` retained stale row indices while the
  other secondary indexes were rebuilt. Fixed by clearing and re-populating
  alongside `name_index` / `kind_index` / `fql_kind_index`.

- **`fql_kind` / `node_kind` predicates silently dropped when combined with
  `LIKE`**: When a `LIKE` pattern produced trigram candidates, `WHERE fql_kind =
  'function'` was incorrectly stripped from per-row evaluation, leaking
  non-matching symbol kinds into results. Fixed with `use_fql_kind_index` /
  `use_kind_index` flags that only strip a predicate when its index actually
  supplied the candidates.

- **Multiple `WHERE name LIKE` clauses — second and subsequent silently
  dropped**: `non_usages_preds` was stripping all `name LIKE` predicates;
  only the first was ever evaluated. Fixed by removing the blanket LIKE strip.

## [0.38.3] — 2026-04-25

### Performance

- **Eliminated redundant `SymbolTable` rebuild after `build_index`**: Previously,
  `build_index` round-tripped through `CachedIndex` (move into cache → save →
  move back out), triggering a full O(N) secondary-index rebuild via `push_row`
  for every symbol. A new `CachedIndex::save_from_parts` method borrows the
  freshly-built `SymbolTable` to serialize it without consuming it, eliminating
  the rebuild entirely.

- **Anchored `MATCHES '^name$'` routed through `name_index`**: Queries like
  `WHERE name MATCHES '^gpio_pin_set$'` previously compiled the regex and
  evaluated it against every row (O(N) — 146 s on 2.7 M Zephyr symbols). A new
  `extract_anchored_literal` function detects `^literal$` patterns with no
  special chars and routes them directly through the O(1) `name_index` hash map.

- **`usages_count: u32` precomputed on `IndexRow`**: The per-row usage count is
  now stored directly on `IndexRow` (populated by `populate_usage_counts()` at
  build time). Engine queries that filter or sort by `usages` read
  `row.usages_count` directly instead of looking up the `HashMap<String, Vec<_>>`
  on every row. Cache version bumped to 24.

- **`IndexStats` for O(1) `GROUP BY fql_kind / language`**: `SymbolTable` now
  carries a `stats: IndexStats` field with pre-aggregated counts by `fql_kind`
  and `language`, maintained in `push_row` and `merge`. A new
  `try_group_by_stats_fast_path` in `exec_find.rs` short-circuits unfiltered
  `GROUP BY fql_kind ORDER BY count DESC` queries to return instantly instead of
  scanning all symbols (11 s → <5 ms on Zephyr).

## [0.38.2] — 2026-04-25

### Bug Fixes

- **Cross-source worktree corruption fixed**: When the same `(branch, alias)`
  pair was used against two different sources (e.g. `USE foo.main AS 'r'`
  followed by `USE bar.main AS 'r'`), both sessions resolved to the same
  worktree directory `worktrees/main.r/`. The second `USE` silently took
  ownership of the first source's worktree — including its `.forgeql-index`
  and any uncommitted changes — leading to confusing query results and
  potential data loss. Two changes harden this:
  - **Worktree directory now includes the source name**: layout is
    `worktrees/{source}.{branch}.{alias}/`, making collisions impossible by
    construction. (`exec_source.rs`: `use_source()`)
  - **`worktree::create()` validates the gitdir backlink** when reusing an
    existing directory and refuses to silently hand it to a different bare
    repo. Returns a clear error instead. (`git/worktree.rs`: `create()`)
  - Auto-reconnect (`exec_session.rs`: `try_auto_reconnect()`) updated to
    parse the new `{source}.{branch}.{alias}` layout.
  - Pre-0.38.2 worktrees on disk become orphans (auto-reconnect skips them
    with a debug log). Remove them manually if disk space matters.

## [0.38.1] — 2026-04-25

### Added
- **`CREATE SOURCE` now writes a sidecar config template** on first clone.
  A commented `.forgeql.yaml` (e.g. `myrepo.forgeql.yaml`) is placed next to
  the bare repo in the ForgeQL data directory, giving newcomers a ready-to-edit
  file with all `line_budget` defaults and a commented `verify_steps` example.
  The call is idempotent (skipped when the file already exists) and non-fatal.
  The result message tells the agent the exact path.
  - `ForgeConfig::write_sidecar_template()` added to `crates/forgeql-core/src/config.rs`
  - Wired into `create_source()` in `crates/forgeql-core/src/engine/exec_source.rs`
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [0.38.0] — 2026-04-19

### Bug Fixes

- **Slashes in USE alias no longer break worktree creation**: When the alias in `USE source.branch AS 'alias'` contained `/` (e.g. `refactor/main-rs-split`), the slash was embedded verbatim in the worktree directory name, creating a nested path that failed with "failed to make directory". Now `/` is replaced with `-` in both the branch and alias components of the filesystem worktree name. The git branch name (`fql/branch/alias`) is unaffected since git refs support slashes natively. (`exec_source.rs`: `use_source()`)

---
## [0.37.5] — 2026-04-19

### Bug Fixes

- **Fixed `= 69` value leak in text formatter**: The Display impl for QueryResult no longer accesses `SymbolMatch.fields` directly. All formatters now use `projected_rows()` which extracts only display-relevant fields.

### Refactor

- **Unified output projection via `SymbolRow`**: Extended `SymbolRow` with `usages`, `count`, `metric_value`, `group_key` fields. Added `QueryContext` and `projected_rows()` as the single entry point for all formatters.
- **Text formatter (display.rs)**: Rewritten to use `projected_rows()` exclusively.
- **Compact formatter (compact.rs)**: `compact_find_grouped_by_kind()` rewritten with `group_rows_by_kind()`/`group_rows_by_field()` operating on `&[SymbolRow]`.
- **JSON serialization (convert.rs)**: `to_json()`/`to_json_pretty()` now build custom JSON for Query results using projected rows — raw `SymbolMatch.fields` HashMap is never serialized.

### Removed

- **Deleted `to_csv()`**: Dead code, replaced by `to_compact()` long ago.
- **Deleted `SymbolRow::from_match()`**: No longer needed; all callers use `projected_rows()`.

---

## [0.37.4] — 2026-04-19

### Tests

- **373 new unit tests across 11 modules** (`filter`, `transforms/diff`, `budget`, `compact`, `enrich/numbers`, `enrich/guard_utils`, `enrich/control_flow`, `ast/index`, `result`, `transforms/change`, `parser`). Covers edge cases for `like_match`, glob matching, all predicate operators, `apply_clauses` offset/having/group-count/AND semantics, diff hunk building, budget sweep/snapshot, compact helpers, number format/suffix parsing, cfg guard stripping, max paren depth, `find_all_defs`, `suggest_similar`, `compact_name` boundary, `ShowResult` display for all 6 variants, CRLF/mixed line endings, parser round-trips, and error paths.

## [0.37.3] — 2026-04-18

### Refactor

- **Deduplicated `node_text()` helper**: Moved 5 identical copies (from `forgeql-lang-{cpp,rust,python}/src/lib.rs` and `macro_expand.rs`) into a single `pub fn node_text()` in `forgeql_core::ast::lang`. All lang crates now import from core.

## [0.37.2] — 2026-04-18

### Bug Fixes

- **Release build broken by `#[cfg(feature)]` import**: `generate_session_id` and `Arc` were imported unconditionally in `exec_session.rs` / `exec_source.rs` but only available under `test-helpers` feature, causing `cargo build --release` to fail. Imports now correctly gated with `#[cfg(feature = "test-helpers")]`.

### Refactor

- **Engine submodule import hygiene**: Removed blanket `#![allow(unused_imports)]` from all 6 `exec_*.rs` files and pruned each file's imports to only what it actually uses (−153 lines of dead imports).

### Added

- **`test-all-before-commit.sh` script**: Pre-commit gate that runs `cargo fmt --all` → fmt check → clippy → release build → tests → SMS regression (budget=5000 with CSV). Designed for `VERIFY build` with compact output (`tail -40` per step).

## [Unreleased]

## [0.54.14] — 2026-05-25 — P2-A: split exec_show match arms into private methods

### Changed

- **`crates/forgeql-core/src/engine/exec_show.rs`** — extracted every `match op { … }` arm
  of `exec_show` (397 lines) into a dedicated private method:
  - `exec_show_context` — resolves symbol + calls `show::show_context`
  - `exec_show_signature` — resolves symbol + calls `show::show_signature`
  - `exec_show_outline` — delegates to `engine.show_outline_for_file`
  - `exec_show_members` — resolves type symbol + calls `show::show_members`
  - `exec_show_body` — resolves body symbol + calls `show::show_body`
  - `exec_show_callees` — resolves body symbol + calls `show::show_callees`
  - `exec_show_lines` — delegates to `show::show_lines`
  - `exec_show_find_files` — full FindFiles clause pipeline (fast-path + filesystem walk);
    returns `Result<serde_json::Value>`; annotated `#[expect(clippy::too_many_lines)]`
  - Four methods that do not access `self` are associated functions (`Self::` call sites);
    four that call `get_or_parse_for_show` / `lang_registry` remain `&self` methods.
  - `exec_show` itself is now a 27-line dispatcher; `#[expect(clippy::too_many_lines)]`
    attribute and unused `let root = workspace.root()` binding removed.
  - Added `storage::StorageEngine` to imports for parameter typing in the new methods.

### Added

- **Phase 05 — columnar storage parity gate** (`tests/parity_find.rs`):
  - Opens a live session against a real registered source via `USE <source>.<branch> AS 'parity'` through `ForgeQLEngine::execute()` — the same parser → IR → `use_source` pipeline that the MCP `run_fql` tool uses.
  - Runs a ≥200-query corpus against both the legacy and columnar backends, canonicalising results to `(name, fql_kind, line)` sorted tuples for SET-equality comparison.
  - Configured via `FORGEQL_DATA_DIR` (required), `PARITY_SOURCE` (default: `zephyr-andre`), `PARITY_BRANCH` (default: `main`).
  - Skips gracefully (prints a message and exits successfully) when `FORGEQL_DATA_DIR` is unset or the source is not registered — never fails due to missing external infrastructure.
  - Gate command: `FORGEQL_DATA_DIR=~/.forgeql cargo test --package forgeql-core --test parity_find`
- **`session_has_columnar` helper** (`engine/exec_session.rs`): `#[cfg(feature = "test-helpers")]` method that returns `true` when the named session has a columnar backend installed.
- **Dedup in `ColumnarStorage::find_symbols`**: Removes duplicate `SymbolMatch` rows (same `name + fql_kind + path + line`) using a `HashSet<DedupeKey>`, matching legacy backend's index uniqueness guarantee.
- **`PythonLanguageInline` in language registry** for the parity test, alongside the existing `CppLanguageInline` and `RustLanguageInline`.

### Bug Fixes

- **`overlay_parity` baseline dedup**: `overlay_find_symbols_matches_legacy_merged` now deduplicates the legacy baseline per-file using `HashSet<(name, fql_kind, path, line)>` — matching the columnar storage dedup — so the two baselines are comparable on large corpora.

### Changed

- **`forgeql-core` crate refactored into focused submodules** (5 atomic commits):
  - `engine.rs` (72 methods, 118 KB) → `engine/exec_{source,find,show,change,transaction,session}.rs` (committed `3fad20b6`)
  - `parser/mod.rs` → `parser/{helpers,clauses,find,change,transaction}.rs` (committed `e96d0c72`)
  - `result.rs` → `result/{display,convert}.rs` (committed `840ea325`)
  - `ast/show.rs` → `show/{body,members,callees}.rs` (committed `dfb94aee`)
  - `filter.rs` → `filter/impls.rs` for `ClauseTarget` implementations (committed `4c19c9ae`)
  - All 174+ tests pass after every commit; zero public-API changes.

### Added

- **MATCHING WORD modifier**: `CHANGE FILE ... MATCHING WORD 'pattern' WITH 'replacement'` wraps the pattern in `\b...\b` regex word boundaries, preventing replacement of compound terms (e.g. `field_declaration` is not touched when replacing `declaration`). Without `WORD`, behavior is unchanged (plain substring match).

### Bug Fixes

- **Duplicate verify_steps names**: `.forgeql.yaml` loading now rejects duplicate step names with a clear error message instead of silently using last-one-wins semantics.

## [0.37.1] — 2026-04-18

### Bug Fixes

- **`FIND globals WHERE node_kind = ...` silently dropped predicate**: When `FIND globals` (which implicitly adds `WHERE fql_kind = 'variable'`) was combined with an explicit `WHERE node_kind = '...'`, the `node_kind` predicate was stripped from post-filters because `kind_exact.is_some()` didn't account for the index-selection priority (`fql_kind` wins). Result: all file-scope variables returned unfiltered. Now only strips the `node_kind` predicate when it was actually used for the index shortcut.

### Refactor

- **`forgeql` crate: split `main.rs` into focused modules** — `cli.rs` (Clap structs + `detect_mode`), `session.rs` (injectable-IO session persistence), `execute.rs` (FQL resolution + formatting), `runner/` (repl, pipe, one_shot, mcp_stdio), `main.rs` reduced to ~82-line orchestrator. 56 new unit tests.

### Improved

- **SMS test coverage**: Added 19 missing enrichment fields to `syntax.json` (`branch_count`, `cast_count`, `enclosing_fn`, `enclosing_type`, `expanded_has_escape`, `expanded_reads`, `expansion_depth`, `expansion_failed`, `expansion_failure_reason`, `guard`, `guard_branch`, `guard_defines`, `guard_group_id`, `guard_kind`, `guard_mentions`, `guard_negates`, `has_cast`, `macro_def_line`, `macro_expansion`). Corrected stale `find_globals` notes from `node_kind = 'declaration'` to `fql_kind = 'variable'`.

## [0.37.0] — 2026-04-17
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
- **Imp 2.4 (partial)**: `FIND` queries using `WHERE text` or `WHERE content` now return a clear error instead of silently returning 0 results. The `text` field is only available on commands that return source lines (`SHOW body`, `SHOW LINES`, `SHOW context`).

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


