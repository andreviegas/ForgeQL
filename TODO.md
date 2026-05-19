# ForgeQL — TODO

## Session architecture

- [x] Wire `SessionCoords` into `exec_source.rs`: replace inline alias≠branch guard,
  `budget_branch` derivation, hardcoded `"anonymous"`, worktree dir construction, and git branch
  format; changed silent cross-source alias collision to a hard error; replaced all 5 ad-hoc
  `data_dir.join("worktrees")` sites with `SessionCoords::worktrees_root()`
- [ ] Add `resolve_commit()` helper in `worktree.rs` (try `find_branch` first, fall back to
  `revparse_single`) to support short-SHA `USE` references
- [x] Change session map key from bare alias to `"{user}:{source}:{branch}:{alias}"`; add
  `user_id: &str` to `execute()`; fix `require_session` — map key is now the full
  four-field `SessionCoords::map_key()` token; `execute()` accepts `Option<&SessionCoords>`;
  MCP/CLI callers decode via `SessionCoords::from_session_id()` before entering the engine
- [ ] Thread real user identity from MCP connection into `execute()`;
  add `user_id: Option<String>` to `RunFqlParams` once JWT/API-key auth is wired in
- [ ] **Opaque wire session token** — return a UUID or HMAC instead of the raw
  `user:source:branch:alias` map key to clients; do this together with JWT auth so the
  two land in the same PR. Preferred approach: HMAC(server-secret, map\_key) — no
  side-map, survives restarts, zero persistence changes to `.forgeql-session`; downside
  is a required server secret (env var). UUID variant needs the token stored in the
  sentinel for warm reconnect and a `token_map: HashMap<String,String>` kept in sync
  with session lifecycle (create / drop / eviction). Defer until auth is implemented.
- [x] Replace `prune_orphaned_worktrees` + `try_auto_reconnect` with a single
  `restore_sessions_from_disk()` called once at MCP startup; extend `.forgeql-session`
  sentinel with `source`/`branch`/`alias`/`user` so warm sessions are restored without
  git traversal; fixed the always-false live-session guard bug in the process

## Session lifecycle

- [ ] `SHOW SESSIONS` command — list active sessions (alias, source, branch, user, last-active)
- [ ] `USE REFRESHED` syntax — fetch from remote before opening the worktree
- [ ] Session TTL dirty flag in `.forgeql-session` sentinel: `dirty=true` prevents GC eviction
  until the session is explicitly dropped

## Query language

- [ ] Full-text index — surface identifiers inside **comments and string literals**
  (invisible to the AST index) and in non-code files (`.cmake`, `.md`, etc.);
  merge results into `FIND usages`; add a `FIND text` grep-style command

## Session startup

- [x] Defer index loading at startup — restore only session metadata from sentinel files; load the columnar index on the first `USE` command for that session
- [x] Fix checkpoint empty-stack write — remove the checkpoint file after a full ROLLBACK instead of writing an unvalidatable empty record, eliminating spurious startup warnings

## Overlay — mmap zero-copy (RAM sharing)

See `data/future-plans/mmap-zero-copy-overlay-plan.md` for the full plan.

- [x] Replace the overlay parity unit test with a golden-value integration test against the frozen `zephyr-andre.zephyr-main` branch
- [x] Switch overlay open from full-file heap read to mmap — eliminate the per-session heap copy of the entire overlay file
- [x] Make the FST zero-copy: `SegmentReader` uses `FstMap<MmapSlice>` (no `to_vec()`); `Overlay` moves bytes into FST (no `.clone()`)
- [ ] Make name-postings zero-copy slice views into the mmap — no heap duplication when multiple sessions share the same commit
- [ ] New FQOV v3 binary format — replace the bincode payload with a TOC-based layout so all structures (row table, bitmaps, FST) are zero-copy; delegates all RAM management to the OS page cache

## Overlay — path acceleration (RAM + speed)

See `data/future-plans/overlay-path-acceleration-plan.md` for the full plan.

### Phase 0 — `GROUP BY file` fast-path (no format change)

- [ ] Add `fn group_by_file_fast_path(clauses: &Clauses) -> bool` — returns `true` when
  `group_by == Some(GroupBy::File)`, no WHERE predicates, no HAVING
  (`columnar_storage.rs`)
