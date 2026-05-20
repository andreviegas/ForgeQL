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
  `matches!(&clauses.group_by, Some(GroupBy::Field(f)) if f == "file")`, no WHERE
  predicates, `having_predicates.is_empty()` (note: `GroupBy` has no `File` variant and no
  `PartialEq`; use `matches!` throughout) (`columnar_storage.rs`)
- [ ] In `find_symbols`, when the fast-path fires, iterate `overlay.segments()`, skip entries
  that don't pass `passes_resolve_glob`, build one `SymbolMatch` with all fields spelled out
  (no `..Default::default()` — `SymbolMatch` does not derive `Default`) with `path` and
  `count: Some(seg.row_count as usize)`. Call `apply_clauses` with a **modified clone** of
  `clauses` where `group_by = None` to preserve ORDER BY/HAVING/LIMIT/OFFSET without
  re-counting the pre-populated `count` values. Return early — zero segment files opened
  (`columnar_storage.rs`)
- [ ] Integration test: `FIND symbols GROUP BY file ORDER BY count DESC LIMIT 5` against
  frozen zephyr — assert ≤ 5 rows, each `count > 0`, wall time < 100 ms

### Phase 1 — `ORDER BY name LIMIT N` fast-path (no format change)

- [ ] Add `fn order_by_name_fast_path(clauses: &Clauses) -> bool` —
  `clauses.order_by.as_ref().map(|o| o.field.as_str()) == Some("name")` (ASC),
  `limit` is `Some`, no GROUP BY, no WHERE predicates (note: `OrderBy` is a struct
  `{ field, direction }`, not an enum — `order_by == OrderBy::Name` will not compile)
  (`columnar_storage.rs`)
- [ ] Add `fn materialize_one_row(&self, local_row_idx: u32, source_path: &Path) -> Option<SymbolMatch>`
  to `SegmentReader` — this method does **not** currently exist; the only API is
  `materialize_rows(&bitmap, source_path)` (`segment_reader.rs`)
- [ ] Add `fn stream_names_asc(&self, offset: usize, limit: usize) -> Vec<SymbolMatch>`:
  walk `overlay.name_fst.stream()`, skip `offset` entries, decode `name_postings`,
  resolve `row_table[id]` → `RowPtr`, call `materialize_one_row`, stop after `limit` rows.
  Note: both `Overlay::name_fst` and `Overlay::decode_postings` are private; the cleanest
  fix is to implement `stream_names_asc` as a method on `Overlay` itself (where both are in
  scope); if it must stay in `columnar_storage.rs`, both must be made `pub(crate)`
  (`columnar_storage.rs`, `overlay.rs`)
- [ ] In `find_symbols`, when the fast-path fires, call `stream_names_asc` and return early
  (`columnar_storage.rs`)
- [ ] Integration test: `FIND symbols ORDER BY name LIMIT 10` — assert names are in ascending
  lexicographic order, wall time < 100 ms on zephyr
- [ ] Integration test: `FIND symbols ORDER BY name DESC LIMIT 10` once DESC variant is
  implemented (DESC requires collect-then-reverse, not FST tail streaming — the `fst` crate
  only iterates ascending; defer or bound separately)

### Phase 2 — Sort segments by path at build time (FQOV v4)

- [ ] In `overlay_builder.rs` step 2, change sort key from `a.1` (hex_content_id) to `a.0`
  (source_path); update the comment to document the new invariant: _segments are in
  lexicographic source_path order; global_row_ids for each path are contiguous_
  (`overlay_builder.rs`)
- [ ] Bump `SCHEMA_VERSION` from 3 to 4; add version history entry in the comment
  (`overlay.rs`)
- [ ] Confirm the version-rejection path in `Overlay::open` returns an error (not a panic)
  for unknown versions — already uses `ensure!` (returns `Err`); this task is a verification
  step. Note: the v3→v4 bump is atomic, so no silent-corruption window exists (`overlay.rs`)
