# ForgeQL Instructions for Claude Code

All source code is accessed exclusively through the ForgeQL MCP server.
The local workspace may be empty — never fall back to local filesystem tools (Bash, grep, find, cat, Read File).

## Critical Rules

1. Always start with `USE source.branch AS 'alias'` before any query. The `AS` clause is mandatory.
2. Never use Bash tools (grep, find, cat, less) or Read File for source code. ForgeQL manages all code access.
3. Never brute-force read code. Use FIND to locate symbols, then SHOW NODE to read them by stable handle.
4. **SHOW body and SHOW context** without LIMIT are capped at 40 lines (`SHOW MORE` pages the rest). If capped, use FIND to get the symbol's node_id, then `SHOW NODE '<id>'`. **`SHOW NODE` returns the node's full span** regardless of size.
5. Stack WHERE clauses aggressively before executing. Multiple WHERE clauses combine as AND — filter first, read later.
6. Filter inside the read — never read then grep. `SHOW body OF 'fn' DEPTH 99 WHERE text LIKE '%pattern%'` returns only matching lines.
7. Always ORDER BY in GROUP BY queries. Use `ORDER BY count ASC` to surface lowest-scope candidates first. Add HAVING constraints to filter at aggregate level.
8. Verify structural assumptions before mutating. Check includes and structure before refactoring, not after.
9. Numbers have no symbolic usages — use text search. Use `FIND symbols WHERE name = 'value' WHERE is_magic = 'true'` or `SHOW body ... WHERE text LIKE '%value%'` for literal search.
10. Persist key findings in `HINTS.md`. After completing a task, append short bullet points of key codebase facts discovered (file locations, naming conventions, architectural decisions) to `HINTS.md` in the workspace root.
11. **Edit by node handle only; the diff is the contract.** Mutations are mechanical — the engine never fixes commas, wraps braces, or re-indents. Every mutation returns `new_node_id`, `lines_written`, `lines_removed`, and a boundary diff with inline `node_id(offset)` handles: read it after every mutation and self-correct any seam with `CHANGE NODE '<id>(off)'`. A large `lines_removed` on a small edit means you clobbered more than intended — `UNDO` reverses it. `CHANGE FILE` on indexed files is disabled. Config files (TOML, YAML, JSON, XML/arxml, DBC, Makefile, CMake, INI, justfile, Markdown) are indexed and edited by node handle like code.

## Query Workflow

```
1. FIND symbols WHERE ... → get name, file, line number
2. SHOW NODE '<node_id>' → read the located node by its stable handle
```

**Progressive disclosure for SHOW body:**
- `DEPTH 0` — signature + enrichment metadata row (default, cheapest)
- `DEPTH 1` — control-flow skeleton
- `DEPTH 99` — full source (add LIMIT)

## Query Strategy

