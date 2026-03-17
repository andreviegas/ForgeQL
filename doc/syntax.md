# ForgeQL Syntax Reference

This document is the authoritative reference for every command and clause accepted by ForgeQL.

Commands are grouped into four families. Every command accepts the [universal clause set](#universal-clauses); inapplicable clauses are silently skipped.

---

## Table of Contents

1. [Notation](#notation)
2. [Session Commands](#session-commands)
3. [Query Commands — FIND](#query-commands--find)
4. [Content Commands — SHOW](#content-commands--show)
5. [Mutation Commands — CHANGE](#mutation-commands--change)
6. [Transaction Commands](#transaction-commands)
7. [Universal Clauses](#universal-clauses)
8. [Operators and Values](#operators-and-values)
9. [Filterable Fields](#filterable-fields)
10. [Use Cases: Pisco Code v1.3.0](#use-cases-pisco-code-v130)

---

## Notation

| Symbol | Meaning |
|---|---|
| `UPPERCASE` | Keyword — write exactly as shown |
| `'string'` | String literal — single quotes only |
| `N` | Integer literal |
| `n-m` | Inclusive line range, e.g. `10-25` |
| `[ … ]` | Optional element |
| `( A \| B )` | Choose one |
| `…` | Repeat one or more |

---

## Session Commands

Session commands connect ForgeQL to a remote repository, switch branches, and manage the active source.

---

### `CREATE SOURCE`

Register a remote repository and clone it locally.

```sql
CREATE SOURCE 'name' FROM 'url'
```

| Parameter | Description |
|---|---|
| `'name'` | Logical alias used in subsequent `USE` commands |
| `'url'` | Any URL accepted by `git clone` (HTTPS, SSH, local path) |

**Example**

```sql
CREATE SOURCE 'pisco' FROM 'https://github.com/pisco-de-luz/Pisco-Code.git'
```

---

### `REFRESH SOURCE`

Re-fetch and re-index a registered source (equivalent to `git fetch` + rebuild index).

```sql
REFRESH SOURCE 'name'
```

**Example**

```sql
REFRESH SOURCE 'pisco'
```

---

### `USE`

Set the active worktree to a specific branch or tag of a registered source. All subsequent queries operate on this worktree.

```sql
USE source.branch [AS 'alias']
```

| Parameter | Description |
|---|---|
| `source.branch` | Dot-separated source name and branch/tag name |
| `AS 'alias'` | Optional human-readable label for the session |

**Examples**

```sql
USE pisco.main
USE pisco.v1.3.0
USE pisco.v1.3.0 AS 'pisco-stable'
```

---

### `SHOW SOURCES`

List all registered sources and their local clone paths.

```sql
SHOW SOURCES
```

---

### `SHOW BRANCHES`

List all branches and tags available for a source.

```sql
SHOW BRANCHES [OF 'source']
```

**Examples**

```sql
SHOW BRANCHES
SHOW BRANCHES OF 'pisco'
```

---

### `DISCONNECT`

Close the active session and release the worktree lock.

```sql
DISCONNECT
```

---

## Query Commands — FIND

`FIND` commands query the index and return rows. All `FIND` commands accept the full [universal clause set](#universal-clauses).

---

### `FIND symbols`

Return indexed AST nodes — functions, classes, macros, variables, includes, enums, and any other named node tree-sitter can identify.

```sql
FIND symbols [clauses]
```

A bare `FIND symbols` returns everything in the index. Use `WHERE node_kind = '...'` to narrow to a specific kind.

**Common `node_kind` values (C/C++)**

| `node_kind` | What it matches |
|---|---|
| `function_definition` | Function definitions with body |
| `function_declarator` | Forward declarations |
| `struct_specifier` | `struct` declarations |
| `class_specifier` | `class` declarations |
| `enum_specifier` | `enum` declarations |
| `preproc_def` | `#define` macros |
| `preproc_include` | `#include` directives |
| `field_declaration` | Member variable declarations |
| `parameter_declaration` | Function parameter declarations |
| `comment` | Single-line (`//`) and block (`/* */`) comments |

**Examples**

```sql
-- All symbols in the index
FIND symbols

-- Only function definitions
FIND symbols
  WHERE node_kind = 'function_definition'

-- Functions whose name starts with "get"
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE name LIKE 'get%'

-- All #define macros in headers
FIND symbols
  WHERE node_kind = 'preproc_def'
  IN 'include/**'

-- Top 10 most-referenced functions
FIND symbols
  WHERE node_kind = 'function_definition'
  ORDER BY usages DESC
  LIMIT 10

-- Functions with a void return type (dynamic field)
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE type LIKE 'void%'

-- Effective substitute for FIND globals
FIND symbols
  WHERE node_kind = 'declaration'
  ORDER BY usages DESC
  LIMIT 20
```

---

### `FIND usages OF`

Return every identifier reference to the named symbol.

```sql
FIND usages OF 'symbol_name' [clauses]
```

**Examples**

```sql
-- All references to PiscoCode::process
FIND usages OF 'PiscoCode::process'

-- Usage count per file (replaces a dedicated COUNT command)
FIND usages OF 'PiscoCode::process'
  GROUP BY file
  ORDER BY count DESC

-- References inside src/ only
FIND usages OF 'PiscoCode::process'
  IN 'src/**'
```

---

### `FIND callees OF`

Syntactic alias for `SHOW callees OF`. Returns all symbols directly called or referenced from inside the named function's body. The parser routes this to the same handler as `SHOW callees OF`.

```sql
FIND callees OF 'symbol_name' [clauses]
```

**Examples**

```sql
-- What does PiscoCode::process call?
FIND callees OF 'PiscoCode::process'

-- Callees restricted to a subtree
FIND callees OF 'PiscoCode::process'
  IN 'src/**'
  ORDER BY name ASC
```

---

### `FIND files`

Return files in the worktree, optionally filtered by path glob or tree depth.
All universal clauses — including `WHERE`, `ORDER BY`, `LIMIT`, and `OFFSET` —
are fully supported.

```sql
FIND files [clauses]
```

**Filterable fields**

| Field | Type | Description |
|---|---|---|
| `path` | string | Relative file path (also used by `IN` / `EXCLUDE` globs) |
| `extension` | string | File extension without the leading `.` — empty string for extension-less files |
| `size` | integer | File size in bytes |
| `depth` | integer | Directory depth relative to the workspace root |

**Examples**

```sql
-- All files
FIND files

-- Only files under src/
FIND files
  IN 'src/**'

-- Top-level directory tree, 2 levels deep
FIND files DEPTH 2

-- Files with the most symbol definitions
FIND files
  ORDER BY count DESC
  LIMIT 20

-- Find all non-C/C++ files (e.g. CMake, Markdown, config)
FIND files IN 'src/**'
  WHERE extension NOT LIKE 'cpp'
  WHERE extension NOT LIKE 'c'
  WHERE extension NOT LIKE 'h'
  WHERE extension NOT LIKE 'hpp'

-- Filter by path pattern instead
FIND files
  WHERE path NOT LIKE '%.cpp'
  WHERE path NOT LIKE '%.h'

-- Find unusually large files
FIND files
  WHERE size > 100000
  ORDER BY size DESC
  LIMIT 10

-- Only markdown files, sorted by path
FIND files
  WHERE extension = 'md'
  ORDER BY path ASC
```

---

## Content Commands — SHOW

`SHOW` commands read source content and return structured output. Every `SHOW` response includes `start_line` and `end_line` so the agent can chain directly into a `CHANGE LINES` command without re-reading the file.

---

### `SHOW body OF`

Return the full source text of a named symbol's body.

```sql
SHOW body OF 'symbol_name' [clauses]
```

| Clause | Effect |
|---|---|
| `DEPTH N` | Collapse nested blocks at depth > N (shows `{ ... }` placeholders) |

**Default:** `DEPTH 0` — returns only the signature with the entire body replaced by `{ ... }`. Use `DEPTH 1` or higher to reveal progressively more nested structure.

**Examples**

```sql
SHOW body OF 'PiscoCode::process'         -- signature only (DEPTH 0)
SHOW body OF 'PiscoCode::init' DEPTH 1   -- top-level body visible
SHOW body OF 'PiscoCode::init' DEPTH 99  -- full source
```

---

### `SHOW signature OF`

Return only the declaration (return type, name, parameters) without the body.

```sql
SHOW signature OF 'symbol_name' [clauses]
```

**Example**

```sql
SHOW signature OF 'PiscoCode::process'
```

---

### `SHOW outline OF`

Return the structural outline of a file: top-level symbols with their line numbers, without bodies.

```sql
SHOW outline OF 'file_path' [clauses]
```

**Example**

```sql
SHOW outline OF 'include/PiscoCode.h'
```

---

### `SHOW members OF`

Return all member declarations (fields and methods) of a class or struct.

```sql
SHOW members OF 'type_name' [clauses]
```

**Example**

```sql
SHOW members OF 'PiscoCode'
```

---

### `SHOW context OF`

Return the surrounding lines of a symbol's definition — useful for understanding the declaration environment without reading the full file.

```sql
SHOW context OF 'symbol_name' [clauses]
```

**Example**

```sql
SHOW context OF 'PISCO_BUFFER_SIZE'
```

---

### `SHOW callees OF`

Return all symbols directly called or referenced from inside the named function's body. This is the same handler as `FIND callees OF` — both syntaxes work.

```sql
SHOW callees OF 'symbol_name' [clauses]
```

> **Template function limitation** — C++ template functions (e.g. `template<typename T> void foo()`) are not resolved by the call-graph walker and will return an empty result. Use `FIND usages OF 'symbol_name'` instead to locate all reference sites:
>
> ```sql
> -- Workaround for template functions
> FIND usages OF 'MyTemplate::process'
> ```

**Examples**

```sql
SHOW callees OF 'PiscoCode::process'

SHOW callees OF 'PiscoCode::process'
  IN 'src/**'
  ORDER BY name ASC
```

---

### `SHOW LINES`

Return a specific line range from a file verbatim.

```sql
SHOW LINES n-m OF 'file_path' [clauses]
```

**Examples**

```sql
SHOW LINES 87-103 OF 'src/PiscoCode.cpp'
SHOW LINES 1-30 OF 'include/PiscoCode.h'
```

---

## Mutation Commands — CHANGE

`CHANGE` commands modify source files. They are most effective inside a `BEGIN TRANSACTION` block, which provides automatic rollback on failure.

```sql
CHANGE (FILE | FILES) file_list change_target [clauses]
```

`FILE` and `FILES` are interchangeable. `file_list` is one or more single-quoted glob patterns separated by commas.

---

### `MATCHING … WITH …`

Replace all occurrences of a literal string with another string across the specified files.

```sql
CHANGE FILES 'glob', 'glob' MATCHING 'old_text' WITH 'new_text'
```

**Examples**

```sql
-- Rename a symbol across all C++ translation units
CHANGE FILES 'src/**/*.cpp', 'include/**/*.h'
  MATCHING 'PiscoCode::process' WITH 'PiscoCode::run'

-- Update a macro value in a single header
CHANGE FILE 'include/config.h'
  MATCHING 'PISCO_VERSION "1.3.0"' WITH 'PISCO_VERSION "1.4.0"'
```

---

### `LINES n-m WITH …`

Replace a specific line range with new content. Typically chained after a `SHOW body` or `SHOW LINES` response that provided the exact line numbers.

```sql
CHANGE FILE 'file_path' LINES n-m WITH 'new_content'
```

> **Escape sequences** — the content string is interpreted literally. Backslash sequences (`\n`, `\t`, etc.) are **not** expanded. Use actual newlines and tabs inside the string literal.

**Examples**

```sql
-- Rewrite a function body (line range from SHOW body)
CHANGE FILE 'src/PiscoCode.cpp'
  LINES 87-103
  WITH 'void PiscoCode::run(Buffer& buffer) {
    for (auto& sample : buffer) {
        sample = this->pipeline.apply(sample);
    }
}'

-- Fix a header guard
CHANGE FILE 'include/PiscoCode.h'
  LINES 1-3
  WITH '#pragma once'
```

---

### `WITH 'content'`

Replace the entire content of the specified file with a new string. If the file does not exist, it is created.

```sql
CHANGE FILE 'file_path' WITH 'new_full_content'
```

**Example**

```sql
CHANGE FILE 'src/generated/version.h'
  WITH '#pragma once
#define PISCO_VERSION "1.4.0"
#define PISCO_BUILD_DATE "2026-03-17"
'
```

---

### `LINES n-m WITH NOTHING`

Delete a specific line range from a file.

```sql
CHANGE FILE 'file_path' LINES n-m WITH NOTHING
```

**Example**

```sql
-- Remove a deprecated function body (line range from SHOW body)
CHANGE FILE 'src/PiscoCode.cpp'
  LINES 200-214
  WITH NOTHING
```

---

### `WITH NOTHING`

Clear the entire content of a file (the file remains on disk but is emptied).

```sql
CHANGE FILE 'file_path' WITH NOTHING
```

**Example**

```sql
-- Clear a generated file entirely
CHANGE FILE 'src/generated/stale_output.cpp' WITH NOTHING
```

---

## Transaction Commands

Transactions group multiple commands atomically. If any step fails (including build verification), all file changes are rolled back automatically.

---

### `BEGIN TRANSACTION … COMMIT MESSAGE`

```sql
BEGIN TRANSACTION 'name'
  statement
  [statement ...]
  [VERIFY build 'target']
COMMIT MESSAGE 'message'
```

| Part | Description |
|---|---|
| `'name'` | Transaction identifier used in logs and rollback messages |
| `VERIFY build 'target'` | Run a build target defined in `.forgeql.yaml`; the transaction aborts and rolls back if it fails |
| `COMMIT MESSAGE` | Descriptive message written to the log (and optionally to a git commit) |

Note: `VERIFY` requires the `build` keyword and accepts a single target name. The target must be defined in the project's `.forgeql.yaml` under `verify_steps`. Place the file in the repository root (ForgeQL walks up from the working directory to find it).

**`.forgeql.yaml` example**

```yaml
# Path to the source tree root, relative to this file.
workspace_root: .

# Named build/test steps referenced by VERIFY build '<name>'.
verify_steps:
  - name: test
    command: "cmake --build build && ctest --test-dir build"
    timeout_secs: 120
  - name: release
    command: "./scripts/Build.sh release"
    timeout_secs: 300

# Additional glob patterns to exclude from indexing.
ignore_patterns:
  - "build/**"
  - "third_party/**"
```

**Examples**

```sql
-- Safe symbol rename with build verification
BEGIN TRANSACTION 'rename-process'
  CHANGE FILES 'src/**/*.cpp', 'include/**/*.h'
    MATCHING 'PiscoCode::process' WITH 'PiscoCode::run'
  VERIFY build 'test'
COMMIT MESSAGE 'rename PiscoCode::process to PiscoCode::run'

-- Multi-step refactor: bump version constant in two files
BEGIN TRANSACTION 'bump-version'
  CHANGE FILE 'include/config.h'
    MATCHING 'PISCO_VERSION "1.3.0"' WITH 'PISCO_VERSION "1.4.0"'
  CHANGE FILE 'src/generated/version.h'
    MATCHING '1.3.0' WITH '1.4.0'
  VERIFY build 'test'
COMMIT MESSAGE 'bump version to 1.4.0'

-- Remove a deprecated helper after verifying nothing breaks
BEGIN TRANSACTION 'remove-legacyHelper'
  CHANGE FILE 'src/PiscoCode.cpp'
    LINES 200-214
    WITH NOTHING
  VERIFY build 'test'
COMMIT MESSAGE 'remove deprecated legacyHelper'
```

---

### `VERIFY build` (standalone)

`VERIFY build` can also be used as a **top-level statement** — outside any
transaction — to run a named step from `.forgeql.yaml` on demand.

**Syntax**

```sql
VERIFY build 'step'
```

| Part | Description |
|---|---|
| `'step'` | Name of a `verify_steps` entry in the project's `.forgeql.yaml` |

The command runs the step's shell command in the worktree directory (or the
data directory when no session is active) and returns a `VerifyBuildResult`
with `step`, `success`, and `output` fields.

**Example**

```sql
-- Check that all unit tests pass right now, without modifying anything
VERIFY build 'test'
```

Result (JSON in MCP mode):

```json
{
  "step": "test",
  "success": true,
  "output": "All 257 tests passed."
}
```

---

### `ROLLBACK`

Restore the session to the state before the last applied transaction. Optionally specify a transaction name.

```sql
ROLLBACK [TRANSACTION 'name']
```

**Examples**

```sql
-- Roll back the most recent transaction
ROLLBACK

-- Roll back a specific named transaction
ROLLBACK TRANSACTION 'rename-process'
```

---

## Universal Clauses

Every command accepts the following clauses. Multiple clauses can be freely combined. The engine always applies them in this fixed pipeline order regardless of how they appear in the query:

```
IN → EXCLUDE → WHERE → GROUP BY → HAVING → ORDER BY → OFFSET → LIMIT
```

```sql
[WHERE field operator value]
[HAVING field operator value]
[IN 'glob']
[EXCLUDE 'glob']
[ORDER BY field [ASC | DESC]]
[GROUP BY (file | kind | node_kind)]
[LIMIT N]
[OFFSET N]
[DEPTH N]
```

---

### `WHERE`

Filter rows before returning results. Multiple `WHERE` clauses are combined with implicit AND.

```sql
WHERE name LIKE 'get%'
WHERE node_kind = 'function_definition'
WHERE line >= 100
WHERE usages = 0
WHERE type LIKE 'void%'
```

---

### `HAVING`

Filter rows after `GROUP BY` aggregation. Operates on computed fields like `count`.

```sql
FIND usages OF 'PiscoCode::process'
  GROUP BY file
  HAVING count >= 3
```

---

### `IN`

Restrict the result set to rows whose file path matches the glob.

```sql
FIND symbols WHERE node_kind = 'preproc_include'
  IN 'src/**'
```

---

### `EXCLUDE`

Remove rows whose file path matches the glob.

```sql
FIND symbols WHERE usages = 0
  IN 'src/**'
  EXCLUDE 'src/tests/**'
```

---

### `ORDER BY`

Sort the result set. Default direction is `ASC`.

```sql
ORDER BY name ASC
ORDER BY usages DESC
ORDER BY line
```

---

### `GROUP BY`

Aggregate results by a grouping field. Each output row represents one group and gains a `count` field.

Supported grouping fields: `file`, `kind`, `node_kind`.

```sql
FIND usages OF 'PiscoCode::process'
  GROUP BY file
  ORDER BY count DESC
```

---

### `LIMIT` / `OFFSET`

Paginate large result sets.

```sql
FIND symbols WHERE node_kind = 'function_definition'
  ORDER BY name ASC
  LIMIT 20
  OFFSET 40
```

---

### `DEPTH`

For `SHOW body` and `FIND files`, collapse or restrict tree depth. `DEPTH 0` shows only the signature with the body replaced by `{ ... }`.

```sql
SHOW body OF 'PiscoCode::process' DEPTH 1
FIND files IN 'src/**' DEPTH 2
```

---

## Operators and Values

### Comparison Operators

| Operator | Meaning |
|---|---|
| `=` | Exact equality |
| `!=` | Not equal |
| `LIKE` | SQL-style wildcard: `%` = any sequence, `_` = any single character |
| `NOT LIKE` | Negated LIKE |
| `>` | Greater than (numeric fields) |
| `>=` | Greater than or equal |
| `<` | Less than |
| `<=` | Less than or equal |

### Value Types

| Syntax | Type | Example |
|---|---|---|
| `'text'` | String | `WHERE name LIKE 'get%'` |
| `42` | Integer | `WHERE usages >= 5` |
| `-10` | Signed integer | `WHERE line >= -1` |
| `true` / `false` | Boolean | *(reserved for future use)* |

---

## Filterable Fields

### Symbol results (`FIND symbols`, `FIND usages OF`, `FIND callees OF`)

| Field | Type | Description |
|---|---|---|
| `name` | string | Symbol name |
| `node_kind` | string | Raw tree-sitter node kind (e.g. `function_definition`) |
| `path` | string | Relative file path — also used by `IN` / `EXCLUDE` globs |
| `line` | integer | 1-based start line of the node |
| `usages` | integer | Number of identifier references to this symbol in the index |

### File results (`FIND files`)

| Field | Type | Description |
|---|---|---|
| `path` | string | Relative file path — also used by `IN` / `EXCLUDE` globs |
| `extension` | string | File extension without the leading `.` (empty string for extension-less files) |
| `size` | integer | File size in bytes |
| `depth` | integer | Directory depth relative to the workspace root |

In addition every row carries **dynamic fields** auto-extracted from the tree-sitter grammar. You can filter on any of them without recompiling ForgeQL:

| Field | Availability | Description |
|---|---|---|
| `type` | C/C++ | Return type text |
| `value` | C/C++ | Initial value (`preproc_def`, `init_declarator`) — always stored as text; numeric comparisons (`>=`, `<=`, `>`, `<`) require the stored text to be a plain integer literal |
| `declarator` | C/C++ | Full declarator including pointer/reference qualifiers |
| `parameters` | C/C++ | Parameter list text |
| `body` | C/C++ | Body text — large; prefer `SHOW body` |

If a field does not exist on a row, a `WHERE` predicate on that field evaluates to false and the row is excluded — identical to SQL `NULL` semantics.

**Numeric operator coercion** — dynamic fields are stored as strings. When you write `WHERE value >= 1000`, ForgeQL attempts to parse the stored text as an integer. If parsing fails (e.g. the value is `"some_constant"` or `"0x1F"`), the predicate silently evaluates to false for that row. Use `LIKE` patterns for non-decimal values.

---

## Use Cases: Pisco Code v1.3.0

The examples below assume:

```sql
CREATE SOURCE 'pisco' FROM 'https://github.com/pisco-de-luz/Pisco-Code.git'
USE pisco.v1.3.0
```

---

### Explore the codebase structure

```sql
-- Top-level directory tree
FIND files DEPTH 2

-- Structural outline of the main header
SHOW outline OF 'include/PiscoCode.h'

-- All classes defined in the library
FIND symbols
  WHERE node_kind = 'class_specifier'
  ORDER BY name ASC
```

---

### Find and inspect functions

```sql
-- All getter/setter methods
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE name LIKE 'get%'
  ORDER BY name ASC

-- Functions with more than 5 callers (high-impact symbols)
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE usages >= 5
  ORDER BY usages DESC

-- Full body of a function
SHOW body OF 'PiscoCode::process'

-- Just the signature without the body
SHOW signature OF 'PiscoCode::process'

-- Everything PiscoCode::process calls
SHOW callees OF 'PiscoCode::process'
```

---

### Audit and dead code detection

```sql
-- Macros that are never referenced
FIND symbols
  WHERE node_kind = 'preproc_def'
  WHERE usages = 0
  IN 'include/**'

-- Functions never called anywhere
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE usages = 0
  EXCLUDE 'src/tests/**'
  ORDER BY path ASC

-- Usage heat-map for a given symbol
FIND usages OF 'PiscoCode::process'
  GROUP BY file
  ORDER BY count DESC

-- Files that define more than 20 symbols (candidates for splitting)
FIND symbols
  GROUP BY file
  HAVING count >= 20
  ORDER BY count DESC
```

---

### Safe refactoring

```sql
-- Rename a symbol with build verification
BEGIN TRANSACTION 'rename-process'
  CHANGE FILES 'src/**/*.cpp', 'include/**/*.h'
    MATCHING 'PiscoCode::process' WITH 'PiscoCode::run'
  VERIFY build 'test'
COMMIT MESSAGE 'rename PiscoCode::process to PiscoCode::run'

-- Update a configuration constant and a generated header atomically
BEGIN TRANSACTION 'bump-version'
  CHANGE FILE 'include/config.h'
    MATCHING 'PISCO_VERSION "1.3.0"' WITH 'PISCO_VERSION "1.4.0"'
  CHANGE FILE 'src/generated/version.h'
    MATCHING '1.3.0' WITH '1.4.0'
  VERIFY build 'test'
COMMIT MESSAGE 'bump version to 1.4.0'

-- Replace a function body using line range from SHOW body
-- (assumes SHOW body OF 'PiscoCode::process' returned start_line=87, end_line=103)
BEGIN TRANSACTION 'rewrite-process'
  CHANGE FILE 'src/PiscoCode.cpp'
    LINES 87-103
    WITH 'void PiscoCode::run(Buffer& buffer) {
    for (auto& sample : buffer) {
        sample = this->pipeline.apply(sample);
    }
}'
  VERIFY build 'test'
COMMIT MESSAGE 'rewrite PiscoCode::run with pipeline approach'

-- Remove a deprecated helper function
-- (line range from SHOW body OF 'legacyHelper')
BEGIN TRANSACTION 'remove-legacyHelper'
  CHANGE FILE 'src/PiscoCode.cpp'
    LINES 200-214
    WITH NOTHING
  VERIFY build 'test'
COMMIT MESSAGE 'remove deprecated legacyHelper'
```

---

### Pagination for large codebases

```sql
-- Browse all function definitions 20 at a time
FIND symbols
  WHERE node_kind = 'function_definition'
  ORDER BY path ASC
  LIMIT 20
  OFFSET 0   -- page 1

FIND symbols
  WHERE node_kind = 'function_definition'
  ORDER BY path ASC
  LIMIT 20
  OFFSET 20  -- page 2
```

---

### Dynamic field filtering

Dynamic fields are extracted automatically from tree-sitter grammar nodes and are queryable without any code changes to ForgeQL.

```sql
-- All void-returning functions
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE type LIKE 'void%'

-- Macros whose value is a plain integer above 1000
-- (non-decimal or symbolic values are silently skipped)
FIND symbols
  WHERE node_kind = 'preproc_def'
  WHERE value >= 1000

-- System includes
-- Note: Angle brackets (<...>) are not preserved in the index.
-- Match by name pattern instead (e.g., 'std%' for standard library)
FIND symbols
  WHERE node_kind = 'preproc_include'
  WHERE name LIKE 'std%'
```

### Prefer CSV output for AI agents

When querying inside an AI agent context (MCP mode), request CSV format to reduce token usage significantly. JSON is the default for programmatic consumption; plain text is the default for interactive use.

```sql
-- Each row is a compact comma-separated line instead of a JSON object
FIND symbols
  WHERE node_kind = 'function_definition'
  ORDER BY usages DESC
  LIMIT 50
  FORMAT CSV
```

---

## Known Limitations

| Area | Description | Workaround |
|---|---|---|
| Template functions | `SHOW callees OF` / `FIND callees OF` returns empty for C++ template functions | Use `FIND usages OF 'name'` to find all reference sites |
| Numeric coercion | `value >= N` silently skips rows where `value` is non-decimal (hex, symbolic constants) | Use `WHERE value LIKE 'pattern'` for non-integer values |
| Escape sequences | `CHANGE … WITH 'text'` interprets content literally — `\n` is two characters, not a newline | Write actual newlines inside the string literal |
