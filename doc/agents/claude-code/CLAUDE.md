# ForgeQL Instructions for Claude Code

All source code is accessed exclusively through the ForgeQL MCP server.
The local workspace may be empty — never fall back to local filesystem tools (Bash, grep, find, cat, Read File).

## Critical Rules

1. Always start with `USE source.branch AS 'alias'` before any query. The `AS` clause is mandatory.
2. Never use Bash tools (grep, find, cat, less) or Read File for source code. ForgeQL manages all code access.
3. Never brute-force read code. Use FIND to locate symbols, then SHOW LINES for exact ranges.
4. **SHOW body and SHOW context** without LIMIT are blocked beyond 40 lines. If blocked, use FIND to get file + line numbers, then SHOW LINES n-m. **SHOW LINES n-m always returns the full requested range** — explicit line ranges bypass the cap.
5. Stack WHERE clauses aggressively before executing. Multiple WHERE clauses combine as AND — filter first, read later.
6. Filter inside SHOW LINES — never read then grep. `SHOW LINES 1-400 OF 'file' WHERE text LIKE '%pattern%'` returns only matching lines.
7. Always ORDER BY in GROUP BY queries. Use `ORDER BY count ASC` to surface lowest-scope candidates first. Add HAVING constraints to filter at aggregate level.
8. Verify structural assumptions before mutating. Check includes and structure before refactoring, not after.
9. Numbers have no symbolic usages — use text search. Use `SHOW LINES WHERE text LIKE '%value%'` or `WHERE name = 'value'` for literal search.
10. Persist key findings in `HINTS.md`. After completing a task, append short bullet points of key codebase facts discovered (file locations, naming conventions, architectural decisions) to `HINTS.md` in the workspace root.

## Query Workflow

```
1. FIND symbols WHERE ... → get name, file, line number
2. SHOW LINES n-m OF 'file' → read only those lines
```

**Progressive disclosure for SHOW body:**
- `DEPTH 0` — signature + enrichment metadata row (default, cheapest)
- `DEPTH 1` — control-flow skeleton
- `DEPTH 99` — full source (add LIMIT)

## Query Strategy

| Need | Command |
|---|---|
| Find a symbol | `FIND symbols WHERE name LIKE 'pattern' [WHERE fql_kind = '...'] [IN 'path/**']` |
| Read specific lines | `SHOW LINES n-m OF 'file'` |
| Symbol signature | `SHOW body OF 'name' DEPTH 0` — also returns enrichment metadata |
| Qualified symbol | `SHOW body OF 'Class::method'` or `SHOW body OF 'Obj.method'` |
| Control flow overview | `SHOW body OF 'name' DEPTH 1` |
| Blast radius | `FIND usages OF 'name' GROUP BY file ORDER BY count DESC` |
| File structure | `SHOW outline OF 'file' [WHERE fql_kind = '...']` |
| Class members | `SHOW members OF 'type'` |
| Call graph | `SHOW callees OF 'name'` |
| File list | `FIND files [IN 'path/**'] [WHERE extension = '...'] ORDER BY size DESC` |
| Context around symbol | `SHOW context OF 'name'` |
| Read + filter lines | `SHOW LINES n-m OF 'file' WHERE text LIKE '%pattern%'` |
| Repo top-level dirs | `FIND files` (returns depth-1 entries) |
| Copy lines between files | `COPY LINES n-m OF 'src' TO 'dst' [AT LINE k]` |
| Move lines between files | `MOVE LINES n-m OF 'src' TO 'dst' [AT LINE k]` |

## Anti-Patterns

| Never do this | Do this instead |
|---|---|
| `SHOW body OF 'func' DEPTH 99` without LIMIT | `FIND symbols WHERE name = 'func'` → `SHOW LINES n-m OF 'file'` |
| `SHOW LINES 1-500 OF 'file'` | `SHOW outline OF 'file'` → `SHOW LINES n-m` for specific symbols |
| `FIND symbols` (unfiltered) | `FIND symbols WHERE fql_kind = '...' WHERE name LIKE '...'` |
| `GROUP BY` without `HAVING` or `ORDER BY` | `GROUP BY file HAVING count >= N ORDER BY count ASC` |
| `SHOW LINES n-m OF 'file'` then filter manually | `SHOW LINES n-m OF 'file' WHERE text LIKE '%pattern%'` |
| `FIND usages OF 'number'` for literal occurrence | `FIND symbols WHERE name = 'value' WHERE is_magic = 'true'` or `SHOW LINES WHERE text LIKE '%value%'` |