- [ ] In `find_symbols`, when the fast-path fires, iterate `overlay.segments()`, skip entries
  that don't pass `passes_resolve_glob`, build one `SymbolMatch { path, count: row_count }`
  per segment, call `apply_clauses`, return early — zero segment files opened
  (`columnar_storage.rs`)
- [ ] Integration test: `FIND symbols GROUP BY file ORDER BY count DESC LIMIT 5` against
  frozen zephyr — assert ≤ 5 rows, each `count > 0`, wall time < 100 ms

### Phase 1 — `ORDER BY name LIMIT N` fast-path (no format change)

- [ ] Add `fn order_by_name_fast_path(clauses: &Clauses) -> bool` — `order_by == Name`,
  `limit` is `Some`, no GROUP BY, no WHERE predicates (`columnar_storage.rs`)
- [ ] Extend `SegmentReader` with `fn materialize_one_row(&self, local_row_idx: u32, source_path: &Path) -> Option<SymbolMatch>` if not already present (`segment_reader.rs`)
- [ ] Add `fn stream_names_asc(&self, offset: usize, limit: usize) -> Vec<SymbolMatch>`:
  walk `overlay.name_fst.stream()`, skip `offset` entries, decode `name_postings`,
  resolve `row_table[id]` → `RowPtr`, call `materialize_one_row`, stop after `limit` rows
  (`columnar_storage.rs`)
- [ ] In `find_symbols`, when the fast-path fires, call `stream_names_asc` and return early
  (`columnar_storage.rs`)
- [ ] Integration test: `FIND symbols ORDER BY name LIMIT 10` — assert names are in ascending
  lexicographic order, wall time < 100 ms on zephyr
- [ ] Integration test: `FIND symbols ORDER BY name DESC LIMIT 10` once DESC variant is done

### Phase 2 — Sort segments by path at build time (FQOV v4)

- [ ] In `overlay_builder.rs` step 2, change sort key from `a.1` (hex_content_id) to `a.0`
  (source_path); update the comment to document the new invariant: _segments are in
  lexicographic source_path order; global_row_ids for each path are contiguous_
  (`overlay_builder.rs`)
- [ ] Bump `SCHEMA_VERSION` from 3 to 4; add version history entry in the comment
  (`overlay.rs`)
- [ ] Confirm the version-rejection path in `Overlay::open` returns an error (not a panic)
  for unknown versions so old v3 overlays trigger a clean rebuild (`overlay.rs`)
- [ ] Test: open a freshly built overlay, read `overlay.segments()`, assert paths are in
  non-decreasing lexicographic order

### Phase 3 — Compute `segment_offsets` at open time (no format change)

- [ ] Add `segment_offsets: Vec<u32>` field to `Overlay` (N_segments + 1 entries, sentinel
  at end = total row count) (`overlay.rs`)
- [ ] Populate `segment_offsets` in `Overlay::open` as prefix sum of `SegmentRecord.row_count`
  immediately after decoding the `segments` blob (`overlay.rs`)
- [ ] Add `pub fn segment_row_range(&self, seg_idx: usize) -> Range<u32>` on `Overlay`
  (`overlay.rs`)
- [ ] Unit test: for each segment index, assert `segment_row_range` ranges are contiguous,
  non-overlapping, and the last range ends at `row_count()`

### Phase 4 — Add `path_fst` blob (FQOV v4 extended)

- [ ] Add `BLOB_PATH_FST: &[u8] = b"path_fst"` constant; increment `TOC_COUNT` from 9 to 10;
  update `HEADER_V3_LEN` (`overlay.rs`, `overlay_writer.rs`)
- [ ] Add `path_fst: Option<FstMap<MmapSlice>>` field to `Overlay`; decode in `Overlay::open`
  when the blob is present, skip gracefully when absent (`overlay.rs`)
- [ ] In `overlay_builder.rs`, after the path-sort step, build path FST: iterate sorted `segs`,
  insert `(path_bytes, seg_idx as u64)` into `fst::MapBuilder::memory()`, finalise
  (`overlay_builder.rs`)
