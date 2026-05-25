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

- [x] Add `fn group_by_file_fast_path_eligible(clauses: &Clauses, dirty_empty: bool) -> bool`
  (`columnar_storage.rs`)
- [x] In `find_symbols`, fast_group_by_file reads `meta.dedup_row_count` (not raw `row_count`
  — Phase 9b provides exact deduplicated counts), builds `SymbolMatch` with path and count,
  calls `apply_clauses` with `group_by = None` clone, returns early — zero segment files
  opened (`columnar_storage.rs`)
- [x] Integration test: covered by golden.json G13–G19 (GROUP BY file variants)

### Phase 1 — `ORDER BY name LIMIT N` fast-path (no format change)

- [x] Add `fn order_by_name_fast_path(clauses: &Clauses) -> bool` (`columnar_storage.rs`)
- [x] Add `pub(crate) fn materialize_one_row(&self, local_row_idx: u32, source_path: &Path)
  -> Option<SymbolMatch>` to `SegmentReader` (`segment_reader.rs`)
- [x] Add `pub(crate) fn stream_names_asc(&self, need: usize, segments: &[Arc<SegmentReader>])
  -> Vec<SymbolMatch>` as a method on `Overlay`; complete current name group before
  checking `need` budget (`overlay.rs`)
- [x] In `find_symbols`, fast-path fires when `order_by_name_fast_path` and
  `dirty.is_empty()`; `need = limit + offset`; dedup on (name, fql_kind, path, line);
  calls `apply_clauses` for residual OFFSET/LIMIT (`columnar_storage.rs`)
- [ ] Integration test: `FIND symbols ORDER BY name LIMIT 10` — assert ascending order,
  wall time < 100 ms on zephyr
- [ ] Integration test: `FIND symbols ORDER BY name DESC LIMIT 10` — defer (DESC requires
  collect-then-reverse; FST only iterates ascending)

### Phase 2 — Sort segments by path at build time (FQOV v4)

- [x] Sort key changed from `a.1` (hex_content_id) to `a.0` (source_path) with invariant
  comment (`overlay_builder.rs`)
- [x] `SCHEMA_VERSION` bumped 3 → 4 (later 4 → 5 in Phase 9b); history comment updated
  (`overlay.rs`)
- [x] Version-rejection via `ensure!` returns `Err` (not panic); `exec_source.rs` warm-path
  also checks `Overlay::open().is_ok()` to auto-rebuild on schema mismatch (`overlay.rs`)
- [x] `overlay_segments_are_in_path_order` test added (`overlay_parity.rs`)

### Phase 3 — Compute `segment_offsets` at open time (no format change)

- [x] `segment_offsets: Vec<u32>` field added; uses `saturating_add` (`overlay.rs`)
- [x] Populated at `Overlay::open` as prefix sum of `SegmentRecord.row_count` (`overlay.rs`)
- [x] `pub fn segment_row_range(&self, seg_idx: usize) -> Range<u32>` added (`overlay.rs`)
- [x] `overlay_segment_row_ranges_are_contiguous` test added (`overlay_parity.rs`)

### Phase 4 — Path prefix → segment/row range lookup (no format change)

  _Implementation note: path_fst blob was not needed — binary search on the sorted
  segments array (Phase 2 invariant) is O(log N) and allocation-free._

- [x] `pub fn path_seg_range(&self, prefix: &str) -> Range<usize>` — binary-search
  `self.segments()` for the contiguous segment-index range covering `prefix` (`overlay.rs`)
- [x] `pub fn path_row_range(&self, prefix: &str) -> Range<u32>` — combines `path_seg_range`
  with `segment_offsets` to return the global row-ID range in O(log N) (`overlay.rs`)
- [x] `overlay_path_seg_range_exact_match` and `overlay_path_row_range_covers_segment_rows`
  tests added (`overlay_parity.rs`)

### Phase 5 — Clamp segment loop to path row range (no format change)

- [x] Path prefix fast-path in `find_symbols`: `path_row_range(prefix)` resolves the segment
  range; segment loop restricted to `seg_first..=seg_last` (`columnar_storage.rs`)
- [x] Glob fallback kept for non-prefix patterns (`columnar_storage.rs`)
- [x] Tests covered by golden.json G26–G29 (IN 'path/**' variants)

### Phase 6 — Clamp prefilter bitmap to path row range (no format change)

- [x] `path_row_range` resolved before `prefilter_global`; passed as `path_floor` bitmap
  mask into `prefilter_global` so kind/name bitmap intersection is clamped before any
  segment is opened (`columnar_storage.rs`)
- [x] Tests covered by golden.json G30–G45 (kind+path combinations)