## Efficiency

- All commands accept `WHERE`, `GROUP BY`, `ORDER BY`, `LIMIT`, `OFFSET` — combine freely.
- `IN 'src'` and `IN 'crates/'` auto-expand to `IN 'src/**'` — bare directory paths are always safe.
- Multiple `WHERE` clauses combine as AND.
- FIND defaults to 20 rows without LIMIT.
- Format defaults to CSV (~60% fewer tokens). Use `format=JSON` only when parsing fields programmatically.
- Every response includes `tokens_approx` — if large, narrow with WHERE, IN, EXCLUDE, or lower LIMIT.
- For magic number exploration: `WHERE num_format = 'dec' WHERE num_value > X WHERE num_value < Y` narrows by semantic domain (timeouts, counts, ASCII ranges) — more surgical than blind GROUP BY.
- Plan multi-read SHOW operations in advance. If you need context around multiple lines in the same file, check whether one contiguous range covers all before issuing separate queries.

## Syntax Reference

### Session
```sql
USE source.branch AS 'alias'
SHOW SOURCES
SHOW BRANCHES
```

Sessions persist across server restarts. To reconnect or hand off to another agent, use
the same `USE` command — the worktree and uncommitted changes are preserved.
Worktrees idle for more than 48 hours are cleaned up automatically.

Worktree identity uses a composite key: filesystem = `branch.alias`, git branch =
`fql/branch/alias`. The `fql/` namespace avoids git loose-ref collisions.

**Line budget:** if configured, each session tracks consumed source lines. Budget
status is returned in every response. When budget is low, use tighter `WHERE` filters,
`LIMIT`, and `DEPTH 0`/`1` to conserve lines.

### FIND
```sql
FIND symbols [clauses]
FIND globals [clauses]
FIND usages OF 'name' [clauses]
FIND callees OF 'name' [clauses]
FIND files [clauses]
```

### SHOW
```sql
SHOW body OF 'name' [DEPTH N] [clauses]
SHOW signature OF 'name' [clauses]
SHOW outline OF 'file' [clauses]
SHOW members OF 'type' [clauses]
SHOW context OF 'name' [clauses]
SHOW callees OF 'name' [clauses]
SHOW LINES n-m OF 'file' [clauses]
```

### CHANGE & Transactions
```sql
CHANGE FILE 'path' LINES n-m WITH 'text'
CHANGE FILE 'path' LINES n-m WITH NOTHING
CHANGE FILES 'glob1','glob2' MATCHING 'old' WITH 'new'
CHANGE FILE 'path' WITH 'full_content'

COPY LINES n-m OF 'src' TO 'dst' [AT LINE k]
MOVE LINES n-m OF 'src' TO 'dst' [AT LINE k]

BEGIN TRANSACTION 'name'
  -- CHANGE / COPY / MOVE / VERIFY commands
COMMIT MESSAGE 'msg'
VERIFY build 'step'
ROLLBACK [TRANSACTION 'name']
```

### Universal Clauses (applied in this order)
`IN → EXCLUDE → WHERE → GROUP BY → HAVING → ORDER BY → OFFSET → LIMIT`

**Operators:** `=`, `!=`, `LIKE`, `NOT LIKE`, `MATCHES`, `NOT MATCHES` (regex), `>`, `>=`, `<`, `<=`

`MATCHES` / `NOT MATCHES` use Rust `regex` crate syntax. Prefix `(?i)` for case-insensitive matching.

`SHOW body`, `SHOW LINES`, and `SHOW context` accept `WHERE` on source lines:
- `text` — line content (supports `=`, `LIKE`, `MATCHES`)
- `line` — 1-based line number

`SHOW callees` accepts `WHERE` on call graph entries:
- `name` — called symbol name
- `path` / `file` — file containing the call
- `line` — line number of the call

Source line filtering runs **before** the 40-line cap.

## fql_kind Values

**Always use `fql_kind` in WHERE clauses.** `fql_kind` is language-agnostic and works identically across C++, Rust, and any future language. Raw `node_kind` values (tree-sitter grammar names) are language-specific and **deprecated**.

