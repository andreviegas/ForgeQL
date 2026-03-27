# ForgeQL Agent Instructions

This file provides instructions for AI coding agents working with the ForgeQL MCP server.
Works with VS Code Copilot and Claude Code (both read AGENTS.md from workspace root).

---

## Core Principle

All source code is accessed **exclusively** through ForgeQL MCP tools.
The local workspace may be empty — never fall back to local filesystem tools (grep, find, cat, read_file).

## Setup

Always start a session with:
```sql
USE source_name.branch
```

## Query Workflow

**The right way to find and read code:**

1. `FIND symbols WHERE ...` → get name, file path, line number
2. `SHOW LINES n-m OF 'file'` → read only those lines

**Do NOT:**
- Dump large function bodies without explicit LIMIT
- Scan files line-by-line with SHOW LINES 1-500
- Fall back to grep/find/cat for code reading
- Use unfiltered FIND queries on large codebases

## Query Strategy

| Need | Command |
|---|---|
| Find a symbol | `FIND symbols WHERE name LIKE 'pattern' [WHERE fql_kind = '...']` |
| Read specific lines | `SHOW LINES n-m OF 'file'` |
| Function signature | `SHOW body OF 'name' DEPTH 0` |
| Control flow overview | `SHOW body OF 'name' DEPTH 1` |
| Blast radius | `FIND usages OF 'name' GROUP BY file ORDER BY count DESC` |
| File structure | `SHOW outline OF 'file'` |
| Class members | `SHOW members OF 'type'` |
| Call graph | `SHOW callees OF 'name'` |
| File listing | `FIND files [IN 'path/**'] [WHERE extension = '...']` |

## Efficiency

- All commands accept `WHERE`, `GROUP BY`, `ORDER BY`, `LIMIT`, `OFFSET` — combine freely.
- Multiple `WHERE` clauses combine as AND.
- FIND defaults to 20 rows. Add LIMIT N for more.
- SHOW commands returning more than 40 lines without explicit LIMIT are blocked.
- Format defaults to CSV (~60% fewer tokens).
- Every response includes `tokens_approx` — if large, narrow with WHERE, IN, EXCLUDE.

## Enrichment Fields for Code Quality

ForgeQL indexes code quality metrics at parse time. Use them in WHERE clauses:

- `is_magic = 'true'` — magic numbers
- `has_assignment_in_condition = 'true'` — assignment in condition
- `mixed_logic = 'true'` — mixed && / || without grouping
- `condition_tests >= 4` — complex conditions
- `has_catch_all = 'false'` — switch without default
- `goto_count >= 1` — functions with goto
- `lines >= 50` — large functions
- `usages = 0` — dead code candidates
- `has_doc = 'false'` — undocumented functions
- `has_escape = 'true'` — local variables escaping their function
- `has_shadow = 'true'` — variable shadowing in nested scopes
- `has_unused_param = 'true'` — unused function parameters
- `has_fallthrough = 'true'` — switch/case fallthrough without break
- `is_recursive = 'true'` — directly recursive functions
- `has_todo = 'true'` — TODO/FIXME/HACK/XXX markers in comments

## Mutations

Use `CHANGE` to modify file content, `COPY LINES` to copy a line range from one file to another, and `MOVE LINES` to move it (cut from source, paste to destination).

| Command | Effect |
|---|---|
| `CHANGE FILE 'f' LINES n-m WITH '...'` | Replace lines n-m |
| `CHANGE FILE 'f' LINES n-m WITH NOTHING` | Delete lines n-m |
| `CHANGE FILES '*.c' MATCHING 'old' WITH 'new'` | Bulk literal replacement |
| `CHANGE FILE 'f' WITH '...'` | Replace entire file |
| `COPY LINES n-m OF 'src' TO 'dst'` | Copy lines, append to dst |
| `COPY LINES n-m OF 'src' TO 'dst' AT LINE k` | Copy lines, insert before line k in dst |
| `MOVE LINES n-m OF 'src' TO 'dst'` | Move lines (cut+paste), append to dst |
| `MOVE LINES n-m OF 'src' TO 'dst' AT LINE k` | Move lines, insert before line k in dst |

Wrap mutations in a transaction for atomic rollback:

    BEGIN TRANSACTION 'name'
      CHANGE FILE 'src/foo.c' LINES 10-12 WITH 'fixed'
      VERIFY build 'test'
    COMMIT MESSAGE 'fix: ...'
