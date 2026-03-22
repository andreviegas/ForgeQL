# Syntax Quick Reference

Condensed ForgeQL syntax. For full details see `doc/syntax.md`.

---

## Commands

### Session
```sql
USE source.branch [AS 'alias']
SHOW SOURCES
SHOW BRANCHES [OF 'source']
DISCONNECT
```

### FIND (returns rows)
```sql
FIND symbols [clauses]
FIND globals [clauses]
FIND usages OF 'name' [clauses]
FIND callees OF 'name' [clauses]
FIND files [clauses]
```

### SHOW (returns content)
```sql
SHOW body OF 'name' [DEPTH N] [clauses]
SHOW signature OF 'name' [clauses]
SHOW outline OF 'file' [clauses]
SHOW members OF 'type' [clauses]
SHOW context OF 'name' [clauses]
SHOW callees OF 'name' [clauses]
SHOW LINES n-m OF 'file' [clauses]
```

### CHANGE (modify code)
```sql
CHANGE FILE 'path' LINES n-m WITH 'text'
CHANGE FILE 'path' LINES n-m WITH NOTHING
CHANGE FILES 'glob1','glob2' MATCHING 'old' WITH 'new'
CHANGE FILE 'path' WITH 'full_content'
```

### Transactions
```sql
BEGIN TRANSACTION 'name'
  -- CHANGE / VERIFY commands
COMMIT MESSAGE 'msg'
VERIFY build 'step'
ROLLBACK [TRANSACTION 'name']
```

---

## Universal Clauses

Applied in this order: `IN → EXCLUDE → WHERE → GROUP BY → HAVING → ORDER BY → OFFSET → LIMIT`

```sql
IN 'glob/**'
EXCLUDE 'glob/**'
WHERE field operator value       -- repeatable (AND)
GROUP BY (file | kind | node_kind)
HAVING field operator value
ORDER BY field [ASC | DESC]
OFFSET N
LIMIT N
DEPTH N
```

### Operators
| Op | Meaning |
|---|---|
| `=` `!=` | Exact match |
| `LIKE` `NOT LIKE` | SQL wildcards: `%` any, `_` one char |
| `>` `>=` `<` `<=` | Numeric comparison |

---

## Filterable Fields

### Symbol Fields (FIND symbols, usages, callees)
| Field | Type | Description |
|---|---|---|
| `name` | string | Symbol name |
| `node_kind` | string | Tree-sitter kind |
| `fql_kind` | string | Universal kind (function, class, struct, …) |
| `language` | string | Language name (e.g. cpp) |
| `path` | string | Relative file path |
| `line` | integer | Start line (1-based) |
| `usages` | integer | Reference count |

### File Fields (FIND files)
| Field | Type | Description |
|---|---|---|
| `path` | string | Relative file path |
| `extension` | string | Without `.` |
| `size` | integer | Bytes |
| `depth` | integer | Directory depth |

### Dynamic Fields (auto-extracted, C/C++)
`type`, `value`, `parameters`, `declarator`

### Enrichment Fields

**Naming:** `naming` (camelCase/PascalCase/snake_case/UPPER_SNAKE), `name_length`

**Comments:** `comment_style` (doc_line/doc_block/block/line), `has_doc`

**Numbers:** `num_format` (`dec`/`hex`/`bin`/`oct`/`float`/`scientific`), `is_magic`, `num_value`, `num_suffix`, `suffix_meaning`

**Control Flow:** `condition_tests`, `paren_depth`, `has_catch_all`, `catch_all_kind`, `has_assignment_in_condition`, `mixed_logic`, `branch_count`, `for_style`

**Metrics:** `lines`, `param_count`, `return_count`, `goto_count`, `string_count`, `member_count`, `throw_count`, `is_const`, `is_volatile`, `is_static`, `is_inline`, `is_override`, `is_final`, `visibility`

**Operators:** `increment_style`, `compound_op`, `operator_category`, `shift_direction`, `shift_amount`

**Casts:** `cast_style`, `cast_target_type`, `cast_safety`

**Scope:** `scope`, `storage`, `binding_kind`, `is_exported`

**Members:** `body_symbol`, `member_kind`, `owner_kind`

**Redundancy:** `has_repeated_condition_calls`, `repeated_condition_calls`, `null_check_count`, `duplicate_condition`

---

## Common node_kind Values (C/C++)

`function_definition`, `declaration`, `struct_specifier`, `class_specifier`, `enum_specifier`, `preproc_def`, `preproc_include`, `field_declaration`, `parameter_declaration`, `comment`

---

## Known Limitations

| Issue | Workaround |
|---|---|
| Template functions show empty callees | Use `FIND usages OF 'name'` instead |
| Numeric comparisons skip hex/symbolic values | Use `WHERE value LIKE 'pattern'` instead |
| Escape sequences treated literally | Write actual newlines in string content |
| Angle brackets not preserved in includes | Use `WHERE name LIKE 'std%'` patterns |