| `fql_kind` | Matches |
|---|---|
| `function` | Function/method definitions |
| `class` | Class declarations |
| `struct` | Struct declarations |
| `enum` | Enum declarations |
| `variable` | Variable and parameter declarations |
| `field` | Class/struct member declarations |
| `comment` | Comments |
| `import` | Import / include directives |
| `macro` | Preprocessor macro definitions |
| `type_alias` | Type alias / typedef declarations |
| `namespace` | Namespace definitions |
| `number` | Numeric literals |
| `cast` | Cast expressions |
| `increment` | Increment/decrement expressions |
| `if` | if statements |
| `while` | while loops |
| `for` | for loops (traditional and range-based) |
| `switch` | switch statements |
| `do` | do-while loops |

## Enrichment Fields

Computed at index time. Use in `WHERE` clauses like any other field.

> **`Applies to`** uses `fql_kind` values.

### Naming
| Field | Applies to | Values / Notes |
|---|---|---|
| `naming` | all named symbols | `camelCase`, `PascalCase`, `snake_case`, `UPPER_SNAKE`, `flatcase`, `other` |
| `name_length` | all named symbols | Character count |

### Comments
| Field | Applies to | Values / Notes |
|---|---|---|
| `comment_style` | `comment` | `doc_line` (`///`), `doc_block` (`/** */`), `block` (`/* */`), `line` (`//`) |
| `has_doc` | `function` | `"true"` if preceded by a doc comment |

### Numbers
| Field | Applies to | Values / Notes |
|---|---|---|
| `num_format` | `number` | `dec`, `hex`, `bin`, `oct`, `float`, `scientific` |
| `is_magic` | `number` | `"true"` for unexplained constants (not 0, 1, -1, 2, powers of 2, bitmasks) |
| `num_suffix` | `number` | `u`, `l`, `ll`, `ul`, `ull`, `f`, `ld` |
| `suffix_meaning` | `number` | `unsigned`, `long`, `float`, etc. |
| `has_separator` | `number` | `"true"` if contains digit separators |
| `num_value` | `number` | Raw text of the literal |

### Control Flow
| Field | Applies to | Values / Notes |
|---|---|---|
| `condition_tests` | `if`, `while`, `for`, `do` | Count of boolean sub-expressions |
| `paren_depth` | `if`, `while`, `for`, `do` | Max parentheses nesting |
| `condition_text` | `if`, `while`, `for`, `do` | Raw condition expression text |
| `has_catch_all` | `switch` | `"true"` if has a catch-all case |
| `catch_all_kind` | `switch` | e.g. `"default"` |
| `for_style` | `for` | `"traditional"` or `"range"` |
| `has_assignment_in_condition` | `if`, `while`, `for` | `"true"` if condition contains `=` (not `==`) |
| `mixed_logic` | `if`, `while`, `for` | `"true"` if mixes `&&` and `||` without grouping |
| `dup_logic` | `if`, `while`, `for`, `do` | `"true"` if duplicate sub-expressions in `&&`/`||` chains |
| `branch_count` | `function` | Total control-flow branch points |

### Operators
| Field | Applies to | Values / Notes |
|---|---|---|
| `increment_style` | `increment` | `"prefix"` or `"postfix"` |
| `increment_op` | `increment` | `"++"` or `"--"` |
| `compound_op` | `compound_assignment` | `+=`, `-=`, `*=`, `/=`, `%=`, `&=`, `|=`, `^=`, `<<=`, `>>=` |
| `operand` | `compound_assignment` | Left-hand side text |
| `shift_direction` | `shift_expression` | `"left"` or `"right"` |
| `shift_amount` | `shift_expression` | Right-hand operand text |
| `operator_category` | `increment`, `compound_assignment`, `shift_expression` | `"increment"`, `"arithmetic"`, `"bitwise"`, `"shift"` |

### Metrics
| Field | Applies to | Values / Notes |
|---|---|---|
| `lines` | `function`, `struct`, `class`, `enum` | Line span |
| `param_count` | `function` | Parameter count |
| `return_count` | `function` | `return` statement count |
| `goto_count` | `function` | `goto` statement count |
| `string_count` | `function` | String literal count |
| `throw_count` | `function` | `throw` statement count |
| `member_count` | `struct`, `class`, `enum` | Member/enumerator count |
| `is_const` | `function`, `variable` | `"true"` if `const` |
| `is_volatile` | `function`, `variable` | `"true"` if `volatile` |
| `is_static` | `function` | `"true"` if `static` |
| `is_inline` | `function` | `"true"` if `inline` |
| `is_override` | `function` | `"true"` if `override` |
| `is_final` | `function` | `"true"` if `final` |
| `visibility` | `field` (class members) | `"public"`, `"private"`, `"protected"` |