| Need | Command |
|---|---|
| Find a symbol | `FIND symbols WHERE name LIKE 'pattern' [WHERE fql_kind = '...'] [IN 'path/**']` |
| Read a located node | `SHOW NODE '<node_id>'` |
| Read/splice lines within a node | `SHOW NODE '<id>(n-m)'` · `CHANGE NODE '<id>(n-m)' WITH '...'` — 1-based offset within the node's own span |
| Symbol signature | `SHOW body OF 'name' DEPTH 0` — also returns enrichment metadata |
| Qualified symbol | `SHOW body OF 'Class::method'` or `SHOW body OF 'Obj.method'` |
| Control flow overview | `SHOW body OF 'name' DEPTH 1` |
| Blast radius | `FIND usages OF 'name' GROUP BY file ORDER BY count DESC` — one row per usage site, includes non-call references |
| Hotspots | `FIND symbols ORDER BY usages DESC LIMIT 10` — `usages` is a real workspace-total count |
| File structure (tree) | `SHOW outline OF 'file'` — structural decls only, `depth` per row; add `ALL` for every node, or `WHERE fql_kind = '...'` |
| Subtree outline | `SHOW outline OF '<node_id>'` |
| Class members | `SHOW members OF 'type'` |
| Call graph | `SHOW callees OF 'name'` |
| File list | `FIND files [IN 'path/**'] [WHERE extension = '...'] ORDER BY size DESC` |
| Context around symbol | `SHOW context OF 'name'` |
| Read + filter a node | `SHOW body OF 'name' DEPTH 99 WHERE text LIKE '%pattern%'` |
| Page a truncated/buffered output | `SHOW MORE [HEAD n \| TAIL n \| n-m] [WHERE text MATCHES '...']` |
| Grep the last `VERIFY` log (no rebuild) | `SHOW MORE WHERE text MATCHES 'error\|warning'` |
| Review an uncommitted change | `SHOW DIFF STAT` for the file map, then `SHOW DIFF IN 'path/**'` |
| Repo top-level dirs | `FIND files` (returns depth-1 entries) |
| Find a file by name | `FIND files WHERE name = 'Kconfig'` (also `LIKE`/`MATCHES`) |
| Insert around a node | `INSERT BEFORE/AFTER NODE '<id>' WITH '...'` |
| Delete a node | `DELETE NODE '<id>' [IF REV '<rev>']` — `'<id>(n-m)'` deletes lines within it |
| Relocate a node | `MOVE NODE '<src>' BEFORE/AFTER NODE '<dst>'` — byte-exact, atomic, cross-file |
| Reverse a bad edit | `UNDO` (most recent) · `UNDO LAST-n` |
| Long test gate | `JOB START 'step'` → `JOB STATUS <id>` / `JOB LIST` (background, FIFO-queued) |

## Anti-Patterns

| Never do this | Do this instead |
|---|---|
| `SHOW body OF 'func' DEPTH 99` without LIMIT | `FIND symbols WHERE name = 'func'` → `SHOW NODE '<id>'` |
| Reading a whole file blindly | `SHOW outline OF 'file'` → `SHOW NODE '<id>'` for specific symbols |
| `FIND symbols` (unfiltered) | `FIND symbols WHERE fql_kind = '...' WHERE name LIKE '...'` |
| `GROUP BY` without `HAVING` or `ORDER BY` | `GROUP BY file HAVING count >= N ORDER BY count ASC` |
| Read a node then filter manually | `SHOW body OF 'name' DEPTH 99 WHERE text LIKE '%pattern%'` |
| `FIND usages OF 'number'` for literal occurrence | `FIND symbols WHERE name = 'value' WHERE is_magic = 'true'` or `SHOW body ... WHERE text LIKE '%value%'` |

## Efficiency

- All commands accept `WHERE`, `GROUP BY`, `ORDER BY`, `LIMIT`, `OFFSET` — combine freely.
- `IN 'src'` and `IN 'crates/'` auto-expand to `IN 'src/**'` — bare directory paths are always safe.
- Multiple `WHERE` clauses combine as AND. `AND` is an accepted synonym for a repeated `WHERE`, so `WHERE fql_kind = 'function' AND lines > 10` parses and runs identically to two `WHERE` clauses.
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

When connected to `forgeql-server` over HTTP, the `USE` response returns a
server-issued `session_id` token scoped to the authenticated user — store it
and pass it verbatim in every subsequent call; do not reconstruct it from the
alias.

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
SHOW outline OF 'file' [ALL] [clauses]   -- structural tree (depth per row); ALL = every node
SHOW outline OF '<node_id>' [clauses]    -- outline a node's subtree
SHOW members OF 'type' [clauses]
SHOW context OF 'name' [clauses]
SHOW callees OF 'name' [clauses]
SHOW NODE '<node_id>' [CONTENT | METADATA] [clauses]   -- '<id>(n)' / '<id>(n-m)' narrows CONTENT
SHOW DIFF [STAT] [clauses]               -- the worktree's UNCOMMITTED diff, inline
```

### CHANGE & Transactions
```sql
-- Indexed code is edited by node handle (below); CHANGE FILE on indexed files
-- is disabled. Raw-text CHANGE FILE / copy / move: non-indexed files only.

