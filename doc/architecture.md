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

---

### Columnar Store

Alongside the in-memory backend, ForgeQL has an on-disk **columnar storage engine** (`crates/forgeql-core/src/storage/columnar/`), enabled automatically when the source has a `.forgeql.yaml`. It is built from three layers:

**Per-file segments** — each source file's index rows are written as one segment, keyed by the file's **path together with its content id** (git blob SHA), plus the enrichment-logic version. An unchanged file never re-indexes: the same path holding the same blob always resolves to the same segment, across branches and sessions. The path belongs in the key because a segment caches the *result of indexing*, and that result is a function of the parser the path selects as well as of the bytes — two byte-identical files with different extensions parse to different trees, and two identical-bytes files can carry different node identities, so neither may share a segment. A segment stores typed columns (`name`, `fql_kind`, `line`, byte ranges, `usages_count`, …), a name FST for symbol lookup, **usage postings** — an FST mapping a code reference to the source lines where it occurs (usually identifier text, but a language may also declare a kind whose *whole text* is one key, so a C include path is stored as the single token `zephyr/pm/device.h` rather than split at each `/`, that being the name a query for it would use) — and, per occurrence role the file produced, a **mention postings** pair (`mentions_<role>_fst` / `mentions_<role>_postings`) mapping a name written in text of that role to its lines. Role blob pairs are additive and discovered from the segment's table of contents, so a file written before a role existed simply carries none. Together they are what *labels* a `FIND usages OF` site with its role — they are no longer what finds it; see the read pass below.

**Workspace overlay** — one mmap-backed file per commit SHA merges all segments into a single queryable index shared by every session on that commit (the OS reference-counts the pages, so RSS does not multiply per session). The overlay carries a global name FST, kind/trigram bitmaps for fast pruning, and a workspace-total **usage-count aggregate** (symbol name → summed usage-site count) — the source of the real `usages` value on every `FIND symbols` row. The aggregate is built from the usage postings alone, so `usages` counts `role = 'code'` sites and stays a measure of how much code depends on a symbol, not of how often its name is written.

**Serving tiers for `WHERE`** — the overlay also carries a sorted `field=value` key set for the enrichment fields, one row bitmap per key. `=` reads the one key; `LIKE`, `MATCHES` and their negations walk the field's keys, test the pattern against each **value**, and union the matching bitmaps, so a pattern costs one test per distinct value rather than one per row. `name MATCHES` is answered the same way from the global name FST's keys. Two things bound what a key set can be trusted for. It is built two ways: a field on the segment posting list is keyed from those postings, and every other field by walking each segment's rows — so only the first kind can be *partial*, when a file's distinct-value count for the field exceeded that field's per-file posting budget and the file therefore wrote no postings at all. Such a file's rows are added back to the candidate set rather than dropped from it. Separately, a whole field is pruned from the overlay once its workspace-wide value count passes that field's bucket limit, which leaves no keys rather than some. Every tier proposes candidates that the row-level filter then verifies, and each steps aside — back to the complete scan — for anything its key set cannot account for: a field with no keys, a pattern that accepts the empty string (an empty value is never keyed), or an unreadable bitmap. A tier narrows *which rows are read*; it never decides the answer alone.

Both budgets are **per field**, not global. The default is 8 values per file and 64 per workspace, sized for enrichment fields that have a handful of values — `naming`, `cast_safety`, `guard_kind`. Five fields are deliberately not like that and carry 4096 / 65,536 instead: the comma-joined guard sets (`guard_defines`, `guard_mentions`, `guard_negates`), `guard_group_id`, and `key_path`, which run to tens of thousands of distinct values on a large corpus. They are posted precisely so that `=` and the pattern operators have a tier at all; under the old single budget they had none and every query on them read every row. What is keyed for them is the **whole** value — for a guard set, the joined string — because that is what the row-level filter compares. Keying the individual members would key something no operator compares against, and both the pattern tier and the absence proof read these keys as values, so a regex spanning a comma would match no key and a whole value that is not a member would be reported absent.

