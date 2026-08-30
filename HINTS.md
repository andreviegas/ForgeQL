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
- **`!=` is the one shape where a COMPLETE answer became a refusal.**
  `fql_kind != 'impl'` returned every row (57,467 on forgeql-pub.frozen), not
  zero — so the "zero rows is a claim about the corpus" doctrine does not reach
  it. The ground is different: the predicate was inert, and an inert filter is
  invisible the way an empty one is. Say so wherever the change is described;
  the `=` framing does not cover it.
- **A value universe must cover what the engine RENDERS, not only what it
  stores.** Three emit paths substitute a literal: `query/outline.rs` (×2) and
  `ast/show/members.rs` render `unknown` for the kindless row, and `compact.rs`
  renders `''` for a site with no role. All three spellings are accepted
  values. `ast/show/members.rs` used to print the RAW tree-sitter kind there —
  a back door to `node_kind`, which is refused as a clause field everywhere —
  and now renders `unknown` like the other two.
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
  `error`, `guard`, `cast`, `macro_call`, `''` and `unknown` are written by the
  indexer as literals rather than read out of a `kind_map`, and `code`/`text`
  likewise for roles, so all of them are asserted separately. Three of them DO
  also appear in a `kind_map` today — which is the point: the config sweep
  passing says nothing about the core route they actually travel. Each is
  spelled once, as a `field_tiers::*_KIND` const used at the site that writes
  it (`ERROR_KIND` and `CAST_KIND` alias the older `ast::lang::FQL_*` names;
  do NOT reuse that family blindly — `FQL_COMPOUND_ASSIGN`/`FQL_SHIFT` there
  do not match the kinds rows carry).
- **`unknown` is the RENDERED spelling of the kindless row**
  (`query/outline.rs` ×2 and `ast/show/members.rs`); the stored value is `''`. Both are
  accepted values, because the outline filters on what it printed. A refusal
  that knows one spelling and a renderer that emits the other is the regression
  `the_rendered_spelling_of_the_kindless_row_is_inside_the_universe` guards.
- A third hardcoded kind list exists, `file_indexer::ADDRESSABLE_FQL_KINDS`
  (which kinds get an ordinal and a `node_id`). Its own module asserts it is a
  SUBSET of `FQL_KIND_VALUES` — a kind added there and not to the universe
  would refuse rows that exist. Subset, not equality: a number, a cast or an
  operator row is queryable without being addressable by handle.
- **`WHERE fql_kind = ''` and `= 'unknown'` are one question and both are
  SERVED** (0.192.x). They were accepted and answered 0 rows while
  `GROUP BY fql_kind` counted thousands under the empty name — the kind lookup
  resolved the value through a key table `step5_build_kind_postings` skipped the
  empty kind out of, and `prefilter_global` turns a missing entry into an EMPTY
  bitmap rather than a scan, so the zero read as a fact about the corpus. Four
  things had to change together, and any three of them pass a green gate: the
  builder posts the empty kind like any other (overlay `SCHEMA_VERSION` 17 —
  content invalidation, no `ENRICH_VER`), `parse_predicate` spells `unknown` to
  the stored value at the one place an agent's text becomes an IR value, and
  the readers that hold a kind COLUMN report the empty kind as the VALUE it is
  rather than collapsing it to `None` — SIX of them: `materialize_rows` AND
  `materialize_one_row` (the separate builder behind the row-view page, the one
  a sweep of "the row builders" misses, whose omission delivered an empty page
  under a total of thousands), both row views, the legacy row and its
  prefilter — without which the SCAN still dropped those
  rows from the equality AND from `NOT MATCHES`. That third one is where the
  first draft went wrong: it substituted the empty string inside the shared
  comparison funnel instead, which reached EVERY row resolving to no value, so
  `FIND usages … WHERE fql_kind = ''` and `!= '<any kind>'` matched every site —
  a predicate that filters nothing while reading as a filtered result. `None` on
  this field has to keep meaning "this row shape has no kind column at all"; an
  occurrence site is that shape and is deliberately untouched. Pinned on three
  corpora, with that exclusion beside them, by `kindless_kind_equality.json`. And a fourth PLACE a columnar-only sweep misses entirely — not a seventh reader but a second index BUILDER: the legacy backend keeps its own copy of the builder skip in `SecondaryIndexBuilder::insert`, where it is worse than a smaller index — `find_symbols_prefilter` STRIPS the predicate once that index has supplied candidates, so an unindexed value reaches no scan at all and answers 0 with a success status. Pinned by `the_kindless_rows_answer_their_own_equality_on_the_legacy_backend`.

