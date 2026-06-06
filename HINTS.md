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