**Serving a core row column** — not every queryable field needs an index. `language` is stored per row, and a `WHERE language = '<lang>'` predicate is answered by comparing those stored values directly, one integer per row, without constructing a result row for any of them. That makes the candidate set *exact* rather than a superset — every row is decided against its own stored value — so unlike a posting-derived tier it may also conclude an absence. A row whose stored language is empty matches nothing, negations included, because the row-level filter fails every operator on a field a row does not report. A segment whose column does not account for its rows one-for-one is not read this way; the query falls back to the complete scan. The same shape is available to any low-cardinality stored column. `node_kind` is the field it cannot serve, and the gap there is not merely one of speed: nothing stores `node_kind` per row, so every row materialised from the index reports it as absent, and a predicate on it could only answer a confident nothing (or, negated, everything) instead of scanning. This backend therefore refuses it in `WHERE`, `ORDER BY` and `GROUP BY`, in its own validation and with a pointer to `fql_kind`, on `FIND symbols`, `FIND globals`, `FIND usages` and `FIND files` — `SHOW outline`, `SHOW members` and `SHOW callees` answer with a JSON value rather than a Result and still accept the field, matching nothing; the legacy in-memory index does store it per row and still filters on it, which is why the name stays in the shared `CORE_WHERE_FIELDS` list. Serving it here instead would mean storing it, which is an index-output change.

**The field/tier table** — `field_tiers::FIELD_TIERS` declares, per queryable field, where its value is stored, which structure serves each operator class, whether that structure decides the answer or only proposes candidates, what it cannot see and which mechanism covers that, the budgets bounding it, and the measurement or the bench class that would produce one. It is a parallel declaration: nothing reads it at query time, and the const lists the builder and query path actually read (`POSTING_ENRICHMENT_FIELDS`, `ZONEMAP_NUMERIC_FIELDS`, `CORE_WHERE_FIELDS`, `SORTABLE_SYMBOL_FIELDS`) are still the ones in force. What the table buys today is a test: every claim it makes is checked against those lists, against the builder's own budget functions, and against what a query returns, so a field whose declared queryability and actual serving path disagree fails the suite instead of answering wrongly at corpus scale. Its own boundary is stated in the same breath: the language-declared enrichment fields cannot be enumerated at compile time, so one catch-all row states their serving path rather than naming them, and a `Gap` variant no test can reach carries the reason no test can reach it.

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

The `WHERE` predicate supports `=`, `!=`, `LIKE`, `NOT LIKE`, `MATCHES`, `NOT MATCHES` (regex via the `regex` crate), and numeric comparisons. `ClauseTarget` is implemented for `IndexRow`, `SymbolMatch`, `SourceLine`, and `CallGraphEntry`, so the full pipeline applies uniformly to FIND queries, SHOW body/lines/context, and SHOW callees.

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
- `node_kind` — the raw tree-sitter node kind (e.g. `function_definition` in C, `function_item` in Rust). Language-specific, computed while parsing to drive kind mapping, and stored on no row of the columnar index: a `WHERE`, `ORDER BY` or `GROUP BY` on it is **refused** there on every `FIND` verb, because a predicate on a field no row carries can only report absence. The legacy in-memory index does keep it per row and still filters on it, which is why the name remains in `CORE_WHERE_FIELDS` — the refusal lives in the columnar backend's own validation, next to the knowledge of what that backend stores.
- `fql_kind` — a universal kind (e.g. `function`, `class`, `struct`). Language-agnostic, defined by `map_kind()`.

Always use `WHERE fql_kind = 'function'`. Every session serves queries from the columnar index, so a `FIND` naming `node_kind` errors with a pointer to `fql_kind` rather than answering a confident zero. Serving it there would mean storing it, which is an index-output change; until something does, the refusal is the honest answer.

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

For a structured-text format, add a module + `config/<lang>.json` kind map inside `forgeql-lang-text` — no new crate needed. For a full programming language, add a single new crate with no changes to `forgeql-core`:

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

Everything else — indexing, enrichment, the clause pipeline, MCP tools, query functions — works without modification.
