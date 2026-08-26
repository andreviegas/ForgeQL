# ForgeQL — Architecture

This document describes the internal design of ForgeQL for contributors and for developers who want to understand how the pieces fit together.

---

## High-Level Diagram

```
┌──────────────────────────────────────────────────────────┐
│  AI Agent (GitHub Copilot · Claude · etc.)               │
│  MCP client in VS Code / any MCP-capable editor          │
└──────────────────────┬───────────────────────────────────┘
                       │  MCP over stdio
┌──────────────────────▼───────────────────────────────────┐
│  ForgeQL  (forgeql binary)                               │
│                                                          │
│  ┌─────────────┐   ┌──────────────────┐                  │
│  │  MCP layer  │   │  Interpreter     │                  │
│  │  (stdio)    │   │  (stdin pipe)    │                  │
│  └──────┬──────┘   └────────┬─────────┘                  │
│         └──────────┬────────┘                            │
│                    ▼                                     │
│           ┌────────────────┐                             │
│           │  Parser (PEG)  │                             │
│           │  forgeql.pest  │                             │
│           └───────┬────────┘                             │
│                   ▼                                      │
│           ┌────────────────┐                             │
│           │  IR (typed AST)│                             │
│           └───────┬────────┘                             │
│                   ▼                                      │
│           ┌─────────────────────────────────┐            │
│           │  Engine                         │            │
│           │  ┌──────────┐  ┌─────────────┐  │            │
│           │  │  Index   │  │  Clause     │  │            │
│           │  │  (rows)  │→ │  Pipeline   │  │            │
│           │  └──────────┘  └─────────────┘  │            │
│           └─────────────────────────────────┘            │
└──────────────────────────────────────────────────────────┘
                       │  git / filesystem
              ┌────────▼──────────────┐
              │  Source Worktrees     │
              │  + index caches       │
              │    (segments/overlay) │
              └───────────────────────┘
```

---

## Components

### Parser

