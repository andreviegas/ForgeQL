# ForgeQL Agent Instructions

This file provides instructions for AI coding agents working with the ForgeQL MCP server.
Works with VS Code Copilot and Claude Code (both read AGENTS.md from workspace root).

---

## Core Principle

All source code is accessed **exclusively** through ForgeQL MCP tools.
The local workspace may be empty — never fall back to local filesystem tools (grep, find, cat, read_file).

## HINTS.md — Persistent Session Knowledge

After completing a meaningful task, append a bullet point summary of the most important codebase facts discovered to a `HINTS.md` file in the **workspace root** (next to the `.forgeql-index`). Use one line per fact:

```markdown
# ForgeQL Hints
- Naming convention: BT controller internal constants use `BT_CTLR_*` prefix + `_CNT` suffix
- Channel count constant for BLE data channels: define in `lll_chan.h`, value = 37
- SD CSD register bit-shift constants belong in `include/zephyr/sd/sd_spec.h`
```

Do **not** create the file if nothing significant was discovered. Keep bullets short — one line each, factual only.

## Setup

Always start a session with (the `AS` clause is mandatory — the alias becomes the session id):
```sql
USE source_name.branch AS 'alias'
```

When connected to `forgeql-server` over HTTP, the `USE` response returns a
server-issued `session_id` token scoped to the authenticated user — store it
and pass it verbatim in every subsequent call; do not reconstruct it from the
alias.

## Query Workflow

**The right way to find and read code:**

1. `FIND symbols WHERE ...` → get name, file path, line number
2. `SHOW NODE '<node_id>'` → read the located node by its stable handle

**Do NOT:**
- Dump large function bodies without explicit LIMIT
- Brute-force whole files instead of `FIND symbols WHERE` + `SHOW NODE`
- Fall back to grep/find/cat for code reading
- Use unfiltered FIND queries on large codebases

## Query Strategy

| Need | Command |
|---|---|
| Find a symbol | `FIND symbols WHERE name LIKE 'pattern' [WHERE fql_kind = '...']` |
| Read a located node | `SHOW NODE '<node_id>'` |
| Read/splice lines within a node | `SHOW NODE '<id>(n-m)'` · `CHANGE NODE '<id>(n-m)' WITH '...'` — 1-based offset within the node's own span |
| Function signature | `SHOW body OF 'name' DEPTH 0` — also returns enrichment metadata |
| Qualified symbol | `SHOW body OF 'Class::method'` or `SHOW body OF 'Obj.method'` |
| Control flow overview | `SHOW body OF 'name' DEPTH 1` |
| Blast radius | `FIND usages OF 'name' GROUP BY file ORDER BY count DESC` — one row per usage site, includes non-call references |
| File structure | `SHOW outline OF 'file'` |
| Class members | `SHOW members OF 'type'` |
| Call graph | `SHOW callees OF 'name'` |
| File listing | `FIND files [IN 'path/**'] [WHERE name = '...'] [WHERE extension = '...']` — every row carries `node_id` + `rev`; directories are rows too, marked by a trailing slash (`WHERE path LIKE '%/'`) |
| Whole file / directory as a node | `n<hex>` with no ordinal (straight from `FIND files`): `SHOW NODE '<hex>'` reads it, `'<hex>(k-m)'` a line range, `INSERT AFTER NODE '<hex>'` appends at EOF |
| Review an uncommitted change | `SHOW DIFF STAT` — the file map; then `SHOW DIFF IN 'path/**'` for hunks. `EXPORT PATCH` covers **committed** work only. |
| Hotspots | `FIND symbols WHERE fql_kind = 'function' ORDER BY usages DESC LIMIT 10` — `usages` is a real workspace-total count |

## Efficiency

