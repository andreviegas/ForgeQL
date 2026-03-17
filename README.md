# ForgeQL

> **Declarative code transformation for the era of AI-assisted development.**

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

## What Is ForgeQL?

ForgeQL is a **declarative, code-aware transformation tool**. You describe *what* you want to find or change in a codebase and ForgeQL executes it precisely — leaving the strategy and file selection to the agent or developer driving it.

Think of it as **SQL for source code**: a small, expressive query language backed by real syntax trees (tree-sitter), not fragile regular expressions.

It works in two modes:
- **MCP server** — connects directly to AI coding agents (GitHub Copilot, Claude, etc.) inside VS Code or any MCP-capable editor.
- **Interpreter** — pipe a FQL statement into the binary from a terminal or script.

---

## Two Core Goals

### 1. Small Command Surface

ForgeQL is intentionally minimal. Everything is built from four command families:

| Family | Commands |
|---|---|
| **Session** | `CREATE SOURCE` · `REFRESH SOURCE` · `USE` · `SHOW SOURCES` · `SHOW BRANCHES` · `DISCONNECT` |
| **Queries** | `FIND symbols` · `FIND usages OF` · `FIND callees OF` · `FIND files` |
| **Content** | `SHOW body` · `SHOW signature` · `SHOW outline` · `SHOW members` · `SHOW context` · `SHOW LINES` |
| **Mutations** | `CHANGE FILE` / `CHANGE FILES` (with `MATCHING`, `LINES`, `WITH`, or `WITH NOTHING`) |

Complex workflows — renaming a symbol, applying a coding standard, migrating a pattern — are **composed by the agent** from these primitives. ForgeQL provides the precision tools; the agent decides the strategy.

### 2. Small Token Footprint

Every command accepts a universal clause set that shapes the output **before** it reaches the agent's context window:

```sql
WHERE field operator value   -- filter rows
HAVING field operator value  -- filter after GROUP BY
IN 'glob'                    -- restrict to files matching a glob
EXCLUDE 'glob'               -- exclude files matching a glob
ORDER BY field ASC|DESC      -- sort
GROUP BY field               -- aggregate
LIMIT N                      -- cap row count
OFFSET N                     -- paginate
DEPTH N                      -- collapse tree depth
```

These clauses work identically on every command. Instead of returning thousands of rows for the agent to sift through, a single precise query returns exactly what is needed:

```sql
FIND symbols
  WHERE node_kind = 'function_definition'
  IN 'src/**'
  ORDER BY usages DESC
  LIMIT 10
```

---

## Build and Install

### Prerequisites

| Tool | Minimum version |
|---|---|
| Rust / Cargo | 1.78 |
| Git | 2.x |
| VS Code | 1.90 (for MCP integration) |

tree-sitter grammars are compiled into the binary — no separate install needed.

### Clone and Build

```bash
git clone https://github.com/andreviegas/ForgeQL.git
cd ForgeQL
cargo build --release
```

The binary lands at `target/release/forgeql` (Linux) or `target\release\forgeql.exe` (Windows).

---

## Usage: MCP Server (VS Code)

This is the primary mode for AI agent use. ForgeQL speaks MCP over stdio; VS Code connects to it automatically once configured.

### Linux

Create `.vscode/mcp.json` in your workspace (or `~/.config/Code/User/mcp.json` for a global setup):

```json
{
  "servers": {
    "forgeql": {
      "command": "/home/<your-user>/ForgeQL/target/release/forgeql",
      "args": ["--mcp", "--data-dir", "/your/data-dir"]
    }
  }
}
```

### Windows

Create `.vscode/mcp.json` in your workspace:

```json
{
  "servers": {
    "forgeql": {
      "command": "C:\\Users\\<YourUser>\\ForgeQL\\target\\release\\forgeql.exe",
      "args": ["--mcp", "--data-dir", "C:\\your\\data-dir"]
    }
  }
}
```

You can also add `"--log-queries"` to the `args` array to write every FQL statement to a log file — useful for debugging what the agent is sending.

After saving, open the Command Palette (`Ctrl+Shift+P`) and run **MCP: Refresh Servers**. The ForgeQL tools appear in the Copilot Chat tool list and can be called by any MCP-aware extension.

---

## Usage: Interpreter Mode

You can also pipe any FQL statement directly to the binary. This is useful for scripting, quick lookups, and testing without an editor.