### Phase 7 — Deduplicated kind counts + `GROUP BY fql_kind` fast-path (FQOV v5)

  _Implemented via Phase 9b (different approach than originally planned: deduplicated kind
  bitmaps + `dedup_row_count` per segment rather than a `count` field in `KindEntry`)._

- [x] Kind bitmaps deduplicated at build time (canonical (name_id, fql_kind_id, line) sets);
  `bitmap.len()` now gives exact deduplicated counts (`overlay_builder.rs`)
- [x] `pub(super) fn kind_global_counts(&self, path_mask: Option<Range<u32>>) -> Vec<(String, u32)>`
  on `Overlay` (`overlay.rs`)
- [x] `SCHEMA_VERSION` bumped 4 → 5; history comment updated (`overlay.rs`)
- [x] `GROUP BY fql_kind` fast-path via `group_by_kind_fast_path_eligible` +
  `fast_group_by_kind`; uses `kind_global_counts`, calls `apply_clauses` with
  `group_by = None` clone, returns early (`columnar_storage.rs`)
- [x] Integration test: covered by golden.json G11, G23–G25

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

### Phase 9 — ORDER BY name+kind fast-path + GROUP BY scaffolding (no format change)

- [x] `pub(crate) fn stream_names_asc_kind_filtered(&self, need: usize, kind_bm: &RoaringBitmap,
  segments: &[Arc<SegmentReader>]) -> Vec<SymbolMatch>` on `Overlay` — streams FST
  names while gating each row through the kind bitmap; avoids full materialization for
  sorted, kind-filtered name queries (`overlay.rs`)
- [x] `has_duplicate_paths` field on `Overlay` (detects dirty/duplication state) (`overlay.rs`)
- [x] `group_by_file_fast_path_eligible` / `group_by_kind_fast_path_eligible` eligibility
  guards added (were disabled pending dedup counts — re-enabled in Phase 9b) (`columnar_storage.rs`)
- [x] Fixed: `apply_clauses` was re-applying `in_glob`/`exclude_glob` to synthetic `SymbolMatch`
  results (path = None) from GROUP BY fast-paths; fast-path methods now strip those
  clauses from the `no_group` clone before calling `apply_clauses` (`columnar_storage.rs`)

### Phase 9b — `dedup_row_count` per segment; FQOV v5 (format change)

- [x] `SegmentRecord` gains `dedup_row_count: u32` (20 bytes; SCHEMA_VERSION 4 → 5)
  (`overlay.rs`, `overlay_writer.rs`)
- [x] `overlay_builder` step 4.5: compute canonical row sets per segment via
  `HashSet<(name_id, fql_kind_id, line)>`; write exact deduplicated count at build time
  (`overlay_builder.rs`)
- [x] `fast_group_by_file` uses `dedup_row_count` instead of raw `row_count` — eliminates
  17–18% overcounting from tree-sitter intra-file duplicate AST nodes (`columnar_storage.rs`)
- [x] GROUP BY file and kind whole-repo queries: ~82 s → sub-second
- [x] `segment_reader.rs` exposes `dedup_row_count` field (`segment_reader.rs`)

## Overlay — enrichment acceleration (FQOV v6/v7)

See `data/future-plans/overlay-enrichment-acceleration-plan.md` for the full plan.

### Phase 2 — Zero-allocation FST stream filtering (no format change)

- [x] Replace per-name `RoaringBitmap` heap allocation with zero-copy `&[u32]` slice via
  `decode_postings_slice` inside `stream_names_asc` and `stream_names_asc_kind_filtered`
  — eliminates thousands of heap allocations per query (`overlay.rs`)

### Phase 3 — Bounded DESC streaming fast-path (no format change)

- [x] `stream_names_desc` and `stream_names_desc_kind_filtered` on `Overlay` — in-memory
  bounded min-heap (`BinaryHeap<HeapEntry>`) over a forward FST walk; retains only the
  alphabetically largest N names in O(K) footprint without opening segment files (`overlay.rs`)

### Phase 4 — `index_files` table in overlay (FQOV v6, format change)

- [x] `SCHEMA_VERSION` bumped 5 → 6; TOC count increased to 10 (`overlay.rs`)
- [x] Flat `u32` file-size array (`index_files_bytes`) serialised alongside segment metadata;
  eliminates disk directory walks for file-system queries (`overlay_builder.rs`, `overlay.rs`)
- [x] Automated version up-conversion; runtime validation on open

### Phase 5 — Global enrichment bitmaps (FQOV v7, format change)

- [x] `SCHEMA_VERSION` bumped 6 → 7; TOC count increased to 11; `enrich_bitmaps` blob added
  (`overlay.rs`, `overlay_builder.rs`)
- [x] `RoaringBitmap`s keyed by `"field=value"` built at overlay-write time for all enrichment
  attributes (step 5.5 in `overlay_builder.rs`)
