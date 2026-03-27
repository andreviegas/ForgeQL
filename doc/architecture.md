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
              │  + .forgeql-index     │
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
    ShowBranches { source },
    Disconnect,

    // Queries — all carry Clauses
    FindSymbols { clauses },
    FindUsages { of, clauses },
    FindFiles { clauses },

    // Content — all carry Clauses
    ShowBody { symbol, clauses },
    ShowSignature { symbol, clauses },
    ShowOutline { file, clauses },
    ShowMembers { symbol, clauses },
    ShowContext { symbol, clauses },
    ShowCallees { symbol, clauses },
    ShowLines { file, start_line, end_line, clauses },

    // Mutations
    ChangeContent { files, target, clauses },
    CopyLines { src, start, end, dst, at },
    MoveLines { src, start, end, dst, at },
    // Composite operations
    Transaction { name, ops, verify, message },
    Rollback { name },
}
```

Note: `FIND callees OF 'x'` and `FIND globals` are accepted by the grammar but the parser routes them to `ShowCallees` and `FindSymbols` (with a `node_kind` predicate) respectively — they are syntactic aliases, not separate IR variants.

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

The MCP layer exposes tools to the agent via the MCP JSON-RPC protocol over stdio:

| Tool | Purpose |
|---|---|
| `run_fql` | Execute any FQL statement — the primary tool for agents that generate query strings |
| `use_source` | Start or resume a session on a specific branch |
| `find_symbols` | Search symbols by pattern (structured parameters) |
| `find_usages` | Find all references to a symbol |
| `show_body` | Show function body with optional depth control |
| `disconnect` | End the active session and release the worktree |

`CREATE SOURCE` and `REFRESH SOURCE` are intentionally blocked through MCP — they must be run via the interpreter.

### Agent Guardrails

The MCP layer includes two mechanisms that prevent AI agents from misusing ForgeQL:

**`with_instructions()`** — The server's `get_info()` response includes a structured instruction text that is injected into the agent's system prompt during the MCP `initialize` handshake. This text contains:
- Critical rules (never use local filesystem, always start with `USE`)
- Query strategy decision tree (FIND → SHOW LINES workflow)
- Efficiency rules (default limits, progressive depth)

These instructions reach the agent regardless of which editor or platform it runs on — they are part of the MCP protocol itself.

**SHOW line blocking** — SHOW commands that return source lines (`body`, `lines`, `context`) are subject to a default line limit (`DEFAULT_SHOW_LINE_LIMIT = 40`). When output exceeds this limit and the agent did not include an explicit `LIMIT` clause:
- Zero lines are returned
- A guidance message tells the agent to use `FIND symbols WHERE` to locate the exact symbol, then `SHOW LINES n-m OF 'file'` for targeted reading
- If the agent genuinely needs more lines, it can re-run with an explicit `LIMIT N`

This creates a teaching moment on first contact — after hitting the block once, agents learn the precision workflow.

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

## Data Flow: a CHANGE transaction

```
BEGIN TRANSACTION 'rename-process'
  CHANGE FILES 'src/**/*.cpp' MATCHING 'process' WITH 'run'
  VERIFY build 'test'
COMMIT MESSAGE 'rename process to run'

1. Parser    → ForgeQLIR::Transaction {
                 ops: [ChangeContent { files: ["src/**/*.cpp"],
                                       target: Matching { "process", "run" } }],
                 verify: Some("test"),
                 message: Some("rename process to run")
               }
2. Engine    → snapshot all matched files (in-memory backup)
3. Engine    → for each op:
                 glob expand → read file → apply replacement → write file
4. Engine    → run verify target (looked up from .forgeql.yaml)
               on failure → restore all snapshots, return error
               on success → return Ok