- **OPEN (found 2026-08-29, not fixed): the legacy `GROUP BY fql_kind` counts RAW
  rows where its own scan counts collapsed ones.**
  `try_group_by_stats_fast_path` (`storage/legacy.rs`) answers a bare
  `GROUP BY fql_kind` from `IndexStats::by_fql_kind`, which
  `SecondaryIndexBuilder::insert` increments once per indexed row, while
  `find_symbols_prefilter` collapses duplicates on
  `(name_id, path_id, fql_kind_id, line)` before returning any. On the
  `motor_control` fixture the empty kind is 3 counted against 2 answered, and
  the same gap applies to ANY kind with an intra-file duplicate — it is not
  about the empty kind, which is only where it happened to surface. This is the
  legacy twin of the class `group_by_counted_paths.rs` was built to catch on the
  columnar side: a counted route reporting a number no row was consulted for.
  Pre-dates 0.192.x; `the_kindless_rows_answer_their_own_equality_on_the_legacy_backend`
  deliberately controls against the SCAN's grouping so it measures the equality
  rather than this.

- **OPEN (not fixed): `role = ''` on `FIND usages` is the LAST member of the
  kindless-value family.** It is an accepted value — `compact.rs` prints the
  empty role for a site the backend tagged with none — and it matches nothing,
  because `eval_predicate_on`'s equality arm is `is_some_and` and `SiteView`
  answers `None`. On the columnar backend that zero is correct (the read pass
  tags every site it finds); on the IN-MEMORY backend, which tags none of them,
  it is a false zero of exactly the shape `fql_kind = ''` had. The fix is the
  same shape too: give the empty role a value the reader can find rather than a
  hole. Until then it is pinned NOWHERE — the `field_tiers` comment beside
  `USAGE_ROLE_VALUES` used to lean on the kindless-kind expect_fail, and that
  case is now green and lives in `kindless_kind_equality.json`, so a fix must
  bring its own pin.

## Enrichers and the leading position (2026-08-26)

- **A comment that OPENS a function body is not a child of the body.**
  tree-sitter attaches an extra where the scanner was standing, and an
  indentation-delimited grammar does not open the block until the first
  *statement* — so in Python the comment lands beside the block, under the
  function node. `TodoEnricher` walked `child_by_field_name("body")` only and
  missed exactly that position; it now walks the body plus every comment that is
  a direct child of the function node.
- **What was actually checked, and what was only argued.** CHECKED: in `enrich/`,
  `is_comment_kind` is called by `todo.rs` and `comments.rs` and nothing else,
  and `comments.rs` reads `prev_named_sibling` on a comment row rather than
  walking a body — so among enrichers that find comments *that way*, `todo.rs`
  was the whole class. ARGUED, not established: that only comment-scanning
  enrichers can have this blindness at all. The reasoning is that a statement is
  never an extra, so it cannot escape the block — but thirteen other enrichers
  walk `child_by_field_name` and no test and no per-grammar enumeration of
  extras backs the claim for them. Treat it as a hypothesis.
- **`is_comment_kind` is an EQUALITY against one declared raw kind**
  (`syntax.comment`), not a set. C, C++ and Python each declare `comment` and
  their grammars use one node for both styles, so nothing is lost there. Rust
  declares `line_comment` while its grammar also emits `block_comment` — which
  `kind_map` DOES map to `fql_kind = 'comment'`, so a Rust `/* */` is a comment
  ROW that no comment-scanning enricher will look at. Marker detection in Rust
  is `//`-only, in every position.
- **`tests/fixtures/` holds one fixture per grammar** (`todo_leading.{py,c,cpp,rs}`).
  The test registry in `tests/common/mod.rs` registers C, C++, Rust and Python,
  so a `.c` fixture indexes — the older note that C was missing is stale.
- **The `test` gate step is not the commit gate.** `JOB START 'test'` leaves
  `FORGEQL_ALLOW_CHANGE_FILE_INDEXED` unset, so three `change_*` tests in
  `engine_integration` fail on it; `test-all-before-commit` sets it.

## Stamp-only boolean defaults (2026-08-26)

- **Three readers have to agree, not two.** A `WHERE <enrichment> = <value>`
  passes the workspace bitmap (`fast_paths::enrichment_eq_bitmap`), the
  PER-SEGMENT posting reader (`segment_reader::prefilter_enrichment_postings`)
  and the per-row evaluator (`filter::eval_predicate_on`). The middle one is the
  easy one to miss: it resolves the value through the segment's own string pool
  and `return`s an EMPTY bitmap when the value is not in it — so a value nothing
  stores loses every row of every segment that posts the field, while segments
  posting none of it keep theirs. The symptom is an answer short by an amount
  that changes per field and per corpus, which looks like anything but a
  per-segment gate. `proves_enrichment_value_absent` has the same `id_of` logic.