### Casts
| Field | Applies to | Values / Notes |
|---|---|---|
| `cast_style` | `cast` | `"c_style"` |
| `cast_target_type` | `cast` | Target type text |
| `cast_safety` | `cast` | `"safe"`, `"moderate"`, `"unsafe"` |

### Redundancy
| Field | Applies to | Values / Notes |
|---|---|---|
| `has_repeated_condition_calls` | `function` | `"true"` if same call in 2+ conditions |
| `repeated_condition_calls` | `function` | Comma-separated function names |
| `null_check_count` | `function` | Count of null-check patterns |
| `duplicate_condition` | `if`, `while`, `for`, `do` | `"true"` if same condition skeleton exists elsewhere in function |

### Scope
| Field | Applies to | Values / Notes |
|---|---|---|
| `scope` | `variable` | `"file"` (top-level) or `"local"` (inside function/block) |
| `storage` | `variable` | `"static"`, `"extern"`, or absent |
| `binding_kind` | `variable` | `"function"` or `"variable"` |
| `is_exported` | `variable` | `"true"` for file-scope declarations without `static` |

### Members
| Field | Applies to | Values / Notes |
|---|---|---|
| `body_symbol` | `field` (methods) | Qualified name linking to out-of-line definition (e.g. `Class::method`) |
| `member_kind` | `field` | `"method"` or `"field"` |
| `owner_kind` | `field` | Enclosing type kind (e.g. `class`, `struct`) |
| `enclosing_type` | `function` | Name of the enclosing class/struct/impl block. Enables qualified name resolution: `SHOW body OF 'Class::method'` |

### Declaration Distance

Data-flow enricher measuring distance between local variable declarations and their first use. Excludes parameters, globals, and member variables.

| Field | Applies to | Values / Notes |
|---|---|---|
| `decl_distance` | `function` | Sum of (first-use line − declaration line) for locals with distance ≥ 2 |
| `decl_far_count` | `function` | Count of locals whose first-use is ≥ 2 lines after declaration |
| `has_unused_reassign` | `function` | `"true"` when a local is reassigned before its previous value was read (dead store) |

### Escape Analysis

Detects local variables that escape their declaring function — via return, address-of, or pointer/array aliasing.

| Field | Applies to | Values / Notes |
|---|---|---|
| `has_escape` | `function` | `"true"` if any local escapes |
| `escape_count` | `function` | Number of distinct escaping locals |
| `escape_vars` | `function` | Comma-separated names of escaping locals |
| `escape_tier` | `function` | Severity: `1` (return), `2` (address-of), `3` (pointer/array alias) |
| `escape_kinds` | `function` | Comma-separated escape mechanisms (e.g. `"return,address_of"`) |

### Shadow Detection

| Field | Applies to | Values / Notes |
|---|---|---|
| `has_shadow` | `function` | `"true"` if any inner variable shadows an outer one |
| `shadow_count` | `function` | Number of shadowing declarations |
| `shadow_vars` | `function` | Comma-separated shadowed variable names |

### Unused Parameters

| Field | Applies to | Values / Notes |
|---|---|---|
| `has_unused_param` | `function` | `"true"` if any parameter is unused |
| `unused_param_count` | `function` | Number of unused parameters |
| `unused_params` | `function` | Comma-separated names of unused parameters |

### Fallthrough Detection

| Field | Applies to | Values / Notes |
|---|---|---|
| `has_fallthrough` | `function` | `"true"` if any non-empty case falls through |
| `fallthrough_count` | `function` | Number of fallthrough cases |

### Recursion Detection

| Field | Applies to | Values / Notes |
|---|---|---|
| `is_recursive` | `function` | `"true"` if the function calls itself |
| `recursion_count` | `function` | Number of self-call sites |

### Todo Markers

| Field | Applies to | Values / Notes |
|---|---|---|
| `has_todo` | `function` | `"true"` if any TODO/FIXME/HACK/XXX marker found |
| `todo_count` | `function` | Total marker occurrences |
| `todo_tags` | `function` | Sorted unique tags (e.g. `"FIXME,TODO"`) |

