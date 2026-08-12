# ForgeQL Codebase Hints

Short, durable facts discovered while working in this codebase.

## Output rendering & SHOW MORE buffer
- `crate::compact::to_compact` (crates/forgeql-core/src/compact.rs) is the canonical
  CSV renderer; it dispatches per `ForgeQLResult` variant. `VerifyBuild` renders via
  `compact_verify` as a header row + raw newline-delimited log (not JSON).
- The single CSV render boundary for MCP is `mcp.rs::run_fql` (`compact::to_compact`).
  Over-cap buffering is applied there via `finalize_csv` → `buffering_params`.
- `crate::showmore` (crates/forgeql-core/src/showmore.rs) owns the `.forgeql-showmore`
  buffer: `write_buffer`/`read_buffer`/`Buffer::window` + `finalize`. The buffer stores
  rendered lines (header + content), one per line, with original 1-based indices.
- `SHOW MORE` is an engine command: grammar `show_more_stmt` (forgeql.pest),
  IR `ForgeQLIR::ShowMore { window, clauses }` + `ShowMoreWindow`, parser
  `parse_show_more_stmt`/`parse_show_more_window`, engine `exec_show_more`
  (exec_show.rs). It builds a `ShowContent::Lines` result and reuses the same
  `eval_predicate` retain loop + `apply_show_lines_cap` as `SHOW LINES`, so
  `WHERE text` / `LIMIT` work for free.

## Git exclusion lists (crates/forgeql-core/src/git/mod.rs)
- `CLEAN_COMMIT_EXCLUDED` — files kept out of user-facing squash commits
  (`.forgeql-index`, `.forgeql-session`, `.forgeql-columnar-delta`,
  `.forgeql-checkpoints`, `.forgeql-showmore`).
- `CHECKPOINT_EXCLUDED` — files kept out of `BEGIN TRANSACTION` checkpoints
  (only `.forgeql-session` + `.forgeql-staging`). A file in CLEAN but NOT in
  CHECKPOINT (delta, showmore) is restored by `ROLLBACK`'s `git reset --hard`
  yet never appears in published history.

## Config (crates/forgeql-core/src/config.rs)
- `OutputConfig { find_limit, show_lines }` and `VerifyStep.summary`
  (`SummaryConfig { direction, lines }`) are loaded from `.forgeql.yaml` and
  frozen onto the session at `USE` time (exec_source.rs), so mid-session config
  edits cannot change live behavior.

## Clippy gotcha
- `parse_statement` (parser/mod.rs) carries `#[allow(clippy::too_many_lines)]`
  on the line directly above it. Inserting a helper function *between* that
  attribute and `fn parse_statement` silently re-targets the attribute — keep the
  attribute glued to `fn parse_statement`.

## Adding a language plugin (crates/forgeql-lang-*)

- One crate per language, mirroring `forgeql-lang-markdown`: `Cargo.toml`,
  `config/<lang>.json` (embedded via `include_bytes!`), and `src/lib.rs`
  implementing `LanguageSupport` (`name`, `extensions`, `tree_sitter_language`,
  `extract_name`, `map_kind`, `config`). A node becomes a queryable row iff
  `extract_name` returns `Some`.
- Wiring (4 spots): root `Cargo.toml` (members + default-members + a
  `tree-sitter-<lang>` dep + an internal `forgeql-lang-<lang>` path dep),
  `crates/forgeql/Cargo.toml` dep, and `crates/forgeql/src/main.rs` (import +
  `Arc::new(...)` in the startup `LanguageRegistry`).
- Addressability (node_id) is a SEPARATE gate from being a row: a row only gets
  an ordinal/`node_id` when its `fql_kind` is in `is_addressable_fql_kind`
  (crates/forgeql-core/src/ast/index/file_indexer.rs). `fql_kind` values are
  free-form per language (e.g. markdown `heading`; json/yaml `pair`/`object`/
  `array`) — add new ones to that `matches!` list to make them editable.
- JSON/YAML name objects/mappings after a `name`/`id`/`key`/`title`/`alias`
  member so each entry of a data file (e.g. the `tests/golden/*.json` cases) is
  individually addressable; repeated keys stay distinct via parent_ordinal +
  content_hash in the ordinal key.
- With columnar configured, `build_index` returns an EMPTY in-memory table by design (inline segment emit) — any path that falls back to the legacy table must rebuild it with `Session::build_fallback_index` (no inline ctx) or it serves zero rows as success
- The row budget helpers live in `columnar_storage/fast_paths.rs` and are re-exported `pub(in crate::storage)` from `columnar_storage.rs` for the legacy backend and the dirty-union check — one bound, several enforcement sites
- `OverlayBuilder::step1_open_segments` refuses a missing/unreadable segment (capped listing, provider dir named once); a COMMIT over a vanished base segment fails and retries cleanly after restore
- `find_usages`' literal tier reads files via `root.join(path)` — tests must pass the storage's real worktree root, not "."
- The test gate auto-fmts the worktree before checking: re-FIND handles after a gate run, write rustfmt-canonical WITH payloads
- `SegmentMeta.dedup_row_count` (overlay/format.rs, FQOV v5) stores each segment's
  exact distinct `(name_id, fql_kind_id, line)` count at overlay build;
  `Overlay::dedup_total()` sums it, and the merged kind postings are canonical-only,
  so `prefilter_kind(K).len()` is the deduped per-kind count — honest stream totals
  come from storage, never an FST walk.
- The name streams (`try_order_by_name_fast_paths`, query/find.rs) serve
  `ORDER BY name [DESC] [WHERE fql_kind =] LIMIT` and the bare `LIMIT`; they decline
  on a non-empty dirty overlay, duplicated source paths, and `need > find_max_rows()`.
- `order_cmp` tie-break is `(name, line, path, fql_kind)` — total on distinct rows;
  the legacy dedupe key is the matching `(name_id, path_id, fql_kind_id, line)`.