- [ ] Add `path_fst_bytes: &[u8]` to `WriteV3Params` (or rename to `WriteV4Params`); emit the
  blob in the writer (`overlay_writer.rs`)
- [ ] Add `pub fn path_prefix_segment_range(&self, prefix: &str) -> Option<(usize, usize)>`:
  FST range query `[prefix, prefix_incremented)` → collect segment indices → return
  `(min_idx, max_idx)` (`overlay.rs`)
- [ ] Add `pub fn path_exact_segment_idx(&self, path: &str) -> Option<usize>` for exact-path
  lookups (`overlay.rs`)
- [ ] Test: build overlay, assert `path_prefix_segment_range("drivers/")` returns a contiguous
  range; assert row range covers only `drivers/` rows

### Phase 5 — Use path row range to skip segments in `materialize_all` (no format change)

- [ ] Add `fn in_glob_as_prefix(glob: &str) -> Option<&str>`: returns the path prefix when the
  glob is `prefix/**` or bare `prefix/`; returns `None` for all other patterns
  (`columnar_storage.rs`)
- [ ] In the fast-path branch of `find_symbols` (`has_path_filter && !has_any_indexed_predicate`),
  when `in_glob_as_prefix` succeeds, iterate only `seg_first..=seg_last` instead of all
  segments; keep existing glob fallback for non-prefix patterns (`columnar_storage.rs`)
- [ ] In the normal path, after `group_by_segment`, when prefix range is available replace
  the full `segments_passing_path_filter` glob scan with
  `map.retain(|&idx, _| idx >= seg_first && idx <= seg_last)` (`columnar_storage.rs`)
- [ ] Test: `FIND symbols WHERE is_recursive = 'true' IN 'drivers/**'` — assert all result
  paths start with `drivers/` and result counts match a full-scan filtered by path

### Phase 6 — Clamp bitmap intersection to path row range (no format change)

- [ ] Resolve the path prefix range early in `find_symbols` (before Stage 1 prefilter) when
  `in_glob_as_prefix` succeeds; store as `path_row_range: Option<Range<u32>>`
  (`columnar_storage.rs`)
- [ ] After `prefilter_global` returns `candidates`, when `path_row_range` is `Some(r)`, apply
  `candidates.remove_range(0..r.start); candidates.remove_range(r.end..u32::MAX)`
  (`columnar_storage.rs`)
- [ ] Test: `FIND symbols WHERE fql_kind = 'function' IN 'drivers/**'` — assert all result
  paths start with `drivers/` and counts match full-scan-filtered result

### Phase 7 — Add `count: u32` to `KindEntry` (FQOV v5)

- [ ] Add `pub(super) count: u32` to `KindEntry` (16 → 20 bytes, 5 × u32, no padding needed);
  update the struct-size comment (`overlay.rs`)
- [ ] In `overlay_builder.rs`, populate `count` from `bitmap.len()` when serialising kind
  entries (`overlay_builder.rs`)
- [ ] Bump `SCHEMA_VERSION` to 5; add version history entry (`overlay.rs`)
- [ ] Add `pub fn kind_count(&self, kind: &str) -> Option<u32>`: binary-search `kind_index`,
  return `entry.count` in O(log N_kinds) (`overlay.rs`)
- [ ] Add `pub fn all_kind_counts(&self) -> Vec<(String, u32)>`: iterate full `kind_index`,
  return all `(kind_name, count)` pairs in O(N_kinds) (`overlay.rs`)
- [ ] In `find_symbols`, add fast-path for `GROUP BY fql_kind` with no WHERE: call
  `all_kind_counts()`, build `Vec<SymbolMatch>`, apply `apply_clauses`, return early
  (`columnar_storage.rs`)
- [ ] Integration test: compare `FIND symbols GROUP BY fql_kind` counts from fast-path against
  a forced full scan; assert counts are identical

## Miscellaneous

- [ ] Per-user engine isolation when lock contention becomes measurable (each user gets their
  own `ForgeQLEngine` instance; `Arc<TokioMutex<Engine>>` becomes a pool)
- [ ] `ALL_CAPS` naming consistency audit for enumerators and constants