```bash
echo "SHOW SOURCES" | forgeql --data-dir /tmp/forgeql-lab

echo "FIND symbols WHERE node_kind = 'function_definition' LIMIT 5" \
  | forgeql --data-dir /tmp/forgeql-lab
```

---

## Quick Start: Pisco Code v1.3.0

The examples below walk through exploring and modifying [Pisco Code](https://github.com/pisco-de-luz/Pisco-Code), an embedded C++ library, pinned at tag `v1.3.0`.

All commands work identically whether typed in Copilot Chat (MCP mode) or piped to the binary (interpreter mode).

### Register and index the repository

```sql
CREATE SOURCE 'pisco' FROM 'https://github.com/pisco-de-luz/Pisco-Code.git'
USE pisco.v1.3.0
```

ForgeQL clones the repository, builds the tree-sitter index, and caches it on disk. Every subsequent query is served from the in-memory index — no re-reading files.

### Explore the structure

```sql
-- Top-level file tree
FIND files DEPTH 2

-- Structural outline of a header
SHOW outline OF 'include/PiscoCode.h'

-- All classes defined in the library
FIND symbols
  WHERE node_kind = 'class_specifier'
  ORDER BY name ASC
```

### Find specific symbols

```sql
-- All getter/setter methods
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE name LIKE 'get%'
  ORDER BY name ASC

-- All #define macros in headers
FIND symbols
  WHERE node_kind = 'preproc_def'
  IN 'include/**'
```

### Inspect a function

```sql
SHOW body OF 'PiscoCode::process'
```

Every `SHOW` response includes `start_line` and `end_line`. Those values feed directly into a `CHANGE LINES` command — no round-trip to re-read the file:

```json
{
  "symbol": "PiscoCode::process",
  "file": "src/PiscoCode.cpp",
  "start_line": 87,
  "end_line": 103,
  "content": "void PiscoCode::process(...) { ... }"
}
```

### Audit dead code

```sql
-- Functions that are never called
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE usages = 0
  IN 'src/**'
  EXCLUDE 'src/tests/**'

-- Usage count per file for a given symbol
FIND usages OF 'PiscoCode::process'
  GROUP BY file
  ORDER BY count DESC
```

### Make changes inside a transaction

Transactions group multiple commands atomically. If `VERIFY` fails, every modified file is restored automatically.

```sql
BEGIN TRANSACTION 'rename-process'
  CHANGE FILES 'src/**/*.cpp', 'include/**/*.h'
    MATCHING 'PiscoCode::process' WITH 'PiscoCode::run'
  VERIFY 'release', 'test'
COMMIT MESSAGE 'rename PiscoCode::process to PiscoCode::run'
```

### Edit a specific function body

```sql
-- Step 1: get the exact line range
SHOW body OF 'PiscoCode::init'

-- Step 2: replace those lines with the new implementation
CHANGE FILE 'src/PiscoCode.cpp'
  LINES 87-103
  WITH 'void PiscoCode::run(Buffer& buffer) {
    for (auto& sample : buffer) {
        sample = this->pipeline.apply(sample);
    }
}'
```

### Remove a deprecated function

```sql
-- After SHOW body returns start_line=200, end_line=214
BEGIN TRANSACTION 'remove-legacyHelper'
  CHANGE FILE 'src/PiscoCode.cpp'
    LINES 200-214
    WITH NOTHING
  VERIFY 'test'
COMMIT MESSAGE 'remove deprecated legacyHelper'
```

---

## About This Project

ForgeQL was conceived, designed, and validated by [Andre Viegas](https://github.com/andreviegas) — a C/C++ developer exploring Rust for the first time through this project.

**Full transparency:** 100% of the Rust code in this repository was initially generated by AI (GitHub Copilot / Claude). The architecture, the ForgeQL language design, the test strategy, and every design decision were mine; the AI translated those decisions into working Rust. This started as a proof of concept to answer a simple question: *can a declarative, AST-aware transformation language make AI-assisted coding safer and more efficient?*

Early results suggest it can. If you find the idea useful, I'd love help from experienced Rust developers to take it further — improving idiomatic Rust patterns, performance, multi-language support, and anything else that makes ForgeQL a better tool. See [CONTRIBUTING.md](CONTRIBUTING.md) for how to get involved.

---

## Further Reading

- [doc/syntax.md](doc/syntax.md) — complete command and clause reference with more examples.
- [doc/architecture.md](doc/architecture.md) — internal design: index model, clause pipeline, MCP layer.

---

## License

Apache License 2.0 — see [LICENSE](LICENSE).