- **`fql_kind = 'function'` is neither a superset nor a subset of "the enricher
  examined this row", and BOTH directions bite.** Probe each separately.
  - *Examined but outside the kind:* a config can declare a raw kind a function
    kind and map it elsewhere. cmake declares `macro_def` a function kind and
    maps it to `fql_kind = 'macro'`. Probe:
    `FIND symbols WHERE fql_kind = 'macro' WHERE param_count >= 0` — 69 on
    zephyr, 43 on pytorch. `param_count` is stamped unconditionally by an
    enricher on the same gate, so it is the witness that the row was examined.
    dbc is NOT an instance: it maps `message` to `object` and has no function
    rows at all.
  - *Inside the kind but not examined:* the todo enricher also gates on
    `config.has_comment()`, and `make`/`cmake` declare no `syntax.comment`, so
    their function rows are never scanned. Measure it with `has_doc`, which
    shares that gate and stores BOTH values: on zephyr `has_doc` true+false =
    95,902 against 96,304 function rows, and the 402 difference is exactly
    cmake (355) + make (47). `param_count` has no comment gate and reads
    96,304, which is what tells the two gates apart.
- **A group key is not a group name.** The scan's `apply_group_by` keeps the
  first row of each group and labels it with THAT ROW's `name`, so the key never
  reaches the result's name column; only the columnar counted path emits a row
  named after the value. A test that asserts a group is NAMED `false` therefore
  passes only on the counted route — on the scan, assert how many groups there
  are instead. (`SymbolRow::group_key` is a separate column, filled by the
  render arm in `result/query.rs`, and that one does read the resolver.)
- **An enricher gates on a language CAPABILITY as well as on the node kind.**
  `has_escape` needs `has_address_of`, `is_recursive` needs
  `has_call_expression`, `has_todo` needs `has_comment`. A language that does
  not declare the capability makes the enricher `return` before reading
  anything, so its rows were never examined even though their kind says they
  were. Python declares `"address_of": ""` — escape analysis has never run on a
  Python function, 138,126 of pytorch's 194,114.
- **And on the shape of the grammar node, which no config declares.** Several
  enrichers read `child_by_field_name("body")` and return when there is none.
  A cmake `function_def` carries no `body` field, so `ShadowEnricher` — which
  has NO capability gate and looked unconditional for exactly that reason —
  never walks one. `has_shadow` therefore reaches c/cpp/python/rust only, like
  the capability-gated three, and the list has to be recomputed from the
  GRAMMARS rather than from the configs.
- **A witness proves only the gate it shares.** `param_count` has no capability
  gate, so it cannot see the class above at all, and using it to settle "was
  this row examined?" is how the question got answered wrong twice. Pick a
  witness that shares the gate you care about (`has_doc` for `has_comment`),
  measure `<field> = 'true' GROUP BY language` and look for a language at zero,
  or — for a gate no config declares — parse a snippet and ask the node.
- **An outcome test cannot see a gate that writes nothing.** "Walked and found
  nothing" and "returned at the gate" produce the identical absent column, so a
  fixture asserting the ANSWER passes under both. The cmake body gate survived
  a green gate, a fixture and a guardian round that way; only a test holding a
  tree-sitter node told the difference.

## Field/tier table: serving is keyed by operator class AND by value class

- `Serving` entries in `FieldTier::serving` describe the values rows STORE. The
  one value a stamp-only field answers without any row storing it has its own
  entry, in `StampDefault::eq` — one struct field, so a second `(=, default)`
  serving cannot be written. `field_tier_table::eq_serving` therefore still
  means "the stored-value entry", and its callers still hold.
- The four stamp-only fields shipped inheriting `ENRICH_EQ` through `posted`,
  which said `= 'false'` was one key read whose empty answer
  `fast_paths::no_segment_carries_enrichment_value` could turn into an absence.
  All three halves were wrong for that value: no key holds it,
  `SegmentReader::proves_enrichment_value_absent` returns `false` for it by
  name, and the `=` arm returns into `stamp_default_candidates` before reaching
  the prover at all. No wrong answers came of it — which is why only reading the
  table against the code found it.
- `Tier::implemented_by()` names the single function a one-function tier IS.
  `a_tier_that_names_its_implementation_names_one_the_code_defines` reads the
  source back through `PROOF_SOURCES`, the same standard a
  `SupersetProvenAbsent` prover is held to. A name in that table is a claim
  about which code runs; a claim nothing reads back is prose.