## Common Recipes

### Dead Code Detection
```sql
FIND symbols WHERE fql_kind = 'function' WHERE usages = 0 EXCLUDE 'tests/**' ORDER BY path ASC LIMIT 30
```

### Rename / Refactor
```sql
FIND symbols WHERE name = 'oldFunction'
FIND usages OF 'oldFunction' GROUP BY file ORDER BY count DESC
BEGIN TRANSACTION 'rename-oldFunction'
  CHANGE FILES 'src/**/*.cpp','include/**/*.h' MATCHING 'oldFunction' WITH 'newFunction'
  VERIFY build 'test'
COMMIT MESSAGE 'refactor: rename oldFunction → newFunction'
```

### Code Smell Audit
```sql
-- Unexplained constants (magic numbers)
FIND symbols WHERE fql_kind = 'number' WHERE is_magic = 'true' ORDER BY path ASC LIMIT 30

-- Assignment inside a condition (likely bug: = instead of ==)
FIND symbols WHERE has_assignment_in_condition = 'true'

-- Mixed && / || without grouping parentheses
FIND symbols WHERE mixed_logic = 'true'

-- Duplicate sub-expressions in a single condition (copy-paste bug)
FIND symbols WHERE dup_logic = 'true'

-- Same condition repeated in multiple branches of the same function
FIND symbols WHERE duplicate_condition = 'true'

-- Same function called in 2+ conditions (extract-variable opportunity)
FIND symbols WHERE has_repeated_condition_calls = 'true'

-- switch without a catch-all / default case
FIND symbols WHERE fql_kind = 'switch' WHERE has_catch_all = 'false'

-- Large functions (refactoring candidates)
FIND symbols WHERE fql_kind = 'function' WHERE lines >= 50 ORDER BY lines DESC LIMIT 20

-- Functions with many parameters (high coupling)
FIND symbols WHERE fql_kind = 'function' WHERE param_count >= 5 ORDER BY param_count DESC LIMIT 20

-- Undocumented public functions
FIND symbols WHERE fql_kind = 'function' WHERE has_doc = 'false' IN 'src/**' LIMIT 30

-- C-style casts (modernization targets)
FIND symbols WHERE fql_kind = 'cast' WHERE cast_style = 'c_style'

-- Unsafe casts
FIND symbols WHERE fql_kind = 'cast' WHERE cast_safety = 'unsafe'

-- Functions with goto
FIND symbols WHERE fql_kind = 'function' WHERE goto_count >= 1

-- Multiple return paths (complexity indicator)
FIND symbols WHERE fql_kind = 'function' WHERE return_count >= 3 ORDER BY return_count DESC LIMIT 20

-- Complex conditions (4+ boolean sub-expressions)
FIND symbols WHERE condition_tests >= 4 ORDER BY condition_tests DESC LIMIT 20

-- Variables declared far from first use (move declarations closer)
FIND symbols WHERE fql_kind = 'function' WHERE decl_far_count >= 3 ORDER BY decl_distance DESC LIMIT 20

-- Dead stores (value overwritten before being read)
FIND symbols WHERE fql_kind = 'function' WHERE has_unused_reassign = 'true'

-- Functions matching a name pattern (regex)
FIND symbols WHERE fql_kind = 'function' WHERE name MATCHES '_impl$' ORDER BY usages DESC

-- Functions with TODO/FIXME/HACK/XXX markers
FIND symbols WHERE fql_kind = 'function' WHERE has_todo = 'true' ORDER BY todo_count DESC LIMIT 20

-- Directly recursive functions
FIND symbols WHERE fql_kind = 'function' WHERE is_recursive = 'true' ORDER BY recursion_count DESC LIMIT 20

-- Local variables escaping their function (return, address-of, aliasing)
FIND symbols WHERE fql_kind = 'function' WHERE has_escape = 'true' ORDER BY escape_count DESC LIMIT 20

-- Variable shadowing in nested scopes
FIND symbols WHERE fql_kind = 'function' WHERE has_shadow = 'true' ORDER BY shadow_count DESC LIMIT 20

-- Unused function parameters
FIND symbols WHERE fql_kind = 'function' WHERE has_unused_param = 'true' ORDER BY unused_param_count DESC LIMIT 20

-- Switch/case fallthrough without break
FIND symbols WHERE fql_kind = 'function' WHERE has_fallthrough = 'true' ORDER BY fallthrough_count DESC LIMIT 20
```