- [ ] Test: open a freshly built overlay, read `overlay.segments()`, assert paths are in
  non-decreasing lexicographic order

### Phase 3 — Compute `segment_offsets` at open time (no format change)

- [ ] Add `segment_offsets: Vec<u32>` field to `Overlay` (`segments.len() + 1` entries;
  sentinel at index `segments.len()` = total row count); populate with `checked_add` to
  guard against u32 overflow (`overlay.rs`)
- [ ] Populate `segment_offsets` in `Overlay::open` as prefix sum of `SegmentRecord.row_count`
  immediately after decoding the `segments` blob (`overlay.rs`)
- [ ] Add `pub fn segment_row_range(&self, seg_idx: usize) -> Range<u32>` on `Overlay`
  (`overlay.rs`)
- [ ] Unit test: for each segment index, assert `segment_row_range` ranges are contiguous,
  non-overlapping, and the last range ends at `row_count()`

### Phase 4 — Add `path_fst` blob (FQOV v4 extended)

- [ ] Add `BLOB_PATH_FST: &[u8] = b"path_fst"` constant; increment `TOC_COUNT` from 9 to 10;
  update `HEADER_V3_LEN` constant. **This is not a two-line edit** — `find_blob_ranges`
  returns `[Range<usize>; 9]`, the destructuring in `Overlay::open` has 9 bindings, and
  `validate_blob_layout` takes `&[Range<usize>; 9]`; all must change together
  (`overlay.rs`, `overlay_writer.rs`)
- [ ] Add `path_fst: FstMap<MmapSlice>` field to `Overlay` (non-optional — the version gate
  ensures every v4 file has the blob; `find_blob_ranges` must `bail!` when it is absent);
  decode after `find_blob_ranges` returns the 10-element array (`overlay.rs`)
- [ ] In `overlay_builder.rs`, after the path-sort step, build path FST: iterate sorted `segs`,
  insert `(path_bytes, seg_idx as u64)` into `fst::MapBuilder::memory()`, finalise
  (`overlay_builder.rs`)
- [ ] Rename `WriteV3Params` → `WriteV4Params` and `write_v3` → `write_v4`; add
  `path_fst_bytes: &[u8]` field; emit the blob in the writer (`overlay_writer.rs`)
- [ ] Add `pub fn path_prefix_segment_range(&self, prefix: &str) -> Option<(usize, usize)>`:
  use two binary searches on `self.segments()` (path-sorted by Phase 2 invariant) — no
  FST stream needed. Use the `prefix_successor` helper (backwards byte-walk) for the upper
  bound to avoid 0xFF overflow (`overlay.rs`)
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
- [ ] In the normal path, after `group_by_segment`, when a prefix range is available apply
  the range retain (`map.retain(|&idx, _| idx >= seg_first as u32 && idx <= seg_last as u32)`),
  then apply `EXCLUDE` glob separately if present (a second retain calling
  `passes_resolve_glob` for the exclude side only); for non-prefix globs
  (`in_glob_as_prefix` returns `None`) keep the existing `segments_passing_path_filter`
  call as fallback — do not remove it (`columnar_storage.rs`)
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
- [ ] Fix kind-count serialisation: `compute_blobs` in `overlay_writer.rs` only receives
  pre-serialised bitmap bytes (`HashMap<String, Vec<u8>>`), not the original
  `RoaringBitmap`, so `bitmap.len()` is unavailable there. Add
  `kind_counts: HashMap<String, u32>` to `WriteV4Params`; populate it in
  `overlay_builder.rs` from `kind_merged.iter().map(|(k, bm)| (k.clone(), bm.len() as u32)).collect()`
  **before** serialising the bitmaps; use `kind_counts[kind_str]` in `compute_blobs` when
  constructing each `KindEntry` (`overlay_builder.rs`, `overlay_writer.rs`)
- [ ] Bump `SCHEMA_VERSION` to 5; add version history entry (`overlay.rs`)
- [ ] Add `pub fn kind_count(&self, kind: &str) -> Option<u32>`: binary-search `kind_index`,
  return `entry.count` in O(log N_kinds) (`overlay.rs`)