The parser is a [pest](https://pest.rs/) PEG grammar defined in `forgeql.pest`. It accepts one or more FQL statements and produces a typed **Intermediate Representation (IR)** in one pass. There is no separate lexer step — the grammar tokenises and structures the input simultaneously.

---

### IR (Intermediate Representation)

The IR is a flat Rust enum with one variant per command. Every query variant carries a `Clauses` struct for the universal clause set.

```rust
pub enum ForgeQLIR {
    // Session
    CreateSource { name, url },
    RefreshSource { name },
    UseSource { source, branch, as_branch },
    ShowSources,
    ShowBranches,

    // Queries — all carry Clauses
    FindSymbols { clauses },
    FindUsages { of, clauses },
    FindFiles { clauses },
    FindNode { node_id },

    // Content — all carry Clauses
    ShowBody { symbol, clauses },
    ShowSignature { symbol, clauses },
    ShowOutline { file, clauses },
    ShowMembers { symbol, clauses },
    ShowContext { symbol, clauses },
    ShowCallees { symbol, clauses },
    ShowLines { file, start_line, end_line, clauses },
    ShowNode { node_id, mode, clauses },

    // Node mutations (primary editing path)
    ChangeNode { node_id, if_rev, content },
    InsertNode { node_id, position, content },
    DeleteNode { node_id, if_rev },

    // Raw-text mutations (non-indexed files)
    ChangeContent { files, target, clauses },
    CopyLines { src, start, end, dst, at },
    MoveLines { src, start, end, dst, at },

    // Workflow
    Transaction { name, ops, verify, message },
    Rollback { name },
    Verify { step, args },
    Run { step, args },
    Undo { last },
    JobStart { label },
    JobStatus { id },
    JobList,
}
```

Note: `FIND callees OF 'x'` and `FIND globals` are accepted by the grammar but the parser routes them to `ShowCallees` and `FindSymbols` (with a `fql_kind = "variable"` predicate) respectively — they are syntactic aliases, not separate IR variants.

---

### Index

The index is the in-memory representation of a source worktree. Building it means walking all source files with tree-sitter and producing a flat vector of `IndexRow` values:

```rust
pub struct IndexRow {
    pub name: String,                         // symbol name
    pub node_kind: String,                    // raw tree-sitter node kind
    pub fql_kind: String,                     // universal FQL kind (function, class, …)
    pub language: String,                     // language name (cpp, typescript, …)
    pub path: PathBuf,                        // relative file path
    pub byte_range: Range<usize>,
    pub line: usize,                          // 1-based start line
    pub fields: HashMap<String, String>,      // all tree-sitter grammar fields
}
```

The `fields` map is populated automatically from the tree-sitter grammar schema — no hardcoded field lists. Every grammar field (`type`, `value`, `body`, `declarator`, `parameters`, etc.) is immediately available in `WHERE` clauses without any code changes when new node kinds or languages are added.

A secondary cross-reference map tracks identifier usages alongside the main row vector:

```rust
pub struct UsageSite {
    pub path: PathBuf,
    pub byte_range: Range<usize>,
    pub line: usize,                          // 1-based line number
}

pub struct SymbolTable {
    pub rows: Vec<IndexRow>,
    pub usages: HashMap<String, Vec<UsageSite>>,
    // occurrence role -> symbol name -> sites written in text of that role
    pub mentions: HashMap<String, HashMap<String, Vec<UsageSite>>>,
    // internal lookup indexes keyed by name and node_kind
}
```

The serialised index is cached on disk as a bincode file (`.forgeql-index`) next to the worktree. A version header detects stale caches; they are discarded and rebuilt automatically.

Building an index parses and enriches files in parallel on a rayon pool of its own, whose workers are given a 256 MiB stack: the AST enrichers recurse over the syntax tree, and deeply nested source overflows rayon's default one and aborts the process. That pool is built for each index run and dropped when the run ends, so the stacks — and every page an enricher touched on them — are handed back instead of staying mapped for the life of the process. It is sized at half the machine's cores, which `FORGEQL_INDEX_THREADS` overrides: how many per-file peaks can overlap is set by how many workers there are, so halving them keeps an index build clear of a concurrent one on a memory-tight machine, where coinciding peaks have driven a 24 GB host into a 2,824-second swap event. Raising the knob trades that headroom back for build wall clock on a machine with memory to spare. The incremental reindex a mutation triggers takes a pool of one worker rather than that many: it walks its handful of edited files sequentially, so it wants the big stack and none of the parallelism.

---

### Columnar Store

Alongside the in-memory backend, ForgeQL has an on-disk **columnar storage engine** (`crates/forgeql-core/src/storage/columnar/`), enabled automatically when the source has a `.forgeql.yaml`. It is built from three layers:

**Per-file segments** — each source file's index rows are written as one segment, keyed by the file's **path together with its content id** (git blob SHA), plus the enrichment-logic version. An unchanged file never re-indexes: the same path holding the same blob always resolves to the same segment, across branches and sessions. The path belongs in the key because a segment caches the *result of indexing*, and that result is a function of the parser the path selects as well as of the bytes — two byte-identical files with different extensions parse to different trees, and two identical-bytes files can carry different node identities, so neither may share a segment. A segment stores typed columns (`name`, `fql_kind`, `line`, byte ranges, `usages_count`, …), a name FST for symbol lookup, a **sorted string index** (the ids of its string pool in the order of the bytes they name, so resolving the value of a `WHERE <field> = <value>` to an id is a binary search over the mapping rather than a heap map of the pool), **usage postings** — an FST mapping a code reference to the source lines where it occurs (usually identifier text, but a language may also declare a kind whose *whole text* is one key, so a C include path is stored as the single token `zephyr/pm/device.h` rather than split at each `/`, that being the name a query for it would use) — and, per occurrence role the file produced, a **mention postings** pair (`mentions_<role>_fst` / `mentions_<role>_postings`) mapping a name written in text of that role to its lines. Role blob pairs are additive and discovered from the segment's table of contents, so a file written before a role existed simply carries none. Together they are what *labels* a `FIND usages OF` site with its role — they are no longer what finds it; see the read pass below.

**Workspace overlay** — one mmap-backed file per commit SHA merges all segments into a single queryable index shared by every session on that commit (the OS reference-counts the mapped pages, and the decoded structures built on top of them — the overlay tables and one reader per segment, which are private heap rather than shared mapping, though a reader is now a small one: it addresses its Roaring posting blobs in place and decodes a bitmap only when a lookup asks for it, and it keeps no name table — its table of contents is read in place and dropped when the open finishes, leaving it holding the byte ranges of the blobs it reads, and the names that repeat across segments (its enrichment column names, its occurrence role names) are held as one interned copy each in a capped process-wide pool rather than one copy per reader — so what a segment costs at rest is its mapping plus those byte ranges and its string-pool bounds (a `WHERE <field> = <value>` resolves its value to a string id by binary searching the pool's sorted index through the mapping, so not even a predicate leaves a table behind), not a heap copy of every posting and every name — are opened once per commit and handed to each session, so neither multiplies per session; what stays per-session is the dirty overlay of uncommitted edits and any index a session derives lazily for itself; and nothing at all is retained for a session that has not arrived, so a commit's decode lives exactly as long as some session is holding it; and an overlay that reads cleanly while naming a segment the disk no longer holds never serves rows through it — the readers are addressed by position, so a dropped one shifts every later position and serves rows under another file's path. A rebuild that shadow-writes from a merged symbol table is allowed to run, since it can write a segment that is absent, and the open then checks that the segment is really there — so the index is repaired or the open refuses, never quietly smaller; a rebuild that would only assemble the segments already on disk, which is what runs whenever an inline segment map is carried, refuses without running at all; and the assembly itself — the step every rebuild and every COMMIT merge ends in — refuses a segment it cannot open rather than building a smaller overlay, with the check after a repairing rebuild kept as the second, independent proof that the segment really came back; a readable overlay is not deleted in either direction, an overlay that is itself unreadable is still removed and rebuilt as before, and a session on such a commit answers completely from its in-memory index meanwhile, and the refusal — with its repair — rides every USE response for that session until an attach that opens cleanly clears it). The overlay carries a global name FST, kind/trigram bitmaps for fast pruning, and a workspace-total **usage-count aggregate** (symbol name → summed usage-site count) — the source of the real `usages` value on every `FIND symbols` row. The aggregate is built from the usage postings alone, so `usages` counts `role = 'code'` sites and stays a measure of how much code depends on a symbol, not of how often its name is written.

**Chain manifests** — the overlay is a cache over the segments, and a COMMIT does not rebuild it: rebuilding merged every name and row in the corpus to absorb a handful of changed files. What a COMMIT writes instead is a small **chain manifest** (`FQCM`, versioned, written atomically) beside where the commit's overlay would live, naming the *master* commit whose overlay this one grew from, the changed files with their segment content ids, the shadowed paths (replaced and deleted, keyed by path, never content hash), and the non-indexed files the changes added. An attach that finds no overlay but a manifest opens the master overlay and seeds the session's dirty overlay from the manifest's entries — the same union read path uncommitted edits use, so rows and totals match a full rebuild of the commit exactly, while the ascending name streams merge the seeded segments in and the count and remaining stream fast paths take the complete path, exactly as in any session with dirty rows. The session's dirty overlay is cumulative across its commits, so each manifest folds everything since the master into one layer — a chain is never a chain of chains, and an attach to any intermediate commit resolves the same way as the tip. Every defect on this path is a refusal that falls back to the full build: an unreadable or truncated manifest, one from another index generation, a master overlay or a named segment not on disk, or an entry that shadows a master path without recording the replacement. A bad manifest can cost the attacher time, never rows. Chains do not grow without bound: past a per-machine threshold (`FORGEQL_CHAIN_COMPACT_PATHS`, default 512 effective paths) an attach **compacts** instead of seeding — the master's unshadowed segments and the manifest's entries are assembled into a full overlay once, under the overlay lock with the usual peer re-check, and every later attach takes the ordinary fast path; the superseded manifest is removed. Manifests from an earlier index generation are collected with their versioned directory, exactly like every other artefact under it.

**Derived chains** — a commit that arrived from upstream (pushed elsewhere, pulled in by `REFRESH SOURCE`) has no manifest, because no session made it; until now every attach to it paid the full merge however few files an upstream push had changed. The attach now derives the manifest it was never given. It picks the nearest ancestor that has an overlay on disk — walked back from the target on the commit graph, newest first, at most 50,000 commits — and reads the change set off the two indexes rather than off git history: the ancestor's segment table against the segment map the attach's own parse just produced (both keyed by workspace-relative path and content id, so a new content id is a replaced file, a path only one side has is an addition or a deletion, and a rename is one of each), and the ancestor's file-only listing against the same worktree walk a full build runs, compared by path and size. Ancestry is what makes the base *near* — a nearer ancestor means a smaller change set — not what makes it correct: the diff is content-keyed, so a stale base costs entries, never rows; a sibling branch's tip is never chosen, and when no ancestor within the walk has an overlay the full build runs. The derived manifest is written exactly where a COMMIT would have written one and served by the same path, so the next attacher opens it directly and a COMMIT from the session inherits its master; the one difference is what a delta file already in the worktree means. Under a written manifest it is the session's own restored state — the seed and the edits on top of it — and is kept, so the chain is not seeded twice; under a derived manifest it cannot be that (no session ever held this commit's chain state): it is a chain seeded on an older base before the worktree was fast-forwarded, or a delta committed into a checkpoint tree, and it is dropped and the chain seeded in its place. Under either kind a delta that restored no segment at all — every staged entry's staging file missing, as when a checkpoint tree is checked out fresh — is dropped and seeded over the same way, since it held no rows to keep. Whatever is dropped, every path it named goes on the re-index queue: a live worktree drains that queue on reconnect from the files on disk, which is also how a file created in the session and never committed — untracked, so absent from the reconnect's diff against HEAD — gets its rows back, while a fresh worktree, which never reconnects, has nothing on disk the seed does not already serve. A change set at or past the compaction threshold is not chained: there the merge is cheaper than seeding and the full build runs, leaving a real overlay behind. `usages` is the commit's own count on such a session, chained or edited alike: the overlay's usage-count aggregate is the master's, so each stamped row is corrected by the sites the dirty overlay shadowed and added — a table of the names whose count actually changed, read off the per-segment usage postings, built once per dirty state and rebuilt when a lookup finds the dirty overlay has moved since (the state is compared, never trusted to an invalidation somebody has to remember). Every overlay any attach ever built stays on disk under its index generation; deriving chains does not add to that set, but it does not collect it either.

**Serving tiers for `WHERE`** — the overlay also carries a sorted `field=value` key set for the enrichment fields, one row bitmap per key. `=` reads the one key; `LIKE`, `MATCHES` and their negations walk the field's keys, test the pattern against each **value**, and union the matching bitmaps, so a pattern costs one test per distinct value rather than one per row. `name MATCHES` is answered the same way from the global name FST's keys. Two things bound what a key set can be trusted for. It is built two ways: a field on the segment posting list is keyed from those postings, and every other field by walking each segment's rows — so only the first kind can be *partial*, when a file's distinct-value count for the field exceeded that field's per-file posting budget and the file therefore wrote no postings at all. Such a file's rows are added back to the candidate set rather than dropped from it. Separately, a whole field is pruned from the overlay once its workspace-wide value count passes that field's bucket limit, which leaves no keys rather than some. Every tier proposes candidates that the row-level filter then verifies, and each steps aside — back to the complete scan — for anything its key set cannot account for: a field with no keys, a pattern that accepts the empty string (an empty value is never keyed), or an unreadable bitmap. A tier narrows *which rows are read*; it never decides the answer alone. The one structure that does decide is the row column itself, below.

Both budgets are **per field**, not global. The default is 8 values per file and 64 per workspace, sized for enrichment fields that have a handful of values — `naming`, `cast_safety`, `guard_kind`. Five fields are deliberately not like that and carry 4096 / 65,536 instead: the comma-joined guard sets (`guard_defines`, `guard_mentions`, `guard_negates`), `guard_group_id`, and `key_path`, which run to tens of thousands of distinct values on a large corpus. They are posted precisely so that `=` and the pattern operators have a tier at all; under the old single budget they had none and every query on them read every row. What is keyed for them is the **whole** value — for a guard set, the joined string — because that is what the row-level filter compares. Keying the individual members would key something no operator compares against, and both the pattern tier and the absence proof read these keys as values, so a regex spanning a comma would match no key and a whole value that is not a member would be reported absent.

**Serving a core row column** — not every queryable field needs an index. `language` is stored per row, and a `WHERE language = '<lang>'` predicate is answered by comparing those stored values directly, one integer per row, without constructing a result row for any of them. That makes the candidate set *exact* rather than a superset — every row is decided against its own stored value — so unlike a posting-derived tier it may also conclude an absence. A row whose stored language is empty matches nothing, negations included, because the row-level filter fails every operator on a field a row does not report. A segment whose column does not account for its rows one-for-one is not read this way; the query falls back to the complete scan. That shape is no longer reserved for one column. The residual `WHERE` a query is left with once the tiers have proposed candidates is now tested against the stored columns *before* any result row is built, so a scan that keeps a few thousand rows out of a few hundred thousand stops paying to build the ones it is about to discard: `name`, `fql_kind`, `language`, `line`, the path of the file and every enrichment column a segment stores are read in place. What each answers must be exactly what the built row would have answered — that equivalence, and not the speed, is what the row view is tested against, because a difference is a missing result rather than a slow one. A predicate that cannot be answered this way is never dropped; it is handed on to the filter that runs after the rows are built, unchanged. What counts as answered includes reporting a confident absence: where no column of a segment holds the field a predicate names, the row that segment would build carries it in neither its struct nor its enrichment map, so both readers resolve it to nothing and the predicate is decided here — false for every row of that segment, `!=`, `NOT LIKE` and `NOT MATCHES` included, since each arm of the row filter fails on a missing value rather than passing it, EXCEPT for the four fields that declare a stamp-only default (`has_todo`, `has_escape`, `has_shadow`, `is_recursive`), where a row inside the declared kinds and languages resolves the absent column to that default instead of to nothing, in the row view and in the built row alike because both read it through the one declaration. Such a segment contributed nothing before either; what has gone is the rows built to establish that, and with them the rule that one segment lacking an enrichment column took the page off row views for every segment that had one. Six kinds always go the other way: `usages`, which is stamped from the workspace overlay only once a row exists; `node_id`, which is built as the row is; `count`, which GROUP BY assigns later still; `body` and `role`, which are written onto a row after its columns are read — `body` out of the file as the row is materialised, `role` by the read pass that finds an occurrence site, so for those two a missing column is not a missing value and a reader that sees only columns must not conclude one; and a regular expression, which is cheaper compiled once for a batch than once per row. That last one used to be more than a cost rule: the batch filter read a value that resolved to nothing as a passing `NOT MATCHES` where a per-row evaluation dropped it, and holding every regex back was what kept the two from answering differently. They now agree, and the reason the regex waits is the compile alone. A name a segment shadows with an enrichment column of that name used to go the other way too, and no longer does. The reason it did was narrow — an operator whose type does not match the struct accessor falls through to the enrichment map of the built row and finds the shadow column there, and the reader used for filtering could not tell which of the two was coming, so it declined the name outright. It is told now: the accessor is part of what it is asked, derived from the operator and the value type, so it reads the fixed column where the built row reads its struct and that same shadow column where the built row reads its map. Both readers resolve every field arm for arm as the built row does, so a shadow changes what `WHERE name = 42` reads on both alike and changes nothing about the ordering or the collapse key. The cost of the old reading was not theoretical: an enrichment column named `name` is a tree-sitter grammar field on essentially every definition node, and it covered 292 of the 293 segments a `WHERE name …` scan selects on this repository, and 7,267 of 9,278 on a 3-million-symbol Zephyr tree. `node_kind` is the field it cannot serve, and the gap there is not merely one of speed: nothing stores `node_kind` per row, so every row materialised from the index reports it as absent, and a predicate on it could only answer a confident nothing (or, negated, everything) instead of scanning. It is therefore refused, in `WHERE`, `ORDER BY` and `GROUP BY`, on every verb that filters rows — `FIND symbols`, `FIND globals`, `FIND usages`, `FIND files`, `FIND callees OF`, `SHOW outline`, `SHOW members` and `SHOW callees` — with a message pointing at `fql_kind`. The reading verbs (`SHOW body`, `SHOW context`, `SHOW NODE`, `SHOW signature`) are outside that set, because their clause also selects which symbol to resolve and that half runs against symbol rows. The legacy in-memory index does store it per row and still filters on it, which is why the name stays in the shared `CORE_WHERE_FIELDS` list; serving it here instead would mean storing it, which is an index-output change.

**Choosing a page before building it** — the same row views also rank, and on a bounded `LIMIT` they carry the whole scan. A query with a `LIMIT` written or defaulted, no `GROUP BY`, no `HAVING` and no two segments over one source path collapses, ranks, trims and pages entirely over row views, and builds a result row only for the `LIMIT + OFFSET` that survive. A view is 48 bytes — a segment reference, the file's path, the row's name borrowed from the mapping, the row index and the line — against about 1,600 for the row it stands for, so a scan matching a million rows to deliver twenty now holds 48 MB of views at its widest instead of 1.6 GB of rows, and builds twenty. `OFFSET` rows are carried because the skip runs downstream, after a session's uncommitted rows have been merged in: a dirty row landing ahead of the window shifts which persistent rows fall inside it, and keeping `LIMIT + OFFSET` is enough whatever the overlay adds, since no persistent row ranked past that can reach the page. Where a predicate has to wait for a built row the query takes the older route, which builds as it goes with a running top-K trim that sheds once the working set passes `k * 4` — and there a segment with more matching rows than that threshold still ranks them as row views and builds only the ones it retains. The comparator is one comparator, the retained size is one number, and the collapse key is one key, which is what makes the two routes the same query rather than similar ones. The working set either route accumulates is in fact a *superset* of the one the scan accumulated before views existed — a segment contributes its own top `2k` where it used to contribute everything and be cut to `2k` against the segments before it — and the true top `k` is inside both, so the page does not move. The top-K partition is unstable, but the ordering leaves it no choice worth naming: rows compare *equal* only when they agree on the ORDER BY field and on all of `name`, `line`, `path` and `fql_kind`, and those four fields are the duplicate-collapse key, so such rows are one row of the answer, merged before any page is cut.

**Choosing a page of usage sites** — `FIND usages` pages the same way and by a different unit. Its `LIMIT` counts **files**, because a usage site is one line of one file and the question behind the query is which files hold the name, so cutting the site list at a row count would report a file as partly used and hide the rest of it. That bound cannot be the row `LIMIT` the scan above hands its engine, and it is not: the file count, the `OFFSET` in files and the site ceiling travel to the backend as their own shape, and the backend selects whole file groups over the sites before it builds a row for any of them. What a site view answers is what the row built from it answers — the queried name, the file, the line, and the `role` where a backend tags one, with every other field absent on both — which is what lets the residual `WHERE` and the `ORDER BY` run over the sites and still decide what they would have decided over the rows. The site list itself is still held whole, and still bounded by the row budget, because selecting whole files out of a computed answer cannot bound what is searched; what is no longer held is a result row per site.

**Counting without the rows** — a `GROUP BY` answers with one row per group, and the scan built every matching row to get there: on a multi-million-symbol corpus `FIND symbols GROUP BY naming` spent the whole result budget, and was refused by it, to produce eight numbers. Three groupings are read from the index instead. `fql_kind` and the source path have counts of their own — a kind bitmap's cardinality, and the per-segment deduplicated row count stored at overlay build — and an enrichment field the segments post per value now joins them: the overlay's `field=value` key table is binary searched to that field's range and each key's bitmap cardinality is that value's group, one bitmap decoded per value rather than one row built per match. Those bitmaps were written by intersecting a segment's postings with its canonical row set, which is the collapse the scan's dedupe pass performs, and a row carries at most one value of a field because the segment posted it from a column holding one value id per row. So the cardinalities are the *collapsed* counts, and they partition the rows the field describes — which is what makes the group of rows it does not describe a subtraction from the stored canonical total rather than something that would have to be scanned for. The same subtraction answers the oldest of the three: `step5_build_kind_postings` skips the empty kind, so a row whose `fql_kind` is empty is in no kind bitmap at all, and `fast_group_by_kind` reports those rows as the remainder — the group the scan keys by the empty string, 2,169 of 59,636 rows on this repository's own corpus, which the counted route used to drop with nothing to say they had gone. Where the subtraction cannot hold, the path declines to the scan rather than publish a count derived from a contradiction. The gates are exactly the cases where the counting would lie, and each declines rather than approximating. First the one an agent meets: a `WHERE`. A stored cardinality counts a value over whole segments, and there is no way to narrow it to the subset a predicate selects — the candidate bitmaps most tiers propose are supersets that a residual `WHERE` still decides — so `WHERE fql_kind = 'function' GROUP BY naming` scans, as `GROUP BY fql_kind` beside any `WHERE` does. `GROUP BY file` is where that reasoning stops: its groups are the segments, so `group_by_file_fast_path_eligible` admits two predicates, `fql_kind = '<value>'` and `name = '<value>'`, and `fast_group_by_file` intersects the bitmap with each segment's row range. The kind one is admitted because `step5_build_kind_postings` intersects the kind postings with each segment's canonical rows, which makes a cardinality of them a count of answer rows rather than of candidates. `step6_build_name_fst` now applies that same intersection to the name postings, which is what admits `name =` beside it. No name PATTERN tier is admitted, at any literal length, and the reason is worth stating rather than leaving as a rule — a counted path never opens a row, so a candidate is counted exactly as it was proposed. A literal shorter than the trigram width makes the tier decline, `prefilter_global` skips the predicate, and every row of a file becomes a candidate; a literal the tier answers still proposes a superset; and a plain `name =` proposed the intra-segment duplicates the answer collapses until `step6_build_name_fst` began intersecting each segment's name postings with its canonical rows, the intersection `step5_build_kind_postings` and `collect_posting_enrichment` had always applied. Nothing at query time settles either pattern: the canonical row set is stored as a per-segment *count* and not as a set, so there is nothing to intersect a candidate against, and each pattern goes to the scan, which decides a row by reading it — an ordinary scan, so the result budget applies to it and on a large enough corpus it can be refused where the counted route returned a number; `IN`/`EXCLUDE` is what narrows it, since the top-K trim is disarmed under a `GROUP BY`. Admitting the name equality cost exactly that intersection at overlay build — an index-output change, which is why the overlay carries a schema version and why one had to be spent on it. Extending any `WHERE` admission to the enrichment table has to clear that same bar per predicate tier, which is why this one takes none. Beyond the `WHERE`, all three counted groupings want a session holding no uncommitted rows, which are in no segment's postings, and an index with no two segments built from one source path, where a per-segment collapse is not the whole collapse. Two more are the enrichment table's own: a field the overlay pruned for carrying more distinct values than its budget allows, so the table holds no key for it at all, and a segment that stores the column and posted none of its values, which happens past the per-field posting budget and leaves its rows carrying values no key counts. The last gate is one all three share: a `HAVING` or `ORDER BY` naming anything but `count` or the grouped field. A group row counted from the index carries those two and nothing else, so a predicate on any other name is false on every group and would deliver an empty set with full confidence — `FIND symbols GROUP BY fql_kind HAVING lines >= 2` answered nothing against the scan's six groups until the two older paths learned the test the enrichment one already applied. That reroute is bounded the same way: the answer is the scan's, and the budget can refuse it on a corpus large enough. `IN` and `EXCLUDE` are applied by intersecting each bitmap with the rows of the segments whose path passes, which is exact because a segment is one source path — the canonical total under the globs is then the sum of those segments' stored counts. Only the selected segments are asked whether they posted the field, since the rows of the rest are in neither the mask nor the total. One thing the two routes do not share is the order of a page the clause has not decided: a counted group row is named by its own value, while a scanned one is the first row of the group and is named by that row, so with no `ORDER BY` — or where an `ORDER BY count` ties — the same groups with the same counts come back in a different sequence, and under the 20-row default that changes which of a wide field's groups the page holds. That, and what the delivered row itself reports — a counted group row is named by the grouped value and carries no path on a kind group and no name or line on a file group, where a scanned one is the first row of its group and reports that row's own name, path and line — are the whole of the difference: never `total`, and never a count. Everything else groups by scanning, as before.

An ordering rides row views only when a view ranks, the way the row it would build ranks, both the `ORDER BY` field and every tie-breaker the comparator consults — `name`, `line`, `path` and `fql_kind`, published as `filter::ORDER_TIE_BREAKERS` so that adding a fifth cannot leave the ranking reading four. Five field names fail that, and they are the five a built row can come to carry from outside its own columns: `usages`, stamped from the workspace overlay after materialisation (the stored column is a stale zero); `node_id`, derived from the row's ordinal as the row is built; `count`, assigned later still by GROUP BY — the three published as `segment_reader::VIEW_CANNOT_ANSWER` — and `body` and `role`, which `field_tiers` declares as written onto the row after its columns are read. Those last two matter more to an ordering than to a predicate: a predicate that declines is merely run later, while an ordering that ranks every row by an absence the built row does not share cuts the wrong top-K and sheds rows that belonged in the answer. Beside the ordering, a segment with a residual predicate no row view could answer takes the built route whole, since a row dropped on rank alone might have been the one that survived the predicate. A field a segment simply does not carry is *not* such a case: absent on the view and absent on the built row is the same rank on both, and the same verdict on both, so it neither bars the ranking nor sends the predicate to the built rows. That was once the difference between the two tests — the ranking admitted an absent field and answerability did not, which left the route taken by almost nothing, since most segments carry no column for any given enrichment field. The two now agree on it, and what separates them now is narrow: `node_kind`, which nothing stores, and `path` on a segment read without a source path — the early `WHERE` declines both where the ranking does not have to ask.

Whether an ordering can ride views is a property of the query and not of any segment, and it used to be the other way round. A row view withheld a field a segment stored under a name a result row answers from a struct field of its own, while the built row still reported the struct — so one segment carrying enrichment columns named `name` and `path` was enough to switch the ranking off for every query in a workspace, and the decision had to be made per segment to contain the damage. It is contained at the source instead: a row view resolves every field arm for arm as the built row resolves it, reading the fixed column for the six struct-backed names and the enrichment column for everything else, so a same-named column changes what `WHERE name = 42` reads — that operator falls through to the enrichment map on both readers — and changes nothing about the ordering or the collapse key. The old reading was expensive rather than merely cautious, and for a structural reason worth stating: a segment's enrichment columns are the enrichers' output *plus* every tree-sitter grammar field of every emitted node, which `extract_fields` copies wholesale into the row's map for the builder to turn into columns. `name` is what most grammars call a definition's identifier child, which makes essentially every code segment a shadowing one: 308 of the 411 segments of this repository's own index, each of them refused. `name` is not the only one it has already happened to — 211 of those same segments carry a column called `path` — and it is not a closed list, since a grammar naming a field after a struct-backed name shadows that one too with nothing to announce it.

**Collapse before you choose, and count what you shed** — a page chosen on rank is only a page if every row in front of the chooser is a distinct answer row, and duplicates on `(name, fql_kind, path, line)` used to be collapsed after both choosers had already run. Where a group of them sorted to the front, the retained window filled with rows that later merged into one and the page came back shorter than its `LIMIT`, the distinct rows that belonged in it having been shed. Every chooser now collapses first, and reads the key from the stored columns: `name`, `fql_kind` and `line`, three fixed columns every segment has, resolved by a row view exactly as the row it would build resolves them — so the collapse can always run, where it used to decline on a segment carrying an enrichment column of one of those names. Two rows carry that key only if they carry the same path, and a path belongs to one segment unless two segments were built from it — so a per-segment collapse is the whole collapse wherever that does not happen, and where it does nothing sheds at all rather than shed on an incomplete one, on either route.

That ordering is also what makes the reported `total` mean anything. `total` is the number of rows that matched, taken before `OFFSET` and `LIMIT` cut a page out of it, and the trim hands back how many rows it discarded so those are counted too — they are answers no page could hold rather than rows about to merge into a survivor. This is also what retired the segment fetch cap. A `LIMIT` with no `ORDER BY` used to stop opening segments at `limit + 1` rows, which made `total` the size of the page, paged an `OFFSET` past rows that were never fetched, and let a larger `LIMIT` surface rows a smaller one had not shown; such a query is now trimmed by the same running top-K as an ordered one, because the pipeline sorts by the `(name, line, path, fql_kind)` tie-break whether or not an `ORDER BY` was written, so `LIMIT k` asks for the k smallest rows under that ordering rather than for whichever k the scan reached first. What that once cost was reachability at the edges of the trim's gate — a `LIMIT` above 1000, or with an `OFFSET`, materialised every matching row and could be refused by the row budget where the cap let it complete with a wrong answer. Neither is outside the bound now: those are exactly the shapes the row-view route takes, since its bound is the page the caller asked for rather than the trim's own threshold, so a `LIMIT 5000` or a `LIMIT 20 OFFSET 100` carries `LIMIT + OFFSET` views through the scan and builds that many rows. What is still outside it is a query no view can page: a `GROUP BY`, a `HAVING`, a predicate or an ordering naming `usages`, `node_id`, `count`, `body` or `role` — the last two written onto a row after its columns are read, so a view reading the columns alone would rank by an absence the built row does not share — a regular expression, or an index holding two segments built from one source path. Those materialise every matching row, and the row budget is what refuses them. The name streams report it honestly too, without the walk they exist to avoid: each segment carries its deduplicated row count from overlay build time and a kind bitmap knows its own cardinality, so the streamed page rides beside the whole answer's size. On the one index shape where those stored counts cannot speak for the answer — two segments built from one source path — the streams decline and the pipeline answers, slower but counted from the collapse itself. A bare `LIMIT k` with no `ORDER BY` at all is served by the same ascending stream when nothing else narrows the query — no `WHERE`, no `IN`/`EXCLUDE`, unique source paths, the asked-for rows within the row budget — since the default ordering starts with `name`: the shortest query an agent can type reads `limit + offset` keys of the name index instead of materialising every row in the corpus. A `FIND symbols` written with no `LIMIT` at all is not outside these gates: when it carries no `GROUP BY` and no `HAVING`, the engine boundary hands it its 20-row default page as the `LIMIT` an explicit `LIMIT 20` would carry, so under the same gates — no `OFFSET` for the trim, the clause-free shape for the stream — it takes the same stream or the same trim, with the same rows and the same counted `total`, rather than materialising every row and being refused by the budget (outside those gates it materialises as before) — the default page is a delivery bound, and the engine is told about it. Uncommitted edits no longer force the ascending arms off this path: every dirty segment carries its own sorted name FST, so the overlay stream and the dirty streams merge name by name, overlay rows under a shadowed path are skipped, and the total becomes the masked stored counts plus the dirty segments' distinct counts — the descending and kind-filtered arms still take the pipeline on a dirty session.

**The field/tier table** — `field_tiers::FIELD_TIERS` declares, per queryable field, where its value is stored, which structure serves each operator class, whether that structure decides the answer or only proposes candidates, what it cannot see and which mechanism covers that, the budgets bounding it, and the measurement or the bench class that would produce one. It is still a parallel declaration for the serving path — nothing consults it to choose a tier — but two things now read it at runtime, and both are places a second copy of the same knowledge had already drifted. `field_tiers::canonical` is the one alias table: every comparison that asks "is this clause naming *that* field" — the kind fast path, the grouping renderer, the outline universe, the row resolvers — spells the written name to its canonical form through it, so an alias added to the table reaches all of them at once and cannot be known to one implementation and not another. And `FieldTier::refusal` is the wording an agent sees when a field cannot be answered, built from the row's own `source` and `elsewhere`, so the error names where the field *is* served rather than restating a list that goes stale. What the table buys beyond that is a test: every claim it makes is checked against the const lists the builder and query path actually read (`POSTING_ENRICHMENT_FIELDS`, `ZONEMAP_NUMERIC_FIELDS`, `CORE_WHERE_FIELDS`), against the per-row-shape declarations on `ClauseTarget`, against the builder's own budget functions, and against what a query returns, so a field whose declared queryability and actual serving path disagree fails the suite instead of answering wrongly at corpus scale. Its own boundary is stated in the same breath: the language-declared enrichment fields cannot be enumerated at compile time, so one catch-all row states their serving path rather than naming them, and a `Gap` variant no test can reach carries the reason no test can reach it.

**Dirty overlay** — per-session, in-RAM segments for files changed inside the session. Query results are the union of persistent overlay rows and dirty rows, with dirty rows taking precedence, so uncommitted edits are immediately queryable without rebuilding the shared overlay. It also records, as bare paths, the files the session touched that produced no segment because no plugin claims their extension: those exist only on disk until the next commit, and `FIND files` and the `FIND usages` read pass would otherwise not know they are there.

**Authoritative read pass (`FIND usages OF`)** — the postings above are a fast tier, and a fast tier is only ever as complete as what a recorder happened to tokenise. Lines exist that none of them recorded (the body of an `.rst` literal block), and whole files exist that produced no segment at all (a `.gitignore`, any extension no plugin claims). So `FIND usages` reads the files themselves on every query and the line's own text decides what is a site; a posting only adds the `role` to a site the bytes already proved, and a line nothing recorded arrives as `text`. The universe read is exactly what `FIND files` lists — committed segments, file-only entries, the files this session reindexed, and the paths of files this session created whose extension no plugin claims (those are in no committed structure at all until the next commit, so the dirty overlay records the path and nothing else) — minus ForgeQL's own runtime artifacts; a file that reaches the worktree without passing through ForgeQL, one a build step wrote, is in neither that list nor this read until it is indexed, and a file excluded by `.gitignore`, `.ignore` or `.forgeql-ignore` is in neither at all — unless this session touched it, since a mutation records the path it wrote without consulting any ignore rule: a path no plugin claims is then listed and searched until the next commit and not after, while one whose extension a plugin claims gets a segment instead, and a segment is carried through the commit, so that file stays in both from then on: the two answer over one universe, exclusions included. Binary files (a NUL byte near the start) skipped so a sweep is never armed on an object file or an index blob — a byte-order mark being believed before that check, so UTF-16 text is read rather than taken for an object file — but only UTF-16, and only where a mark declares it: UTF-16 without a mark is indistinguishable from binary, and UTF-32 is not decoded at all even when its mark declares it, so both stay unsearched — and everything else decoded leniently so a file that is text apart from a stray legacy byte still answers. A path the index lists but the worktree no longer holds is skipped silently, having no bytes to search; one that exists but cannot be read is counted in a `hint` — a count, not a list of paths, so it says how much is missing and not which file. A UTF-16 site is found and cannot be rewritten in place: a line boundary there is not a byte boundary, so any node- or line-scoped mutation on such a file — and any `COPY LINES` or `MOVE LINES` whose destination is one, since the payload spliced in is UTF-8 — is refused with an error naming the encoding rather than attempted, replacing the file whole included: a whole-file `CHANGE NODE` is lowered to a line range like any other, and a line range over UTF-16 does not even reach the last byte. Replacing every byte at once is safe and is not refused: `CHANGE FILE ...WITH ...` does it on a non-indexed file; an indexed one has to be deleted and written again, or converted outside ForgeQL. Reading such a line back is bounded the same way — `SHOW` renders raw bytes, so the decode reaches the site list and not the display. This is what lets an empty result mean the corpus does not hold the name. Cost is one read per in-scope file per query, bounded by `IN`/`EXCLUDE`, which prune the reading and not merely the rows; measured below the benchmark harness's 200 ms noise floor on a three-million-symbol corpus **with no `IN` scope at all**, which is the whole-workspace shape and the worst case — measured on a query that the read pass demonstrably answered, returning 179 sites where the postings alone return 140.

**Reindex on mutation** — every successful mutation re-indexes the touched files. An ordinal remapper matches the new parse against the old rows by content hash, so existing nodes keep their `node_id` even as line numbers shift; only genuinely new or rewritten nodes receive fresh ordinals (surfaced as `new_node_id` in the mutation response). This is what makes node handles drift-proof.

---

### Clause Pipeline

All filtering, sorting, grouping, and pagination is handled by a single `apply_clauses()` function that operates on any type implementing the `ClauseTarget` trait. The pipeline always runs in this fixed order:

```
raw results
    → IN / EXCLUDE  (path glob filter)
    → WHERE         (field predicate filter)
    → GROUP BY      (aggregate — adds a count field per group)
    → HAVING        (filter on aggregated rows)
    → ORDER BY      (sort)
    → OFFSET        (skip first N rows)
    → LIMIT         (truncate to N rows)
```

The `WHERE` predicate supports `=`, `!=`, `LIKE`, `NOT LIKE`, `MATCHES`, `NOT MATCHES` (regex via the `regex` crate), and numeric comparisons. `ClauseTarget` is implemented for every result shape — `IndexRow`, `SymbolMatch`, `FileEntry`, `DiffFileEntry`, `OutlineEntry`, `MemberEntry`, `SourceLine`, `CallGraphEntry` and `CommitRow`, plus `SegRowRef`, which is not a result shape at all but a segment row viewed in place, used only to decide a residual `WHERE` before the row is built (see **Serving a core row column**) — so the full pipeline applies uniformly to FIND queries, SHOW body/lines/context, SHOW outline/members/callees, SHOW COMMITS and SHOW DIFF. Each implementation also *declares* the field names it resolves (`STR_FIELDS`, `NUM_FIELDS`) and whether anything outside them can resolve at all (`OPEN_FIELDS`). How a verb is checked follows from how many consumers its clause has.

**One consumer.** `FIND files`, `SHOW outline`, `SHOW NODE`, `SHOW LINES`, `SHOW MORE`, `SHOW COMMITS`, `SHOW DIFF`, and — index-aware, in the columnar backend's Stage 0, because a symbol row's enrichment map is open — `FIND symbols`/`globals`/`usages` name a file, a handle or a path, so nothing resolves a symbol and the whole clause is held to `filter::reject_unresolvable_fields::<T>`: a field `T` does not carry is refused rather than answered with nothing.

**Two consumers.** `SHOW members`, `SHOW callees`, `FIND callees OF`, `SHOW body` and `SHOW context` also address a symbol, and their `WHERE` is split between the two by `filter::clauses_for_rows` / `clauses_for_lookup`: a predicate the returned rows carry filters those rows, and one they do not carry goes to `StorageEngine::resolve_*`, which evaluates it against each candidate. Both backends do — `storage/legacy/resolve.rs` always has; `query/resolve.rs` now materialises each candidate row and applies the predicate through the same `eval_predicate`, and only when the lookup half is non-empty, so an unfiltered `SHOW` pays nothing for it. No predicate is dropped, and the only one that reaches BOTH consumers is a `ClauseTarget::LOOKUP_FIELDS` name — a field the rows carry whose value on every row is a property of the resolved symbol, so filtering rows by it can only keep all of them or none. `path` on a callees row is the one such field today: every call sits in the resolved function's own file, and routing it to the rows alone answered zero whenever the lookup had picked a definition elsewhere. The globs go the same way for the opposite reason — `IN` and `EXCLUDE` describe a file, and a members row and a source line report no path at all, so `clauses_for_rows` drops them for any shape that cannot answer `path` and leaves them to the lookup, which is what they were always about. What those verbs still refuse is what NEITHER consumer can answer (`filter::reject_refused_fields::<T>`, read from `field_tiers` minus what shape `T` carries) and — for `ORDER BY`, `GROUP BY` and `HAVING`, which no resolver reads and which are therefore never split — the row shape itself, via `reject_unresolvable_shaping_fields::<T>`.

**No rows to shape.** Every verb that answers with source lines — `SHOW body`, `SHOW context`, `SHOW signature`, `SHOW NODE`, `SHOW LINES`, `SHOW MORE` — answers in source order, and nothing between the `WHERE` filter and the line caps sorts, groups or aggregates. `ORDER BY`, `GROUP BY` and `HAVING` are therefore refused on all six by `ForgeQLEngine::reject_line_shaping` rather than accepted and read by nothing: `LIMIT` *is* honoured, so an accepted-and-ignored `ORDER BY line DESC LIMIT 4` handed back the first four lines — the opposite page to the one asked for, a wrong answer rather than an inert clause. `SHOW signature` goes further: it renders one line rather than building a row set, so it has no row consumer at all, its whole clause reaches the lookup, and a `WHERE` on a field only a source-line row carries — `text`, `marker`, `rev`, derived as what `SourceLine` resolves minus what a symbol row resolves — is refused there too. `IN`/`EXCLUDE` are refused by `reject_globs` on the four verbs that resolve no name AND whose rows carry no path — `SHOW NODE`, `SHOW LINES`, `SHOW MORE`, `SHOW COMMITS` — where a glob has neither a lookup to scope nor a row path to match.

Most of these checks run **before dispatch**, in `ForgeQLEngine::reject_show_clause_fields`, which names each SHOW verb it covers with the row shape it answers over. Ordering is the point: a refusal that ran after the lookup would arrive as `no symbol 'Foo' matches …` once a doomed predicate had already excluded every candidate, turning a fact about the query into one about the code. The verbs absent from it are checked where they execute, each likewise before the work it gates: `SHOW NODE` in `exec_show::read` — it never reaches `exec_show` as itself, since CONTENT arrives re-synthesised as `SHOW LINES` and METADATA returns before that — plus `SHOW MORE`, `SHOW COMMITS`, `SHOW DIFF` and the `FIND` verbs. A `ForgeQLIR` variant that carries a clause and is named in none of those places fails `every_clause_carrying_verb_decides_how_its_fields_are_checked`, which reads the variant list off `ir.rs` instead of restating it — `SHOW COMMITS` had been missing from two hand-written enumerations of exactly that set. Two checks run earlier still and for every verb at once, in `dispatch_op` over `ir::clauses_of`: an uncompilable regex pattern, and a VALUE outside a set the ENGINE owns rather than the corpus. `fql_kind` and `role` are the two engine-owned sets — a language plugin maps its grammar onto the kind vocabulary rather than extending it, and the occurrence roles are minted by the read pass — so `WHERE fql_kind = 'impl'` is refused naming every accepted kind, while `guard_kind = 'ifdef'`, a value the CORPUS merely does not hold, still answers empty and that empty is the fact about the code. Neither check reads the row shape, so neither is enumerated per verb: a variant added to `clauses_of` inherits both.

One limit, in the same breath: only the columnar `FIND` verbs can tell an unknown field from an unmatched one, because only they see which enrichment columns a segment stored. On a `SHOW`, a misspelt field falls to the lookup half, satisfies no candidate, and is reported as a lookup that matched nothing rather than as a bad name. The clause also matters as much as the field: grouping keys a row through `field_str` alone, so a numeric-only name would fabricate one empty-named group holding every row, and `count` is written by the grouping pass, so it is answerable in `HAVING` and `ORDER BY` and refused in a `WHERE` that runs before it.

Clauses that do not apply to a given result type are silently skipped. There is no per-command clause handling code.

---

### MCP Layer

The MCP layer exposes a **single tool** to the agent via the MCP JSON-RPC protocol, over two transports: stdio (`forgeql --mcp`) and streamable HTTP (`forgeql-server`, `POST /mcp`). The HTTP daemon implements the client-to-server half of the MCP handshake — `initialize` (with version negotiation and connect-time instructions), `notifications/*` (acknowledged with `202 Accepted`), `tools/list`, and `ping` — so remote MCP clients such as Claude Code connect to it directly with no local binary:

| Tool | Purpose |
|---|---|
| `run_fql` | Execute any ForgeQL statement — `USE`, `FIND`, `SHOW`, `CHANGE NODE` / `INSERT NODE` / `DELETE NODE`, `BEGIN TRANSACTION`, `COMMIT`, `ROLLBACK`, `VERIFY`, `JOB`, `UNDO`, `SHOW SOURCES`, `SHOW BRANCHES` |

Every ForgeQL operation is accessible through `run_fql`. There are no separate tools for individual operations — one tool, one mental model, no ambiguity about which tool to reach for.

Sessions start with `USE source.branch AS 'alias'` and are cleaned up automatically: a worktree carrying no work (no commits over its base and no uncommitted changes) is removed after about 2 hours idle, one with work after 48 hours, by a server-side background task. Multiple agents can work on the same branch by reconnecting with the same `USE` command — the worktree and any uncommitted changes are preserved.

**Over stdio, the alias you supply in `AS '...'` is the `session_id`** — it is deterministic and reconstructable from the `USE` command the model already knows; if a model forgets its `session_id` it simply re-issues `USE source.branch AS 'same-alias'` to reconnect. **Over HTTP (`forgeql-server`), the `session_id` is a server-issued token** scoped by the authenticated user and returned in the `USE` response — clients store it and pass it verbatim in every subsequent call instead of reconstructing it from the alias.

**Auto-reconnect:** if the server restarts and a client passes a `session_id` whose worktree still exists on disk, the engine transparently re-creates the in-memory session — no `USE` command required. The source name and branch are derived from the worktree directory name and git metadata.

**Cross-process worktree liveness:** more than one engine process can share a data directory, and each runs a reclaim pass at startup that deletes worktrees which look abandoned. A process cannot see another's in-memory sessions, so liveness is recorded on disk instead: an advisory lock on a claim file beside each worktree directory (`session/liveness.rs`). An owner takes a shared lock before `git worktree add` runs and holds it for the session's life; a reclaim sweep takes an exclusive lock and deletes only while holding it, so a worktree a peer is still checking out is never mistaken for an orphan. The OS releases both locks when a process dies, which is what makes a crashed owner's worktree reclaimable with no lease to expire.

`CREATE SOURCE`, `REFRESH SOURCE`, and `VACUUM` are intentionally blocked through stdio MCP — they must be run via the interpreter or CLI. On `forgeql-server` they additionally require an admin bearer token from the `--auth-file` token store; normal and anonymous principals can only `USE` existing sources.

Structured self-healing rejections — `rev_mismatch`, `node_not_found`, and the
bulk-`FOUND` refusals (`no_found_set`, `found_truncated`, `found_refused`) — are
returned as error-flagged (`isError`) tool results whose text is the JSON
payload, on both stdio and HTTP, rather than buried inside a JSON-RPC protocol
error. Plain precondition errors (missing session, invalid arguments) remain
JSON-RPC errors on every transport.

### Agent Guardrails

The MCP layer includes two mechanisms that prevent AI agents from misusing ForgeQL:

**`with_instructions()`** — The server's `get_info()` response includes a structured instruction text that is injected into the agent's system prompt during the MCP `initialize` handshake. This text contains:
- Critical rules (never use local filesystem, always start with `USE`)
- Query strategy decision tree (FIND → SHOW NODE workflow)
- Efficiency rules (default limits, progressive depth)

These instructions reach the agent regardless of which editor or platform it runs on — they are part of the MCP protocol itself.

**SHOW line capping** — SHOW and FIND output is subject to a default inline cap (`DEFAULT_SHOW_LINE_LIMIT = 40` lines). When output exceeds the cap and the agent did not include an explicit `LIMIT` clause:
- The first window of lines is returned, with the full output buffered server-side (a 5-slot `LAST-n` ring in the session worktree)
- A guidance message tells the agent to page the rest with `SHOW MORE` — or better, to use `FIND symbols WHERE` to locate the exact symbol and read it by handle with `SHOW NODE '<id>'`
- If the agent genuinely needs more lines inline, it can re-run with an explicit `LIMIT N`

This creates a teaching moment on first contact — after hitting the cap once, agents learn the precision workflow.

### Mechanical Mutations

The engine never auto-corrects the text it splices — no comma fixing, no brace balancing, no re-indentation. The safety mechanisms are all *visibility* and *reversal*, not intelligence:

- every mutation reindexes the touched files and answers with `new_node_id`, `lines_written`, and `lines_removed` (the destructive-edit signal);
- the response includes a **boundary diff** — context lines above and below the change, each carrying an inline `node_id(offset)` handle — built *after* apply + reindex so the handles address the post-edit tree;
- an optimistic-concurrency guard (`IF REV`) rejects a mutation when the node changed since it was read, returning the node's current content;
- `UNDO [LAST-n]` restores the pre-edit bytes from a 10-slot per-session ring;
- transactions checkpoint the worktree and `ROLLBACK` restores it wholesale.

### Verify, Commit Gate, and Background Jobs

`VERIFY build '<step>'` runs a vetted command from `.forgeql.yaml` (`verify_steps`), with optional typed positional params substituted only after arity/type validation. Steps are frozen at `USE` so an edit cannot tamper with a gate command. Since 0.110 every `VERIFY build` / `RUN` executes on the background job pool: the caller still gets a synchronous `success` + `output` response (the transport waits on the job with the engine lock released), but a long gate can no longer freeze the engine for other sessions or tenants. A run that outlives the step's `timeout_secs` degrades to a `job_started` response for `JOB STATUS` polling.

Steps marked `commit_gate: true` gate `COMMIT`: the step must have passed **since the most recent mutation**, and every successful mutation invalidates prior passes. Multiple gated steps AND together.

`JOB START '<step>' ['<arg>'…]` runs the same verify steps (including typed params) as detached background jobs (`jobs.rs`): the request returns a job id immediately; `JOB STATUS` / `JOB LIST` poll state and output. Jobs execute through a bounded worker pool with a FIFO queue — at most `FORGEQL_MAX_CONCURRENT_JOBS` (default 2) run at once, the rest wait `Queued` — so a burst of heavy builds is throttled instead of exhausting machine memory. A `commit_gate: true` step run as a job satisfies the commit gate at completion, but only when no mutation happened while it ran (the session's mutation counter is snapshotted at submission and compared at reconcile time); reconciliation happens on `JOB STATUS`, `JOB LIST`, and `COMMIT`.

### Agent Distribution

ForgeQL ships pre-built agent configuration files in `doc/agents/`:

| File | Platform | Effect |
|---|---|---|
| `forgeql.agent.md` | VS Code Copilot | Locks agent to `forgeql/*` tools via `tools:` frontmatter |
| `AGENTS.md` | VS Code / Claude Code | Workspace-level behavioral instructions |
| `CLAUDE.md` | Claude Code | Native format adapter |
| `.cursorrules` | Cursor | Native format adapter |

The VS Code Custom Agent is the strongest enforcement — `tools: [forgeql/*]` means the agent literally cannot call grep, find, or cat. Other platforms rely on behavioral instructions combined with the MCP server's built-in guardrails.

---

## Data Flow: a FIND query

```
Agent sends:  FIND symbols WHERE fql_kind = 'function' LIMIT 5

1. Parser          → ForgeQLIR::FindSymbols {
                         clauses: { where: [fql_kind = "function"],
                                    limit: Some(5) }
                       }
2. Engine          → table = session.index()
3. Engine          → raw = table.rows.iter().collect()   // all rows, unfiltered
4. Clause pipeline → apply WHERE: keep rows where fql_kind == "function"
                   → apply LIMIT: take first 5
5. Engine          → ForgeQLResult::Query { results: [SymbolMatch × 5] }
6. MCP layer       → serialise to JSON → send to agent
```

---

## Data Flow: a CHANGE NODE mutation

```
Agent sends:  CHANGE NODE 'nb1be37eea3f0.0124' IF REV 'h0123456789abcdef'
                WITH 'fn run(buf: &mut [u8]) { buf.fill(0); }'

1. Parser    → ForgeQLIR::ChangeNode { node_id, if_rev, content }
2. Engine    → resolve node_id → file + byte/line span
               check rev guard: mismatch → reject with the node's
               current rev + content (self-healing payload)
3. Engine    → snapshot pre-edit bytes (undo ring slot LAST-0)
               splice the replacement text over the node's span —
               byte-exact, no syntax correction
4. Engine    → reindex the touched file; ordinal remapper keeps
               unchanged nodes' ids stable, resolves new_node_id
5. Engine    → build the boundary diff (pre-edit bytes vs. disk),
               annotate present lines with node_id(offset) handles;
               invalidate any commit-gate passes
6. MCP layer → return { new_node_id, lines_written, lines_removed, diff }
```

---

## Directory Structure

```
ForgeQL/
├── crates/
│   ├── forgeql/                  # Binary entry point, MCP server, CLI flags
│   │   └── src/
│   │       ├── main.rs
│   │       ├── mcp.rs            # MCP tools + with_instructions() + guardrails
│   │       ├── cli.rs / execute.rs / session.rs
│   │       └── path_utils.rs
│   ├── forgeql-client/           # Thin client binary (remote server mode)
│   ├── forgeql-server/           # HTTP server binary (bearer-token auth)
│   ├── forgeql-core/             # All core logic (no binary, no language grammars)
│   │   └── src/
│   │       ├── ast/
│   │       │   ├── lang.rs       # LanguageSupport trait, LanguageConfig, LanguageRegistry
│   │       │   ├── index.rs + index/  # IndexRow, SymbolTable, node-id assignment; index/ holds the file indexer, build, and tests
│   │       │   ├── query.rs      # find_symbols, find_usages
│   │       │   ├── show.rs       # show_body, show_signature, show_outline, …
│   │       │   ├── cache.rs      # Index serialization/deserialization (bincode)
│   │       │   └── enrich/       # Enrichment modules (naming, comments, numbers,
│   │       │                     #   control_flow, operators, metrics, casts,
│   │       │                     #   redundancy, scope, member, decl_distance,
│   │       │                     #   escape, shadow, unused_param, fallthrough,
│   │       │                     #   recursion, todo, macro_expand_enrich)
│   │       │                     #   + guard_utils (guard stack + exclusivity)
│   │       ├── parser/
│   │       │   ├── forgeql.pest  # PEG grammar
│   │       │   └── mod.rs        # Parser functions → IR
│   │       ├── git/
│   │       │   ├── mod.rs        # Repo basics: open, branch, reset, run_git
│   │       │   ├── commit.rs     # Staging + commit shapes (checkpoint, clean, squash)
│   │       │   ├── diff.rs       # Change lists, dirty paths, SHOW DIFF surface
│   │       │   ├── excludes.rs   # Which files never reach a commit
│   │       │   ├── patch.rs      # EXPORT PATCH: git am-ready mbox files
│   │       │   ├── source.rs     # Source + SourceRegistry (bare repo management)
│   │       │   └── worktree.rs   # Worktree lifecycle: create, list, remove
│   │       ├── session/          # Session management (user → worktree → index)
│   │       ├── storage/
│   │       │   ├── mod.rs        # StorageEngine trait (backend boundary)
│   │       │   ├── source_provider.rs  # SourceProvider trait (content addressing)
│   │       │   ├── legacy.rs     # In-memory SymbolTable backend
│   │       │   └── columnar/     # Segments, overlay, dirty overlay, reindex
│   │       ├── transforms/
│   │       │   ├── mod.rs        # TransformPlan, ByteRangeEdit, FileEdit
│   │       │   ├── change.rs     # File mutation: matching, lines, with, delete
│   │       │   ├── copy_move.rs  # COPY LINES / MOVE LINES planning and execution
│   │       │   └── diff.rs + diff/  # Unified + compact diff: apply, compact, lcs
│   │       ├── verify/           # Run build/test verification steps + typed params
│   │       ├── workspace/
│   │       │   ├── mod.rs        # Workspace root discovery, safe_path confinement
│   │       │   └── file_io.rs    # Atomic write, .forgeql-ignore support
│   │       ├── engine.rs + engine/  # Command dispatch, node mutations, commit gate
│   │       ├── jobs.rs           # Background job scheduler (worker pool + FIFO queue)
│   │       ├── undo.rs           # Per-session undo ring (pre-edit byte snapshots)
│   │       ├── node_id.rs        # Node handle encoding (segment prefix + ordinal)
│   │       ├── showmore.rs       # SHOW MORE output ring (LAST-n slots)
│   │       ├── budget.rs         # BudgetState: deduction, recovery, persistence, sweep
│   │       ├── compact.rs        # Compact CSV output renderer (MCP mode)
│   │       ├── filter.rs         # apply_clauses(), ClauseTarget trait
│   │       ├── ir.rs             # ForgeQLIR, Clauses, Predicate, ChangeTarget
│   │       ├── result.rs + result/ # ForgeQLResult, SymbolMatch, ShowResult
│   │       ├── config.rs         # .forgeql.yaml deserialization
│   │       ├── auth.rs           # Bearer-token authentication (server mode)
│   │       ├── error.rs          # ForgeError (thiserror)
│   │       └── query_logger.rs   # FQL statement logging (--log-queries)
│   ├── forgeql-lang-c/           # C language support crate (config/c.json)
│   ├── forgeql-lang-cpp/         # C++ language support crate
│   │   └── src/
│   │       ├── lib.rs            # CppLanguage, CPP_CONFIG, map_kind(), cpp_registry()
│   │       └── macro_expand.rs   # CppMacroExpander — extract_def, extract_args, substitute
│   ├── forgeql-lang-python/      # Python language support crate (config/python.json)
│   ├── forgeql-lang-rust/        # Rust language support crate
│   │   ├── config/
│   │   │   └── rust.json         # kind_map, enricher hints, node kind sets, macros section
│   │   └── src/
│   │       ├── lib.rs            # RustLanguage, RUST_CONFIG, rust_registry()
│   │       └── macro_expand.rs   # RustMacroExpander — macro_rules! extraction + expansion
│   └── forgeql-lang-text/        # ALL structured-text formats, one module each:
│       ├── config/               #   xml, dbc, toml, json, yaml, ini, kconfig, just, make,
│       └── src/                  #   cmake, markdown, rst — plus config/<lang>.json
├── doc/
│   ├── syntax.md                 # Command and clause reference
│   ├── architecture.md           # This file
│   └── agents/                   # Distributable agent configs
│       ├── forgeql.agent.md      # VS Code Copilot Custom Agent (tools locked)
│       ├── AGENTS.md             # Platform-agnostic workspace instructions
│       ├── README.md             # Installation guide
│       ├── claude-code/          # Claude Code adapter
│       └── cursor/               # Cursor adapter
└── tests/                        # Integration tests + fixtures
```

---

## Language-Agnostic Architecture

ForgeQL's core (`forgeql-core`) contains zero language-specific code. All language knowledge lives in external crates — one per language.

### Key Abstractions (defined in `ast/lang.rs`)

**`LanguageConfig`** — a static struct containing all language-specific data: node kind sets, modifier maps, cast kinds, number suffixes, comment prefixes, visibility keywords, data-flow analysis node kinds (`parameter_list_raw_kind`, `identifier_raw_kind`, `assignment_raw_kinds`, `update_raw_kinds`, `init_declarator_raw_kind`, `block_raw_kind`), and guard configuration (`block_guard_kinds`, `elif_kinds`, `else_kinds`, `condition_field`, `name_field`, `negate_ifdef_variant`). Each language crate defines a `static CPP_CONFIG: LanguageConfig` (or equivalent).

**`LanguageSupport`** — a trait that every language crate implements:

```rust
pub trait LanguageSupport: Send + Sync {
    fn name(&self) -> &'static str;                      // e.g. "cpp"
    fn extensions(&self) -> &'static [&'static str];     // e.g. &[".cpp", ".h", ".cc", ...]
    fn config(&self) -> &'static LanguageConfig;         // static config data
    fn tree_sitter_language(&self) -> Language;           // tree-sitter grammar
    fn extract_name(&self, node: Node, source: &[u8]) -> Option<String>;
    fn map_kind(&self, raw_kind: &str) -> Option<&'static str>;  // "function_definition" → "function"
}
```

**`LanguageRegistry`** — holds all registered `LanguageSupport` implementations. The engine uses it to route files to the correct language by extension.

### Dual Kind System

Every `IndexRow` carries two kind fields:
- `node_kind` — the raw tree-sitter node kind (e.g. `function_definition` in C, `function_item` in Rust). Language-specific, computed while parsing to drive kind mapping, and stored on no row of the columnar index: a `WHERE`, `ORDER BY` or `GROUP BY` on it is **refused** on every verb that filters rows — `FIND symbols`, `FIND globals`, `FIND usages`, `FIND files`, `FIND callees OF`, `SHOW outline`, `SHOW members` and `SHOW callees`, but not the reading verbs, whose clause also selects which symbol to resolve — because a predicate on a field no row carries can only report absence. The legacy in-memory index does keep it per row and still filters on it, which is why the name remains in `CORE_WHERE_FIELDS` — the refusal lives in the columnar backend's own validation, next to the knowledge of what that backend stores.
- `fql_kind` — a universal kind (e.g. `function`, `class`, `struct`). Language-agnostic, defined by `map_kind()`.

Always use `WHERE fql_kind = 'function'` — or `kind`, its alias, which is answered wherever `fql_kind` is. Every session serves queries from the columnar index, so a row-filtering verb naming `node_kind` errors with a pointer to `fql_kind` rather than answering a confident zero. Serving it there would mean storing it, which is an index-output change; until something does, the refusal is the honest answer.

### Crate Dependencies

```
forgeql (binary)
├── forgeql-core          zero language grammars
├── forgeql-lang-c        tree-sitter-c + CLanguage
├── forgeql-lang-cpp      tree-sitter-cpp + CppLanguage + CppMacroExpander
├── forgeql-lang-python   tree-sitter-python + PythonLanguage
├── forgeql-lang-rust     tree-sitter-rust + RustLanguage + RustMacroExpander
└── forgeql-lang-text     all structured-text grammars (XML, DBC, TOML, JSON,
                          YAML, INI, Kconfig, justfile, Make, CMake, Markdown, reST)
```

`forgeql-core` depends on `tree-sitter` (the library) but NOT on any grammar crate. Grammar dependencies live exclusively in language crates. The `forgeql` and `forgeql-server` registries splice every text format in with one `text_languages()` call, so a new text format is picked up by both binaries automatically.

---

## Adding a New Language

For a structured-text format, add a module + `config/<lang>.json` kind map inside `forgeql-lang-text` — no new crate needed. For a full programming language, add a single new crate, with no changes to `forgeql-core` beyond the two value lists noted after the steps:

The JSON is deserialized strictly: a key the loader does not recognise is a hard
error that names the offending key, not a silently ignored line. A correctly
spelled key at the wrong nesting level therefore fails the first time the
language is used, instead of leaving the feature it configures quietly switched
off.

1. **Create `crates/forgeql-lang-<name>/`** with `Cargo.toml` depending on `forgeql-core` + `tree-sitter-<name>`.

2. **Implement `LanguageSupport`** — define the static `LanguageConfig`, `extract_name()` for the grammar's naming conventions, and `map_kind()` for the FQL kind taxonomy.

3. **Register in the binary** — add the language to the `LanguageRegistry` in `main.rs`:
   ```rust
   let registry = Arc::new(LanguageRegistry::new(vec![
       Arc::new(CppLanguage),
       Arc::new(TypeScriptLanguage),  // new
   ]));
   ```

Everything else — indexing, enrichment, the clause pipeline, MCP tools, query
functions — works without modification, with **one obligation on the core
side**: a plugin may only map its grammar onto `fql_kind` names, and declare
`mention_text_kinds` roles, that `field_tiers::FQL_KIND_VALUES` and
`USAGE_ROLE_VALUES` already carry. Those two lists are what a clause VALUE is
refused against, so a kind or role a plugin introduces and core does not know
would be refused on `WHERE` although rows of it exist.
`crates/forgeql-core/tests/engine_owned_value_universes.rs` reads every
`crates/*/config/*.json` and fails on exactly that, so the omission is a red
gate rather than a wrong answer — but it does mean a genuinely new kind is a
two-file change, and the sentence above is true of everything except those two
lists. (Deriving both from the `LanguageRegistry` at query time would remove
the obligation; it is not what ships today.)