### High-Coupling Hotspots
```sql
FIND symbols WHERE fql_kind = 'function' ORDER BY usages DESC LIMIT 10
FIND symbols GROUP BY file HAVING count >= 20 ORDER BY count DESC
```

### Bug Fix Workflow
```sql
-- 1. Locate the symbol — get exact file and line numbers
FIND symbols WHERE name = 'buggyFunction'
-- Result gives: path=src/module.cpp, line=42

-- 2. Read only the relevant lines (preferred over SHOW body)
SHOW LINES 42-89 OF 'src/module.cpp'

-- 3. If you need to see what the function calls
SHOW callees OF 'buggyFunction'

-- 4. If you need a broader structural overview (last resort)
-- SHOW body OF 'buggyFunction' DEPTH 1

-- 5. Blast radius — who else calls this?
FIND usages OF 'buggyFunction' GROUP BY file ORDER BY count DESC

-- 6. Fix atomically
BEGIN TRANSACTION 'fix-buggyFunction'
  CHANGE FILE 'src/module.cpp' LINES 42-89 WITH 'fixed code...'
  VERIFY build 'test'
COMMIT MESSAGE 'fix: handle edge case in buggyFunction'

-- 7. Roll back if verification fails
ROLLBACK
```

### Magic Number → Named Constant

Find repeated magic numbers across files and replace with a named constant.

```sql
-- Step 1: Discover repo top-level structure
FIND files

-- Step 2: Find repeated magic numbers in a numeric range
FIND symbols
  WHERE is_magic = 'true'
  WHERE num_format = 'dec'
  WHERE num_value > 16 WHERE num_value < 64
  IN 'subsys/**'
  GROUP BY name HAVING count >= 2 HAVING count <= 7
  ORDER BY count ASC LIMIT 10

-- Step 3: List all occurrences of the candidate value
FIND symbols WHERE is_magic = 'true' WHERE name = '30U'
  IN 'subsys/**' LIMIT 20

-- Step 4: Read context for each semantic domain (parallel)
SHOW LINES 306-333 OF 'subsys/sd/mmc.c' WHERE text LIKE '%30U%' LIMIT 5

-- Step 5: Confirm no existing constant
FIND symbols WHERE fql_kind = 'macro' WHERE value = '30U'
  IN 'subsys/sd/**' LIMIT 5

-- Step 6: Find common header
SHOW LINES 1-20 OF 'subsys/sd/mmc.c' WHERE text LIKE '%include%' LIMIT 8

-- Step 7: Apply atomically
BEGIN TRANSACTION 'sd-csd-struct-shift'
  CHANGE FILE 'include/zephyr/sd/sd_spec.h'
    LINES 777-777 WITH '#define SDMMC_CSD_STRUCT_SHIFT 30U'
  CHANGE FILE 'subsys/sd/mmc.c' MATCHING '>> 30U' WITH '>> SDMMC_CSD_STRUCT_SHIFT'
  CHANGE FILE 'subsys/sd/sd_ops.c' MATCHING '>> 30U' WITH '>> SDMMC_CSD_STRUCT_SHIFT'
  VERIFY build 'test'
COMMIT MESSAGE 'refactor: name CSD structure field bit shift constant'
```

### Copy / Move Lines

Relocate a block of lines within or across files (v0.31.0+).

```sql
-- Copy to another file (append)
COPY LINES 10-25 OF 'src/module.c' TO 'include/module.h'

-- Copy and insert at a specific line
COPY LINES 10-25 OF 'src/module.c' TO 'include/module.h' AT LINE 42

-- Move (cut + paste, atomic)
MOVE LINES 10-25 OF 'src/module.c' TO 'include/module.h' AT LINE 42

-- Same-file reorder
MOVE LINES 80-95 OF 'src/module.c' TO 'src/module.c' AT LINE 20

-- Wrap in a transaction
BEGIN TRANSACTION 'extract-helper'
  MOVE LINES 42-58 OF 'src/module.c' TO 'include/helpers.h' AT LINE 10
  VERIFY build 'test'
COMMIT MESSAGE 'refactor: extract helper to shared header'
```
