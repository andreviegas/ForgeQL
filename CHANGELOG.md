# Changelog

All notable changes to ForgeQL will be documented in this file.

ForgeQL uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.181.0] — 2026-08-22 — a usages page is cut before its rows are built

### Performance

- `FIND usages` cuts its page in the engine instead of building a row for every
  site and letting the caller select afterwards. Everything that can remove or
  reorder a site — path globs, `WHERE`, `HAVING`, `ORDER BY` — still runs over
  the whole answer, and `total` is still taken before the cut; only the sites
  inside the page are built. The site list itself is still held whole and still
  bounded by the result budget, because a cut that selects whole files out of a
  computed answer cannot bound what is searched.

- A `WHERE` on a field no column of a segment holds is answered from that
  absence rather than deferred until the rows are built. It answers what the
  built row's filter would have answered, including for the negative operators:
  a missing value fails `!=` and `NOT LIKE` rather than passing them, so the
  early answer can only remove rows the built path also removes.

### Notes

- `body` and `role` are written onto a result row after its columns are read, so
  a `WHERE` or `ORDER BY` naming either now falls off the bounded-page route and
  can be refused by the result budget where it was previously answered. The
  ordering is the reader that could least afford the old reading: ranking by an
  absence the built row does not share cuts the wrong top-K and sheds rows that
  belonged in the answer.

- `total` on `FIND usages` counts groups rather than sites under `GROUP BY`, and
  an explicit `LIMIT` clips it; the default page does not. Unchanged behaviour,
  now stated where it is met.

## [0.180.0] — 2026-08-21 — a page is chosen from row views, and only the page is built

### Performance

- A bounded `FIND symbols` built a result row for every row it matched and then
  threw almost all of them away. It now carries 48-byte views of the segment
  columns through the residual `WHERE`, the duplicate collapse, the ordering and
  the page cut, and builds a result row only for the `LIMIT` + `OFFSET` that
  survive. Every segment is still read, tested and counted, and `total` is still
  the number of rows that matched.

  On a 3.06M-symbol corpus: `FIND symbols IN 'drivers/**' LIMIT 5000` fell from
  4,317 ms and 1,365 MB of private heap to 411 ms and 142 MB, against a 131 MB
  floor for an open index — so the query's own heap went from 1,234 MB to 11 MB.
  `FIND symbols WHERE fql_kind = 'if'` fell from 463 ms to 55 ms.

- Two shapes that were outside every bound are now inside one. A `LIMIT` above
  1,000 and a `LIMIT` beside an `OFFSET` could not arm the running trim over
  built rows, so both materialised the whole answer to deliver a page; both now
  carry `LIMIT` + `OFFSET` views instead. Queries no view can page — a
  `GROUP BY`, a `HAVING`, a regular-expression predicate, an ordering or
  predicate naming `usages`, `node_id` or `count`, or an index with two segments
  over one source path — build every matching row as before, and are refused by
  the same budget.

### Fixed

- A row view reported a field absent whenever a segment happened to carry an
  enrichment column of the same name, which disqualified that whole segment from
  every route that ranks or keys rows before building them. A built row answers
  those names from its own struct fields and never from its enrichment map, so a
  view reading the same fixed column now agrees with it. The old reading was
  expensive: 308 of the 411 segments of this repository's own index carry an
  enrichment column called `name` — one for every file holding a macro
  definition, since `name` is what most grammars call a definition's identifier
  child — and each was excluded from the cheaper route for every query.

### Notes

- The result budget now bounds two things with two defaults out of the same
  2 GiB: the rows a scan carries (about 44.7 million row views at 48 bytes) and
  the rows it builds for delivery (about 1.34 million at about 1,600).
  `FORGEQL_FIND_MAX_ROWS` overrides both, in rows, and each refusal says which
  bound it is.

- Whether an `ORDER BY` can ride a view is now a property of the field rather
  than of a segment: any field except `usages`, `node_id` and `count`, which a
  built row fills in from outside its own columns. A field a segment does not
  carry is not a bar — both readings agree it is absent.

## [0.179.0] — 2026-08-21 — a golden run refuses to answer for another tree

### Fixed

- The golden harness refuses to run when it was built somewhere other than where
  it is running. The directory it reads its suites from is fixed when the harness
  is compiled, and several checkouts on one machine may share a build directory —
  so a harness built in one tree could be handed to another, read the first
  tree's suites, and report them as the second tree's result. A pass then said
  nothing about the tree under test, and an unexpected count of cases was the
  only hint anything was wrong. The harness now compares where it was built
  against where it is running and stops with both paths named. It asks that
  before it looks for a corpus, so a foreign harness with no corpus is refused
  rather than reporting a skip.

  Two ways a run can still describe something other than the tree it was invoked
  in, both unchanged: the engine binary the harness drives is addressed by a path
  into that same shared build directory, so a concurrent build in another tree can
  replace it between compiling and running; and the segment cache is keyed by the
  enrichment version, so a change to index output without a version bump is still
  answered from pre-change segments.

## [0.178.0] — 2026-08-20 — the overlay is written as it is built, not held whole

### Performance

- The overlay build no longer holds every blob in memory until the end. Each is
  written and dropped at the point the file's layout puts it, with the header
  and its table of contents filled in last; the kind, trigram and enrichment
  bitmaps stay Roaring until the write, so the per-bitmap serialised buffers and
  the concatenated copies are gone. The trigram reduce folds one partial map per
  worker instead of one per split, and the indexing thread pool is built per
  index run rather than for the life of the process, so its stacks are released
  when the run ends. Measured on a cold index of a 589,461-row corpus: peak RSS
  1,065 MB → 826 MB, the overlay build's own peak 1.0 GiB → 826 MiB, and the
  build itself 1,763 ms → 1,366 ms. **The overlay is byte-identical** — same
  checksum from both writers on the same corpus — so nothing reindexes.

- The incremental reindex runs on one worker instead of a pool. It parses files
  in sequence, so the rest were idle.

### Notes

- The writer now refuses rather than producing a file it cannot vouch for: a
  blob out of layout order, an unfinished table of contents, and — for every
  index written before the payload it points into — a payload whose length
  contradicts that index. All three refuse before the file is published, so the
  previous overlay survives. That last one converts a silent wrong-answer mode
  into a failed build.

- The indexing pool now defaults to half the available cores, overridable with
  `FORGEQL_INDEX_THREADS`.

## [0.177.0] — 2026-08-20 — every counted GROUP BY answers what the scan answers

### Fixed

- The two older counted groupings returned numbers no row had been consulted
  for. `GROUP BY file` counted whatever candidate set the index tiers proposed
  and cleared the residual `WHERE` before delivering, so nothing was ever tested
  against its own row: on a Zephyr checkout `WHERE name LIKE '%ab%' GROUP BY
  file` reported 32,612 files where 11,099 hold a matching name, and gave its
  top file 76,310 rows — more than that file's whole deduplicated row count of
  67,595. It now admits one predicate, `fql_kind = '<value>'`, whose postings
  are intersected with each segment's canonical rows at index build and are the
  only tier that both verifies and deduplicates. No name tier is admitted at any
  literal length, because nothing at query time can settle a candidate: the
  canonical row set is stored as a per-segment count, not as a set. Those
  queries are answered by the scan, which reads each row — slower than the wrong
  answer was, and on a large enough corpus refusable by the result budget.

- `GROUP BY fql_kind` dropped the rows whose kind is empty, which the kind
  postings skip at build: 2,169 of 59,636 rows on this repository's own corpus,
  41 groups where the scan finds 42. They are now the remainder — the canonical
  total of the selected segments less the rows the kinds account for — and a
  subtraction that cannot hold sends the query to the scan instead. Reading the
  kind index is all-or-nothing for the same reason: skipping past an unreadable
  entry would have reported those rows as carrying no kind.

- Both routes now hand back a `HAVING` or `ORDER BY` naming a field a counted
  group row does not carry, the test the enrichment grouping already applied.
  `GROUP BY fql_kind HAVING lines >= 2` answered nothing against the scan's six
  groups.

### Notes

- Six golden cases that pinned these as open defects now assert the behaviour,
  and each also pins *which route* answered — a counted group row carries the
  grouped value and its count and nothing else, where a scanned one is the first
  row of its group and carries a name and a line. Without that a case cannot
  tell a correct count from a correct count reached the other way, and would go
  on passing if the counted path quietly died.

## [0.176.0] — 2026-08-19 — a GROUP BY the index already counted

### Added

- `FIND symbols ... GROUP BY <field>` on an enrichment field the segments post
  per value is answered from the overlay's stored cardinalities instead of
  materialising every matching row. On a 3M-symbol corpus `GROUP BY naming`
  used to spend the whole result budget and be refused; it now answers. The
  counts are the scan's collapsed counts, not an approximation: each stored
  bitmap was written as a segment's postings intersected with its canonical row
  set, and a row carries at most one value of a field, so the cardinalities
  partition the rows. Rows the field says nothing about are one group keyed by
  the empty string.

  Anywhere those counts would not be the collapsed ones the query is handed
  back to the scan — a refusal, never an approximation: any `WHERE`, a field
  the overlay pruned, a selected segment storing the column but posting none of
  its values, a session with uncommitted edits, two segments built from one
  source path, a `HAVING`/`ORDER BY` naming anything but `count` or the grouped
  field, and a key table that cannot be read whole. `IN` and `EXCLUDE` are
  welcome: a segment is one source path, so a glob selects whole segments.

  A counted group row carries the value and the count and nothing else, so with
  no `ORDER BY` the groups come back in a different order from the scan's, which
  names each group by the first row that fell into it. `total` and every count
  are the same either way.

### Notes

- Writing the gates down exposed wrong answers on the two older counted
  groupings, pinned as open defects rather than fixed, since fixing either
  changes what the grouping returns. `GROUP BY file` admits a `WHERE` on
  `name`, but every name tier proposes a superset and none verifies, so it
  counts high at any literal length — only `fql_kind =` is exact. `GROUP BY
  fql_kind` drops the rows carrying no kind instead of grouping them under the
  empty string. Both older routes answer a `HAVING`/`ORDER BY` on a field their
  group row does not carry with an empty set instead of declining to the scan.

## [0.175.0] — 2026-08-19 — a segment's index costs its mapping and little else

### Performance

- Opening a segment no longer copies its table of contents onto the heap. The
  table is read in place and dropped when the open finishes, leaving the reader
  holding the byte ranges of the blobs it reads, and the names that repeat
  across every segment — enrichment column names, occurrence role names — are
  interned once for the process instead of once per segment. Measured on a
  3.06M-row, 32,748-segment corpus: opening the segments adds 8 MiB where it
  added 299, a session holds 437 MiB once its index is open instead of 825, and
  a cold index peaks at 2,824 MB instead of 3,143 — at the same wall time, with
  the overlay byte-identical, so nothing reindexes. Extra sessions on one commit
  still cost 3 MB each.

  With the posting lists that stopped being decoded at open in 0.173.0, opening
  every segment of that corpus now costs 8 MiB where it cost about 560.

- Whole-corpus scans came out faster: whether a segment shadows a field a result
  row answers from its own struct is a bit test resolved at open, not a walk of
  that segment's column names on every row.

### Notes

- A reader no longer keeps the path of the file it was opened from, so a
  diagnostic raised inside one names the segment by its content id. The overlay
  build, which knows the path, still names the path.

## [0.174.0] — 2026-08-18 — a name with a multi-byte character stops crashing the page it is on

### Fixed

- A query whose page held a name longer than 120 bytes ending in a multi-byte
  character crashed the process: the display truncation measured the name in
  bytes and cut it at byte 120, which is not a legal place to split a string
  when that byte sits inside a character. Indexing this repository was enough
  to hit it — `FIND symbols ORDER BY line DESC` paged in a documentation line
  ending in an em dash. The cut is now made in characters, as the documented
  behaviour always said. Reachable from the server as well as the CLI, though
  it cost one request rather than the daemon. Present since 2026-03-21.

## [0.173.0] — 2026-08-18 — an opened segment stops copying its postings onto the heap

### Performance

- Opening a segment no longer decodes its Roaring posting lists into the heap;
  they are read in place over the mmap and one bitmap is decoded per lookup.
  On a 3.06M-row, 32,748-segment corpus: the open phase adds ~340 MiB of
  anonymous memory instead of ~550, a session after `USE` holds 938 MiB instead
  of ~1.1 GiB, peak RSS 3,127 MB instead of 3,399, shared open 4.3 s instead of
  5.4. The overlay is byte-identical. A `name LIKE 'xy%'` — the one shape that
  reaches the per-segment name-prefix index — is 1.25x slower (73.7 → 92.2
  ms/query); a whole-corpus `fql_kind` scan is unchanged at 1.03x. Accepted:
  the per-segment heap copy is what stops many sessions querying one commit at
  once.

- A `FIND symbols` with no `LIMIT` written is handed its default page as the
  engine's `LIMIT`, so it takes the same routes an explicit `LIMIT 20` does and
  holds only the page. A bare `FIND symbols` on a 3M-symbol corpus went from
  2,365 MB and a refusal to 380 MB and a completed page; `WHERE fql_kind = 'if'`
  with no `LIMIT` from 852 MB / 634 ms to 384 MB / 298 ms. Rows and `total` are
  unchanged; `GROUP BY` and `HAVING` keep the clauses as written.

- `forgeql-server` now runs on jemalloc, as the `forgeql` binary already did, so
  memory freed after an index build returns to the operating system. Not on
  Windows, where jemalloc does not build under MinGW.

### Notes

- A posting bitmap that will not decode is reported by the lookup that reaches
  it instead of refusing the segment at open: prefilters stop narrowing, the
  residual filter still decides, and no row is dropped.

## [0.172.0] — 2026-08-17 — a commit pulled from upstream stops re-merging the whole corpus

### Added

- Attaching to a commit that arrived from `REFRESH SOURCE` no longer merges the
  whole corpus. Such a commit never had a chain manifest — only a ForgeQL
  `COMMIT` wrote one — so the attach now derives it: the nearest ancestor with
  an overlay on disk becomes the master, and the change set is read off the two
  indexes rather than from git history, so a stale or distant base costs extra
  entries and never wrong rows. The manifest is written beside the missing
  overlay, so later sessions on that commit skip the derivation.

  Ancestry is checked on the commit graph: a sibling branch's overlay is never
  a base. With no ancestor overlay within reach, or a change set at or past
  `FORGEQL_CHAIN_COMPACT_PATHS`, the full build runs as before — so attaching
  to a commit *older* than anything indexed is unchanged, since every overlay
  on disk is then a descendant rather than an ancestor.

  Measured on a 3,062,139-row corpus. Attaching is always cheaper than the full
  build — 38% faster at one changed file, still 13% at eight thousand.
  Answering queries afterwards is not: +34% at one changed file, +51% at the
  512-path default, +112% at eight thousand, because rows come through the
  seeded overlay for the life of the session. The default of 512 predates that
  measurement and has not been chosen against it.

### Fixed

- `usages` on a session with dirty rows — an uncommitted edit or a chained
  attach — is the commit's own count. It was the master overlay's aggregate,
  which still counted a replaced file's old sites and none of its new ones.
- A chain attach no longer adopts a delta file it finds in the worktree when
  that delta cannot describe this commit — an older chain's seed after a
  fast-forward, or a checkpoint tree's delta without its staging. Either left
  the attach unseeded and the commit's changed files unserved; both are now
  dropped and their paths queued for re-index.
- On reconnect, a queued re-index path was workspace-relative while the git
  diff paths beside it were absolute, so it was looked for in the wrong
  directory, taken for a deletion, and its rows hidden instead of restored.

## [0.171.0] — 2026-08-16 — one traversal where there were four, and a field that stops reshuffling itself

**Re-indexes once on upgrade:** the enrichment version moves to 70.

### Performance

- The C/C++ body walk makes one bounded traversal where it made several. Metrics
  counted return, goto, string and throw in four separate passes over the same
  function body; the escape enricher collected local, array and static
  declarations in three. Measured on a 3.06M-row corpus, normalised against
  parsing (untouched by the change): metrics −41.9%, escape −21.2%, at 2,095,758
  calls each and no memory cost. `decl_distance` was deliberately left alone —
  its two passes use different bounds and one seeds the other.
- The overlay's name-index merge no longer allocates per posting. Strictly less
  work for identical output; it does not move wall clock, and is not claimed to.

### Fixed

- `escape_vars` no longer spells the same variables in a different order on
  every run. The macro-escape scan iterated a hash set while the field is
  emitted in discovery order, so one source file produced a different value each
  time it was indexed — enough to move an overlay checksum on a macro-heavy
  corpus about one run in four, which made a single before/after comparison an
  unreliable check. It now iterates a sorted view, as the sibling
  `escape_kinds` already did.

### Notes

The name-FST step now reports where its time goes. On a 29,864,281-row corpus:
preparing per-segment inputs 19,252 ms, merging 5,117 ms (17,368 ms of CPU, so
3.4x parallel), and the serial FST build 24,222 ms of a 48,597 ms step. Half the
step is a serial build whose cost tracks output size — 253 MB here — and is
reachable only by shrinking that output, not by more parallelism.

## [0.170.0] — 2026-08-16 — the name index merges in parallel, and a file indexes the same way twice

**Re-indexes once on upgrade:** the enrichment version moves to 69, so cached
segments and overlays are re-keyed.

### Performance

Measured on a 29,864,281-row corpus (the Linux kernel, 80,426 segments), cold,
against 0.169.0 on the same machine:

| | before | after |
|---|---|---|
| name-FST merge | 48,646 ms | 43,487 ms |
| usages-count FST | 12,958 ms | 3,841 ms |
| whole overlay build | 77,078 ms | 57,115 ms |
| cold index, wall clock | 6:23 | 5:09 |

- The overlay's name and usages-count indexes are merged in parallel shards of
  the key space rather than by one sequential pass over every segment. The
  shards are disjoint, so the merge still holds one copy of the keys and peak
  memory does not move; a first-byte summary read from each segment lets a
  shard skip the segments it has no keys in.

### Fixed

- Indexing the same file twice now produces the same segment bytes. The
  enrichment columns were walked in hash order, and because a string column's
  values are interned into the segment's string pool as the column is walked,
  every run assigned different ids — 3,237 of 3,240 segments differed between
  two cold indexes of one C corpus; it is now 34.

Answers are unchanged by either: the overlay checksum is identical before and
after on every corpus measured, including the 29.9M-row one.

## [0.169.0] — 2026-08-16 — the index stops rebuilding strings it already holds as integers

**Re-indexes once on upgrade:** the enrichment version moves to 68, so cached
segments and overlays are re-keyed. What is rebuilt is byte-identical to what
it replaces.

### Performance

Measured on a 29,864,281-row corpus (the Linux kernel, 80,426 segments), cold,
against 0.168.0 on the same machine:

| | before | after |
|---|---|---|
| enrichment-bitmap phase | 78,156 ms | 26,948 ms |
| whole overlay build | 111,630 ms | 77,078 ms |
| macro-collection phase, anonymous memory | 6.3 GiB | 4.5 GiB |
| cold index, peak resident | 19.48 GiB | 18.97 GiB |

- The enrichment-bitmap phase of the overlay build now walks segment columns as
  the 32-bit value ids they already are, resolving each distinct value to text
  once per segment instead of rebuilding a string per field per row — on the
  corpus above, roughly 10^8 allocations removed from the production of a
  133 MB blob.
- Collecting macro definitions no longer keeps a path per definition and three
  copies of every macro name; both are interned and the records carry 4-byte
  ids. On the corpus above the table's paths fall from 759 MiB to 10 MiB and
  its name copies from 257 MiB to nothing. This phase holds almost no
  reclaimable memory, so it is the one that decides whether a constrained host
  can index at all.

Output is unchanged, and checked rather than argued: the overlay checksum is
identical before and after at 29.9M, 3.06M and 589k rows, which covers every
key, every bitmap and every decision to drop an over-budget field.

## [0.168.0] — 2026-08-15 — a broken question is answered as a fact about the question

### Fixed

- `WHERE`/`HAVING` predicates using `MATCHES`/`NOT MATCHES` now reject an
  uncompilable regex pattern before any row is read, naming the field, the
  operator, and the underlying regex error. Previously an invalid pattern was
  silently treated as "matches nothing" for `MATCHES`, and — worse —
  "matches everything" for `NOT MATCHES`, so a broken pattern on the negated
  form returned a plausible-looking, unfiltered result instead of an obviously
  empty one.
- `SHOW body`/`context`/`members`/`callees` and `FIND callees OF` now refuse an
  unknown or misspelled `WHERE` field by name, instead of reporting a
  misleading "no symbol matches" or "eliminated by filters" as if the symbol
  itself were the problem. A real field with no satisfying candidate still
  gets the old message, unchanged — only a genuinely unrecognized field name
  is now told apart. Both storage backends share the same field-recognition
  check, and the legacy backend's enrichment-field dictionary has been
  completed with several fields (`is_override`, `is_final`, `macro_expansion`,
  `macro_def_file`, `macro_def_line`, `macro_arity`, `enclosing_type`,
  `owner_kind`, `suffix_meaning`) that were previously recognized only by
  accident, when a workspace's own data happened to carry a matching column.
- Four more enrichment fields an enricher really writes — `error_scope`,
  `expansion_depth`, `expanded_reads` and `expansion_failure_reason` — are
  recognised by that same refusal, which previously called them unknown.
  Because it decides typo-or-real from a fixed list, a field the list omitted
  was rejected as a misspelling on any corpus holding no row for it, and
  `doc/syntax.md` offers `WHERE error_scope = 'root'` as a worked example. A
  query on one of these now reaches the ordinary no-candidate message instead
  of being refused as nonsense.

### Tests

- A new golden suite, `broken_query_refused`, pins the refusals above the way
  an agent meets them: by writing a statement and reading the answer. It
  covers an uncompilable `MATCHES` and `NOT MATCHES` pattern, the same in
  `HAVING`, and an unknown `WHERE` field, and each refusal travels with a
  control proving the query answers once the broken part is removed — so a
  refusal can never be mistaken for an empty corpus, and the validation can
  never regress into refusing a working query. The pattern check previously
  had unit tests only; those construct the clause value and call the checking
  function directly, so all three stayed green when the single line wiring the
  check into the engine was deleted, and the protection could have been lost
  without any test noticing. The new suite fails in exactly that case. The
  filter test module now states the rule that separates the two levels.
- A second new golden suite, `open_defects`, pins five known-open defects as
  expected failures: `-1` and context-dependent `0` literals stamped
  `is_magic` against the documented exemption list; the `'false'` value of
  most boolean enrichment fields answering zero rows while `has_doc` and
  `is_magic` answer both values; `SHOW NODE … LINES A-B` failing to parse
  while the error advertises the clause; an unknown `fql_kind` value matching
  nothing silently instead of being refused; and local variables reporting the
  corpus-wide count of their name as `usages`. Each case asserts the correct
  behaviour, runs without failing the gate while the defect stands, and fails
  the gate on a PASS — so the commit that fixes a defect must promote its case
  in the same change.
- The golden runner evaluated `error: true` / `error_contains` only on the
  steps of a multi-step case — on a single-query case the keys were accepted
  and silently never read, so such a case could pass with no assertion
  evaluated. A single-query case now honours them with the same contract as a
  step.

## [0.167.0] — 2026-08-14 — a bulk mutation gate that no longer re-walks the worktree per member

### Performance

- Verifying `IF REV` on a bulk `NODES FOUND` mutation no longer re-walks the
  worktree once per member. Whole-path members are answered from their
  recorded paths — one walk, built only when the set holds a directory, is
  shared by every directory member — so the gate on a directory-heavy set
  costs about one listing where it previously cost members × workspace
  (minutes at 95,000-file scale, the same cliff the arming path had).
  Symbol-handle members still resolve through the index, and the per-file
  content fingerprint is unchanged. A 2,000-directory set now verifies in
  under a second where it previously took four minutes.

### Fixed

- A member deleted out from under an armed set now reads as a set change and
  is refused with the usual re-run-the-`FIND` recovery, where it previously
  surfaced a raw not-found error with no stated way forward.

### Changed

- A directory's revision fingerprint now has one definition instead of three.
  A listing stamps a directory row's `rev`, a bulk `IF REV` gate re-derives
  it, and a bare directory handle resolves to it — three copies of the same
  fold, each of which had to agree to the bit or every mutation on a
  directory would be refused with a mismatch that re-running the `FIND` could
  never clear. They now call one function. Behaviour is unchanged; the
  previous copies agreed, but nothing was checking that they still did.

### Tests

- The regression test guarding symbol resolution against uncommitted edits now
  builds its fixture from an edit that really produces a reference row — a Rust
  struct literal in a session-created file — and asserts that row exists in the
  index before asserting anything about resolution. The previous fixture edit, a
  bodyless C++ enum reference, indexes no row at all, so the test kept passing
  even with the mention demotion it guards removed. Behaviour of the engine is
  unchanged; only the test's evidence is.
- Unit tests now pin what a directory rev means and that the bare-handle
  resolver really derives it the same way a listing does — the end-to-end test
  that used to compare those two derivations stopped covering the resolver
  when the bulk gate moved to the one-walk derivation.

## [0.166.0] — 2026-08-14 — FIND files is served from the index, and a bulk copy refuses to overwrite itself

### Fixed

- `COPY NODES FOUND TO 'dir/'` and `MOVE NODES FOUND TO 'dir/'` now refuse a
  set in which two members share a basename, naming both sources and the
  destination they collide on. Previously both members targeted the same new
  file and the second silently appended to the first — two unrelated files
  welded into one, reported as success. With every destination claimed exactly
  once, the reported `edit_count` again equals the number of destination files
  a copy creates. A refused statement writes nothing: destinations are now
  validated for every member before any directory is created.
- Symbol resolution in a session with uncommitted edits no longer lets an
  edited file that merely mentions a name outrank the file that declares it.
  Previously, once any edited file carried the name as a bare reference,
  `SHOW members OF 'Type'` — and `SHOW body` / `SHOW context` / `SHOW
  callees`, which share the resolver — answered from that file and failed
  with "AST node not found" for a symbol that plainly exists elsewhere.
  A declaration edited in-session still wins exactly as before; only bare
  reference rows now rank behind the persistent index instead of ahead of it.

### Performance

- `FIND files` answers every query shape from the index's own file list —
  the union of indexed segments, non-indexed file entries and the session's
  uncommitted adds, with deletions masked — instead of walking the worktree
  and stat-ing every file on each call. Only the extension-filtered shape
  used the stored list before; the guard dated from when older overlays
  carried source files only, a reason that no longer exists. Directory rows
  are now derived from that same list: one row per directory that has files
  beneath it, `size` the total bytes of those files (previously the same
  directory could appear twice, once with `size` meaning child count and
  once meaning summed bytes). An empty directory created in-session is
  addressable by the handle its creation returned but is not listed. The
  filesystem walk survives only for storage backends that expose no file
  list. On a 95,000-file corpus an unscoped `FIND files` previously burned
  tens of minutes of CPU; the listing itself is now free of filesystem work,
  and what remains is the per-row rev stamping of the rows actually
  returned.
- A complete `FIND files` result no longer takes a different route from a
  truncated one. Arming the result set's master rev re-derived every member
  through the engine — and each directory member's lookup walked the whole
  worktree, so an unbounded listing of a 95,000-file corpus burned upward of
  fifteen minutes after the rows themselves were ready in seconds, while
  `LIMIT 90000` (truncated, so never armed) returned in under forty. The set
  is now armed from the revs already carried by the served rows, for every
  FIND verb; the fresh per-member derivation remains where it belongs, in
  the `IF REV` verification before a bulk mutation. The total is counted in
  the same single clause pass that filters the rows — no clone, no second
  pass — and `LIMIT` only windows the end of that one pipeline. Without
  `LIMIT`, `FIND files` now serves the standard 20-row FIND page with the
  honest total, so the previously unbounded verb pages like every other
  FIND. A directory-heavy scaling test pins that a complete listing costs
  the same route as a truncated one, and a parity test pins that a master
  rev armed from served rows verifies against the fresh lookup.
- `FIND files` computes directory revs in one pass over the worktree instead
  of re-scanning the entire file list for every directory row. On a
  95,000-file corpus an unscoped `FIND files` previously spun for over 17
  minutes of CPU without answering; the listing is now one walk plus a lookup
  per row. The revs themselves are unchanged: a directory rev is still the
  same flat XOR of the path fingerprints of every file underneath it.


## [0.165.0] — 2026-08-13 — a commit writes a manifest instead of rebuilding the index

### Added

- Attaching to a commit whose merged index was never built no longer pays a
  full corpus merge. A commit now writes a small manifest naming the merged
  index it grew from and the files it changed; the next attach opens that
  index and serves the changes the same way uncommitted edits are served.
  Rows and totals are identical to a full rebuild. Queries that ride the
  name-stream and count fast paths take the slower complete path on such
  commits, exactly as they do in a session with uncommitted edits, and a
  missing or unreadable manifest falls back to the full build — a bad
  manifest can cost time, never rows.
- The ascending name-stream shapes — a bare `LIMIT` and `ORDER BY name` — no
  longer fall back to the complete scan in a session with uncommitted edits,
  or on a commit attached through its chain manifest. Every edited file's
  index carries its own sorted name list, so the stream merges those with the
  shared index name by name, skips the shared rows the edits replaced, and
  reports a total that counts the merged answer. The descending and
  kind-filtered stream shapes still take the complete scan in such sessions.
- A commit chain that has grown past a configurable size
  (`FORGEQL_CHAIN_COMPACT_PATHS`, default 512 changed-or-removed paths) is
  compacted on attach: its merged index is assembled once from the master
  index and the recorded changes, written where every later attach finds it,
  and the superseded manifest is removed. Below the threshold an attach
  keeps seeding the changes directly, which costs milliseconds; the
  compaction is the one full merge, paid once per chain instead of never
  bounding it.

### Performance

- The overlay build runs its enrichment-bitmap step, its name-index merge and
  its trigram pass in parallel instead of back to back. The three read the
  same immutable inputs and write different outputs, so the result is
  unchanged; the wall-clock cost of the group drops toward the cost of the
  slowest member. Peak build memory does not drop and can rise: the steps
  now hold their working memory at the same time instead of one after
  another.
- The overlay build computes the substring-search trigram bitmaps in a
  parallel pass over the per-file indexes instead of inside the serial
  name-index merge. The bitmaps produced are unchanged; the serial part of
  the build shrinks by the trigram work, which now spreads across cores.


## [0.164.0] — 2026-08-12 — a bare LIMIT rides the name index, and every stream counts the whole answer

### Performance

- A bare `FIND symbols LIMIT k` — no WHERE, no ORDER BY — is now served by the
  ascending name stream instead of materialising every row in the corpus. Its
  default ordering starts with `name`, so the streamed page is exactly the
  page the full scan sorted its way to. Measured on a 3.05 M-symbol corpus,
  twenty runs per shape with the session attach subtracted: the bare form
  took about 7.4 s per query where the explicit `ORDER BY name ASC LIMIT k`
  form took near zero. The bare form now takes the same path and its cost is
  no longer distinguishable from that of the explicit one, or from the
  run-to-run variance of doing nothing.
- The name-stream fast paths now report an honest `total`. They used to
  report the size of the page they streamed — `total: 5` on a corpus of any
  size — which was pinned as an expected failure in the golden suite and is
  now enforced: the whole-corpus total is the sum of the per-segment
  deduplicated row counts stored when the overlay is built, and a
  kind-filtered stream's total is its bitmap's cardinality, so neither walks
  the index it avoided reading. On the one index shape where those stored
  counts cannot speak for the answer — two segments built from one source
  path — every stream form, the bare one included, declines and the full
  scan answers: slower there, never a wrong count.
- The ordering that decides every `FIND symbols` page now ends its tie-break in
  `fql_kind`, making it `(name, line, path, fql_kind)` — the same four fields
  as the duplicate-collapse row identity, so two rows an answer tells apart
  never compare equal. Until now, rows equal on the ordering field and on
  name, line and path while differing in `fql_kind` were ordered arbitrarily:
  which of them a `LIMIT k` page held could change between runs or with `k`.
  That caveat is gone from the guarantee rather than restated. Two visible
  consequences: a page containing one of two such previously-tied rows may now
  deterministically hold the other one, and the in-memory backend collapses
  duplicate rows on the same identity the columnar backend uses (`fql_kind`
  rather than the language-level node kind), so the two backends agree on what
  a duplicate is. A segment that stores an enrichment column named `fql_kind`,
  shadowing the built-in field, now builds its rows before ranking them
  instead of ranking from its columns — slower for that segment, never a
  different page.


## [0.163.0] — 2026-08-12 — a commit no longer rebuilds the index for the commit it just made

### Performance

- A `COMMIT` no longer rebuilds the index for the commit it just made. It used
  to merge the whole corpus into a new overlay every time — every name, every
  row, changed or not — so committing one edited comment re-indexed everything.
  The overlay is a cache over the segments the commit promotes, and only the
  commit something later attaches to is ever read, so a worktree that commits
  ten times built ten and threw nine away. It is now built by whoever attaches
  to that commit: `USE` on a commit with no overlay builds one, which is what
  the takeover, the review, or the branch landing on main already does.

  Measured on the Linux kernel — 80,426 files, 29,864,281 rows — a `USE`,
  one-node edit and `COMMIT` went from **778 seconds to 80**, and its peak
  memory from **59.1 GiB to 23.6 GiB**. Ten commits in a worktree fall from
  about two hours to about thirteen minutes, plus one build when the work is
  picked up.

  The session keeps answering from its base overlay plus its dirty one, which
  is the same set of rows it was serving a moment earlier: a commit moves
  changes into git, it does not change which rows a query should return. What
  grows instead is the cost of reading them, since rows in the dirty overlay
  sit outside the base overlay's name index and posting lists — a worktree
  that commits a very large number of files will notice its queries slowing
  before it is merged.

  Two details that keep the guarantees intact. Promoting a staged segment now
  links it into the store rather than moving it, so the delta file that
  restores the dirty overlay after a restart still resolves; without that,
  every committed file would revert to its pre-edit rows on the next
  reconnect. And a `COMMIT` still refuses when a segment its index would be
  built from has vanished from the store, naming the files — the build that
  refuses on this now happens after the session that could act on it has gone,
  so the same fault is reported at the commit instead.


## [0.162.0] — 2026-08-12 — a cold index of the Linux kernel peaks at 19.6 GiB instead of 30.2

### Fixed

- A `USE` whose columnar index refuses to open now says so in its own response
  message, instead of only in the server log. The message carries the
  refusal's repair ("restore that file, or index this source from scratch"),
  which previously never reached the agent that had to act on it; a `USE`
  that resumes such a session repeats the note for as long as the session
  serves from the fallback, and a later attach that opens the columnar index
  cleanly drops it.
- The in-memory fallback behind that message now actually holds the index.
  With columnar configured, the index build emits segments inline and
  deliberately returns an empty in-memory table — correct while the columnar
  open succeeds, and exactly wrong on the one path that falls back to it: a
  session whose columnar open failed answered every query with zero rows
  while `USE` reported success. The fallback path now rebuilds the in-memory
  index for real before serving.

- Building an overlay now refuses when a segment it names is missing or
  unreadable, instead of skipping it. The skip produced an overlay that was
  self-consistent and silently smaller — every later session on that commit
  dropped the file's rows from every answer, with nothing anywhere saying so.
  The routes that reach this build hold no other copy of the rows (a COMMIT
  merges the base overlay with the session's staged segments; a rebuild from
  an inline segment map assembles what is already on disk), so the refusal
  names each affected file and where its segment was expected. A COMMIT
  refused this way is not sticky: restore the segment files, or re-index the
  source, and the same COMMIT succeeds.


### Performance

- Indexing no longer holds the macro table through the whole overlay build. It
  is collected in a first pass, feeds macro expansion while each file is
  indexed, and after that has exactly one reader left — the writer of the
  legacy on-disk cache, which the columnar path never calls. It was kept
  anyway, so a build held one heap allocation per macro definition, unread,
  until the session dropped its legacy index at the very end. It is now
  released as soon as the pass that needs it has finished. Incremental reindex
  is unaffected: it takes no macro table and never has.

  Measured on the Linux kernel — 6,119,906 macro definitions, 80,510 files,
  29,864,281 rows — the table is 6.5 GiB, and it was resident for the five
  minutes of overlay assembly that never looks at it, with every other phase's
  memory stacked on top.

  Together with segments no longer building a lookup table that only queries
  read, a cold index of that corpus peaks at **19.6 GiB where it used to peak
  at 30.2 GiB**, a 35% reduction, and produces a byte-identical overlay.

- Opening a segment no longer builds a lookup table nothing is going to read.
  Each segment carried a reverse map from string to id, built eagerly when the
  segment was opened, with an owned copy of every string in its pool as keys —
  bytes the memory-mapped file already holds. Its only callers are query
  prefilters, so an index build constructed one per segment and read none of
  them. It is now built the first time a prefilter asks that segment a
  question, and not before.

  Measured on the Linux kernel, 80,426 segments: the phase that opens them all
  for the overlay build went from 17.8 seconds to 1.4. The overlay it produces
  is byte-identical to the one built before the change, which is the property
  worth keeping if this is ever made eager again.

  This does not reduce the memory that phase holds — its 7.5 GiB of growth was
  unchanged by the removal, so the allocation belongs to something else opened
  at the same time.


### Changed

- The 2 GiB row budget now covers every path that serves result rows except
  `FIND files`, not only the `FIND symbols` scan over the on-disk index.
  Newly bounded, each with the same refusal and the same
  `FORGEQL_FIND_MAX_ROWS` override: the union of a session's uncommitted rows
  into that scan's answer (previously appended after the check), the
  `FIND usages` site list on both backends (on the on-disk one the check runs
  between matching tiers, so the peak can overshoot the bound by one tier's
  finds), and the in-memory backend's scan, trimmed or not — its running trim
  holds the retained window to a few multiples of the `LIMIT`, which keeps
  any small page clear of the budget, and a `LIMIT` so large that even its
  trimmed window outgrows the budget now refuses exactly as the on-disk
  backend refuses it. `FIND files` deliberately carries no budget: its answer
  is one small row per workspace file, so its size is the workspace's file
  count, never anything a query matches.


### Added

- Every indexing phase now logs how much memory it is holding, beside the time
  it already logged. A build that ends at 29 GiB used to say only which phase
  was slow, so attributing the peak to a phase meant guessing, and a report of
  "the reindex needed 25 GB" could not be acted on without reproducing it under
  a profiler. Each `TIMING` line now carries `anon`, `file` and `peak`.

  The split matters more than the total. `anon` is heap and thread stacks —
  memory the process owns, that nothing else can reclaim, and that an
  out-of-memory kill is decided on. `file` is page cache behind the mapped
  segments: shared between every session that mapped the same segment, dropped
  by the kernel under pressure, and counted once per mapping rather than once
  per page, so it overstates. `top`, `ps` and every process viewer report the
  two added together, which is why a large reading there says nothing about
  whether a build is in danger. Measured on the Linux kernel, an index peaking
  at 29.2 GiB was 15.5 GiB of the first kind and 10.2 GiB of the second.

  Two phases that had no line at all now have one, both single-threaded and
  both landing after the overlay build reports its total — which made them read
  as time nothing accounted for. Opening the overlay and its segments is timed
  separately from building them, because a cold index opens every segment twice
  and only the first open was visible; and freeing the build-time table is
  timed, because returning one heap allocation per macro definition is
  single-threaded work that looks like a hang on a corpus with six million of
  them.



## [0.161.0] — 2026-08-12 — LIMIT bounds delivery, and `total` counts the answer

### Fixed

- The `total` a `FIND symbols` response carries is now the number of rows that
  matched, not the number the page holds. It used to be taken after `OFFSET`
  and `LIMIT` had already cut the page, so under any explicit `LIMIT` the two
  were equal and every first page read as a whole answer:
  `ORDER BY lines DESC LIMIT 2` over seven matching rows answered `total: 2`
  and now answers `total: 7`. The running top-K trim, which discards rows on
  rank while the segments are still being read, now reports how many it dropped
  so those rows are counted too.
- An `ORDER BY … LIMIT k` page can no longer come back holding fewer than `k`
  rows because duplicates collapsed after the page was chosen. Rows agreeing on
  name, kind, path and line are now collapsed per segment before anything sheds
  on rank — both in the running trim and in the bounded choice a segment makes
  over its own row IDs before building any of them — so a group of duplicates
  sorting to the front of the window no longer costs the distinct rows that
  belonged in the answer. Where two segments were built from the same source
  path a duplicate pair can span both, which a per-segment collapse cannot see;
  there the trim is not armed at all rather than shed on an incomplete one.
- A `LIMIT` with no `ORDER BY` no longer changes the answer. It used to stop the
  scan after `LIMIT + 1` rows, so `total` came back as the size of the page
  rather than the number of rows that matched, an `OFFSET` paged past rows that
  were never fetched, and raising the limit could surface rows a smaller one had
  not shown. Such a query is now bounded the way an ordered one is — a running
  top-K trim across the whole scan — because the pipeline already sorts by
  `(name, line, path)` when no `ORDER BY` is given, so `LIMIT k` asks for the k
  smallest rows under that ordering rather than for whichever k the scan reached
  first — with one qualification the ordering never settled in the first place:
  where more rows than the page holds compare *equal* on the ordering field and
  on all of `name`, `line` and `path`, which two rows can be while differing in
  `fql_kind`, the bounded partition is unstable and raising the limit may still
  swap which of them is shown. Four cases in the golden suite that recorded the
  old behaviour as a known defect are now enforced instead.
- The in-memory backend carried the same defect at its own early exit and is
  fixed the same way: it too builds and ranks every matching row, holding the
  retained set to a bounded window instead of keeping whichever rows the scan
  reached first.

### Changed

- The trim that bounds a page needs `k` no greater than 1000, no `OFFSET`, no
  `GROUP BY` and no `HAVING`; outside that gate an unscoped scan materialises
  every matching row and can now be refused by the row budget where the old cap
  let it complete with a wrong answer.
- The candidate row IDs a scan holds before it builds anything now have a bound
  of their own, about 537 million — the same 2 GiB of memory measured against
  four bytes a row ID instead of 1,600 a built row.
  `FORGEQL_FIND_MAX_ROW_IDS` overrides it and `0` disables it, matching
  `FORGEQL_FIND_MAX_ROWS`.

### Known defects

- One place still answers with the size of the page rather than of the answer:
  `ORDER BY name` with a small `LIMIT`, no `IN`/`EXCLUDE` and no uncommitted
  edits streams `limit + offset` rows out of the name index and stops, so it
  cannot count what it did not read. It is pinned as a failing case in the
  golden suite rather than left to be discovered.

## [0.160.0] — 2026-08-11 — an ordered page is chosen from the columns, and only its winners are built

### Performance

- `ORDER BY <field> LIMIT k` no longer builds every matching row of a file in
  order to throw almost all of them away. A file whose rows the running top-K
  trim was going to shed now ranks them from its own stored columns, read in
  place, and builds only the ones that survive. The rows returned do not change:
  the ranking uses the same comparator that sorts the built rows, at the same
  threshold and keeping the same number, so what it sheds is what the trim would
  have shed on the very next statement. Rows the comparator ranks equal are the
  one exception, and only in the sense they always were — where more rows tie on
  the `ORDER BY` field and on `name`, `line` and `path` than a page can hold,
  which of them it shows was never decided by the ordering and still is not.
- The choice is made per file, so one file cannot switch it off for the rest. It
  is made only where a file can rank by every field that comparator reads — the
  `ORDER BY` field and the `name`, `line`, `path` tie-breakers — and where no
  predicate is left to run once a row exists. `ORDER BY usages` is therefore
  still built first, because the stored count is a stale zero that the workspace
  count replaces during materialisation, as is any ordering by a field a file
  stores under a name of its own. A field a file simply does not carry is fine:
  it reads as absent both before and after a row is built, and ranks the same
  either way.
- The `k` no greater than 1000, no `OFFSET`, no `GROUP BY` and no `HAVING` gate
  is unchanged, and so is the caveat that duplicate rows are collapsed after the
  page is chosen.


## [0.159.0] — 2026-08-11 — a page holds what the query asked for, and one decode serves every session

### Added

- `RUN`, `VERIFY build` and `JOB START` accept a heredoc for their
  arguments, not only a quoted literal. A quoted argument cannot carry the
  quote that delimits it, and this DSL escapes neither by doubling nor by
  backslash — `''` split the
  argument in two and `\'` failed to parse. Any prose holding both an
  apostrophe and a double quote was therefore unwritable as a step argument
  at any length, which ruled out passing a message, a report, or an issue
  body to a step. The argument was already bound to the step's stdin rather
  than to its command line, so the limit was never size; it was quoting.

      RUN 'file_issue' <<BODY
      `WHERE name = 'x'` answers with a row that carries no handle,
      and the message reads "not found". It doesn't refuse.
      BODY

  All three take arguments through the same binding — `JOB START` submits a
  verify step, so the two spellings of one step now accept the same
  arguments. Quoted arguments are unchanged.

  One constraint worth knowing when the body is prose: a line that is itself
  all-uppercase reads as a closing tag, so a bare `TODO` or `NOTE` line ends
  the body early and the statement is refused for a tag mismatch. Choose a
  tag, or indent the line.

### Fixed

- **A `HAVING` predicate now decides which rows a page contains, rather than
  which of an already-chosen page survive.** `FIND symbols … HAVING … ORDER BY
  name LIMIT k` returned the wrong rows, silently: ordering by name walks the
  name index and stops once it holds `k` rows, and `HAVING` is applied after
  that, so the answer was the first `k` rows by name minus those failing the
  predicate instead of the first `k` rows by name that satisfy it. Rows that
  qualified were never fetched, nothing was truncated in the reply, and no error
  was raised. A limit far larger than the answer truncated it just the same, and
  reversing the direction changed which rows came back at all — on one corpus a
  seven-row answer came back as three ascending and one descending.
- The same omission was in three more places, found by asking where else the
  engine stops reading early: the running top-K trim, which sheds rows mid-scan
  by the ORDER BY; the segment fetch cap, which stops opening segments once it
  holds `LIMIT + 1` rows and so broke the same query shape with no `ORDER BY` at
  all — a seven-row answer came back as none at `LIMIT 3` and as two at
  `LIMIT 1001`; and the in-memory backend's own early exit, which had the same
  gap. That backend answers every query for a source with no `.forgeql.yaml`,
  since no columnar index is built for one, so the gap was reachable in ordinary
  use rather than only through an explicit choice of backend. All are now gated
  on `HAVING` being absent through one shared predicate, so the two backends
  cannot drift apart on it again and the next stage added after paging has a
  single place to declare itself.
- One gap in the coverage, stated because it would otherwise be invisible: the
  in-memory backend's fix is not pinned by a test. The query-level cases run
  against the columnar backend — every corpus they can use carries a
  `.forgeql.yaml`, and installing a columnar index drops the in-memory table — so
  removing that one condition leaves the whole suite green. The fix is derived
  from the code path rather than demonstrated by a failing case, and the test
  file says so where a reader will meet it.
- **This is the third defect of one shape**, and worth naming: an early exit that
  assumes `LIMIT` bounds the answer while a later stage still filters. The other
  two are still open, each pinned as a known defect rather than fixed. A bare
  `LIMIT` with no `ORDER BY` truncates the scan instead of paging it, so the
  reported `total` falls short and an `OFFSET` pages past rows never fetched —
  four `expect_fail` cases in
  `crates/forgeql/tests/golden/clause_pipeline.json`. And duplicate rows are
  collapsed only after the top-K trim and the name-index streams have stopped
  reading, so an `ORDER BY … LIMIT k` page can come back shorter than `k` — an
  ignored test, `crates/forgeql-core/tests/topk_trim_before_dedupe.rs`. Each was
  found separately, and each cost a separate investigation to attribute. Anything
  that stops reading early has to account for every stage that still removes
  rows.

- `WHERE file MATCHES ...` and `WHERE file NOT MATCHES ...` silently ignored the
  `file`/`path` alias and every other alias the field table declares. Every other
  operator (`=`, `LIKE`, `>`, `HAVING`, `ORDER BY`, `GROUP BY`) resolved an alias
  to its canonical field before reading a row; the regex operators read the
  written field name as-is. An alias that named no struct field and no
  enrichment map entry then read as absent on every row, so `MATCHES` matched
  nothing and `NOT MATCHES` matched everything — both silently, with no error.

  The regex operators now resolve the field the same way every other operator
  does, once, before compiling the pattern. `WHERE file MATCHES` now answers
  identically to `WHERE path MATCHES`.

- An index that names a segment the disk no longer holds no longer serves rows
  through it. The readers are addressed by position, so a reader that would not
  open did not merely remove that file's rows: it shifted every later position,
  and rows were then served against a different file's reader — the right name
  and line under another file's path and content, and node handles addressing a
  file the query never named. An answer, not an error, and wrong. The state is
  reachable without anything exotic — an external cleaner, or a reclaim that
  removed a segment file while leaving the index that names it — though
  ForgeQL's own `VACUUM` cannot produce it, since it removes whole cache-version
  directories, index and segments together.
- What happens instead depends on whether the segment can be written again.
  Rebuilding an index by shadow-writing from a merged symbol table can write a
  segment that is absent, so where one is available the rebuild is allowed to
  run — and the open then checks that the segment is really there before
  answering from it. That check is what makes this safe rather than hopeful: a
  rebuild skips a segment whose first few bytes still look right even when
  opening it fails, a failed write leaves nothing behind, and the assembly step
  that ends every rebuild drops what it cannot read. So the index is repaired,
  or the open refuses; it is never quietly one file smaller.
- A rebuild that could only assemble the segments already on disk — which is
  what runs whenever the caller carries an inline set of them — cannot write a
  missing one back, and refuses without running. Having *a* rebuild available is
  not enough; it has to be one that can write the segment.
- A refusal names the index, the file, where the open looked for it and why it
  would not open. A readable index file is never deleted on the strength of a
  missing segment, in either direction — removing one cannot be undone, and the
  repairing rebuild replaces it atomically without needing it gone. An index
  file that is itself unreadable is still removed and rebuilt, exactly as before.
- A refused open caches nothing, so putting the missing segment back by hand is
  enough for the very next open to succeed, without a restart.
- Two boundaries this does not cross. A session whose commit is in this state
  still answers completely and does not report the refusal: the columnar open
  declines, the refusal is logged, and the session falls back to its full
  in-memory index. And assembling an index from the segments on disk still drops
  one it cannot read wherever that assembly is reached other than through an
  open — the open is guarded by the check above, nothing else is.

### Performance

- Sessions reading the same commit now share one copy of the committed index.
  Opening a session decoded that commit's overlay and one reader per indexed
  file, and every session on the commit repeated the whole of that work.
  Measured on a large repository with three sessions open on one commit in a
  single process, the second and later sessions each cost 617 MB of private
  heap at peak; they now cost 2-3 MB. A few sessions warming together has been
  enough to exhaust the memory on the machine serving them.

  The overlay and its readers cannot change once opened, and every file behind
  them is addressed by its content, so they are now opened once per commit and
  handed to each session, behind a per-commit gate so that sessions starting
  together decode once between them rather than once each. The background
  warmer's "is this commit already built?" is answered from that same state,
  where it previously decoded an index only to discard it.

  A session's own uncommitted edits are untouched by this: they were never part
  of the shared state, and still take precedence over it at query time.

  Opening those sessions is also faster, for the same reason: three sessions on
  one commit previously performed three decodes and now perform one, and the
  measured time to open them fell to roughly a third.

  The saving is per *additional* session on a commit, so a benchmark that opens
  a single session cannot see this change at all. Nothing is held back for a
  session that has not arrived either: an entry lives exactly as long as some
  session is holding it.

- A query with a `WHERE` the index cannot answer used to build every candidate
  row and then throw the non-matching ones away. `FIND symbols WHERE fql_kind =
  'if' WHERE line >= 100 WHERE line <= 105` built a full row — its name, kind,
  language, path, node handle and enrichment map — for every `if` in the corpus
  in order to keep the few hundred on those lines.

  The residual `WHERE` is now tested against the segment's columns first, in
  place, and only the rows that pass are built. Answers are unchanged: same
  rows, same order, same totals, same paging.

  A predicate the columns cannot answer is not dropped — it is handed to the
  filter that runs after the rows are built, exactly as before. That covers a
  workspace usage count, which is only known once a row exists; a node handle,
  which is built as the row is; a field no column of that segment holds; and a
  regular expression, which is cheaper compiled once for a batch than once per
  row. A segment that stores an enrichment column named after a field a result
  row answers from a struct field of its own answers no fixed column early at
  all, because the two would disagree.

  Measured on a three-million-symbol corpus, this is a saving of time and not
  of memory, and the saving is confined to queries that ask for a large
  filtered answer: streaming twenty thousand name-ordered rows runs 1.7x
  faster, while queries capped at a small number of rows, and unfiltered scans
  with nothing to discard, are unchanged. Peak resident memory does not move on
  any of them, because the discarded rows were already built and released one
  file at a time — they cost processor time, never footprint. What a large
  answer holds in memory is the rows it returns, and a filter cannot reduce
  those.

- `FIND symbols` no longer copies each candidate row's name, kind, path and line
  into an owned key while it removes duplicates. Collapsing duplicates means
  comparing every candidate against the ones already kept, and it did that by
  building a set of owned keys -- three heap allocations per candidate row, held
  for the whole pass on top of the result set it was scanning. It now stores a
  64-bit hash of those same four fields and confirms each hash match against the
  fields themselves, so a collision costs a comparison rather than changing the
  answer. The answer is unchanged for every input.

  Measured on a 2.9-million-symbol corpus with `FIND symbols`, which builds
  every row, over two runs: peak private memory 7,219 MB before against
  6,912 MB after, and 7,225 before against 6,915 after -- a saving of 307 MB
  and 310 MB. The same query ran 17.5 s before against 15.3 s after, and
  16.6 s before against 15.0 s after. Run-to-run variation on the "before"
  figures is larger than the gap between the two savings -- 6 MB against 3 MB
  for memory, 0.9 s against 0.6 s for time -- so read the pair of deltas rather
  than either single run.

  Both results are for a query that materialises every row. The memory saving
  was flat on a 20,000-row query -- 2,550 MB against 2,548 MB -- which is what
  a per-row saving of this size looks like at that scale; the time was not
  measured there, so nothing is claimed about it.

- `Overlay`'s enrichment-field lookups (equality, existence, and the
  numeric `>=`/`<=` prefilters) now binary-search the on-disk index
  directly instead of first copying every distinct `field=value` key into
  an owned `String` at session open. The on-disk array was already sorted
  and already binary-searched by key, so the copy reproduced work the file
  format had already done; removing it drops one heap allocation per
  distinct enrichment key from every session's private memory. No on-disk
  format change.

  Measured on a 3M-row corpus: resident private memory right after opening
  a session did not move outside rounding (2,516 MB before and after, at
  megabyte-granularity sampling). The eliminated allocations are real, but
  they are small next to a multi-gigabyte baseline whose bulk is per-row
  segment decode, not this index -- a few hundred distinct keys at roughly
  sixty bytes each does not register against 2.5 GB. The query class
  measured for timing does not exercise this code path (it reads the
  unrelated kind-bitmap index instead), so no timing claim is made here.

- Reading that index in place makes its ordering load-bearing, so the blob
  is now validated once when the file is opened: every key must lie inside
  the key region and must not sort before its predecessor. A blob that
  fails is refused whole: the enrichment tier then reports no opinion and
  those queries fall back to reading the segments — slower, never fewer
  rows — rather than a single corrupt entry silently misdirecting a binary
  search past its well-formed neighbours. This can only trigger on an overlay file
  truncated or damaged on disk -- never on one this build wrote -- and
  costs one allocation-free pass over the entries at open.

- Also corrected the file's own module doc, which had drifted three format
  revisions out of date: it still described a 9-blob, 600-byte table of
  contents, and still gave the header's schema version as 3. The format in
  use has 13 named blobs behind an 856-byte header + TOC region, and a
  schema version of 15 -- a value the reader refuses to open a file without.
  The doc now lists all thirteen blobs, including the four it had never
  mentioned.

- A segment carrying an enrichment column named after one of a result row's own
  fields no longer stops answering *every* such field from its columns — only
  the colliding name falls back. The two have to be told apart because an
  operator whose type does not match the struct accessor falls through to the
  enrichment map of the built row and finds the shadow column there, which the
  row view would never consult; but a collision on one name says nothing about
  the others, and the whole segment was being sent to the slower path for it.
  No query answer changes — only how early a predicate is decided. Predicates on
  the fields a segment does not shadow are filtered before their rows are built
  again, as they are in every other segment. The effect on a real corpus has not
  been measured; segments carrying such a column are the unusual case.

### Changed

- The bound on how much one `FIND symbols` may materialise is now stated as the
  memory it is meant to protect — 2 GiB of result rows — rather than as a row
  count. It had been a bare five million rows, chosen when a result row was much
  smaller and never revisited as the row grew; measured against today's rows
  that authorised roughly 7.5 GB, against a budget of 2 GiB. The count is now
  derived from the budget and the working cost of a row, which works out at
  about 1.34 million, and a test pins the derivation so the next growth of the
  row moves the bound instead of quietly widening what a query may spend.
- **This lowers the effective ceiling about 3.7x, so a scan that used to
  complete can now be refused.** That is a reachability change rather than a
  restatement, and where the new line falls on a multi-million-symbol corpus has
  not been measured. The case to watch is a `GROUP BY` that no fast path
  accepts: its answer is a handful of rows, but it materialises every matching
  row to get there. `FORGEQL_FIND_MAX_ROWS` raises the bound in rows and `0`
  disables it.
- The refusal is unchanged in kind — past the bound a query has always been
  refused rather than truncated, which is what keeps a partial answer from
  passing for a complete one — and it now names two remedies that hold. Narrow
  the scan with `IN 'path/**'` or a more selective `WHERE`; or order it, since an
  `ORDER BY <field> LIMIT k` with `k` no greater than 1000, no `OFFSET` and no
  `GROUP BY` still scans every segment while a running top-K trim holds the
  working set to a few thousand rows, so that form returns the true top K and
  never reaches the default bound. Outside that gate nothing is trimmed and the
  query is refused as before. The ordered form has one hole of its own, described
  in its own entry: the trim sheds rows before duplicates are collapsed, so the
  page can come back short.
- The bound is now documented, in `doc/syntax.md` and in each of the agent
  guides. It was enforced but written down nowhere, so the first an agent knew
  of it was a refusal citing an environment variable it had never been told
  about.
- What the bound does not cover, stated wherever it is claimed: it is enforced
  on the `FIND symbols` scan over the on-disk index and nowhere else — and there
  it is tested once per segment rather than once per row, so the real peak is
  the budget plus the one segment being materialised. `FIND usages` builds its
  rows in one step on both backends, `FIND files` pushes one entry per file with
  no bound at all, the in-memory backend materialises its whole result before any
  clause applies, and a session's uncommitted rows are unioned in after the
  check. A query answered by one of those can still exhaust host memory.

- A bare `LIMIT` is documented as what it is: delivery paging, not a way to bound
  an oversized scan. Without an `ORDER BY` a `LIMIT` stops the scan early rather
  than paging its result, so an `OFFSET` pages past rows that were never fetched
  and which rows come back depends on the limit itself. Four `expect_fail` cases
  in `crates/forgeql/tests/golden/clause_pipeline.json` pin that defect. An
  `ORDER BY <field> LIMIT k` — with `k` no greater than 1000, no `OFFSET` and no
  `GROUP BY` — does not have that problem: it scans every segment and a running
  top-K trim bounds the working set, so it is the ordered form, not the bare one,
  that the result-budget refusal now offers as a remedy. Outside that gate
  nothing is trimmed. The ordered form has a separate hole, described in its own
  entry: the trim sheds rows before duplicates are collapsed, so the page can
  come back short.
- Related, and stated at each of those sites: under any explicit `LIMIT` the
  `total` that `FIND symbols` reports is the number of rows returned, not the
  number that matched, because the `LIMIT` is applied before the total is taken.
  `FIND usages` deliberately differs — its `total` is the true site count, which
  is what a rename campaign measures progress against.

### Notes

- Known defect, now reproduced and written down: an `ORDER BY <field> LIMIT k`
  can return fewer than `k` rows even when far more than `k` distinct rows match.
  While a scan runs, a top-K trim sheds rows down to `k * 2` whenever the working
  set passes `k * 4`, and duplicate rows are only collapsed afterwards. So where
  enough rows agreeing on `name`, `fql_kind`, path and line sort ahead of the
  distinct ones, the retained window can be filled by rows that later become one,
  and distinct rows that belonged in the answer were already discarded. The page
  comes back short without saying so.
- This is stated wherever the ordered form is recommended — the refusal text,
  `doc/syntax.md` and each of the agent guides — because that form is the remedy
  offered for a scan too large to materialise, and recommending it without naming
  its hole is the failure that advice is supposed to prevent. The reproduction is
  an ignored test, `crates/forgeql-core/tests/topk_trim_before_dedupe.rs`, which
  builds the rows directly so the counts are exact; the control beside it asserts
  the same index answers with more distinct rows than the page asks for, so the
  case cannot quietly stop reproducing. Nothing changes in what a query returns:
  the defect is unchanged, only no longer undocumented.


## [0.158.0] — 2026-08-09 — a segment's columns are found once, not once per value

### Changed

- Reading rows out of a columnar segment named its columns as it went. Every
  value of every row built the string `col_<name>` and looked that name up in
  the segment's table of contents: five allocations and five hash lookups per
  row for the fixed columns, and one more of each for every enrichment column
  on top of them. Nothing being looked up had moved since the file was mapped,
  and a query that returns three million rows repeated the lookup three million
  times.

  A segment now locates its columns once, when the file is opened, and every
  accessor indexes the mapping directly from the byte range resolved then. The
  enrichment columns are resolved once per batch of rows instead of once per
  row. The mapping itself is untouched — the index is still navigated in place,
  with no copy — and the answers are identical: same rows, same order, same
  totals. What changed is the cost of producing an answer, not what the answer
  says.

  Measured on a three-million-symbol corpus, this is a saving of time and not
  of memory. Queries run 1.7x to 5.1x faster depending on how many rows they
  materialise — a whole-corpus scan 2.1x, a scan filtered to one node kind
  2.9x, that scan narrowed further by line range 5.1x, and name-ordered
  streaming 1.7x. Peak resident memory does not move: the strings this removes
  were built and dropped within a single column read, so they cost processor
  time rather than footprint, and what a large answer holds in memory is the
  rows themselves.

  A column a segment does not store resolves to the empty range, which reads
  exactly as a missing table-of-contents entry did: `0`, `u32::MAX`, or no
  value, never a failure. Segments written by an older version, which can lack
  a column entirely, are covered by a test that drives every accessor down that
  path rather than by the hope that some corpus still contains one.

## [0.157.0] — 2026-08-08 — one clause, two consumers: WHERE is split between the lookup and the rows

### Fixed

- `SHOW members OF 'Foo' WHERE language = 'cpp'` answered with nothing for a
  type that exists. These verbs — `SHOW members`, `SHOW callees`, `FIND callees
  OF`, `SHOW body`, `SHOW context`, `SHOW signature` — hand one clause to two
  consumers: the lookup that decides which `Foo` was meant, and the rows that
  come back from it. The clause went whole to both. The lookup evaluated none of
  it, and then every returned row failed a predicate naming a field it does not
  carry, so the answer was empty. Zero members for a type that has members is
  the same confident absence the previous release set out to remove, arriving by
  a different route.

  Each predicate now reaches the consumer that can answer it, decided by the
  row shape: a field the returned rows carry filters those rows, and one they
  do not carry goes to the lookup, which evaluates it against every candidate.
  `SHOW members OF 'Point' WHERE language = 'rust'` returns the Rust `Point`'s
  members and `WHERE language = 'cpp'` the C++ one's, and the two answers
  differ. No predicate is dropped. The only kind that reaches both consumers is
  a field the rows carry whose value comes from the resolved symbol rather than
  the row — `path` on a callees row, since every call sits in the resolved
  function's own file — where filtering the rows by it could only keep all of
  them or none, and scoping the lookup by it is what was meant. `IN` and
  `EXCLUDE` go the same way on the verbs whose rows carry no file of their own.
  Both backends do this; the indexed one previously applied no `WHERE` at all
  during resolution, and pays for it now only when the lookup's half is
  non-empty.

- When no candidate satisfies the lookup the answer says so —
  `no symbol 'Point' matches WHERE language = 'python'` — naming the clause that
  excluded it. A name that does not exist and a name that exists but was scoped
  away are different facts, and an agent told only "not found" for the second
  goes and checks its spelling instead of its filter. Neither reads as a
  refusal.

- A lone candidate is no longer waved through unfiltered. Where a name had
  exactly one definition, `IN` and `WHERE` were skipped as an optimisation, so
  `SHOW body OF 'f' WHERE language = 'python'` returned the C++ `f`, and an `IN`
  naming a directory it does not live in returned it too.

- `SHOW COMMITS` accepted `WHERE path LIKE '%src%'` and answered a confident
  zero. It filtered commit rows through the symbol shape, which carries `path`,
  `line` and `fql_kind` — all empty on a commit — and, being open-ended, could
  refuse nothing. Commits now have their own closed shape, `hash` and `subject`,
  and any other name is refused with those two named.

- `SHOW NODE` and `SHOW LINES` address bytes by handle or by path, so nothing
  resolves a name and their whole clause belongs to the lines they return.
  `SHOW NODE '<id>' WHERE language = 'cpp'` is refused rather than silently
  emptying the node.

- `ORDER BY`, `GROUP BY` and `HAVING` are refused on every verb that answers
  with source lines — `SHOW body`, `SHOW context`, `SHOW signature`,
  `SHOW NODE`, `SHOW LINES`, `SHOW MORE`. Those answer in source order and
  nothing sorts, groups or aggregates them, so the clauses were accepted and
  read by nothing; because `LIMIT` *is* honoured, that silence produced a wrong
  answer rather than an inert one — `SHOW body OF 'f' DEPTH 99 ORDER BY line
  DESC LIMIT 4` handed back the first four lines, the opposite page to the one
  asked for.

- `IN` and `EXCLUDE` are refused on the verbs that resolve no name and whose
  rows carry no path — `SHOW NODE`, `SHOW LINES`, `SHOW MORE`, `SHOW COMMITS`.
  A glob there has neither a lookup to scope nor a row path to match. On
  `SHOW COMMITS` it was worse than inert: every commit row failed the path
  filter, so `SHOW COMMITS IN 'crates/**'` answered zero — the same confident
  absence that the closed commit shape had just been introduced to stop.

- `OFFSET` without a `LIMIT` was read by nothing on a line answer, so
  `SHOW body OF 'f' DEPTH 99 OFFSET 40` returned lines 1-40 — exactly the page
  it asked to skip. The pair had been gated on the `LIMIT` being present; every
  other verb applies `OFFSET` on its own, and now so does this one.

- `DEPTH` is read by three verbs — `SHOW body` (collapse depth), `SHOW context`
  (context window) and `FIND files` (tree depth) — and the parser handed it to
  all fifteen, so everywhere else it was accepted and read by nothing. It is
  refused there now. `SHOW outline` was the misleading one: its rows carry a
  `depth` column of their own, so `SHOW outline OF 'f' DEPTH 2` read as a
  request for a depth-limited tree and returned the whole one.

- `CHANGE FILE` accepted the whole universal clause block and read none of it —
  the range it rewrites travels in its target, not its clause. On a mutation
  that is the worst place to ignore one: an agent that believed it had scoped
  an edit had not. Any clause there is now refused.

### Changed

- Every clause field is checked **before** the verb does its work: before the
  lookup runs, before a diff is computed, before the paging buffer is read,
  before the commit history is opened. A check that ran afterwards arrived as
  "no symbol matches" or "could not open worktree" — a fact about the code or
  the workspace standing in for a fact about the query.

- `ORDER BY`, `GROUP BY` and `HAVING` are never split. No lookup reads them, so
  on every verb they must name a field the returned rows carry:
  `SHOW members OF 'Foo' ORDER BY language` is refused while
  `WHERE language = 'cpp'` is accepted. The asymmetry is deliberate, and is now
  stated in the same sentence as the split wherever the split appears.

- `SHOW signature` no longer ignores its clause. It renders one line rather than
  a row set, so it has no row consumer at all: the whole clause reaches the
  lookup — including `line` and `node_id`, which a source line and a symbol row
  both carry and which a split would have handed to a row half that does not
  exist — and a field only a source-line row carries (`text`, `marker`, `rev`)
  is refused rather than accepted and quietly discarded.

- One limit, stated alongside the claims rather than elsewhere: only
  `FIND symbols`, `FIND globals` and `FIND usages` can tell an unknown field
  from an unmatched one, because only they see which enrichment columns the
  index stored. On a `SHOW`, a misspelt field reaches the lookup, satisfies no
  candidate, and is reported as a lookup that matched nothing.

- A test reads the list of clause-carrying verbs out of the command IR instead
  of restating it, and requires each to be named together with the query it must
  refuse. `SHOW COMMITS` had been missing from two hand-written copies of that
  set; a verb added without a decision about how its fields are checked now
  fails the suite rather than shipping unchecked.

## [0.156.0] — 2026-08-08 — a field a query cannot answer is refused, on every verb

### Changed

- A `WHERE`, `ORDER BY`, `GROUP BY` or `HAVING` naming a field the verb's rows
  cannot carry now returns an error naming the field and where it *is*
  answered. It never returns zero rows. Zero rows is a claim about the corpus;
  an error is a fact about the query, and the two are indistinguishable in the
  answer. Seven names had been accepted and answered a confident zero on
  `FIND symbols` — `count`, `size`, `depth`, `extension`/`ext`, `signature`,
  `marker` and `declaration` — because the list validation consulted is a union
  across every result shape, so membership said the name exists *somewhere* and
  nothing about the query in hand. Each verb is now checked against the shape it
  actually returns: `size`, `depth` and `extension` belong to a `FIND files`
  row, `marker` to a `SHOW` line row, `declaration` to a `SHOW members` row, and
  every refusal says so.

- `count` is the one name whose answer depends on the clause rather than the
  operator: the grouping pass is what writes it, so `HAVING count >= 2` and
  `ORDER BY count DESC` read a real number and a `WHERE` on it — which runs
  before grouping — reads nothing on every row. It is answered in the first two
  and refused in the third, with a message that says which to use.

- `SHOW outline`, `SHOW members`, `SHOW callees` and `FIND callees OF` can
  refuse at all. They answered with a JSON value and had no channel for an
  error, so a clause naming a field their rows cannot carry was accepted and
  dropped every row in silence — `SHOW callees OF 'f' WHERE fql_kind = 'x'`
  returned an empty call graph rather than saying a call site has no kind. They
  return a `Result` now, and the `node_kind` refusal that previously stopped at
  the four `FIND` verbs reaches every verb that carries a clause but one:
  `FIND symbols`, `FIND globals`, `FIND usages`, `FIND files`, `FIND callees
  OF`, `SHOW outline`, `SHOW members`, `SHOW callees`, `SHOW body`, `SHOW
  context` and `SHOW NODE`. `SHOW signature` is the exception and stays one: it
  renders a single line rather than building a row set, so it applies no
  `WHERE` and refuses none either.

- The two checks reach different sets of verbs, and both say which. `FIND
  files`, `SHOW outline`, `SHOW MORE`, `SHOW DIFF` and the `FIND` verbs over
  symbol rows resolve nothing, so their clause is only ever a row filter and
  every field their rows cannot carry is refused, with the ones they do carry
  listed. `SHOW members`, `SHOW callees`, `FIND callees OF`, `SHOW body`, `SHOW
  context` and `SHOW NODE` hand the same clause to the symbol lookup as well,
  so gating on their row shape would refuse `SHOW members OF 'Foo' WHERE
  language = 'cpp'`, which the legacy backend answers by scoping the lookup.
  Those verbs refuse only what no row of any shape can answer.

- **Known, unfixed, and now pinned by a test:** on the columnar backend — the
  one every session queries — the symbol lookup evaluates none of those
  predicates (only posted-enrichment `=`), so `SHOW members OF 'Foo' WHERE
  language = 'cpp'` scopes nothing and then filters away every member of the
  type it did find. An empty answer for a symbol that exists. It is left alone
  deliberately: refusing it breaks the legacy backend, where the predicate does
  scope the lookup, and dropping it before the row filter answers as though it
  had been applied — measured, `WHERE language = 'rust'` then returned the C++
  members, a false positive traded for a false negative. The fix belongs in the
  resolver, evaluating the predicate per candidate as the legacy backend
  already does.

- `FIND files` refuses a `GROUP BY` on a field a file row cannot resolve,
  closing the last path that fabricated a group. Grouping keys a row through
  one string accessor and defaults an unresolved name to the empty string, so
  `FIND files GROUP BY lang` reported exactly one group, named by the empty
  string, whose count was every file. The same check now covers `SHOW outline`,
  `SHOW MORE` and `SHOW DIFF` — the other verbs whose clause only filters —
  each against its own row shape.

- `SHOW outline` opens its full set of nodes for **any** `WHERE`, not only for a
  predicate on the kind field. Which field the predicate named used to decide
  which rows existed: `WHERE fql_kind = 'guard'` searched every node in the file
  while `WHERE name = 'x'` searched only the structural tree, so a comment or a
  guard region named `x` was reported absent. It also made `depth` — which
  counts the ancestors that were listed — vary between two predicates on the
  same file. A filter now never searches a smaller tree than the one it is
  written against.

- Field aliases are resolved in one place, from one table. `kind`, `file`,
  `lang`, `ext` and `content` are spelled to their canonical names by
  `field_tiers::canonical`, and every comparison that asks "is this clause
  naming *that* field" — the kind fast path, the `GROUP BY` fast path, the
  grouping renderer, the outline universe, the row resolvers, the diff line
  filter — goes through it. Each of those previously carried its own list, and
  an alias known to one and not another is how `WHERE kind = 'guard'` came to
  answer zero where `WHERE fql_kind = 'guard'` answered three, on the same file
  and the same field. Adding an alias is now a one-line change to the table, and
  a test drives every alias in it through both spellings and compares the
  answers.

- The key column of a `GROUP BY` keeps the spelling you wrote — `GROUP BY file`
  still heads its column `file` — while the grouping itself runs on the
  canonical field, so an alias and its canonical name always produce the same
  groups under different labels. Grouping on the kind is the one case with no
  key column at all: the compact layout already groups a symbol listing by
  kind, so `GROUP BY fql_kind` and `GROUP BY kind` both render in that layout.

### Fixed

- `SHOW DIFF WHERE content LIKE '%x%'` filtered file rows instead of diff lines.
  Only the exact spelling `text` was routed to the line filter, so the alias
  fell through to the row filter, which no diff row resolves, and every file was
  dropped.

## [0.155.0] — 2026-08-07 — `node_kind` is refused instead of silently matching nothing

### Changed

- `node_kind` in `WHERE`, `ORDER BY` or `GROUP BY` now returns an error naming
  `fql_kind` instead of an answer, on `FIND symbols`, `FIND globals`, `FIND
  usages` and `FIND files`. `node_kind` is the raw tree-sitter grammar kind: it
  is computed while parsing to drive kind mapping and is stored on no row of
  the index a session queries, so every materialised row reported it absent.
  Each clause failed a different way and all three read like answers — the
  predicate matched nothing while its negation matched everything, the sort
  tied every row and fell back to name order, and the grouping put every row
  into one empty-named group whose count was the whole result. `SHOW outline`,
  `SHOW members` and `SHOW callees` answer with a value rather than an error
  and still accept the field, matching nothing. Serving it would mean storing
  it, which is an index-output change and is not part of this release.
- `FIND symbols` and `FIND globals` refuse a `GROUP BY` on a field a symbol row
  cannot resolve, rather than answering it. Grouping previously reported
  exactly one group, named by the empty string, holding every row — a shape
  indistinguishable from a real result, and one that `GROUP BY size`,
  `GROUP BY kind` and `GROUP BY marker` all produced. Grouping keys a row
  through its string fields only, so the accepted set is exactly those: `name`,
  `fql_kind`, `path`/`file`, `language`/`lang`, `node_id`, any enrichment field,
  and any extra column a segment stores. `line`, `usages` and `count` are
  numeric and are not groupable; `size`, `depth` and `extension` belong to
  `FIND files`, which groups file rows through its own path and is unchanged
  apart from `node_kind`. `FIND usages` groups occurrence rows, where `role`
  and `file` are the useful keys.
- `kind` now resolves everywhere it is printed. It was accepted by validation
  on `FIND symbols`, `SHOW outline` and `SHOW members`, resolved on none of
  them, and so answered a confident zero — while being the very key a symbol
  row is printed under in JSON output, so copying the field name out of an
  answer produced a query that matched nothing. It is an alias of `fql_kind`
  on all three, in `WHERE`, `ORDER BY` and `GROUP BY`, and on `FIND symbols`
  it reaches the same kind bitmap `fql_kind` does rather than falling to a
  scan. On `SHOW outline` the two spellings also open the same set of rows:
  the default outline lists structural declarations only and a kind predicate
  opts back into every node, so had `kind` not been carried into that decision
  it would have filtered the structural tree while `fql_kind` filtered the
  file — `WHERE kind = 'guard'` answering zero where `WHERE fql_kind = 'guard'`
  answers three, for the same file and the same field. `GROUP BY` resolves the
  alias in one place for the same reason: grouping has two implementations,
  one the backend runs for `fql_kind` and a generic one for everything else,
  so the alias is spelled to its canonical name as the clause is parsed rather
  than taught to each. Left to the generic path it rendered `"(empty)",9`
  where `GROUP BY fql_kind` rendered three real groups — the fabricated group
  this release exists to refuse, reintroduced at the renderer.
- `GROUP BY node_id` groups by the handle instead of collapsing. The projection
  that builds each row's group key read the enrichment map for any field it did
  not name explicitly, and `node_id` is not in it, so every row keyed to the
  empty string. It now falls back to the same row resolver `WHERE` and
  `ORDER BY` use, so a field the grouping check admits cannot arrive at the
  renderer unresolved. Rows that carry no handle — kinds the index does not
  make addressable — still group under `(empty)`, which is the honest key for
  a row that has none.
- The `ORDER BY` and `GROUP BY` refusal messages list the accepted fields from
  the same constants that decide acceptance, rather than restating them. Both
  lists had already drifted from the fields this release added.
- `text` and `content` are refused in `ORDER BY` and `GROUP BY` as well as in
  `WHERE`. They were already rejected in `WHERE` on FIND queries for the same
  reason — no row of a FIND result carries source text — but the other two
  clauses went through and produced the tie and the fabricated group above.

### Added

- A declaration, one row per queryable field, of where its value is stored,
  which structure serves each operator class, whether that structure decides
  the answer or only proposes candidates, what it cannot see and which
  mechanism covers that, the budgets bounding it, and the measured cost or the
  benchmark class that would produce one. It is a parallel declaration —
  nothing reads it while answering a query, and the lists the builder and
  query path actually consult are unchanged — paired with a test suite that
  checks every claim it makes against those lists, against the builder's own
  budget functions, and against what a query returns on a real workspace. A
  field whose declared queryability and actual serving path disagree now fails
  the test suite rather than answering wrongly, which is how each of the last
  four releases' defects reached a corpus in the first place.

### Fixed

- Documentation described `node_kind` as merely "deprecated" for query use in
  the syntax reference, the architecture notes and both agent guides, which
  read as a style preference rather than as a query that cannot work.

## [0.154.0] — 2026-08-07 — `FIND usages` finds a name written on a line nothing in the index recorded

### Fixed

`FIND usages` could report zero for a name a file plainly contains. In a manual
page, a `.. code-block:: c` block holds the line `#define DT_DRV_COMPAT
dummy_device`; asking for `DT_DRV_COMPAT` in that directory answered nothing at
all, while `gpio_callback` three lines below it answered normally. The
difference is not the file and not the block: it is that a nested construct
there gets a node of its own and the surrounding block's own text does not, so
nothing recorded a token for those lines.

Every tier of the matcher answered out of those recordings, so all of them were
bounded by the same gap — a plain identifier as much as a dotted name — and no
rearrangement of them could see a line none of them held. An agent reads zero
as "this name is not used here" and deletes accordingly.

**The files themselves are now read, on every query, and the line's own text
decides what is a site.** A site exists wherever the bytes say it does, whether
or not anything recorded it, so an empty answer now means the corpus does not
hold the name. The recordings are still consulted, but only to *label* what the
bytes found: a site something did record keeps its role — `code`, `comment`,
`string`, `config`, `doc` — instead of flattening, and only a line nothing
recorded arrives as `text`. The authority runs the other way too: where the
bytes were read, a recorded site the bytes no longer carry is dropped rather
than reported, so a segment that has drifted from the worktree — a build step
wrote the file, a checkout replaced it — can no longer hand back a line that
does not hold the name, under a file handle and a rev that both still resolve.
Recordings still stand on their own only for a file the read could not open or
could not decode, where they are the only evidence there is.

Every file the workspace knows about is read, not only the ones that produced
symbols: a `.gitignore`, or any extension no plugin claims, is tracked by path
and size alone and holds text like anything else. `FIND usages OF 'cscope'`
answered zero across a three-million-symbol tree whose root `.gitignore` names
it on line 23 — a zero an agent reads as permission to rename. The set read is
the same one `FIND files` lists, minus ForgeQL's own runtime artifacts. Binary
files — a NUL byte near the start, the line `grep` draws — are not searched,
because an object file or an index blob embeds symbol names and a site there is
bytes no sweep should rewrite. Everything else decodes leniently, so a file that
is text apart from a stray byte in a legacy encoding still answers on every line
that holds the name; a file that exists and cannot be read is still counted in
the response's `hint` — a count, not a list of paths.

A byte-order mark is believed before that binary check, so **UTF-16 text is
searched** rather than mistaken for an object file: every ASCII character in
UTF-16 carries a NUL byte, which is exactly what the check looks for, so
without this a plain UTF-16 document answered zero over lines it visibly holds.
Only a mark counts, and only UTF-16 — UTF-16 written without one cannot be told
apart from a compiled object, and guessing from NUL density would let an object
file arm a sweep; UTF-32 is not decoded at all, even where its own mark declares
it. Both stay unsearched, and this is said plainly rather than left to be
discovered.

**A UTF-16 site can be found and cannot be rewritten in place.** A line
boundary there is two bytes, not one, so splicing UTF-8 text into it by byte
offset would shift every byte after the edit and destroy the file — and nothing
previously stopped that, because the line-to-byte conversion scans for `0x0A`
and finds one. Any node- or line-scoped `CHANGE`, `INSERT` or `DELETE` on such
a file is now refused with an error naming the encoding, and so is a
`COPY LINES` or `MOVE LINES` whose destination is one — those reach a
destination through a separate offset calculation, and they are the edit verbs
for exactly the non-indexed files this release added to the search universe.
Replacing the file whole *through a node handle* is refused with them: a
whole-file `CHANGE NODE` is lowered to a line range like any other, and a line
range over UTF-16 does not even reach the last byte. Replacing every byte at
once is safe and is not refused — `CHANGE FILE '<path>' WITH ...` does it, and
is available on non-indexed files, which is what a UTF-16 file in a source tree
usually is; an indexed one has to be deleted and written again, or converted
outside ForgeQL. Reading such a line back is bounded the same
way: `SHOW` renders the raw bytes, so the decode reaches the site list and not
the display.

One class of file was still outside that set: one **created during the session**
whose extension no plugin claims. It produces no segment, and the list of
non-indexed files is a snapshot taken when the workspace was indexed — so
between writing a new `prj.conf` and committing it, the file was in nothing the
search enumerated and every query over it answered zero. The session now records
those paths, and both `FIND files` and the read pass include them. A path the
index lists but the worktree no longer holds — a file deleted in the session —
is skipped without comment instead of counted as unreadable: it has no bytes, so
the answer over it is complete rather than short.

The universe now has one stated edge rather than an unstated one: a file that
reaches the worktree without passing through ForgeQL — written by a build step,
say — is in no segment, no file list and no session record until it is indexed,
and a file excluded by `.gitignore`, `.ignore` or `.forgeql-ignore` is in none
of them at all — unless this session touched it, since a mutation records the
path it wrote without consulting any ignore rule: a path no plugin claims is
then listed and searched until the next commit and not after, while one whose
extension a plugin claims gets a segment instead, and a segment is carried
through the commit, so that file stays in both from then on. `FIND files`
excludes exactly the same two on the same terms, so it does not list what the
read pass does not open. The two
answer over one set, and that set is what "every file the workspace knows
about" means everywhere the phrase appears.

**Reading files does not widen a query into a substring search.** An identifier
is matched on token boundaries — a letter, digit or underscore in any script
continues a token — so `FIND usages OF '256'` still means the token `256` and
never the digits inside `sha256`, and `k_sleep` is not a site inside `ék_sleep`.
That is the rule `MATCHING WORD` rewrites on, so a find and the sweep it arms
agree about where a token starts and ends. A name carrying anything outside
`[A-Za-z0-9_]` is matched literally, separators and all, which is what makes an
include path askable.

### Changed

`IN` and `EXCLUDE` now bound the reading as well as the rows: a file outside
their globs can only produce rows the clause pipeline would drop, so it is never
opened. Scoping a blast-radius query to the subtree you are about to edit does
proportionally less work for an identical answer.

Counts grow wherever the gap was real. On a three-million-symbol embedded tree
the unfiltered site count for one widely-used configuration flag moves from 140
to 179, and all thirty-nine added sites are exactly the ones nothing had
recorded — most of them project configuration files switching that flag on. A
golden pins those thirty-nine beside the total, so the two have to move
together and the growth stays accounted for rather than absorbed.

**Cost, measured.** An A/B run against that corpus, both sides built with the
same profile, leaves all three query shapes below the harness's noise floor on
the branch exactly as on the baseline — identifier names, names carrying a
separator, and names no part of which can be a token all under 200 ms per query
either way, a difference the harness cannot resolve, with the branch reading
every in-scope file on every query. Those three classes carry no `IN` scope, so
the shape measured is the whole-workspace one — the worst case, not a narrowed
one — and the query used to confirm the read actually happens returns 179 sites
on that corpus where the postings alone return 140. Reading the whole workspace
per query is therefore not detectable at this corpus size; a tree large enough
to make it so would be bounded by `IN` and `EXCLUDE`, which cut the reading and
not just the rows.

**Upgrading with uncommitted work.** The session delta gained a field, so its
format version moved and a delta written by an earlier build is no longer read
as-is. It is not discarded either: each version so far has added a trailing
field and the encoding is read as a prefix, so the previous layout still yields
its staged paths and its removal set. A session reconnecting across the upgrade
with uncommitted edits keeps its deletions shadowed and gets its edited files
queued for a fresh index — the staged segments go, the knowledge of which files
need rebuilding does not. A delta older than one layout back is refused
outright, because there is no safe way to guess where its fields begin.

## [0.153.0] — 2026-08-07 — the guard set fields and `key_path` are indexed

Re-indexing is required: this release changes what a segment stores, so the
cache version moves and every source is re-indexed on first use.

### Changed

- **`guard_defines`, `guard_mentions`, `guard_negates`, `guard_group_id` and
  `key_path` now have a serving index.** They were excluded from the posting
  index by a single global budget of eight distinct values per file, which
  exists for fields like `naming` that have a handful of values. These five do
  not: measured on a 3,062,139-symbol corpus they carry 45,918 / 42,451 /
  19,421 / 10,157 / 9,032 distinct values. Every query on them therefore
  materialised the whole corpus so the row-level filter could read a field the
  row already carried. The budget is now per field, and these five are indexed
  under one sized for them.

  What is indexed is each field's **whole value**. The guard fields hold a
  comma-joined set, and every operator has always compared that joined string,
  so the index keys the same thing the query compares. Indexing the individual
  members instead would key something nothing compares against — and would
  break two existing behaviours silently, since both read these keys as values:
  a regex spanning a comma would match no key and return nothing, and a whole
  value that is not a member would be reported absent.

- **Membership on a set-valued field has an exact spelling, now documented.**
  `guard_defines = 'CONFIG_BT'` means "guarded by CONFIG_BT *and nothing
  else*" — it has never meant membership, and still does not. For membership
  use `guard_defines MATCHES '(^|,)CONFIG_BT(,|$)'`, which is exact, is served
  by the same index, and does not also match `CONFIG_BT_HCI` the way the
  previously documented `LIKE '%CONFIG_BT%'` does. The recipes in
  `doc/syntax.md` and `doc/agents/forgeql.agent.md` were using the substring
  form and have been corrected.

A file whose distinct-value count for one of these fields exceeds the per-file
budget writes no index for it — never a partial one — and every row of such a
file stays a candidate on every operator, including the numeric comparisons
`guard_group_id` accepts, so the answer is complete whichever files the budget
covered. The whole-workspace budget is all-or-nothing in the same way: the
merged index holds every value of a field or none of it.

Measured after release on the 3,062,139-symbol corpus (ci profile, ten
distinct queries per class, before → after; "<200 ms" is the harness's noise
floor, and the answers were verified row-identical across the two versions):

| query shape                                    | before      | after   |
|------------------------------------------------|-------------|---------|
| `guard_mentions = '<flag>'`                    | 16.8 s/q    | <200 ms |
| `guard_defines = '<value that exists nowhere>'`| 26.0 s/q    | <200 ms |
| `guard_mentions MATCHES '(^|,)<flag>(,|$)'`    | 15.7 s/q    | <200 ms |
| `key_path LIKE '<prefix>%'`                    | 18.1 s/q    | <200 ms |
| `guard_defines MATCHES '<alt regex>'`          | 1.3 s/q     | <200 ms |
## [0.151.0] — 2026-08-07 — `WHERE language = '<lang>'` reads the language column instead of every row

### Changed

- **`language` / `lang` predicates are served from the stored language
  column.** Every row already carries its language as a stored value, but no
  index tier consulted it, so the query materialised the whole corpus — a full
  result row and enrichment map apiece — for the row-level filter to read a
  field the row was holding all along. On the 3,046,476-symbol reference corpus
  that measured 4.6 s per query. Comparing the stored values instead touches
  one integer per row and constructs nothing. `=`, `!=`, `LIKE`, `NOT LIKE`,
  `MATCHES` and `NOT MATCHES` all take this path; a numeric comparison on
  `language` still falls through to the scan.

  Unusually for a fast path, the candidate set it produces is **exact** rather
  than a superset: every row of every file is decided against its own stored
  value, so an empty answer is a conclusion the tier is entitled to reach.
  `FIND symbols WHERE language = 'cobol'` on a corpus with no COBOL now returns
  nothing without reading a row. A row whose stored language is empty matches
  nothing at all, negations included, which is what the row-level filter has
  always done with a field a row does not report. A file whose language column
  does not account for its rows one-for-one sends the whole query back to the
  complete scan rather than answering from part of the picture.

Other core row fields were audited and left alone. `name` and `fql_kind`
already have index tiers, `line` and `usages` have range maps, and `path`,
`file` and `node_id` are high-cardinality — they need a different kind of index
and are not this change.

`node_kind` came out of that audit worse than unserved, and this release does
not fix it: nothing stores it per row, so every row materialised from the index
reports it as absent, and a query on it answers with false confidence rather
than slowly. `WHERE node_kind = '<anything>'` returns no rows at all, and
`WHERE node_kind NOT MATCHES '<anything>'` returns every row. Closing it means
either storing the field or refusing the predicate outright; until one of those
lands, do not read a `node_kind` result as evidence of anything.

## [0.150.0] — 2026-08-07 — regex and LIKE are answered from a field's values, not its rows

### Fixed

- **`WHERE <enrichment field> = '<value>'` could return fewer rows than the
  index holds.** The workspace overlay stores one row bitmap per
  `field=value`. For the low-cardinality fields carried on the per-file posting
  list those bitmaps are assembled from the postings — and a file whose
  distinct-value count for such a field exceeds the per-file budget contributes
  no postings at all, while still storing the column. Its rows therefore carry
  values that no bitmap names, and narrowing a query to that bitmap dropped
  them from the answer without a word. In a two-file workspace where one file
  opens ten `#if`/`#elif` arms and the other two,
  `FIND symbols WHERE guard_branch = '0'` returned the second file's two rows
  and neither of the first file's. Such a file's rows now stay candidates and
  the row-level filter decides them, so the answer is complete whichever files
  the budget covered. Fields keyed the other way — by walking each file's rows,
  which is every field not on the posting list — were never affected and are
  left alone.

### Changed

- **`MATCHES`, `NOT MATCHES`, `LIKE` and `NOT LIKE` on an indexed enrichment
  field are now served by an index tier.** The pattern is evaluated against the
  field's DISTINCT VALUES — a few dozen, corpus-wide — and the matching values'
  row bitmaps are unioned, so the cost tracks how many values a field has
  rather than how many rows the corpus holds. Previously only `=` consulted the
  index and every pattern fell through to materialising all 3,046,476 rows of
  the reference corpus so the filter could reject them one at a time; the regex
  itself was never the expense. Arbitrary patterns are fine by construction:
  the value set is filtered by an opaque predicate, so an alternation or a
  character class costs exactly what a literal costs.

- **`WHERE name MATCHES '<regex>'` is evaluated against the name index's keys.**
  The distinct names of a corpus number in the hundreds of thousands where its
  rows number in the millions, so the regex is asked once per name. The literal
  prefilter this precedes cannot serve an alternation at all — a match needs
  the literals of one branch, so intersecting the branches' candidate sets
  drops every real match — and correctly declined, which meant a full scan.
  `name MATCHES '^(alpha|beta)$'` measured 2.1 s/query before this change.

Both tiers propose candidates that the row-level filter still verifies, and
both step aside — back to the complete scan — wherever the value universe
cannot account for every row: a field the index does not carry, a pattern that
accepts the empty string (an empty value is never keyed), a name key that is
not valid UTF-8, or an unreadable bitmap. Rows from files edited in the current
session are matched after the candidate set is built and are unaffected.
## [0.149.2] — 2026-08-07 — asking for an enrichment value that does not exist no longer scans the corpus

### Fixed

**`WHERE <enrichment field> = '<value>'` cost a full-corpus scan whenever the
value did not exist.** The workspace overlay stores one row bitmap per
`field=value` pair. When a query named a value the corpus never stores, that
lookup missed, the prefilter reported "no opinion", and the engine fell back to
materialising every row in the index so the residual filter could reject them
one at a time. The answer was correct — always empty — but it was paid for by
reading the entire corpus.

On a 3,046,476-symbol corpus, `FIND symbols WHERE guard_kind = 'ifdef'` took
**7.48 s to return nothing**. It could never return anything: `guard_kind` only
ever holds `preprocessor`, `attribute` or `heuristic`, so `ifdef` matches no row
by construction. A missed lookup now yields an empty candidate set directly,
and the same query answers in **under 200 ms** — the floor this bench can
resolve. Queries naming a value that *does* exist are unchanged, whether the
value is common or rare (both measured at the same floor before and after), so
this is strictly the absent-value path.

The identical mistake was fixed for `fql_kind` some time ago, where an unknown
kind used to cost about eight seconds for the same reason; enrichment fields
were simply never given the same treatment.

An empty candidate set is an assertion — "no such row exists" — so the answer
is not taken from the overlay, which is not in a position to make it. The
overlay's keys are built only from rows that survive a per-segment
`(name, fql_kind, line)` dedup and the shadowing of duplicate source paths, so
a value carried solely by a losing twin is keyed nowhere even though the row
exists and the scan would return it; a missing key also cannot be told apart
from a key whose bitmap failed to read. The question is instead put to the
per-segment postings, which are keyed by raw row id and so cover every row a
segment holds.

The shortcut is confined to fields the enrichment index actually serves. Core
row metadata such as `language` is stored elsewhere entirely, so that index
having nothing to say about it is not evidence about which rows exist, and
those queries keep their existing path untouched.

A segment that holds the field but no postings for it — which is what the
builder does when the field's per-segment cardinality exceeds its cap — can
prove nothing, and a single such segment is enough to keep the complete scan.
A ten-arm `#if`/`#elif` chain does exactly this to `guard_branch` in one file,
and that case is pinned by a regression test.

Rows from uncommitted in-session edits were never at risk: they are
materialised in a later stage, downstream of the candidate set, which is also
pinned by a test.
## [0.149.1] — 2026-08-06 — fix: `FIND usages` stopped searching once one tier had answered, and stopped early when the work looked large

### Fixed

The tier that reaches a name the index stored only in pieces ran **only when the
tiers above it came back empty**. So a name one language stores whole and
another stores split answered from whichever tier spoke first and silently
dropped every site the other one held. On a three-million-symbol embedded tree,
`FIND usages OF 'pm/device_runtime'` answered 161 C include sites and never
mentioned the Python file that spells the same path inside a string a few
directories away — a complete-looking answer that was missing sites, which is
the failure the tier was added to remove.

All three tiers now answer every query and their sites are merged into one list
with one honest `total`. A site two tiers both reach is listed once.

The 5,000-candidate ceiling is gone. It abandoned the whole search when a name's
parts together proposed more lines than that, and said so in a `hint` — but a
hint explains a paged *delivery*; it cannot turn an unfinished *search* into an
answer. ForgeQL answers like a database: `find` walks the whole filesystem
however long that takes, and `LIMIT`/`OFFSET` page a result computed in full.
Every candidate a name's parts propose is now read, however many there are.

**What that costs, measured rather than assumed.** An A/B run against a
three-million-symbol embedded corpus, both sides built with the same profile,
puts the substring-needle class below the measurement harness's noise floor on
the branch exactly as on the baseline — under 200 ms per query either way, a
difference this harness cannot resolve. Two things that run does not cover, said
plainly: a name whose every part is a common word — `zephyr/pm/device.h`, whose
parts are between them posted on some seventy thousand lines of that tree — now
reads every file holding a candidate before it answers, and a name with no
usable part reads the files in scope directly. Neither is measured. Either may
be slow on a large corpus until those lookups are served from an index; that is
the accepted trade, and the answer is complete meanwhile.

`IN` and `EXCLUDE` now bound the reading as well as the delivery. A candidate
outside the query's own globs would be dropped by the clause pipeline anyway, so
it is no longer read at all — identical rows, less work, and the lever to reach
for when a blast-radius query on a large tree is slower than you want. It asks
the pipeline's own matcher rather than keeping a second copy of the rule, on
purpose: a look-alike that failed to expand a bare directory into `dir/**` would
discard every candidate before reading it and answer a confident zero, which is
the failure this whole change exists to remove.

### Added

A name no part of which could ever have been a token — `a.b`, `->`, `1.2` —
leaves the token index with nothing to propose from at all. Those queries used
to answer zero with a reason; they now read the indexed files and return the
lines that hold the name. Such a site carries the new `role` value `text`: the
bytes say the line holds the name, nothing says what kind of occurrence it is,
and nothing pretends to.

The only `hint` `FIND usages` still emits for reasons of its own names a
candidate file that could not be read — an answer short by something specific,
never short because the work looked large.

## [0.149.0] — 2026-08-06 — fix: `FIND usages` answered zero for a name the index stored only in pieces

### Fixed

`FIND usages OF 'forgeql-pub.frozen'` returned nothing on a corpus holding that
string on forty-odd lines across eight files, while `FIND usages OF 'forgeql-pub'`
returned rows from those same files. Same for `zephyr-andre.frozen`,
`golden.json`, `tests/golden` and `Cargo.toml`: a silent zero, indistinguishable
from "this name is not used here", on the query agents run to size a rename.

Both existing tiers can only answer with a **stored token** — exact lookup for a
name written in `[A-Za-z0-9_]`, substring-of-a-stored-token for one carrying
anything else. What gets stored is what the recorders produce, and a recorder
continues a token only over the extra characters its language declares: `/` and
`.` for a C include path, so `zephyr/pm/device.h` is one token and has always
been findable, but nothing of the sort for a JSON string, where
`forgeql-pub.frozen` is stored as `forgeql-pub` and `frozen`. Neither tier can
reach a name that was never stored whole, however many files hold it.

A third tier now runs when those two find nothing and the name carries a
non-identifier character. The name's own pieces *were* stored, so they propose
candidate lines and the source line itself confirms them:

- **Every piece proposes, not the cheapest one.** Which piece a given site
  stored depends on the extra characters that site's language continues a token
  over, so the cheapest piece is routinely not the one that reaches the sites.
  For `forgeql-pub.frozen` the piece `pub` is the cheapest by a wide margin and
  is stored on none of the lines that hold the name, while `frozen` is stored on
  every one of them. Proposing from the cheapest piece would have replaced the
  silent zero with a silent undercount.
- **The line is the arbiter**, so no proposal however loose can produce a false
  positive. A line carrying every piece in some other arrangement —
  `frozen.forgeql-pub` against a file full of `forgeql-pub.frozen` — is rejected.
- **One read per file**, not one per candidate site.
- **All or nothing.** When the pieces together propose more than 5,000 candidate
  lines the tier declines to run and says why, because a truncated site list
  reads exactly like a complete one — the failure being fixed here. A name with
  no run of two or more identifier characters opening on a letter (`a.b`) has
  nothing to propose from, and also answers with a reason rather than a bare zero.

The tier is skipped entirely when the tiers above found the name, because then it
IS a stored token and they are authoritative for it. An include path such as
`zephyr/pm/device.h` still answers every one of its sites from the whole-token
tier without paying for a piece enumeration, so no query that worked before does
any new work.

Nothing stored changes: no enrichment version bump, no reindex, no rewarm.

### Changed

`Storage::find_usages` returns `(Vec<SymbolMatch>, Option<String>)`. The second
value is a reason the answer may be short of what the corpus holds, and reaches
the caller as the query's `hint`; `None` means the sites are everything the index
can reach.

Within the new tier, two of a name's parts landing on one line propose it twice
and it is verified once, so a line is reported once however many parts reached
it. Rows from the tiers above are untouched.

### Known gap

Two cases stay narrower than they could be. A name that is only ever a *fragment*
of a stored token — `orgeql-pub` against a stored `forgeql-pub` — is reached only
where some other piece of it happens to be a stored token on the same line, or
through the whole-token dictionary, which holds usage tokens and not mention
tokens. And a name stored whole by one language while another language in the
same corpus splits it keeps only its whole-token sites, because the new tier does
not run once the tiers above have answered.

## [0.148.24] — 2026-08-06 — fix: the golden harness reissued a failed attach's alias and never cleaned it up

### Fixed

The golden suite failed intermittently — four of seven runs over two unrelated
change sets — with cases dying at `USE` rather than at an assertion:

```
USE zephyr-andre.frozen AS 'gt-2623291-3-0': tools/call error:
{"code":-32603,"message":"reference refs/heads/fql/anonymous/zephyr-andre/frozen/gt-2623291-3-0 is already checked out; class=Worktree (32)"}
```

Two defects in the harness (`crates/forgeql/tests/golden_test.rs`), either of
which alone leaves the suite flaky.

**A failed attach's alias was reissued.** `session_for` numbered its aliases
`gt-<pid>-<member>-<sessions.len()>`, and `sessions` only grows when a `USE`
returns cleanly. A `USE` that failed *after* the engine had already created the
branch ref therefore left debris named exactly what that pool member's next
attach would compute, and the member collided with its own dead first attempt —
every alias in every observed failure was a member's first, `-0`. The index now
comes from a counter bumped on every alias the harness mints, successful or not,
so a failed attempt's name is never reachable again. `rw_session` numbered its
own aliases the same only-advances-on-success way, from `created.len()`, and now
draws from the same counter.

**A failed attach was never recorded for teardown.** Both methods pushed the
alias onto the client's cleanup list *after* the `USE` returned, so an attach
that failed partway left behind the branch it had already created — permanently,
one landmine per failed run, in a bare repository shared between sessions. Any
later run that computed that name failed on contact through no fault of its own.
The alias is now recorded before the `USE` is issued. Tearing down a session that
never existed is harmless: teardown resolves a branch name derived from that
session's own coordinates, so it can only ever remove that session's own worktree
and branch; finding neither warns once per repository in the data dir and changes
nothing.

This makes cleanup possible, not certain. Teardown deletes the real session
branch by reading it from the live worktree, and falls back to a legacy name that
matches nothing when that worktree cannot be opened — so a `USE` that created the
ref but no usable checkout still leaks it. The failures actually observed report
`is already checked out`, which means a worktree registration did exist, so they
are covered; the ref-only case is not, and closing it means changing how teardown
resolves the branch, which is engine code and not part of this change.

### Added

Two harness self-tests, registered as trials beside the suites — 265 golden
cases, 267 trials:

- `harness_selftest::failed_use_consumes_its_alias_index` — two `USE`s of a
  source that cannot exist must mint two different aliases, and a real attach on
  the same member afterwards must still succeed.
- `harness_selftest::failed_use_is_recorded_for_teardown` — a failed `USE` must
  already appear, under its own alias, in the list the client tears down on drop.

They run on their own engine process under their own member index, and are
appended after the round-robin loop, so which pool member handles which golden
case is unchanged.

### Note

What makes that first attach fail is a separate question and is not addressed
here. The collision is now unreachable either way — the second attempt no longer
asks for the first one's name — but a seed failure would still surface as one
failing case. It was only ever seen on the largest corpus, and always on a pool
member's first attach, which points at contention between the four members
opening sessions on one bare repository at once.

## [0.148.23] — 2026-08-06 — refactor: lift the COPY/MOVE tests out of `transforms/copy_move.rs`

### Changed

`crates/forgeql-core/src/transforms/copy_move.rs` was 468 lines, and 226 of them
were an inline `#[cfg(test)] mod tests` block.

- The block moves to `crates/forgeql-core/src/transforms/copy_move/tests.rs` —
  thirteen `#[test]` functions plus the `apply_plan_to_bytes` and `make_file`
  fixtures, covering appending, inserting before a target line, and copying to a
  destination that does not yet exist; moving across files and within one file
  in both directions, including a wider delete-end that absorbs the source's
  trailing blanks; the errors for a destination inside the source range and for
  a delete-end before the end; and how an insertion line resolves to a byte
  offset. The parent declares `#[cfg(test)] mod tests;` and drops to 244 lines.
- **No import rewiring**, for the same reason as the nine lifts before it: an
  inline `mod tests` and a `tests.rs` file are the same module path, so the
  block's `use super::*;` and its two other module-scope imports still resolve
  as they did — as does the function-local `use std::fmt::Write;` inside
  `make_file`.
- The `// Tests` banner rule stays in the parent above the declaration.

The moved code is unchanged apart from the four-column de-indent that un-nesting
forces — 223 body lines out, 223 in — and the child opens with a six-line `//!`
header that exists nowhere in the parent.

No `ENRICH_VER` bump: nothing here changes index output.

### This completes the inline-test lift series

Ten source files carried `#[cfg(test)] mod tests` blocks large enough to bury
the code they tested. Across 0.148.14–0.148.23 those blocks became sibling
`tests.rs` files: **3,153 body lines, 165 tests and 15 fixtures moved**, and the
ten parents went from 7,058 lines to 3,895.

Nothing was rewritten. An inline `mod tests` and a `tests.rs` file are the same
module path, so no import and no visibility needed adjusting in any of the ten,
and the three blocks that carried a lint attribute — two `#[expect(…)]` and one
`#[allow(…)]` — moved theirs onto the `mod tests;` declaration verbatim, in the
same file and position, widening nothing. The only change to the moved code in
every case was the four-column de-indent that un-nesting forces, plus a `//!`
header naming what each child covers.

Files with blocks of 200 lines or more still remain, most of them smaller than
these. `ast/enrich/guard_utils.rs` has the largest in the tree at 613 lines and
is deliberately untouched while other work is rewriting that file.

## [0.148.22] — 2026-08-05 — refactor: lift the git-diff tests out of `git/diff.rs`

### Changed

`crates/forgeql-core/src/git/diff.rs` was 623 lines, and 238 of them were an
inline `#[cfg(test)] mod tests` block.

- The block moves to `crates/forgeql-core/src/git/diff/tests.rs` — nine
  `#[test]` functions and no fixtures, covering the HEAD-to-worktree diff and
  what it leaves out, the worktree diff that does include untracked files, the
  runtime and control files both exclude, and the source-changes classification
  that tells research apart from work. The parent declares
  `#[cfg(test)] mod tests;` and drops to 387 lines.
- **No import rewiring**, for the same reason as the eight lifts before it: an
  inline `mod tests` and a `tests.rs` file are the same module path, so the
  block's `use super::*;` still resolves to `crate::git::diff`. Its three other
  imports are unaffected for the same reason — including
  `crate::git::testutil::make_normal_repo`, which is `crate::`-absolute and so
  never depended on the file's depth at all.
- Unlike the earlier lifts in this series there is no `// Tests` banner comment
  to leave behind — this file never had one, and none was added.

The moved code is unchanged apart from the four-column de-indent that un-nesting
forces — 235 body lines out, 235 in — and the child opens with a four-line `//!`
header that exists nowhere in the parent.

No `ENRICH_VER` bump: nothing here changes index output.

## [0.148.21] — 2026-08-05 — refactor: lift the transform-plumbing tests out of `transforms/mod.rs`

### Changed

`crates/forgeql-core/src/transforms/mod.rs` was 683 lines, and 266 of them were
an inline `#[cfg(test)] mod tests` block. This is the module every transform
routes through, so it is a natural first stop for a contributor, and 39% of it
was the tests.

- The block moves to `crates/forgeql-core/src/transforms/tests.rs` — fifteen
  `#[test]` functions and no fixtures, covering applying an edit set to a
  buffer, reverse-sorting edits so earlier offsets stay valid while later ones
  are rewritten, merging edits by file and detecting overlaps, and the
  `lines_written` / `lines_removed` counts a mutation reports back. The parent
  declares `#[cfg(test)] mod tests;` and drops to 419 lines.
- **No import rewiring**, for the same reason as the seven lifts before it — but
  the file layout genuinely differs here. The parent is a `mod.rs`, and Rust
  resolves `mod tests;` against the directory *containing* it, so the child is
  the sibling `transforms/tests.rs` rather than a new subdirectory. The module
  path is `crate::transforms::tests` either way, which is what makes
  `use super::*;` resolve unchanged. `parser/tests.rs` is the existing example
  of this exact shape; `transforms/diff/tests.rs` is not — its parent is the
  plain file `transforms/diff.rs`, so it follows the other pattern.
- The `// Tests` banner rule stays in the parent above the declaration.

The moved code is unchanged apart from the four-column de-indent that un-nesting
forces — 263 body lines out, 263 in — and the child opens with a four-line `//!`
header that exists nowhere in the parent.

No `ENRICH_VER` bump: nothing here changes index output.

## [0.148.20] — 2026-08-05 — refactor: lift the session-file tests out of `session.rs`

### Changed

`crates/forgeql/src/session.rs` was 436 lines, of which 268 — **61% of the
file** — were an inline `#[cfg(test)] mod tests` block. That is the most
lopsided ratio of any file in this series: someone opening it to learn how a
session is persisted and resumed met roughly two lines of test for every line
of the thing being tested.

- The block moves to `crates/forgeql/src/session/tests.rs` — eighteen `#[test]`
  functions plus the `os` and `make_test_engine` fixtures, covering
  config-directory resolution from `XDG_CONFIG_HOME` or `HOME`, the session file
  path, loading a file that is missing, empty, invalid, partial or complete,
  saving and its round-trip, and resume, which no-ops without a session id and
  clears one that has gone stale. The parent declares `#[cfg(test)] mod tests;`
  and drops to 171 lines.
- **The lint expectation rides along**, as in the MCP-handler lift: the
  `#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]`
  between `#[cfg(test)]` and `mod tests {` moves onto the `mod tests;`
  declaration unchanged.
- **No import rewiring**, for the same reason as the six lifts before it: an
  inline `mod tests` and a `tests.rs` file are the same module path, so the
  block's `use super::*;` and its two other imports still resolve as they did.

This file is session state that outlives the process, so it is worth naming what
stays pinned: the production half is byte-identical, the `#[cfg(test)]` gating is
unchanged, and the module path is unchanged, so every load, save and resume test
still runs under the same name.

The moved code is unchanged apart from the four-column de-indent that un-nesting
forces — 264 body lines out, 264 in — and the child opens with a four-line `//!`
header that exists nowhere in the parent.

No `ENRICH_VER` bump: nothing here changes index output.

## [0.148.19] — 2026-08-05 — refactor: lift the shadow-writer tests out of `shadow_writer.rs`

### Changed

`crates/forgeql-core/src/storage/columnar/shadow_writer.rs` was 710 lines, and
277 of them were an inline `#[cfg(test)] mod tests` block.

- The block moves to
  `crates/forgeql-core/src/storage/columnar/shadow_writer/tests.rs` — five
  `#[test]` functions plus the `make_table` and `identity_hash` fixtures,
  covering that an empty table writes no segments, that one segment is written
  per file, that enrichment fields land in the extra columns, that pre-computed
  content skips the file read, and that the manifest is written once the run
  completes. The parent declares `#[cfg(test)] mod tests;` and drops to 441
  lines.
- **The `#[allow(…)]` rides along.** This block carried a six-line
  `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic,
  clippy::items_after_statements)]` between its `#[cfg(test)]` and its
  `mod tests {`. It moves onto the `mod tests;` declaration verbatim, in the
  same file and the same position, so it covers the same code and widens
  nothing.
- **No import rewiring**, for the same reason as the five lifts before it: an
  inline `mod tests` and a `tests.rs` file are the same module path, so the
  block's `use super::*;` and its two other imports still resolve as they did.

The moved code is unchanged apart from the four-column de-indent that un-nesting
forces — 268 body lines out, 268 in — and the child opens with a four-line `//!`
header that exists nowhere in the parent.

**No `ENRICH_VER` bump, and this is the first file in the series where that is
worth spelling out.** This one does write index output: it is the shadow segment
writer. But nothing in this diff changes *what* it writes — the production half
is byte-identical and only `#[cfg(test)]` code moved.

On evidence, precisely: the golden suites are the wrong instrument for this
commit. Against a warm cache they read pre-change segments, because same content
plus same version is exactly a cache hit. That is a property of the cache, not a
claim that nothing exercises the writer — the five moved unit tests build their
segments in a tempdir, which the cache never serves, so they run honestly
against `ShadowWriter` wherever they live.

## [0.148.18] — 2026-08-05 — refactor: lift the CHANGE-transform tests out of `transforms/change.rs`

### Changed

`crates/forgeql-core/src/transforms/change.rs` was 699 lines, and 313 of them
were an inline `#[cfg(test)] mod tests` block.

- The block moves to `crates/forgeql-core/src/transforms/change/tests.rs` —
  twenty-nine `#[test]` functions and no fixtures, covering line-to-byte-range
  conversion including CRLF and mixed endings, trailing-blank absorption, the
  multi-file validation rules, `MATCHING` resolution with and without word
  boundaries, line splices and their trailing newline, glob detection, and
  `DELETE`. The parent declares `#[cfg(test)] mod tests;` and drops to 388
  lines.
- **No import rewiring**, for the same reason as the four lifts before it: an
  inline `mod tests` and a `tests.rs` file are the same module path, so the
  block's lone `use super::*;` still resolves as it did.
- The `// Tests` banner rule stays in the parent above the declaration.

The moved code is unchanged apart from the four-column de-indent that un-nesting
forces — 310 body lines out, 310 in — and the child opens with a four-line `//!`
header that exists nowhere in the parent.

No `ENRICH_VER` bump: nothing here changes index output.

## [0.148.17] — 2026-08-05 — refactor: lift the line-budget tests out of `budget.rs`

### Changed

`crates/forgeql-core/src/budget.rs` was 706 lines, and 323 of them were an
inline `#[cfg(test)] mod tests` block.

- The block moves to `crates/forgeql-core/src/budget/tests.rs` — twenty-two
  `#[test]` functions plus the `test_config` fixture, covering construction and
  the snapshot it exposes, deduction and recovery, the warning and critical
  thresholds and the status line they drive, mutation rewards, and the on-disk
  round-trip with its sweep of expired budget files. The parent declares
  `#[cfg(test)] mod tests;` and drops to 385 lines.
- **No import rewiring**, for the same reason as the three lifts before it: an
  inline `mod tests` and a `tests.rs` file are the same module path, so the
  block's lone `use super::*;` still resolves as it did.
- The `// Tests` banner rule stays in the parent above the declaration.

The moved code is unchanged apart from the four-column de-indent that un-nesting
forces — 320 body lines out, 320 in — and the child opens with a four-line
`//!` header that exists nowhere in the parent.

No `ENRICH_VER` bump: nothing here changes index output.

## [0.148.16] — 2026-08-05 — refactor: lift the query-execution tests out of `ast/query.rs`

### Changed

`crates/forgeql-core/src/ast/query.rs` was 681 lines, and 353 of them were an
inline `#[cfg(test)] mod tests` block. The file holds read-only query execution
against the symbol table — the code path behind every `FIND` — and more than
half of it was the tests for that path rather than the path itself.

- The block moves to `crates/forgeql-core/src/ast/query/tests.rs` —
  twenty-five `#[test]` functions plus the `table_with_symbols` and `file_entry`
  fixtures, covering `LIKE` and glob pattern matching, the `FIND symbols` and
  `FIND usages` lookups, and the file-grouping helpers behind directory-depth
  collapsing. The parent declares `#[cfg(test)] mod tests;` and drops to 330
  lines.
- **No import rewiring**, for the same reason as the previous two lifts: an
  inline `mod tests` and a `tests.rs` file are the same module path, so the
  block's `use super::*;` and its two `std` imports all still resolve as they
  did.
- The `// Tests` banner rule stays in the parent above the declaration.

The moved code is unchanged apart from the four-column de-indent that un-nesting
forces — 350 body lines out, 350 in — and the child opens with a three-line
`//!` header that exists nowhere in the parent.

No `ENRICH_VER` bump: nothing here changes index output.

## [0.148.15] — 2026-08-05 — refactor: lift the MCP handler tests out of `mcp.rs`

### Changed

`crates/forgeql/src/mcp.rs` was 1,004 lines, and 406 of them were an inline
`#[cfg(test)] mod tests` block — eleven tests and four fixtures sitting under
the handler they exercise. The file is the first thing a contributor reads to
learn how a `tools/call` becomes an engine invocation, so the 40% of it that
was test scaffolding was 40% of the wrong thing.

- The block moves to `crates/forgeql/src/mcp/tests.rs` — eight `#[tokio::test]`
  functions, three `#[test]` functions, and the `make_registry`, `fixtures_dir`,
  `mcp_with_session` and `first_text` fixtures. The parent declares
  `#[cfg(test)] mod tests;` and drops to 599 lines.
- **The lint expectation rides along.** This block carried
  `#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]`
  between its `#[cfg(test)]` and its `mod tests {`. Attributes on an out-of-line
  `mod tests;` declaration apply to the file it names, so the expectation moves
  onto the declaration unchanged and stays fulfilled — no `#![expect(…)]` inner
  attribute in the child, and no allowance widened. Both forms work; the other
  out-of-line test module in the tree, `ast/index/tests.rs`, uses the inner one.
- **No import rewiring**, for the same reason as the previous lift: an inline
  `mod tests` and a `tests.rs` file are the same module path, so the block's
  `use super::*;` and its three `std` imports all still resolve as they did.

As before the moved code is unchanged apart from the four-column de-indent that
un-nesting forces — 404 body lines out, 404 in — and the child opens with a
three-line `//!` header that exists nowhere in the parent.

No `ENRICH_VER` bump: nothing here changes index output.

## [0.148.14] — 2026-08-05 — refactor: lift the worktree tests out of `git/worktree.rs`

### Changed — first of a series of inline-test lifts

`crates/forgeql-core/src/git/worktree.rs` was 1,048 lines, and 516 of them — 49%
of the file — were an inline `#[cfg(test)] mod tests` block. Someone opening the
file to learn how a session's worktree is created and torn down had to scroll
past eighteen tests and two fixture helpers to reach the thirteen functions that
implement it.

- The block moves to `crates/forgeql-core/src/git/worktree/tests.rs` — eighteen
  `#[test]` functions plus the `make_bare_repo` and `default_branch` fixtures.
  The parent declares `#[cfg(test)] mod tests;` and drops to 531 lines, all of
  them worktree logic.
- **No import rewiring at all.** An inline `mod tests` and a `tests.rs` file are
  the same module path — `crate::git::worktree::tests` either way — so the
  `use super::*;` the block already carried still resolves to the same scope,
  and so do its two other imports.
- The `// Tests` banner rule stays in the parent above the declaration, which is
  still exactly what it heads.

**Not byte-identical, but the difference is only whitespace.** Un-nesting a
`mod tests` de-indents every line of the block by four columns; that de-indent,
and any re-wrapping rustfmt derives from the freed width, are the only edits to
the moved code. No assertion, name, literal or call differs. The child does open
with something the parent never held — a two-line `//!` module-doc header, and
the blank line under it — and that is the whole of the addition: 516 body lines
left the parent and 516 arrived in the child.

Thirteen further source files carry inline test blocks of 200 lines or more,
several of them a larger share of the file than this one. They follow one at a
time.

## [0.148.13] — 2026-07-27 — refactor: lift `Overlay`'s unit tests out of `overlay.rs`

### Changed — first of four cuts on `storage/columnar/overlay.rs`

`crates/forgeql-core/src/storage/columnar/overlay.rs` is 1,119 lines, most of it
a single 833-line `impl Overlay`. This cut takes the one part that needs no
surgery on that impl.

- The inline `#[cfg(test)] mod tests` — 122 lines, four tests and one helper —
  moved to `storage/columnar/overlay/tests.rs`; the parent now declares
  `#[cfg(test)] mod tests;` and drops to 996 lines.
- **No import rewiring at all.** The module path does not change: it was
  `columnar::overlay::tests` as an inline module and it is
  `columnar::overlay::tests` as a file, so the `use super::*;` the block already
  carried still resolves to the same `overlay` scope, and so do its three other
  imports.
- The `// Unit tests` box-rule banner stays in the parent above the
  declaration, which is still exactly what it heads.

**This cut is not byte-identical, but the difference is only whitespace.**
Un-nesting a `mod tests` de-indents every line of the block by four columns.
That is the whole of it here: 122 body lines before, 122 after, so rustfmt
found nothing the freed columns let it re-join, and no assertion, name, literal
or call differs. On a block with longer expressions the de-indent can also
trigger re-wrapping; on this one it did not.

The three remaining cuts split `impl Overlay` itself, and their contract is
narrower rather than looser: the moved *method bodies* must be byte-identical,
while each child necessarily adds lines that exist nowhere in the parent — a
`//!` header, its own `impl Overlay { … }` wrapper, and its own `use` lines.
Imports do not travel the way they did here: `mod tests` rode on
`use super::*;`, whereas a non-test child must name what it needs, and the
parent will in turn lose imports that only the moved methods used. So the
parent changes textually too, and only the bodies are the invariant.

No `ENRICH_VER` bump, and the reason here is simpler than usual: nothing in this
diff changes what is written into a segment. `Overlay` is the segment *read*
path, and the only non-test line that changed is a `mod` declaration.

## [0.148.12] — 2026-07-27 — refactor: parser mutation tests out, `tests.rs` becomes a table of contents

### Changed — cut seven of seven on `parser/tests.rs`

The last nineteen tests moved into `parser/tests/mutations.rs` as one range: the
`CHANGE` command, heredoc-payload, `CHANGE FILE … LINES n-m NOTHING` regression
and `UNDO` sections, all four of their banner comments with them. Two rejection
tests came along, and the new file's header says why: what
`parse_top_level_rename_is_rejected` and `parse_error_missing_quote` reject is a
mutation — the top-level `RENAME` removed in v0.10.0, and an unterminated quote
in a `CHANGE FILE`.

`parser/tests.rs` is now **15 lines**: a module header and seven `mod`
declarations. It holds no tests, no imports and no `use super::*;` — the last
was deleted because nothing in the file names anything any more.

### The arc, end to end

`crates/forgeql-core/src/parser/tests.rs` was 1,340 lines and 91 `#[test]`
functions in one flat `#[cfg(test)]` module with no helpers, ordered by when
each test was written rather than by what it covers. Seven cuts later:

| file | tests |
|---|---|
| `tests/mutations.rs` | 19 |
| `tests/sources.rs` | 17 |
| `tests/clauses.rs` | 15 |
| `tests/show.rs` | 15 |
| `tests/find.rs` | 13 |
| `tests/backends.rs` | 8 |
| `tests/transactions.rs` | 4 |

91 before, 91 after, same names, every body byte-identical. Six of the original
1,340 lines did not move: the two top-level `use` statements, deleted because
nothing in the parent names anything any more, and four duplicate `#[test]`
attributes discarded where a moved range carried the following test's attribute
along with it.

Eighty-three lines were written rather than moved, all of them scaffolding
rather than test content: forty-five `//!` lines making up eight module headers
(the seven children and the parent), eleven `use` lines, seven `mod`
declarations, sixteen blank separators, three section banners — two new, for
groups that would otherwise have landed under a heading that did not describe
them, and one rename of a banner whose section got split across cuts — and one
`#[test]` attribute retyped to re-head a range whose own attribute stayed
behind.

Because the file was ordered chronologically, most families were not
contiguous — the seven children came out of eighteen separate line ranges. Each
cut moved its ranges in descending order, anchored at a fixed node in the child
so they stacked back into source order, which is why no test changed position
relative to its neighbours.

No `ENRICH_VER` bump on any cut in the arc. A segment is keyed by path, content
id and enrichment version, so an edited or new file re-indexes on its own; a
bump exists to invalidate segments whose content did *not* change, and no
indexing code changed here.

## [0.148.11] — 2026-07-27 — refactor: parser FIND tests into their own file

### Changed — cut six of seven on `parser/tests.rs`

- Thirteen tests moved into `parser/tests/find.rs` in two ranges: the four
  `FIND symbols` / `FIND usages` tests near the top of the file, and one
  contiguous run from `parse_find_files_no_in` to the end — five `FIND files`
  tests, the `── FIND globals / ORDER BY / LIMIT ──` block, and
  `parse_find_unknown_target_is_error` under its `-- missing error path tests --`
  banner. Both banners moved with the tests they head.
- `parser/tests.rs` drops from 481 to 269 lines and gains `mod find;`; the new
  file is 221 lines and takes `CompareOp`, `PredicateValue` and
  `SortDirection` from `crate::ir`.
- **The parent's import line narrows to `use crate::ir::ChangeTarget;`.** With
  the `FIND` tests gone, the other three names have no user left in it. That is
  forced by `-D warnings`, not a tidy-up.
- The `── UNDO / FIND files ──` banner, renamed one cut ago, is renamed again to
  `── UNDO ──`: the `FIND files` half has now left too, and what remains under
  it is the single `parse_undo_last_n`. It should travel unchanged when the
  mutation tests are cut next.

91 tests before, 91 after, same names, bodies byte-identical.

No `ENRICH_VER` bump. A segment is keyed by path, content id and enrichment
version, so the files this cut touches re-index on their own; a bump exists to
invalidate segments whose content did *not* change, and no indexing code
changed here.

## [0.148.10] — 2026-07-27 — refactor: parser code-exposure tests into their own file

### Changed — cut five of seven on `parser/tests.rs`

- Fifteen tests moved into `parser/tests/show.rs`, in four ranges: the
  `SHOW commands (Code Exposure API)` block (`context`, `signature`, `outline`,
  `members`, `body`, `callees` — eight tests), two later runs of `SHOW MORE`
  and `SHOW LINES` tests, and `parse_show_body_bare_name` with the
  `── Relaxed quoting (bare symbol names) ──` banner it was given in the
  previous cut.
- `parser/tests.rs` drops from 708 to 481 lines and gains `mod show;`; the new
  file is 236 lines. It needs no file-level `crate::ir` import — the three
  `SHOW MORE` tests that touch `ShowMoreWindow` either name it fully qualified
  or import it inside the function body, exactly as they did in the parent.
  All four names the parent imports still have users there.
- The `SHOW commands (Code Exposure API)` banner heads everything in the child
  up to the relaxed-quoting banner, `SHOW MORE` and `SHOW LINES` included, so
  no new banner had to be written for them.
- One banner was renamed rather than moved. `── SHOW LINES / FIND files ──`
  named two groups this cut separates: the `SHOW LINES` tests leave, the
  `FIND files` tests stay. Left alone it would have been wrong on both halves —
  no `SHOW LINES` test remains under it, and its first member is now
  `parse_undo_last_n`, which was never a `SHOW LINES` test either. It reads
  `── UNDO / FIND files ──` in the parent from here on. That is the one line in
  this cut that is written rather than moved; it is a line that stayed put, so
  the byte-identity of the moved payload is untouched.

91 tests before, 91 after, same names, bodies byte-identical.

No `ENRICH_VER` bump. A segment is keyed by path, content id and enrichment
version, so the files this cut touches re-index on their own; a bump exists to
invalidate segments whose content did *not* change, and no indexing code
changed here.

## [0.148.9] — 2026-07-26 — refactor: parser source-and-session tests into their own file

### Changed — cut four of seven on `parser/tests.rs`

- Seventeen tests moved into `parser/tests/sources.rs`, in five ranges: the
  original `CREATE SOURCE` / `REFRESH SOURCE` / `VACUUM` / `USE` /
  `SHOW SOURCES|BRANCHES|VERSION|STATS` run (twelve tests), two later `USE`
  tests, `parse_use_source_hyphenated_branch`, `parse_use_source_bare_as_branch`
  and `parse_use_missing_dot_is_error`.
- `parser/tests.rs` drops from 954 to 708 lines and gains `mod sources;`; the
  new file is 258 lines. It needs nothing from `crate::ir`, and all four names
  the parent imports still have users there.
- Two banners were split rather than moved whole, because each headed a run
  that no longer belongs to one file:
  - `── Relaxed quoting (double-quoted and bare values) ──` went with the two
    `USE` quoting tests it mostly covers. The third test under it,
    `parse_show_body_bare_name`, stays in the parent for the code-exposure
    cut, and would otherwise have been left inside the
    `── FIND globals / ORDER BY / LIMIT ──` section above it — a `SHOW body`
    test under a `FIND` heading. It gets a
    `── Relaxed quoting (bare symbol names) ──` line of its own, which will
    travel with it in the next cut.
  - `-- missing error path tests --` stayed in the parent, where it still
    heads `parse_find_unknown_target_is_error`.
    `parse_use_missing_dot_is_error` moved out from under it, and sits in
    `sources.rs` directly after the other `USE` tests rather than at the end
    of the file, so that it does not land under the relaxed-quoting banner
    there — which would not describe it either.
- The `// (parse_disconnect test removed — DISCONNECT command eliminated)`
  note travels with the source tests it was appended to. The
  `// SHOW commands (Code Exposure API)` banner directly below it does not —
  it heads the next cut's material and stays.

91 tests before, 91 after, same names, bodies byte-identical.

No `ENRICH_VER` bump. A segment is keyed by path, content id and enrichment
version, so the files this cut touches re-index on their own; a bump exists to
invalidate segments whose content did *not* change, and no indexing code
changed here.

## [0.148.8] — 2026-07-26 — refactor: parser WHERE-predicate tests into their own file

### Changed — cut three of seven on `parser/tests.rs`

- Fifteen tests moved into `parser/tests/clauses.rs`, in four ranges: the
  `── WHERE predicates ──` block (three tests), the
  `── Comparison operators in WHERE ──` block together with the
  `── Negative number literals in predicates ──` subsection nested inside it
  (seven), two relaxed-quoting value tests, and the
  `-- comparison operator round-trips --` block (three). All four section
  banners travel with the tests they head.
- The first of those was going to go to a `find.rs`. The file's own banner
  says otherwise and it is right: `parse_find_symbols_usages_n`,
  `parse_find_symbols_where_name_like` and
  `parse_find_symbols_where_name_not_like` use `FIND symbols` only as a
  carrier — what each one asserts is the predicate the clause parser built.
- `parser/tests.rs` drops from 1,173 to 954 lines and gains `mod clauses;`;
  the new file is 232 lines. It imports `CompareOp`, `PredicateValue` and
  `SortDirection` from `crate::ir`, and all three still have users left in the
  parent, so the parent's import line is unchanged.

One banner stayed behind deliberately. `── Relaxed quoting (double-quoted and
bare values) ──` heads a run of four tests that mixes three concerns: two `USE`
tests on branch-name quoting, two bare-or-quoted `WHERE` values, and
`SHOW body` with an unquoted name. Only the `WHERE` pair belongs in
`clauses.rs`, so the banner stays in the parent, where it still describes the
three tests left under it. The pair arrives in the child under a new
`── Relaxed quoting in predicate values ──` banner of its own — the one line in
this cut that is written rather than moved. Without it those two tests would
have landed under `── Negative number literals in predicates ──`, which does
not describe them.

91 tests before, 91 after, same names, bodies byte-identical.

No `ENRICH_VER` bump. A segment is keyed by path, content id and enrichment
version, so the files this cut touches re-index on their own; a bump exists to
invalidate segments whose content did *not* change, and no indexing code
changed here.

## [0.148.7] — 2026-07-26 — refactor: parser transaction tests into their own file

### Changed — cut two of seven on `parser/tests.rs`

- Four tests moved into `parser/tests/transactions.rs`: `parse_transaction`,
  `parse_commit_standalone` and `parse_checkpoint_sequence`, which form one
  contiguous run, plus `parse_job_commands`, which the file kept some 640
  lines further down because it was appended much later. `JOB START` can
  stand in for a synchronous `VERIFY` on a commit-gate step, so the `JOB`
  verbs read closest to the transaction tests — that is a proximity argument,
  which is all the grouping needs. Two further transaction-adjacent tests stay
  in the parent with the statement they are really about:
  `parse_commit_message_heredoc_with_apostrophes` is a heredoc-quoting test
  and `parse_change_in_transaction_sequence` is a `CHANGE` test.
- `parser/tests.rs` drops from 1,236 to 1,173 lines and gains
  `mod transactions;`. The child needs nothing from `crate::ir` — every name
  the four bodies reference arrives through `use crate::parser::*;`.

91 tests before, 91 after, same names, bodies byte-identical.

No `ENRICH_VER` bump. A segment is keyed by path, content id and enrichment
version, so the two files this cut touches re-index on their own — their
content ids moved, and nothing cached for them can be served. What a bump
exists for is invalidating segments whose content did *not* change; no
indexing code changed here, so every other file's cached segment is still
correct.

## [0.148.6] — 2026-07-26 — refactor: start splitting the parser test suite

### Changed — the `USING` clause tests get their own file

`crates/forgeql-core/src/parser/tests.rs` was 1,340 lines and 91 tests in one
flat `#[cfg(test)]` module, grown by appending rather than organised by what
each test covers. This is the first of seven cuts turning it into a parent
over themed children.

- The `USING` clause block — eight tests covering backend routing, from
  `parse_find_symbols_no_using_defaults_to_default_backend` to
  `change_file_has_no_using_clause`, together with the banner comment that
  already fenced them off — moved verbatim into `parser/tests/backends.rs`.
  It went first because it is the only themed run that never recurs later in
  the file; each of the remaining six clusters is spread over two to four
  separate line ranges.
- `parser/tests.rs` drops to 1,236 lines and gains `mod backends;`. Its
  `use crate::ir::{…}` line loses `Backend`, which no longer has a user
  there; the child carries its own `use crate::ir::Backend;`, since
  `parser/mod.rs` does not import that name and so cannot supply it.
- Everything else the child needs arrives through `use crate::parser::*;`
  where the parent used `use super::*;`. That is the same set of names read
  from one level deeper: a module can see an ancestor's private items, and
  the glob re-exposes the ancestor's own private `use` bindings too, which is
  what carries `ForgeQLIR` and `parse` into scope.

91 tests before, 91 after, same names, bodies byte-identical.

No `ENRICH_VER` bump — but not because these are tests. The indexer has no
notion of `#[cfg(test)]`; it indexes this file like any other source, and this
change does alter what gets stored for it: a new file's rows appear and the
parent's ordinals shift. The reason no bump is owed is that no *indexing* code
changed, so a segment cached under the current version is merely older than
the tree, never wrong about it.

## [0.148.5] — 2026-07-26 — refactor: the ordinal tests move next to the ordinal code

### Changed — `file_indexer.rs` no longer carries the remapper's unit tests

- The `identical_sibling_tombstone_tests` module moved from
  `ast/index/file_indexer.rs` into `ast/index/file_indexer/ordinals.rs`. Its
  nine items — the `twin_hint` / `twin_key` / `fn_hint` / `child_hint` /
  `fn_key` builders and four `#[test]` functions — construct and call nothing
  but `ordinals.rs`'s own types, so they belong beside the code they cover.
  `file_indexer.rs` drops from 340 to 218 lines: the per-file entry surface —
  macro collection, `IndexContext`, `is_addressable_fql_kind`, `index_file`,
  `index_file_from_source` — plus the submodule declarations and the
  `pub use ordinals::{…}` re-export that five other modules reach those types
  by.

- **`OrdinalRemapper::assign` is private again.** It was widened to
  `pub(super)` in an earlier step for one reason only: the test module sat in
  the parent and had to reach down into `ordinals`. With the tests inside
  `ordinals.rs` that reason is gone — the only remaining callers are
  `assign_ordinal`, in the same file, and the tests themselves. Narrowing it
  in the same commit as the move is what keeps a temporary widening from
  becoming permanent by accident.

- Otherwise pure motion. The moved module is byte-identical apart from a
  single deleted line: `use super::ordinals::OrdinalMatchKey;` is gone,
  because from inside `ordinals.rs` that path no longer resolves and no
  longer needs to — `OrdinalMatchKey` is a sibling now and arrives through
  the `use super::*;` that the module already had. Four tests before, four
  after, same names.


## [0.148.4] — 2026-08-05 — refactor: `exec_source.rs` becomes a table of contents

### Changed — third and last cut on `engine/exec_source.rs`

- `create_source`, `refresh_source`, `vacuum_report` and `vacuum` moved into
  `engine/exec_source/admin.rs`, 359 lines. `exec_source.rs` is now **15
  lines**: a module header and three `mod` declarations. It holds no code, no
  imports and no `impl` block.
- `create_source`, `refresh_source` and `vacuum` take
  `pub(in crate::engine)`. `vacuum_report` keeps plain `pub` — it is `VACUUM`'s
  shared implementation and the CLI calls it directly, and it needs no
  re-export to stay reachable: an inherent `pub fn` resolves through the type
  even from inside a private module.
- `create_source`'s signature had to be rewrapped. At 91 columns it fitted;
  `pub(in crate::engine)` is eleven characters longer than `pub(super)`, which
  pushed it to 102 against `max_width = 100`. `clippy` does not check width, so
  this is invisible until `fmt check`.

**Five moved lines are not byte-identical, and the reason is worth recording.**
`create_source` and `refresh_source` reached the warm-up machinery as
`super::warm::…`. In `exec_source.rs` `super` was `engine`, so that resolved;
in `exec_source/admin.rs` `super` is `exec_source`, which has no `warm`, and
the crate stops compiling. The five sites now say `warm::…` against a new
`use crate::engine::{…, warm}`. A bare `super::`-relative path is load-bearing
on the file it sits in — importing the item instead makes the call site
independent of how deep the file is nested, which is why the other two cuts
needed no such repair.

### Fixed

- The 0.148.3 entry, as published, said `exec_source.rs` dropped to 358 lines;
  it was 357. The count was written before a stray blank line was removed from
  that same commit. The entry above is corrected in this one.

With all three cuts landed, the 969-line original is four files: `admin.rs`
359, `attach.rs` 440, `readouts.rs` 226, and a 15-line parent naming them.

No `ENRICH_VER` bump: nothing in this diff changes index output — no enricher,
kind map, ordinal assignment or emit path is touched.

## [0.148.3] — 2026-08-05 — refactor: split the `USE` pipeline out of `exec_source.rs`

### Changed — second of three cuts on `engine/exec_source.rs`

- `use_source` and its five private steps — `configure_columnar_build`,
  `restore_session_on_reconnect`, `finalize_use_source`, `try_resume_session`
  and `load_session_index` — moved into `engine/exec_source/attach.rs`, 440
  lines. `exec_source.rs` drops from 774 to 357, and its module header now
  says what is left: the source admin verbs.
- **Only one signature changed.** `use_source` was the cluster's sole
  `pub(super)` method and becomes `pub(in crate::engine)` for the same reason
  as the previous cut. The other five carry no visibility keyword at all: each
  has exactly two references in the file — its definition and a single call
  site inside `use_source` — so the whole cluster is self-contained and moves
  with the private helpers still private, now to `exec_source::attach` instead
  of `exec_source`.
- Imports again named by the compiler rather than guessed. The parent loses
  `git_blob_sha1`, `self as git`, `worktree` and the entire
  `session::{Session, SessionCoords, liveness::WorktreeClaim}` group — every
  one of them belonged to the `USE` path. The child drops `source::Source`,
  `QueryResult` and `SymbolMatch`, which belong to the admin verbs and the
  readouts.

One thing that looks like a break and is not: the golden probe
`rust_documented_fns_that_still_have_unused_params` in `probes_rust.json` pins
`try_resume_session` at `crates/forgeql-core/src/engine/exec_source.rs`, line
352 — a path this commit invalidates. The probe runs against
`forgeql-pub.frozen`, a snapshot branch, so it reads the pre-split tree and is
unaffected. It will need repinning whenever that snapshot is next refreshed.

Every method body is byte-identical. Beyond the moved bytes the change is one
visibility keyword, the two import blocks, the parent's module header, the new
eight-line header on `attach.rs`, and one `mod attach;`. No signature crossed
`max_width` this time — `use_source`'s was already in the multi-line form.

No `ENRICH_VER` bump — but not because "nothing here writes a segment", which
would be false of this cluster in particular: `configure_columnar_build`
installs the `ShadowWriter` context that writes segments, and
`load_session_index` calls `ColumnarStorage::warm_or_open`, which builds and
writes the overlay. The reason is narrower and is the one that matters: nothing
in this diff changes index *output*. No enricher, kind map, ordinal assignment
or emit path is touched, so a segment written after this commit is
byte-identical to one written before it.

## [0.148.2] — 2026-08-05 — refactor: split the session readouts out of `exec_source.rs`

### Changed — first of three cuts on `engine/exec_source.rs`

`crates/forgeql-core/src/engine/exec_source.rs` was 969 lines, all of it a
single `impl ForgeQLEngine` block holding three unrelated concerns: the source
admin verbs, the `USE` attach pipeline, and the session readouts. They are
contiguous and do not interleave, so each comes out as one range.

- `show_sources`, `show_branches`, `exec_show_commits`, `show_version` and
  `show_stats` moved into `engine/exec_source/readouts.rs`, which is 226 lines.
  `exec_source.rs` goes 969 → 774: 767 after the move, plus a seven-line
  module header it had never had — the file used to open directly on
  `use std::sync::Arc;`.
- **Every moved signature changed `pub(super)` to `pub(in crate::engine)`.**
  Forced, not cosmetic: in `exec_source.rs` `pub(super)` expands to *visible in
  `crate::engine`*, which is where the dispatcher calls these from. One level
  deeper the same keyword means *visible in `crate::engine::exec_source`*, and
  every call site stops compiling. `pub(in crate::engine)` is the literal old
  expansion — not `pub(crate)`, which would widen the reach and trip
  `clippy::redundant_pub_crate` inside a private child module.
- No re-exports are needed. Inherent methods resolve through the type rather
  than the module path, so `ForgeQLEngine`'s methods stay reachable from
  `engine` wherever they sit — the same reason the neighbouring
  `exec_change.rs` is nothing but `mod` declarations. The child reaches back up
  the way that module already does, `use crate::engine::{…}` rather than
  `super::super::{…}`: an absolute path does not depend on how deep the file
  happens to sit.
- Imports were moved by letting the compiler name them, not by guessing: the
  parent loses `SessionStats`, `ShowContent` and `require_session_id`; the child
  takes only `Result`, the six `crate::result` types, `ForgeQLEngine` and
  `require_session_id`. Both prunings are forced by `-D unused_imports`.

Every method *body* is byte-identical. The changed lines are the five
visibility keywords — one of which pushes `show_branches`'s signature past
`max_width = 100`, so it is rewrapped into the four-line form rustfmt would
produce anyway — the two import blocks, the two new module headers, and one
`mod readouts;`. Nothing else in the 204 moved lines differs.

No `ENRICH_VER` bump: this is the engine's execution layer, and nothing here
writes a segment.

## [0.148.1] — 2026-08-04 — chore: delete the retired legacy golden runner

### Removed

- `crates/forgeql/tests/zephyr_golden.rs` and its emptied
  `crates/forgeql/tests/golden.json` are deleted. The runner had been retired
  in the previous release — every case now lives in the data-driven suites
  under `crates/forgeql/tests/golden/` or in the raw-text engine test that
  owns its own corpus — and the runner was kept only so the pre-commit gate's
  legacy phase still had a target. That gate phase is gone, so the runner
  goes with it. Comments that pointed at the old runner now point at the
  surviving suites.

## [0.148.0] — 2026-08-04 — test: retire the legacy golden runner, and pin the CHANGE FILE contract

### Added

- **`CHANGE FILE` contract pinned in the golden suites.** A new
  `change_file_contract` suite asserts both halves of the rule agents meet:
  raw-text `CHANGE FILE` is refused on an indexed source file, naming the
  refusal, and leaves the file byte-identical; and it still works on a
  non-indexed file, which it can also create.
- **`error_contains` assertion.** `error: true` alone accepts *any* failure,
  including a typo in the query — it records that a step failed, not why. Steps
  may now pair it with `error_contains: "<substring>"` to pin one specific
  refusal. Without this, a case written to pin a refusal passes whenever the
  query is merely malformed.
- **`raw_text_chains` integration test.** The raw-text verbs are covered end to
  end on a corpus the test builds itself: a multi-file sweep, a line-range
  splice, `COPY LINES`, `MOVE LINES`, nested transactions where the inner
  rollback keeps the outer's edit, and a rollback that removes a file the
  transaction created. It opens with a control proving `CHANGE FILE` is refused
  before the override is enabled, so the rest cannot pass for another reason.

### Changed

- **The legacy golden runner is retired**; `tests/golden.json` is now an empty
  case list. Everything it covered lives in the data-driven suites under
  `tests/golden/`, which assert shapes rather than exact values and so survive
  corpus drift, or — for the raw-text chains that had no equivalent there — in
  `raw_text_chains`. The runner and the file remain so the pre-commit gate's
  legacy phase still has a target to invoke; with no cases it passes without
  touching a corpus.
- **Golden pool engines no longer inherit `FORGEQL_ALLOW_CHANGE_FILE_INDEXED`.**
  The pre-commit gate exports it for the legacy phase, so the suites saw a
  different `CHANGE FILE` contract under the gate than standalone, and could
  never pin the refusal.

### Fixed

- **`total` assertions could never be satisfied for `FIND files`.** The runner
  read `total` only from the top level of a result, where `FIND symbols` reports
  it; `FIND files` reports it inside `content`, so the assertion silently
  resolved to 0 rather than failing loudly. It now falls back to the nested
  field. The two verbs also differ in meaning — `FIND files` counts every match,
  `FIND symbols` reports the count capped by `LIMIT` — and both are now
  documented and pinned.

## [0.147.5] — 2026-08-04 — test: pin directory depth, heredoc operands and ascending order

### Added

`tests/golden/depth_and_heredoc_forms.json` — four cases over DSL surfaces no
other suite reached.

- `FIND files DEPTH 1` lists **directories**, with the aggregate size of
  everything beneath each one, not files. That shape was unpinned.
- `WHERE depth =` is a predicate in its own right, composable with `extension`
  and an `IN` glob.
- `ORDER BY size ASC` selects the smallest files, mirroring the descending form
  the other suites use.
- A heredoc operand returns exactly what the quoted literal returns, asserted
  for both a scalar (`WHERE name = <<SYM … SYM`) and a glob
  (`IN <<GLOB … GLOB`), each paired with its quoted equivalent in the same
  case. The glob case additionally places `LIMIT` before `IN`, pinning that
  clause order is not positional.

## [0.147.4] — 2026-08-04 — test: pin that enrichment follows a mutation and an UNDO

### Added

`tests/golden/enrichment_refresh.json` — one multi-step read-write case
asserting that a symbol's enrichment tracks the source through a
`CHANGE NODE` and back through an `UNDO`.

Enrichment fields are not row keys, so each value is observed through the
predicate that selects on it, and every step asserts both directions: after the
edit the new `param_count` matches and `has_todo` is true, **and** the old
`param_count` stops matching. Asserting only the new value would pass even if
the mutated symbol had merely been added alongside a stale one, which is the
failure this case exists to catch.

### Note

Zero and false enrichment values are not indexed: `param_count = 0` and
`has_todo = 'false'` match nothing at all, rather than matching the functions
that take no parameters or carry no marker. The first draft of this case used a
zero-parameter function as its baseline and could not even capture a handle.
The case now starts from a function with `param_count = 2`, so that the "old
value stops matching" half of each assertion is meaningful.

## [0.147.3] — 2026-08-04 — test: pin the FIND files filters and the heredoc regex form

### Added

`tests/golden/find_files_filters.json` — four cases. `clause_pipeline` covers
`ORDER BY size DESC`; these cover the filters deciding which files get ordered
at all, plus the heredoc regex form.

- `WHERE size >` is a numeric comparison over every file, not only source.
- `WHERE path =` resolves exactly one file.
- `WHERE extension =` and an `IN` glob narrow together.
- `MATCHES <<RE … RE` is asserted to return exactly what the inline
  single-quoted regex returns, in one case, so the two forms cannot drift
  apart silently.

### Note

A `FIND files` response carries no `total` field. The count the CSV renderer
prints beside `find_files` is not present in the JSON, so an assertion on
`total` reads 0 regardless of how many files matched. These cases assert
`row_count` and the rows instead. Whether `total` should be populated there is
left open rather than pinned either way.

## [0.147.2] — 2026-08-04 — test: pin the SHOW read verbs

### Added

`tests/golden/show_read_verbs.json` — four cases covering the read verbs the
existing suites do not reach. `node_addressing` covers `SHOW NODE` by handle
and `structured_text` covers `SHOW outline` over JSON and YAML; nothing covered
the rest.

- `SHOW body … DEPTH 0` returns the signature only and `DEPTH 99` returns the
  whole span, asserted against each other so the progressive-disclosure
  contract is what fails if it regresses.
- `SHOW signature` returns the declaration line alone.
- `SHOW callees` lists the calls a function makes, with their lines.
- `SHOW outline` narrows by `fql_kind` and `name` together.

## [0.147.1] — 2026-08-04 — test: pin the name and path predicate operators

### Added

`tests/golden/name_and_path_predicates.json` — four cases covering the operator
surface on `name` and `path`, which `enrich_naming` did not reach: it pins the
`naming` values and `name_length` equality, not the operators.

- `name LIKE` is a prefix wildcard and is **case-insensitive** — `'k_sem_%'`
  returns `K_SEM_DEFINE` alongside `k_sem_init`. That behaviour was unpinned.
- `name MATCHES` is a regex and honours a `^` anchor.
- `name_length` upper and lower bounds select the expected extremes.
- `WHERE path =` is an exact single-file scope, returning the same rows as the
  equivalent `IN` form, and narrows to nothing when combined with a predicate
  the file does not satisfy.

An enrichment field used as a predicate does not come back as a row key, so
these cases assert the selection a bound produces rather than the field value.
Asserting the field directly derives `null` and the check passes without
testing anything.

## [0.147.0] — 2026-08-04 — test: clause-pipeline semantics move into a data-driven golden suite

### Added

`tests/golden/clause_pipeline.json` — nine cases covering how the clause
pipeline composes on the frozen Zephyr corpus: repeated `WHERE` against its
`AND` synonym, exact `name` matching, `ORDER BY` ascending and descending,
`OFFSET` windowing, `IN` path globs, `GROUP BY` with `HAVING` and
`ORDER BY count`, `FIND files ORDER BY size`, and `SHOW LINES` narrowed first
by range, then `LIKE`, then a regex.

These were previously covered only by the older non-data-driven golden runner,
one query per case. Each case here stacks the clauses it is about and asserts
the shape the clause guarantees — `ordered`, `all_same`, `row_count` — keeping
exact row pins for the values that are contractual rather than incidental. The
older runner is unchanged and still covers them; this suite does not replace it
yet.

## [0.146.0] — 2026-08-04 — perf: the data-driven golden suite runs against a pool of engines

### Changed

The `golden_test` suite's 237 cases already ran on parallel threads, but every
one of them locked a single shared harness for a whole request round-trip, so
the suite was serialised behind one engine process and the extra threads bought
nothing measurable (default threads 197.9s vs `--test-threads=1` 202.7s).

The suite now runs against a small pool of independently spawned engines (4),
with cases assigned round-robin. The per-request locking is unchanged; a case
just locks its own pool member instead of the one everybody shares. A case
stays on one member for its whole run, because a session id is only valid in
the process that created it.

Each member's aliases now carry its pool index. Without that, two members would
invent the same alias for the same corpus, land on one worktree, and have two
engine processes mutating a single checkout — which read-write cases would have
reported as unexplained failures.

Measured on the same machine, all 237 cases passing throughout. In the
pre-commit gate, which builds optimized, the suite's phase went from **185s to
95.3s**. A controlled run that changed nothing but the pool size, on warm
caches and an unoptimized build, went from **489.4s with one engine to 308.1s
with four**.

Both are real, and both are sub-linear in the pool size, because each member
independently checks out and indexes its own worktree per corpus: the setup
work is multiplied by the pool size even though the query work is divided by
it. That fixed cost is a larger share of an unoptimized run, which is why the
unoptimized number improves least.

## [0.145.0] — 2026-08-04 — fix: a second engine process no longer deletes a worktree the first is still checking out

### Fixed

Two engine processes sharing one data directory could destroy each other's
work. Every process runs a reclaim pass at startup that deletes worktrees which
look abandoned, and "looks abandoned" was decided from that process's own
in-memory session maps — which cannot contain another process's sessions. A
worktree a peer has just begun checking out is on disk, carries no session
sentinel yet, and is unknown to the sweeper: on disk it is indistinguishable
from an orphan left by a crash. The sweeper deleted it mid-checkout, and the
peer's `USE` failed with a "No such file or directory" error raised from inside
the checkout. The same race applied to git's own worktree metadata, which a
peer registers before its checkout directory appears.

Liveness is now recorded where every process can see it: an advisory lock on a
claim file beside each worktree directory.

- An owner takes a **shared** lock before `git worktree add` runs, and holds it
  for the session's whole life. Shared, so two processes attached to the same
  session both hold it without contending.
- A reclaim sweep takes an **exclusive** lock and deletes only while holding
  it. A live owner makes that fail and the sweep moves on; holding the lock
  across the teardown stops an owner from starting up into a directory that is
  being deleted.

Owners block for their lock and so never fail on contention; sweeps never
block. The OS releases both when the holding process dies, which is what a
crash needs: no lease to expire, no stale record to clean up, and the dead
owner's worktree reclaimable immediately.

A worktree directory younger than five minutes that carries no session sentinel
is also left alone, covering a peer running an older build that takes no claim.
The cost is that a genuine orphan survives one extra startup.

Claim files are never unlinked. Removing one while another process is about to
open it would hand the two processes locks on different inodes for the same
worktree — precisely the failure the claim exists to prevent. They are empty
files and cost one directory entry each.

## [0.144.1] — 2026-08-04 — perf: pre-commit gate no longer link-time-optimises test binaries

### Changed

- The `ci` build profile — used only by the pre-commit test gate — no longer
  inherits `lto = "thin"` and `codegen-units = 1` from `release`. Those
  settings re-optimised the entire dependency graph (every tree-sitter
  grammar) once per linked binary, and with ~50 separate integration-test
  binaries the gate spent minutes in pure link time while the tests
  themselves finish in seconds (relinking one trivial test binary took 69s;
  a plain link takes a few). `opt-level = 3`, debug assertions and overflow
  checks are kept, so test coverage and golden-suite runtime are unchanged,
  and shipped release builds are unaffected.

## [0.144.0] — 2026-08-03 — feat: sibling runs of includes, defines and imports are addressable blocks

### Added — block groups for C, C++, Python and Rust

A file is organized in runs the grammar refuses to parent: the includes at the
top, the table of `#define` flags under them, the banner comment above a
section, the imports opening every Python module. Each member was addressable;
the run as a whole was not. Moving an include section meant one mutation per
line, and "insert after the includes" had no anchor to name.

Runs of 2+ adjacent same-kind siblings now surface as one addressable block
row, declared entirely in per-language config:

- **C / C++** — comment runs (`comment_block`, split by `comment_style` so a
  `/*` paragraph never merges with a `//` one), `#include` runs
  (`include_block`) and `#define` runs (`macro_block`). C++ additionally
  groups typedef/`using` runs (`type_alias_block`).
- **Python** — comment runs and import runs (`import_block`); `import` and
  `from` statements share a kind, so a mixed run is one block.
- **Rust** — `use` runs (`import_block`) join the existing doc-comment group.

The blocks behave like every block before them: the block is a sibling of its
members, every member keeps its own row and handle, and a different-kind node
between two runs splits them — so blocks follow the paragraphs the author
already wrote. A run inside a preprocessor guard is scoped to its branch and
never merges with the run outside it. `MOVE NODE` on a block relocates the
whole run; `INSERT AFTER NODE` on one lands after its last member.

Local variable declarations are deliberately not grouped: a declaration run is
the most edit-churned span in any function, the hoist/sink refactor that
matters is a single-node move already, and the row cost would be paid in every
function of every corpus for a block nobody moves.

Existing `variable`/`import`/`macro` queries are unchanged — blocks add rows,
they never replace member rows.

## [0.143.0] — 2026-08-03 — feat: filter a config row by its key path

### Added — `key_path`

A key in a config file could only be found by its own name, which is often not
enough to identify it. A Zephyr west manifest contains twelve `pair` rows all
named `remote`; eleven are per-project overrides and one is the manifest
default, and nothing in a query could tell them apart.

Rows in a format that maps a `pair` kind now carry `key_path`: the dotted chain
of enclosing keys, with the row's own key appended when the row is itself a
pair. The default is `manifest.defaults.remote` and the overrides are
`manifest.projects.remote`, so one filter separates them.

```sql
FIND symbols WHERE key_path = 'manifest.defaults.remote' IN 'west.yml'
FIND symbols WHERE key_path LIKE 'jobs.%.container.image' IN '.github/workflows/**'
```

Sequence position is deliberately never encoded. A path that named a slot would
follow the slot rather than the node, so reordering two list entries would make
each answer to the other's path — the same hazard the naming rules avoid, one
field over. It also makes the aggregate query the useful one:
`jobs.clang-build.steps.uses` covers every step rather than one.

The chain is built by the indexing walk, alongside the parent-kind tracking
already maintained there, rather than by walking ancestors when a row is
written: an ancestor walk costs one step per preceding sibling, which is
quadratic across the wide arrays config files are full of. Rows outside a key
hierarchy — every row in every code language — carry no column at all.

Cached segments are rebuilt on first use. No row moves and no handle changes;
this adds a column, not a node.

## [0.142.0] — 2026-08-03 — feat: a run of YAML comments is one addressable block

### Added — `comment_block` for YAML

A banner comment — the fifteen `#` lines at the top of a Zephyr west manifest, or
any run of adjacent comments — was fifteen separate rows with no way to address
the run as a whole. YAML now declares a block group, so a run of 2+ adjacent
comments is surfaced as a single `comment_block` node, the same treatment Rust
doc-comment runs already receive.

The block is the *sibling* of its members, never their parent. Every comment
keeps its own row and its own handle, and additionally gains the block's address,
so a member can be reached either directly or through the block at
`block_id(offset)`. Nothing is hidden and no row is replaced.

Unlike Rust, YAML does not split a run by comment style: there is only one
comment syntax, so adjacent comments always form one block.

Block rows are addressable and therefore consume an ordinal, so they shift the
handles of nodes that follow them in the same file. Cached segments are rebuilt
on first use.

## [0.141.0] — 2026-08-03 — feat: comments in YAML and JSON are addressable rows

### Added — a comment in a config file is now a symbol row

A comment in a YAML or JSON file was mapped to the `comment` kind but never
produced a row. It could not be found by name, owned no handle, and carried no
mention. A note written beside a value — `# do not rename` — was invisible to
every query, while the same comment in a C or Rust file indexed normally.

The cause was the naming ladder rather than the kind map. Row emission is gated
on the language returning a name for a node, and the ladder shared by the
structured-text formats named pairs, containers and sequences only, falling
through to `None` for everything else. Comments are now named by their own raw
text, the rule the code languages already use.

Two things follow. `FIND symbols WHERE fql_kind = 'comment'` returns rows for
these formats, each carrying a handle and a rev, so a comment can be read and
edited like any other node. And because mention recording hangs off comment
nodes the walk has visited, YAML now records comment-role mentions: a name
written inside a comment answers to
`FIND usages OF '<name>' WHERE role = 'comment'`.

New comment rows shift the ordinals of nodes that follow them in the same file,
so cached segments are rebuilt on first use after upgrading.

The remaining structured-text formats are deliberately unchanged. TOML, INI and
the others carry their own name-extraction rules, and each needs its own test
coverage before its comments begin emitting rows.

## [0.140.1] — 2026-08-02 — fix: a Kconfig flag is a macro, not a variable

### Changed — `config X` indexes as `macro`

A Kconfig entry shipped as `fql_kind = 'variable'` in the previous release.
That was the wrong bucket in two visible ways.

It made build flags masquerade as C variables. On a Zephyr tree,
`FIND symbols WHERE fql_kind = 'variable' IN 'arch/arm/**' ORDER BY name ASC`
returned Kconfig flags ahead of every C variable, because flag names sort early
— so the query stopped answering the question it was asked.

And it left the flag out of a default `SHOW outline`. The outline lists
structural declarations and deliberately excludes `variable`, so that C locals
do not flood it; a Kconfig file's outline was therefore empty even though every
entry in it was indexed and findable.

`macro` is what a flag actually is: `config PM_DEVICE_RUNTIME` is the
definition site of `CONFIG_PM_DEVICE_RUNTIME`, a preprocessor macro in the
generated header. It is already a structural kind, so:

```sql
SHOW outline OF 'subsys/pm/Kconfig'
```

now lists the flags a file defines, and `variable` queries mean C variables
again.

Same rows in the same places — only the kind they are filed under changes. The
corpus total is unchanged; the counts move from one bucket to the other.

## [0.140.0] — 2026-08-02 — feat: a build flag's definition site is addressable

### Added — Kconfig files are indexed

A config flag is defined in `Kconfig` and referenced from C, CMake, YAML and
every other file in the tree. ForgeQL could find every reference and not the
definition: `Kconfig` had no grammar, so those files produced no rows at all.
Renaming a flag meant reading the definition by hand and hoping.

`Kconfig` is now a supported format. `config X` and `menuconfig X` index as
`variable` rows named `X`, so the definition site has a stable handle and turns
up in `FIND symbols` like any other declaration:

```sql
FIND symbols WHERE name = 'PM_DEVICE_RUNTIME' IN 'subsys/pm/Kconfig'
FIND usages OF 'PM_DEVICE_RUNTIME' IN 'subsys/pm/**'
```

Every flag reference in a Kconfig file is a usage site, so `depends on X`,
`select X` and `if X` all answer `FIND usages OF 'X'`. That falls out of the
grammar rather than being enumerated: each of those spells its operand as the
same `symbol` node, so one declaration covers all three.

The file name carries no extension, which the registry's existing file-name
fallback already handles — a plugin declaring `kconfig` claims a file called
`Kconfig`.

`if` is deliberately not an addressable row. Its condition is a bare symbol, so
naming the guard after it would report a flag's own name twice in the file that
defines it — once for the definition, once for the guard — and `if` nests the
entries it guards, so mapping it would also re-parent them.

Not yet: a config entry does not appear in a default `SHOW outline`. It is
indexed and `FIND symbols` finds it, but `variable` is excluded from the
outline's structural filter so that C locals do not flood every outline.
Making it structural is a decision about which kind a build flag should be,
not a matter of more indexing, and is left for a follow-up.

Known residue: files with vendor suffixes (`Kconfig.stm32`, `Kconfig.defconfig`)
are not claimed. The suffix is open-ended, so there is no list to match, and
guessing at one would claim files no grammar was written for.

## [0.139.85] — 2026-08-02 — feat: an include path is a name you can search for

### Added — substring matching for names that are not identifiers

`FIND usages OF '<name>'` has always matched whole tokens, which is right for
an identifier and useless for a path. `#include <zephyr/pm/device_runtime.h>`
names a file the same way a call names a function, but asking where that file
was included returned nothing: the path is not an identifier, and nothing in
the index held it as a searchable name.

Two changes make it searchable.

A C or C++ angle-bracket include path is now recorded as an occurrence under
its own name. It is stored as **one** token holding the whole path, not split
at each `/` and `.`, because a query naming the path has to match something a
file actually wrote. The angle brackets are trimmed rather than declared: the
recorded token is the node's text with the leading and trailing characters
outside `[A-Za-z0-9_]` removed, so no language has to tell the engine which
delimiters it uses.

And a query holding a character outside `[A-Za-z0-9_]` — a `/`, a `.`, a space —
is now matched as a **substring** of the stored tokens as well as exactly:

```sql
FIND usages OF 'zephyr/pm/device_runtime.h'   -- every file that includes it
FIND usages OF 'net/core/'                    -- every include under a directory
```

An identifier query is unchanged and still matches whole tokens only, so
`FIND usages OF 'CONFIG_X'` still does not match `CONFIG_X_ASYNC`. Substring
matching is case-sensitive, like the exact lookup it extends.

Behind it is a trigram index over the workspace's token dictionary, built on the
first substring query and reused until a `COMMIT` replaces the overlay it was
read from. The trigram tier is a prefilter, not an answer: it proposes a
superset of candidate tokens and each one is confirmed with a real substring
test before it is searched for. Sessions that only ever name identifiers never
build it.

The test cuts both ways, which is what keeps it complete. Only tokens that
themselves hold a character outside `[A-Za-z0-9_]` are searched, and a token
containing your query must hold every character your query does — so nothing
reachable is skipped. A name written only in that alphabet is matched exactly:
`FIND usages OF '256'` still means the token `256`, and never widens into
`sha256`.

Two further limits. Candidates come from code occurrences, so a name written in
a comment, string or config value is matched exact-only. And a query under three
characters is matched exactly, being shorter than the index can narrow on.

The quoted include form (`#include "local.h"`) is not claimed: its node kind is
`string_literal`, shared with every other string in the language, and one kind
carries one rule.

## [0.139.84] — 2026-08-02 — feat: a name written in a config value is an occurrence

### Added — `role = 'config'` for YAML, TOML and JSON values

A build is configured in YAML, TOML and JSON as much as in CMake, and the names
that appear there — a container image, a toolchain action, a package, a path —
are the same names that appear in code. They were invisible to `FIND usages`:
asking where a name was written skipped every config file in the repository, so
a rename looked complete while the pipeline still referred to the old name.

Scalar values in those three formats now contribute occurrences under
`role = 'config'`. Keys deliberately do not. A key is already the pair's own
name on the symbols side, so tagging it here would answer "where is this name
written?" twice for one byte range; the division is keys → `FIND symbols`,
values → `FIND usages ... WHERE role = 'config'`.

Keeping that line straight is the whole difficulty, because a value can contain
keys:

```yaml
outer:
  inner: deep
```

`inner` sits inside `outer`'s value and is still a key. YAML and JSON label the
two sides with tree-sitter fields, so a rule may name the field it applies
under, and the *nearest* label on the path wins — crossing into a value arms
the rule, crossing into a key disarms it again, however deep. Sequence entries
and block scalars carry no label of their own and so inherit, which is what
makes list values count. TOML labels nothing at all, but there its keys and its
values are different node kinds, so a plain kind rule is enough.

Only leaf scalars are mapped. A YAML value passes through two wrapper nodes and
a JSON string wraps its own content; mapping a wrapper as well would report
every token in it twice.

The object form of a rule is a named struct rather than an inline enum variant.
An untagged variant does not inherit the enclosing section's rejection of
unknown keys, so a typo inside one would have been dropped in silence — the
very failure the previous release closed.

### Added — `mention_token_extra_chars`

Config values are full of hyphenated names — `ubuntu-latest`,
`tree-sitter-yaml` — and the token alphabet stopped at `-`, so each arrived as
two unrelated tokens and neither whole name could be found. A language may now
widen the alphabet for the characters that continue a token. The start of a
token is not widened and a token never ends on a widened character, so the
extra characters can only join a name, never invent one. YAML, TOML and JSON
add `-`; every other language is unchanged.

### Note — indexes rebuild on first use

The cache version moved, so the first query against a source after upgrading
re-indexes it.
## [0.139.83] — 2026-08-02 — fix: an unrecognised key in a language config is now an error

### Fixed — language configs are deserialized strictly

Each language's grammar mapping lives in a JSON file (`config/<lang>.json`) that
is compiled into its crate and parsed the first time that language is used. The
parser accepted any key it did not recognise and dropped it. A key that was
spelled correctly but written at the wrong nesting level was therefore not a
failure of any kind: the config loaded, the language worked, and the one feature
that key was meant to switch on simply never ran. Nothing reported it — not the
build, not the tests, not the query that came back empty.

Unknown keys are now refused, and the error names the offending key alongside
the keys that were expected in its place.

Turning this on found one straight away. The C config carried `loop_kinds` and
`exception_handler_kinds` inside its `syntax` block, which does not define
either; both were already present, with identical values, in the `control_flow`
block that does. The dead copies are removed. Nothing about how C is indexed
changes, because nothing ever read them.

Two tests hold the rule itself as a pair: a config with a key one level too high
must be refused, and the same key in its proper place must still parse. A test
that checked only the refusal would pass just as happily if the config were
being rejected for some unrelated reason.

Checking the shipped configs belongs to the crates that ship them. Each language
crate now parses its own config in its own test suite, and the crate that ships
several scans its config directory instead of listing the files, so a format
added later is covered without anyone remembering to add it.
## [0.139.82] — 2026-08-02 — feat: a symbol named in documentation prose is an occurrence

### Added — `role = 'doc'` for Markdown and reStructuredText prose

Documentation is where a symbol is explained, and it goes stale the same way a
log message does. Rename a flag in the code, leave the manual alone, and the
manual is quietly wrong. Prose was invisible to `FIND usages`: a documentation
file is not code, so a name written in one resolved to nothing.

Markdown and reStructuredText prose now contributes occurrences under
`role = 'doc'`. Markdown maps paragraphs and atx headings;
reStructuredText maps paragraphs and titles.

Containers that enclose paragraphs — list items, block quotes, directives,
fields and sections — are deliberately not mapped. The prose of a list item is
already the paragraph inside it, so mapping both would report every token twice.
A setext heading is not safe: it keeps its text in a child paragraph, so mapping
it too would report every token in it twice. An atx heading keeps its text in an
inline node instead, so it cannot overlap a paragraph and is mapped. A
golden pins one reStructuredText site by exact line and a test pins one token
per Markdown container, which is what enforces the property: a nesting mistake
surfaces as two occurrences of one token rather than as a subtly wrong count.

Occurrence lines are per token, not per node. The pinned site sits on the line
where the name is written, not on the line where its paragraph begins.

### Changed — indexes rebuild on first use

The cache version moved, so the first query against a source after upgrading
re-indexes it. Large trees take a few minutes; later queries are served from the
rebuilt cache.
## [0.139.81] — 2026-08-01 — feat: a config flag named in a build file is an occurrence

### Added — `role = 'config'` for CMake call arguments

A build system decides which sources compile, and it names the flags that
decide it. `zephyr_sources_ifdef(CONFIG_PM_DEVICE_RUNTIME device_runtime.c)`
is where a config flag actually takes effect, and it was invisible: build
files are not code, so the flag's name in them resolved to nothing. Renaming a
flag and missing the build file silently drops a source from the build.

CMake call arguments now contribute occurrences under `role = 'config'`.

Which node kind carries an argument was settled by driving the grammar rather
than reading it, and the answer matters. CMake's `argument_list` is the whole
parenthesised list — including any `#` comment written inside it — so mapping
the list would have tagged comments as config. The `argument` node is the
wrapper around exactly one unquoted, quoted or bracket argument and can never
contain a comment, so that is what the config declares. A golden pins the
contract that comment text yields no `config` row.

### Changed — indexes rebuild on first use

Every CMake file gains a posting blob, so the enrichment-logic version moves
and cached indexes rebuild on the first `USE` of each source. No row, ordinal
or existing blob changes.
## [0.139.80] — 2026-08-01 — feat: a name written in a string is an occurrence too

### Added — `role = 'string'`

Some names live only in strings. A plugin registers itself by a literal, a
dispatch table keys on one, an op is labelled with one — and none of it is an
identifier the grammar resolves, so `FIND usages` reported none of it. Renaming
such a name compiled cleanly and broke at runtime.

String literals now contribute occurrences under `role = 'string'`, alongside
the `code` and `comment` roles:

```csv
"file","role","node_id","rev","[lines]"
"crates/forgeql-core/src/engine/exec_find.rs","code","n17a6f22550","hafdce2f8b…","50,50"
"crates/forgeql-core/src/engine/exec_find.rs","string","n17a6f22550","hafdce2f8b…","72"
```

Same rules as the other roles: token-exact, so a literal spelling
`CONFIG_X_ASYNC` is not a hit for `CONFIG_X`; and each occurrence reports its
own line, so a name inside a multi-line raw string comes back where it is
written. `char` literals are deliberately excluded — a single character cannot
hold a name, and a multi-character constant is not prose.

Which node kinds carry strings is declared per language, so the core still
never names a construct. C, C++, Rust and Python declare theirs today;
languages that do not simply emit no `string` occurrences.

As with comments, string occurrences are enumerated, not judged: whether a
literal means your symbol is the caller's call.

### Changed — an unfiltered rename sweep now rewrites string literals

`FIND usages` arms every occurrence it returns, so a `CHANGE NODES FOUND`
sweep driven by an unfiltered query renames the log message and the doc
comment along with the code. This is usually what a rename wants — a function
whose own log line still prints the old name is not renamed — but it is a real
change from when strings were invisible to the query.

The narrowing is one clause: `FIND usages OF 'x' WHERE role = 'code'` arms only
what the compiler resolves, restoring the previous reach exactly. Both halves
are pinned by tests so the difference cannot drift.

### Changed — indexes rebuild on first use

Every file holding a string gains a posting blob, so the enrichment-logic
version moves and cached indexes rebuild on the first `USE` of each source.
Nothing existing changes — no row, no ordinal, no other blob — so `FIND
symbols` and every node handle answer exactly as before.
## [0.139.79] — 2026-08-01 — feat: comments are part of a name's blast radius

### Added — `FIND usages` returns typed occurrences, not just resolved references

A rename is not finished when the code compiles. The same name is written in
comment text — trailing `#endif` markers, doc blocks explaining a flag, notes
naming a function — and none of it was reachable. `FIND usages` reported only
what the grammar resolved as an identifier, so an agent renaming a symbol had
no way to enumerate the prose that still spelled the old one, and no way to
know how much of it there was.

`FIND usages` now returns those occurrences too, and every row carries a `role`
saying what the name was written in:

| `role` | The name appears in |
|---|---|
| `code` | an identifier the grammar resolved |
| `comment` | comment text |

`role` filters and groups like any other field. `WHERE role = 'code'` narrows to
the references a compiler sees — the previous behaviour, still one clause away.
`GROUP BY role` sizes a campaign by kind before touching anything. The compact
output collapses sites per file *and* role, so the two never blur together:

```csv
"file","role","node_id","rev","[lines]"
"include/zephyr/pm/device.h","code","n8b70d2ae61ff","h4e6620bb17ac9d35","211,231,255,271"
"include/zephyr/pm/device.h","comment","n8b70d2ae61ff","h4e6620bb17ac9d35","214,242,276,702,702,704,705"
```

Two properties matter for using this safely:

**Matching is token-exact.** `FIND usages OF 'CONFIG_X'` does not match
`CONFIG_X_ASYNC`, in prose any more than in code. A comment naming a
neighbouring flag is not a hit.

**Each occurrence reports its own line.** A name buried in a twelve-line doc
block comes back at the line it is written on, not the line the comment opened
on, and a line holding the name twice yields it twice — the line lists above are
real coordinates you can splice at, not an approximation pointing at the top of
a block.

The engine classifies the *container*, never the meaning. Whether a comment
occurrence refers to your symbol or merely spells it the same way is a judgment
call, so comment sites are a review queue: read them before sweeping them.
Grouping them under one query is what makes that review possible at all.

Occurrences are collected at index time into per-role posting blobs, so queries
stay lookups — no text scanning at query time. Comment kinds are declared per
language, so the core never names a construct. Every occurrence flows through
the existing machinery unchanged: file grouping, the response size ceiling,
per-file handles and revs, and `FOUND` set arming, where a comment line
contributes exactly its own line to a sweep.

### Changed

- `FIND usages … GROUP BY <field>` now labels the key column with the field
  actually grouped on. It previously always printed `file`, which was true for
  the only grouping that existed and wrong for any other.
- `MemEstimate`'s usage figures now cover mention sites as well as identifier
  usages; the field names and the session stats that read them are unchanged.

### Changed — indexes rebuild on first use

Occurrence data is stored, so the enrichment-logic version moves and every
cached index is invalidated. The first `USE` of each source after upgrading
re-indexes that worktree; subsequent sessions are served from cache as before.
Existing segments are not readable for the new data: they were written before
mention postings existed, so a stale one would silently under-report comment
occurrences rather than fail.

Note that `FIND symbols`' `usages` column deliberately does **not** grow. It
counts `role = 'code'` sites only, so it keeps ranking symbols by how much code
depends on them; `FIND usages OF`'s `total` is the one that now counts every
occurrence. The two differing for the same name is expected.
## [0.139.78] — 2026-08-01 — feat: `#ifdef` / `#if` / `#elif` are addressable nodes

### Added — a conditional directive is a construct you can find and read

A config flag's guard sites were the one place the index would not take you.
`#ifdef CONFIG_X` did produce a row, but with no kind, so no `fql_kind` filter
could ever select it; `#if defined(CONFIG_X)` and `#elif` produced no row at
all, because the grammar gives them a condition rather than a name. On a real
tree that meant `FIND symbols WHERE name = 'CONFIG_PM_DEVICE_RUNTIME'` reported
21 files while the flag was actually referenced in 44 — and the missing ones
were exactly the conditional compilation the rename had to get right.

All three now emit `fql_kind = 'guard'` rows, addressable like any other
construct:

```sql
FIND symbols WHERE fql_kind = 'guard' WHERE name LIKE '%CONFIG_X%'
SHOW NODE '<node_id>'          -- reads the guarded region
```

`#ifdef` / `#ifndef` keep the macro name they always carried. `#if` / `#elif`
are named by their condition, collapsed to one line of single-spaced tokens —
the same normalisation the guard fields use, from the same function, so a row's
name and its `guard` value agree character for character. A guard node spans
its whole region through the matching `#endif`, so reading one by handle shows
every branch.

`guard` is now both an `fql_kind` value and an enrichment field name. They
select opposite things and the docs say so: `WHERE fql_kind = 'guard'` finds
the directive, `WHERE guard = '…'` finds the rows *inside* the region it opens.

### Added — an empty `FIND symbols` says when the name exists as a usage

A name the worktree only references — declared in a header outside it, reached
through an include that is not indexed — returned the same empty set as a typo.
An exact-name query that finds nothing now reports how many usage sites carry
the name and points at `FIND usages OF`, so "absent" and "not declared here"
stop looking alike.

### Changed — indexes rebuild on first use

Directive rows are new stored rows and consume identity slots, so `node_id`
ordinals shift in every C/C++ file holding a directive. Cached indexes are
rebuilt automatically on the next `USE`; handles captured from an earlier
session should be re-read rather than reused.

One classification moves with them. `error_scope` distinguishes an unparseable
region that no named construct owns (`"file"`) from one sitting inside a
construct the language could name (`"nested"`). A directive is now such a
construct, so a parse error inside `#if … #endif` — the shape macro-heavy C
produces constantly — is reported as `nested` where it used to be `file`. The
errors themselves are unchanged in number and position; only the answer to
"does anything own this?" improved.

## [0.139.77] — 2026-08-01 — change: a `FIND usages` listing has a size, not just a file count

### Changed — the listing withholds whole files once it is large enough

Selecting whole files bounds how many files a `FIND usages` answer covers, but
not how big it is: a hot identifier holds hundreds of sites in each of twenty
files, and nothing capped the total. Measured on real trees, a rename campaign
returns around 130 sites across its twenty files and the largest single file
anywhere holds about 700 — but a query on a common local name runs to several
thousand.

The listing now stops once it has rendered enough sites, and stops the way the
file cap already did: **whole files, from the tail**. File order never changes,
no file is ever shown partially, and the first selected file is always shown
complete however large it is — a listing that shows no file at all answers
nothing.

When files are withheld the response says so in a `hint`, and names the lever
that helps. The two cases differ: files beyond the `LIMIT` come back if you
raise it, files beyond the size ceiling do not — there, narrow with `IN` /
`WHERE`, or use `GROUP BY file` for per-file counts without the line lists.

`total` is unchanged: still every site in the worktree, so the gap between it
and the rows you can see is still how you tell the listing is partial.

### Documentation

The golden-suite guide now warns that its derived `_file` field yields the node
hex for any row carrying a handle and falls back to the path only for rows
without one. Which side a row lands on is an engine property that moves — usage
rows recently gained a handle — so a case that means "the file" should assert
`path` directly.

## [0.139.76] — 2026-08-01 — feat: a `FIND usages` row is editable where you read it

### Added — usage rows carry their file's handle and rev

A usage site is a line, not a node, so it had no handle of its own — and the
row carried nothing else either. Finding a site and changing it meant a second
round trip through `FIND files` to learn the handle and rev of the file it sat
in.

Every site row now carries the `node_id` and `rev` of its file — the same two
values `FIND files` reports for that file — so the edit is written straight
off the row:

```sql
FIND usages OF 'oldName' IN 'src/**'
CHANGE NODE '<node_id>(<line>)' IF REV '<rev>' MATCHING WORD 'oldName' WITH 'newName'
```

The rev is the file's, so it is deliberately coarse: any edit anywhere in that
file invalidates it. That is the honest gate for a target addressed by line
number, because line numbers move.

What a usages row contributes to `FOUND` is unchanged: one line, not the file.
A `CHANGE NODES FOUND` sweep armed from a usages query still rewrites only the
site lines, never the rest of the file.

## [0.139.75] — 2026-08-01 — change: `FIND usages` caps by file, so a listed file shows every one of its sites

### Changed — `LIMIT` on `FIND usages` counts files, not rows

A usage site is one line of one file, and the question behind `FIND usages` is
"which files hold this name?". The row cap answered a different one: it cut the
site list mid-file, so a file came back with part of its sites and nothing said
the rest had been dropped. A file with 61 sites reported 20 lines and looked
complete.

The cap now selects whole files. `LIMIT 5` means "the first five files, every
site in each"; the default cap means the first 20 files; `OFFSET` skips whole
files, so paging never splits one file across two pages. Files come back
ordered by their first site, and sites within a file ascend by line — the same
file order the compact output showed before, now with complete line lists.

`total` is the true site count across the worktree **even under an explicit
`LIMIT`**: it is what a rename campaign measures progress against, and a
`total` larger than the row count is how you see that files were left out.
This diverges from `FIND symbols`, whose `total` is still capped by `LIMIT`;
the difference is deliberate.

A result with files left out still arms no `found_rev`, so every `FOUND` verb
refuses over it — but a listing that now fits, such as one large file's whole
site set, arms cleanly where it used to be refused.

`GROUP BY` is unaffected: its rows are aggregates, already one per group, and
its `LIMIT` counts those groups.

## [0.139.74] — 2026-07-31 — fix: a re-USE after the base moves rebuilds the session instead of resuming the old snapshot

### Fixed — stale session resume after `REFRESH SOURCE`

Re-issuing `USE <source>.<base> AS '<alias>'` after the base moved (typically
`REFRESH SOURCE` syncing the bare repo) silently resumed the in-memory
session at the pre-move commit — while the response's `base_commit` reported
a fresh resolution of the requested base, i.e. the NEW head. An agent was
told it stood on the refreshed commit while every read served the old one.

Three defects fixed together:

- The resume staleness check compared the bare branch head against a commit
  the session never recorded on the columnar path (`None` compared as "not
  stale"), and could not evaluate commit-hex bases at all (a hex is not a
  branch). Sessions now record the commit the requested base resolved to at
  creation, and a re-`USE` compares a fresh resolution against it — failing
  CLOSED: if either side is unknown, the session is evicted and rebuilt.
  Same-hex re-`USE` still resumes (a hex is immutable).
- `USE` responses could report a base the checkout did not match. Resumed
  sessions now report the commit their index was actually built from, and a
  reused worktree or session branch reports its real tip — never a fresh
  re-resolution of the requested base.
- A reused clean worktree (or session branch with no checkout) sitting behind
  the requested base is fast-forwarded to it, so the rebuilt session actually
  serves the new content. Local modifications and session commits are never
  touched — those sessions keep their state and say so via `base_commit`.

### Fixed — uncommitted index state silently dropped across an engine upgrade

`.forgeql-columnar-delta` (the per-session dirty-overlay state) recorded no
index-output generation. After an `ENRICH_VER` upgrade, each staged entry
failed its version-suffixed file lookup one by one — a warning per entry —
and the dropped entries also dropped their path shadowing, leaving the
pre-edit base rows visible for files whose edits lived in the delta.

The delta file now records the generation it was written under (format
version 3; older formats are refused loudly, as before). A delta from a
previous generation keeps its version-independent deletions, discards the
staged segments in one explicit decision, and hands the affected source
paths back to the engine, which re-indexes them from the worktree on both
reconnect and `ROLLBACK`. A missing staging segment file queues its path the
same way instead of being skipped silently.

`doc/syntax.md` documents the strengthened `base_commit` guarantee. Nothing
stored in index segments changes — no `ENRICH_VER` bump.

## [0.139.73] — 2026-07-31 — docs: the guard surface is what the engine can answer

### Removed — documented guard configuration with nothing behind it

- Five language-configuration fields — `file_guard_pattern`,
  `file_guard_suffix_pattern`, `comptime_guard_kinds`, `builtin_guard_patterns`
  and `source_set_pattern` — were declared on the schema, mapped through, and
  given accessors that nothing called. No shipped language configuration set any
  of them either, so they were not an unfinished feature so much as a shape where
  one had been intended. Two of them appeared in `has_guard_support`'s
  disjunction, where being permanently empty made them contribute nothing.
  Removing them changes no behaviour: the schema does not reject unknown fields,
  so a configuration that names one is ignored now exactly as it was before.
- `guard_kind` no longer lists `"build_tag"` or `"comptime"`. Nothing produced
  them: the three frame builders write `"preprocessor"`, `"attribute"` and
  `"heuristic"`, and that is the whole set. The two extra values survived only a
  round-trip through the field reader, which handed back whatever had been
  written. A query filtering on them could only ever return nothing.

- `guard_info_from_fields`, which rebuilt a guard identity by parsing a row's
  field map, had no callers: everything reads the guard stack directly, where
  the kind is already a static string and nothing needs re-parsing. It went with
  its only helper. This is what kept the two dead `guard_kind` values alive —
  they were reachable solely as strings that function might have read back, from
  a map nothing wrote them into.
### Documentation

- The `guard_kind` row in both tables now says which construct produces each
  value rather than listing bare strings, and the agent-contract table gains
  `"heuristic"`, which it had been missing while carrying two values that did not
  exist.

## [0.139.72] — 2026-07-31 — fix: a guard group keeps its identity across restarts

### Fixed — `guard_group_id` is derived, not counted

- The ID shared by the arms of one `#ifdef`/`#if` block came from a
  process-global counter. Segments cache the rows that carry it, so after a
  restart a cached file kept the previous run's numbering while every freshly
  indexed file began again at 1. The same group then answered to two different
  numbers depending on which files happened to be cached, and two unrelated
  groups could answer to the same one.
- The visible cost was in re-indexing. `guard_group_id` is part of the key that
  matches a node to its previous ordinal, so once the numbering disagreed the
  guard component of that key stopped matching anything. It is a soft filter, so
  nothing broke loudly — node handles simply lost a disambiguator they were
  supposed to have, silently and only after a restart.
- The ID is now derived: a hash of the repo-relative path and the byte offset of
  the opening directive. The same content therefore yields the same ID in every
  run, every restart and every checkout. The path is taken relative to the
  worktree root for the same reason segment keys are — an absolute path would
  key one file differently in each worktree of a bare repo.
- Two behaviours worth knowing. The value is opaque: it is not an ordinal, not a
  count, and carries no meaning beyond equality. And it moves when the file is
  edited, because the opening directive moves — group rows by it, do not use it
  to follow a group across edits.
- Rust `#[cfg]` attribute guards were the worst case and are fixed by the same
  change: they took a fresh counter value for **every row**, so an item's stored
  `guard_group_id` was per-row rather than per-group, and never reproducible.

### Changed

- `IndexContext` gains `workspace_root`. Only guard group IDs read it, and only
  to strip the prefix before hashing. Callers that persist rows pass the real
  root; tests and one-off indexing pass `None`, which hashes the path as given —
  deterministic within a run, which is all an uncached index needs.
- The staged-segment file name gains the enrichment version. A staged segment
  holds index output, but its name carried only a path and a content hash, so a
  version bump did not invalidate it: an uncommitted transaction staged before
  an upgrade survived the restart, the re-index skipped the file because a
  segment already existed, and COMMIT promoted the previous generation's rows
  into the new store. Committed segments have always lived under a versioned
  directory; the staging area now matches.

### Also fixed in this change

- The incremental re-index of a non-columnar source derived its guard IDs from
  the absolute path while the full build used the relative one, so the two
  disagreed on the very first edit — the same defect this change removes,
  surviving on one path. That index is written to disk and restored on resume,
  so it was not a test-only gap. The worktree root is now carried alongside the
  table and threaded into the re-index.
- One node can open two guard frames — a block guard and an environment guard —
  and both derive their ID from that node's position. The guard kind is part of
  the hash so the two do not collapse into one identity with the same branch
  ordinal, which the exclusivity test would have read as opposite arms of a
  single block. No shipped language configuration declares a kind as both, so
  this was reachable by configuration rather than live.

## [0.139.71] — 2026-07-30 — fix: a guard reaches every row, not only declarations

### Fixed — control-flow and expression rows carry their guard

- Guard fields were injected only into declaration-like rows. An `if`, `switch`,
  `for`, numeric literal, cast or call inside a conditional region carried none,
  so any query pairing a guard with a field that lives on those kinds matched
  nothing — silently, with no error and no hint that the question was
  unanswerable. `WHERE fql_kind='number' WHERE is_magic='true' WHERE guard_defines
  LIKE '%CONFIG_X%'`, and the MISRA-shaped "is this `switch` without a `default`
  actually compiled in this build", were both permanently empty.
- A row is inside whatever conditional region the walk is inside, whatever kind
  of row it is, so the guard is now injected for all of them. The injection
  happens before the ordinal key is built, so the key gains its guard component
  too rather than the fields being populated while the key stays blank.
- The docs said these fields applied to "all symbols", which was never true and
  is still not: a `comment_block` / `array_block` row spans its members rather
  than being walked, so it carries no guard, and an attribute guard scopes only
  the item it annotates. Both tables (the reference and the agent contract) now
  say "all walked rows" and name the exceptions underneath.

### Tests

- The four golden cases that pinned this defect pass and are promoted to hard.
  With them the guard suites carry no expected failures at all: every case
  asserts behaviour the engine now produces, and the suites become a permanent
  hard gate rather than a defect inventory.
- Four hard cases needed their pins re-derived, since a guarded region now yields
  more guarded rows than before — measured on two of them at +37% and +38%, which
  moves any count over such a region. One had lost its premise outright: it asked
  for a kind that carried no guard, and there is no longer such a kind, so it now
  exercises the same mechanism through a field that is genuinely kind-scoped.
  Those moved counts are also the positive proof that the corpus actually
  reindexed rather than serving cached segments.

- Promoting the cases also exposed a golden assertion that had never worked. A
  `distinct` check named its column `fql_kind`, which is what the CSV rendering
  calls it; a JSON result row names it `kind`. Every row therefore derived to
  null, the distinct set collapsed to one value, and the check held for any query
  returning at least one row of any shape. While the case was marked a known
  failure this was invisible — a wrong assertion and a real defect look the same
  from outside. Only asking a case to pass asks its assertions to be true.

## [0.139.70] — 2026-07-30 — fix: guard text now says what the source says

Four defects in how a guard condition is rendered and decomposed. They ship
together because they are one mechanism: each needs a frame to remember its own
condition separately from the accumulated one, and fixing any in isolation would
rewrite the strings the next one produces.

### Fixed — an arm carries the negations of the arms before it

- An `#elif` arm was reported as though it were a standalone `#if`. Only the arm's
  own condition appeared, so the `#else` of a three-arm chain claimed merely that
  the last arm was false — a predicate the earlier arms also satisfy. Asking
  "what is compiled only when this feature is absent" returned a set that
  included builds where it is present.
- An arm now renders as `!(c₀) && … && !(c_{k-1}) && cₖ`, and contributes each
  earlier arm's identifiers to `guard_negates`. A two-arm `#ifdef X` / `#else`
  still yields exactly `!X`.

### Fixed — a conjunct holding a top-level `||` is parenthesised

- `&&` binds tighter than `||`, so splicing a disjunctive child into its parent
  bare produced `A && B || C`, which parses as `(A && B) || C` — satisfied by `C`
  alone, and so a weaker predicate than the source. In one Zephyr header the
  emitted guard was true outside the include guard it was written inside.
- Parenthesising happens only where composition can reassociate: a lone condition
  is left exactly as written, so grouping is unaffected.

### Fixed — negating a disjunction decomposes

- `!(A || B)` is `!A && !B`, so both identifiers must be undefined; only
  `guard_mentions` listed them. The terms a top-level `||` withholds from
  `guard_defines` are now retained for exactly this purpose.
- The complementary correction: negating a condition that mixes `&&` yields a
  disjunction, where no identifier is individually required either way. That case
  previously swapped the two lists and asserted both — so the `#else` of
  `#if defined(A) && !defined(B)` claimed A must be undefined, which is false
  whenever B is defined. Such an arm now contributes nothing, and the raw text
  remains the record.

### Fixed — `guard` is whitespace-normalised

- A directive continued across lines kept its backslashes, newlines and leading
  tabs, so `GROUP BY guard` split identical conditions on whitespace alone and the
  value could not be written into a `WHERE` clause at all.

### Fixed — a long `#elif` chain no longer exhausts memory while indexing

- Found while building the above: an arm folded each earlier arm's *accumulated*
  identifier lists rather than its own, so arm *k* contained arm *k−1*'s fold,
  which already contained *k−2*'s. The lists doubled per arm and were never
  deduplicated. Indexing a corpus with long chains exhausted 32 GB in 46 seconds;
  the same corpus now completes in about two minutes.
- Arms fold each other's own terms, and the accumulated lists are deduplicated,
  so their size is bounded by a chain's distinct identifiers rather than by its
  arm count. A unit test folds 24 arms and asserts the exact size at every step.

## [0.139.69] — 2026-07-30 — fix: end a guard region at its own closing directive

### Fixed — `extern "C"` no longer leaks its condition into the rest of a header

- A guard region took its extent from the parser's guard node. That node is not
  bounded by the directive that closes the region: the grammar consumes block
  items until it finds a closing directive, so anything that swallows the real
  one runs the node on to the *next* close. The C idiom
  `#ifdef __cplusplus` / `extern "C" {` / `#endif` does exactly that — the brace
  opens a linkage specification whose body absorbs the `#endif` — and the node
  then spans two unrelated groups.
- Everything between them inherited a `&& __cplusplus` conjunct it was never
  subject to. In a Zephyr snapshot that was 128,451 of 2.9M indexed symbols,
  including 1,548 carrying the condition twice over. `GROUP BY guard` and
  `WHERE guard = …` were unusable on C public headers, and anything reading
  `guard` as a boolean concluded most of the API existed only in C++.
- A region now ends at its own closing directive, found by counting directive
  nesting forward from the opener. The parser's span still bounds the scan and
  is still the answer when the directives do not balance: the node over-extends
  but never stops short, so its end is a safe cap and a safe fallback. Nothing
  repairs a malformed file.
- The directive markers are language configuration, not core knowledge — a
  language that declares none keeps the previous behaviour untouched.

### Known limits

- A directive written with space between the sigil and the word (`#  ifdef`) is
  not recognised. The scan then declines to balance and the parser's span stands,
  so that region behaves as it did before.
- The scan reads lines, not tokens, so a directive inside a comment or a string
  literal counts. The two directions are not symmetric. A commented *opening*
  directive is harmless: it inflates the nesting, the real close no longer settles
  it, and the scan declines — the parser's span stands. A commented *closing*
  directive with no commented opener to match it will end the region early, which
  strips the guard from rows that are inside it. A fully commented-out pair is
  balanced and therefore also harmless, which is the common shape.
- Making the scan comment-aware needs the comment delimiters declared as language
  configuration, the same way the directive markers now are; it is not something
  core can infer.

### Tests

- The two golden cases that pinned the correct values for this defect now pass
  and are promoted to hard. The arch header they use accounts exactly: 124 rows
  ended in `&& __cplusplus` before, of which 122 belonged to the enclosing
  `!_ASMLANGUAGE` region and join its bucket (5 there already, 127 after), while 2
  remain — the two `#ifdef __cplusplus` conditions themselves, each genuinely
  inside its own group. The second step pins that 2, so the split is asserted
  from both sides rather than taken on trust.
- Unit tests drive the scan directly: nesting, the `extern "C"` shape, an
  unbalanced region, a region with no trailing newline, indentation and trailing
  comments, and the two documented limits. One pins that `#if` must not match an
  `#ifdef` line — without a word boundary the opener is counted twice and the
  region never balances.

## [0.139.68] — 2026-07-29 — fix: stop reporting valid guard fields as misspellings

### Fixed — the seven guard fields are now registered field names

- `WHERE guard = …` and its six siblings (`guard_defines`, `guard_negates`,
  `guard_mentions`, `guard_group_id`, `guard_branch`, `guard_kind`) were absent
  from the enrichment-field registry, with two consequences. An empty result was
  reported as *"unknown WHERE fields match nothing. Check the spelling…"* for a
  correctly spelled, documented field. And on any corpus where no loaded segment
  happened to carry a column by that name, the query was **rejected outright** —
  so the same query errored on one repository and returned rows on another.
- `naming` and `name_length` were unregistered for the same reason and are
  included: they apply to every named symbol, so like the guard fields they have
  no node-kind list to declare.
- The registry is still not exhaustive. `suffix_meaning`, `binding_kind` and
  `owner_kind` are documented and remain unregistered, so they keep the same
  corpus-dependent refusal. They are kind-scoped, unlike the fields above, so
  fixing them means giving the field→kind map a real kind list rather than adding
  a name — left for whoever does that work.
- Registration is deliberately kept out of the field→kind map. That map's values
  drive prefilter narrowing, so an entry there needs a list of the kinds that
  carry the field — and a guessed list would silently drop rows that do carry it.
  These fields cut across kinds rather than selecting any, so they live in a
  separate name list and continue to narrow nothing.

### Tests

- `unknown_where_field_gets_hint` gains a guard case, pinning the fix where the
  contract it belongs to already lives: a documented, queryable guard field must
  come back as an ordinary empty result, with no hint. That assertion fails on
  the previous revision. It does not cover the outright refusal — that path is
  corpus-dependent and this fixture set carries guard rows, so no segment lacks
  the column.
- Unit tests cover the registry directly: the seven names are recognised, a
  misspelling still is not, and none of the unscoped fields appears in the
  field→kind map. The last of those is the one that matters going forward — it is
  what stops a kind list being added here on a guess, which would silently drop
  rows that do carry the field.

## [0.139.67] — 2026-07-29 — test: pin the preprocessor-guard contract

### Added — `enforcement: "expect_fail"` for golden cases

- Golden cases already carried an `enforcement` level (`hard`, `soft`,
  `ignore`). A fourth value, `expect_fail`, marks a case that asserts *correct*
  behaviour the engine does not produce yet: a failure is the expected state and
  does not gate, while an unexpected **pass** fails the gate.
- The asymmetry is the point. A defect's correct behaviour can be pinned the day
  it is diagnosed rather than the day it is fixed, and because passing is hard,
  the change that fixes it cannot forget the case — the run goes red until the
  key is removed, in the same commit where the promotion is reviewable.
- `expect_fail_reason` is required beside it and says why the behaviour does not
  hold. It is printed with each expected failure and quoted back in the
  promotion message, and the run ends with a count, so the open inventory is
  visible in every test run.
- `tests/golden/README.md` documents the value and the two ways it rots: a case
  whose asserted value is itself wrong stays "expected failure" silently, so each
  fix must re-check that the remaining cases still fail for the reason they state;
  and because any error counts as the expected failure, a data dir missing a
  suite's corpus reds the suite's hard cases while passing every `expect_fail`
  case in it vacuously.

### Added — two golden suites for preprocessor guards

- `enrich_guard.json` (14 cases) and `probes_guard.json` (3 cases), over a
  frozen Zephyr corpus. A single Rust `#[cfg]` test had been the whole of the
  guard feature's coverage; the C preprocessor path — `#ifdef`, `#elif` chains,
  `#else` negation, `extern "C"` headers — had none.
- Seven cases pin behaviour that is already correct, so later changes cannot
  quietly break it: `#ifdef X` / `#else` yielding `X` and `!X` with matching
  `guard_defines` / `guard_negates`, a nested `#ifdef` composing with `&&`, arm
  ordinals, rows of one arm sharing a group, and a known field with no carriers
  returning an empty set rather than an error.
- Ten cases are `expect_fail`, each pinning the value a hand-read of the source
  says the field should carry: an `#endif` that fails to close an `extern "C"`
  block so the rest of the header inherits `&& __cplusplus`; `#elif` arms
  reported as if they were standalone `#if`s, dropping the negations of the arms
  before them; a child condition holding a top-level `||` spliced under `&&`
  without parentheses, which parses as a weaker predicate than the source; a
  negated disjunction that populates neither term of `guard_negates`; line
  continuations and tabs kept verbatim in `guard`, which splits `GROUP BY guard`
  on whitespace alone; and guard fields never reaching control-flow or
  expression rows, which silently empties any query pairing a guard with
  `is_magic`, `has_catch_all` or a control-flow field.
- Every pinned value was captured from a live query against the corpus or
  derived by reading the file's raw directives.

### Added — unit coverage for the guard helpers

- Direct tests for `parse_condition_text`, `negate_frame`,
  `inject_guard_fields` and `are_guards_exclusive`: the empty stack, a negation
  with no parent frame, two-arm negation reading `!X` rather than `!(X)`,
  innermost-arm deduplication when a chain puts every arm on the stack at once,
  and the positive terms a disjunction withholds from `guard_defines` while
  still listing them in `guard_mentions`. They need no corpus and localise a
  failure that the end-to-end suites can only report.

### Changed

- `golden_test.rs`: trial construction moved to `build_trial`, and the two
  end-of-run summaries to `report_non_gating`, keeping `main` within the
  function-length lint after the new enforcement arm.

## [0.139.66] — 2026-07-26 — refactor: give the tree walk its own module

### Changed — `file_indexer.rs` is now the entry surface, not the whole pass

- `collect_nodes`, `update_guard_stack` and `ascend_to_next_sibling` moved from
  `ast/index/file_indexer.rs` into `ast/index/file_indexer/walk.rs`. They are
  the depth-first pass itself, and they own the state that only means anything
  mid-walk: the parent/ordinal stacks, the guard stack recording which
  conditional a node sits under, and the block run currently being spanned.
- The module doc records why the traversal order is load-bearing: a newly seen
  node takes the next value from the per-file counter as the walk reaches it,
  so reordering the walk renumbers every handle it cannot match to a prior
  hint.
- `ast/index/file_indexer.rs` is down to the entry surface it is named for —
  macro collection, `IndexContext`, the addressability predicate, and
  `index_file` / `index_file_from_source` — from 1,436 lines to 340, of which
  the inline ordinal-remapper tests are now the bulk.
- Code motion with two deliberate exceptions, both outside the logic:
  `collect_nodes` gained `pub(super)` on its signature, and one line of its body
  was requalified from `ordinals::OrdinalRemapper::next_ordinal` to
  `OrdinalRemapper::next_ordinal`. The bare `ordinals::` prefix resolved only
  from the parent module; the new import names the same
  `file_indexer::ordinals::OrdinalRemapper`, so the call is unchanged. Every
  other line of all three functions is byte-identical, which is what makes this
  safe to ship without an `ENRICH_VER` bump.

## [0.139.65] — 2026-07-26 — refactor: gather row emission in one module

### Changed — the code that decides what a row *is* now has its own module

- `PreparedRow`, `RowSink`, `MAX_ERROR_SNIPPET`, `error_snippet`,
  `process_node_rows`, `build_row_fields`, `row_rev`, `emit_addressable_row`,
  `emit_extra_rows` and `extract_fields` moved from
  `ast/index/file_indexer.rs` into
  `ast/index/file_indexer/rows.rs`. This is the centre of the indexing pass:
  for one node it decides addressability, assigns the ordinal, gathers the
  enrichment fields, and emits the addressable row plus whatever the enrichers
  contributed. Nearly everything a later query sees — a row's name, kind,
  fields, `node_id` and `rev` — is decided here.
- Pure code motion. Every body is byte-identical, and the cluster turned out to
  be almost entirely self-contained: only `process_node_rows` (called by the
  tree walk) and `row_rev` (called by block emission) needed `pub(super)`. The
  other eight items stayed private — now to a focused module rather than to a
  1,400-line file.

## [0.139.64] — 2026-07-26 — refactor: gather block-group synthesis in one module

### Changed — the synthetic block nodes now have their own module

- `ActiveBlock`, `BlockTag`, `block_group_key`, `scan_block_run`,
  `emit_block_row` and `attr_extended_start` moved from
  `ast/index/file_indexer.rs` into `ast/index/file_indexer/blocks.rs`. This is
  what turns a run of adjacent same-kind siblings into one addressable node —
  the `comment_block` and `array_block` kinds that exist in no grammar — while
  leaving each member its own row and tagging it with the block's address.
- The module doc now records why `scan_block_run` walks with
  `next_named_sibling` rather than `next_sibling`: run members are often
  separated by anonymous punctuation, so a raw sibling walk breaks the run at
  the first separator. That is the bug that once made `array_block` emit
  nothing at all, and it hid because Rust comment runs have no separator, so
  for them the two walks agree.
- Pure code motion. Every body is byte-identical; the only changes are the
  visibility keywords the move forces on the six items and on the fields of
  the two structs, which the parent both builds and reads.

## [0.139.63] — 2026-07-26 — refactor: gather ordinal assignment in one module

### Changed — what keeps a `node_id` stable now lives in one place

- `OrdinalHint`, `OrdinalTombstones`, `OrdinalRemapper`, `OrdinalMatchKey`,
  the whole `impl OrdinalRemapper`, and `assign_ordinal` moved from
  `ast/index/file_indexer.rs` into `ast/index/file_indexer/ordinals.rs`. This
  is the machinery that lets a handle survive a re-index — match each node of
  the new pass against the previous pass's hints, and tombstone the ordinals a
  removal freed so a byte-identical surviving sibling cannot adopt one.
- `ast::index` keeps re-exporting `OrdinalHint`, `OrdinalRemapper` and
  `OrdinalTombstones` at their existing paths, so nothing outside the crate
  changes.
- Pure code motion. Every body is byte-identical; the only changes are the
  visibility keywords the move forces, on the items the parent still reaches:
  `OrdinalMatchKey` and its seven fields (the parent builds the literal in six
  places), `assign_ordinal`, and `OrdinalRemapper::assign`.
- `OrdinalRemapper::next_ordinal` gained a read-only accessor rather than
  becoming a visible field. The parent only reads the counter, and leaving the
  field private is what keeps "only `from_previous` and `assign` may move it"
  enforced by the compiler rather than by convention.

## [0.139.62] — 2026-07-26 — refactor: give the rev content hashes their own module

### Changed — the hashing behind a node's `rev` is now one small file

- `short_sha256_hex`, `first_body_statement_fingerprint` and
  `node_content_hash` moved from `ast/index/file_indexer.rs` into
  `ast/index/file_indexer/hash.rs`. They are the three functions that decide
  what a `rev` is — an eight-byte SHA-256 prefix, short enough to travel in a
  CSV row beside its handle — and they had no business being buried at line
  1,267 of the file that walks the tree.
- Pure code motion. All three keep byte-identical bodies; the only change is
  `pub(super)` on each, which is the minimum the move forces since the parent
  calls all three.

## [0.139.61] — 2026-07-26 — refactor: move the index unit tests out of the index

### Changed — `ast/index.rs` is the runtime core again, not mostly tests

- The inline test module was roughly three quarters of `ast/index.rs`. It moved
  into `ast/index/tests/`, split by what each group covers: `table.rs` for the
  symbol table and its secondary indexes, `snippets.rs` for what indexing a
  snippet actually produces (comment blocks, error rows, block aliases, and
  node-id stability across an unrelated edit), `cpp.rs` for the C++ node kinds,
  and `util.rs` for the two snippet-indexing helpers the others share.
- `ast/index.rs` goes from 1,574 lines to 361. What remains is the runtime: the
  table types, the stats accessors, `reassign_intern_ids` and `node_text`. The
  file was never as large as its size suggested, and now it does not look it.
- Pure code motion: no test was added, dropped or reordered, and every body is
  semantically unchanged. The only edits are each file's imports, the two shared
  helpers becoming `pub(super)` so the sibling test modules can call them, and
  five wrapped expressions that rustfmt re-flowed once the de-indent freed four
  columns.

## [0.139.60] — 2026-07-26 — refactor: gather index and backend management in one module

### Changed — the session's index lifecycle now lives beside itself

- The twenty methods that build, persist, resume, re-index and drop a
  session's index — and that install the columnar backend or retire the legacy
  one — moved from `session/mod.rs` to `session/indexing.rs`. They were more
  than half the file, and they are the part with a real invariant to state: a
  session outlives the process that created it, so an index that lived only in
  memory would be rebuilt on every reconnect.
- `session/mod.rs` keeps the session's identity, lifecycle and persistence:
  the sentinel, the last-active timestamp, the checkpoint plumbing. It goes
  from 932 lines to 626.
- Pure code motion; no visibility changed and no persist call moved.

## [0.139.59] — 2026-07-26 — refactor: give the session line budget its own module, and fix two module docs

### Changed — the line budget moved out of the session god-object

- The seven budget methods moved from `session/mod.rs` to `session/budget.rs`.
  They were already a separate `impl Session` block behind a section banner, so
  the move is the banner made real: reading deducts the source lines it
  discloses, writing credits back a line for every line a mutation wrote up to
  the configured ceiling, and the persisted fields are written as they change
  so a budget survives a restart with the session it belongs to.

### Fixed — two module docs were attached to the following item

- `session/mod.rs` and `session/checkpoint_file.rs` both opened with a `///`
  *outer* doc comment, so each documented the first `use` statement below it
  rather than its own module. Both are now `//!`. The session header also
  dropped two references to private planning phases, which mean nothing to a
  reader of this repository, and gained an index of the submodules.

## [0.139.58] — 2026-07-26 — refactor: separate engine construction from command dispatch

### Changed — `engine.rs` is now state plus a single dispatch path

- `ForgeQLEngine::new` and the startup scan that rediscovers sources left by an
  earlier process moved to `engine/construct.rs`. Both run once, before any
  command does, which is what separates them from everything left in
  `engine.rs`.
- That completes the split: `engine.rs` goes from 1,039 lines to 535, and what
  remains is what its own header claims — the engine's state, and the single
  path every operation takes through `execute` and `dispatch_op`. The other
  concerns now sit in `coach_report`, `construct`, `jobs` and `status`
  alongside the existing `exec_*` modules.
- Pure code motion, with no visibility change at all: `new` was already public
  and stays so, and the scan is private to the new module because its only
  caller moved with it.

## [0.139.57] — 2026-07-26 — refactor: give background jobs and status reads their own modules

### Changed — two more concerns lifted out of the engine dispatcher

- Collecting background work now lives in `engine/jobs.rs`: `finish_pending`,
  which turns a job snapshot into a result, and `reconcile_gate_jobs`, which
  folds finished gated jobs back into the transaction waiting on them. The
  worktree liveness guard moved with them, because it defends the same
  assumption from the other side — that the worktree a session was opened
  against still exists when the work lands.
- The read-only accessors moved to `engine/status.rs`: command counts, budget
  snapshots, worktree paths, the inline cap. They answer questions without
  changing anything, and transports call them to build status output.
- Pure code motion. Only `check_worktree_alive` changed visibility, from
  private to `pub(super)`, which is the reach it already had.

## [0.139.56] — 2026-07-25 — refactor: separate coach reporting from the engine, and fix the engine's module doc

### Changed — coach telemetry moved out of the dispatcher

- The eight functions that describe a finished command to the onboarding coach
  moved from `engine.rs` into `engine/coach_report.rs` — roughly a fifth of the
  file, and the one concern in it that has nothing to do with executing a
  command. Gathering them makes the direction of the relationship legible: the
  coach is handed an outcome that has already been decided, and the hint it
  returns can change what the user is told, never what the engine did.
- Five of the eight are now private to that module rather than merely private
  to a 1,039-line file. `observe_command` becomes `pub(super)`, which is the
  same reach it already had.

### Fixed — the engine's architecture doc was attached to an import

- `engine.rs` opened with a 32-line `///` *outer* doc comment, so the
  architecture diagram documented `use std::collections::HashMap;` rather than
  the module. It is now `//!`, and carries a short index of the modules
  alongside it.

## [0.139.55] — 2026-07-25 — refactor: gather the row-building SHOW verbs in one module

### Changed — `exec_show.rs` is now a dispatcher over four focused modules

- `SHOW outline`, `SHOW members`, `SHOW callees` and `FIND files` moved into
  `engine/exec_show/listings.rs`, together with the two helpers that shape their
  JSON. What these four share, and what separates them from the reading verbs,
  is that their rows are *built* rather than quoted: each produces a set the
  clause pipeline then filters, sorts and caps.
- `engine/exec_show.rs` keeps only what every verb shares — the dispatcher, the
  clause pipeline, and the two readers that fetch a symbol's bytes — and gained
  a module header explaining the split. This step takes it from 502 lines to
  266; across the three steps, from 1,026.
- Pure code motion; no behaviour change and no response byte altered.

## [0.139.54] — 2026-07-25 — refactor: separate reading source text from paging it

### Changed — the source-reading and `SHOW MORE` verbs moved into their own modules

- `SHOW context`, `SHOW signature`, `SHOW body`, `SHOW LINES` and `SHOW NODE` now
  live together in `engine/exec_show/read.rs`. They all end in the same place —
  a span of lines out of one file — so keeping them together makes the shared
  line cap, `WHERE text` predicate and budget accounting visible in one screen
  instead of scattered through a thousand-line executor.
- `SHOW MORE` moved to `engine/exec_show/more.rs`, on its own because it reads
  the buffered window a capped read left behind rather than the source file.
- Pure code motion. Two functions that the wider engine calls, `exec_show_node`
  and `exec_show_more`, keep their exact previous reach: one level deeper in the
  module tree `pub(super)` would have meant something narrower, so they are now
  spelled `pub(in crate::engine)`.

## [0.139.53] — 2026-07-25 — refactor: give SHOW's handle stamping its own module

### Changed — handle and metadata stamping moved out of the SHOW executor

- Moved the three helpers that stamp `node_id`, `rev` and error counts onto
  `SHOW` result rows, together with the predicate that decides whether a query
  asked for error counts at all, out of `engine/exec_show.rs` and into
  `engine/exec_show/stamps.rs`. They sat at the far end of a thousand-line file,
  which made their differing schedules hard to see: path handles are stamped
  after LIMIT, while error counts must be filled in before filtering because
  the clauses filter on them. Pure code motion — no behaviour change.

## [0.139.52] — 2026-07-25 — test: split the tail enrichers out of enrichment_integration.rs

### Changed — eight per-enricher files carved out of enrichment_integration.rs

- Split the DeclDistance, Escape, Shadow, UnusedParam, Fallthrough, Recursion,
  and Todo enricher tests, plus the fetch-cap early-exit regression test, into
  eight focused files (`enrichment_decl_distance.rs`, `enrichment_escape.rs`,
  `enrichment_shadow.rs`, `enrichment_unused_param.rs`,
  `enrichment_fallthrough.rs`, `enrichment_recursion.rs`, `enrichment_todo.rs`,
  and `enrichment_fetch_cap.rs`), each including the shared `enrichment_harness`
  module. Test bodies are unchanged.

## [0.139.51] — 2026-07-25 — test: extract a shared prelude for the enrichment integration suite

### Changed — enrichment integration tests now share a prelude module

- Added `tests/enrichment_harness/mod.rs`, which holds the thin
  `(engine, session_id, TempDir)` adapters and the `*_case!` table macros that
  the enrichment integration tests rely on, so their assertions can be split
  across small per-enricher files without duplicating setup.
  `enrichment_integration.rs` now pulls them in with
  `mod enrichment_harness; use enrichment_harness::*;`; every test body is
  unchanged. This is the first step of breaking the ~3500-line suite into
  focused, single-enricher files.

## [0.139.50] — 2026-07-25 — refactor: move each git test beside the code it covers

### Changed — the `git/mod.rs` test module is distributed across the submodules

Final step of the `git/mod.rs` split. The single 490-line test module is gone;
each test now lives in the module it exercises: the exclude-policy tests in
`excludes.rs`, the staging and squash tests in `commit.rs`, the
change-reporting tests in `diff.rs`, and the patch-export tests in `patch.rs`.

The two shared fixtures — building a normal repository with one commit, and
committing everything raw the way a checkpoint would — stay in `git/mod.rs` as
a small `#[cfg(test)] mod testutil`, which is where the submodule tests import
them from.

No test body or name changed. Two doc comments that documented moved tests
were left behind by the move and have been restored above the tests they
describe.

That completes the split. `git/mod.rs` is down from 55 KB to 7 KB —
repository basics and the git-subcommand runner — over `excludes.rs` (10 KB),
`commit.rs` (10 KB), `diff.rs` (23 KB) and `patch.rs` (7 KB), alongside the
pre-existing `source.rs` and `worktree.rs`. Every public name is re-exported
from `git`, so no call site anywhere changed.
## [0.139.49] — 2026-07-25 — refactor: separate committing from patch export

### Changed — `git/commit.rs` and `git/patch.rs`

Third and fourth concerns out of the `git/mod.rs` catch-all, in one step
because they are small and independent.

`git/commit.rs` holds the four staging-and-commit entry points: the internal
checkpoint that keeps the index cache so a hard reset can restore it, the
user-facing commit that strips it, the squash-onto-a-branch variant, and the
path-scoped commit. Putting them together makes the distinction between the
commit shapes legible — each pairs one staging filter from `excludes` with one
commit — which was hard to see when they were 170 lines apart from the policy
they use.

`git/patch.rs` holds `ExportedPatch` and `export_patches`, the `git am`-ready
mbox export, reordered so the record type reads before the function producing
it.

Six public names, all re-exported from `git`, so no call site changes.
`git/mod.rs` is now 26 KB — repository basics, the git-subcommand runner, and
the test module.
## [0.139.48] — 2026-07-25 — refactor: gather the change-reporting surface in one module

### Changed — everything that answers "what changed" moves to `git/diff.rs`

Second concern out of the `git/mod.rs` catch-all. `git/diff.rs` now holds the
whole change-reporting surface: the source-change list between a session
branch and its base, the file list between two arbitrary commits, dirty
working-tree paths, the HEAD-to-worktree path list, the `DiffFile` record and
the patch accumulator behind it, the uncommitted diff that `SHOW DIFF` renders,
a single commit's diff, the commit list a session branch carries, and the
uncommitted-change count that `EXPORT PATCH` reports as a hint.

Grouping them makes one invariant visible that was previously spread over
340 lines: every path these functions return has already been through the
exclude policy, so runtime artifacts cannot leak into a diff or a patch.

All ten public names are re-exported from `git`, so no call site changes.
`git/mod.rs` is down from 55 KB to 35 KB.
## [0.139.47] — 2026-07-25 — refactor: isolate the commit-exclude policy

### Changed — which files ForgeQL refuses to commit now lives in `git/excludes.rs`

`git/mod.rs` had grown into a plumbing catch-all mixing five concerns. The
first one out is the exclude policy, which is the safety-critical one: it
decides which paths never reach a commit, and a regression there puts
ForgeQL's own runtime artifacts into user history.

Moved verbatim into `git/excludes.rs`: the two exclusion lists — the wider set
a user-facing commit drops and the narrower set an internal checkpoint drops —
their `is_*_excluded` predicates, the component-wise directory check, the
index sweep that removes excluded entries staged by an earlier pass, the
runtime-artifact block written into the repository's exclude file, and the
`is_runtime_or_control` test that keeps those files out of diffs and patches.

`ensure_runtime_excludes` and `PATCHES_DIR_NAME` are re-exported from
`git`, so callers outside the module are unaffected; everything else is
`pub(super)` or private to the new module. No behaviour change, and the
existing exclude tests are untouched and still cover the policy.
## [0.139.46] — 2026-07-25 — refactor: move the diff tests into their own file

### Changed — `transforms/diff.rs` is now 26 lines of module wiring

Final step of the `transforms/diff.rs` split. The inline `#[cfg(test)] mod
tests` block — around 670 lines, over 40% of the original file — moves
into `transforms/diff/tests.rs`, declared from the parent as `mod tests;`. The
same shape the `result.rs` split settled on.

Every test keeps its exact name and body, and the module's imports are
unchanged, so `super::` still resolves to `transforms::diff` exactly as it did
when the module was inline. The only edit beyond the move is dropping the
now-pointless section banner that separated tests from code in the old
single-file layout.

That completes the split: `diff.rs` went from 52 036 bytes holding three
concerns plus its tests to 26 lines of documentation, `mod` declarations and
re-exports, over `apply.rs`, `compact.rs`, `lcs.rs` and `tests.rs`. Public
paths are unchanged throughout, and nothing written into an index segment is
affected.
## [0.139.45] — 2026-07-25 — refactor: give the plan/apply layer its own module

### Changed — `transforms/diff.rs` is now only module wiring

Third step of separating the concerns that shared `transforms/diff.rs`. The
layer that turns a plan into a diff — `diff_file_edit`, `diff_plan`, the
in-memory edit application, and unified-diff rendering — moves to
`transforms/diff/apply.rs`.

What remains in `diff.rs` is the module documentation, three `mod`
declarations, the re-exports that keep `transforms::diff::…` paths resolving
for callers, and the test module. `diff.rs` is down from 52 036 bytes before
this series to roughly 25 000, nearly all of it tests.

Verbatim motion: no function body or signature changed, `diff_file_edit` and
`diff_plan` stay public at the same paths, and the two helpers the other
submodules and the tests need are `pub(super)`. Nothing written into an index
segment is affected, so cached segments stay valid.
## [0.139.44] — 2026-07-25 — refactor: give the compact diff preview its own module

### Changed — the compact preview moves to `transforms/diff/compact.rs`

Second step of separating the three concerns that shared `transforms/diff.rs`.
The token-bounded preview rendered for a pending change — `CompactDiffConfig`,
`compact_diff_plan`, `compact_diff_addressed`, the hunk builder and its
renderer, the edit-driven change-range derivation, and line truncation — now
lives in its own module, leaving `diff.rs` with the plan/apply layer and the
module wiring.

Verbatim motion again: no rendered preview byte changed. The three public names
are re-exported from `transforms::diff`, so every existing call path still
resolves as it did, and the rest of the module is `pub(super)` or private.
Nothing written into an index segment is affected, so cached segments stay
valid.
## [0.139.43] — 2026-07-24 — refactor: give the diff math its own module

### Changed — LCS and hunk assembly move to `transforms/diff/lcs.rs`

`transforms/diff.rs` had grown to hold three separate concerns: the layer that
turns edits into a plan and applies them, the token-thrifted compact preview,
and — underneath both — the diff computation itself. The last of those now
lives in its own module: the `ChangeRange` type, the line-level LCS
(`lcs_matches`, `gaps_from_matches`, `change_ranges`), unified-diff hunk
assembly (`build_hunks`, `render_hunk`), the byte-offset helpers
(`line_start_offsets`, `byte_offset_to_line`), in-place range merging
(`merge_change_ranges`), and the context-line constant they share.

The move is verbatim: no function body, no signature, and no rendered diff byte
changed. The moved items are `pub(super)`, so nothing new is reachable from
outside `transforms::diff` and every existing call path still resolves as it
did. Nothing written into an index segment is affected, so cached segments stay
valid.
## [0.139.42] — 2026-07-25 — test: finish splitting overlay_parity.rs into focused files

### Changed — overlay_parity.rs fully dissolved into single-concern files

- Split the last three clusters into small, focused files (test bodies
  unchanged): `overlay_parse_cache.rs` (parse-cache hit / LRU eviction /
  repeat-run speedup, 2 tests), `overlay_segment_layout_parity.rs` (segment
  path order and row-range contiguity plus index stats, 6 tests), and
  `overlay_validation_parity.rs` (dirty-deletion visibility and query
  validation, 5 tests).
- Removed the now-empty `overlay_parity.rs`. The original 4850-line suite is now
  eleven focused parity files sharing the `overlay_harness` test module.

## [0.139.41] — 2026-07-25 — test: split overlay find-node, persistence, and commit/rollback parity into their own files

### Changed — three more overlay parity clusters carved out of overlay_parity.rs

- Split into three small, focused files (each including the shared
  `overlay_harness` module), test bodies unchanged:
  - `overlay_find_node_parity.rs` — find_symbols regex-alternation matching and
    find_node handle resolution over the dirty overlay (3 tests).
  - `overlay_persistence_parity.rs` — external-edit freshness, file purge, and
    the delta write / reindex / simulated-restart round-trip (5 tests).
  - `overlay_commit_rollback_parity.rs` — rollback GC of orphaned staging
    segments and commit promotion into a fresh overlay (4 tests).

## [0.139.40] — 2026-07-25 — test: split overlay dirty-overlay parity into its own file

### Changed — overlay dirty-overlay parity carved out of overlay_parity.rs

- The six dirty-overlay parity tests (shadowing and union of uncommitted edits
  across find_symbols, find_usages, and symbol resolution, plus the re-index
  refresh) now live in a small, focused `overlay_dirty_parity.rs` that includes
  the shared `overlay_harness` module. Test bodies are unchanged.

## [0.139.39] — 2026-07-25 — test: split overlay segment-pruning parity into its own file

### Changed — overlay pruning / fast-path parity carved out of overlay_parity.rs

- The nine segment-pruning and fast-path parity tests (path-glob and
  short-prefix prefilters, zone-map range pruning, enrichment posting filters,
  the enrichment-only fast path, and their combined and negative cases) now
  live in a small, focused `overlay_prune_parity.rs` that includes the shared
  `overlay_harness` module. Test bodies are unchanged.

## [0.139.38] — 2026-07-25 — test: split overlay SHOW-surface parity into its own file

### Changed — overlay SHOW parity carved out of overlay_parity.rs

- The SHOW-surface parity tests (show_outline, show_body, show_signature,
  show_members, show_context, and show_callees) plus the bare-repo fallback
  test (source bytes read from the git blob when the file is absent from disk)
  now live in a small, focused `overlay_show_parity.rs` that includes the
  shared `overlay_harness` module. Test bodies are unchanged.

## [0.139.37] — 2026-07-25 — test: split overlay query-surface parity into its own file

### Changed — overlay query parity carved out of overlay_parity.rs

- The seven query-surface parity tests (find_symbols, kind prefilter,
  exact-name and LIKE filters, ORDER BY, enrichment-field filter, and
  multi-segment name lookup) now live in a small, focused
  `overlay_query_parity.rs` that includes the shared `overlay_harness` module.
  Test bodies are unchanged.

## [0.139.36] — 2026-07-25 — test: split overlay symbol-resolution parity into its own file

### Changed — overlay resolve parity carved out of overlay_parity.rs

- The three symbol-resolution parity tests (type-preference, body-symbol
  redirect, and deterministic resolution on duplicates) now live in a small,
  focused `overlay_resolve_parity.rs` that includes the shared `overlay_harness`
  module. Test bodies are unchanged. Continues breaking the oversized overlay
  parity suite into single-concern files.
- The remaining shared overlay builders (`single_segment_cpp_overlay`,
  `cpp_cached_parse`, `build_dirty_segment`, `build_dirty_segment_with_usages`)
  moved into `overlay_harness` too, so every future split file reaches them
  through the shared module.

## [0.139.35] — 2026-07-25 — test: extract a shared overlay parity harness

### Changed — overlay parity setup helpers moved to a reusable harness module

- The overlay/columnar parity suite's setup helpers (fixture indexing, segment
  building, result key-tuple flattening) now live in a dedicated
  `overlay_harness` test module that also re-exports the shared types. This lets
  the large parity suite be split into small, single-concern files without
  duplicating setup. No test behavior changes.
## [0.139.34] — 2026-07-24 — fix: point the missing-IF-REV hint at a handle FIND NODE can resolve

### Fixed — the "requires IF REV" error no longer suggests an unresolvable FIND NODE

When a mutation that needs `IF REV` is issued without one, the error tells you
to recover the current rev with `FIND NODE '<handle>'`. For a handle that
carries an offset or line-range suffix — `n<hex>(a-b)` (a file line range) or
`<id>(n)` (a node-relative offset) — `FIND NODE` rejects that form with
`invalid node_id format`, so the suggested command could not be run. The hint
now strips the suffix and names the addressable base handle, which `FIND NODE`
resolves: for a line-range handle that is the enclosing file handle, whose rev
is exactly the one the mutation gates on. Plain node handles are unaffected.
## [0.139.33] — 2026-07-24 — test: split the query surface into its own suite

### Changed — engine_queries is a new themed test file carved from engine_integration

Continuing the split of the large `engine_integration` suite, the read/query
surface moves into a new `engine_queries` suite: the `FIND symbols` /
`FIND usages` / `FIND globals` / `FIND variables` tests and the `SHOW body` /
`SHOW context` / `SHOW outline` / `SHOW members` tests, together with their
filter, ordering, pagination, projection, and result-shape cases, and the small
class-method fixture helper two of them share. The tests were relocated in whole
contiguous blocks so their doc comments and section headers travel with them; no
test body changed and each keeps its exact name. The shared session and exec
helpers already live in `tests/common`. After the move, the `Backend`/`Clauses`
IR imports and the extra-files session helper are no longer used in
`engine_integration` and are dropped there. Test inventory is unchanged: the
query cases left `engine_integration` and now run under `engine_queries`, none
lost or duplicated. Test-only — no engine or index-output change.
## [0.139.32] — 2026-07-24 — test: split the file-lifecycle cases into their own suite

### Changed — engine_file_ops is a new themed test file carved from engine_integration

Continuing the split of the large `engine_integration` suite, the seven tests
that exercise whole-file and directory lifecycle mutations — creating files and
directories (`INSERT NODE FOR`), renaming and moving them (`MOVE NODE … TO`),
copying them (`COPY NODE … TO`), and listing them (`FIND files`) — move verbatim
into a new `engine_file_ops` suite. No test body changed; the functions only
relocated, each keeping its exact name so `cargo test <name>` still selects it.
The shared helpers they use already live in `tests/common`. Test inventory is
unchanged: the seven cases left `engine_integration` and now run under
`engine_file_ops`, none lost or duplicated. Test-only — no engine or
index-output change.
## [0.139.31] — 2026-07-24 — test: split the bare-hex addressing cases into their own suite

### Changed — engine_addressing is a new themed test file carved from engine_integration

The `engine_integration` suite mixed many unrelated engine concerns in one large
file. This begins splitting it into navigable themed files: the ten tests that
exercise bare-hex file and directory handles and directory-rev tracking —
resolving, reading, deleting, changing, and inserting around files and
directories by their stable `n<hex>` handles — move verbatim into a new
`engine_addressing` suite. No test body changed; the functions only relocated,
each keeping its exact name so `cargo test <name>` still selects it. The shared
helpers they use already live in `tests/common`. Test inventory is unchanged:
the ten cases left `engine_integration` and now run under `engine_addressing`,
none lost or duplicated. Test-only — no engine or index-output change.
## [0.139.30] — 2026-07-24 — test: promote the shared engine-integration helpers into the common harness

### Changed — engine_integration's tuple-style helpers now live in tests/common

As a prerequisite for splitting the large `engine_integration` suite into
themed files, the helpers every test in it uses — `engine_with_session` and its
extra-files and legacy variants, plus `execute_fql`, `try_fql`, `fql_err`, and
`node_rev` — move into the shared `tests/common` module. Each `tests/*.rs` file
is its own crate, so a split-off file cannot see another file's private
helpers; hosting them in `common` lets every future themed file call them. The
helper bodies are unchanged; `engine_integration` now imports them from
`common` and every call site is otherwise untouched. The columnar/legacy
session constructors these adapters wrap already lived in `common`.
`syntax_coverage` keeps its own local `engine_with_session` (a different body)
and is not affected. Test-only — no engine or index-output change.
## [0.139.29] — 2026-07-24 — test: table-drive the single-symbol field-assertion enrichment families

### Changed — the metrics-flag, member-count, and member-kind families are now table-driven

Continuing the enrichment coverage cleanup, several families shared one shape —
look up a single symbol by name and assert one enrichment field on it. They move
onto a new `named_field_case!` macro: the metrics inline/const/volatile flags and
the struct/enum member-count checks, plus the member-kind and owner-kind checks.
Each generated test keeps its original name so `cargo test <name>` still selects
a single case, and every query string, looked-up name, and expected value is
preserved verbatim. Cases with a different shape — the member-count that parses
and compares, the two-field owner/kind check, and the scope filters — stay
standalone. The suite still runs on the legacy backend. Test-only — no engine or
index-output change.
## [0.139.28] — 2026-07-24 — test: table-drive the numeric-field and cast-safety enrichment families

### Changed — the field_num and cast_safety families are now table-driven

Continuing the enrichment coverage cleanup, the `field_num_*` family is fully
collapsed: the four "assert an expected name appears" cases (return-count,
branch-count, member-count, null-check-count) move onto the existing
`names_contains_case!` macro, and the four "parse a numeric field and check a
per-row bound" cases (name-length greater/less, condition-tests, lines) move
onto a new `field_num_bound_case!` macro that preserves each query, the parsed
field, the comparison, and the per-case non-empty expectation. The
`cast_safety_*` family's four uniform cases (c-style unsafe, static_cast safe,
reinterpret_cast unsafe, const_cast moderate) move onto `field_all_case!`. Cases
with a different shape — cast target-type presence, cast-count totals, and the
member/parameter kind checks — stay standalone. Every query string and expected
value is preserved verbatim, and each generated test keeps its original name so
`cargo test <name>` still selects a single case. The suite still runs on the
legacy backend. Test-only — no engine or index-output change.
## [0.139.27] — 2026-07-24 — test: table-drive the suffix-meaning and operator-category enrichment families

### Changed — the number suffix_meaning and operator_category families are now table-driven

Continuing the enrichment coverage cleanup, the `number_suffix_meaning_*` (u, ul,
ll) and `operator_category_*` (increment, compound arithmetic, compound bitwise,
shift) families move onto the `field_all_case!` table macro — one row per case,
each generating the same named test and asserting the expected value for the
named enrichment field on every returned row. Every query string and expected
value is preserved verbatim. The broader operator family (prefix/postfix
increment and decrement, compound-operator and shift-amount checks) has
non-uniform assertion shapes and stays standalone. The suite still runs on the
legacy backend. Test-only — no engine or index-output change.
## [0.139.26] — 2026-07-24 — test: table-drive the number-format and suffix enrichment families

### Changed — the number_format and number_suffix families are now table-driven

Continuing the enrichment coverage cleanup, the `number_format_*` (dec, hex, bin,
oct, float, scientific) and `number_suffix_*` (u, ul, ll) families move onto the
`names_contains_case!` table macro — one row per case, each generating the same
named test. Every query string and expected value is preserved verbatim. The
number edge-case tests (magic-number regressions, string/error-recovery
suppression, sign, numeric comparison) keep their per-case comments and stay
standalone. The suite still runs on the legacy backend. Test-only — no engine or
index-output change.

## [0.139.25] — 2026-07-24 — test: begin table-driving the enrichment coverage families

### Changed — the naming and comment enrichment families are now table-driven

The `enrichment_integration` suite had families of near-identical `#[test]`
functions that differed only in a query string and an expected value. Two reusable
table macros are introduced — `names_contains_case!` (assert the listed names
appear among the results) and `field_all_case!` (assert every returned row carries
the expected value for a named enrichment field) — and the naming-style and
comment families are moved onto them: one table row per case, generating the same
named test so `cargo test <name>` still selects a single case. Every query string
and expected value is preserved verbatim; cases with a different shape (a scalar
`name_length` check, a numeric comparison) stay standalone. The suite still runs on
the legacy backend. Test-only — no engine or index-output change.

## [0.139.24] — 2026-07-23 — test: collapse the WHERE / ORDER BY / LIMIT coverage families into tables

### Changed — repetitive one-test-per-case families are now table-driven

The `syntax_coverage` suite had families of near-identical `#[test]` functions
that differed only in a query string and a single comparison — one function per
`WHERE name` operator, per `WHERE usages`/`line` bound, per `ORDER BY` field, and
per `LIMIT`. Each family is now a small `macro_rules!` plus a data table: one
table row per case, generating the same named test so `cargo test <name>` still
selects a single case and a failure still names it. Every query string and
expected value is preserved verbatim; cases with a distinct assertion shape (a
determinism probe, an OFFSET comparison, an extra membership check) stay as
standalone functions. Test-only — no engine or index-output change.

## [0.139.23] — 2026-07-23 — fix: DELETE NODES FOUND removed the whole file instead of the armed nodes

### Fixed — bulk-deleting a subset of a file's nodes no longer deletes the file

Arming `DELETE NODES FOUND` with a `FIND symbols` result and deleting it removed
the entire containing file, not the armed nodes. The bulk delete mapped every
armed member to its containing file path and unlinked those whole files, so
deleting a handful of a file's functions destroyed the file and everything else
in it — silent data loss, recoverable only by `UNDO`.

The bulk delete now routes each member the way the single-node `DELETE NODE`
already does: a whole-file or directory handle (from `FIND files`) removes the
path, while a node handle (from `FIND symbols`) removes only that node's line
span, merged per file and applied as one atomic plan. Freed ordinals are
tombstoned so a byte-identical sibling cannot adopt a dead handle, matching
`CHANGE NODES FOUND`. A set that mixes whole-file and in-file handles is refused
before anything is removed rather than guessed at.

## [0.139.22] — 2026-07-22 — test: collapse the fql_kind coverage family into a table

### Changed — the fql_kind FIND coverage is now table-driven

The `syntax_coverage` suite had eight near-identical `#[test]` functions, one per
`fql_kind` value, each running `FIND symbols WHERE fql_kind = '<kind>'` and
asserting every returned row carried that kind. They are replaced by a single
`fql_kind_case!` macro that generates the same named tests from a compact table,
so adding a kind is one line instead of a fourteen-line copy. Each case stays its
own named test, and every query string and per-case expectation is preserved
unchanged — including the kinds the fixture may legitimately lack, which keep
their "may be empty" allowance. The one case with an extra assertion (the `macro`
kind, which also checks a specific name is present) stays a standalone test.
Test-only — the engine and its index output are unchanged.

## [0.139.21] — 2026-07-22 — test: remove the unused Python inline language double

### Removed — the in-crate Python test double, now that nothing uses it

`forgeql-core` keeps inline in-crate clones of its language support so its own
unit and integration tests can build a language registry without depending on
the external `forgeql-lang-*` plugins. Nothing references the Python clone any
more — every integration suite builds its registry from the real plugins, and no
in-crate test needs Python — so `PythonLanguageInline`, its `python_config`
helper, and the `python_node_text` helper are removed, together with the
`tree-sitter-python` dev-dependency and its slot in the `test-helpers` feature.
The C++ and Rust clones are retained by design: a handful of in-crate unit tests
both parse real source and assert on `pub(crate)` interning state, so they can
neither move to an integration test nor run on a fake language stub; a comment
atop `inline.rs` now records why. Test-only — the engine and its index output are
unchanged.

## [0.139.20] — 2026-07-22 — test: collapse the shared harness onto a single real-plugin registry

### Changed — the test harness now has one registry, built from the real language plugins

The shared `tests/common` harness carried two parallel registries during the
migration: one built from in-crate inline language clones and one built from the
real `forgeql-lang-*` plugins, each with its own set of session constructors.
Now that every integration suite runs on the real plugins, the inline
`make_registry` and its inline session constructors are removed, and the
real-plugin variants are renamed to the canonical names — `make_registry`,
`legacy_session`, `legacy_session_in`, `columnar_session`, and
`columnar_session_in`. Every suite calls those names and indexes with the
production plugins; no `tests/` file references an inline language clone. This is
a test-only change — the engine and its index output are untouched.

## [0.139.19] — 2026-07-22 — test: relocate the C++ language-behaviour tests to an integration suite

### Changed — the src/ast/lang.rs language tests now run against the real plugin

The `#[cfg(test)]` module in `src/ast/lang.rs` tested the C++ language
implementation — `map_kind`, `extract_name`, `config`, and registry resolution —
against the in-crate inline clone, because a crate's own unit tests cannot link a
`forgeql-lang-*` plugin (Cargo reports "multiple different versions of crate
forgeql_core"). Most of that module — the `map_kind`, `extract_name`, registry
resolution, and query-method accessor tests — moved to a new
`tests/lang_behavior.rs` integration suite, where it runs against the real
`forgeql-lang-cpp` `CppLanguage`. A `tree-sitter` dev-dependency was added for
the one test that builds a parse tree directly. Two tests that inspect
`LanguageConfig`'s private fields (config consistency and the C++ feature flags)
stay in-crate on the clone, because those fields are not public API and so are
unreachable from an integration test. The move surfaced one real divergence: the
inline clone claimed the `.c` extension for C++, but the production `CppLanguage`
resolves only `cpp`/`cc`/`cxx`/`h`/`hpp`/`hxx`/`ino` (a `.c` file is C, not C++),
so the extension-resolution assertion now matches the shipping plugin.

## [0.139.18] — 2026-07-22 — test: route the last two straggler suites through the real-plugin registry

### Changed — restore_scan_safety and sms now use make_registry_real

The `restore_scan_safety` and `sms_integration` suites built their registries
inline from the language clones. Both now build through the shared
`common::make_registry_real()`, so no `tests/` file constructs an inline-clone
registry any more. `restore_scan_safety` only needs a language to exist for its
filesystem-scan assertions; `sms_integration` keeps its deterministic-PRNG
harness and only its registry moved. This takes the entire `tests/` tree off the
inline clones. No drift surfaced.

## [0.139.17] — 2026-07-22 — test: index the benches and the sms suite with the real language plugins

### Changed — the columnar benches and the sms suite drop the inline clones

The `columnar_filter` and `columnar_range` benches and the `sms_integration`
suite built their registries from the inline clones. Each now imports
`CppLanguage` from `forgeql-lang-cpp` and `RustLanguage` from `forgeql-lang-rust`
and indexes through those production plugins. The sms suite keeps its bespoke
deterministic-PRNG harness untouched — only its registry construction moved to
the real plugins. With this, the only remaining references to the inline
`CppLanguageInline`/`RustLanguageInline` clones are the deliberately retained
inline `make_registry` in `tests/common` and the in-crate `#[cfg(test)]` unit
tests, which cannot use the real plugins directly. No drift surfaced.

## [0.139.16] — 2026-07-22 — test: index the overlay-parity suite with the real language plugins

### Changed — the overlay-parity suite drops the inline clones

The `overlay_parity` suite — the A/B harness that checks the columnar overlay
and segment path against the legacy merged view — referenced the inline clones
in one top-level import, ten function-local imports, and dozens of body sites.
All now resolve to `CppLanguage` from `forgeql-lang-cpp` and `RustLanguage` from
`forgeql-lang-rust`. Both sides of every parity comparison use the same real
plugin, so the legacy-versus-columnar parity holds. This was the last suite
under `tests/` to reference the inline language clones; every integration suite
now indexes through the production plugins. No drift surfaced.

## [0.139.15] — 2026-07-22 — test: index the direct-index suites with the real language plugins

### Changed — three low-level index suites drop the inline clones

The `find_row_cap`, `lang_coverage_integration`, and `segment_parity` suites
drive the index and columnar-segment layers directly (via `index_file` /
`index_canonical`) rather than through a session. They referenced the inline
clones by type name; each now imports `CppLanguage` from `forgeql-lang-cpp` and
`RustLanguage` from `forgeql-lang-rust` and indexes the canonical fixtures with
those production plugins. `segment_parity` compares legacy versus columnar
segment output — both A/B sides use the same real plugin, so the parity holds.
No drift surfaced for any of the three.

## [0.139.14] — 2026-07-22 — test: index the enrichment-integration suite with the real language plugins

### Changed — the enrichment field suite indexes through the real plugins

The `enrichment_integration` suite flipped its three legacy-backend engine
builders and its one inline registry construction from the inline-clone
`make_registry`/`legacy_session` to the real-plugin
`make_registry_real`/`legacy_session_real`. This suite asserts on every
enrichment field (naming, comments, numbers, control flow, operators, metrics,
casts, redundancy, scope, members, shadow, unused params, fallthrough,
recursion, todo) across a C++ fixture, so it is the most exacting check that the
inline clone and the real `CppLanguage` agree. They do: all 204 tests pass
unchanged against the real plugin — no field drifted. The suite stays on the
legacy backend because columnar local sessions do not yet carry enrichment
columns.

## [0.139.13] — 2026-07-21 — test: index the engine-integration and syntax-coverage suites with the real language plugins

### Changed — the two large columnar C++ suites index through the real plugins

The `engine_integration` and `syntax_coverage` suites flipped their columnar
session construction from the inline-clone `make_registry`/`columnar_session`/
`columnar_session_in` (and, in `engine_integration`, the legacy-backend
divergence helper) to the real-plugin `make_registry_real`/
`columnar_session_real`/`columnar_session_in_real`/`legacy_session_real`. Both
exercise the C++ plugin heavily — the full FIND/SHOW/CHANGE command surface and
every clause — so they now run against the same C++ plugin the deployed engine
ships. The shared harness gained `columnar_session_real` and
`columnar_session_in_real` to carry this. The inline-clone constructors stay for
the suites not yet flipped. No drift surfaced for either suite.

## [0.139.12] — 2026-07-21 — test: index the checkpoint-persist and commit-gate suites with the real language plugins

### Changed — the two git-based structural suites index through the real plugins

The `checkpoint_persist` and `commit_gate` suites flipped their engine
construction from the inline-clone `make_registry`/`legacy_session_in` to the
real-plugin `make_registry_real`/`legacy_session_in_real`. Both are
structural suites (checkpoint files and the pre-commit gate) that index a C++
fixture only as scaffolding, so they now build that scaffolding with the same
language plugins the deployed engine ships. The inline-clone constructors stay
for the suites not yet flipped. No drift surfaced for either suite.

## [0.139.11] — 2026-07-21 — test: index the multilang-resolve suite with the real language plugins

### Changed — cross-language resolution now runs against the real C++ and Rust plugins

The shared `tests/common` harness gained real-plugin session constructors —
`legacy_session_real` and `legacy_session_in_real` — that mirror
`legacy_session`/`legacy_session_in` but index through `make_registry_real()`.
The `multilang_resolve_integration` suite, which checks ambiguous-symbol
resolution across a C++ and a Rust canonical fixture, flipped to them, so its
disambiguation assertions run against the same C++ and Rust plugins the deployed
engine ships. The inline-clone constructors stay for the suites not yet flipped.

## [0.139.10] — 2026-07-21 — test: index the reconnect-dirty suite with the real language plugins

### Changed — the shared harness gains a real-plugin registry, and reconnect_dirty uses it

The shared `tests/common` harness gained `make_registry_real()`, which builds
its registry from the production `CppLanguage`, `RustLanguage`, and
`PythonLanguage` plugins (plus the text languages) rather than the in-crate
inline clones the integration suites have indexed against until now. The
`reconnect_dirty` suite — which constructs its engines directly — now indexes
through `make_registry_real()`, so its git-reindex assertions run against the
same language implementations the deployed engine ships. The inline
`make_registry()` stays for the suites not yet flipped, so they can move over
one at a time.

## [0.139.9] — 2026-07-21 — test: migrate the reconnect-dirty and change-integration suites onto the shared harness

### Changed — two more suites drop their private registry/fixtures copies

The `reconnect_dirty` and `change_integration` suites dropped their private
`make_registry` and `fixtures_dir` copies in favour of the shared `tests/common`
harness. `reconnect_dirty` routes its four inline engine constructions to
`common::make_registry()` and its source-repo fixture copy to
`common::fixtures_dir()`, keeping its bespoke git-repo and branch helpers local.
`change_integration` builds a `Workspace` (not a session), so it keeps
`setup_workspace` but delegates the fixture copy to `common::copy_fixtures`.
Pure motion: all three reconnect-dirty tests and all eight change-integration
tests are unchanged.

## [0.139.8] — 2026-07-21 — test: migrate the enrichment-integration suite onto the shared harness

### Changed — the enrichment-integration suite now boots sessions through `tests/common`

The `enrichment_integration` suite dropped its private `make_registry`
(constructed inline), `fixtures_dir`, and local `as_query` copies in favour of
the shared `tests/common` harness. Its three engine builders
(`engine_with_session`, `engine_enrichment_only`, `engine_bug2b_only`) now
delegate to `common::legacy_session` — this suite stays on the legacy backend
because columnar local sessions do not yet carry enrichment columns, and that
constraint is now documented on the helpers block. The `as_query` extractor
routes to the shared harness; the enrichment-specific assertion helpers
(`find_by_name`, `names`, `field`, `field_opt`) stay local. The `(engine,
session_id, TempDir)` tuple idiom is preserved via `TestSession::into_parts`,
leaving every test body unchanged. Pure motion: all 204 tests and their
assertions are unchanged.

## [0.139.7] — 2026-07-21 — test: migrate the syntax-coverage suite onto the shared harness

### Changed — the syntax-coverage suite now boots sessions through `tests/common`

The `syntax_coverage` suite dropped its private `make_registry` and
`fixtures_dir` copies and its inline session-and-registry construction in
favour of the shared `tests/common` harness. `engine_with_session` now
delegates to `common::columnar_session`; the git-backed
`engine_with_git_session` keeps its local repo setup but delegates
fixture-copying and registration to `common::copy_fixtures` /
`common::columnar_session_in`. The `as_query`, `as_show`, and `as_mutation`
result extractors moved into the shared harness (alongside a now-public
`copy_fixtures`) so every suite can share them. The `(engine, session_id,
TempDir)` tuple idiom is preserved via `TestSession::into_parts`, leaving every
test body unchanged. Pure motion: all 189 tests and their assertions are
unchanged.

## [0.139.6] — 2026-07-20 — test: migrate the engine-integration suite onto the shared harness

### Changed — the engine-integration suite now boots sessions through `tests/common`

The `engine_integration` suite dropped its private `make_registry`,
`fixtures_dir`, and `path_handle` copies, along with the two bespoke
engine-and-session builders that duplicated the shared harness. It now boots
every session through `tests/common`. To keep the suite's `(engine,
session_id, TempDir)` tuple idiom — and so leave every test body unchanged — a
new `TestSession::into_parts` hands back the three owned pieces. Pure motion:
all 92 tests and their assertions are unchanged.

## [0.139.5] — 2026-07-20 — test: migrate commit_gate onto the shared harness

### Changed — the commit-gate suite now uses the shared session helpers

`commit_gate` dropped its private `make_registry`, `exec`, and `try_exec` copies
in favour of the shared `tests/common` harness. Its bespoke setup (a git repo
carrying a commit-gated `.forgeql.yaml`) keeps a local `gated_session` helper,
which now returns a `TestSession`; the blocking `exec_blocking` /
`try_fql_blocking` variants drive the gate/job statements, and the filesystem
assertions read the workspace through `workspace()`. The restart test rebuilds
the engine in place over the same `TestSession` workspace. Pure motion: all
fifteen tests and their assertions are unchanged.

Test infrastructure only — no index output change, so the enrichment cache
version is unchanged.

## [0.139.4] — 2026-07-20 — test: migrate checkpoint_persist onto the shared harness

### Changed — the checkpoint suite now uses the shared session helpers

`checkpoint_persist` dropped its private `make_registry`, `fixtures_dir`, and
`exec` copies in favour of the shared `tests/common` harness. Its git-repo
setup keeps a local helper, but that helper now returns a `TestSession` whose
`Drop` frees the temp workspace, and the engine-level tests inspect the
`.forgeql-checkpoints` file through the new `workspace()` accessor. Pure motion:
the four tests and their assertions are unchanged.

Test infrastructure only — no index output change, so the enrichment cache
version is unchanged.

## [0.139.3] — 2026-07-20 — test: prove the shared harness API before rolling it out

### Added — a smoke suite that exercises every shared-harness helper

Before migrating the remaining integration suites onto the shared
`tests/common` harness, its API is now proven end to end by a dedicated
`common_smoke` suite. It drives `legacy_session` and `columnar_session` (both
backends), `columnar_session_in` over a hand-built workspace, the `exec` /
`exec_blocking` / `try_fql` / `try_fql_blocking` / `err` query paths, and the
`file_handle` / `path_handle` / `workspace` accessors. Several of these helpers
were unreferenced by any suite, so a regression in them would have passed the
gate unnoticed.

`TestSession` also gains blocking `exec_blocking` / `try_fql_blocking` variants
(for suites whose statements spawn gate or verify jobs) and a `workspace()`
accessor (for tests that inspect on-disk session artifacts such as the
checkpoint file).

Test infrastructure only — no index output change, so the enrichment cache
version is unchanged.

## [0.139.2] — 2026-07-20 — chore: shared integration-test harness

### Changed — one place for the test registry, session setup, and teardown

The `forgeql-core` integration suites each carried their own copy of the same
fixture-loading, registry-building, and session-registration boilerplate, so the
language set and backend choice could silently drift between suites. A new shared
`crates/forgeql-core/tests/common/mod.rs` module now owns them in one place:

- `make_registry()` — the single language registry every suite builds from.
- `legacy_session()` / `columnar_session()` — temp-workspace setup that copies
  fixtures and registers a session on the legacy or the columnar backend.
- `TestSession` — an RAII guard whose `Drop` frees the temp workspace at end of
  scope, even on panic, plus `exec` / `try_fql` / `err` / `file_handle` helpers.

The `forgeql-lang-cpp`, `forgeql-lang-rust`, and `forgeql-lang-python` crates are
added under `[dev-dependencies]` (mirroring the existing `forgeql-lang-text`
entry) so the shared registry can later be pointed at the real language plugins.
The `multilang_resolve_integration` suite is migrated onto the shared harness as
the pilot, deleting its duplicated helpers.

Test infrastructure only — no index output changes, so the enrichment cache
version is unchanged.

## [0.139.1] — 2026-07-19 — refactor: split the result module by response family

### Changed — the result DTO catalog is now one file per response family

`result.rs` had grown into a single ~1150-line catalog of every response
struct. Its data types now live in a `result/` submodule directory — `query`,
`show`, `mutation`, `source_ops`, `transaction`, `jobs`, and `diff_patch` —
each holding the structs for one response family. `result.rs` keeps the shared
`ForgeQLResult` enum, the small display/path helpers, and re-exports every
moved type, so every `crate::result::…` path is unchanged.

Pure code motion: no struct, field, derive, or serde attribute changed, so the
JSON/CSV wire format is byte-for-byte identical and the enrichment cache
version is unchanged.

## [0.139.0] — 2026-07-19 — fix: cap the node body echoed in a rev_mismatch error

### Fixed — a stale-rev refusal no longer dumps a whole file into the error

When a `CHANGE NODE … IF REV` (or `DELETE NODE … IF REV`) guard fails, the
`rev_mismatch` payload hands back the node's current source so the agent can
re-target without a second read. For a small statement that source is
invaluable; for a whole-file node it embedded the entire file — hundreds of
lines, thousands of tokens — in a single error body.

- `current_content` is now capped: a node within a 40-line budget is echoed
  verbatim (unchanged for statement-sized nodes), while anything larger is
  elided to its first 24 and last 8 lines with a note reporting how many lines
  were dropped and pointing at `SHOW NODE '<id>'` for the full text.

This changes error-payload text only; no index output changes, so the
enrichment cache version is unchanged.

## [0.138.0] — 2026-07-19 — fix: SHOW MORE returns readable source, not a re-encoded blob

### Fixed — paging a buffer no longer double-encodes its own output

Paging a truncated response with `SHOW MORE` replayed the buffered text back
through the CSV field writer, so every field was quoted a second time and the
buffered column header resurfaced as a spurious data row. An agent that followed
the paging footer to read the rest of a construct got an unreadable
double-encoded blob instead of source lines.

`SHOW MORE` now hands the buffered window back verbatim — the exact bytes the
original response showed, windowed to the requested range — with the paging
footer preserved. This change alters no index output, so the enrichment cache
version is unchanged.

## [0.137.0] — 2026-07-19 — fix: SHOW NODE and SHOW LINES now carry the rev an edit needs

### Fixed — read-then-edit no longer needs a second lookup

Every mutation that names an existing node requires `IF REV '<rev>'`, but
`SHOW NODE` and `SHOW LINES` reported no rev on the lines they returned. The
natural `SHOW NODE` → `CHANGE NODE` flow therefore forced a separate `FIND`
just to learn the rev of code already on screen.

- Each returned line that resolves to an addressable node now carries that
  node's `rev`. In JSON it is a per-line `rev` field; in the compact per-line
  view it is a new `rev` column, printed once per node — on the node's first
  line — to keep multi-line reads cheap. It is the same value `FIND` and
  `FIND NODE` report, so it feeds `CHANGE NODE 'id(off)' IF REV '<rev>'`
  directly.
- The rev is resolved through the same node lookup the mutation layer uses, so
  a block-surfaced handle reports the block's rev rather than a member's.

This changes query output only; no index output changes, so the enrichment
cache version is unchanged.

## [0.136.0] — 2026-07-19 — fix: reject an unsortable ORDER BY field on FIND symbols

### Fixed — ORDER BY on a non-symbol field no longer silently reorders

`FIND symbols … ORDER BY size` — and any other field that carries no
per-symbol value, such as `depth` — was silently ignored. The rows came
back in alphabetical name order while the requested column was still
printed in the output, so an agent reasoning over "the top N by size"
was handed unrelated rows with no error or warning. `size` and `depth`
describe files and outline entries, not symbols, so they never resolve
on a symbol row and every row tied and fell back to the name tie-break.

The columnar backend now rejects an `ORDER BY` field that is neither a
sortable symbol field, a known enrichment field, nor a materialised
column. The error names the field and points `size`/`depth` at
`FIND files`; to rank functions by span, order by an enrichment metric
such as `lines`. This matches the validation the legacy backend already
performed — the columnar (production) path simply never ran it.

## [0.135.0] — 2026-07-19 — fix: grouped `FIND symbols … GROUP BY file` CSV labels the file column

### Fixed — the grouped-CSV outer column names the actual GROUP BY key

`FIND symbols … GROUP BY file` produced a compact-CSV response that
contradicted its JSON. JSON returned clean `{path, count}` rows, but the CSV
labeled the outer column `fql_kind` (the `WHERE` field, not the grouping key),
left that key cell empty, and buried the file path inside per-row tuples with an
empty name and a zero line — so the count sat behind a column header that named
the wrong field entirely.

The cause: the query layer set the renderer's group-by field for every custom
field but deliberately excluded `file`, dropping `GROUP BY file` into the
group-by-kind rendering path meant for `GROUP BY fql_kind`. `file` is now carried
like any other grouping field (and resolves to the file path, as it does
everywhere else `file` and `path` are interchangeable), so the response is
`"file","[count]"` with one `path,count` row per file — the same data JSON
returns. `GROUP BY fql_kind` still uses its own richer per-kind layout, and other
grouping fields are unchanged.

This change is confined to the query and output layers, so no index output or
enrichment cache version changes.
## [0.134.0] — 2026-07-19 — fix: grouped `FIND usages` CSV now reports the count, matching JSON

### Fixed — every output format reports the same aggregate under GROUP BY

`FIND usages OF 'x' GROUP BY file ORDER BY count DESC` returned different data
depending on the output format. JSON carried the per-file `count` on every row;
the default compact CSV silently dropped it. The CSV renderer ignored the
counts the engine had already computed and re-collapsed the rows into a
`file,[lines]` shape — so each cell showed a lone representative line number
where the query had asked for a count, and the value the query was ordered by
never appeared at all. A line number is easily misread as a count, so the
blast-radius workflow the server instructions recommend returned quietly
misleading output.

Grouped `FIND usages` now renders `file,count`, reading the aggregate the engine
attached to each group — the same value JSON shows and the same column the
grouped `FIND symbols` renderer already emitted. Ungrouped `FIND usages` is
unchanged: it still collapses raw sites into a `file,[lines]` list.

This change alters no index output, so the enrichment cache version is unchanged.

## [0.133.0] — 2026-07-19 — fix: SHOW MORE no longer replays its own output

### Fixed — paging a buffer is now read-only

A bare `SHOW MORE` (and any `SHOW MORE` window large enough to be capped
again) fed its own rendered output back into the paging buffer. Three
things went wrong as a result:

- the buffer's ring rotated on every page, so a second `SHOW MORE` never
  advanced past the first window — it replayed the same opening lines;
- the already-rendered output was re-escaped each time it was shown as a
  buffered line, so quotes doubled, then quadrupled, on each replay;
- the injected header rows inflated the reported "N lines total" on every
  call.

`SHOW MORE` now opts out of buffering: paging an existing buffer is a
read, so it never rotates the ring or re-escapes its own output. A bare
`SHOW MORE` returns the full buffered result once, cleanly.

This change alters no index output, so the enrichment cache version is
unchanged.

## [0.132.0] — 2026-07-19 — feat: the onboarding coach adapts to the session and paces itself

### Added — proactive teaching, paced by mode and silenced by fluency

The optional onboarding coach previously spoke only when a command failed. It
now also teaches proactively: as an agent works, it surfaces the next protocol
skill the agent has not yet shown — connect, then locating and filtering, then
reading, then editing by handle and the `IF REV` contract — one short hint at a
time.

- **It follows the session.** A read-leaning session is taught reading and query
  skills; a session that is editing is taught the mutation contract, with `IF REV`
  surfaced ahead of enrichment trivia.
- **It paces itself and goes quiet.** Proactive hints fire below a per-skill
  recency threshold and on a cooldown, and stop entirely once the agent is fluent
  in everything relevant — a fluent replay sees nothing at all.
- **It notices two wasteful reading patterns.** Reading a file in many small
  adjacent `SHOW LINES` ranges, or hitting the line cap again and again without
  paging, each draws a one-time nudge toward reading a whole node at once.

### Changed — one channel for the fragmented-read nudge

The engine's built-in "sequential `SHOW LINES`" tip has been retired; the coach's
repeated-read detector above replaces it, so a single channel owns that topic.
That tip lived in the engine and fired even with the coach disabled; it no longer
exists there. Disabling the coach with `FORGEQL_COACH=0` now opts out of that
guidance too, by design.

This change alters no index output, so the enrichment cache version is unchanged.

## [0.131.0] — 2026-07-19 — fix: CLI output windowing, node-not-found rejections, and cap-hint ownership

### Fixed — the CLI pipe now windows oversized output

The CLI pipe rendered `SHOW` results in full — no inline cap, no `show_more`
footer, and no paging buffer — while the stdio and HTTP transports windowed
them. Oversized reads now cap and buffer identically on every transport, so
`SHOW MORE` pages a CLI result just as it does an MCP one.

### Fixed — a missing handle now reports a structured rejection

A node handle whose file resolves but whose ordinal does not exist (the common
"stale handle" case) returned an untyped error. It now returns the same
structured `node_not_found` rejection as the other resolution paths — the
message text is unchanged, but callers can now classify it like `rev_mismatch`
and the other self-healing rejections. The onboarding coach, in turn, can offer
its re-locate guidance for this case.

### Changed — the line-cap footer owns the "output capped" topic

A capped response already carries the `show_more` footer, which names the
cheaper alternatives (`SHOW NODE`, `SHOW MORE`, a lower `DEPTH`). The coach no
longer adds a second, redundant hint for a single capped read; that guidance now
comes from the footer alone. (The coach still observes capping, for a future
detector aimed at the case the footer cannot see: capping repeatedly without
ever paging.)

This change alters no index output, so the enrichment cache version is unchanged.


## [0.130.0] — 2026-07-19 — feat: the onboarding coach now teaches from failures

### Added — reactive protocol hints

The optional onboarding coach — silent since its introduction — now emits short,
just-in-time corrective hints when a command fails or a read is truncated. A
failure is concrete evidence of a protocol gap, so the hint rides the very
response that carries the error:

- An `IF REV` mismatch, an unresolved node handle, and the three bulk
  `NODES FOUND` refusals each get a targeted recovery sequence: re-read with the
  returned rev, re-locate the handle, or re-run the arming FIND for its master rev.
- Output that hits the line cap points at `SHOW NODE`, `SHOW MORE`, and lower
  `DEPTH`; a session low on line budget points at tighter reads.
- A statement that fails to parse gets a nearest-verb correction — whether its
  first word is a known verb, the clause order, and the connect-and-locate
  starting points. Parse failures never reach the engine's executor, so each
  transport (CLI, stdio, and HTTP) observes them through a dedicated hook.

Hints are opt-out with `FORGEQL_COACH=0` and never appear when the coach is
absent (library embedders and the test suites). This change alters no index
output, so the enrichment cache version is unchanged.

## [0.129.0] — 2026-07-19 — fix: self-healing rejections render consistently across transports

### Fixed — parseable rejections everywhere

A structured self-healing rejection — one the caller recovers from by looking
again (`rev_mismatch`, `node_not_found`) — is meant to come back as an
error-flagged result whose body is a JSON payload the caller parses. Two gaps
broke that promise:

- **The bulk `NODES FOUND` verbs returned opaque strings when they could not
  proceed** — no armed FIND, a truncated FIND (so no master rev was issued), or
  a missing `IF REV`. Each now returns a structured payload
  (`{"error":"no_found_set" | "found_truncated" | "found_refused", …}`) carrying
  the same recovery guidance, parseable exactly like `rev_mismatch` and
  `node_not_found`.
- **The HTTP server buried every rejection inside a protocol-error string.**
  It now hands structured rejections back as error-flagged tool results — the
  JSON payload the caller is meant to act on — matching the stdio transport.
  Plain precondition errors (missing session, invalid arguments) remain
  JSON-RPC errors on every transport.

- **Rejections now carry a typed kind end to end.** Each transport decides
  whether a failure is a parseable self-healing rejection or a plain
  precondition error from that typed kind, rather than by inspecting the
  message text — so the two categories cannot be confused as the error
  wording evolves.

## [0.128.0] — 2026-07-18 — feat: introduce an optional onboarding coach

### Added — an optional, decoupled onboarding coach

ForgeQL can now carry short, just-in-time protocol hints alongside command
responses, to help a caller become fluent without reading the syntax reference
up front. The coach lives in a separate crate behind a small trait the engine
owns, so the core takes on no dependency on it, and library embedders and the
test suites run without it.

- **Enabled by default; opt out with `FORGEQL_COACH`.** Set `FORGEQL_COACH` to
  `0`, `off`, `false`, or `no` to disable it entirely, which leaves the command
  path untouched.
- **Per-learner state persists across restarts.** The coach keeps a small
  per-branch cookie under `<data-dir>/coach/`, updated as commands run, so what
  it has taught survives a server restart.
- **Delivery.** When the coach emits a hint it rides the response as a trailing
  `coach:` line (CSV) or field (JSON), at most one per response. In this release
  the coach observes every command and records state but stays silent in normal
  operation; a diagnostic mode (`FORGEQL_COACH_DEBUG`) surfaces its bookkeeping.

## [0.127.0] — 2026-07-19 — fix(storage): columnar test sessions served an empty index; WHERE no longer breaks SHOW resolution

### Fixed — the columnar test helper silently produced an empty index

`register_local_session_with_columnar` reconstructed segment paths against
an obsolete storage layout (flat `{hex}.fqsf` files under an unversioned
provider directory) and swallowed every failed segment open, leaving a
columnar store with an overlay but no rows: symbol queries returned empty,
no error anywhere. The helper now installs columnar through the same
`warm_or_open` path a real `USE` takes, so the storage layer owns the
on-disk layout end to end, and any failure is a hard error. It also uses
the production `git_blob_sha1` content hash instead of a stand-in hasher,
and synthesizes an all-zero snapshot id for non-git workspaces
(`overlay_path_for` panics on an empty commit id).

### Fixed — WHERE on a SHOW no longer breaks symbol resolution

On the columnar backend, any filtered SHOW — body, callees, members —
failed with a false `symbol not found`: the WHERE filter meant for the
output rows was also applied to the symbol row during resolution, which
it can never match. Resolution no longer applies WHERE predicates; they
keep filtering the SHOW output as documented.

### Changed — integration suites exercise the columnar read path

`engine_integration` and `syntax_coverage` now register their local
sessions through the columnar shadow-write helper — the backend real
sessions serve reads from — instead of the legacy in-memory backend.
Two genuine backend divergences surfaced and are recorded in the tests:

- Unknown WHERE fields: legacy answered with an empty result plus a hint;
  columnar refuses with an error naming the field. The refusal is the
  behaviour real sessions exhibit, so the test now pins it.
- File revs: the rev handed out by `FIND files` on columnar does not
  round-trip through `IF REV` on the mutation layer; that test stays on
  the legacy backend until the two derivations agree.

`enrichment_integration` stays on the legacy backend entirely: columnar
local sessions do not carry enrichment columns yet (the inline segment
emit skips the enrichment post-pass), so enrichment behaviour currently
has test coverage only on legacy. Real worktree sessions were probed
live and do carry enrichment columns.

## [0.126.0] — 2026-07-19 — feat(session): idle sessions with no work reclaim faster

### Changed — a shorter TTL for sessions that did nothing

A session that has produced no work — no commits over its base and no
uncommitted changes — is now reclaimed after a much shorter idle period (2 hours
by default) instead of the full session TTL (48 hours). Sessions that carry
committed or uncommitted work keep the full TTL, and an explicit per-session TTL
override still takes precedence. Both the live idle-eviction and the startup
prune apply the same rule.

The base a session is compared against may now be a commit hash, not only a
branch name, so a session based on a commit is classified correctly.

The short TTL is configurable via `FORGEQL_SHORT_SESSION_TTL_SECS`.
## [0.125.0] — 2026-07-19 — feat(session): SHOW COMMITS lists a session's own commits

### Added — a session reports its commits for handoff

`SHOW COMMITS` lists the commits this session's branch carries over its base,
newest first — the abbreviated hash and the subject line, and little else.
Universal clauses apply (default `LIMIT 20`), so `WHERE`, `ORDER BY`, and `LIMIT`
all work. It is deliberately session-scoped and does not enumerate other
branches: an agent reports its own state with `SHOW COMMITS` and hands a hash to
the next agent, who bases a session on it directly.
## [0.124.0] — 2026-07-19 — feat(diff): SHOW DIFF OF a committed hash

### Added — review any commit, not only the pending worktree diff

`SHOW DIFF OF '<commit>'` renders the diff of a commit against its first parent,
in the same form as the pending-change `SHOW DIFF` — `STAT` for the file map and
`IN` / `EXCLUDE` / `WHERE` / `ORDER BY` / `LIMIT` all apply. Because the commit
lives in the shared repository, any session of the same source can review a
committed hash without checking it out.
## [0.123.0] — 2026-07-19 — feat(session): base a session on a commit, not only a branch

### Added — `USE source.<commit-hash>` and the resolved base in every USE response

`USE` now accepts a commit hash where it previously accepted only a branch
name: `USE source.<commit-hash> AS 'alias'` bases a session on that immutable
commit. The branch position accepts a 7–40 character hex token; resolution
tries a local branch of that name first and otherwise resolves the token as a
commit, so an existing branch always wins and a bare hash resolves when no such
branch exists.

Every `USE` response — for both the branch and the commit form — now reports
the `base_commit` it resolved to (the full commit hash), so a second session
can confirm exactly which commit it was based on.

## [0.122.0] — 2026-07-18 — fix(mutations): every removal form now retires the freed handle

### Added — JSON query results now carry `found_rev`

A `FIND` that arms a set returns that set's master rev, which a following
`... NODES FOUND` mutation must quote in `IF REV`. The rev was emitted only in
the CSV rendering, so a caller reading results as `format=JSON` had no way to
obtain it and could not use any `NODES FOUND` verb at all. The `found_rev`
field is now included in the JSON result whenever a set is armed.

### Fixed — `CHANGE NODES FOUND` now retires the handles it blanks away

`CHANGE NODES FOUND ... WITH ''` sweeps an empty replacement across the matched
nodes. Where the sweep blanked a construct's whole span it removed the
construct, but — like the other removal paths — left its node handle alive, so a
byte-identical sibling re-claimed the freed ordinal on the reindex and the dead
handle silently repointed to it. The sweep now retires the handle of every
member whose whole node span is left blank, by the same removed-range rule as a
delete and `CHANGE ... WITH ''`; a rename or partial replacement leaves a node
behind and keeps its handle.

### Fixed — `CHANGE ... WITH ''` now retires the emptied construct's handle

Emptying a construct's span with an empty replacement removes it, but its
node handle stayed alive, and a byte-identical sibling re-claimed the emptied
construct's ordinal on the reindex — so the dead handle silently repointed to
the sibling. `CHANGE ... WITH ''` is the delete form of CHANGE, so it now
retires handles by the same removed-range rule as a delete.

### Fixed — deleting by offset or line range now retires the deleted handle

Deleting a construct by a line offset or a line range removed its bytes but
left its node handle alive, because handle retirement was staged only for the
bare whole-node delete form. A byte-identical sibling could then be reached
through the dead handle, and because the two share a rev, `IF REV` could not
tell them apart — so a stale edit landed on the wrong construct silently.

Handle retirement is now derived from the byte range a delete removes rather
than from the verb or addressing form that removed it: every root construct
whose whole span falls inside the removed range has its handle retired.
Whole-node delete is unchanged, since its own span is exactly its range.

## [0.121.0] — 2026-07-16 — fix(index): a file no longer inherits a byte-identical file's parse or node identities

### Fixed — one file could be served another file's index data

A segment caches the result of indexing one file, and that result depends on the
file's bytes **and** on which parser its path selects. Segments were stored under
a key derived from the **content alone**, so any two byte-identical files shared
one segment — whichever was written first won, and the other file's segment was
silently discarded. Three separate failures came out of that single omission:

- **A file could report another language's parse.** `void twin() { int q = 3; }`
  is a valid C++ function and is not valid Rust. Byte-identical `a.cpp` and
  `b.rs` shared a segment, so after the `.cpp` file was indexed first the `.rs`
  file reported `language = cpp` and `fql_kind = function` instead of its own
  `error` rows. `FIND symbols … WHERE language = 'rust'` missed the file
  entirely, and `FIND symbols WHERE fql_kind = 'error'` — the documented way to
  learn a file is already unparseable **before** editing it — answered that a
  broken file was clean.
- **Editing one file emptied another.** The set of persistent segments hidden
  behind pending edits was keyed by content hash. Editing `a.cpp` hid *every*
  segment whose bytes matched `a.cpp`'s previous state, so an untouched
  `b.cpp` holding identical bytes lost all of its symbols: `FIND files` still
  listed it and `SHOW NODE` still read it, while every symbol query treated it
  as empty.
- **Node identities merged at COMMIT.** Two identical-bytes files can carry
  different node ordinals — one file's node may sit at ordinal 2 because it
  survived a sibling's deletion, while a freshly written file has the same node
  at ordinal 0. Promotion kept whichever segment arrived first, so after COMMIT
  one file's node ids silently became the other's, with no edit to that file.

Segments are now keyed by **(path, content)** everywhere they are written, read,
promoted, or hidden — the rule the per-session staging layer already followed
since 0.120.0, now applied to the committed store and to the fresh-build path as
well. Reuse across commits and across worktrees is unaffected: the same path with
the same bytes still resolves to the same segment. Only sharing between
*different paths* that happen to hold identical bytes is given up, which is what
made one file's data reachable through another's.

- **`ENRICH_VER` 34 → 35.** Segment and overlay trees are namespaced by this
  version, and v34 trees are laid out under the old content-only names, so they
  cannot be resolved by the new key. The first `USE` after upgrading re-indexes.
- **Upgrading discards a session's pending edits from the index, not from disk.**
  Uncommitted work is written to the worktree, and re-indexing reads the worktree,
  so nothing is lost — but the artifacts describing that pending state are not
  carried across. The staging area no longer resolves segments staged under the
  previous content-only name, and the per-session delta file carries a format
  version and is refused if it was written by an older engine (its removal set
  recorded content hashes, which the new one would have read as paths — a silent
  misread that would have resurrected a deleted file's symbols).

## [0.120.0] — 2026-07-16 — fix(index): a reindexed file no longer adopts node identities cached for identical bytes

### Fixed — stale node handles could silently re-point at a byte-identical surviving sibling

- The per-session reindex cache stored each reindexed file's segment under a key
  derived from the file's **content alone**. Two files with identical bytes —
  or one file whose bytes, after a deletion, came to match a state already
  indexed for *any* file earlier in the session — were therefore served the
  cached segment verbatim, **skipping ordinal remapping and the removal
  tombstones entirely**. Node ordinals are file-history-dependent identity, not
  content-derived data: after deleting one of two byte-identical sibling nodes,
  the survivor could adopt the deleted node's node_id, and because byte-identical
  twins share a rev, the stale handle + rev pair kept resolving — the one case
  the `IF REV` guard exists to prevent. The longer a session ran, the more
  cached contents accumulated and the more likely the collision.
- Staged segments are now keyed by **(path, content)**, so one file can never be
  served another file's node identities, and a reindex carrying removal
  tombstones always re-runs the ordinal remap instead of reusing a cached
  segment — a removal is an identity change even when the resulting bytes match
  a previously indexed state of the same file. Same-path/same-bytes reuse is
  preserved where it is sound: `UNDO` still restores a file's original node ids.
- Golden `segment_cache_does_not_serve_another_files_ordinals` pins the exact
  scenario (seed file arms the cache; twin delete in a second file must kill the
  stale handle). A remapper unit test additionally locks root-only tombstoning
  of a node that carries a child subtree.
- Migration: a session whose staged segments predate this release is still
  readable — reconnect and commit fall back to the legacy content-only file
  name when the new one is not on disk, so uncommitted staged state survives
  the upgrade.

## [0.119.0] — 2026-07-15 — refactor(engine): one module per mutation verb family; MOVE NODE absorbs the trailing blank separator

### Changed

- The mutation-verb implementation, previously a single ~1,900-line source
  file, is split into eight focused modules under `engine/exec_change/`:
  `raw_text` (`CHANGE FILE` and `COPY`/`MOVE LINES`), `change`, `insert`,
  `delete`, `relocate`, `found` (the FOUND set and its bulk verbs), `plan`
  (the shared plan → apply → reindex → diff pipeline and UNDO), and `resolve`
  (handle → span resolution and the `IF REV` guards). Internal reorganization
  only: every verb, error message, and result shape is unchanged.

### Fixed — MOVE NODE no longer accumulates blank lines in the source file

- Moving a node out of a file left its trailing blank separator behind, so
  repeated `MOVE NODE` operations piled up consecutive blank lines in the
  source — enough to fail `cargo fmt --check` on a file the agent never
  hand-edited. `DELETE NODE` already absorbed the trailing blank run; the
  removal half of a move did not. The two removal paths now share one policy:
  `absorb_trailing_blank_lines` lives in the transforms layer, and
  `plan_move_lines` takes the removed range separately from the moved payload.
  Whole-node moves (`MOVE NODE`, `MOVE NODE … TO`) absorb the trailing blank
  run exactly like `DELETE NODE`; the payload spliced at the destination is
  still the node's exact span, and the line-addressed `MOVE LINES` verb stays
  byte-exact. Offset sub-range moves (`'<id>(n-m)'`) are line-addressed and
  also stay exact.

## [0.118.0] — 2026-07-15 — fix(index): stale node handles die instead of re-pointing at a byte-identical sibling

### Fixed

- Deleting one of two byte-identical sibling nodes no longer silently transfers
  the deleted node's handle to the survivor. The ordinal remapper reused the
  lower ordinal on its min-ordinal tiebreak, so after deleting the first of two
  identical siblings the second adopted the deleted node's `node_id` — and
  because the two spans are byte-identical they also share a `rev`, so an
  `IF REV` guard aimed at the deleted node passed and silently mutated the
  survivor instead. A node-removal verb (`DELETE NODE`, `MOVE NODE` away) now
  tombstones the removed root ordinal for the reindex it triggers: the surviving
  sibling keeps its own ordinal, and the deleted handle resolves to
  `node_not_found` instead of re-pointing at a live node. Bumps `ENRICH_VER`
  (33 → 34) because a reindex after such a removal now stores different ordinals.

## [0.117.0] — 2026-07-15 — feat(mutations): a mutation that breaks a structured file is flagged

### Added

- **`structural_errors` on every mutation result.** When an edit leaves a
  touched structured-text file unparseable under a strict, format-native parser,
  the mutation result now names the file and carries the parser's diagnostic
  (with line and column), so an agent learns *at edit time* — not later in the
  pipeline — that it broke the file. Each entry records whether the file parsed
  cleanly before the edit, distinguishing a break this edit introduced from one
  that was already there. A mutation whose touched files all still parse (or have
  no strict validator) reports nothing.

  JSON is checked with a strict RFC-8259 parser. This is deliberately not built
  on tree-sitter's `error` regions: tree-sitter is error-tolerant and recovers a
  tree from broken input, so a common defect such as a missing comma leaves no
  top-level error at all and slips through. A real parser catches every such case
  with a precise location. The `.jsonc` dialect (comments, trailing commas) is
  exempt, since a strict JSON parser would wrongly reject it.

- **A `validate_source` hook on the language-support interface** lets any
  language plugin supply a strict well-formedness check. Languages without one
  simply never report a structural break, so the core stays language-agnostic.

### Added — extended to YAML, TOML and XML

- **Strict validators for YAML, TOML and XML**, joining JSON behind the same
  `validate_source` hook. A mutation that leaves any of these files unparseable
  is reported in `structural_errors` with the parser's diagnostic, exactly as for
  JSON. Each catches the corruptions tree-sitter recovers from and hides — a
  reshaped YAML indent, a TOML key with no value, an XML tag that does not match
  its opener.

  - YAML (`yaml`, `yml`) — the `saphyr` YAML 1.2 parser.
  - TOML (`toml`, and `Cargo.lock`) — the `toml` crate.
  - XML (`xml`, `arxml`, `xdm`, `epc`, `epd`, `ecuc`, `odx`) — `quick-xml`
    well-formedness (balanced tags and valid syntax; not schema/DTD validity),
    applied uniformly to every dialect.

  Formats with no strict grammar (Markdown, reStructuredText, INI, Make, CMake,
  just, DBC) remain unvalidated and report nothing, as before.

## [0.116.0] — 2026-07-15 — fix(enrich): condition skeletons keep the assignment operator

### Fixed

- The normalized condition skeleton (`condition_text`) no longer dissolves an
  assignment inside a condition into a bare operand. An assignment written where
  a comparison was meant — `if ((x = a + b) > 0)` — is the single most
  defect-shaped token a condition can hold, yet the skeleton reported `((a)>b)`,
  showing no assignment at all and silently contradicting the
  `has_assignment_in_condition` flag reported beside it. The walker now keeps the
  `=` and folds the assigned value to one operand, so the condition normalizes to
  `((a=b)>c)`: the smell is visible in the skeleton an agent reads to confirm it,
  and dup-shape comparison still works. Comparison and boolean operators are
  unchanged, and arithmetic outside an assignment is still preserved.

## [0.115.0] — 2026-07-15 — feat(dsl): SHOW VERSION

### Added

- **`SHOW VERSION`** reports the crate version compiled into the running
  binary. Session-independent and exempt from budget logging. A long-lived
  server (or a stdio MCP process that outlived a binary reinstall) can now be
  interrogated in one statement instead of inferred from behaviour.

### Fixed

- Commits made after a `FIND` no longer include `.forgeql-foundset`. The
  armed-set persistence file introduced with `FOUND` was missing from the
  clean-commit exclusion list, so it leaked into user-facing commits.

## [0.114.0] — 2026-07-15 — feat(dsl)!: FOUND bulk mutations under one master rev; IF REV mandatory on existing-node verbs

### Added — `SHOW outline` and `SHOW members` rows carry their rev (and members, their handle)

The "handle and rev travel together" rule now holds on every read surface:

- **`SHOW outline`** rows already had a `node_id`; they now carry its `rev` beside it. Schema:
  `[fql_kind,name,line,node_id,rev]`.
- **`SHOW members`** rows had **neither** — a member row was effectively read-only, and an agent
  that wanted to edit a field had to go back and `FIND` it by name first. They now carry both.
  Schema: `[declaration,line,node_id,rev]`.

Members are read from the AST, not the index, and the two disagree about a member's *line*: the
indexed `field` node starts at the attribute or doc-comment line **above** the declaration, so an
exact-line match finds nothing. Handles are therefore attached by **byte containment** — the
member's first byte lies inside exactly one innermost indexed node — via a new
`StorageEngine::find_node_id_at_byte`. That is a relation that actually holds between the two
views, not a fuzzy line-offset guess. Backends without byte
spans return `None` and the row simply carries no handle, as before.

Covered by the `member_and_outline_rows_carry_a_usable_handle_and_rev` golden case
(`tests/golden/node_mutations.json`), which reads a handle+rev off a member row and mutates through
it. The in-process `engine_integration` harness cannot cover this: it runs the legacy AST path,
which has neither revs nor byte spans, so an assertion there would only ever be false.

Writing that case exposed a hole in the v2 golden client: a structured self-healing rejection
(`rev_mismatch`, `node_not_found`) comes back as an error-**flagged** tool result whose text is
valid JSON — not as a JSON-RPC protocol error — so the client parsed it and reported success. Every
`assert: {"error": true}` on such a step would have passed vacuously. The client now honours
`result.isError`.

### Changed — **BREAKING**: `IF REV` is mandatory on every verb that names an existing node

`CHANGE NODE`, `CHANGE NODE … MATCHING`, `INSERT BEFORE|AFTER NODE`, `DELETE NODE`, `MOVE NODE`
(both forms) and `CHANGE NODES FOUND` now **require** `IF REV`. Ungated, they are refused with a
message naming the handle and where its rev came from.

It costs nothing, because **the handle and its rev now travel together**:

- `FIND symbols` rows carry `rev` beside `node_id` (they already did on `FIND files`).
- Every mutation returns `new_rev` beside `new_node_id`, so a second edit on the same node — or a
  write into a file `INSERT NODE FOR` just created — needs no re-read.

**Why.** A handle is *stable*: it survives edits, insertions and even re-parenting, and it never
silently comes to mean a different node (a deleted node's handle errors; it does not get reused).
That is exactly what makes the gate necessary. An agent can carry a handle across dozens of
commands, come back, and the handle will still resolve — while the code under it has moved. A rev
is the SHA-256 of the node's **whole span**, so an edit to any *child* changes the enclosing node's
rev too: touch a comment inside a function and the function's rev moves. Nothing but the rev can
tell the agent that the node it remembers is not the node that is there. A stale rev is refused
with `rev_mismatch`, which hands back the current rev, span and source — enough to re-target on the
spot.

Creation stays ungated — there is nothing yet to fingerprint, and it cannot clobber:
`INSERT NODE FOR`, `COPY NODE … TO`, `COPY NODES FOUND TO`, and `INSERT … NODE '<file_hex>'` (the
BOF/EOF append form).

### Changed — **BREAKING**: `LAST` is now `FOUND`, and the noun is always plural

`LAST` collided with the *history* axis it shares the grammar with (`SHOW MORE LAST-1`,
`UNDO LAST-2`, `EXPORT PATCH LAST 3`), which counts backwards through time. The FIND-result set is
a different idea, so it gets a different word — the past participle of the verb that produced it:

```sql
CHANGE NODES FOUND IF REV '<master>' MATCHING 'old' WITH 'new'
DELETE NODES FOUND IF REV '<master>'
MOVE NODES FOUND IF REV '<master>' TO 'archive/'
COPY NODES FOUND TO 'api/v2/'
```

The response field is `found_rev` (was `last_rev`); the session file is `.forgeql-foundset`.
`DELETE NODES FOUND` (singular) is gone: the set is always plural.

### Added — `FOUND`: mutating a whole FIND result under one rev

```sql
FIND usages OF 'oldName'                                    -- rows + found_rev: h9c…
CHANGE NODES FOUND IF REV 'h9c…' MATCHING 'oldName' WITH 'newName'

FIND files IN 'legacy/**' WHERE extension = 'c'             -- rows + found_rev: h4b…
DELETE NODES FOUND IF REV 'h4b…'                            -- unlink every member
MOVE NODES FOUND IF REV 'h4b…' TO 'archive/'                -- each keeps its basename
COPY NODES FOUND TO 'api/v2/'                               -- creation only, so ungated
```

`FIND` **is** the set-selection syntax — a query with precise filters already names the set, so
the bulk verbs address it as `FOUND` instead of carrying a second glob grammar. The rows a FIND
returns are saved in the session, and the response carries a **master rev**: a hash over every
member's `(handle, rev)`. Quote it back in `IF REV` and the mutation runs only if not one member
has moved since you looked — the set-level extension of the per-node `IF REV` contract. Unlike a
directory's membership rev, it covers **content** too, because `CHANGE NODES FOUND` edits content.

Every member is mutated in **one plan**: one boundary diff, one `UNDO` step, never half-applied.
An 80-file relocation is now two calls (`FIND files …` → `MOVE NODES FOUND … TO 'api/v2/'`).

Three refusals, in the order an agent hits them:

- **No set** — nothing armed, or the previous mutation cleared it.
- **A set you were never shown in full** — a FIND truncated by the default limit issues **no**
  master rev, and every FOUND verb then refuses. `FIND usages` capped at 20 of 500, swept, would
  otherwise rename 20 and report success; that failure mode is now impossible.
- **A set that moved** — the rev is re-derived from the live members at mutation time, so a
  cached rev proves nothing. The mismatch hands back **no** replacement rev on purpose: the only
  safe recovery is to re-run the FIND and look at the rows again.

### Changed

- **`CHANGE NODES FOUND` sweeps a node's whole span, not its declaration line.** Armed from
  `FIND symbols`, it now rewrites the entire function body; armed from `FIND usages` (whose rows
  are call sites, not nodes) it still rewrites exactly those lines. Overlapping member spans are
  merged, so no byte is edited twice.
- **`FIND files` arms `FOUND`** — it is a FIND, and its rows have carried handles and revs since
  0.113.0. `FIND symbols`/`FIND usages` arm it as before.
- **Every `FOUND` verb requires `IF REV`** except `COPY NODES FOUND`, which only creates.
- **`FIND files` responses carry `total`** — the pre-`LIMIT` row count, so a capped result is
  distinguishable from a complete one. `FIND symbols`/`usages` already reported it.

### Fixed

- **A `GROUP BY` result no longer leaves the previous set armed.** `record_find_sites` returned
  early when a result carried no `(path, line)` rows, so `FIND usages OF 'x'` → `FIND symbols
  GROUP BY file` → `CHANGE NODES FOUND` swept the **first** query's set — code the agent believed
  it had replaced. An aggregate row is a count with a filename on it: it addresses nothing, so it
  now clears `FOUND` rather than arming a set no verb can act on.
- **`FOUND` survives a server restart.** The set is written to `.forgeql-foundset` in the
  worktree when it changes and restored on reconnect, so the set survives a server restart between
  the FIND and the mutation. It is re-gated against live revs on use, so
  restoring it can only re-offer a target — never authorise a stale one.

## [0.113.1] — 2026-07-14 — fix(git): user-facing commits no longer include undo-ring files

### Fixed

- The final `COMMIT` squash could include `.forgeql-undo-<n>` runtime files.
  Checkpoint commits track those slot files on purpose (so `git reset --hard`
  restores the undo ring), but the squash's staging filter only inspects paths
  staged from the working tree — entries inherited from a checkpoint passed
  through into the user-facing commit. The squash now sweeps the index and
  drops every runtime-excluded entry, wherever it came from.

### Added

- Compound golden probe suites (`probes_zephyr`, `probes_pytorch`,
  `probes_rust`) exercising multi-enricher predicate stacks and multi-step
  capture cases against the frozen corpora.

## [0.113.0] — 2026-07-14 — feat(dsl): files and directories as addressable nodes; ROLLBACK cleans what it created

### Added — files and directories are addressable nodes (`n<hex>`)

```sql
FIND files IN 'legacy/**'                       -- every row: path, node_id, rev
SHOW NODE '<hex>'                               -- read a whole file
SHOW NODE '<hex>(12-40)'                        -- read lines 12–40 of it
INSERT AFTER NODE '<hex>' WITH '...'            -- append at EOF (works on a 0-byte file)
CHANGE NODE '<hex>' IF REV '<rev>' WITH '...'   -- overwrite a whole file
DELETE NODE '<hex>' IF REV '<rev>'              -- delete a file, or a directory's subtree
```

The `node_id` grammar always had a hole in it: `n<hex>.<ordinal>` addresses a node **inside** a
file, and the dotless form — the file itself — was rejected. So files were the one thing an agent
could see but not act on: `FIND files` handed back path strings, and the only whole-file delete
(`CHANGE FILE … WITH NOTHING`) had no `IF REV` guard at all. This closes the hole. Every row of
every FIND result is now a usable, version-stamped mutation handle.

- **Bare-hex handles** resolve to a whole file, or to a **directory** (a file and a directory can
  never share a path, so there is no new ambiguity). Nothing is stored: the node is synthesized
  from the path fingerprint and the bytes on disk, so there is no index cost and no `ENRICH_VER`
  trigger. Resolution lives in the storage trait, so every backend answers it.
- **`FIND files` gains `node_id` and `rev` on every row**, and lists **directories**, marked by a
  trailing slash on `path` (`src/`) — no new column, and `WHERE path LIKE '%/'` selects them.
  Revs are computed after `LIMIT`, so only the rows you actually see cost a read.
- **`IF REV` is mandatory** on whole-file `DELETE` and `CHANGE`, and on a whole-file `MOVE` source.
  A node edit can be corrected afterwards; deleting a file leaves nothing to re-read.
- **A file rev** is the SHA-256 of its bytes. **A directory rev** is a membership XOR over the
  paths of every file underneath it: it moves when the subtree gains, loses or renames a file
  anywhere, and deliberately not when a file's content changes — a recursive delete has to be
  gated on the membership you saw, not on bytes you never read.
- **`DELETE NODE '<file_hex>'` unlinks the file** rather than blanking its lines (which would have
  left a 0-byte ghost behind); a directory handle deletes its files in one atomic plan and then
  removes the emptied directories bottom-up.
- **`SHOW NODE '<hex>(k-m)'`** reads a file's line range, and **`SHOW outline OF '<hex>'`** outlines
  a file or lists a directory.
- **`INSERT BEFORE/AFTER NODE '<hex>'`** is prepend-at-BOF / append-at-EOF, and now works on an
  empty file — the create-then-write bootstrap.

### Added — creating, renaming and relocating paths

```sql
INSERT NODE FOR 'src/new_module.rs'                    -- create an empty file, get its handle
INSERT NODE FOR 'docs/'                                -- create a directory
MOVE NODE '<hex>' IF REV '<rev>' TO 'src/renamed.rs'   -- rename (the source is unlinked)
MOVE NODE '<hex>' IF REV '<rev>' TO '<dir_hex>'        -- move into a directory
COPY NODE '<hex>' TO 'api/v2/'                         -- copy, keeping the basename
COPY NODE '<hex>.<ord>' TO 'src/extracted.rs'          -- lift one node into a new file
```

Handles address what exists. Creation and renaming are the two operations that cannot start from
one — the destination has no fingerprint yet — so they take a path, and hand a handle back.

- **`INSERT NODE FOR '<path>'`** replaces the file-creation idiom nobody could discover
  (`COPY LINES 1-1 OF … TO …`). Paired with `INSERT AFTER NODE '<hex>'`, it is the
  create-then-write bootstrap. A trailing slash creates a directory instead — with the caveat,
  documented rather than papered over, that git does not track empty directories and the engine
  will not invent a `.gitkeep` for you.
- **`MOVE NODE … TO`** is the rename ForgeQL never had (it took a COPY LINES + DELETE dance).
  **`COPY NODE … TO`** is its ungated twin — a real task once copied 80 files one `COPY LINES` call
  at a time. The `TO` argument is a directory handle (basename kept) or a path (full rename); an
  existing destination is refused, never clobbered. A whole-file MOVE unlinks the source rather
  than leaving a 0-byte husk, and is gated by the same mandatory `IF REV` as DELETE.

### Fixed

- **`ROLLBACK` now removes files created inside the transaction.** `git reset --hard`
  restores *tracked* paths, and staging is deferred to COMMIT — so a created file was still
  untracked when the reset ran, and survived it, on disk and in the index. Created paths are now
  recorded per checkpoint and removed explicitly, deepest-first.

  The list is **persisted on every append**, not just at BEGIN. A session outlives the server: an
  agent can disconnect and reconnect hours later, and the ROLLBACK that consumes the list may run
  in a process that has restarted since. A list held only in RAM would come back empty and the
  created files would survive the rollback — the exact bug the list exists to prevent. Paths are
  stored worktree-relative. The checkpoint file version goes 1 → 2; an old file is discarded with a
  warning, as a corrupt one already was.

  Only what the engine created is removed. Not a blanket `git clean` — that would destroy the
  user's unrelated untracked files. And not "any empty parent directory", either: git does not
  track empty directories, so one that was already there is never restored by the reset, and
  deleting it would be unrecoverable.

## [0.112.0] — 2026-07-13 — feat(dsl): `MOVE NODE` — relocate a node by handle

```sql
MOVE NODE '<src_id>' [IF REV '<rev>'] (BEFORE | AFTER) NODE '<dst_id>'
```

The last verb the node-addressing model was missing. Until now, relocating code meant reading the
node, holding its text, `INSERT`ing it at the destination and `DELETE`ing the original — four steps,
a read round-trip, and a window where the file contained the node twice or not at all.

### Added

- **`MOVE NODE`** — relocation, not re-authoring. The node's bytes are lifted **verbatim** and
  spliced at the anchor. The delete and the insert land in **one atomic plan**, so the file is never
  briefly missing the node and a failure leaves nothing half-moved. Source and destination may be in
  different files. The response carries `new_node_id` (re-parenting changes `parent_ordinal`, so the
  node earns a fresh handle).

  It is a thin node-resolving wrapper over the existing `plan_move_lines`, which already handles
  same-file moves in both directions and cross-file moves — so `MOVE NODE` adds **no new byte-
  splicing logic**, and inherits its guard: an anchor inside the moved span is **refused** rather
  than silently corrupting the file.

  **The engine does not re-indent (P1).** On an indentation-sensitive format the seam is real: a node
  lifted from inside a block keeps its original leading whitespace. Guessing the right indent is
  exactly the kind of "smart" the engine refuses to be — the boundary diff shows the seam and the
  agent closes it with `CHANGE NODE '<new_id>(1-n)'`.

  `INTO` is deliberately **not** offered: "first child of a container" has no mechanical definition
  that holds across languages, and the engine will not guess one.

### Fixed

- **A same-file relocation reported its file twice.** `apply_plan` collected `files_changed` *before*
  `merge_by_file()`, and a same-file move arrives as two `FileEdit`s on one path — so `MOVE LINES`
  has always reported `2 file(s)` for a one-file move. Now merged first. Cosmetic, but the mutation
  result is the thing an agent reads back.

No index output changes, so `ENRICH_VER` stays 32.

## [0.111.3] — 2026-07-13 — feat(index): `error` rows are addressable — you can finally repair the damage

`error` has been emitted since 0.109.3 but was never in `is_addressable_fql_kind`, so every region
came back with an **empty `node_id`**: findable, and never repairable by handle. The documented
recipe — `SHOW NODE '<id>'` → `CHANGE NODE '<id>'` — could not run at all. That is half of what the
kind exists for.

The fix is one line. Landing it was not, because addressability **consumes ordinals**, which shifts
node_ids in every file that holds an error — and the legacy suite hardcoded node_ids *inside its
`fql` strings*. With stale pins the mutation cases fired at the wrong nodes: a `DELETE` removed the
wrong node and an `INSERT` landed in the wrong place, duplicating `thread_runq` and producing
failures that read like indexer bugs but were mis-targeting. `GOLDEN_UPDATE=1` could not repair it
either — it rewrites `expect_*` values, not the pins inside `fql`, so blessing would have recorded
the duplicate as the expected answer.

So the pins had to go first.

### Changed

- **`error` is addressable.** `FIND symbols WHERE fql_kind = 'error'` now hands out a real
  `node_id`; `SHOW NODE` reads the region and `CHANGE NODE` repairs it. The engine still never
  repairs anything itself (P1).

- **`ENRICH_VER` 31 → 32.** Ordinals are consumed, so node_ids move in any file with an error.

### Tests

- **`tests/golden.json` is now free of hardcoded node_ids.** The last pinned cases (`FN01`–`FN06`,
  `FN08`, `FF01`–`FF07`, and the `node_id` values baked into `kernel/priority_queues.c`
  expectations) are ported to the v2 suite, which captures every handle from a `FIND` at run time.
  `tests/golden/node_addressing.json` now carries four cases covering handle resolution across
  three files, the full mutation round-trip, and the v0.60.2 sequential-change duplication guard.
  A new kind can never mis-target this suite again.

- **`golden_test.rs::rows_of` now reads `/content/lines`.** It didn't, so `row_count` on any
  `SHOW NODE` / `SHOW body` / `SHOW context` result was **always 0**: an assert of 1 could never
  pass, and — far worse — an assert of 0 passed *vacuously*. A case that looked like a guard tested
  nothing. Same family as the `array_block` false green.

- `error_row_hands_out_a_usable_handle` captures an `error` row's `node_id` and resolves it. The
  previous suite only asserted that `error` rows *existed*.

## [0.111.2] — 2026-07-13 — feat(index): `error_scope` + `parse_coverage` — make `error` mean something

`error` (0.109.3) was emitted for every tree-sitter `ERROR` node, and 0.111.1 built file-level
triage on top of that. Both were **wrong about what an ERROR means**, and the docs said "damaged
files" and "syntax damage" — which would send an agent hunting for corruption in perfectly healthy,
idiomatic kernel C.

tree-sitter parses C **without running the preprocessor**. It cannot know that an unknown identifier
in declaration-specifier position is a macro, so `static ALWAYS_INLINE void f(void)` yields an
`ERROR` beside the return type — while `f` itself indexes perfectly as a `function` with correct
boundaries. This is not a tree-sitter bug and it is not damage; it is inherent to parsing C without
a preprocessor. Measured across Zephyr:

| `error_scope` | count | |
|---|---:|---|
| `nested` | 16 480 | inside a node that indexed fine — span intact, safe to edit by handle |
| `file` | 4 994 | loose at top level (file-scope macros the parser could not model) |
| `root` | **207** | **the file did not parse at all** |

An alarm that fires on 21 681 regions of healthy code is not an alarm. The 207 are the real signal.

### Added

- **`error_scope`** (`root` / `file` / `nested`) and **`error_bytes`** on `error` rows. Position and
  size only — the engine passes no judgement (P1). Derived from the tree via the language's own
  `extract_name`, so core gains no language knowledge (P2).

- **`parse_coverage`** on `FIND files` — percent of a file's bytes tree-sitter parsed (0–100).
  Integer because the clause engine compares as `i64`, so `WHERE parse_coverage < 50` works. This is
  the number that separates a macro-heavy but healthy header (~99) from a file whose extension lies
  (~0). Only outermost `ERROR`s are emitted, so spans never overlap and the per-file sum is exact.

### Changed

- **`has_error` / `error_count` now count only `root` regions** — i.e. the file did not parse as its
  declared language. Previously they counted every `ERROR`, which on Zephyr meant firing on
  essentially every macro-heavy C file. `FIND files WHERE has_error = 'true'` now returns the files
  that are genuinely not what they claim to be.

- Docs no longer call this "damage". `error` marks an **unparsed span**; use `error_scope` for the
  raw picture and `parse_coverage` for magnitude.

- `ENRICH_VER` 29 → **31**. (30 is BURNED: an abandoned mid-session draft that made `error`
  addressable wrote v30 segments before being reverted. Reusing 30 silently read those poisoned
  ordinals. A version is spent the moment any build writes segments under it.) This change adds
  FIELDS only — `error` stays out of `is_addressable_fql_kind`, no ordinals are consumed, and the
  zephyr golden node_id pins are unaffected, which the gate confirms.

### Unchanged by design

- Ragged CSV rows and duplicate JSON keys are **not** errors — they parse fine. They surface through
  block-group splitting.

## [0.111.1] — 2026-07-13 — feat(find): `FIND files WHERE has_error` — file-level error triage

### Added

- **`FIND files WHERE has_error = 'true'`** and **`error_count`** — file-level triage for syntax
  damage. `error` rows (0.109.3) find broken *regions*; these find the broken *files*, so an agent
  can check what it is about to mutate before it mutates it. This half was specified with the
  `error` kind but never built: `FIND files WHERE has_error = 'true'` silently returned zero rows
  while `FIND symbols WHERE fql_kind = 'error'` returned plenty.

  Both are **derived on demand**: they cost one indexed `fql_kind = 'error'` scan, so they are
  computed only when a clause names them, and the `error_count` column appears in the output only
  when you asked for it. A plain `FIND files` is unchanged and pays nothing. `error_count: None`
  therefore means *not asked for*, never *no errors* — an unpopulated entry deliberately matches
  neither `has_error = 'true'` nor `= 'false'`, so a query that never asked can never be read as a
  clean bill of health.

- **Guards for the node_id identity contract** (`tests/golden/structured_text.json`):
  `reorder_preserves_node_ids` swaps two sibling YAML steps and asserts the handle held for one
  still resolves to *its own* content, not to the sibling that took its slot; `no_positional_*`
  asserts no emitted `object`/`array` name ever ends in `[N]`. A node_id follows **identity**,
  never **position** — had any name encoded a slot (`steps[0]`), two swapped siblings would *trade*
  node_ids and a `DELETE`/`CHANGE` would silently hit the wrong node. Nothing tested this before.

### Known gap

- **`error` rows are emitted but not addressable.** `error` is missing from
  `is_addressable_fql_kind`, so every broken region comes back with an empty `node_id`: it can be
  found and never repaired by handle, which is half of what the kind exists for. The fix is a
  one-line addition, but it hands `error` rows ordinals and therefore shifts node_ids in every file
  that contains one — so it needs an `ENRICH_VER` bump **and** a regeneration of the hardcoded
  node_id pins in `tests/golden.json` (the FCN/FSD/FE cases). Held back to keep that migration on
  its own commit; see the note on `ENRICH_VER`.

## [0.111.0] — 2026-07-13 — feat(dsl): SHOW DIFF — see an uncommitted change

### Added

- **`SHOW DIFF [STAT] [clauses]`** — the session worktree's **uncommitted** diff
  against `HEAD`, returned inline.

  `EXPORT PATCH` exports *committed* work only ("uncommitted worktree edits
  belong to no commit and are never exported"), and a reviewer agent may have no
  filesystem access to the worktree at all — so a pending change was, until now,
  **impossible to see through ForgeQL**. A pre-commit reviewer was structurally
  blind: it could either guess or refuse.

  - Leads with the **file map** (`status`, `added`, `removed`, `file`), then the
    unified-diff text.
  - **Untracked files are included**, as whole-file additions. `git diff HEAD`
    omits them, which would have hidden every newly added source file from a
    review — the exact silent omission the mutation layer's boundary diff exists
    to prevent.
  - `ForgeQL` runtime files (`.forgeql-*`) are excluded, as in `EXPORT PATCH`.
  - Clauses apply to the per-file rows (`path`, `name`, `status`, `added`,
    `removed`, `changed`) via the standard `ClauseTarget` pipeline — no new
    filtering machinery. `WHERE text` instead filters the diff's own **lines**,
    exactly as for `SHOW body` / `SHOW NODE`, and runs **before** the inline cap,
    so grepping a 50 000-line diff costs no more than grepping a 50-line one.
  - Output routes through the existing `SHOW MORE` ring: the file map arrives
    inline, hunks page from the top.

  A reviewer's whole triage now collapses to three cheap queries:

  ```sql
  SHOW DIFF STAT                              -- what changed at all?
  SHOW DIFF STAT IN 'crates/forgeql-core/**'  -- was the engine touched?
  SHOW DIFF STAT IN 'doc/**'                  -- did the docs move with it?
  ```

- `git::worktree_diff()` — libgit2 `diff_tree_to_workdir_with_index` with
  `include_untracked` + `show_untracked_content`, per-file `+`/`-` counts and
  hunk text. Covered by `worktree_diff_includes_untracked_files` (the one that
  matters), `..._reports_modified_tracked_file`, `..._excludes_runtime_files`,
  and `..._is_empty_for_a_clean_worktree`.

### Notes

- **Mechanical (P1).** `SHOW DIFF` reports the bytes git already computed,
  filtered and windowed. It does not interpret, validate, or repair them — it
  exists so the *agent* can see, which is the same principle as `lines_removed`
  and the boundary diff.
- Deliberately a `SHOW`, not an `EXPORT`: `EXPORT PATCH` writes mbox files to
  disk, which is useless to a reviewer that cannot read the disk. `SHOW DIFF`
  returns bytes inline and rides the `SHOW MORE` ring.

## [0.110.0] — 2026-07-13 — feat: VERIFY/RUN through the job pool; agent hints

### Changed

- **`VERIFY build` and `RUN` now execute on the background job pool.** The
  response is unchanged — the caller still gets a synchronous `success` +
  `output` — but the engine lock is released while the subprocess runs, so a
  long test gate can no longer freeze the engine for other sessions or, on
  `forgeql-server`, for other tenants. A run that outlives the step's
  `timeout_secs` returns a `job_started` row instead; poll it with
  `JOB STATUS '<id>'`.
- **`FORGEQL_MAX_CONCURRENT_JOBS` default raised from 1 to 2**, letting one
  long gate and one quick build overlap while still bounding memory use.
- `JOB STATUS` output that exceeds the inline window is now buffered for
  `SHOW MORE` on both transports, like `VERIFY build` output. The HTTP server
  gained the same `SHOW MORE` windowing/buffering the stdio transport already
  had.

### Added

- **`JOB START` accepts typed positional args** (`JOB START 'step' 'arg'…`),
  with the same arity/type validation and injection-safe substitution as
  `VERIFY build`.
- **Background gate jobs satisfy the commit gate.** A `commit_gate: true` step
  run via `JOB START` (or a timed-out `VERIFY build`) marks the gate satisfied
  when the job completes — unless an edit happened while it ran, in which case
  the gate stays blocked because the run tested stale sources. Reconciliation
  happens on `JOB STATUS`, `JOB LIST`, and `COMMIT`.
- **Inline next-step hints on job responses.** `JOB START` answers with the
  exact poll command, a running `JOB STATUS` says how to re-check, and a failed
  `VERIFY build` / `RUN` / job carries the `SHOW MORE WHERE text MATCHES …`
  recipe for grepping the buffered log — the agent no longer needs the syntax
  reference for the happy path.

## [0.109.3] — 2026-07-13 — fix(core): array_block never fired; map syntax damage as `error` rows

### Fixed

- **`array_block` (0.109.2) shipped DEAD — it never emitted a single row.**
  `scan_block_run` walked `next_sibling()`, but JSON array elements are separated
  by `,` tokens: anonymous siblings whose `map_kind` is empty. The run therefore
  broke at the first comma, so a 201-element array scanned as a run of **one** and
  no block was ever emitted. `crates/forgeql/tests/corpus.json` still indexed to
  **zero rows** — the exact problem 0.109.2 claimed to fix.

  The run is now scanned over **named** siblings. Rust comment runs have no
  separator between members, which is why every existing comment-block test kept
  passing and nothing flagged it.

  This also revives block groups in `rust.json`, `c.json` and `cpp.json`, which
  were equally dead.

  **Why the test suite missed it:** the unit test asserted that `json.json`
  *declares* a block group — it never asserted a block row is *emitted*. It tested
  the config file, not the behaviour, and passed on dead code. Found by driving
  the built binary directly (`RUN 'run_fql'`), which is the only check that asks
  the engine a question about a real file.

### Added

- **`error` rows.** A tree-sitter `ERROR` region — bytes the parser could not
  parse — now emits an addressable row with `fql_kind = 'error'`. Until now these
  were tracked only to suppress phantom enrichment and emitted nothing, so a
  broken file was **silently, partially indexed** and an agent had no way to learn
  that the file it was about to mutate was already damaged.

  ```sql
  FIND symbols WHERE fql_kind = 'error' GROUP BY file ORDER BY count DESC
  SHOW NODE '<id>'   →   CHANGE NODE '<id>' WITH '…'
  ```

  Only the **outermost** damage is emitted; a nested `ERROR` would report one
  wound as several. Zero-width `MISSING` tokens are deliberately **not** emitted:
  a row spanning no bytes could be seen but not read or repaired, and a row you
  cannot act on is the half-measure this change exists to avoid.

  **P1:** the engine maps the damage and hands over a handle. It does not
  validate, refuse, or repair — that is the agent's job.

### Changed

- `ENRICH_VER` 26 → 29 (27 and 28 consumed mid-development, never released).

  **This was missed for three commits.** 0.109.1 (JSON/YAML naming) and 0.109.2
  (`array_block`) both changed index output and neither bumped `ENRICH_VER`, so
  every corpus golden suite read **pre-change v26 segments** and kept reporting
  `symbols_indexed=2874572 ✓` for Zephyr — the same number as before the change.
  Three commits of indexing work were never once exercised against the real
  corpora, and every gate was green throughout.

  The bump is required on **every iteration** of an indexing change, not once per
  feature: a v(N) cache built from an earlier draft of your own change is exactly
  as stale as a v(N-1) cache. This is now guardian principle **P5**.

## [0.109.2] — 2026-07-12 — refactor(core): block-group key belongs to the language, not the engine

### Fixed

- **The last language-shaped fact in `forgeql-core` is gone.** `block_group_key`
  (`ast/index/file_indexer.rs`) computed a run's grouping key by matching the
  literal string `"comment_style"` — a language concept hardcoded in the
  language-agnostic core. It is now a `LanguageSupport::block_group_key` trait
  method with an empty default; core knows only the rule *"same key groups,
  different key splits the run"* and asks the language what the key is.

  Rust supplies the comment style (so `///` doc runs and `//` line runs still
  form separate blocks); a future format supplies whatever it needs — CSV
  splitting records by field count needs **no core change at all**.

### Added

- **`array_block`** — JSON now declares a `block_groups` rule collapsing a run of
  8+ adjacent `array` siblings into one synthetic block node.

  **This is what makes a keyless JSON document addressable.** An array of arrays
  of strings (`crates/forgeql/tests/corpus.json` — 26 KB, 733 entries) has no
  keys anywhere, so the naming ladder can name nothing in it: it indexed to
  **zero rows** and was invisible to every `FIND`, `SHOW` and `CHANGE`. It is now
  one `array_block` node, and its entries are reachable by node-relative offset
  with verbs that already exist:

  ```sql
  SHOW NODE   '<block>' WHERE text MATCHES 'g07_'
  CHANGE NODE '<block>(42)' WITH '  ["g01_new", "FIND …"],'
  DELETE NODE '<block>(40-52)'
  ```

### Notes

- **`collapse_members` was scoped for this release and deliberately dropped.**
  The plan assumed block members would need suppressing to stop a huge run
  exploding the index. They do not — after the key-set/breadcrumb naming of
  0.109.1, the elements of a keyless array are *unnamed*, so they emit no rows to
  begin with and the block row alone does the job. The first real consumer is CSV
  (240 000 named records), so the flag lands with its consumer rather than as
  speculative config.
- **Footgun, guarded by a test:** a language that declares `split_on_attr` but
  does not implement `block_group_key` silently gets an empty key, so its runs
  never split. `comment_block_splits_on_style` caught exactly this during the
  refactor (the in-core test fixture `RustLanguageInline` had not been updated);
  it is kept in sync with the production impl on purpose.

## [0.109.1] — 2026-07-12 — fix(lang-text): every JSON/YAML structural node is addressable

### Fixed

- **Structural nodes with no natural name emitted no row at all, so they had no
  `node_id` and could not be moved, changed or deleted.** In
  `.github/workflows/ci.yml` the step `- uses: actions/checkout@v4` (no `name:`
  key) was invisible to the index — and, worse, its `uses` pair was reparented
  onto the enclosing `steps` pair, so `SHOW outline` reported a child as a
  *sibling*. `extract_name` named only pairs and containers carrying an
  identifier-like member (`name`/`id`/`key`/`title`/`alias`); everything else
  returned `None`, and `process_node_rows` only emits a row when a name exists.
- **`fql_kind = 'array'` was documented but could never be produced.**
  `json.json` and `yaml.json` both mapped the kind, but nothing named a
  sequence, so `FIND symbols WHERE fql_kind = 'array'` returned 0 rows across
  every JSON/YAML file in the repo.

### Added

- **`forgeql-lang-text::structure`** — one shared naming ladder for the
  structured-text formats. JSON and YAML now delegate `extract_name` to
  `structured_name`, supplying only a `StructureSpec` of their tree-sitter kind
  names, so changing the naming rules is a one-file edit:
  - `pair` → its (unquoted) key text;
  - container with an identifier-like member → that member's value (unchanged);
  - container **without** one → its **key-set skeleton**, the sorted keys
    comma-joined (`uses`, `name,run`);
  - `array`/sequence → the key of its nearest ancestor pair (`steps`);
  - anything else → `None`.

### Notes

- **Names never encode a position.** `OrdinalRemapper::assign` re-attaches a node
  to its previous ordinal by matching `(name, fql_kind, parent_ordinal)`. A
  slot-based name (`steps[0]`) would follow the *position* rather than the node:
  swap two sibling elements and each matches the other's hint, so the two nodes
  trade ordinals and a handle held for one silently resolves to the other. The
  key-set skeleton is derived from the node's own content — stable under sibling
  reorder and under a value edit (`@v4` → `@v5` leaves the key set `{uses}`
  untouched) — the same contract as the condition skeleton that names `if`
  statements. `no_name_encodes_a_position` guards this in both plugins.
- A JSON document containing no keys at all (`crates/forgeql/tests/corpus.json`,
  an array of arrays of strings) still indexes to zero rows: it has no breadcrumb
  to name anything. Making it addressable needs block-grouping of same-kind
  sibling runs, which is a separate change.

## [0.109.0] — 2026-07-11 — feat(server): --log-queries CSV query log

### Added

- **`forgeql-server --log-queries`** — the HTTP daemon can now write the same
  per-statement CSV query log as the `forgeql` binary: one row per executed
  statement in `{data-dir}/log/{source}.csv` with timestamp, clipped command,
  lines returned, and approximate token counts. Rows are keyed to the
  session that executed them (a `USE` earlier in a batch keys the following
  statements to the new session), so multi-tenant agent activity over HTTP is
  auditable exactly like stdio sessions.

## [0.108.0] — 2026-07-11 — feat(server): full MCP handshake over HTTP

### Added

- **`forgeql-server` now speaks the complete client-to-server half of the
  MCP streamable-HTTP protocol.** `POST /mcp` handles `initialize` (protocol
  version negotiation, tools capability, server identity, and connect-time
  usage instructions), `notifications/*` (acknowledged with `202 Accepted`
  and no body), `tools/list` (the `run_fql` tool with an input schema matching
  the stdio server's), and `ping` — in addition to the existing `tools/call`.
  Remote MCP clients such as Claude Code can now connect to the daemon
  directly over HTTP with a bearer token, with no local binary required.

## [0.107.0] — 2026-07-11 — feat(gc): clearer `forgeql gc` output and CLI ergonomics

### Changed

- `forgeql gc` now prints a purpose-built human report in every output format.
  Previously, with `--format text`, the summary line was dropped entirely and
  the per-directory size was mislabeled as `usages:` with a stray `:0`, so it
  was impossible to tell how much would be reclaimed — the deletion still worked
  (a single run removes every stale version at once), but the feedback hid that.
  It now lists only the directories that will be deleted (each with its size),
  followed by an explicit total (`N directories, X reclaimable`), and reports
  `Deleted N directories, reclaimed X` after applying. When nothing is stale it
  says so. `--format json` emits the structured report for scripting.
- The CLI now defaults to human-readable `text` output. Agents on the MCP
  surface still receive compact CSV independently of this flag; pass
  `--format compact` for token-efficient CSV in scripts.
- `--data-dir` is now a global flag, so it is accepted after a subcommand
  (`forgeql gc --data-dir …`), not only before it.
- The top-level `--help` now lists the run modes and points to per-command help
  (`forgeql <command> --help`).

### Removed

- Deleted a stray `docs/zz_blankrepro.md` reproduction file that was publicly
  visible in the repository.

### Internal

- Extracted `ForgeQLEngine::vacuum_report`, a public method returning the full
  (uncapped) `VacuumReport`, shared by the `VACUUM` DSL verb and the CLI so both
  read the same structured totals.

## [0.106.0] — 2026-07-11 — feat(vacuum): VACUUM verb and `forgeql gc` to reclaim stale cache versions

### Added

- `VACUUM [SOURCE 'name'] [KEEP n] [ALL] [APPLY]` — a new admin-only statement
  that reclaims disk space by deleting stale columnar cache version directories.
  Each indexed repository accumulates `<provider>-v<N>` directories under
  `forgeql/overlays/` and `forgeql/segments/` on every enrichment-version bump;
  only the current version is live and the rest are dead weight that never got
  cleaned up. VACUUM previews by default — it reports the in-scope version
  directories grouped by whether each will be kept or deleted (with per-directory
  sizes) plus a summary line carrying the count and total reclaimable bytes, and
  removes nothing unless `APPLY` is given.
  Classification keys purely on the parsed `<N>` versus the current enrichment
  version, ignoring the provider prefix, so a future content-hashing scheme is
  handled identically with no code change. By default only versions older than
  the current one are removed; the current version and any newer ones (which
  belong to a newer binary) are preserved. `KEEP n` retains the n newest older
  versions, and `ALL` removes every version including the current one. With no
  `SOURCE` the command spans every registered source.
- `forgeql gc [--source NAME] [--keep N] [--all] [--yes]` — a CLI wrapper over
  `VACUUM` that previews, prompts for confirmation, then applies. `VACUUM`
  carries the same clearance as source management: it is rejected over the MCP
  surface and requires an admin token over HTTP, exactly like `CREATE SOURCE`.

## [0.105.0] — 2026-07-10 — fix(c/c++): SHOW members resolves struct/union definitions

### Fixed

- `SHOW members OF '<type>'` returned nothing for many C and C++ structs and
  unions. A `struct Foo` used only as a type reference (`struct Foo *p;`, a
  function parameter or return type) or written as a forward declaration
  (`struct Foo;`) was indexed as its own `struct`/`union`/`enum` symbol, even
  though it has no body and no members. When a type was referenced in more
  files (or later in the same file) than it was defined, symbol resolution
  could land on one of these bodyless references, so `SHOW members` — and any
  lookup that resolves a type by name — saw an empty body. References and
  forward declarations of `struct`/`class`/`union`/`enum` are no longer
  indexed as type symbols: only the definition, which carries the members, is.
  As a result `SHOW members` and type resolution always reach the definition.
- Independently, when a bodyless reference to a type appeared before its
  definition within the same file, the member lookup walked to the reference
  first and stopped there. The lookup now prefers the definition (the matching
  node that actually has a body) over a bodyless reference of the same name.
- Bumped `ENRICH_VER` 25 → 26 (`storage/columnar/mod.rs`): dropping the
  reference rows changes index output, so cached per-file segments and
  overlays are rebuilt on next use instead of being reused with the old rows.
## [0.104.0] — 2026-07-09 — feat(cpp): index unions, typedef aliases, and enum constants

### Fixed

- C and C++ `union` types were not indexed. A `union Name { … }` produced no
  `union` symbol: it could not be found by type name, carried no node id, and
  `SHOW members` on it returned nothing. Unions are now first-class symbols
  with node ids and member listing, matching structs — for both the named
  `union Name { … }` and the `typedef union { … } Name;` forms.
- `typedef` aliases were invisible to the index. A scalar
  `typedef unsigned int paddr_t;`, a function-pointer typedef, and the name
  introduced by `typedef struct { … } Name;` / `typedef enum { … } Name;`
  each produced no symbol, so the only name a caller has for the type could
  not be located or edited by node handle. Typedef aliases are now indexed as
  `type_alias` symbols with node ids, including the anonymous struct and enum
  forms.
- Enumerator constants inside an enum body had no node id and an empty kind,
  so there was no supported way to insert or change an enumerator through a
  node handle. Each enumerator is now an addressable `enumerator` symbol.
- Bumped `ENRICH_VER` 24 → 25 (`storage/columnar/mod.rs`): the new `union`,
  `type_alias`, and `enumerator` rows change index output, so cached per-file
  segments and overlays are rebuilt on next use instead of being silently
  reused with the old (incomplete) rows.

## [0.103.0] — 2026-07-08 — feat(export): EXPORT PATCH — git am-ready patches from session commits

### Added

- `EXPORT PATCH [LAST n]`: writes the session's commits as `git am`-ready
  mbox files under `.forgeql-patches/` in the worktree and returns them
  inline — exported range, one row per file with absolute path, size and
  sha256 (verify after transfer with `sha256sum`), then the patch text,
  windowed through `SHOW MORE`. Without `LAST` the range is everything the
  session committed over its base branch; `LAST n` exports the last n
  source-touching commits. Patches are generated with `--binary`, so binary
  files survive `git am`.
- The export is transaction-safe: `.forgeql-*` paths are excluded from every
  patch, so transaction checkpoint commits produce no patch, a commit mixing
  source with runtime files exports only its source part, and the series
  still applies in order. Uncommitted worktree edits are surfaced as a hint,
  never silently dropped into a patch. `.forgeql-patches/` itself is kept
  out of commits, checkpoints, and `git status` (existing deployments gain
  the ignore entry automatically on the next `USE`).

## [0.102.0] — 2026-07-07 — release: field-report fixes rollup

Release marker for the changes shipped as 0.100.1–0.101.3, so a single tag
carries the complete set. No functional changes beyond 0.101.3.

- Worktree paths restored at their pre-0.100 location via a compat symlink —
  see 0.100.1.
- Runtime artifacts kept out of commits and git status; startup scan made
  symlink-safe — see 0.100.2.
- Memory-bounded FIND: unknown-field refusal, per-segment filtering, and the
  `FORGEQL_FIND_MAX_ROWS` budget — see 0.101.0.
- `FIND files` duplicate rows and COPY/MOVE line counts — see 0.101.1.
- Generated config template and syntax reference caught up with VERIFY/RUN —
  see 0.101.2.
- Structured rejections as error-flagged tool results; `DELETE NODE` op
  label — see 0.101.3.

## [0.101.3] — 2026-07-07 — fix(mcp): structured rejections as tool results + DELETE NODE op label

### Fixed

- Structured engine rejections that carry a self-healing JSON payload — a
  rejected `IF REV` guard returning the node's current rev, line range, and
  source, or a `node_not_found` lookup — arrived as MCP protocol errors with
  the JSON buried inside the error string. They are now returned as
  error-flagged tool results, so a client parses the payload directly instead
  of unwrapping a protocol error.
- `DELETE NODE` responses reported the op as `change_content` (the
  line-delete plumbing it reuses); they now report `delete_node`.

## [0.101.2] — 2026-07-07 — docs(config): generated template and syntax reference catch up with VERIFY/RUN

### Changed

- The generated `.forgeql.yaml` template now documents everything a step can
  declare — `commit_gate`, typed `params` (`ident` substituted, `string`
  stdin-bound), `weight` tiers and explicit cost maps, `summary` windows,
  `run_steps` templates — plus the `FORGEQL_*` environment contract every
  VERIFY/RUN/JOB subprocess receives (in particular `FORGEQL_WORKTREE` and
  the per-worktree `FORGEQL_BUILD_DIR`). A test keeps the template loadable
  and its feature list in sync.
- The syntax reference no longer claims the alias *is* the `session_id`: `USE`
  returns an opaque composite token that must be passed back verbatim. The
  worktree naming scheme, the `.forgeql.yaml` example (which used line-budget
  keys that never existed), and the environment contract are now documented
  as implemented.

## [0.101.1] — 2026-07-07 — fix(output): FIND files duplicate rows + COPY/MOVE line counts

### Fixed

- `FIND files` could return the same path twice (same path and size,
  consecutive rows) when the workspace overlay held duplicate path entries —
  a state the symbol pipeline already guards its GROUP BY fast paths against.
  File listings are now deduplicated on path, keeping the freshest entry.
- `COPY LINES` / `MOVE LINES` reported one line fewer than the addressed
  range on whole-file copies: the line-addressing model counts the position
  after a final newline as an addressable (zero-byte) line, but the counter
  counted the payload's text lines. Both ops now report the addressed range
  length, and a clean MOVE reports the same number written and removed. File
  bytes were always correct — the mismatch was presentation only.

## [0.101.0] — 2026-07-07 — feat(query): memory-bounded FIND — unknown-field refusal, per-segment filtering, row budget

### Added

- `FIND symbols` now rejects a WHERE field that is neither a core field nor an
  enrichment column present anywhere in the index, with an error that names
  the field, lists the core fields, and points at `SHOW LINES … WHERE text
  MATCHES` for content search. Previously such a query silently matched
  nothing — after materialising every candidate row, which on a
  42-million-symbol index could exhaust host memory and take the machine down.
- `FORGEQL_FIND_MAX_ROWS` (default 5 000 000, `0` disables): a hard budget on
  the rows one FIND may materialise before ORDER BY / GROUP BY / LIMIT apply.
  Queries that exceed it fail fast with guidance to narrow the scan instead of
  growing without bound.

### Changed

- Residual WHERE predicates are now applied per segment during
  materialisation, so non-matching rows are dropped as each segment is read
  instead of accumulating across the whole index. Result sets are unchanged;
  peak query memory now scales with matching rows, not candidate rows.

### Fixed

- `WHERE usages …` and `ORDER BY usages` compared against a stale always-zero
  per-segment column when an explicit `LIMIT` enabled the early-exit or top-K
  paths, returning wrong (usually empty) results. Workspace usage counts are
  now stamped onto rows before any predicate or ordering decision.

## [0.100.2] — 2026-07-07 — fix(git): keep runtime artifacts out of commits and git status

### Fixed

- `COMMIT` staged the contents of the mutation staging area into user-facing
  commits: the exclusion check looked only at leaf file names, and staging
  entries live at `.forgeql-staging/<hex>/<name>` with ordinary leaf names —
  a commit could gain a hundred-plus binary segment files, which ForgeQL then
  deleted, leaving staged-deletion noise in `git status`. The check is now
  component-wise, matching the checkpoint path's behaviour.
- Transaction checkpoints committed the `SHOW MORE` paging buffers as tracked
  files, which let host pre-commit hooks (e.g. trailing-whitespace fixers)
  rewrite ForgeQL's own runtime state during later verify runs and fail the
  build. Checkpoints now exclude the paging buffers; the index cache, undo
  ring, and columnar delta remain checkpoint-committed by design so
  `ROLLBACK` restores them instantly.
- `USE` now writes the never-committed runtime artifacts
  (`.forgeql-session`, `.forgeql-staging/`, `.forgeql-showmore*`) to the
  repository's `info/exclude` (idempotent managed block), so they stay out
  of `git status` and host tooling for every worktree of the source.
- The startup session-restore scan could follow the compatibility symlinks
  introduced at the old worktree location and misread a worktree's own
  contents as session directories — pruning every subdirectory that lacked a
  session sentinel. The scan now inspects entry types without following
  symlinks, and the pruner refuses to touch any directory that does not
  contain a `.git` entry (a stray directory under `worktrees/` is never
  deleted, whatever the scan thinks of it).

## [0.100.1] — 2026-07-07 — fix(session): compatibility symlink at the old worktree path

### Fixed

- Session worktrees moved from `worktrees/{source}.{branch}.{alias}` to a
  per-user layout `worktrees/{user}/{source}.{branch}.{alias}`, which broke
  host tooling that resolves the old path — container runners and mount
  scripts used by `VERIFY`/`JOB` steps failed with "worktree not found"
  even though the session itself worked, blocking every build gate. `USE`
  now maintains a compatibility symlink at the old location (never
  clobbering a real directory or another session's link), and session
  teardown removes it. Scripts should still prefer the `FORGEQL_WORKTREE`
  environment variable, which always carries the session's real worktree
  path.

## [0.100.0] — 2026-07-06 — release: memory-bounded indexing rollup

Release marker for the three changes shipped as 0.99.0–0.99.2, so a single
tag carries the complete set. No functional changes beyond 0.99.2.

- The mechanical rename sweep (`CHANGE NODE … MATCHING`,
  `CHANGE NODES FOUND MATCHING`) — see 0.99.0.
- Peak indexing memory bounded by a size-aware admission queue — see 0.99.1.
- Cold `USE` reuses existing per-file segments instead of re-parsing — see
  0.99.2.

## [0.99.2] — 2026-07-06 — fix(index): reuse per-file segments instead of re-parsing

### Fixed

- A cold `USE` re-parsed every file even when its per-file segment already
  existed on disk: existing segments only skipped the final write, after the
  full parse cost had been paid. Any overlay-cache miss — a new commit on the
  branch, or a lost/failed overlay — therefore re-parsed the whole
  repository. Indexing now hashes each file's raw bytes first (a cheap read,
  no parse) and, when a valid segment for that exact content already exists,
  registers it for the overlay build and skips the parse entirely. An
  incremental commit now re-parses only the files it changed, and rebuilding
  a lost overlay from intact segments costs seconds instead of a full
  re-index. Combined with the bounded large-file queue from 0.99.1, initial
  indexing memory stays flat across repeated `USE` calls.

## [0.99.1] — 2026-07-06 — fix(index): bound peak memory during initial indexing

### Fixed

- Initial indexing could exhaust system memory on repositories containing
  many large files (for example generated automotive XML of 50–100 MB each).
  Peak memory during indexing is dominated by parse trees, whose size is
  proportional to file size, and every file was parsed at full parallelism —
  one huge tree alive per CPU core, observed to exceed 25 GB of RAM and OOM
  the host. Indexing now takes one filesystem-metadata pass up front and
  splits files at a size threshold: small files keep full parallelism, while
  large files drain a dedicated largest-first queue with a bounded number of
  workers, so at most a few large parse trees exist at any moment. Both lanes
  run concurrently on the same thread pool, so small-file throughput is
  unchanged. Tunables: `FORGEQL_BIG_FILE_MB` (size threshold, default 4 MB)
  and `FORGEQL_BIG_FILE_SLOTS` (large-file workers, default 2).

## [0.99.0] — 2026-07-06 — feat(mutate): the mechanical rename sweep

### Added

- `CHANGE NODE '<id>' [IF REV '<rev>'] MATCHING [WORD] 'old' WITH 'new'` —
  replace pattern occurrences inside one node's span only. Same matching
  semantics as the file-level form (plain substring, or `WORD` for
  whole-word boundaries), scoped to the node's current lines.
- `CHANGE NODES FOUND MATCHING [WORD] 'old' WITH 'new'` — apply the
  replacement on every line of the previous FIND result in the session.
  This completes the two-step rename workflow: `FIND usages OF 'old'`
  aims at the exact occurrence sites (string literals and comments are
  not usage sites, so they survive untouched), then the sweep replaces
  only on those lines across every file, in one mutation with one
  boundary diff. Works across every indexed format — code, AUTOSAR XML,
  CMake, DBC, reStructuredText.
- Sessions remember the `(path, line)` sites of the most recent FIND
  result; a sweep without a previous FIND fails with guidance, and a
  pattern that matches none of the remembered lines is an error rather
  than a silent no-op. Any mutation (including UNDO) invalidates the
  remembered sites — line numbers may have shifted, so the sweep must
  re-aim with a fresh FIND.

### Fixed

- Node insertions and deletions did not invalidate the commit gate: a
  gated VERIFY pass stayed "satisfied" across a subsequent INSERT NODE /
  DELETE NODE, so COMMIT could accept unverified edits. All plan-based
  mutations now share the same bookkeeping (gate invalidation, edit
  counter, FIND-site invalidation).

## [0.98.0] — 2026-07-06 — feat(hints): oversized-response and unknown-field guidance

### Added

- Responses estimated above ~2,000 tokens now carry a one-line hint with
  the narrowing tools (WHERE / IN / EXCLUDE, LIMIT with OFFSET paging, or
  GROUP BY aggregation). Motivated by usage-log analysis: a single
  unbounded directory walk once returned a 50,000-token response.
- A WHERE clause naming a field that no row type carries used to match
  nothing silently. FIND results that come back empty with such a field
  now include a hint naming the unknown field and pointing at the core
  and enrichment field lists. Valid enrichment fields that merely have no
  matching rows stay hint-free; both behaviors are regression-tested.
- Query results gain an optional `hint` value (omitted unless populated),
  rendered as a final row in the compact output.

## [0.97.0] — 2026-07-06 — feat(hints): targeted guidance on common command mistakes and oversized reads

### Added

- The parse-error hint table now covers more common mistakes, each answered
  with the correct command instead of a bare grammar error:
  `CHANGE NODE … WITH NOTHING` → use `DELETE NODE '<id>'`;
  `INSERT NODE AFTER` → the position keyword comes first
  (`INSERT AFTER NODE`); `ROLLBACK` without a name → name the transaction;
  an unknown `FIND` target → the four valid targets with examples;
  `REFRESH` without `SOURCE` → the correct admin statement.
- The output-windowing footer on oversized SHOW results now also teaches
  the cheaper alternatives — filtering with `WHERE text MATCHES` or
  addressing the construct directly with `SHOW NODE '<node_id>'` /
  `SHOW body OF 'symbol'` — instead of only offering to page through
  everything. Motivated by usage logs: raw line reads over 100-line spans
  were the second-largest token consumer.

### Fixed

- The MCP tool instructions described oversized SHOW output as blocked
  with zero lines returned; they now describe the shipped behavior (the
  output is windowed and the remainder is available via `SHOW MORE`) and
  recommend narrowing before paging.

## [0.96.0] — 2026-07-06 — docs: node-handle editing is the documented default across all documentation

### Changed

- Documentation overhaul (README, syntax reference, architecture, all agent
  guides, cursor rules): node-handle editing (`CHANGE NODE` / `INSERT
  BEFORE|AFTER NODE` / `DELETE NODE`, node-relative line offsets, `IF REV`)
  is now the primary documented editing model everywhere. Raw line and file
  operations (`CHANGE FILE … LINES`, `MOVE LINES`, `COPY LINES`,
  `CHANGE FILES MATCHING`) are compressed into one legacy chapter scoped to
  non-indexed files and file scaffolding.
- New documentation for shipped features: mutation responses and the
  boundary-diff contract (`new_node_id`, `lines_written`, `lines_removed`,
  inline node handles in diff context; the engine never auto-corrects
  syntax — the caller reads the diff and fixes seams), usage-site queries
  and real `usages` counts (`FIND usages OF`, `ORDER BY usages`), the
  structured-text format family (XML/AUTOSAR/tresos naming cascade, DBC,
  INI, justfile, Make, CMake, reStructuredText, TOML, JSON, YAML,
  Markdown), `UNDO LAST-n`, `VERIFY` typed params, background `JOB`
  commands, and the commit gate.
- Architecture guide now describes the columnar store: content-addressed
  per-file segments with name FST and usage postings, the memory-mapped
  workspace overlay with the usage-count aggregate, the dirty overlay for
  in-transaction edits, and reindex-on-mutation with stable node ids.
- Corrected stale statements: `DISCONNECT` (never existed) removed from the
  README; `CHANGE FILE … WITH NOTHING` deletes the file (docs said it only
  cleared the content); oversized `SHOW` output is windowed through the
  `SHOW MORE` buffer (docs described a hard block).

## [0.95.0] — 2026-07-06 — feat(xml): AUTOSAR ECUC parameter values named by their definition reference

### Fixed

- AUTOSAR ECU-configuration values (`.arxml`/`.ecuc`):
  `ECUC-NUMERICAL-PARAM-VALUE`, `ECUC-TEXTUAL-PARAM-VALUE`, and
  `ECUC-REFERENCE-VALUE` elements carry neither a SHORT-NAME child nor an
  identifying attribute, so every parameter row was named by its bare tag —
  hundreds of identical `ECUC-NUMERICAL-PARAM-VALUE` rows, making it
  impossible to find a parameter by name. The XML naming cascade gains a
  step between SHORT-NAME and tag-name fallback: the last `/`-segment of a
  `DEFINITION-REF` child's text
  (`…/CanIfPublicCfg/CanIfPublicTxBuffering` → `CanIfPublicTxBuffering`).
  Containers keep SHORT-NAME priority; the `DEFINITION-REF` element itself
  keeps its tag name.
- Cache invalidation: segment content version 23→24, overlay schema 14→15,
  legacy index cache 32→33 — existing XML rows re-index under the new names.

### Added

- Synthetic automotive fixtures (`tests/fixtures/EcucCanIf.arxml` — nested
  AUTOSAR ECUC module configuration; `tests/fixtures/TresosAdc.xdm` — EB
  tresos datamodel) and an end-to-end test proving the GUI-bypass workflow:
  find an ECUC parameter or tresos variable by its real name, then edit its
  value through the node handle.

## [0.94.0] — 2026-07-06 — feat(query): real usages_count on FIND symbols rows (BUG-006 slice U3)

### Added

- **Overlay usages aggregate** (BUG-006 U3): the overlay gains a 13th TOC
  blob, `usages_count_fst` — an FST mapping symbol name → total usage-site
  count, summed across every segment's `usages_fst` values at overlay-build
  time (the count is the low 32 bits of each FST value; postings bytes are
  never decoded). Zero-length blob when no segment carries postings.
  Overlay `SCHEMA_VERSION` 13→14 (existing v23 segments rebuild the overlay
  on next USE — no re-index).

### Fixed

- `FIND symbols` rows reported `usages = 0` forever: `usages_count` is now
  stamped at query time from the overlay aggregate (main pipeline + ORDER BY
  name fast paths), so `ORDER BY usages DESC` and `WHERE usages > N` return
  real results.
- `WHERE usages > N` silently returned nothing: the per-segment
  `usages_count` zone map (an all-zeros legacy column) pruned every
  candidate segment. Zone-map pruning is skipped for the `usages` field;
  all other numeric columns keep it.
- Remaining slice: U4 audits collection completeness (`Point::new` path
  segments).

## [0.93.0] — 2026-07-06 — feat(query): FIND usages OF returns real usage sites (BUG-006 slice U2)

### Fixed

- **BUG-006**: `FIND usages OF '<name>'` on the columnar backend returned
  only definition rows (it read the definitions name-FST), so every
  blast-radius query came back empty or definition-only. It now reads the
  per-segment usage postings written at index time (0.92.0's
  `usages_fst`/`usages_postings`), scanning the persistent overlay
  (dirty-shadow aware) plus the dirty overlay, and returns one row per
  usage site — name + path + line, matching the legacy backend's row
  shape. Occurrences without call parentheses (function-pointer
  assignments, type positions) are included; `GROUP BY file` counts are
  now real. Regression test pins the `encenderMotor` function-pointer
  site at motor_control.cpp:34 — the exact case grep-style discovery
  misses.
- Zephyr golden `GBUG11a_usages_encode_node_call_at_679` re-blessed: it
  documented the bug (expected 0 rows for the call site at
  `hci_driver.c:679`); the site is found now (1 row, line 679).
- Remaining slices: U3 populates `usages_count` on FIND symbols rows;
  U4 audits collection completeness (`Point::new` path segments).

## [0.92.0] — 2026-07-06 — feat(index): usage postings in segments (BUG-006 slice U1, reference-index storage)

### Added

- **Usage postings** (BUG-006 U1): every `.fqsf` segment now carries
  `usages_fst` / `usages_postings` blobs mapping identifier text to the
  1-based source lines where it occurs — the storage half of the reference
  index. Same FST wire format as the definitions name-FST, but postings hold
  lines, not row ids; files with no usage sites omit the blobs entirely.
  `SegmentBuilder::add_usage` + `SegmentReader::lookup_usage_lines` are the
  new API surface. All three segment writers feed it: the inline per-file
  emit, the shadow writer (usage sites pre-grouped by `path_id` once, not
  per-file — the merged-table scan would be quadratic), and the
  post-mutation reindex path.
- Cache invalidation: segment schema 1→2, `ENRICH_VER` 22→23 (segments
  cache per blob under `{provider}-v{N}`), overlay `SCHEMA_VERSION` 12→13,
  legacy `CachedIndex` 31→32. v22 segments lack the blobs and would
  silently report zero usages, so a full re-index is forced.
- Query side (`FIND usages OF` reading the postings, real `usages_count`)
  lands in slices U2/U3.

## [0.91.7] — 2026-07-05 — fix(mutate): MOVE/COPY numeric-dest validation; WITH NOTHING deletes indexed files; condition_text doc relabel

### Fixed

- **BUG-016 residual**: `MOVE LINES … TO 3` (or any purely numeric `TO`
  destination) silently created a file literally named `3`. Both `MOVE LINES`
  and `COPY LINES` now reject a numeric destination with guidance pointing at
  `TO '<path>' AT <line>` — input validation, not path policy.
- **BUG-014 residual**: `CHANGE FILE … WITH NOTHING` now actually deletes.
  Three layers were broken: the indexed-file gate blocked the verb entirely;
  beneath that `resolve_delete` only *truncated* the file to 0 bytes (the
  original ghost-file bug) — it never unlinked; and `merge_by_file()` rebuilt
  every `FileEdit` with a hardcoded `delete: false`, silently downgrading the
  deletion again. Deletion is now exempt from the gate (naming a file
  explicitly for removal is not raw-text editing; the diff shows the deleted
  content), the merge preserves the flag, `apply()` removes the file from
  disk (`FileEdit.delete`), and COMMIT stages the removal
  (`index.remove_path`) instead of failing on the missing path.
- **BUG-023 (docs)**: the enrichment tables in `doc/syntax.md` and the agent
  guides described `condition_text` as "raw condition expression text". It is
  a normalized *skeleton* (operands alpha-renamed for shape comparison,
  e.g. `a||b&&c`); grammars without a `condition` field (CMake, Make, C++
  range-`for`) name rows by the raw first line instead. Docs now say so.

## [0.91.6] — 2026-07-05 — fix(find): string/boolean WHERE fields projected into FIND output (BUG-024)

### Fixed

- **BUG-024**: filtering on a string or boolean enrichment field
  (`WHERE mixed_logic = 'true'`, `WHERE cast_style = 'as_cast'`) returned rows
  that never showed the filtered value — only numeric filters projected their
  field. `detect_metric_hint` now falls back to any remaining non-core WHERE
  field after the numeric and ORDER BY priorities, so the value you filtered
  on is always visible in the output row (the display path was already
  string-safe). Core row-identity fields (`fql_kind`, `language`, `path`, …)
  are excluded from metric projection.

## [0.91.5] — 2026-07-05 — fix(query): multiple EXCLUDE clauses are all honored (BUG-017)

### Fixed

- **BUG-017**: a query with more than one `EXCLUDE '<glob>'` clause silently
  applied only the last one — `Clauses.exclude_glob` was an `Option<String>`,
  so the parser overwrote earlier clauses. Now `exclude_globs: Vec<String>`:
  every clause is collected and a row is dropped when ANY pattern matches its
  path. All readers updated (filter pipeline, FIND files walk, legacy
  prefilter/resolve hints, columnar fast paths and segment pruning).
- Re-diagnosis note: the glob matcher itself was already correct and
  gitignore-consistent (patterns anchor at the path root unless they start
  with `**/`; the documented invariant `kernel/**` must NOT match
  `tests/kernel/…` stands). The old "EXCLUDE doesn't filter tests/**" report
  was this multi-clause bug plus anchoring expectations.

## [0.91.4] — 2026-07-05 — fix(enrich): C/Rust shift expressions get fql_kind = shift_expression (BUG-019)

### Fixed

- **BUG-019**: `<<`/`>>` shift expressions in C and Rust indexed with an empty
  `fql_kind` — `FIND symbols WHERE fql_kind = 'shift_expression'` returned
  nothing for those languages while working for C++. Config-only fix mirroring
  `cpp.json`: `expressions.shift_kinds = ["shift_expression"]` (the canonical
  output label) plus the `"shift_expression": "shift_expression"` `kind_map`
  entry in `c.json` and `rust.json`. The `shift_direction`/`shift_amount`
  enrichment fields were always attached; now the rows are queryable by kind.
  `ENRICH_VER` 21 → 22 (+ overlay 11 → 12, `CachedIndex` 30 → 31) force the
  one-time re-index.

## [0.91.3] — 2026-07-05 — fix(find): FIND files WHERE name; hide runtime artifacts

### Fixed

- **`FIND files WHERE name = 'Kconfig'` now works.** File rows only exposed
  `path`/`file` and `extension`/`ext` fields; `name` — the idiomatic first
  guess, mirroring `FIND symbols WHERE name` — silently matched nothing
  (agents reported "FIND files can't find some files"; non-indexed files were
  in fact tracked since FQOV v8, but the natural predicate didn't resolve).
  `name` now matches the bare file name and works with `LIKE`/`MATCHES`.
- `FIND files` no longer lists session infrastructure: the worktree gitfile
  pointer (`.git`) and forgeql runtime artifacts (`.forgeql-session`, …) are
  filtered from both the overlay fast path and the filesystem-walk path
  (query-time only — no re-index needed). `COMMIT` already excluded the same
  set.

## [0.91.2] — 2026-07-05 — fix(lang): gaps found exercising the text formats live on zephyr

Found by exercising every new format through `run_fql` against the zephyr
corpus (mutations under `BEGIN TRANSACTION`, rolled back).

### Fixed

- **CMake/Make control flow was not addressable**: `if()`/`foreach()`/
  `while()` blocks and Make conditionals had `kind_map` entries but no
  `control_flow` config section — and only that section makes the indexer
  emit control-flow rows. Both configs now declare it. Two follow-on gaps
  surfaced while verifying at zephyr scale:
  - control-flow rows from grammars without a `condition` field (CMake,
    Make) were emitted **nameless** — unfindable by `FIND`. The enricher now
    falls back to naming them by the construct's first line
    (`if(CONFIG_USERSPACE OR CONFIG_DEVICE_DEPS)`).
  - bumping the overlay `SCHEMA_VERSION` alone did **not** re-index: per-file
    segments cache under `{provider}-v{ENRICH_VER}/` keyed by blob hash, so
    rebuilt overlays reassembled the stale segments. `ENRICH_VER` (20 → 21)
    is the constant that invalidates row *content*; the overlay version
    (9 → 11) and `CachedIndex` version (27 → 30) invalidate their own layers.
  Result on zephyr: +4,748 control-flow rows (3,084,394 → 3,089,142).
- **`SHOW body` could not resolve text-format definitions**: `FIND` reported
  a Makefile rule as a `function`, but `SHOW body OF 'clean'` failed —
  body resolution reads `definitions.function_kinds`, which none of the new
  configs declared. Now: Make `rule`, CMake `function_def`/`macro_def`,
  just `recipe`, DBC `message`.
- **Default `SHOW outline` was empty for Markdown/reStructuredText**:
  `section` was missing from the structural-kind filter (pre-existing for
  Markdown), so doc files outlined as nothing without an explicit
  `WHERE fql_kind` or `ALL`. Sections now outline by default.

### Verified working live (zephyr, 0.91.1 binary)

- `.editorconfig` (extensionless dotfile) fully indexed via file-name
  fallback; CHANGE NODE on its pairs works.
- CHANGE/INSERT/DELETE NODE on `.cmake`, Makefile rules (tabs and `$(VAR)`
  preserved through heredocs), `.rst` paragraphs (text-named nodes get a
  fresh `node_id`, surfaced via `new_node_id`); ordinal remapping stays
  consistent across sibling inserts/deletes; ROLLBACK TRANSACTION restores.
- One `FIND` returns Makefile rules and C functions side by side;
  `FIND usages OF 'zephyr_library_sources_ifdef'` sweeps all CMakeLists.txt.

## [0.91.1] — 2026-07-04 — fix(index): invalidate cached indexes built without the new text formats

### Fixed

- Overlays and index caches built by pre-0.87 binaries were **silently
  reused**: the freshness check compares only the corpus commit, so adding
  the structured-text languages (0.87–0.91) never triggered a re-index and
  `USE` kept serving indexes that had never walked a `CMakeLists.txt` or
  `.rst` file. Bumped the overlay `SCHEMA_VERSION` (8 → 9) and the legacy
  `CachedIndex` `CURRENT_VERSION` (27 → 28), so every cached index rebuilds
  once with the full registry. Measured on the reference corpora: zephyr
  2,830,615 → 3,084,394 symbols (+253k from the new formats), pytorch
  3,109,089 → 3,116,758, forgeql-pub 55,775 → 58,305.

### Changed

- Re-blessed the zephyr/pytorch golden values (63 assertions) to the
  post-re-index baselines above.
- `jobs.rs`: the test module's blanket `#[allow(clippy::unwrap_used, …)]`
  (flagged by the gate's clippy-allowlist phase) converted to the
  test-scoped `#![cfg_attr(test, allow(…))]` convention used everywhere else.

## [0.91.0] — 2026-07-04 — feat(lang): Makefile, CMake, and reStructuredText support

### Added

- **Makefile support** (`.mk`, `Makefile`/`makefile`/`GNUmakefile`): rules
  index as `function` named by their target list, `VAR = value` assignments
  as `variable`, `define` blocks as `macro`, `include` lines as `import`,
  `ifeq`/`ifdef` blocks as `if`.
- **CMake support** (`.cmake`, `CMakeLists.txt`): `function()`/`macro()`
  definitions named by their first argument; every command call (`set`,
  `add_library`, …) as `call_statement` named by the command identifier;
  `if`/`foreach`/`while` blocks get nested control-flow node ids like code.
- **reStructuredText support** (`.rst`, `.rest`): sections named by title
  (nested sections nest), paragraphs/list items by normalized text snippet,
  `.. directive::` blocks as `macro_call`, `:field:` entries as `pair`,
  substitution definitions as `variable` — documentation that mentions a
  symbol is reachable by the same `FIND` sweep that finds it in code.

### Changed

- The registry's file-name fallback now also applies when a path's extension
  is *unclaimed* (not just missing), so a plugin can claim the full
  `cmakelists.txt` name without owning the `txt` extension.

## [0.90.0] — 2026-07-04 — feat(lang): justfile + INI support; extensionless file-name matching

### Added

- **justfile support** (slice 3 of structured-text addressing): recipes index
  as `function` rows named by recipe name, `:=` assignments and `alias` lines
  as `variable`, `set` lines as `pair`, `mod` as `namespace`, `import` as
  `import` — one `node_id` per recipe.
- **INI support**: `.ini`/`.cfg` plus the well-known `.editorconfig` and
  `.gitconfig` file names. `[section]` blocks index as `object`; `key = value`
  settings nest inside their section as `pair` — the same object/pair shape
  as the JSON/YAML/TOML family.
- **Extensionless file-name matching**: `LanguageRegistry::language_for_path`
  now falls back, for paths with no extension, to the lowercased file name
  with any leading dot stripped, matched against the same key table. A
  language that claims `"justfile"` therefore matches `justfile`,
  `.justfile`, `Justfile`, and `x.justfile` alike. The registry itself learns
  no file-name knowledge — plugins declare their own names.

### Known gap

- `.gitignore` has no published tree-sitter grammar on crates.io yet; it
  stays raw-text (`CHANGE FILE`) for now.

## [0.89.0] — 2026-07-04 — feat(lang): DBC (Vector CAN database) support

### Added

- **DBC support** (`.dbc`, slice 2 of structured-text addressing): CAN bus
  descriptions are now node-addressable. `BO_` messages index as `object`
  named by message name; `SG_` signals nest inside their message as `field`
  rows (one nested `node_id` per signal); `VAL_TABLE_`/`VAL_` enumerations
  as `enum`; `BA_DEF_`/`BA_` attributes as `pair`; `EV_` environment
  variables as `variable`. An agent can edit one signal of one message by
  handle without touching the rest of the file.

## [0.88.0] — 2026-07-04 — refactor(lang): one forgeql-lang-text crate for all structured-text formats

### Changed

- Consolidated the five structured-text language crates (`forgeql-lang-json`,
  `-yaml`, `-toml`, `-markdown`, `-xml`) into one **`forgeql-lang-text`**
  crate. Each format is now a module plus a `config/<lang>.json` kind map, so
  adding a new text extension is a three-step recipe in a single place. The
  `forgeql` and `forgeql-server` registries splice all text formats in with
  one `text_languages()` call — new formats are picked up automatically by
  both binaries. No behavior change: same extensions, same kinds, same names.

## [0.87.0] — 2026-07-04 — feat(lang): XML family support (arxml/xdm/ecuc)

### Added

- **XML family support** (`forgeql-lang-xml`, slice 1 of structured-text
  addressing): `.xml`, AUTOSAR `.arxml`, EB tresos `.xdm`/`.epc`/`.epd`,
  `.ecuc`, and `.odx` files are now node-addressable. Every element gets a
  nested `node_id` (like nested `if`/`for` blocks), named by a mechanical
  cascade: identifier attribute (`name`/`id`/`key`/`title`/`alias`,
  case-insensitive) → AUTOSAR `SHORT-NAME` child element → tag-name fallback,
  so ECU configuration containers can be located with `FIND symbols` and
  edited with `CHANGE NODE`/`INSERT NODE` instead of through a GUI.
  Attributes are not indexed as separate rows (token thrift on large arxml).

### Fixed

- `forgeql-server` now registers the same language set as the `forgeql`
  binary — JSON, YAML, and TOML were missing from the server registry, so it
  could not index files the MCP tool could.

## [0.86.2] — 2026-07-04 — fix(enrich): decl_distance O(n) on deeply-nested functions

### Fixed

- Indexing no longer spends unbounded single-core time on functions with very
  deep ASTs. `DeclDistanceEnricher` called `is_inside_parameter_list` for every
  identifier in a function body, and that predicate walks the ancestor chain up
  to the enclosing function — O(depth) per identifier, i.e. O(n²) on deeply
  nested bodies. The rustc parser stress test `survive-peano-lesson-queue.rs`
  (8 MB of thousands-deep nested `if/else`) pinned one core for over an hour.
  The body walk now skips parameter-list subtrees entirely (parameter
  identifiers were only visited to be excluded), removing the per-identifier
  ancestor walk. Results are unchanged; indexing that file's worst function
  drops from >90 s to 0.4 s.

## [0.86.1] — 2026-07-04 — fix(index): stack-overflow-safe indexing of deeply-nested source

### Fixed

- Indexing no longer aborts the whole process with a fatal stack overflow on
  deeply-nested source (the rustc tree, large C/C++). AST enrichers recurse over
  the syntax tree on rayon workers, whose default ~2 MiB stack could be exhausted
  by real-world tree depth. All parse+enrich passes now run on a dedicated
  `indexing_pool` whose workers get a 256 MiB stack, and the hottest recursive
  walker (`first_absorbed_toplevel_in_compound`) was rewritten to an explicit
  heap work stack.
- The **incremental reindex** paths (`SymbolTable::reindex_files` and
  `ColumnarStorage::reindex_files_impl`) now run their per-file parse+enrich on
  the same big-stack pool. Previously only the full initial build was protected,
  so a deeply-nested file that indexed fine on `USE` could still overflow the
  stack and crash the process when re-indexed after a `CHANGE`/`MOVE` edit or on
  `USE` reconnect.

## [0.86.0] — 2026-07-01 — feat(cli): --version for client/server + publish full binary set

### Added

- `forgeql-client` and `forgeql-server` now support `--version`, printing the
  workspace version (e.g. `forgeql-client 0.86.0`) like the `forgeql` tool
  already did.

### Fixed

- `ci-check.sh` now publishes the **full binary set** to the canonical bins
  directory on a green run. `cargo build --workspace` always built
  `forgeql-client` and `forgeql-server`, but the publish step only copied
  `forgeql`, so the client/server binaries in `/sstate-forgeql/bins/` went stale.
  The publish helper (`bc_publish_mcp_binary`) now atomically publishes
  `forgeql`, `forgeql-client`, and `forgeql-server` in lockstep.

## [0.85.0] — 2026-06-30 — chore(release): rollup release (first tag since v0.81.0)

This is the first tagged/published release since `v0.81.0`. It ships the
previously-merged-but-unreleased work — node-addressable `mod`/`type`
declarations (0.82.0), the JOB background scheduler slice 1 (0.83.0) and
slice 2's bounded worker pool + FIFO queue (0.84.0) — together with the change
below. See the per-version sections that follow for details.

### Changed

- Re-blessed the zephyr/pytorch golden values
  (`crates/forgeql/tests/golden.json`, 335 entries) to match the drifted
  external corpora — fixture values only, no engine or test-harness change.

## [0.84.0] — 2026-06-29 — feat(jobs): bounded worker pool + FIFO queue for JOB (scheduler slice 2)

### Added

- Background jobs now run through a **bounded worker pool**: at most
  `max_concurrent` jobs execute at once and the rest wait `Queued` in a FIFO
  queue, starting automatically as slots free. The cap comes from the
  `FORGEQL_MAX_CONCURRENT_JOBS` environment variable and defaults to `1`, so a
  burst of `JOB START` builds runs strictly serially instead of all at once —
  the backpressure that stops many parallel heavy builds from exhausting
  machine memory (Slice 2 of the server-side job scheduler).

### Changed

- `JobState` gains a `Queued` variant; `JOB STATUS` and `JOB LIST` report it
  while a job waits for a slot. `VERIFY build` is unchanged and still runs
  unbounded (deprecation comes later).

## [0.83.0] — 2026-06-29 — feat(jobs): JOB START/STATUS/LIST — background build jobs (scheduler slice 1)

### Added

- `JOB START '<label>'` / `JOB STATUS '<id>'` / `JOB LIST` — run a verify step as a
  detached background job that returns a job id immediately instead of blocking the
  request (Slice 1 of the server-side job scheduler). Verify steps gain an optional
  `weight` in `.forgeql.yaml` — a tier (`light`|`medium`|`heavy`) or an explicit
  `{cores, memory_mb, max_seconds}` map — recorded on each job for the future
  scheduler. `VERIFY build` is unchanged and runs alongside.

## [0.82.0] — 2026-06-24 — fix(index): make `mod` and `type` declarations node-addressable

### Fixed
- `mod` declarations and `type` aliases now carry node_ids, making them node-addressable for mutations (added to `is_addressable_fql_kind`).

## [0.81.1] — 2026-06-23 — ci(release): drop the hanging macOS build

### Fixed

- The release workflow's `build-macos` job (aarch64 + x86_64 Apple targets on
  GitHub-hosted macOS runners) hung indefinitely and froze the whole tagged
  release. Removed the job and dropped it from the `release` job's `needs`, so a
  `v*` tag now ships Linux (gnu + musl) and `.deb` artifacts without waiting on
  the Mac runners. No binary change. Notably this is the first release-workflow
  edit made entirely through `run_fql` — `.github/workflows/release.yml` (YAML)
  and `Cargo.toml` (TOML) are both node-addressable now. Re-add a
  timeout-guarded macOS job when the runner issue is resolved.
## [0.81.0] — 2026-06-22 — feat(lang): TOML support — `Cargo.toml` + `Cargo.lock` node-addressable

### Added

- New `forgeql-lang-toml` crate adds TOML as an indexed language via
  `tree-sitter-toml-ng`, registered for the `toml` and `lock` extensions. Since
  `Cargo.lock` is itself TOML (`[[package]]` tables), one grammar makes BOTH
  Cargo manifests node-addressable: a `version = "…"` pair, or a `[[package]]`
  entry, can now be located with FIND / SHOW and edited by `CHANGE NODE` instead
  of raw-text `CHANGE FILE` — so workspace and lockfile version bumps run by node.
  Each `pair` is indexed under its key; each `[table]` / `[[table-array]]` is
  named after its `name` / `id` / `key` member when present, else its header key.
  Config-driven like the other language plugins (`config/toml.json` kind_map),
  with zero `forgeql-core` changes; registered in the CLI language registry.
  Tests: `pairs_are_named_by_key`, `tables_named_by_header_or_member`,
  `embedded_config_is_valid` (parse real TOML, assert the naming).
## [0.80.9] — 2026-06-22 — fix(output): surface `lines_removed` in the default CSV output

### Fixed

- `compact_mutation` (the default compact-CSV renderer) hardcodes its rows and
  never emitted the `lines_removed` field added in 0.80.7, so the destructive-edit
  signal was visible only with `format=JSON` — invisible in the default output an
  agent actually reads. A signature-only `CHANGE NODE` that deletes a 20-line body
  now reports `lines_removed: 26` next to `lines_written: 6` in CSV too. Added the
  row beside `lines_written`; regression test `compact_mutation_surfaces_lines_removed`.
## [0.80.8] — 2026-06-22 — feat(undo): `UNDO [LAST-n]` command + per-session undo ring

### Added

- `UNDO [LAST-n]` reverses a recent mutation by restoring the exact pre-edit
  bytes that `apply()` already captures. Every mutation now writes a snapshot of
  its `TransformResult.originals` to a per-session ring (`.forgeql-undo-<n>`, 10
  deep, beside the SHOW MORE ring), reusing the same atomic `LAST-n` token as
  SHOW MORE. `UNDO` (= `LAST-0`) reverses the most recent mutation; `UNDO LAST-1`
  the two most recent, and so on. The restore reindexes the touched files and
  invalidates the commit gate exactly like a forward mutation — fully mechanical
  and language-agnostic, no engine intelligence. The ring is excluded from
  user-facing commits (`git::is_clean_commit_excluded`) and denied to user paths
  by the `.forgeql*` confinement, and dies with the worktree. New `undo` module;
  grammar `undo_stmt` / `undo_last`; `ForgeQLIR::Undo`; `ForgeQLEngine::exec_undo`.
  Tests: `snapshot_roundtrips_paths_and_bytes`,
  `ring_pages_previous_snapshots_as_last_n`, `missing_slot_is_none`,
  `empty_originals_writes_nothing`, `parse_undo_last_n`,
  `undo_restores_previous_file_contents`, `undo_with_no_snapshot_errors`.
## [0.80.7] — 2026-06-22 — feat(mutation): report `lines_removed` (destructive-edit signal)

### Added

- `MutationResult` now carries `lines_removed` alongside `lines_written`: the
  number of original source lines each edit overwrote, counted mechanically
  against the pre-edit bytes `apply()` already captures. Paired with
  `lines_written` it is the loudest language-agnostic signal of a destructive
  edit — replacing a 60-line node with a 6-line body reports
  `lines_removed: 54, lines_written: 6`. This surfaces the
  CHANGE-NODE-on-a-folded-function footgun (the node byte range spans the whole
  body even though `DEPTH 0` shows only the signature) as a structured field
  the agent can assert on, with no language knowledge in the engine. New
  `transforms::lines_removed` helper; tests
  `lines_removed_counts_original_span_lines`,
  `lines_removed_is_zero_for_pure_insertion`.
## [0.80.6] — 2026-06-21 — fix(security): symlink-safe + denylisted path confinement (BUG-018)

### Security

- `Workspace::safe_path` now closes two confinement holes. It previously did
  only a lexical `starts_with(root)` after `normalise_path`, which (a) does not
  follow symlinks, so a symlinked directory inside the worktree could point out
  of it, and (b) had no denylist, so the repo's own `.git` store and ForgeQL's
  runtime/control files (`.forgeql*`) were readable/writable through a query.
  Added: a root-level `.git` / `.forgeql*` denylist (precise — `.gitignore`
  stays allowed; `..` tricks that resolve back to a protected entry are caught
  after normalisation), and symlink-safe containment that canonicalizes the
  deepest existing ancestor and verifies it stays inside the canonical root
  (skipped when the root cannot be canonicalised, e.g. a virtual unit-test
  root). Tests: `safe_path_rejects_dot_git`,
  `safe_path_rejects_forgeql_runtime_files`,
  `safe_path_allows_gitignore_not_dot_git`,
  `safe_path_dot_dot_into_dot_git_rejected`, `safe_path_rejects_symlink_escape`.

## [0.80.5] — 2026-06-21 — feat(diff): inline node addresses on the mutation diff (BUG-022)

### Changed

- Every present line of a mutation diff (added + context lines) now carries an
  inline `node_id(offset)` handle, so the agent can correct the breakage the
  diff reveals with a copy-paste `CHANGE NODE 'node_id(offset)'` — no follow-up
  `SHOW LINES` round-trip. Removed lines have no post-edit position and stay
  unaddressed. Unindexed files degrade gracefully to an unaddressed diff.
- The mutation diff is now built **after** apply + reindex (previously before),
  so post-edit node ordinals exist to address against. `apply()`'s captured
  `originals` supply the pre-edit side; disk supplies the post-edit side. Both
  mutation paths (`exec_mutation` and `apply_plan`) share one helper,
  `build_post_edit_diff`, over the new `transforms::diff::compact_diff_addressed`.
  Test: `compact_diff_addressed_prefixes_present_lines_with_handles`.

## [0.80.4] — 2026-06-21 — fix(diff): boundary-visible mutation diff (BUG-022)

### Changed

- The compact mutation diff now shows **context-before** lines (the unchanged
  lines immediately above a change), not just context-after. An `INSERT` after
  the last element of a collection no longer hides the breakage: the prior
  element — which a mechanical, language-agnostic splice leaves without the
  now-required trailing separator (invalid JSON, BUG-022) — is shown directly
  above the inserted `+` line, so the agent sees the corruption and self-corrects.
  The engine stays mechanical; only diff *visibility* improved.
- `max_line_width` widened 40 → 120 so a changed line is no longer truncated
  mid-content (e.g. `{ hered…teral }`), which hid the actual edit.
- The many-region render no longer shows only the first and last hunk while
  **silently dropping every middle region** (a 28-edit rename showed 2 of 28).
  It now emits leading regions in order until the line budget is reached, then a
  one-line summary counting the remaining regions and lines — never a silent drop.
  Tests: `compact_preview_context_before_surfaces_prior_line`,
  `compact_preview_multi_hunk_shows_leading_regions_and_summary` (`transforms/diff.rs`).

## [0.80.3] — 2026-06-21 — feat(output): LAST-n ring buffer for SHOW MORE

### Added

- The `SHOW MORE` buffer is now a **`LAST-n` ring** (`RING_SIZE` = 5 slots,
  written in the worktree as `.forgeql-showmore-<n>`). Each buffered command
  pushes the existing slots back one and writes the new `LAST-0`, so a recently
  buffered output (e.g. a mutation diff) survives a subsequent SHOW/FIND that
  would previously have overwritten the single buffer. `SHOW MORE LAST-1` pages
  the previous buffer; a bare `SHOW MORE` is `LAST-0`. The selector is an atomic
  grammar token (`@{ "LAST-" ~ ASCII_DIGIT+ }`) so `SHOW MORE LAST-1 1-1000`
  parses as selector `LAST-1` + range `1-1000` without colliding with the range
  hyphen. The ring slots are excluded from user-facing commits by prefix
  (`git::is_clean_commit_excluded`). Tests: `ring_pages_previous_buffers_as_last_n`
  (`showmore.rs`) and `parse_show_more_last_n` (`parser/tests.rs`).

## [0.80.2] — 2026-06-21 — feat(output): universal SHOW MORE cap for SHOW and FIND

### Changed

- `SHOW` and `FIND` output now flows through the same `SHOW MORE` buffer as
  `VERIFY build` / `RUN`: it is windowed to the session's inline cap (default
  40 lines) and the full output is buffered for paging, replacing the old hard
  block that returned **zero** lines with a "Blocked" guidance message once an
  unbounded `SHOW` exceeded the cap. Over-cap reads now return the first page
  plus a `SHOW MORE` hint; the full output is recoverable with `SHOW MORE 1-N`,
  `HEAD n`, `TAIL n`, or `WHERE text MATCHES '…'`. `format=JSON` stays exempt
  (full structured dump). The inline cap is now purely a presentation concern
  applied at the single CSV render boundary (`mcp.rs::finalize_csv`); the
  engine's `apply_show_lines_cap` only bounds the result *set* (LIMIT/OFFSET)
  and applies the budget-critical cap. Test:
  `show_and_query_route_through_show_more_buffer` (`crates/forgeql/src/mcp.rs`).

## [0.80.1] — 2026-06-21 — feat(query-logger): per-agent `session` column + larger preview cap

### Added

- The query log CSV (`{data_dir}/log/{source}.csv`) now leads with a `session`
  column identifying which agent issued each command. Previously every agent
  writing to the same per-source file was indistinguishable — the rows got
  mixed together. The value is the full session id (`user:source:branch:alias`)
  with its `source` component removed (the file is already named per source), so
  it reads `user:branch:alias`; a malformed token falls back to the raw value.
  Wired through `QueryLogger::log` and both call sites (`mcp::run_fql`,
  `execute_and_print`).

### Changed

- `LOG_PREVIEW_MAX_CHARS` doubled 160 → 320 so the `command_preview` column no
  longer truncates the meaningful tail of longer statements.

### Tests

- `query_logger_creates_csv_with_header` now asserts `session` is the first
  column and the data row carries the source-stripped id; new
  `query_logger_session_column_strips_source` covers both the strip and the
  malformed-token fallback; the field-index assertions in the source-line tests
  shifted by one.

## [0.80.0] — 2026-06-21 — feat(engine): RUN verb for allowlisted command templates

### Added

- `RUN '<step>' <args…>` runs a named command template from a new `run_steps:`
  section in `.forgeql.yaml` — the typed, auth-gateable counterpart to VERIFY's
  open allowlist. A template declares positional params: `ident` args are
  substituted into the command (`$name`, validated `[A-Za-z0-9_.-]+`,
  injection-safe); `string` args are bound to the subprocess **stdin** and are
  never spliced into the shell, so a payload with quotes/spaces/metacharacters
  cannot inject shell syntax. `run_steps` are frozen at `USE` like `verify_steps`,
  so a later CHANGE cannot tamper a template, and the output flows through the
  `SHOW MORE` buffer. This enables a self-test loop: a `run_fql` template can pipe
  a query into the freshly built `$FORGEQL_BUILD_DIR/debug/forgeql`. The
  per-principal capability gate for the HTTP server is deferred; the local stdio
  path is anonymous + allowlist-only. Tests: `resolve_template` units (ident
  substitution, longest-name-first, string→stdin, arity, injection) and RUN
  end-to-end (ident, stdin bind, unknown step, injection) in `tests/commit_gate.rs`.

## [0.79.0] — 2026-06-20 — feat(verify): typed parameters and per-session env vars for VERIFY steps

### Added
- `VERIFY build '<step>' '<arg>'…` now passes positional arguments to a step.
  A step declares typed params in `.forgeql.yaml` (`params: [{ name: target,
  type: ident }]`); each `$name` in the step's `command` is substituted after
  the argument count and type are validated. The only type today is `ident`
  (`[A-Za-z0-9_.-]+`), and grammar args are quoted-only, so a substituted value
  can never inject shell metacharacters or swallow a following statement's
  keyword — non-ident args are rejected before the command runs. Steps without
  params are unchanged. Tests: param substitution, arity check, injection
  rejection (`tests/commit_gate.rs`).
- VERIFY (and future RUN) steps now receive per-session environment variables:
  `FORGEQL_BUILD_DIR` (`<worktree>/target` — per-worktree so concurrent agents
  never share build artifacts; consume as `cargo --target-dir $FORGEQL_BUILD_DIR`,
  not `CARGO_TARGET_DIR`, which would defeat sccache) plus `FORGEQL_SESSION_ID`,
  `FORGEQL_SOURCE`, `FORGEQL_BRANCH`, `FORGEQL_ALIAS`, and `FORGEQL_WORKTREE`.
  A step can locate its freshly-built binary at `$FORGEQL_BUILD_DIR/debug/forgeql`.
  Test: a step echoes `$FORGEQL_SOURCE` / `$FORGEQL_BUILD_DIR` and asserts both
  are present.

## [0.78.0] — 2026-06-20 — feat(engine): gate COMMIT on commit_gate verify steps

### Added
- Verify steps in `.forgeql.yaml` may now set `commit_gate: true`. When set,
  `COMMIT` is refused until that step has passed **since the most recent
  mutation** — and any edit after a pass re-blocks the commit until the step is
  re-run. Several steps may be gated; every gated step must pass (logical AND).
  Steps without the flag never gate commits. This lets a project require, e.g.,
  a green test/lint run to be reproduced against the exact tree being committed,
  so a commit can never record an unvalidated state. Tests:
  `commit_is_gated_until_the_gated_step_passes`,
  `an_edit_after_the_gate_re_blocks_commit` (`tests/commit_gate.rs`).

## [0.77.7] — 2026-06-20 — fix(worktree): unify worktree+branch teardown; GC empty research sessions

A worktree and its branch are now treated as a single unit: every teardown path
removes both together (never orphaning one), and a session is kept past its TTL
only when it carries real committed work.

### Fixed
- Removing a worktree no longer leaks its git branch. Background warming
  (`warm_snapshot`) and startup stale-worktree pruning each called
  `worktree::remove` (which deletes the checkout) without deleting the backing
  branch, so every warmed HEAD leaked a `fql/__warm__/…` ref and stale named
  sessions leaked a `fql/{user}/…` ref into the bare repo. Both now go through a
  single helper that removes the worktree and deletes its branch together.

### Changed
- Added `git::worktree::remove_with_branch(repo, wt_path, name, known_branch)` —
  the single teardown entry point for every worktree the server creates (live
  sessions, TTL eviction, startup stale-pruning, background warming). It
  resolves the branch in priority order — caller-known name → live HEAD read
  from the checkout → legacy `forgeql/<name>` fallback — so a custom session
  branch is deleted by its true name even after the checkout directory is gone.
  `teardown_worktree`, TTL eviction, the startup prune and the warmer all route
  through it. Regression tests: `remove_with_branch_removes_worktree_and_branch`,
  `remove_with_branch_resolves_live_head_when_unknown`.
- TTL eviction now garbage-collects **research** sessions. When an idle session
  expires, its branch is diffed against its base via `git::source_changes`
  (ignoring control files): if there are no committed changes the whole unit
  (worktree + branch) is removed; if the branch carries real work it is retained
  in full for manual review. Previously every `USE … AS` branch was kept
  unconditionally — accumulating indefinitely — because the work-detection that
  `source_changes` provides was orphaned when `DISCONNECT` was removed. On a
  `source_changes` error the branch is conservatively kept so work is never lost.

## [0.77.6] — 2026-06-20 — fix(show): SHOW body for attribute-decorated functions; feat(parser): heredoc in COMMIT MESSAGE

### Fixed
- `SHOW … body` returned "function definition not found in AST" for any function carrying an attribute/decorator (`#[test]`, `#[inline]`, `#[must_use]`, multi-line `#[expect(...)]`, …). The index folds a symbol span back over its contiguous leading attribute siblings (`attr_extended_start`), so the stored start byte points at the attribute rather than the `function_item`, and all three existing resolution strategies missed it. Added a fourth strategy that locates the function-kind node whose own attribute-extended start equals `def_start` — the exact inverse of the index fold — so it never matches an unrelated function. `FIND`, `SHOW outline` and `SHOW context` were unaffected. Regression test: `show_body_resolves_attributed_function`.

### Added
- `COMMIT MESSAGE` now accepts heredoc syntax, not just a single-quoted string. A commit message containing an apostrophe or single quote (ordinary in prose) previously could not be written through `run_fql` and had to be reworded. `commit_stmt` is now routed through the same `content_value` rule (`heredoc | string_literal`) that the `CHANGE` forms use. Parser test: `parse_commit_message_heredoc_with_apostrophes`.

## [0.77.5] — 2026-06-20 — fix(tests): parity session id + de-hardcode the session user

### Fixed
- `parity_find` passed the bare alias `'parity'` as the `session_id` on every `run_fql` call, which the server rejects (`invalid session id 'parity': expected 'user:source:branch:alias'`). Root cause: its `run_fql` helper read `content[0]`, but `USE` returns a human-readable "store this session_id" warning as the first block and the JSON payload (carrying the real `user:source:branch:alias` token) as the last block. It now reads the last content block — like the zephyr/golden harnesses — and uses the returned token. The test only surfaced when `FORGEQL_DATA_DIR` is set (otherwise it skips), so a normal pre-commit run never hit it.

### Changed
- `golden_test` and `zephyr_golden` teardown now derive the session user from `forgeql_core::auth::auth(AuthContext::Mcp)` instead of the literal `"anonymous"`, matching what the MCP server assigns and keeping the single source of truth in `forgeql_core::auth` (the literal is documented to live only there).

## [0.77.4] — 2026-06-19 — fix(session): teardown deletes the real session branch instead of orphaning it

### Fixed
- `session::teardown_worktree` deleted a reconstructed `forgeql/<wt_name>` branch that never matched the actual `fql/{user}/{source}/{branch}/{alias}` ref created by `USE … AS`, so every torn-down session left its branch behind in the bare repo (the accumulating `frozen/gt-*` refs). It now reads the worktree's real checked-out branch via `branch_of_worktree` before removal and deletes that exact ref, falling back to the legacy name only when HEAD is detached. The server's TTL-eviction path still intentionally keeps named branches for review.
- Regression test `teardown_deletes_custom_session_branch` covers the `fql/…` custom-branch scheme that the prior test missed.

## [0.77.3] — 2026-06-19 — test(golden): add 16 enrichment golden suites

### Added
- 16 data-driven enrichment golden suites under `crates/forgeql/tests/golden/*.json`, one per enricher: `enrich_naming`, `enrich_comments`, `enrich_scope`, `enrich_metrics`, `enrich_control_flow`, `enrich_operators`, `enrich_casts`, `enrich_redundancy`, `enrich_members`, `enrich_decl_distance`, `enrich_escape`, `enrich_shadow`, `enrich_unused_param`, `enrich_fallthrough`, `enrich_recursion`, `enrich_todo`
- Each suite queries frozen `forgeql-pub` branches and asserts enrichment-field classification, filtering and negative cases against the golden harness v2

## [0.77.2] — 2026-06-18 — feat(server): bearer-token authentication with admin-gated source management

### Added
- `crates/forgeql-server/src/auth.rs` — file-backed `TokenStore` mapping bearer tokens to a `Principal` (user + role); loaded at startup via `--auth-file`/`FORGEQL_AUTH_FILE`
- `Authorization: Bearer <token>` header parsing on every MCP request; unknown or missing token resolves to anonymous/normal role
- Admin tokens may run `CREATE SOURCE` and `REFRESH SOURCE`; normal and anonymous callers receive a clear rejection instead of a blanket ban
- `forgeql-client` gains `--token`/`FORGEQL_TOKEN` to send a bearer token on each request
- `forgeql-core` and the `forgeql` binary are untouched — auth applies only to the HTTP server

## [0.77.1] — 2026-06-18 — fix(index): attribute span folding and guard detection are now language-agnostic

### Fixed
- `attr_extended_start` and `collect_attribute_guard_frames` hardcoded Rust's `"attribute_item"` tree-sitter kind instead of reading the language config — Python decorators (`@...`) were silently ignored when computing a function's start line and when detecting attribute guards. Both functions now read `LanguageConfig::decorator_kind()` from the language JSON config; languages with no decorator kind (C, JSON, Markdown) exit early without scanning.

## [0.77.0] — 2026-06-17 — feat(golden): add data-driven enrichment golden harness v2

### Added
- Golden test harness v2 (`crates/forgeql/tests/golden_test.rs`) driven by JSON suite files under `tests/golden/*.json`; replaces the old `.golden` format
- `enrich_is_magic.json` — 16 enrichment test cases across C, C++, Python and Rust frozen branches
- `node_mutations.json` — 4 mutation/transaction test cases (delete, change, nested rollback, error on bare rollback)
- `tests/golden/README.md` documenting suite schema and assert fields
- Mutation mode (`"mode": "rw"`) and `capture` mechanism for runtime node_id resolution

## [0.76.46] — 2026-06-17 — feat(golden): add data-driven enrichment golden harness v2

### Added
- Golden test harness v2 (`crates/forgeql/tests/golden_test.rs`) driven by JSON suite files under `tests/golden/*.json`; replaces the old `.golden` format
- `enrich_is_magic.json` — 16 enrichment test cases across C, C++, Python and Rust frozen branches
- `node_mutations.json` — 4 mutation/transaction test cases (delete, change, nested rollback, error on bare rollback)
- `tests/golden/README.md` documenting suite schema and assert fields
- Mutation mode (`"mode": "rw"`) and `capture` mechanism for runtime node_id resolution

## [0.76.45] — 2026-06-17 — comment_block rows show a descriptive label in SHOW outline

### Changed

- **`comment_block` rows in `SHOW outline` now show a descriptive label** — the first member
  snippet plus the member count (e.g. `/// Convert the inner content… (×8)`) — instead of the
  bare kind string `comment_block` (P5). The label is stored as a display-only `block_label`
  field; the row identity name stays `comment_block`, so reindex node-id stability and
  `WHERE fql_kind = 'comment_block'` filtering are unchanged. `ENRICH_VER` 19 → 20.

### Fixed

- Added a regression test (`block_node_id_survives_editing_its_own_member`) confirming that
  editing a comment inside a block does not churn that block's node id when a sibling block
  shares the same parent (BUG-021 content-edit case — already handled by the `content_hash`
  disambiguator + source-order reuse; now locked by a test).
## [0.76.44] — 2026-06-17 — `SHOW outline … ALL` emits every node in source order

### Fixed

- **`SHOW outline … ALL` now emits every node in strict source order, nested by span
  containment**, instead of promoting analysis-only rows (type refs, casts, numbers — which
  carry no ordinal) to depth-0 roots and flushing them after their enclosing subtree, where
  they appeared at the end of the file or in the middle, out of line order (P4). The ordinal
  tree cannot place an ord-less row (it is never a parent key); the new `all` path
  (`push_outline_all_source_order`) sorts rows by byte span and derives depth from a
  containment stack, so block members nest under their block and every row lands at its true
  position. `SHOW outline '<id>' ALL` restricts to the node subtree. Default (structural)
  outline is unchanged. Query-only — no `ENRICH_VER` bump.
## [0.76.43] — 2026-06-17 — Comment names render as a first-line snippet, not `len:N`

### Changed

- **A multi-line comment name now renders as a single-line snippet** (first line, trimmed,
  truncated to 120 chars with a trailing `…` when content is dropped) instead of the opaque
  `len:N` placeholder. Applied uniformly across `FIND` (`result::compact_name`, via the new
  `comment_snippet` helper) and `SHOW outline` (`compact_outline` now calls `compact_name`),
  so `//`, `///`, and `/* */` comments all surface the same readable hint regardless of style
  or renderer (P3). The name column stays an orientation hint; full text is read via `SHOW NODE`.
## [0.76.42] — 2026-06-17 — has_doc detects docs through interposed attributes

### Fixed

- **`has_doc` is now true for a documented item that has an interposed attribute/decorator.**
  `CommentEnricher::enrich_row` walks back over leading attribute siblings (e.g. Rust `#[...]`)
  before testing for a preceding doc comment, mirroring the indexer leading-attribute span fold.
  Previously a `///` block separated from its `fn` by `#[allow(...)]`/`#[must_use]` reported
  `has_doc = false` (the attribute was the function prev-sibling, not the doc comment) (P6).
- Bumped `ENRICH_VER` 18 → 19 (`storage/columnar/mod.rs`): the `has_doc` change alters index
  output, so cached segments must cold-reindex.
## [0.76.41] — 2026-06-17 — SHOW LINES/NODE surface block members under the block handle

### Fixed

- **`SHOW LINES` and `SHOW NODE CONTENT` now surface block members under the shared block
  handle.** A line belonging to a comment block resolves to the block id with a block-relative
  offset (e.g. `…0006` at offset 1/2/3), matching what `FIND` and `SHOW outline` already show.
  Previously these paths returned each member raw per-line handle (`…0007/…0008/…0009`, all at
  offset 1), so the same line had two different addresses depending on the command (P1).
  Extracted `node_id::block_node_id` as the shared helper that `surface_block_id` now builds on.
## [0.76.40] — 2026-06-16 — Block grouping: single-line offsets for doc/block comments

### Fixed

- **A one-line doc (`///`) or block (`/* */`) comment now surfaces as a single offset, not a 2-line range.** Such comments include the trailing newline in their tree-sitter span (`end_position` is column 0 of the next line), so the member offset was computed one line too long — e.g. `block(1-2)`, `block(2-3)` instead of `block(1)`, `block(2)`. `collect_nodes` (`crates/forgeql-core/src/ast/index/file_indexer.rs`) now clamps the member's end to its last content line. `ENRICH_VER` 17 → 18.

### Tests

- `doc_comment_block_members_get_single_line_offsets` (`ast/index.rs`).

## [0.76.39] — 2026-06-16 — CHANGE NODE returns the post-edit handle even when it churns

### Fixed

- **`CHANGE NODE` now reports the correct `new_node_id` when an edit changes a node's identity.** It previously re-resolved the *old* node id after the edit, which returned a stale/wrong handle whenever the edit shifted the node's `content_hash` and the remapper assigned a new ordinal (e.g. editing a `comment_block`'s text when sibling blocks exist, or a rename). `exec_change_node` now re-resolves by the base node's **start line** via `find_node_id_at_line`, so the returned handle is the node's current id even after churn. `NodeSpan` carries `node_line` (the base node's start line) instead of `base_id`. Behaviour is unchanged for ordinary edits (the line still maps to the same node) — the existing golden `GS` tests, which assert the returned handle, continue to pass.

## [0.76.38] — 2026-06-16 — Block alias surfacing unified across FIND and SHOW outline

### Changed

- **One shared helper now surfaces block-member handles everywhere.** Extracted `node_id::surface_block_id(own_id, block_ord, block_off)` (`crates/forgeql-core/src/node_id.rs`): when a row carries `block_ord`/`block_off` it returns `block_id(offset)` (reusing the member's own segment prefix), otherwise the id unchanged. `SymbolRow::from_match_with_ctx` (FIND, via `surface_block_alias`) and `push_outline_tree` (`SHOW outline`, reading the fields with `SegmentReader::extra_field_str`) both call it, so a block member now surfaces the **same way** in `FIND` and `SHOW outline` instead of only in `FIND`.

### Tests

- `surface_block_id_builds_handle_for_members`, `surface_block_id_passes_through_non_members` (`node_id.rs`); the existing `surface_block_alias_*` tests now exercise the shared helper through the FIND wrapper.

## [0.76.37] — 2026-06-16 — Node addressing: DELETE NODE offsets + shared resolver

### Fixed

- **`DELETE NODE 'id(n-m)'` now works.** Previously DELETE rejected the offset suffix ("invalid ordinal") even though SHOW NODE and CHANGE NODE accepted it, so a member surfaced as `block(1)` could not be deleted by that handle. Offset addressing is now extracted into one shared helper, `Engine::resolve_node_span` (`crates/forgeql-core/src/engine/exec_change.rs`), which both `exec_change_node` and `exec_delete_node` route through: `split_node_offset` → `resolve_node` → `offset_lines`. A whole-node delete still absorbs trailing blank lines; an offset delete removes exactly the addressed line range.

### Changed

- `exec_change_node` and `exec_delete_node` no longer duplicate the offset-resolution sequence — it lives in `resolve_node_span` (returns the file, target line span, the whole-node end line, and the offset flag). The `(n-m)` parsing (`split_node_offset`/`offset_lines`) is unchanged and already unit-tested; CHANGE NODE offset behavior is exercised by the existing golden `GS` tests through the new helper.

## [0.76.36] — 2026-06-16 — Block grouping: stable block node ids

### Fixed

- **`comment_block` node ids no longer churn when sibling blocks are added or removed.** `emit_block_row` (`crates/forgeql-core/src/ast/index/file_indexer.rs`) now stores the block's `content_hash` as a row field, the same way `build_row_fields` does for every other row. The reindex hint (`OrdinalHint`, built in `build.rs`) reads `content_hash` from that field to disambiguate nodes that share a name — and since all blocks share the constant `comment_block` name, without it the remapper could not tell two blocks apart and would slide one into the other's freed ordinal. Members were already stable; this makes the block handles stable too. `ENRICH_VER` 16 → 17.

### Tests

- `block_node_ids_survive_deletion_of_a_sibling_block` (`ast/index.rs`) — indexes two comment blocks, drops one via the reindex/remapper path, and asserts the survivor keeps its ordinal.

## [0.76.35] — 2026-06-15 — Node arch: block grouping (Stage 2 — member alias surfacing)

### Added

- **Block members now surface as `block_id(offset)`** in `FIND`/`SHOW` output. At index time each member of a block is tagged with `block_ord` (the block node's 4-digit ordinal) and `block_off` (the member's 1-based offset within the block); `SymbolRow::from_match_with_ctx` (`crates/forgeql-core/src/result.rs`) then renders the member's handle as `block_id(offset)` instead of its own node id, reusing the member's own segment prefix so only the ordinal + offset change. The member's own node id still resolves under the hood — this only changes what the discovery surfaces display, nudging agents toward the block handle. `ENRICH_VER` 15 → 16.

### Changed

- `emit_block_row` returns the block ordinal; `collect_nodes` tracks the active block as `ActiveBlock` (ord suffix, start line, end byte, member kind) and computes a per-member `BlockTag` threaded through `process_node_rows` → `emit_addressable_row` (`crates/forgeql-core/src/ast/index/file_indexer.rs`).

### Tests

- `block_members_carry_block_alias_fields` (`ast/index.rs`); `surface_block_alias_builds_block_handle_for_members`, `surface_block_alias_passes_through_non_members` (`result/tests.rs`).

### Notes

- Like Stage 1, only Rust is enabled, so the C/C++ golden corpus is unaffected. The surfacing is backend-agnostic (projection layer) and only triggers on rows carrying the block fields, so `overlay_parity` (which compares raw `SymbolMatch`) is unchanged. `SHOW outline` member entries still show their own node id — extending the alias there, and enabling other languages, remain follow-ups (each gated on a reviewed golden rebaseline).

## [0.76.34] — 2026-06-15 — Node arch: block grouping (Stage 1 — comment blocks)

### Added

- **Configurable block grouping** (`block_groups` in the language JSON). A run of adjacent same-kind sibling members (e.g. comments) is now spanned by a synthetic, **childless** "block" node that shares the members' parent and gives one addressable handle over the whole run — `SHOW`/`CHANGE`/`DELETE NODE` on the block, or `block(n-m)` to splice a sub-line range. The individual member rows are emitted unchanged and keep their own node ids; only the block is added, so member node ids do not move. Adjacency bridges blank lines for free (blank lines are not tree nodes, so a node of a different kind is what ends a run); `split_on_attr` keeps `///` doc runs and `//` line runs in separate blocks. New `BlockGroupSpec` (`crates/forgeql-core/src/ast/lang.rs`), `LanguageConfig::block_groups`/`block_group_for_member` (`lang/config.rs`), `BlockGroupJson` (`lang_json.rs`), and `block_group_key`/`scan_block_run`/`emit_block_row` + `collect_nodes` wiring (`ast/index/file_indexer.rs`). `ENRICH_VER` 14 → 15.
- **Comments wired as the first consumer** (`crates/forgeql-lang-rust/config/rust.json`): a run of 2+ adjacent same-style comments forms a `comment_block`. Enabling cpp/c/python is a one-line config addition each (deferred — each needs its own golden rebaseline).

### Tests

- `comment_run_births_a_childless_block`, `comment_block_bridges_blank_lines`, `comment_block_splits_on_style`, `single_comment_gets_no_block` (`crates/forgeql-core/src/ast/index.rs`).

### Notes

- Stage 1 enables block grouping only for Rust, so the C/C++ golden corpus is unaffected. Stage 2 (surfacing a member node id as `block(offset)` via an alias in `FIND`/`SHOW`) and enabling other languages are follow-ups, each gated on a reviewed golden rebaseline.

## [0.76.33] — 2026-06-15 — Node arch: explicit self-row flag (hardening)

### Changed

- **Control-flow self-row detection is now explicit** (`crates/forgeql-core/src/ast/enrich/mod.rs`, `crates/forgeql-core/src/ast/index/file_indexer.rs`). `ExtraRow` gains an `is_self_row: bool` field; enrichers that emit the row representing the visited node itself set it `true`, and `emit_extra_rows` recovers the node's own ordinal from the first row whose `is_self_row` is set. This replaces the implicit `extra.byte_range == node.byte_range()` equality check. Behaviour is unchanged — every current self-row already spans the node's exact byte range — but the contract is now declared at the enricher call site instead of inferred from a range comparison, so a future enricher emitting a synthetic same-span row (a scope wrapper, derived symbol, or usage) can no longer be silently mistaken for the node itself.

## [0.76.32] — 2026-06-15 — Node arch: branches-as-parents (§4.1)

### Changed

- Control-flow nodes (`if`/`while`/`for`/`switch`/`do`) are now the **parents of
  their body statements** instead of the enclosing function (plan §4.1). In the
  DFS, `process_node_rows` promotes the (nameless) control-flow row — emitted by
  `emit_extra_rows`, which now returns the whole-node row's ordinal — to the
  current node for descent. Keyed on `config.is_control_flow_kind` / `map_kind`
  (universal `fql_kind`), so it applies to every language with no per-language
  code. Nav pointers self-correct from `parent_ordinal`.
- Bumped `ENRICH_VER` 13 → 14 (`storage/columnar/mod.rs`): the new parent shape
  requires a cold reindex so cached flat-graph segments don't keep the old
  flat parenting. Node-ids are unchanged in a fresh index (emission order is
  identical); only parent/outline structure changes — golden-neutral (329/0).

### Tests

- `control_flow_node_parents_its_body` — a statement inside an `if` parents to
  the if-node, not the function.
- `control_flow_body_preserves_sibling_node_ids_across_unrelated_edit` — building
  an `OrdinalRemapper` from a nested index and re-indexing a drifted file keeps
  the second if's node-id (node-id survival holds under the new model).
## [0.76.31] — 2026-06-14 — Node arch: DELETE NODE absorbs trailing blank lines

### Changed

- `engine::exec_change`: `DELETE NODE` now extends its delete range forward over the contiguous
  run of blank lines immediately following the node, so deleting a node also removes its trailing
  blank separator instead of leaving a stray blank (no accumulation). New pure helper
  `absorb_trailing_blank_lines` widens only the DELETE extent — whitespace is not part of the
  node's span/rev, and `CHANGE NODE` / explicit line-range deletes are unaffected. Best-effort:
  a read failure or a non-blank next line leaves the range unchanged. This removes the `fmt-apply`
  round-trips that node deletes used to force.

### Added

- `absorb_trailing_blank_lines_extends_over_blank_run` unit test (no-trailing, single/multi blank
  run, last-line, EOF-blank, whitespace-only-line cases).

## [0.76.30] — 2026-06-14 — Node arch (step 1): fold leading attributes into the node span

### Changed

- `ast::index`: a node's span (`byte_range` / `line` / `rev`) now folds in its contiguous
  leading attribute items (`#[...]`). New `attr_extended_start` walks `prev_named_sibling`
  matching `attribute_item` (mirrors `collect_attribute_guard_frames`) and extends the span back
  to the first attribute, so `rev` covers attributes and edits/`IF REV` protect them. Ordinal
  matching keeps using the unextended `content_hash`, so attribute edits do **not** churn node_ids.
  First step of the nested node-id re-architecture (see `plan-node-id-rearchitecture.md`).
  Currently folds Rust attributes only (other languages' attribute kinds are left unchanged);
  golden 329/0, no corpus churn.

### Added

- `index::tests::leading_attribute_folds_into_node_span` — indexes `#[derive(Clone)]\nstruct
  Widget;` and asserts the row reports line 1 / byte 0 (the attribute), not the `struct` keyword.

## [0.76.29] — 2026-06-13 — Refactor: decompose `SegmentReader::open`

### Changed

- `storage::columnar::segment_reader`: split the 149-line `SegmentReader::open` into
  `map_and_validate` (sections 1–2: mmap + outer FQSF magic/version/endianness checks) and
  `parse_header_blob` (sections 4–5: inner FQSG header decode + extra-column collection,
  returning a `HeaderFields` struct). `open` is now a ~53-line section-by-section pipeline.
  Pure refactor — segment loading unchanged (golden 329/0).

## [0.76.28] — 2026-06-13 — Refactor: decompose `parse_change`

### Changed

- `parser`: extracted the ~80-line `change_target` match out of the 113-line `parse_change`
  into `parse_change_target`, and deduplicated the four line-number parses behind a `next_usize`
  helper. `parse_change` is now a ~34-line file-list → target → clauses pipeline. Pure refactor —
  parse output unchanged (golden 329/0).

## [0.76.27] — 2026-06-13 — Refactor: decompose columnar `show_outline_for_file_impl`

### Changed

- `storage::columnar` query: split the 116-line `show_outline_for_file_impl` into the two
  outline forms it dispatches — `outline_subtree` (node_id → that node + descendants) and
  `outline_glob` (file/glob, committed-authoritative with dirty-overlay fallback). The method
  is now a ~22-line dispatcher that wraps the chosen `results` in the response JSON. Pure
  refactor — outline output unchanged (golden 329/0).

## [0.76.26] — 2026-06-13 — Refactor: decompose columnar `find_node_impl`

### Changed

- `storage::columnar` query: extracted the ~85-line committed-path body of `find_node_impl`
  into `build_committed_node_result` (live-row proximity lookup, stale-byte / deleted-node
  fallback, and `FindNodeResult` construction). `find_node_impl` is now a ~42-line
  dirty-first → committed → dirty dispatch. Pure refactor — node resolution unchanged
  (golden 329/0).

## [0.76.25] — 2026-06-13 — Refactor: decompose columnar `warm_or_open`

### Changed

- `storage::columnar` commit: split the 128-line `warm_or_open` into helpers — `finish_open`
  (the "open segments + construct + load delta" triple shared by all three return paths) and
  `build_overlay` (the lock-guarded inline-vs-shadow-write build). `warm_or_open` is now a
  ~63-line fast-path / lock / final-open skeleton. Removed the now-unfulfilled `too_many_lines`
  expectation. Pure refactor — overlay output unchanged (golden 329/0).

## [0.76.24] — 2026-06-13 — Refactor: decompose `make_inline_ctx`

### Changed

- `storage::columnar::build_context`: lifted the ~108-line `emit_fn` closure body out of the
  133-line `make_inline_ctx` into associated helpers — `emit_inline_segment` (orchestrator:
  path derivation, segment-map registration, idempotency, flush), `populate_inline_builder`
  (per-row emit + field collection), and `fill_inline_navigation` (first-child / sibling
  post-pass). `make_inline_ctx` is now a ~33-line wiring function whose closure just delegates.
  Pure refactor — segment output unchanged (golden 329/0).

## [0.76.23] — 2026-06-13 — Refactor: decompose columnar `resolve_impl`

### Changed

- `storage::columnar` query: split the 157-line `resolve_impl` segment-resolution method into
  three focused helpers — `prune_seg_order_by_zone_maps` (numeric zone-map pruning),
  `collect_resolve_candidates` (per-segment enclosing-type / enrichment / WHERE filtering, returning
  the `ResolveCandidates` `(all, preferred)` pair), and `pick_best_resolved` (last-write-wins
  selection). `resolve_impl` is now a ~71-line pipeline. Pure refactor — resolution behaviour
  unchanged (golden 329/0).

## [0.76.22] — 2026-06-13 — Refactor: split `use_source` into focused helpers

### Changed

- `engine::exec_source`: extracted three cohesive helpers out of the 165-line `use_source`
  method — `configure_columnar_build` (columnar shadow-write setup), `restore_session_on_reconnect`
  (FT6 checkpoint restore + FT7 dirty-file reindex), and `finalize_use_source` (stats, session-map
  registration, result construction). `use_source` is now a ~97-line linear driver. Pure refactor —
  session behaviour unchanged (golden 329/0).

## [0.76.21] — 2026-06-13 — Refactor: extract the SHOW family out of `parse_statement`

### Changed

- `parser`: pulled the ten read-only `SHOW *` arms (sources, branches, stats, context,
  signature, outline, members, body, callees, lines) out of the 198-line `parse_statement`
  dispatch into a dedicated `parse_show_statement` helper; `parse_statement` now routes them
  with a single combined match arm and is ~104 lines. Pure refactor — parse output unchanged
  (golden 329/0).

## [0.76.20] — 2026-06-13 — Refactor: decompose `collect_nodes` in the file indexer

### Changed

- `ast::index::file_indexer`: split the 209-line `collect_nodes` AST-walk loop into three
  focused free helpers — `update_guard_stack` (guard-frame pop/push), `process_node_rows`
  (named / `macro_call` / `extra_rows` / usage-site emission for one node, returning its
  ordinal), and `ascend_to_next_sibling` (cursor unwind + stack/depth bookkeeping).
  `collect_nodes` is now a 108-line driver loop. Pure refactor — row output and node
  ordinals are unchanged (golden 329/0).

## [0.76.19] — 2026-06-13 — Refactor: decouple LegacyMemoryStorage from the columnar engine (steps 1, 1b-part1, 1b-part2)

### Changed

- `storage::columnar`: `ColumnarStorage::warm_or_open` / `warm` now take a backend-neutral `BuildInput { table, prebuilt_segment_map }` instead of `Option<&LegacyMemoryStorage>` — the columnar engine no longer names the legacy storage type at all
- `storage::legacy`: removed `prebuilt_segment_map` from `LegacyMemoryStorage`; the inline columnar segment map (build output) now lives on `Session::prebuilt_segment_map`
- `storage::legacy`: removed the `seg_ctx` field and `install_segment_build_ctx`; `SegmentBuildCtx` is now built by `Session::build_index` and passed directly to a new `build_with_seg_ctx` inherent method — `LegacyMemoryStorage` now holds only its `SymbolTable`, macro table, and language registry

Pure refactor — no behaviour change.

## [0.76.18] — 2026-06-13 — Cross-platform release packaging: Debian, static musl, and macOS

### Added

- Debian packaging via `cargo-deb` for `forgeql`, `forgeql-server`, and `forgeql-client` — each builds an installable `.deb` (binary in `/usr/bin`, man page in `/usr/share/man/man1`)
- `.deb` build job in the release workflow (`release.yml`) that packages the three binaries and attaches them to the GitHub Release on every tag
- Man pages for all three binaries, generated by help2man from per-binary `.h2m` include files (NAME, DESCRIPTION, EXAMPLES, SEE ALSO, AUTHOR, REPORTING BUGS)
- Static musl binaries for `x86_64` and `aarch64`, built via cargo-zigbuild in a `build-musl` release job — fully self-contained (no glibc), so they run on any Linux distro and inside containers
- macOS binaries for Apple Silicon (`aarch64-apple-darwin`) and Intel (`x86_64-apple-darwin`), built natively on GitHub macOS runners in a `build-macos` release job

### Fixed

- Em-dash in the `forgeql` CLI description rendered as `???` in the generated man page; replaced with ASCII
- Release `build` job now ships all three binaries (`forgeql`, `forgeql-server`, `forgeql-client`) in the linux-gnu and Windows archives — previously only `forgeql` was packaged

## [0.76.17] — 2026-06-13 — Refactor: move filter.rs unit tests into filter/tests.rs

### Changed

- `filter`: moved the ~990-line `#[cfg(test)] mod tests` block out of `filter.rs` into a new `filter/tests.rs` file module (declared `#[cfg(test)] mod tests;`), joining the existing `impls` submodule. Tests unchanged; `filter.rs` drops from ~1455 to ~460 lines.

## [0.76.16] — 2026-06-13 — Refactor: move compact.rs unit tests into compact/tests.rs

### Changed

- `compact`: moved the ~810-line `#[cfg(test)] mod tests` block out of `compact.rs` into a new `compact/tests.rs` file module (declared `#[cfg(test)] mod tests;`). Tests unchanged; `compact.rs` drops from ~1532 to ~720 lines.

## [0.76.15] — 2026-06-13 — Refactor: move parser unit tests into parser/tests.rs

### Changed

- `parser`: moved the ~1160-line `#[cfg(test)] mod tests` block out of `parser/mod.rs` into a new `parser/tests.rs` file module (declared `#[cfg(test)] mod tests;`), joining the existing `change`/`clauses`/`find`/`helpers`/`transaction` submodules. Tests unchanged; `parser/mod.rs` drops from ~1612 to ~448 lines.

## [0.76.14] — 2026-06-13 — Refactor: extract segment_reader loader helpers into a submodule

### Changed

- `storage::columnar::segment_reader`: moved the eight open-time loader free functions (`parse_toc`, `blob_slice`, `parse_column_entries`, `load_kind_postings`, `load_enrichment_postings`, `load_zone_maps`, `decode_name_postings`, `load_name_prefix`) out of `segment_reader.rs` into a new `segment_reader::load` submodule as `pub(super)` helpers. Pure code organisation — no behavioural change; `segment_reader.rs` drops another ~270 lines.

## [0.76.13] — 2026-06-13 — Refactor: move segment_reader.rs unit tests into segment_reader/tests.rs

### Changed

- `storage::columnar::segment_reader`: moved the ~400-line `#[cfg(test)] mod tests` block out of `segment_reader.rs` into a new `segment_reader/tests.rs` file module (declared `#[cfg(test)] #[expect(clippy::unwrap_used, clippy::expect_used)] mod tests;`). Tests unchanged; `segment_reader.rs` drops from ~1522 to ~1110 lines.

## [0.76.12] — 2026-06-13 — Refactor: move result.rs unit tests into result/tests.rs

### Changed

- `result`: moved the ~800-line `#[cfg(test)] mod tests` block out of `result.rs` into a new `result/tests.rs` file module (declared as `#[cfg(test)] mod tests;`). Tests are unchanged; `result.rs` drops from ~1595 to ~790 lines, joining the existing `result/convert.rs` and `result/display.rs` submodules.

## [0.76.11] — 2026-06-13 — Refactor: extract overlay open-time parsing helpers into a submodule

### Changed

- `storage::columnar::overlay`: moved the open-time parsing free functions (`parse_header`, `open_blobs`, `build_segment_offsets`, `parse_file_entries`, `parse_enrich_index`, `parse_toc_entries`, `find_blob_ranges`, `decode_segment_metas`, `validate_blob_layout`) out of `overlay.rs` into a new `overlay::parse` submodule as `pub(super)` helpers. Pure code organisation — no behavioural change; `overlay.rs` drops another ~300 lines.

## [0.76.10] — 2026-06-13 — Refactor: extract overlay on-disk format records into a submodule

### Changed

- `storage::columnar::overlay`: moved the on-disk format constants, the fixed-size `#[repr(C)]` `Pod` record types (`TocEntry`, `RowPtr`, `KindEntry`, `TrigramEntry`, `SegmentRecord`, `EnrichEntry`) and the heap-decoded `SegmentMeta` into a new `overlay::format` submodule, re-exported with `pub use format::*`. Pure code organisation — no behavioural change; `overlay.rs` drops from ~1530 to ~1340 lines.

## [0.76.9] — 2026-06-13 — Fix BUG-015 and consolidate golden test sessions

### Fixed

- BUG-015: JSON/data container nodes are now treated as structural, so a plain SHOW outline is no longer empty for files whose root is a JSON array or object

### Changed

- Golden test suite: folded `nid-tests`, `rw-tests`, and `enrich-stale-tests` read/write windows into the shared Zephyr session, reducing session-setup overhead

## [0.76.8] — 2026-06-13 — Refactor: decompose large functions across enrich, show, engine, filter, and columnar modules

### Changed

- `walk_scopes_iterative` (enrich): extracted into `ScopeTracker` struct + `push_children` helper
- `update_guard_stack` (enrich): hoisted into shared `guard_utils` module
- Escape/enrich-row return and macro walks (enrich): split into dedicated helpers
- `control_flow` post-pass (enrich): decomposed into build-phase and apply-phase helpers
- `show_body` (show): split into `collect_body_lines`, `attach_node_id`, and `select_metadata`
- `exec_rollback` (engine): extracted `pop_rollback_checkpoint` + `restore_session_after_reset`
- `exec_show` dispatch (engine): extracted `dispatch_show_op` + `apply_list_clauses`
- `restore_sessions_from_disk` (engine): split into `restore_one_worktree` + `prune_stale_git_worktrees`
- `apply_clauses_inner` (filter): split into where/group-by/ordering helpers
- `materialize_all` (columnar): decomposed into segment-ordering, cap, and per-segment helpers
- `step55_build_enrich_bitmaps` (columnar): split into posting/numeric/serialize helpers

No behaviour change.

## [0.76.7] — 2026-06-12 — Refactor: decompose analyse_uses into UseTracker accumulator and helpers

### Changed

- Decomposed `analyse_uses` into a `UseTracker` accumulator struct and focused helper functions for readability and testability. No behaviour change.

## [0.76.6] — 2026-06-12 — Refactor: decompose enrich/index/engine/segment/query functions; add ci build profile

### Changed

- Decomposed large functions for readability: `skeleton_walk` node dispatch (enrich), `SymbolTable::build` passes (index), `exec_show_find_files` and the `convert_show_content` list parsers (engine), `SegmentBuilder::flush` (segment), and `reindex_files_impl` (query). No behaviour change.
- Added `[profile.ci]` (inherits `release` plus `debug-assertions` and `overflow-checks`) so cached CI scripts can share one optimised build/test compile without losing assertion coverage.

## [0.76.5] — 2026-06-11 — Refactor: decompose large functions across engine, parser, query, shadow-writer, and segment modules

## [0.76.4] — 2026-06-10 — Fix find_node resolving dirty-first to prevent wrong-line edits

### Fixed

- `find_node` resolved node_ids committed-first, so when the `OrdinalRemapper` reassigned a committed ordinal to a different node (indistinguishable same-name siblings + an insertion), the id emitted by `SHOW LINES`/`FIND` and the node resolved by `CHANGE NODE` diverged — silently editing the wrong line (BUG-011). `find_node` now consults the dirty segment first; the committed path only runs when the file has no dirty segment this session.

## [0.76.3] — 2026-06-10 — Fix stale SHOW outline and phantom node spans after dirty edits

### Fixed

- `SHOW outline` served the committed segment for any file edited this session, so deleted nodes stayed listed at stale pre-edit lines with pre-edit node_ids (BUG-013). The glob form now skips a file's committed segment when a dirty segment exists and renders the dirty overlay instead, matching `SHOW LINES` / `FIND`.
- `find_node` resolved a committed node that had been deleted or relocated in the dirty overlay to a phantom inverted span — the stale committed line paired with an EOF-clamped `end_line` — leaving the node permanently uneditable with `end line < start line` (BUG-012). When the file has dirty edits and the committed node is no longer present by name, it now resolves by ordinal in the dirty segment, reporting not-found if the node is gone, instead of handing back a corrupt range.

## [0.76.2] — 2026-06-10 — chores: refactor file_indexers.rs

- Refactor of file_indexer.rs spliting their functions in other fil
es

## [0.76.1] — 2026-06-10 — chores: refactor query.rs

### Changed

- Refactor of query.rs spliting their functions in other files

## [0.76.0] — 2026-06-09 — Worktree teardown helper and per-session TTL

### Added

- `forgeql_core::session::teardown_worktree(data_dir, wt_path, wt_name)` plus the convenience method `SessionCoords::teardown(data_dir)`: remove a worktree's git registration, delete its session branch, and delete its working directory. Best-effort and panic-free, so callers (notably test harnesses that mint a per-run worktree) can reclaim it from a `Drop` guard. `ForgeQLEngine::prune_single_worktree` now delegates to this single implementation.
- Per-session TTL override via the `FORGEQL_SESSION_TTL_SECS` environment variable, read once at session creation and persisted in the worktree's `.forgeql-session` sentinel as a `ttl=` line. `evict_idle_sessions` and `restore_sessions_from_disk` honour the per-session value, falling back to the global 48h `SESSION_TTL_SECS` when unset, so a short-lived test fleet can self-reclaim on a 1h TTL without shortening the TTL of unrelated worktrees in a shared data directory.

### Changed

- The Zephyr golden-test harness now spawns its MCP server with `FORGEQL_SESSION_TTL_SECS=3600`, gives read-only USE windows a shared reusable worktree per source (alias `ro`, reclaimed by the 1h TTL), and gives each mutating window a unique alias that is torn down when the client drops. Previously every USE minted a PID-suffixed worktree per `cargo test` run and none were reclaimed.

## [0.75.6] — 2026-06-09 — Scope the CHANGE FILE block so it no longer blocks node edits

### Fixed

- The experimental block on `CHANGE FILE` for indexed files (0.75.4) also blocked `CHANGE NODE` and `DELETE NODE`: those commands are implemented on top of the same internal mutation path, so the block could not tell them apart from a user `CHANGE FILE`. The block is now applied only to the user-facing `CHANGE FILE` / `CHANGE FILES` command; editing indexed code by node handle (`CHANGE NODE` / `INSERT NODE` / `DELETE NODE`) is never blocked. `INSERT NODE` was already unaffected.

## [0.75.5] — 2026-06-09 — Fix CHANGE NODE swallowing the blank line after a Markdown block

### Fixed

- A whole-node `CHANGE NODE` on a Markdown block (table, paragraph, heading) deleted the blank line that separates it from the next block, merging the two and corrupting structure (for example, the following paragraph was absorbed into a table as if it were a row). The node-resolution path derived a block's end line from the raw parse range, which for Markdown folds in the trailing newline and the following blank line; it now trims trailing newline bytes the way the rest of the engine already does, so a block's end line is its last content line. Editing by node-relative offset (`'<id>(n)'` / `'<id>(n-m)'`) was never affected, and code files are unchanged.

## [0.75.4] — 2026-06-09 — Experimental: block CHANGE FILE on indexed files

### Changed

- **Experimental, opt-out:** `CHANGE FILE` and `CHANGE FILES` now refuse to edit indexed source files, returning guidance to edit them by node handle instead (`CHANGE NODE` / `INSERT NODE` / `DELETE NODE`, with `'<id>(n-m)'` for a sub-node line range). Raw-text `CHANGE FILE` remains available for non-indexed files (config, fixtures, plain text). This is a temporary experiment to evaluate retiring file-range editing of indexed code in the long run; set `FORGEQL_ALLOW_CHANGE_FILE_INDEXED=1` to restore the previous behavior.

## [0.75.3] — 2026-06-09 — Document node-relative line offsets

### Changed

- Documented the node-relative line offset — append `(n)` or `(n-m)` to a node identifier to read or splice a single line or inclusive range within that node's own span — in the README and the agent instruction guides, alongside the existing coverage in the syntax reference.

## [0.75.2] — 2026-06-09 — Node-first command documentation

### Changed

- Reworked the syntax reference and the agent instruction guides (README, AGENTS, and the Claude Code / ForgeQL agent files) so node-addressed commands — `SHOW NODE`, `CHANGE NODE`, `INSERT BEFORE/AFTER NODE`, `DELETE NODE` — are presented as the primary way to read and edit indexed code. The byte-range and whole-file commands are now collected in a dedicated raw-text chapter at the end of the syntax reference, documented as the fallback for non-indexed files only.

## [0.75.1] — 2026-06-09 — Fix flat node hierarchy in the from-scratch index build

### Fixed

- The inline path that builds an index from scratch now persists each node's parent ordinal and its first-child / sibling links, matching the per-file reindex path. Previously it stored only the node ordinal, so a freshly built index recorded a flat node hierarchy. The first reindex of any file then recomputed the correct nested hierarchy; the node-identifier remapper could no longer match the changed structure, and every node identifier in that file was reassigned (and the per-file ordinal base inflated). Node identifiers are now stable from the initial build onward.

## [0.75.0] — 2026-06-08 — Add a `--debug <file>` diagnostic trace log

### Added

- New `--debug <FILE>` server flag that installs a file-backed debug log. When set, instrumented internals append diagnostic lines to the file; when unset the logging is a cheap no-op, so instrumentation can stay in hot paths permanently. Output goes only to the file, never to query responses, so it never affects normal results. Initial instrumentation traces ordinal assignment during reindex (which node ids are reused versus newly allocated, and why).
## [0.74.0] — 2026-06-08 — JSON and YAML language support

### Added

- JSON language support (`.json`, `.jsonc`), backed by `tree-sitter-json`:
  - Each object member is indexed under its key (kind `pair`), and each object
    is named after the value of its `name`, `id`, `key`, `title`, or `alias`
    member when present. This makes individual entries of a large data file —
    such as a golden-test corpus — directly searchable and addressable by a
    stable node identifier.
  - Kind mappings for `object`, `array`, and `pair`.
- YAML language support (`.yaml`, `.yml`), backed by `tree-sitter-yaml`:
  - Mapping members are indexed under their key and mappings are named after an
    identifier-like member, mirroring the JSON behaviour for both block- and
    flow-style collections.
  - Kind mappings for `object` (mappings), `array` (sequences), and `pair`.
- `object`, `array`, and `pair` are now addressable node kinds, so data-file
  entries can be targeted directly by node-identifier edits rather than by
  fragile line ranges.

### Changed

- New `forgeql-lang-json` and `forgeql-lang-yaml` crates are registered in the
  language registry at startup.

## [0.73.0] — 2026-06-08 — Add node-stability regression tests for Markdown edits

### Added

- Golden regression tests confirming node identifiers stay stable when a Markdown document is edited. After a new section is inserted, the existing headings keep their original node identifiers even though their line numbers shift, so later edits that target those identifiers still resolve to the correct nodes.
## [0.72.0] — 2026-06-08 — Self-healing `CHANGE NODE … IF REV` rejection

### Added
- When a `CHANGE NODE … IF REV '<rev>'` (or `DELETE NODE … IF REV`) guard fails
  because the node changed, the rejection now returns a self-healing payload:
  the node's `current_rev`, its `line_start`/`line_end`, and its
  `current_content`. The agent can re-read the new rev and re-target the edit
  without a follow-up query. Mandatory `IF REV` on every `CHANGE NODE` remains a
  deferred, opt-in decision.

## [0.71.0] — 2026-06-08 — Fix SHOW outline tree order

### Fixed
- `SHOW outline` without an explicit `ORDER BY` now preserves the structural
  tree's pre-order (source) sequence. The shared clause pipeline applied a
  default tie-break sort by name, which alphabetized the entries and flattened
  the tree (e.g. a method sorted ahead of its own class). Outline now keeps its
  natural order unless an `ORDER BY` is given; all other commands are unchanged.

## [0.70.0] — 2026-06-07 — Structural outline tree and `SHOW NODE` line offsets

### Added
- `SHOW outline` now returns a nesting-aware structural tree instead of a flat list. By default it
  shows only structural declarations (functions, classes, structs, enums, traits, unions,
  namespaces, modules, type aliases, macros), and each entry carries a `depth` so the compact
  output reads as an indented tree in source order. The `depth` field is filterable and sortable.
- `SHOW outline OF 'file' ALL` includes every node, not just structural declarations. A
  `WHERE fql_kind = '...'` predicate also opts into the full node set so the kind filter sees
  everything.
- `SHOW outline OF '<node_id>'` scopes the outline to that node's subtree.
- `SHOW NODE 'id(n)' CONTENT` and `SHOW NODE 'id(n-m)' CONTENT` narrow the output to a single
  node-relative line or an inclusive range within the node's own span, mirroring the offset
  addressing already supported by `CHANGE NODE`. Offsets are 1-based; an offset on `METADATA` is
  rejected because metadata describes the whole node.

## [0.69.0] — 2026-06-07 — `CHANGE NODE` sub-node line addressing

### Added
- `CHANGE NODE 'id(n)' WITH content` and `CHANGE NODE 'id(n-m)' WITH content` — edit a single
  node-relative line, or an inclusive range of lines, inside an addressable node without
  re-emitting the whole node. Offsets are 1-based and inclusive, matching the per-line offsets
  shown by `SHOW LINES`, so the offset an agent reads is the offset it edits. `CHANGE NODE 'id'`
  with no `(…)` suffix still replaces the entire node, as before.
- An out-of-bounds offset is rejected as a corruption guard; a content/size mismatch is spliced,
  exactly like `CHANGE FILE LINES`. `IF REV` keeps checking the whole node's rev even when a
  `(range)` is given, so any edit inside the node — including outside the targeted range —
  safely invalidates a stale handle.

## [0.68.0] — 2026-06-06 — `SHOW LINES` per-line node handles & offsets

### Changed
- **`SHOW LINES` now renders each line by its innermost containing node and a
  node-relative offset instead of an absolute line number** (CSV output, on
  tree-sitter-parsed files). The shared segment handle (`n<hex>`) is hoisted
  once into the header; each row carries that node's short ordinal (`.NNNN`)
  and a 1-based `off`set within the node:
  ```
  "show_lines","","src/lib.rs","40-43","nabc123def456"
  "node","off","text"
  ".0264","1","    let x = 1;"
  ".0265","1","    if x > 0 {"
  ".0265","2","        log();"
  ```
  An agent can now edit straight from a `SHOW LINES` read — addressing the line
  through its node handle — without a separate `FIND` to recover it. A line
  covered by no indexed node (e.g. a blank line at file scope) shows an empty
  handle and is reported text-only.
- Files without a fresh tree-sitter index — unparsed formats, or a file changed
  on disk but not re-indexed — keep absolute line numbers. This is the safe
  fallback: a stale offset must never produce a wrong edit handle.
- Absolute line numbers remain available via `format=JSON` (`SourceLine.line`
  is unchanged); the resolved offset is also exposed there as
  `SourceLine.node_offset`.

## [0.67.0] — 2026-06-06 — `SHOW body` node-relative line offsets

### Changed
- **`SHOW body` now addresses lines by node-relative offset, not absolute line
  number** (CSV output). When the shown symbol is an addressable node, the
  first column becomes a 1-based `off`set within the node and the node's id is
  stated once in the header row:
  ```
  "show_body","compact_verify","crates/.../compact.rs","443-449","nf817bc7b334d.1237"
  "off","text"
  1,"fn compact_verify(v: &VerifyBuildResult) -> String {"
  2,"    let verdict = if v.success { ""PASS"" } else { ""FAIL"" };"
  ```
  This lets an agent edit straight from the read it just did — `CHANGE NODE
  '<id>'` for the whole symbol — without a separate `FIND` to recover the
  handle, and prepares for node-relative sub-range edits (`CHANGE NODE
  'id(a-b)'`, planned next). Absolute line numbers are still available via
  `format=JSON` (the `SourceLine.line` field is unchanged) and via `SHOW LINES`,
  which is unaffected. `SHOW body` of a symbol with no node ordinal (unparsed
  source / legacy backend) falls back to absolute line numbers.

## [0.66.0] — 2026-06-06 — `SHOW MORE` output buffer

### Added
- **`SHOW MORE` — paged retrieval of a command's full output.** When a
  command's output is too large to return inline, ForgeQL now shows a bounded
  window and buffers the full output server-side. Retrieve the rest without
  re-running the command:
  ```sql
  SHOW MORE                -- the whole buffered output
  SHOW MORE HEAD 40        -- the first 40 lines
  SHOW MORE TAIL 40        -- the last 40 lines
  SHOW MORE 120-240        -- an explicit line range
  SHOW MORE WHERE text MATCHES 'error|fail'   -- grep the buffer
  ```
  Every window form composes with `WHERE text` (regex or `LIKE`) and `LIMIT`,
  and each returned line keeps its original buffer index so a precise follow-up
  range can be requested. The most valuable case is filtering a long
  `VERIFY build` log (`SHOW MORE WHERE text MATCHES 'error'`) without paying for
  another multi-minute build.
- **`VERIFY build` output is now windowed and buffered.** Instead of dumping the
  entire build/test log, `VERIFY` shows the last lines inline (where verdicts and
  errors land) and buffers the rest for `SHOW MORE`. ForgeQL never parses or
  summarizes the log — build output has no universal pass/fail grammar — it only
  windows it. The window is configurable per step in `.forgeql.yaml`:
  ```yaml
  verify_steps:
    - name: test
      command: ./run-tests.sh
      summary:
        direction: tail   # tail (default) | head
        lines: 40         # inline lines before buffering the rest
  ```
  `summary` is optional and defaults to the last 40 lines.

### Changed
- `VERIFY build` results now render as readable newline-delimited text in the
  default CSV output mode (previously a single-line JSON blob), so the log can be
  windowed and grep-filtered line by line.

### Internal
- New `.forgeql-showmore` buffer file, stored in the session worktree beside
  `.forgeql-columnar-delta`. It is excluded from user-facing commits and included
  in transaction checkpoints, so a `ROLLBACK` restores the pre-transaction buffer
  exactly as it restores the columnar delta.

## [0.65.0] — 2026-06-06 — Configurable inline output caps

### Added
- `.forgeql.yaml` now supports an `output` section to tune how many rows/lines
  each query returns inline, replacing the previously hard-coded limits:
  ```yaml
  output:
    find_limit: 20   # rows returned by FIND / list queries without an explicit LIMIT
    show_lines: 40   # source lines returned by SHOW LINES / SHOW body / SHOW context
  ```
  Both keys are optional and default to their former built-in values (20 and 40),
  so existing configurations are unaffected. The caps are frozen at `USE` time
  alongside `verify_steps`, so a later `CHANGE` to the config file cannot alter
  them mid-session.
## [0.64.0] — 2026-06-06 — `AND` as a synonym for `WHERE`

### Added
- `AND` is now accepted anywhere a repeated `WHERE` clause is, as a pure
  synonym. A query such as
  `FIND symbols WHERE fql_kind = 'function' AND lines > 10` now parses and runs
  identically to stacking two `WHERE` clauses. Predicates still combine with AND
  semantics, so results are unchanged — this only removes a common failure mode
  where a SQL-style `AND` produced a parse error. The alias lives in the shared
  clause grammar, so it applies uniformly to every command (`FIND`, `SHOW`, and
  mutations) with no per-command handling.
## [0.63.0] — 2026-06-07 — Networked server and terminal client (HTTP MCP)

### Added
- `forgeql-server`: a standalone HTTP daemon that exposes the ForgeQL engine over
  MCP JSON-RPC. `GET /health` is a liveness probe; `POST /mcp` accepts a
  `tools/call` request for the `run_fql` tool and returns the same compact-CSV or
  JSON output as the existing stdio server. The bind address and port are set with
  `--host` and `--port` (default `0.0.0.0:8080`) and the index data directory with
  `--data-dir`. Authentication is not yet enabled.
- `forgeql-client`: a thin terminal client that talks to `forgeql-server` over
  HTTP. It supports an interactive REPL, one-shot execution (`-e`), and piped
  scripts on stdin. The connection target is set with `--host`/`--port` (or the
  `FORGEQL_HOST`/`FORGEQL_PORT` environment variables). The session token issued by
  `USE` is captured automatically and threaded into later statements, so `USE`
  followed by `FIND`/`SHOW` works across REPL lines and piped scripts.

## [0.62.0] — 2026-06-06 — SHOW body returns a bounded symbol in full
### Changed
- `SHOW body OF '<symbol>'` is no longer blocked by the implicit 40-line cap. A
  single addressable symbol is a bounded unit, so its full extent is returned
  without requiring an explicit `LIMIT`. The cap still applies to unbounded
  output (e.g. whole-file reads). `SHOW NODE [CONTENT]` was already exempt — it
  resolves to the node's exact line range. This removes the most common reason
  agents fell back to raw `SHOW LINES` just to read a function.
## [0.61.0] — 2026-06-06 — Quieter output; parser, regex, and node-addressing fixes

### Changed
- Response footers are now shown only when they are actionable. The
  `tokens_approx` row is emitted only when the response is large enough that an
  agent might want to narrow it (≈500+ tokens); small responses omit it. The
  `line_budget` row is emitted only when the session budget is in a warning or
  critical state, instead of on every response. This trims two rows of noise
  from the majority of responses with no loss of information when it matters.

### Fixed
- `CHANGE FILE … WITH '…'` no longer drops a trailing quote from the content. A
  single-quoted payload whose text ended in a double-quote (e.g.
  `WITH 'version = "0.60.4"'`) silently lost the closing `"` and applied the
  malformed edit, because the unquoting step used a greedy trim that ate any
  quote characters adjacent to the delimiter. It now strips exactly one
  surrounding quote from each end, preserving content quotes. (BUG-005)
- `WHERE name MATCHES '<regex>'` with a top-level alternation (e.g.
  `'foo|bar'`) now returns rows matching either branch. The columnar trigram
  prefilter split the pattern at `|` and intersected the per-branch candidate
  sets, requiring a name to contain every branch at once, so alternation
  queries silently returned nothing. The prefilter now falls back to a full
  scan when the pattern contains `|` (the real regex still filters the rows).
  Other regex metacharacters were unaffected. (BUG-007)
- A node created in the current session (via `INSERT … NODE`, or in a brand-new
  file) is now resolvable by the `node_id` that `FIND symbols` returns, without
  a `COMMIT`. `find_node` resolved ordinals against the committed segment only,
  so a freshly-created node — whose ordinal lives only in the dirty segment —
  failed with "node_id not found", even though `FIND symbols` had just handed
  out that id. `find_node` now falls back to the dirty segment (rebuilding the
  id exactly as the query path does) when the committed lookup misses. (BUG-008)

## [0.60.4] — 2026-06-05 — Fix: node operations never act on stale line numbers

### Fixed
- `FIND NODE`, `SHOW NODE`, `CHANGE NODE`, `INSERT … NODE`, and `DELETE NODE`
  now confirm that a file's indexed content still matches what is on disk
  before resolving a node's line range. Previously the persistent (committed)
  segment stored absolute line numbers stamped at index-build time and trusted
  them blindly; if the file had since changed in a way the session had not
  re-indexed — HEAD advanced past the cached index, a file was reverted while
  git-clean, or the file was edited outside ForgeQL — the stored line was stale.
  A read then returned the wrong line, and `CHANGE NODE` computed its
  replacement range from that stale line and could overwrite an adjacent
  function instead of the intended one.
- The fix adds a content-addressed freshness check. Each segment is already
  identified by the git blob SHA-1 of the bytes it was indexed from, so a node
  operation now compares that hash against the live file and, on a mismatch,
  re-indexes just that one file before resolving. The check is scoped to the
  single file a node operation targets, so broad `FIND` / `SHOW` scans are
  unaffected — no measurable change in query latency or memory use.
- As a consequence, a `node_id` returned by `FIND symbols` is now reliably
  resolvable by a later `FIND NODE` / `CHANGE NODE` even when the file has
  drifted out of sync with the cached index.
## [0.60.3] — 2026-06-05 — Fix: stable node_id ordinals across every reindex

### Fixed
- Node IDs are now stable across `COMMIT MESSAGE` and any subsequent edit.
  Previously, `reindex_files` used `ordinal_remapper: None`, assigning fresh
  sequential DFS ordinals on every reindex. After a commit promoted the dirty
  segments to committed, the new committed segments had different ordinals,
  so a node_id obtained before the commit resolved to a different (or wrong)
  symbol after it.
- The fix: `reindex_files` in the columnar path now builds an `OrdinalRemapper`
  from the most-recent existing segment for each file (dirty-first, then
  committed) before evicting it. This mirrors the existing `build.rs` incremental
  reindex path, which has always used `OrdinalRemapper`. Symbols that survive
  a reindex keep their ordinal; new symbols receive a fresh one beyond the
  existing high-water mark. This makes `CHANGE NODE` safe to use across
  transaction levels and after `COMMIT MESSAGE` without needing to re-query
  node IDs.
## [0.60.2] — 2026-06-05 — Fix: find_node resolves dirty segment by name+kind, not ordinal

### Fixed
- Second (and subsequent) `CHANGE NODE` calls in the same transaction on the
  same file now target the correct function even when an earlier edit added or
  removed indexed nodes (e.g. comment lines) that shift DFS ordinals.
  Root cause: dirty segments are built without an ordinal remapper, so their
  DFS ordinals are raw sequential counters — any prior edit that adds/removes
  indexed nodes shifts every subsequent ordinal, causing `find_node` to resolve
  to a completely different symbol (e.g. a `// ---` comment instead of the
  intended function).
  Fix: dirty-segment lookup in `find_node` now uses `lookup_name(name) +
  fql_kind filter + closest-line tie-breaking` instead of ordinal lookup,
  making it robust to ordinal shifts from any prior in-transaction edit.
## [0.60.1] — 2026-06-05 — Fix: find_node end_line stale after in-transaction edits

### Fixed
- `CHANGE NODE` on the second or later edit in a transaction no longer leaves
  orphaned closing delimiters (`}`, `)`, `}))`, etc.) after the replaced function.
  Root cause: `find_node` read `byte_end` from the committed (pre-edit) segment
  but counted newlines in the current (already-modified) file, producing an
  `end_line` that was several lines too low whenever a prior edit in the same
  transaction had shifted bytes in the same file.
  Fix: `find_node` now prefers the dirty (post-reindex) segment's byte positions
  when a dirty segment exists for the target file, ensuring `byte_end` and the
  file bytes are always from the same version.
## [0.60.0] — 2026-06-05 — Phase E: post-mutation node_id tracking

### Added
- `MutationResult.new_node_id` — after `CHANGE NODE`, the response now includes
  the node_id of the replaced symbol (confirmed stable via post-reindex lookup).
  After `INSERT BEFORE|AFTER NODE`, returns the node_id of the first addressable
  symbol found at the insertion line, or `null` if no symbol was defined there.
- `compact_mutation` formatter — mutation results are now returned as compact CSV
  instead of falling back to JSON, with `new_node_id` surfaced as a dedicated row.

### Fixed
- `end_line` computation undercount: `CHANGE NODE` on functions whose body closes
  with `}))` or nested braces left orphaned closing delimiters. This was a
  pre-existing `end_line` bug triggered by the Phase E edits; the orphaned lines
  are now cleaned up and the duplicate-row bug in `compact_find_node` is resolved.
## [0.59.0] — 2026-06-05 — Phase D: SHOW NODE command and CSV end_line fix

### Added
- `SHOW NODE 'id'` / `SHOW NODE 'id' CONTENT` — return the source lines of any
  addressable node in one step, eliminating the two-query FIND NODE + SHOW LINES
  workflow. All WHERE predicates, LIMIT, OFFSET, and budget caps apply normally.
- `SHOW NODE 'id' METADATA` — return nav + location fields for a node (equivalent
  to `FIND NODE` but consistent with the SHOW verb family).

### Fixed
- `FIND NODE` CSV output now includes the `end_line` field between `line` and `rev`
  (header was `[name,path,line,rev]`, now `[name,path,line,end_line,rev]`).
## [0.58.0] — 2026-06-05 — Phase C: node-addressed mutations

### Added
- `CHANGE NODE 'id' [IF REV 'rev'] WITH content` — replace the source lines of any
  addressable node by its stable `node_id`. The optional `IF REV` guard rejects the
  edit if the stored revision differs from the caller's expected value (optimistic
  concurrency).
- `INSERT BEFORE NODE 'id' WITH content` — insert new source lines immediately before
  the node's first line without touching any existing code.
- `INSERT AFTER NODE 'id' WITH content` — insert new source lines immediately after
  the node's last line.
- `DELETE NODE 'id' [IF REV 'rev']` — delete the source lines occupied by a node.
  The `IF REV` guard works identically to `CHANGE NODE`.
- `FindNodeResult` now includes an `end_line` field (1-based, inclusive) so callers
  can see the full line span of a node without a separate query.
- 21 new golden regression tests covering all four Phase C commands, the `end_line`
  field, and rollback/restore semantics.
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