- [x] `prefilter_global` intersects enrichment bitmaps for Eq/Bool/Gte/Gt/Lte/Lt predicates —
  candidate set shrinks from 37k+ rows to ~50–500 rows before segment materialisation
  (`columnar_storage.rs`)
- [x] `group_by_file_fast_path_eligible` intentionally excludes enrichment predicates; those
  queries fall through to the normal pipeline (now fast via bitmap narrowing)
- [x] 15 strategic golden queries added (`GST1`–`GST15` in `golden.json`)
- [ ] Numeric range prefilter: replace lexicographic-scan + parse in `prefilter_enrichment_ge/le`
  with a compact sorted `Vec<(i64, u32)>` value→row map stored in the `enrich_bitmaps` blob

## Miscellaneous

- [ ] Per-user engine isolation when lock contention becomes measurable (each user gets their
  own `ForgeQLEngine` instance; `Arc<TokioMutex<Engine>>` becomes a pool)
- [ ] `ALL_CAPS` naming consistency audit for enumerators and constants

---

## Code Quality Refactoring (added 2026-05-25, branch: code-refactore)

All items tracked here came from a structural analysis of the codebase on 2026-05-25.
One commit per step. Each step must pass `VERIFY build 'test-all-before-commit'` before commit.
See CHANGELOG [0.54.7] for the analysis that generated this list.

### Phase 1 — Parameter Clustering

Eliminate functions with 6+ parameters by grouping related arguments into structs.
Do these before Phase 2 — moved functions carry smaller signatures.

- [x] **P1-A** `ShowRequest<'_>` struct — `show_body` (9p), `show_callees` (7p), `show_signature` (7p),
  `show_members` (7p) all pass the same 7 args (`cached`, `path`, `byte_range_start`, `hint_line`,
  `workspace`, `symbol`, `lang_registry`). Create `ast/show/request.rs`; update all four functions
  and their call sites in `exec_show.rs`.
- [x] **P1-B** `IndexContext<'_>` struct — `collect_nodes` (8p) and `index_file` (7p) share
  `path`, `language`, `enrichers`, `macro_table`, `table`, `source`. Create in `ast/index.rs`.
- [x] **P1-C** `SecondaryIndexBuilder` struct — `index_row_into_secondaries` (8p) passes 5 separate
  mutable maps/stats. Replace with a `builder.insert(row, idx)` method.
- [x] **P1-D** `EscapeLocals<'_>` + `EscapeAccumulator` — `check_expr_escape` (9p) mixes 4 read-only
  input sets with 3 mutable accumulator outputs. Split into two structs in `ast/enrich/escape.rs`.
- [x] **P1-E** `Session::from_coords` — `Session::new` (6p) takes `id`, `user_id`, `source_name`,
  `branch` separately; `SessionCoords` already holds those four. Add `Session::from_coords` and
  update `use_source`.
- [x] **P1-F** `field_to_kinds_for_config` lookup map — 214-line match with 72 string literals.
  Replace with a `HashMap<&'static str, fn(&LanguageConfig) -> Vec<String>>` built via `OnceLock`.

### Phase 2 — File Splitting

Break monolith files into module folders. One commit per item.

- [x] **P2-A** Split `exec_show.rs` (383 lines) — extract each match arm into a private
  `fn exec_show_<variant>` method; `exec_show` shrinks to ~30-line dispatcher.
- [x] **P2-B** Split `build_and_persist` (486 lines, 44 branches) — each numbered comment step
  (// 1. // 2. // 2.5. …) becomes a private `&self → Result<T>` method; orchestrator ~40 lines.
- [x] **P2-C** Split `columnar_storage.rs` (100 KB) into module folder:
  `columnar_storage.rs` (root) / `fast_paths.rs` / `query.rs` / `commit.rs`.
- [x] **P2-D** Split `lang.rs` (81 KB) into module folder:
  `lang.rs` (root, structs + traits + registry) / `lang/config.rs` (impl LanguageConfig) / `lang/inline.rs` (test-only inline impls).
- [ ] **P2-E** Split `ast/index.rs` (77 KB) into module folder:
  `mod.rs` / `build.rs` / `secondaries.rs` / `query.rs`.
- [ ] **P2-F** Move test data out of code — `corpus()` (616 lines), `golden_values()` (321 lines),
  `sms_combinatorial()` (203 lines) → external TOML files loaded with `include_str!`.

### Phase 3 — Enforce Limits

- [ ] **P3-A** Add Clippy size thresholds to `.clippy.toml`:
  `too-many-lines-threshold = 60`, `cognitive-complexity-threshold = 25`,
  `too-many-arguments-threshold = 5`. Warnings first, tighten to errors after Phase 1+2.