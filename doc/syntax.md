# ForgeQL Syntax Reference

Authoritative grammar for every ForgeQL command and clause.
Optimized for AI agent consumption — syntax first, advanced patterns second.

---

## Table of Contents

1. [Notation](#notation)
2. [Command Syntax](#command-syntax)
   - [Session Commands](#session-commands)
   - [FIND Commands](#find-commands)
   - [SHOW Commands](#show-commands)
   - [CHANGE Commands](#change-commands)
   - [COPY / MOVE Commands](#copy--move-commands)
   - [Transaction Commands](#transaction-commands)
3. [Universal Clauses](#universal-clauses)
4. [Operators and Values](#operators-and-values)
5. [Filterable Fields](#filterable-fields)
   - [Symbol Fields](#symbol-fields)
   - [Outline Fields](#outline-fields)
   - [Member Fields](#member-fields)
   - [File Fields](#file-fields)
   - [Dynamic Fields](#dynamic-fields)
   - [Enrichment Fields](#enrichment-fields)
6. [Advanced Patterns](#advanced-patterns)

---

## Notation

| Symbol | Meaning |
|---|---|
| `UPPERCASE` | Keyword — write exactly as shown |
| `'string'` or `"string"` | String literal — single or double quotes |
| `N` | Integer literal |
| `n-m` | Inclusive line range, e.g. `10-25` |
| `[ … ]` | Optional element |
| `( A \| B )` | Choose one |
| `…` | Repeatable |

---

## Command Syntax

### Session Commands

```sql
CREATE SOURCE 'name' FROM 'url'

REFRESH SOURCE 'name'

USE source_name.branch AS 'alias'

SHOW SOURCES

SHOW BRANCHES
```

`source_name` is an unquoted identifier that may contain hyphens (e.g. `pisco-code`).
`branch` is an unquoted identifier that may contain hyphens (e.g. `main`, `v1_3_0`, `line-budget`).
`alias` is the worktree branch name — single/double-quoted or bare (unquoted).
**The alias you choose becomes the `session_id`** for all subsequent `run_fql` calls on that
session. It is deterministic and reconstructable: if you forget the session_id, simply
re-issue the same `USE` command with the same alias to reconnect.

Sessions start automatically on the first `USE` and persist until the worktree has been
idle for 48 hours (server-side TTL). There is no explicit disconnect command — multiple
agents can reconnect to the same worktree at any time with the same `USE` command.

Worktree identity uses a composite key: filesystem directory = `branch.alias` (flat),
git branch = `fql/branch/alias` (under the `fql/` namespace).

---

### FIND Commands

```sql
FIND symbols [clauses]

FIND globals [clauses]

FIND usages OF 'symbol_name' [clauses]

FIND callees OF 'symbol_name' [clauses]

FIND files [clauses]
```

**clauses**: see [Universal Clauses](#universal-clauses).

| Command | Returns |
|---|---|
| `FIND symbols` | All indexed AST nodes. Use `WHERE fql_kind = '...'` to narrow. |
| `FIND globals` | Shorthand for file-scope `declaration` nodes only. |
| `FIND usages OF` | Every identifier reference to the named symbol. |
| `FIND callees OF` | Symbols called from inside the named function body. Alias for `SHOW callees OF`. |
| `FIND files` | Files in the worktree. Supports `WHERE`, `DEPTH`, `ORDER BY size`, etc. |

> **Use `fql_kind` for all filtering.** It is language-agnostic and portable across C++, Rust, and any future language. Raw `node_kind` values (tree-sitter grammar names) are language-specific and **deprecated**.

---

### SHOW Commands

```sql
SHOW body OF 'symbol_name' [DEPTH N] [clauses]

SHOW signature OF 'symbol_name' [clauses]

SHOW outline OF 'file_path' [clauses]

SHOW members OF 'type_name' [clauses]

SHOW context OF 'symbol_name' [clauses]

SHOW callees OF 'symbol_name' [clauses]

SHOW LINES n-m OF 'file_path' [clauses]
```

| Command | Returns |
|---|---|
| `SHOW body OF` | Source text of a symbol. **Default `DEPTH 0`**: signature only, body replaced by `{ ... }`. `DEPTH 1`+: progressively reveals nested structure. `DEPTH 99`: full source. |
| `SHOW signature OF` | Declaration line only (return type, name, parameters). |
| `SHOW outline OF` | Structural outline of a file: all top-level symbols with fql_kind, name, line. Supports `WHERE fql_kind = '...'`, `ORDER BY`, `LIMIT`, `OFFSET`. |
| `SHOW members OF` | Member declarations of a class/struct/enum: fields, methods, enumerators. Supports `WHERE fql_kind = '...'`, `ORDER BY`, `LIMIT`, `OFFSET`. |
| `SHOW context OF` | Surrounding lines of a symbol definition. `DEPTH N` controls how many context lines (default 5). |
| `SHOW callees OF` | All symbols called from inside the named function body. |
| `SHOW LINES n-m OF` | Verbatim line range from a file. |

Every `SHOW` response includes `start_line` and `end_line` — chain directly into `CHANGE LINES` without re-reading.

> **Template limitation** — `SHOW callees OF` does not resolve C++ template functions. Use `FIND usages OF 'symbol'` instead.

---

### CHANGE Commands

```sql
CHANGE (FILE | FILES) file_list
    MATCHING 'old_text' WITH 'new_text'

CHANGE (FILE | FILES) file_list
    LINES n-m WITH 'new_content'

CHANGE (FILE | FILES) file_list
    LINES n-m WITH NOTHING

CHANGE FILE 'file_path'
    WITH 'new_full_content'

CHANGE FILE 'file_path'
    WITH NOTHING
```

`file_list`: one or more single-quoted glob patterns, comma-separated.
`FILE` and `FILES` are interchangeable.

| Variant | Effect |
|---|---|
| `MATCHING … WITH …` | Replace all literal occurrences across matched files |
| `LINES n-m WITH '…'` | Replace a specific line range with new content |
| `LINES n-m WITH NOTHING` | Delete a specific line range |
| `WITH '…'` | Replace entire file content (creates file if absent) |
| `WITH NOTHING` | Clear file content (file remains on disk, empty) |

#### Heredoc syntax

Every `WITH 'content'` form also accepts a heredoc block as the replacement text:

```sql
-- Replace a line range — no escaping needed for Rust lifetimes, char literals, etc.
CHANGE FILE 'src/lib.rs' LINES 10-15 WITH <<RUST
fn longest<'a>(x: &'a str, y: &'a str) -> &'a str {
    if x.len() > y.len() { x } else { y }
}
RUST

-- Find-and-replace across files
CHANGE FILES 'src/**/*.rs' MATCHING 'old_api()' WITH <<CODE
new_api()
CODE

-- Replace full file content
CHANGE FILE 'src/config.rs' WITH <<END
// regenerated
const VERSION: &str = "2.0";
END
```

| Heredoc rule | Detail |
|---|---|
| Opening tag | `<<TAG` immediately after `WITH` — tag must be **all-uppercase** (e.g. `RUST`, `CODE`, `END`) |
| Closing tag | Must appear on its **own line** with **no leading whitespace**, matching the opening tag exactly |
| Body | May contain any characters — single quotes, double quotes, embedded ForgeQL keywords — without escaping |
| Purpose | Prefer over `'…'` when the replacement contains single quotes (Rust char literals, lifetimes, C-style string escapes) |

---

### COPY / MOVE Commands

Copies or moves lines n..=m (1-based, inclusive) from `src_path` into `dst_path`.

Syntax:

    COPY LINES n-m OF 'src' TO 'dst'
    COPY LINES n-m OF 'src' TO 'dst' AT LINE k

    MOVE LINES n-m OF 'src' TO 'dst'
    MOVE LINES n-m OF 'src' TO 'dst' AT LINE k

| Argument | Meaning |
|---|---|
| `n-m` | 1-based inclusive source line range |
| `'src'` | Relative source file path |
| `'dst'` | Relative destination file path (may equal `'src'`) |
| `AT LINE k` | Insert before line `k` in `dst`; omitted = append at end |

**COPY** — inserts the lines into `dst` without modifying `src`.
**MOVE** — inserts the lines into `dst` then deletes them from `src`. Same-file moves are handled atomically.

---

### Transaction Commands

```sql
BEGIN TRANSACTION 'name'

COMMIT MESSAGE 'message'

VERIFY build 'step'

ROLLBACK [TRANSACTION 'name']
```

| Command | Effect |
|---|---|
| `BEGIN TRANSACTION` | Create a named git checkpoint. Dirty state is auto-committed first. Checkpoints stack — multiple `BEGIN` calls push; `ROLLBACK` pops. |
| `COMMIT MESSAGE` | Stage all changes and create a git commit. |
| `VERIFY build` | Run a named step from `.forgeql.yaml` `verify_steps`. Returns `success` + `output`. Does **not** auto-rollback on failure. |
| `ROLLBACK` | Revert to the most recent checkpoint, or to a named one (discards later checkpoints). |

**`.forgeql.yaml`** may be in the repo root **or** in the directory directly above it (sidecar, outside the tracked tree):

```yaml
workspace_root: .
verify_steps:
  - name: test
    command: "cmake --build build && ctest --test-dir build"
    timeout_secs: 120
line_budget:
  ceiling: 5000           # max lines per session
  warning_pct: 20         # warning state below 20% remaining
  critical_pct: 5         # critical state below 5% — caps SHOW LINES output
  recovery_pct: 2         # recovery per qualifying command (% of ceiling)
  recovery_window_secs: 60
  idle_reset_secs: 300    # auto-delete budget file after idle gap
```

**Line budget:** when `line_budget` is present, each session tracks how many source
lines the agent has consumed. Budget status (`remaining/ceiling (delta)`) is returned
in every MCP response via the `line_budget` metadata field. Budget files are persisted
to `.budgets/{source}@{branch}.json` under the ForgeQL data directory. Expired files
are auto-deleted on the next `USE` via `sweep_expired()`.

---

## Universal Clauses

Every command accepts these clauses. Inapplicable clauses are silently ignored.
Multiple `WHERE` clauses combine with implicit AND.

Engine applies clauses in this fixed pipeline order, regardless of written order:

```
IN → EXCLUDE → WHERE → GROUP BY → HAVING → ORDER BY → OFFSET → LIMIT
```

```sql
[WHERE field operator value] …
[HAVING field operator value]
[IN 'glob']
[EXCLUDE 'glob']
[ORDER BY field [ASC | DESC]]
[GROUP BY (file | fql_kind)]
[LIMIT N]
[OFFSET N]
[DEPTH N]
```

| Clause | Purpose |
|---|---|
| `WHERE` | Filter rows. Repeatable (implicit AND). Works on all field types including dynamic and enrichment fields. |
| `HAVING` | Filter after `GROUP BY` aggregation. Operates on `count`. |
| `IN` | Restrict to files matching glob pattern. |
| `EXCLUDE` | Remove files matching glob pattern. |
| `ORDER BY` | Sort results. Default `ASC`. Any filterable field including enrichment fields (numeric values like `shadow_count`, `escape_count` sort numerically). |
| `GROUP BY` | Aggregate by field. Adds `count` to each group. |
| `LIMIT` | Maximum rows returned. Implicit cap of 20 when omitted on `FIND`. |
| `OFFSET` | Skip N rows (pagination). |
| `DEPTH` | For `SHOW body`: collapse depth. For `FIND files`: directory tree depth. |

---

## Operators and Values

| Operator | Meaning |
|---|---|
| `=` | Exact equality |
| `!=` | Not equal |
| `LIKE` | SQL wildcard: `%` = any sequence, `_` = any single char (case-insensitive) |
| `NOT LIKE` | Negated LIKE |
| `MATCHES` | Regex match (Rust `regex` crate syntax, case-sensitive by default; use `(?i)` for case-insensitive) |
| `NOT MATCHES` | Negated regex match |
| `>` `>=` `<` `<=` | Numeric comparison |

| Value syntax | Type |
|---|---|
| `'text'` | String (single-quoted) |
| `"text"` | String (double-quoted) |
| `bare_value` | Unquoted string — alphanumeric, `_`, `:`, `-`, `.`, `/` (where quoting is optional) |
| `42` | Integer |
| `-10` | Signed integer |
| `true` / `false` | Boolean (reserved) |

**Quoting rules:** `CHANGE … MATCHING` and `COMMIT MESSAGE` require explicit quotes
(content may contain spaces). `CHANGE FILE` paths require explicit quotes for mutation
safety. All other positions accept bare values or either quote style.

---

## Filterable Fields

### Symbol Fields

Applies to: `FIND symbols`, `FIND usages OF`, `FIND callees OF`

| Field | Type | Description |
|---|---|---|
| `name` | string | Symbol name |
| `fql_kind` | string | Universal kind: `function`, `class`, `struct`, `enum`, `variable`, `field`, etc. |
| `language` | string | Language name: `cpp`, `rust`, `python`, etc. |
| `path` | string | Relative file path (also used by `IN`/`EXCLUDE` globs) |
| `line` | integer | 1-based start line |
| `usages` | integer | Reference count across the index |

### Outline Fields

Applies to: `SHOW outline OF`

| Field | Type | Description |
|---|---|---|
| `name` | string | Symbol name |
| `kind` | string | Universal kind (`fql_kind` value, e.g. `function`, `class`). Falls back to raw tree-sitter name for unmapped nodes. |
| `path` / `file` | string | Relative file path |
| `line` | integer | 1-based start line |

### Member Fields

Applies to: `SHOW members OF`

| Field | Type | Description |
|---|---|---|
| `kind` / `type` | string | Member kind (`field`, `method`, `enumerator`) |
| `text` / `declaration` / `name` | string | Declaration text |
| `line` | integer | 1-based line number |

### File Fields

Applies to: `FIND files`

| Field | Type | Description |
|---|---|---|
| `path` / `file` | string | Relative file path |
| `extension` / `ext` | string | Extension without `.` (empty for extension-less files) |
| `size` | integer | File size in bytes |
| `depth` | integer | Directory depth from workspace root |

### Source Line Fields

Applies to: `SHOW body OF`, `SHOW LINES n-m OF`, `SHOW context OF`

| Field | Type | Description |
|---|---|---|
| `text` | string | Line content (supports `LIKE`, `MATCHES`, `=`) |
| `line` | integer | 1-based line number |
| `marker` | string | Prefix marker (e.g. `+`, `-` in diff output) |

Filtering runs **before** the implicit `DEFAULT_SHOW_LINE_LIMIT` cap, so the full function body is searched even when not all lines are returned.

### Call Graph Fields

Applies to: `SHOW callees OF`

| Field | Type | Description |
|---|---|---|
| `name` | string | Called symbol name |
| `path` / `file` | string | File containing the call |
| `line` | integer | 1-based line number of the call |

### Dynamic Fields

Auto-extracted from tree-sitter grammar. Queryable with `WHERE` without recompiling.

| Field | Availability | Description |
|---|---|---|
| `type` | C/C++ | Return type text |
| `value` | C/C++ | Initial value (`preproc_def`, `init_declarator`) |
| `declarator` | C/C++ | Full declarator with pointer/reference qualifiers |
| `parameters` | C/C++ | Parameter list text |

If a field does not exist on a row, `WHERE` evaluates to false (SQL `NULL` semantics).

**Numeric coercion** — dynamic fields are stored as strings. `WHERE value >= 1000` parses the stored text as an integer; if parsing fails, the predicate silently evaluates to false.

### Enrichment Fields

Computed at index time. Queryable with `WHERE` like any other field.

**Naming convention for enrichment fields:**

| Prefix | Meaning | Example |
|---|---|---|
| `is_` | Intrinsic property of the symbol itself | `is_recursive`, `is_exported`, `is_const`, `is_magic` |
| `has_` | The symbol's body **contains** something | `has_shadow`, `has_escape`, `has_fallthrough`, `has_cast`, `has_todo` |
| `_count` | Numeric count (often paired with `has_` or `is_`) | `shadow_count`, `cast_count`, `recursion_count`, `param_count` |

> **Rule of thumb:** `is_X` describes *what a symbol is*; `has_X` describes *what it contains*.
> For example, a function `is_recursive` (it calls itself) and `has_shadow` (variables inside it shadow outer ones).
#### NamingEnricher

| Field | Applies to | Description |
|---|---|---|
| `naming` | all named symbols | `camelCase`, `PascalCase`, `snake_case`, `UPPER_SNAKE`, `flatcase`, `other` |
| `name_length` | all named symbols | Character count of symbol name |

#### CommentEnricher

| Field | Applies to | Description |
|---|---|---|
| `comment_style` | `comment` | `doc_line` (`///`), `doc_block` (`/** */`), `block` (`/* */`), `line` (`//`) |
| `has_doc` | `function` | `"true"` if preceded by a doc comment |

#### NumberEnricher

| Field | Applies to | Description |
|---|---|---|
| `num_format` | `number` | `dec`, `hex`, `bin`, `oct`, `float`, `scientific` |
| `is_magic` | `number` | `"true"` for unexplained constants (not 0, 1, -1, 2, powers of 2, bitmasks) |
| `num_suffix` | `number` | Type suffix: `u`, `l`, `ll`, `ul`, `ull`, `f`, `ld` |
| `suffix_meaning` | `number` | Semantic meaning of suffix: `unsigned`, `long`, `float`, etc. |
| `has_separator` | `number` | `"true"` if contains digit separators |
| `num_value` | `number` | Raw text of the literal |

#### ControlFlowEnricher

| Field | Applies to | Description |
|---|---|---|
| `condition_tests` | `if`, `while`, `for`, `do` | Number of boolean sub-expressions |
| `paren_depth` | `if`, `while`, `for`, `do` | Max parentheses nesting |
| `condition_text` | `if`, `while`, `for`, `do` | Raw condition expression |
| `has_catch_all` | `switch` | `"true"` if switch has a catch-all case |
| `catch_all_kind` | `switch` | Kind of catch-all (e.g. `"default"`) when present |
| `for_style` | `for` | `"traditional"` or `"range"` |
| `has_assignment_in_condition` | `if`, `while`, `for` | `"true"` if condition contains `=` (not `==`) |
| `mixed_logic` | `if`, `while`, `for` | `"true"` if mixes `&&` and `\|\|` without grouping |
| `dup_logic` | `if`, `while`, `for`, `do` | `"true"` if condition contains duplicate sub-expressions in `&&`/`\|\|` chains |
| `branch_count` | `function` | Total control-flow branch points |
| `enclosing_fn` | `if`, `switch`, `for`, `while`, `do` | Name of the containing function — enables `SHOW body OF` directly from a CF-enrichment query result |

#### OperatorEnricher

| Field | Applies to | Description |
|---|---|---|
| `increment_style` | `increment` | `"prefix"` or `"postfix"` |
| `increment_op` | `increment` | `"++"` or `"--"` |
| `compound_op` | `compound_assignment` | `+=`, `-=`, `*=`, `/=`, `%=`, `&=`, `\|=`, `^=`, `<<=`, `>>=` |
| `operand` | `compound_assignment` | Left-hand side text |
| `shift_direction` | `shift_expression` | `"left"` or `"right"` |
| `shift_amount` | `shift_expression` | Right-hand operand text |
| `operator_category` | `increment`, `compound_assignment`, `shift_expression` | `"increment"`, `"arithmetic"`, `"bitwise"`, `"shift"` |

#### MetricsEnricher

| Field | Applies to | Description |
|---|---|---|
| `lines` | `function`, `struct`, `class`, `enum` | Line span |
| `param_count` | `function` | Parameter count |
| `return_count` | `function` | `return` statement count |
| `goto_count` | `function` | `goto` statement count |
| `string_count` | `function` | String literal count |
| `throw_count` | `function` | `throw` statement count |
| `member_count` | `struct`, `class`, `enum` | Member/enumerator count |
| `is_const` | `function`, `variable` | `"true"` if `const` present |
| `is_volatile` | `function`, `variable` | `"true"` if `volatile` present |
| `is_static` | `function` | `"true"` if `static` |
| `is_inline` | `function` | `"true"` if `inline` |
| `is_override` | `function` | `"true"` if `override` |
| `is_final` | `function` | `"true"` if `final` |
| `visibility` | `field` (class members) | `"public"`, `"private"`, `"protected"` |

#### CastEnricher

| Field | Applies to | Description |
|---|---|---|
| `cast_style` | `cast` | `"c_style"` (named C++ casts not indexed in tree-sitter-cpp 0.23) |
| `cast_target_type` | `cast` | Target type text |
| `cast_safety` | `cast` | `"safe"`, `"moderate"`, or `"unsafe"` |
| `has_cast` | `function` | `"true"` if the function body contains any cast expressions |
| `cast_count` | `function` | Number of cast expressions in the body |
#### RedundancyEnricher

| Field | Applies to | Description |
|---|---|---|
| `has_repeated_condition_calls` | `function` | `"true"` if same call in 2+ conditions |
| `repeated_condition_calls` | `function` | Comma-separated function names |
| `null_check_count` | `function` | Count of null-check patterns |
| `duplicate_condition` | `if`, `while`, `for`, `do` | `"true"` if same condition skeleton exists elsewhere in function |

#### ScopeEnricher

| Field | Applies to | Description |
|---|---|---|
| `scope` | `variable` | `"file"` (top-level) or `"local"` (inside function/block) |
| `storage` | `variable` | `"static"`, `"extern"`, or absent |
| `binding_kind` | `variable` | `"function"` or `"variable"` |
| `is_exported` | `variable`, `function` | `"true"` for file-scope declarations without `static` storage (C/C++) or `pub` functions (Rust) |
#### MemberEnricher

| Field | Applies to | Description |
|---|---|---|
| `body_symbol` | `field` (methods) | Qualified name linking to out-of-line definition (e.g. `Class::method`) |
| `member_kind` | `field` | `"method"` or `"field"` |
| `owner_kind` | `field` | `fql_kind` of enclosing type (e.g. `class`, `struct`) |

#### DeclDistanceEnricher

Data-flow enricher that measures how far local variable declarations are from their first use. Excludes parameters, globals, and member variables.

| Field | Applies to | Description |
|---|---|---|
| `decl_distance` | `function` | Sum of (first-use line − declaration line) for locals with distance ≥ 2 |
| `decl_far_count` | `function` | Count of local variables whose first-use is ≥ 2 lines after declaration |
| `has_unused_reassign` | `function` | `"true"` when a local is reassigned before its previous value was read (dead store) |

#### EscapeEnricher

Detects local variables that escape their declaring function — via `return`, address-of (`&`), or pointer/array aliasing.

| Field | Applies to | Description |
|---|---|---|
| `has_escape` | `function` | `"true"` if any local escapes |
| `escape_count` | `function` | Number of distinct escaping locals |
| `escape_vars` | `function` | Comma-separated names of escaping locals |
| `escape_tier` | `function` | Severity: `1` (return), `2` (address-of), `3` (pointer/array alias) |
| `escape_kinds` | `function` | Comma-separated escape mechanisms (e.g. `"return,address_of"`) |

#### ShadowEnricher

Detects variables declared in inner scopes that shadow an outer-scope variable or parameter of the same name.

| Field | Applies to | Description |
|---|---|---|
| `has_shadow` | `function` | `"true"` if any inner variable shadows an outer one |
| `shadow_count` | `function` | Number of shadowing declarations |
| `shadow_vars` | `function` | Comma-separated names of shadowed variables |

> **Note — `#ifdef` blocks:** As of Phase 1, the ShadowEnricher uses
> structural guard exclusivity (`guard_group_id` + `guard_branch`) to
> suppress false positives from `#ifdef`/`#else` siblings.  Variables
> declared in opposite arms of the same guard group are no longer reported
> as shadows.

#### UnusedParamEnricher

Detects function parameters that are never referenced in the function body.

| Field | Applies to | Description |
|---|---|---|
| `has_unused_param` | `function` | `"true"` if any parameter is unused |
| `unused_param_count` | `function` | Number of unused parameters |
| `unused_params` | `function` | Comma-separated names of unused parameters |

#### FallthroughEnricher

Detects switch/case statements where a non-empty case falls through to the next without `break` or `return`. Empty cases (intentional grouping) are not flagged.

| Field | Applies to | Description |
|---|---|---|
| `has_fallthrough` | `function` | `"true"` if any case falls through |
| `fallthrough_count` | `function` | Number of fallthrough cases |

#### RecursionEnricher

Detects direct (single-function) self-recursion. Does not detect mutual recursion (A→B→A).

| Field | Applies to | Description |
|---|---|---|
| `is_recursive` | `function` | `"true"` if the function calls itself |
| `recursion_count` | `function` | Number of self-call sites in the body |

#### TodoEnricher

Detects TODO, FIXME, HACK, and XXX markers in comments inside function bodies. Word-boundary-aware matching avoids false positives.

| Field | Applies to | Description |
|---|---|---|
| `has_todo` | `function` | `"true"` if any marker comment is found |
| `todo_count` | `function` | Total number of marker occurrences |
| `todo_tags` | `function` | Comma-separated, sorted unique tags found (e.g. `"FIXME,TODO"`) |

#### GuardEnricher

Tags every symbol inside a C/C++ `#ifdef`/`#if`/`#elif`/`#else` block with
the guard condition that controls its compilation.  Guard fields are injected
into **every** indexed symbol row by `collect_nodes()` — no separate enricher
call is needed.  All seven fields are queryable via `WHERE`, `ORDER BY`, and
`GROUP BY`.

| Field | Applies to | Description |
|---|---|---|
| `guard` | all symbols | Raw guard condition text (e.g. `"defined(CONFIG_SMP)"`, `"!X"`, `"Y && X"`) |
| `guard_defines` | all symbols | Comma-separated symbols that **must be defined** for this branch |
| `guard_negates` | all symbols | Comma-separated symbols that **must be undefined** for this branch |
| `guard_mentions` | all symbols | All symbols mentioned in the condition (superset of defines + negates) |
| `guard_group_id` | all symbols | Unique u64 identifying the `#ifdef`/`#if` block; all arms share the same ID |
| `guard_branch` | all symbols | Ordinal within the group: `0` = if, `1` = first elif/else, `2` = second, … |
| `guard_kind` | all symbols | `"preprocessor"` \| `"attribute"` \| `"build_tag"` \| `"comptime"` \| `"heuristic"` |

**Guard field decomposition rules:**

| Source | `guard` | `guard_defines` | `guard_negates` | `guard_mentions` |
|---|---|---|---|---|
| `#ifdef X` | `"X"` | `"X"` | `""` | `"X"` |
| `#ifndef X` | `"!X"` | `""` | `"X"` | `"X"` |
| `#if defined(A) && defined(B)` | `"defined(A) && defined(B)"` | `"A,B"` | `""` | `"A,B"` |
| `#else` of `#ifdef X` | `"!X"` | `""` | `"X"` | `"X"` |
| Nested `#ifdef X` inside `#ifdef Y` | `"Y && X"` | `"Y,X"` | `""` | `"Y,X"` |

**Example queries:**

```sql
-- All code that REQUIRES CONFIG_BT
FIND symbols WHERE guard_defines LIKE '%CONFIG_BT%'

-- All code compiled when CONFIG_BT is ABSENT
FIND symbols WHERE guard_negates LIKE '%CONFIG_BT%'

-- All code that MENTIONS CONFIG_BT (either direction)
FIND symbols WHERE guard_mentions LIKE '%CONFIG_BT%'

-- Unconditionally compiled code only
FIND symbols WHERE guard = ''

-- Count symbols per guard define
FIND symbols GROUP BY guard ORDER BY count DESC
```

**Structural exclusivity:** Two symbols with the same `guard_group_id` and
different `guard_branch` are definitively mutually exclusive — they are in
opposite arms of the same `#ifdef` block.  The ShadowEnricher and
DeclDistanceEnricher use this fact to eliminate false positives.

#### MacroExpandEnricher

Enriches `macro_call` rows with macro definition metadata and best-effort
single-level expansion text.  Registered after `TodoEnricher` in the enricher
pipeline.  Requires a `MacroTable` populated during the two-pass indexing
pipeline.

| Field | Applies to | Description |
|---|---|---|
| `macro_def_file` | `macro_call` | Source file of the resolved macro definition |
| `macro_def_line` | `macro_call` | 1-based line of the definition |
| `macro_arity` | `macro_call` | Parameter count (`"0"` for object-like macros) |
| `macro_expansion` | `macro_call` | Best-effort single-level expansion text |
| `expanded_reads` | `macro_call` | Local variable names read in expanded text |
| `expanded_has_escape` | `macro_call` | `"true"` if expanded text contains `&local` escape |
| `expansion_depth` | `macro_call` | Expansion nesting depth (currently always `"1"`) |
| `expansion_failed` | `macro_call` | `"true"` when macro resolution fails |
| `expansion_failure_reason` | `macro_call` | Reason for failure (e.g. `"definition not found"`) |

**Supported languages:** C/C++ (`CppMacroExpander`) and Rust (`RustMacroExpander` for `macro_rules!`).

---

## Advanced Patterns

These patterns show ForgeQL capabilities that are non-obvious or combine multiple features.

### Progressive function exploration

`SHOW body` defaults to `DEPTH 0` (signature only). Incrementally reveal structure without reading full source:

```sql
-- Step 1: signature only — understand the interface
SHOW body OF 'PiscoCode::process'

-- Step 2: top-level branches visible — see the control flow
SHOW body OF 'PiscoCode::process' DEPTH 1

-- Step 3: full source when needed
SHOW body OF 'PiscoCode::process' DEPTH 99
```

### Dead code detection pipeline

```sql
-- Unreferenced functions (skip test files)
FIND symbols
  WHERE fql_kind = 'function'
  WHERE usages = 0
  EXCLUDE 'tests/**'
  ORDER BY path ASC

-- Unreferenced macros in headers
FIND symbols
  WHERE fql_kind = 'macro'
  WHERE usages = 0
  IN 'include/**'

-- Dead code behind guards (unreferenced guarded functions)
FIND symbols
  WHERE fql_kind = 'function'
  WHERE guard != ''
  WHERE usages = 0
  EXCLUDE 'test/**'
  ORDER BY lines DESC

-- Symbol distribution (spot bloated files)
FIND symbols
  GROUP BY file
  HAVING count >= 20
  ORDER BY count DESC
```

### Guard analysis pipeline

```sql
-- All code gated on a specific config option
FIND symbols WHERE guard_defines LIKE '%CONFIG_BT%'

-- Code compiled only when a feature is ABSENT
FIND symbols WHERE guard_negates LIKE '%CONFIG_SMP%'

-- Large functions in #else branches (often forgotten)
FIND symbols
  WHERE fql_kind = 'function'
  WHERE guard_branch = '1'
  ORDER BY lines DESC
  LIMIT 15

-- Recursive functions behind guards
FIND symbols
  WHERE is_recursive = 'true'
  WHERE guard != ''
  ORDER BY recursion_count DESC

-- Guard distribution by kind
FIND symbols
  WHERE guard != ''
  GROUP BY guard_kind
  HAVING count >= 1
  ORDER BY count DESC
```

### Code quality audit

```sql
-- Functions longer than 50 lines (refactoring candidates)
FIND symbols
  WHERE fql_kind = 'function'
  WHERE lines >= 50
  ORDER BY lines DESC

-- Complex conditions (4+ sub-tests)
FIND symbols WHERE condition_tests >= 4

-- Switch without default
FIND symbols
  WHERE fql_kind = 'switch'
  WHERE has_catch_all = 'false'

-- Mixed && / || without grouping parentheses
FIND symbols WHERE mixed_logic = 'true'

-- Assignment in condition (likely bug)
FIND symbols WHERE has_assignment_in_condition = 'true'

-- Magic numbers
FIND symbols WHERE is_magic = 'true'

-- C-style casts (modernization targets)
FIND symbols WHERE cast_style = 'c_style'

-- Functions with goto
FIND symbols WHERE goto_count >= 1

-- Duplicated conditions within same function
FIND symbols WHERE duplicate_condition = 'true'

-- Duplicate logic within a single condition (copy-paste bugs)
FIND symbols WHERE dup_logic = 'true'

-- Functions with repeated conditional calls (extract-variable opportunity)
FIND symbols WHERE has_repeated_condition_calls = 'true'

-- Variables declared far from their first use (move declaration closer)
FIND symbols
  WHERE fql_kind = 'function'
  WHERE decl_far_count >= 3
  ORDER BY decl_distance DESC

-- Dead stores (value written but never read before overwrite)
FIND symbols
  WHERE fql_kind = 'function'
  WHERE has_unused_reassign = 'true'

-- Regex search: functions whose name ends with _impl
FIND symbols
  WHERE fql_kind = 'function'
  WHERE name MATCHES '_impl$'

-- Source lines containing TODO/FIXME (case-insensitive)
SHOW body OF 'PiscoCode::run' DEPTH 99
  WHERE text MATCHES '(?i)TODO|FIXME'
```

> **Tip — exclude test directories:**  Enrichment queries on large codebases
> can be noisy if the results include test harnesses, mocks, and generated
> test code.  Add `EXCLUDE` clauses to focus on production code:
>
> ```sql
> FIND symbols WHERE has_assignment_in_condition = 'true'
>   EXCLUDE '**/testsuite/**'
>   EXCLUDE '**/tests/**'
>   EXCLUDE '**/test/**'
> ```

### Filtered outline and member inspection

`SHOW outline` and `SHOW members` support the full clause pipeline including `WHERE`:

```sql
-- Only enum declarations in a header
SHOW outline OF 'include/config.h'
  WHERE fql_kind = 'enum'

-- Only function definitions in outline
SHOW outline OF 'src/PiscoCode.cpp'
  WHERE fql_kind = 'function'
  ORDER BY line ASC

-- Only field members of a class (skip methods)
SHOW members OF 'PiscoCode'
  WHERE fql_kind = 'field'

-- Paginate a large outline
SHOW outline OF 'include/PiscoCode.h'
  LIMIT 10 OFFSET 20
```

### Usage heat-map and call graph

```sql
-- Which files reference this symbol the most?
FIND usages OF 'PiscoCode::process'
  GROUP BY file
  ORDER BY count DESC

-- What does this function call?
SHOW callees OF 'PiscoCode::process'

-- Top 10 most-referenced functions
FIND symbols
  WHERE fql_kind = 'function'
  ORDER BY usages DESC
  LIMIT 10
```

### Safe multi-step refactoring

Each statement executes independently — the agent sees every result and decides whether to proceed.

```sql
-- 1. Checkpoint
BEGIN TRANSACTION 'rename-process'

-- 2. Rename across all translation units
CHANGE FILES 'src/**/*.cpp', 'include/**/*.h'
  MATCHING 'PiscoCode::process' WITH 'PiscoCode::run'

-- 3. Verify the build
VERIFY build 'test'

-- 4a. Success → commit
COMMIT MESSAGE 'rename PiscoCode::process to PiscoCode::run'

-- 4b. Failure → rollback
ROLLBACK TRANSACTION 'rename-process'
```

### Checkpoint stack for phased changes

```sql
-- Phase 1
BEGIN TRANSACTION 'phase-1-rename'
CHANGE FILES 'src/**/*.cpp', 'include/**/*.h'
  MATCHING 'OldName' WITH 'NewName'
VERIFY build 'test'
COMMIT MESSAGE 'rename OldName to NewName'

-- Phase 2
BEGIN TRANSACTION 'phase-2-add-param'
CHANGE FILE 'include/NewName.h'
  LINES 12-12
  WITH 'void NewName::run(Buffer& buf, int flags);'
VERIFY build 'test'

-- Phase 2 failed — roll back only phase 2; phase 1 commit preserved
ROLLBACK TRANSACTION 'phase-2-add-param'
```

### SHOW body → CHANGE LINES workflow

`SHOW body` returns `start_line` / `end_line`, enabling line-precise edits:

```sql
-- Read the function (DEPTH 99 for full source)
SHOW body OF 'PiscoCode::process' DEPTH 99
-- Response includes start_line=87, end_line=103

-- Rewrite it
BEGIN TRANSACTION 'rewrite-process'
CHANGE FILE 'src/PiscoCode.cpp'
  LINES 87-103
  WITH 'void PiscoCode::run(Buffer& buffer) {
    for (auto& sample : buffer) {
        sample = this->pipeline.apply(sample);
    }
}'
VERIFY build 'test'
COMMIT MESSAGE 'rewrite PiscoCode::run'
```

### File system exploration

```sql
-- Large files (potential split candidates)
FIND files
  WHERE size > 100000
  ORDER BY size DESC
  LIMIT 10

-- Non-source files in src/
FIND files IN 'src/**'
  WHERE extension NOT LIKE 'cpp'
  WHERE extension NOT LIKE 'h'

-- Directory tree 2 levels deep
FIND files DEPTH 2
```

### Compact CSV output (MCP mode)

In MCP mode the default output is compact CSV — token-efficient grouped format.
Pass `format=JSON` for full structured JSON.

All compact output follows a uniform 2-column structure:

```csv
"op",total_count
"group_key","[field1,field2,...]"
"group_value_a","[v1,v2],[v3,v4]"
"group_value_b","[v5,v6]"
"tokens_approx",N
```

**FIND symbols** — grouped by `fql_kind`:
```csv
"find_symbols",8
"fql_kind","[name,path,line,usages]"
"function","[encenderMotor,src/motor_control.cpp,12,7],[apagarMotor,src/motor_control.cpp,28,5]"
"class","[MotorControl,include/motor_control.hpp,5,2]"
```

When a numeric `WHERE` or `ORDER BY` targets an enrichment field, the last
column shows that field's value instead of `usages`:
```csv
-- FIND symbols WHERE member_count > 10
"find_symbols",3
"fql_kind","[name,path,line,member_count]"
"class","[Serial_Protocol,src/Serial_Protocol.h,24,17],[Button,src/buttons.h,31,12]"
"struct","[MpptState,src/SolarCharger.h,57,11]"
```

**FIND usages** — grouped by file:
```csv
"find_usages","encenderMotor",5
"file","[lines]"
"src/motor_control.cpp","45,89"
"include/motor_control.hpp","34"
```

**SHOW outline** — grouped by kind, comments compressed to `len:N`:
```csv
"show_outline","include/types.hpp"
"fql_kind","[name,line]"
"comment","[len:18,1],[len:23,55]"
"type_alias","[int16_t,17],[int32_t,18]"
```

**SHOW members** — grouped by kind:
```csv
"show_members","MotorControl","include/motor_control.hpp"
"type","[declaration,line]"
"field","[uint16_t rpm_setpoint;,28],[bool is_locked;,51]"
"method","[void setRPM(uint16_t);,35]"
```

**SHOW body / lines / context** — 2 columns (line, text):
```csv
"show_body","convertByte2Volts","src/adc.cpp","42-44"
"line","text"
42,"float convertByte2Volts(uint8_t raw) {"
43,"    return raw * 3.3f / 255.0f;"
44,"}"
```

**SHOW signature** — single flat row:
```csv
"show_signature","setPeakLevel","src/signal.cpp",125,"void setPeakLevel(int level)"
```

**SHOW callees** — grouped by file:
```csv
"show_callees","setPWMDuty"
"file","[name,line]"
"src/pwm_driver.cpp","[writePWM,189]"
"src/timer.cpp","[updateTimer,405]"
```

**FIND files** — 2 flat columns:
```csv
"find_files",142
"path","size"
"src/motor_control.cpp",12847
```

Mutations, transactions, and source ops keep their JSON format (already small).

