# ForgeQL Quick Reference for AI Agents

**Last Updated**: March 17, 2026  
**ForgeQL Version**: 0.19.2

This guide is for AI coding agents (GitHub Copilot, Claude, etc.) working with ForgeQL via MCP.

---

## Setup (Human does once)

1. Create `.vscode/mcp.json`:
```jsonc
{
  "servers": {
    "forgeql": {
      "command": "/path/to/forgeql",
      "args": ["--mcp", "--data-dir", "/tmp/forgeql-workspace"]
    }
  }
}
```

2. Register source and create index:
```bash
echo "CREATE SOURCE 'myproject' FROM 'https://github.com/.../repo.git'" | forgeql --data-dir /tmp/forgeql-workspace
```

3. Tell agent: "USE myproject.main" or "USE myproject.branch-name"

---

## Commands (All Accept Universal Clauses)

### Session
| Command | Purpose |
|---------|---------|
| `USE source.branch [AS 'alias']` | Switch to branch/tag (always first!) |
| `SHOW SOURCES` | List registered sources |
| `SHOW BRANCHES [OF 'source']` | List available branches |
| `DISCONNECT` | Clean up session |

### Query (Return rows)
| Command | Purpose |
|---------|---------|
| `FIND symbols [WHERE ...]` | Query AST nodes (functions, classes, macros, includes, comments, etc.) |
| `FIND usages OF 'name' [GROUP BY file]` | Find all references + blast radius |
| `FIND callees OF 'name'` | What does `name` call? |
| `FIND files [WHERE ...] [ORDER BY ...] [LIMIT N]` | List/explore directory tree — all clauses supported |

### Inspect (Return formatted content)
| Command | Purpose |
|---------|---------|
| `SHOW body OF 'name' [DEPTH N]` | Full function body (default DEPTH=0: signature only) |
| `SHOW signature OF 'name'` | Declaration + parameters only |
| `SHOW outline OF 'file'` | File structure (all top-level symbols) |
| `SHOW members OF 'Class'` | Class/struct fields + methods |
| `SHOW context OF 'name'` | ±5 lines around definition |
| `SHOW callees OF 'name'` | What does `name` call? |
| `SHOW LINES n-m OF 'file'` | Verbatim line range |

### Mutate (Modify code)
| Command | Purpose |
|---------|---------|
| `CHANGE FILE 'path' WITH 'content'` | Create/overwrite entire file |
| `CHANGE FILE 'path' LINES n-m WITH 'text'` | Replace line range |
| `CHANGE FILE 'path' LINES n-m WITH NOTHING` | Delete line range |
| `CHANGE FILES 'f1','f2' MATCHING 'old' WITH 'new'` | Multi-file text replace (AST-scoped) |

### Transactions (Atomic mutations)
| Command | Purpose |
|---------|---------|
| `BEGIN TRANSACTION 'name' ... COMMIT MESSAGE 'msg'` | Atomic block (auto-rollback on error) |
| `VERIFY build 'step'` | Run build target from `.forgeql.yaml` (standalone or in txn) |
| `ROLLBACK [TRANSACTION 'name']` | Undo transaction(s) |

---

## Universal Clauses (Always Available)

**Pipeline order** (applied automatically regardless of order in query):  
`IN` → `EXCLUDE` → `WHERE` → `GROUP BY` → `HAVING` → `ORDER BY` → `OFFSET` → `LIMIT`

```sql
WHERE field LIKE 'pattern'          -- SQL wildcards: % (any), _ (one)
WHERE field = 'exact'               -- Exact match
WHERE field > N                      -- Numeric comparison (>, >=, <, <=, !=)
WHERE field NOT LIKE 'pattern'       -- Negation
HAVING count >= 10                  -- Post-GROUP BY filtering
IN 'glob/**'                         -- Only these files
EXCLUDE 'tests/**'                  -- Skip these files
GROUP BY (file | kind | node_kind)  -- Aggregate by field
ORDER BY (name | path | usages | line | count) [ASC | DESC]
LIMIT N                              -- Max results
OFFSET N                             -- Skip N results
DEPTH N                              -- For SHOW body / FIND files
```

### Filterable Fields

**Always available (symbol results):**
- `name` (string) — symbol name
- `node_kind` (string) — tree-sitter kind (any kind is filterable)
- `path` (string) — relative file path
- `line` (integer) — 1-based start line
- `usages` (integer) — reference count

**FIND files results:**
- `path` (string) — relative file path
- `extension` (string) — file extension without the leading `.` (e.g. `cpp`, `h`, `md`)
- `size` (integer) — file size in bytes
- `depth` (integer) — directory depth

**Dynamic (any tree-sitter field):**
- `type` — return type / variable type
- `value` — macro value / initial value
- `parameters` — function parameter list
- etc. (Any field tree-sitter extracts)

**Special node_kind values:**
- `function_definition`, `function_declarator`, `class_specifier`, `struct_specifier`, `enum_specifier`, `preproc_def`, `preproc_include`, `declaration`, `comment`, `field_declaration`, `parameter_declaration`

---

## Best Practices for AI Agents

### 1. **Use CSV Format (saves ~60% tokens)**
```sql
-- Always request CSV for FIND queries
FIND symbols WHERE node_kind = 'function_definition' LIMIT 50
-- Specify in MCP call: format="CSV"
```

