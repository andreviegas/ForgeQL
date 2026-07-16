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
    // internal lookup indexes keyed by name and node_kind
}
```

The serialised index is cached on disk as a bincode file (`.forgeql-index`) next to the worktree. A version header detects stale caches; they are discarded and rebuilt automatically.

---

### Columnar Store

Alongside the in-memory backend, ForgeQL has an on-disk **columnar storage engine** (`crates/forgeql-core/src/storage/columnar/`), enabled automatically when the source has a `.forgeql.yaml`. It is built from three layers:

**Per-file segments** — each source file's index rows are written as one segment, keyed by the file's **path together with its content id** (git blob SHA), plus the enrichment-logic version. An unchanged file never re-indexes: the same path holding the same blob always resolves to the same segment, across branches and sessions. The path belongs in the key because a segment caches the *result of indexing*, and that result is a function of the parser the path selects as well as of the bytes — two byte-identical files with different extensions parse to different trees, and two identical-bytes files can carry different node identities, so neither may share a segment. A segment stores typed columns (`name`, `fql_kind`, `line`, byte ranges, `usages_count`, …), a name FST for symbol lookup, and **usage postings** — an FST mapping identifier text to the source lines where it occurs. The postings are the reference index behind `FIND usages OF`.

**Workspace overlay** — one mmap-backed file per commit SHA merges all segments into a single queryable index shared by every session on that commit (the OS reference-counts the pages, so RSS does not multiply per session). The overlay carries a global name FST, kind/trigram bitmaps for fast pruning, and a workspace-total **usage-count aggregate** (symbol name → summed usage-site count) — the source of the real `usages` value on every `FIND symbols` row.

**Dirty overlay** — per-session, in-RAM segments for files changed inside the session. Query results are the union of persistent overlay rows and dirty rows, with dirty rows taking precedence, so uncommitted edits are immediately queryable without rebuilding the shared overlay.

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

Sessions start with `USE source.branch AS 'alias'` and are cleaned up automatically: worktrees idle for more than 48 hours are removed by a server-side background task. Multiple agents can work on the same branch by reconnecting with the same `USE` command — the worktree and any uncommitted changes are preserved.

**Over stdio, the alias you supply in `AS '...'` is the `session_id`** — it is deterministic and reconstructable from the `USE` command the model already knows; if a model forgets its `session_id` it simply re-issues `USE source.branch AS 'same-alias'` to reconnect. **Over HTTP (`forgeql-server`), the `session_id` is a server-issued token** scoped by the authenticated user and returned in the `USE` response — clients store it and pass it verbatim in every subsequent call instead of reconstructing it from the alias.

**Auto-reconnect:** if the server restarts and a client passes a `session_id` whose worktree still exists on disk, the engine transparently re-creates the in-memory session — no `USE` command required. The source name and branch are derived from the worktree directory name and git metadata.

`CREATE SOURCE`, `REFRESH SOURCE`, and `VACUUM` are intentionally blocked through stdio MCP — they must be run via the interpreter or CLI. On `forgeql-server` they additionally require an admin bearer token from the `--auth-file` token store; normal and anonymous principals can only `USE` existing sources.

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
│   │       │   ├── index.rs      # IndexRow, SymbolTable, collect_nodes, node-id assignment
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
│   │       │   ├── mod.rs        # Branch, stage, commit via git2
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
│   │       │   └── diff.rs       # Boundary diff with inline node_id(offset) handles
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
│   │       ├── result.rs         # ForgeQLResult, SymbolMatch, ShowResult
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
│       ├── config/               #   xml, dbc, toml, json, yaml, ini, just, make,
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
- `node_kind` — the raw tree-sitter node kind (e.g. `function_definition`, `class_specifier`). Language-specific; internal use only. Deprecated for query use.
- `fql_kind` — a universal kind (e.g. `function`, `class`, `struct`). Language-agnostic, defined by `map_kind()`.

Always use `WHERE fql_kind = 'function'` rather than `WHERE node_kind = 'function_definition'`. `node_kind` remains in the index for internal purposes but is deprecated for agent queries.

### Crate Dependencies

```
forgeql (binary)
├── forgeql-core          zero language grammars
├── forgeql-lang-c        tree-sitter-c + CLanguage
├── forgeql-lang-cpp      tree-sitter-cpp + CppLanguage + CppMacroExpander
├── forgeql-lang-python   tree-sitter-python + PythonLanguage
├── forgeql-lang-rust     tree-sitter-rust + RustLanguage + RustMacroExpander
└── forgeql-lang-text     all structured-text grammars (XML, DBC, TOML, JSON,
                          YAML, INI, justfile, Make, CMake, Markdown, reST)
```

`forgeql-core` depends on `tree-sitter` (the library) but NOT on any grammar crate. Grammar dependencies live exclusively in language crates. The `forgeql` and `forgeql-server` registries splice every text format in with one `text_languages()` call, so a new text format is picked up by both binaries automatically.

---

## Adding a New Language

For a structured-text format, add a module + `config/<lang>.json` kind map inside `forgeql-lang-text` — no new crate needed. For a full programming language, add a single new crate with no changes to `forgeql-core`:

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