5. MCP layer → return result to agent
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
│   │       └── path_utils.rs
│   ├── forgeql-core/             # All core logic (no binary, no language grammars)
│   │   └── src/
│   │       ├── ast/
│   │       │   ├── lang.rs       # LanguageSupport trait, LanguageConfig, LanguageRegistry
│   │       │   ├── index.rs      # IndexRow, SymbolTable, collect_nodes
│   │       │   ├── query.rs      # find_symbols, find_usages
│   │       │   ├── show.rs       # show_body, show_signature, show_outline, …
│   │       │   ├── cache.rs      # Index serialization/deserialization (bincode)
│   │       │   └── enrich/       # Enrichment modules (naming, comments, numbers,
│   │       │                     #   control_flow, operators, metrics, casts,
│   │       │                     #   redundancy, scope, member, decl_distance,
│   │       │                     #   escape, shadow, unused_param, fallthrough,
│   │       │                     #   recursion, todo)
│   │       ├── parser/
│   │       │   ├── forgeql.pest  # PEG grammar
│   │       │   └── mod.rs        # Parser functions → IR
│   │       ├── git/
│   │       │   ├── mod.rs        # Branch, stage, commit via git2
│   │       │   ├── source.rs     # Source + SourceRegistry (bare repo management)
│   │       │   └── worktree.rs   # Worktree lifecycle: create, list, remove
│   │       ├── session/
│   │       │   └── mod.rs        # Session management (user → worktree → index)
│   │       ├── transforms/
│   │       │   ├── mod.rs        # TransformPlan, ByteRangeEdit, FileEdit
│   │       │   ├── change.rs     # File mutation: matching, lines, with, delete
│   │       │   ├── copy_move.rs  # COPY LINES / MOVE LINES planning and execution
│   │       │   └── diff.rs       # Pure-Rust LCS unified diff generator
│   │       ├── verify/
│   │       │   └── mod.rs        # Run build/test verification steps
│   │       ├── workspace/
│   │       │   ├── mod.rs        # Workspace root discovery, file enumeration
│   │       │   └── file_io.rs    # Atomic write, .forgeql-ignore support
│   │       ├── engine.rs         # Command dispatch + session management + SHOW guardrails
│   │       ├── compact.rs        # Compact CSV output renderer (MCP mode)
│   │       ├── filter.rs         # apply_clauses(), ClauseTarget trait
│   │       ├── ir.rs             # ForgeQLIR, Clauses, Predicate, ChangeTarget
│   │       ├── result.rs         # ForgeQLResult, SymbolMatch, ShowResult
│   │       ├── config.rs         # .forgeql.yaml deserialization
│   │       ├── context.rs        # RequestContext + Permission
│   │       ├── error.rs          # ForgeError (thiserror)
│   │       └── query_logger.rs   # FQL statement logging (--log-queries)
│   ├── forgeql-lang-cpp/         # C++ language support crate
│   │   └── src/
│   │       └── lib.rs            # CppLanguage, CPP_CONFIG, map_kind(), cpp_registry()
│   └── forgeql-lang-rust/        # Rust language support crate
│       ├── config/
│       │   └── rust.json         # kind_map, enricher hints, node kind sets
│       └── src/
│           └── lib.rs            # RustLanguage, RUST_CONFIG, rust_registry()
├── doc/
│   ├── syntax.md                 # Command and clause reference
│   ├── architecture.md           # This file
│   └── agents/                   # Distributable agent configs
│       ├── forgeql.agent.md      # VS Code Copilot Custom Agent (tools locked)
│       ├── AGENTS.md             # Platform-agnostic workspace instructions
│       ├── README.md             # Installation guide
│       ├── references/           # On-demand reference docs for agents
│       ├── claude-code/          # Claude Code adapter
│       └── cursor/               # Cursor adapter
└── tests/                        # Integration tests + fixtures
```

---

## Language-Agnostic Architecture

ForgeQL's core (`forgeql-core`) contains zero language-specific code. All language knowledge lives in external crates — one per language.

### Key Abstractions (defined in `ast/lang.rs`)

**`LanguageConfig`** — a static struct containing all language-specific data: node kind sets, modifier maps, cast kinds, number suffixes, comment prefixes, visibility keywords, and data-flow analysis node kinds (`parameter_list_raw_kind`, `identifier_raw_kind`, `assignment_raw_kinds`, `update_raw_kinds`, `init_declarator_raw_kind`, `block_raw_kind`). Each language crate defines a `static CPP_CONFIG: LanguageConfig` (or equivalent).

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
├── forgeql-core        zero language grammars
├── forgeql-lang-cpp    tree-sitter-cpp + CppLanguage
└── forgeql-lang-rust   tree-sitter-rust + RustLanguage
```

`forgeql-core` depends on `tree-sitter` (the library) but NOT on any grammar crate. Grammar dependencies live exclusively in language crates.

---

## Adding a New Language

Adding a new language requires a single new crate with no changes to `forgeql-core`:

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