### 2. **Follow the Safe Refactoring Pattern**
```sql
-- Step 1: Discover (name only, low token cost)
FIND symbols WHERE name = 'oldFunction'

-- Step 2: Blast radius (count references per file)
FIND usages OF 'oldFunction' GROUP BY file ORDER BY count DESC

-- Step 3: Inspect context if needed (use DEPTH to limit output)
SHOW body OF 'oldFunction' DEPTH 1

-- Step 4: Apply atomically with verification
BEGIN TRANSACTION 'rename-oldFunction'
  CHANGE FILES 'src/**/*.cpp', 'include/**/*.h' MATCHING 'oldFunction' WITH 'newFunction'
  VERIFY build 'test'
COMMIT MESSAGE 'refactor: rename oldFunction → newFunction'
```

### 3. **Progressive Code Disclosure (Token Efficient)**
```sql
SHOW body OF 'complexFunc'              -- Default DEPTH=0 → signature only (60 tokens)
SHOW body OF 'complexFunc' DEPTH 1      -- Control-flow skeleton (~675 tokens)
SHOW body OF 'complexFunc' DEPTH 99     -- Full body only if absolutely needed (3800+ tokens)
```

### 4. **Pagination for Large Results**
```sql
FIND symbols WHERE ... LIMIT 50 OFFSET 0   -- Page 1
FIND symbols WHERE ... LIMIT 50 OFFSET 50  -- Page 2
```

### 5. **Scope Queries to Reduce Output**
```sql
-- ✅ Good: Filtered + scoped + limited
FIND symbols WHERE node_kind = 'function_definition' WHERE usages = 0 
  IN 'src/**' EXCLUDE 'tests/**' LIMIT 20

-- ❌ Avoid: Unfiltered on large codebase
FIND symbols
```

---

## Workflow Templates

### Find and Fix Dead Code
```sql
FIND symbols WHERE node_kind = 'function_definition' WHERE usages = 0 
  EXCLUDE 'tests/**' IN 'src/**' ORDER BY path ASC

-- For each: SHOW body OF 'funcName' to verify it's safe to delete
-- Then: BEGIN TRANSACTION ... CHANGE FILE ... LINES ... WITH NOTHING ... COMMIT
```

### Rename Symbol Across Codebase
```sql
FIND usages OF 'OldName' GROUP BY file ORDER BY count DESC
-- Shows you which files are affected and how many times each

BEGIN TRANSACTION 'rename-OldName'
  CHANGE FILES 'src/**/*.cpp', 'include/**/*.h' MATCHING 'OldName' WITH 'NewName'
  VERIFY build 'test'
COMMIT MESSAGE 'refactor: rename OldName → NewName'
```

### Audit: Find Functions with High Coupling
```sql
FIND symbols WHERE node_kind = 'function_definition' WHERE usages >= 10 
  ORDER BY usages DESC LIMIT 10
-- Shows hotspots worth refactoring
```

### Find Non-Code Files in the Source Tree
```sql
-- All files that are NOT C/C++ source or headers
FIND files IN 'src/**'
  WHERE extension NOT LIKE 'cpp'
  WHERE extension NOT LIKE 'c'
  WHERE extension NOT LIKE 'h'
  WHERE extension NOT LIKE 'hpp'

-- Equivalent: match by path pattern
FIND files WHERE path NOT LIKE '%.cpp' WHERE path NOT LIKE '%.h'

-- Find large files (e.g. generated or binary assets)
FIND files WHERE size > 100000 ORDER BY size DESC LIMIT 20

-- Only CMake and markdown files
FIND files WHERE extension = 'cmake'
FIND files WHERE extension = 'md'
```

### Update Configuration Values
```sql
BEGIN TRANSACTION 'bump-version'
  CHANGE FILE 'include/version.h' MATCHING 'VERSION "1.0"' WITH 'VERSION "1.1"'
  CHANGE FILE 'src/config.cpp' MATCHING 'VERSION_STR = "1.0"' WITH 'VERSION_STR = "1.1"'
  VERIFY build 'test'
COMMIT MESSAGE 'bump version to 1.1'
```

---

## Known Limitations & Workarounds

| Issue | Workaround |
|-------|-----------|
| Template functions show empty callees | Use `FIND usages OF 'name'` instead |
| Numeric comparisons skip hex/symbolic values | Use `WHERE value LIKE 'pattern'` instead |
| Escape sequences treated literally | Write actual newlines in string content |
| Angle brackets not preserved in includes | Use `WHERE name LIKE 'std%'` patterns |

---

## Quick Checklist for New Session

- [ ] `.vscode/mcp.json` configured with correct forgeql path + data-dir
- [ ] Source registered: `echo "CREATE SOURCE ..." | forgeql ...`
- [ ] Run `USE source.branch` as first command
- [ ] Use CSV format for all FIND queries
- [ ] Scope queries with `IN`, `EXCLUDE`, `LIMIT`
- [ ] Use `DEPTH 0` by default for SHOW body
- [ ] Wrap multi-step changes in transactions
- [ ] Include `VERIFY build` in critical refactorings

---

## Example: Full Bug Fix Workflow

```sql
-- 1. Discover the bug location
USE myproject.main
FIND symbols WHERE name = 'buggyFunction'
SHOW body OF 'buggyFunction' DEPTH 1

-- 2. Check what calls it (blast radius)
FIND usages OF 'buggyFunction' GROUP BY file

-- 3. Trace dependencies
SHOW callees OF 'buggyFunction'

-- 4. Fix atomically with verification
BEGIN TRANSACTION 'fix-bug-in-buggyFunction'
  CHANGE FILE 'src/module.cpp' LINES 45-67 WITH 'fixed code...'
  VERIFY build 'test'
COMMIT MESSAGE 'fix: buggyFunction now handles edge case correctly'

-- 5. Verify fix didn't break anything
ROLLBACK                -- If needed to undo
```

---

**For full command reference**: See `syntax.md` in the repository root.