- All commands accept `WHERE`, `GROUP BY`, `ORDER BY`, `LIMIT`, `OFFSET` — combine freely.
- `IN 'src'` and `IN 'crates/'` auto-expand to `IN 'src/**'` — bare directory paths are always safe.
- Multiple `WHERE` clauses combine as AND.
- FIND defaults to 20 rows. Add LIMIT N for more.
- SHOW body and SHOW context returning more than 40 lines without explicit LIMIT are capped to the first window (`SHOW MORE` pages the rest). **`SHOW NODE '<id>'` returns a node's full span** — read a located node by its handle, at any size.
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

Edit indexed code by stable `node_id` **only**: `CHANGE NODE` replaces a node, `INSERT BEFORE/AFTER NODE` adds around it, `DELETE NODE` removes it, and `MOVE NODE` relocates it byte-for-byte in one atomic step. `CHANGE FILE` on indexed files is disabled; raw-text file edits (`CHANGE FILE`, line-range copy and move) exist for non-indexed files and file scaffolding — see the syntax reference. This applies to config too: TOML, YAML, JSON, XML/arxml, DBC, Makefiles, CMake, INI, justfiles, and Markdown are all node-addressable.

| Command | Effect |
|---|---|
| `CHANGE NODE '<id>' WITH '...'` | Replace the node's source span (heredoc `WITH <<TAG … TAG` when content has quotes) |
| `CHANGE NODE '<id>(n-m)' WITH '...'` | Replace lines n–m within the node |
| `INSERT BEFORE/AFTER NODE '<id>' WITH '...'` | Insert lines around the node |
| `DELETE NODE '<id>'` / `DELETE NODE '<id>(n-m)'` | Delete the node, or lines n–m within it |
| `DELETE NODE '<hex>' IF REV '<rev>'` | Delete a whole **file** — or a **directory** and its subtree. `IF REV` is **mandatory** here, as it is for a whole-file `CHANGE NODE '<hex>'`: a node edit can be corrected afterwards, a deleted file cannot be re-read. Take the `rev` straight off the `FIND files` row. |
| `MOVE NODE '<src>' (BEFORE\|AFTER) NODE '<dst>'` | Relocate the node byte-for-byte — atomic, cross-file, no re-indent |
| `MOVE NODE '<hex>' IF REV '<rev>' TO '<path>'` | Rename or move a file: `TO` takes a directory handle (basename kept) or a path. The source is unlinked, not emptied. `COPY NODE … TO …` is the ungated twin. |
| `INSERT NODE FOR '<path>'` | Create an empty file — or a directory, with a trailing slash — and get its handle back. Then write into it with `INSERT AFTER NODE '<hex>' WITH …`. |
| `<mutation> IF REV '<rev>'` | Guard a mutation on the node's content rev |
| `UNDO` / `UNDO LAST-n` | Reverse recent mutations from the per-session undo ring |
| Raw-text `CHANGE FILE` / copy / move | Non-indexed files only — see the syntax reference |

**The diff is the contract.** Mutations are mechanical — the engine never fixes commas, wraps braces, or re-indents. Every mutation returns `new_node_id`, `lines_written`, `lines_removed`, and a boundary diff whose context lines carry inline `node_id(offset)` handles. Read the diff after every mutation: if it shows a seam you created, issue the follow-up `CHANGE NODE '<id>(off)'` yourself. A large `lines_removed` on a small edit means you clobbered more than intended — `UNDO` reverses it.

**Renames** are a composition: `FIND usages OF 'old'` → inspect sites → one targeted `CHANGE NODE` per site.

Wrap mutations in a transaction and gate the commit:

    BEGIN TRANSACTION 'name'
      CHANGE NODE '<node_id>' WITH 'fixed'
      VERIFY build 'test'          -- or JOB START 'test' for long gates
    COMMIT MESSAGE 'fix: ...'

Verify steps marked `commit_gate: true` in `.forgeql.yaml` must pass **after** the last edit or COMMIT is refused — re-run the gate after every fix. For long-running gates use `JOB START '<step>'` (returns a job id; poll with `JOB STATUS <id>`) instead of blocking on `VERIFY`.