- [ ] Add `pub fn all_kind_counts(&self) -> Vec<(String, u32)>`: iterate full `kind_index`,
  return all `(kind_name, count)` pairs in O(N_kinds) (`overlay.rs`)
- [ ] In `find_symbols`, add fast-path for `GROUP BY fql_kind` with no WHERE
  (`matches!(&clauses.group_by, Some(GroupBy::Field(f)) if f == "fql_kind")`): call
  `all_kind_counts()`, build `Vec<SymbolMatch>` with all fields spelled out (no
  `..Default::default()`), call `apply_clauses` with a modified clone where `group_by = None`
  to prevent re-counting the pre-populated counts, return early (`columnar_storage.rs`)
- [ ] Integration test: compare `FIND symbols GROUP BY fql_kind` counts from fast-path against
  a forced full scan; assert counts are identical

### Phase 8 — Bounded top-K materialization (no format change)

- [x] Extract the existing order-by comparator from `apply_clauses` into a free function
  `pub(crate) fn order_cmp<T: ClauseTarget>(a: &T, b: &T, clauses: &Clauses) -> Ordering`;
  keep behaviour identical including the `(name, line, path)` tie-breakers (`filter.rs`)
- [x] Add `fn collect_top_k<T, F>(items: Vec<T>, k: usize, cmp: F) -> Vec<T>` using
  `slice::select_nth_unstable_by` (introselect, O(N) average) to partition the k-best
  rows, then sort only that k-element window — faster in practice than a `BinaryHeap`
  (`filter.rs`)
- [x] Add `const TOPK_THRESHOLD: usize = 1_000`. In `apply_clauses`, after HAVING (step 5),
  gate on `order_by.is_some() && limit.is_some_and(|k| k <= TOPK_THRESHOLD) && offset.unwrap_or(0) == 0 && group_by.is_none()`;
  when true replace steps 6–8 with `collect_top_k`; otherwise fall through unchanged
  (`filter.rs`)
- [ ] Unit tests covering: (a) numeric ASC/DESC top-K matches full-sort; (b) string ORDER BY
  top-K matches full-sort; (c) ties broken by `(name, line, path)` produce the same
  ordering in both paths; (d) `OFFSET > 0` is not redirected to top-K path;
  (e) `GROUP BY` queries are not redirected (`filter.rs`)
- [x] In `columnar_storage::materialize_all`, when `order_by.is_some()`, `limit.is_some_and(|k| k <= TOPK_THRESHOLD)`,
  and `group_by.is_none()`, use a running trim via `collect_top_k` (keeping `K * TOPK_OVER_FETCH`
  rows) instead of accumulating all rows. WHERE predicates are applied per-segment before
  the trim (`columnar_storage.rs`)
- [x] Secondary over-fetch trim in `materialize_all`: `const TOPK_OVER_FETCH: usize = 4`;
  trim fires when `results.len() > k * 4`, retaining `max(k * 2, k)` survivors via
  `collect_top_k` — bounds peak memory to O(K) (`columnar_storage.rs`)
- [ ] Regression test: for each of `param_count`, `lines`, `usages`, `condition_tests`, run
  `FIND symbols WHERE <field> >= 1 ORDER BY <field> DESC LIMIT 20` under top-K path and a
  forced full-sort path; assert byte-identical results (`crates/forgeql-core/tests/`)
- [ ] Micro-benchmark: `FIND symbols ORDER BY usages DESC LIMIT 20` on a fixture of
  N = 100 000 rows; target ≥5× speedup vs. full-sort and ≤50 KB peak heap
  (`crates/forgeql-core/benches/`)

## Miscellaneous

- [ ] Per-user engine isolation when lock contention becomes measurable (each user gets their
  own `ForgeQLEngine` instance; `Arc<TokioMutex<Engine>>` becomes a pool)
- [ ] `ALL_CAPS` naming consistency audit for enumerators and constants