- `Measured::RowCeiling { passes, per_row, then_per_candidate }` is what to
  declare when no bench class exists: a hand-timed number from an agent session
  is noise dressed as precision (the run-to-run spread here is +/-10%, and only
  classes over two seconds mean anything), while a ceiling in rows is a property
  of the code. `Measured::Unmeasured` on a NEW tier is the thing P7 exists to
  catch. Three traps in stating one. (1) `Serving::measured` is the cost of
  ASKING, so the ceiling has to carry the `Serving::then` follower as well as
  the tier — the follower is usually the larger term. (2) The unit is per
  PREDICATE, not per query: the enrichment arms run inside the loop over a
  clause's `WHERE` predicates and nothing memoises the sweep across them.
  (3) A CEILING has to cover what widens it, not what a corpus happened to
  measure — `rows_missing_field_postings` contributes EVERY row of a segment
  that holds the column and posted no keys, whatever the kind, and a narrowing
  that declines leaves the complete scan. Writing the measured number as the
  ceiling understates both.
- Adding a `Tier` variant with the same body as an existing arm trips clippy's
  `match_same_arms` in `intrinsic_exactness`; merge the pattern and put the
  second reason in the arm's comment.
- Counting the readers of a hand-maintained list, a narrow criterion
  undercounts and a raw grep overcounts, and one pass makes both mistakes. The
  enumeration in the `field_tiers` module doc was scoped to "read at query
  time" and so missed `CAST_KIND` in `ast/enrich/casts.rs` and
  `MACRO_CALL_KIND` in `ast/index/file_indexer/rows.rs`, which mint the same
  constants while INDEXING — the coupling is what puts a site on the list, not
  the phase it runs in. It also missed `filter::is_known_symbol_field`, which
  asks `lookup` whether a NAME is real at all: sharing an entry point with a
  bullet already on the list is enough to hide a reader. The same grep then
  offered a `#[cfg(test)]` assertion in `ast/index/file_indexer.rs` as a third
  minting site, which it is not. Grep the whole crate, then open every hit.
- That grep finds readers of the TABLE, not everything coupled to it. `error`
  is minted at `ast/index/file_indexer/rows.rs:41,125` through
  `ast::lang::FQL_ERROR` rather than through `field_tiers::ERROR_KIND` — the
  same constant, since `ERROR_KIND` is defined as `FQL_ERROR`, and the same
  coupling to the value universe, in the same file as a site the grep DOES
  find. A `field_tiers::` grep cannot see a site that reaches the value by its
  other name.

## Rewrites outside ForgeQL (2026-08-30)

- The freshness gate lives in `engine.rs::execute` → `ensure_files_fresh` → `files_named_by` (which op names which file, mirroring each SHOW verb's own lookup) → `ensure_file_fresh` (stat → `Session.fresh_stamps` → `StorageEngine::is_path_fresh` → `reindex_session`). A read that re-indexed carries the notice through `ForgeQLResult::note_reindexed_outside_forgeql` (the `hint` of Show / Query / FindNode).
- `ColumnarStorage::is_path_fresh` compares the DIRTY segment's `content_id` with the disk too; it used to answer `true` unconditionally when a dirty segment existed, which is why a gate's auto-fmt (always after a mutation) stayed invisible until the next mutation.
- `innermost_nodes_for_lines_impl` refuses a stale file before either branch and `fold_segment` skips a row whose end precedes its start — that subtraction underflowed (`attempt to subtract with overflow`) and killed HTTP requests, because `forgeql-server` had no `catch_unwind` around `execute()`; both transports now share `engine::helpers::panic_message`.
- The `test` step stops at `engine_integration`'s three env-gated failures, so a new integration suite that sorts after it never runs there; name it `a_…` temporarily to see it, or run the full gate. The full gate refuses a golden harness built from another worktree (shared target dir) until a source change under `crates/forgeql` forces a rebuild in this tree.
- `SHOW outline OF '<node_id>'` (subtree form) is served by `ColumnarStorage::outline_subtree` (query/outline.rs); it preferred the COMMITTED segment over the dirty one, so after any in-session edit the subtree answered pre-edit lines — now dirty-first like `find_node_impl`. The by-file form goes through `outline_path_target`/`outline_glob`, which already merged dirty rows. A new pin in `worktree_edited_outside_forgeql.rs` (`an_outline_read_by_handle_reports_current_lines`) caught it.
- `StorageEngine::path_freshness` is three-valued (`Verified` / `Stale` / `Unknown`); `is_path_fresh` is `!= Stale`. The gate stamps `Session.fresh_stamps` only on `Verified` — an `Unknown` (no content id, unreadable file) must never be cached as verified, or a file readable again with the same stat would skip the hash and serve stale rows. The stamp on the re-index path is taken BEFORE the verifying hash (a rewrite between the two moves the stat).
