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
- The overlay's **name postings are canonical-only too** since FQOV v16
  (`step6_build_name_fst` takes `seg_dedup` and probes `canonical.contains(row)`
  per posting entry — a sorted slice, so a probe rather than step 5's bitmap
  AND). That is what re-admitted `name = '<value>'` to `fast_group_by_file`
  (`counts_exactly` is the one predicate list both gates read). **Seven readers**
  take those postings, all in `overlay.rs` via `decode_postings`/`_slice`:
  `prefilter_name_values`, `lookup_name_bitmap` (→ `prefilter_global` and
  `query/resolve.rs`), and the five `stream_names_*`.
- **Resolution is LAST-wins, not first** — `pick_best_resolved`
  (`query/resolve.rs`) says so itself ("mirroring the legacy last-write-wins
  strategy: last preferred → last definition → last overall") and uses
  `rposition`/`.last()`, while `collect_resolve_candidates` walks each segment's
  bitmap ascending. `step45_dedup_segments` keeps the LOWEST row id of a
  `(name, fql_kind, line)` group, so canonical-intersecting the postings means a
  duplicate group's highest row used to win and its lowest wins now. Those two
  rows share name, kind and line but can differ in byte range, ordinal and
  handle, **so `SHOW body`/`signature`/`NODE OF '<name>'` can return a different
  span.** What keeps it from moving in practice is that the names anyone
  resolves by are not duplicated: Andre swept 40 function names through
  `SHOW signature` on the pre-change and post-change binaries and every one was
  byte-identical. That is evidence about the sampled names, NOT a guarantee from
  the ordering — do not write it as one, which is what the first version of this
  note did.
- Dirty segments are NOT intersected (no dedup pass runs for them) — harmless,
  because every counted grouping already requires an empty dirty overlay.
- The name streams (`try_order_by_name_fast_paths`, query/find.rs) serve
  `ORDER BY name [DESC] [WHERE fql_kind =] LIMIT` and the bare `LIMIT`; they decline
  on a non-empty dirty overlay, duplicated source paths, and `need > find_max_rows()`.
- `order_cmp` tie-break is `(name, line, path, fql_kind)` — total on distinct rows;
  the legacy dedupe key is the matching `(name_id, path_id, fql_kind_id, line)`.

## Chain attach (crates/forgeql-core/src/storage/columnar)
- `warm_or_open` tries: overlay on disk → chain (`columnar_storage/upstream_chain.rs::chain_or_fall_through`: a written `<sha>.chain` manifest first, then one *derived* from the nearest ancestor overlay via `git/ancestry.rs::nearest_ancestor` + `chain_derive.rs`) → full build. Overlay candidates come from `ColumnarBuildContext::overlay_commits()` (the `.bin` files of the versioned overlays dir).
- The derived change set is content-keyed (segment table vs the inline `prebuilt_segment_map`; file-only entries vs `overlay_builder::collect_file_only`), so ancestry only decides *nearness*; the threshold is the existing `FORGEQL_CHAIN_COMPACT_PATHS` (at/past it → full build, no manifest written).
- `usages` on any dirty session is the master aggregate + `columnar_storage/usage_adjust.rs` correction (per-segment `usages_fst` counts, cached by a fingerprint of the dirty overlay, fetched once per query via `usage_stamper()`).
- Test fixtures: `overlay_harness::build_segment` writes rows only; `build_segment_with_id` also writes usage/mention postings and takes an explicit `(rel_path, content_id)` — use it whenever one path holds different bytes at two commits.

## Memory profile of a cold index (probe on zephyr-andre, 0.172.0, 2026-08-17)
- (Before this slice) `SegmentReader::open` eagerly deserialised `kind_postings`, `field_postings` (38 fields) and `name_prefix` into heap RoaringBitmaps per segment: step1 opening 32,748 segments cost ~550 MiB anon (723→1.3 GiB), re-paid at `shared_open` and resident for the life of the SharedOpen entry; steady state after USE was anon=1.1 GiB, file=1.4 GiB. After the in-place change: step1 +~340 MiB, steady state anon=938 MiB, peak 3,399→3,127 MB, overlay md5 unchanged. `zone_maps` stays eager (tiny). `name_prefix` is never read by the overlay build (only `fast_paths.rs`).
- Segment/overlay mmaps are plain `MmapOptions::map` (no populate/madvise): one VMA per segment — 80,426 on linux; needs `vm.max_map_count` above the old 65,530 default (this box: 1,048,576).
- `forgeql-server` HAD no `#[global_allocator]` (glibc malloc) until this slice gave it the same tikv-jemallocator (workspace feature `background_threads`) that `crates/forgeql/src/main.rs` already used.
- `indexing_pool()` was a process-wide rayon pool with 256 MiB stacks whose touched stack pages were never returned. It is now built per index run and dropped at the end of it: `build_indexing_pool()` (private, build only) sizes it by `FORGEQL_INDEX_THREADS`, default half the cores; `with_indexing_pool()` (`pub(crate)`, both incremental reindex paths) takes **one** worker, because those bodies are strictly sequential — `rayon` appears nowhere in `reindex_files_inner`, `reindex_files_on_pool` or `index_file` — and `install` gives the closure a big-stack worker whatever the pool size. Pool construction returns `Err` rather than panicking, which matters now that one is built per mutation.
- Overlay build used to hold every step's output (`global_row_table` 8 B/row, kind/trigram/enrich/name blobs) until `step8_write_overlay` wrote them all at once. It now writes and drops each blob at the point the file's layout puts it (`OverlayWriter` in `overlay_writer.rs`, driven by `OverlayBuilder::write_blobs`), with the header and its table of contents seeked back to and filled in last. The kind, trigram and enrichment bitmaps stay `RoaringBitmap` until the write, so the per-bitmap serialised buffer and the concatenated `bitmap_data`/`enrich_bitmaps` copies are gone. The corpus md5 cannot gate byte identity (non-reproducible run to run), so the algorithm each part replaced is kept as an oracle instead: `overlay_writer/tests.rs` pins the header, TOC and inter-blob padding; `overlay_builder/tests.rs` pins `enrich_bitmaps`, the four kind/trigram blobs and the two segment blobs. `name_fst`, `name_postings`, `row_table`, `index_files`, `file_entries` and `usages_count_fst` are pass-through byte slices with no layout work, and carry no oracle. Where an index blob is written BEFORE the payload it points into — `kind_index`/`trigram_index` before `bitmap_data`, `segments` before `segment_strings` — `OverlayWriter::blob_of_len` refuses the file at run time if the payload does not write the predicted number of bytes.
- Per-segment postings are read IN PLACE since this slice (`segment_reader/postings.rs`: `PostingBlob`/`Entries`/`decode`; reader accessors `kind_rows`, `field_rows`, `name_prefix_rows`, `kind_postings()`, `field_postings(field)`, `posted_fields()`, `has_name_prefix_index()`). `open` validates entry tables only; a corrupt bitmap surfaces at lookup as `Err` and every query caller treats it as "cannot narrow". Measured share: the eager decode was ~210 of the ~550 MiB a 32,748-segment open cost — the other ~340 MiB is reader tables (`blobs` HashMap<String>, `extra_cols` Strings, mention_fsts BTreeMap, PathBufs; ~10 KB/segment) and was the next target — done by the table-of-contents slice below, which took a 32,748-segment open from +299 MiB to +8 MiB.
- `RUN 'bench_ab'` on this shared box: `usages_split`, a class that touches none of the changed code, read 0.94x then 1.11x in two consecutive runs of the same binaries — ±10% is the run-to-run spread here; only repeated runs on >2 s classes mean anything.
## Segment reader (crates/forgeql-core/src/storage/columnar/segment_reader.rs)

- A reader keeps **no name table**: the FQSF table of contents is parsed into
  `segment_reader/load.rs::Toc` (entry names borrowed from the mapping, sorted,
  binary searched) and dropped when `open` returns. What survives are byte
  ranges — `name_postings`, `usages_postings`, `MentionIndex.postings`,
  `ExtraCol.data` — so a blob the reader must read has to be resolved in `open`,
  not looked up later.

- A value lookup (`StringPool::id_of`) is a **binary search over the
  `strings_sorted` blob** — the pool's ids in the order of the bytes they name —
  comparing against `strings_data` in the mapping. It replaced a lazily built
  `HashMap<String, u32>` per segment (`ENRICH_VER` 71). A segment without the
  blob is refused at open rather than falling back, and none can be reached:
  segments cache under `{provider}-v{ENRICH_VER}/`. Census through
  `RUN 'run_fql'` (counters in `id_of`, printed at the end of the query): one
  `WHERE naming = 'snake_case'` touches **24,056 of zephyr's 32,748** segments
  and **397 of this repo's 411**; the maps it no longer builds held 5,721,758
  strings / 221,016,000 bytes of string data there, 193,481 / 11,020,016 here.
  The search that replaced them costs 197,965 comparisons for that whole query
  on zephyr (8.2 per lookup), 3,929 here (9.9). Four different enrichment
  predicates in one session reach 400 of 411 — the ceiling is the segments the
  session touches, not the number of predicates.
- **`WHERE fql_kind = '<kind>'` calls `id_of` zero times** (0 of 32,748 and 0 of
  411, measured) — the kind equality is answered before `prefilter_kind` is
  reached. That is why no `bench_mem` class ever built these maps: `kind_scan`,
  `wide_scope`, `name_stream` and `full_scan` carry no enrichment equality.
  `bench_ab naming_eq` is the only class with the shape.
- Names that repeat across segments (enrichment column names, occurrence role
  names) are interned process-wide in `segment_reader/intern.rs` as `Arc<str>`
  (capped pool, private copy past the cap). Reading a name back out of the
  mapping instead costs a whole-corpus scan ~90 ms per query — measured — because
  `materialize_rows` resolves every column name once per segment per query.
- `row_field` is asked which column answers a clause field, and the answer
  depends on the ACCESSOR the clause will read it through, not on the field name
  alone. A built row reads `name`/`node_kind`/`fql_kind`/`language`/`path`/
  `node_id` from its struct through `field_str` and `usages`/`count`/`line`
  through `field_num`, reaching its enrichment map for every name the other
  accessor holds — so a segment with an enrichment column called `name` is
  invisible to the built row under one accessor and IS what it reads under the
  other. `accessor_for` derives which is coming from the operator and the value
  type. There is no shadow mask: it existed to decline such a name outright, and
  declining cost 292 of the 293 segments a `WHERE name …` scan selects on this
  repository.

## Engine-owned value universes (`fql_kind`, `role`)

- Who owns a field's VALUES is declared on `FieldTier.values`
  (`field_tiers::ValueUniverse`): `Corpus` for almost everything, `Engine(list)`
  for `fql_kind` and `role` only. `filter::reject_unknown_enum_values` reads it
  and refuses an `=`/`!=` value outside an engine-owned list.
- It is called from `ForgeQLEngine::dispatch_op` over `ir::clauses_of`, beside
  `reject_invalid_patterns` — one call, so every clause-carrying verb and both
  storage backends are covered and a new verb inherits it. Do not add a
  per-verb copy.
- A field whose values the CORPUS owns must stay `ValueUniverse::Corpus`:
  `guard_kind = 'ifdef'` answering empty is blessed behaviour, pinned by
  `control_a_corpus_owned_value_answers_empty_and_is_not_refused` in
  `engine_owned_value_refusal.json` and by a table test that fails if a third
  field declares `Engine`.
- `FQL_KIND_VALUES` must be a SUPERSET of what the plugins can produce — a kind
  missing from it refuses a legitimate query. `tests/engine_owned_value_universes.rs`
  reads every `crates/*/config/*.json` by walking the crate directories (not a
  list of crates) and fails on a kind or role the declaration does not carry.
  `error`, `guard`, `cast`, `macro_call` and `''` are written by the indexer as
  literals rather than read out of a `kind_map`, and `code`/`text` likewise for
  roles, so all of them are asserted separately. Three of the five DO also
  appear in a `kind_map` today — which is the point: the config sweep passing
  says nothing about the core route they actually travel.
- A third hardcoded kind list exists, `file_indexer::ADDRESSABLE_FQL_KINDS`
  (which kinds get an ordinal and a `node_id`). Its own module asserts it is a
  SUBSET of `FQL_KIND_VALUES` — a kind added there and not to the universe
  would refuse rows that exist. Subset, not equality: a number, a cast or an
  operator row is queryable without being addressable by handle.
- `WHERE fql_kind = ''` is ACCEPTED but answers 0 rows while `GROUP BY fql_kind`
  counts thousands under the empty name — the kind prefilter resolves the value
  through the segment string pool, which holds no empty value. Pinned as an
  open defect (`the_kindless_rows_answer_their_own_equality`); do not read it as
  the refusal swallowing the shape.