CHANGE NODE '<node_id>' [IF REV '<rev>'] WITH 'text'   -- '<id>(n)' / '<id>(n-m)' splices node lines
INSERT (BEFORE | AFTER) NODE '<node_id>' WITH 'text'
DELETE NODE '<node_id>' [IF REV '<rev>']               -- '<id>(n-m)' deletes lines within the node
MOVE NODE '<src_id>' (BEFORE | AFTER) NODE '<dst_id>'  -- relocate byte-exact; atomic; cross-file OK

-- Heredoc form when content contains quotes: WITH <<TAG … TAG (tag uppercase, own line)

UNDO                     -- reverse the most recent mutation
UNDO LAST-n              -- restore the state from n mutations back

BEGIN TRANSACTION 'name'
  -- CHANGE / INSERT / DELETE / MOVE NODE / VERIFY commands
COMMIT MESSAGE 'msg'
VERIFY build 'step'      -- synchronous; grep the buffered log: SHOW MORE WHERE text MATCHES '^error|-->'
JOB START 'step'         -- background job for long gates; JOB STATUS <id> / JOB LIST
ROLLBACK [TRANSACTION 'name']
```

Every mutation answers with `new_node_id`, `lines_written`, `lines_removed`, and a
boundary diff (context lines carry `node_id(offset)` handles) — read it, then fix
any seam yourself. Steps marked `commit_gate: true` in `.forgeql.yaml` must pass
**after** the last edit or COMMIT is refused.

### Universal Clauses (applied in this order)
`IN → EXCLUDE → WHERE → GROUP BY → HAVING → ORDER BY → OFFSET → LIMIT`

**Operators:** `=`, `!=`, `LIKE`, `NOT LIKE`, `MATCHES`, `NOT MATCHES` (regex), `>`, `>=`, `<`, `<=`

`MATCHES` / `NOT MATCHES` use Rust `regex` crate syntax. Prefix `(?i)` for case-insensitive matching.

`SHOW body`, `SHOW NODE`, and `SHOW context` accept `WHERE` on source lines:
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
| `comment_block` | A run of 2+ adjacent same-style comments, as one addressable node (`///` doc runs and `//` line runs form separate blocks) |
| `array_block` | A run of 8+ adjacent JSON `array` siblings, as one addressable node — this is what makes a keyless JSON document (an array of arrays) addressable at all |
| `error` | A tree-sitter `ERROR` region — a span the parser could not parse, emitted as one addressable node (outermost only; nested ERRORs are not repeated). **Its presence means the file is already broken before you touch it.** Check with `FIND symbols WHERE fql_kind = 'error' GROUP BY file` before mutating, then read and repair by handle — the engine never fixes it for you. |

A **block** node is the *sibling* of its members, never their parent. Members keep
their own rows and node_ids; the block is added, nothing is hidden. Read or edit a
member through the block's node-relative offset: `SHOW NODE '<block>(42)'`,
`CHANGE NODE '<block>(42)' WITH '...'`, `DELETE NODE '<block>(40-52)'`.

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
| `condition_text` | `if`, `while`, `for`, `do` | Normalized condition *skeleton* (operands alpha-renamed: `a||b&&c`) — NOT raw source; grammars without a `condition` field use the raw first line |
| `has_catch_all` | `switch` | `"true"` if has a catch-all case |
| `catch_all_kind` | `switch` | e.g. `"default"` |
| `for_style` | `for` | `"traditional"` or `"range"` |
| `has_assignment_in_condition` | `if`, `while`, `for` | `"true"` if condition contains `=` (not `==`) |
| `mixed_logic` | `if`, `while`, `for` | `"true"` if `&&` and `||` appear at the same top-level without explicit parentheses (MISRA Rule 12.1) |
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

### Rename / Refactor (the mechanical sweep)
```sql
-- 1. Definition + blast radius: one row per usage SITE (includes non-call references)
FIND symbols WHERE name = 'oldFunction'
FIND usages OF 'oldFunction' GROUP BY file ORDER BY count DESC
FIND usages OF 'oldFunction' LIMIT 50

-- 2. One targeted CHANGE NODE per site, each confirmed by its returned diff
BEGIN TRANSACTION 'rename-oldFunction'
  CHANGE NODE '<definition_id>' WITH '...renamed definition...'
  CHANGE NODE '<site_id>(off)' WITH '    newFunction(args);'
  -- …repeat per site
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

-- 2. Read the node by its stable handle
SHOW NODE '<node_id>'

-- 3. If you need to see what the function calls
SHOW callees OF 'buggyFunction'

-- 4. If you need a broader structural overview (last resort)
-- SHOW body OF 'buggyFunction' DEPTH 1

-- 5. Blast radius — who else calls this?
FIND usages OF 'buggyFunction' GROUP BY file ORDER BY count DESC

-- 6. Fix atomically
BEGIN TRANSACTION 'fix-buggyFunction'
  CHANGE NODE '<node_id>' WITH 'fixed code...'
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
SHOW NODE '<node_id>' WHERE text LIKE '%30U%' LIMIT 5

-- Step 5: Confirm no existing constant
FIND symbols WHERE fql_kind = 'macro' WHERE value = '30U'
  IN 'subsys/sd/**' LIMIT 5

-- Step 6: Find common header
SHOW outline OF 'subsys/sd/mmc.c' WHERE fql_kind = 'import' LIMIT 8

-- Step 7: Apply atomically — insert the constant, then splice each occurrence
--         by node handle (each occurrence's enclosing node came from step 3)
BEGIN TRANSACTION 'sd-csd-struct-shift'
  INSERT AFTER NODE '<header_anchor_id>' WITH '#define SDMMC_CSD_STRUCT_SHIFT 30U'
  CHANGE NODE '<mmc_site_id>(off)' WITH '    csd->csd_structure = raw >> SDMMC_CSD_STRUCT_SHIFT;'
  CHANGE NODE '<sd_ops_site_id>(off)' WITH '    version = resp >> SDMMC_CSD_STRUCT_SHIFT;'
  VERIFY build 'test'
COMMIT MESSAGE 'refactor: name CSD structure field bit shift constant'
```

### Extract or relocate code

Indexed code is edited by node handle — `CHANGE NODE`, `INSERT BEFORE/AFTER NODE`, `DELETE NODE`, `MOVE NODE`. To **relocate** a node, use `MOVE NODE`: it lifts the bytes verbatim and splices them at the anchor in one atomic plan — no read round-trip, and no window where the file holds the node twice or not at all. Relocating a raw line range across non-indexed files uses `COPY`/`MOVE` from the syntax reference.

```sql
-- Relocate a function to another file, atomically, by node handle
BEGIN TRANSACTION 'extract-helper'
  -- FIND symbols / SHOW outline gave both node_ids; no need to read the source
  MOVE NODE '<helper_id>' AFTER NODE '<dst_anchor_id>'
  VERIFY build 'test'
COMMIT MESSAGE 'refactor: extract helper to shared header'
```

`MOVE NODE` returns `new_node_id` — re-parenting changes `parent_ordinal`, so the moved node earns
a fresh handle. The engine never re-indents (P1): a node lifted out of a block keeps its original
leading whitespace, the boundary diff shows the seam, and you close it with
`CHANGE NODE '<new_id>(1-n)'`. When you want to control the indent from the start, `INSERT` +
`DELETE` in a transaction is still the better tool.
