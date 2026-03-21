# Query Strategy Reference

Decision tree and anti-patterns for ForgeQL queries.

---

## Decision Tree

```
What do you need?
│
├─ A symbol's location?
│  → FIND symbols WHERE name LIKE 'pattern' [WHERE node_kind = '...']
│  → Result gives you: name, path, line, usages
│
├─ Source code at a known location?
│  → SHOW LINES n-m OF 'file'
│
├─ A function's interface?
│  → SHOW body OF 'name' DEPTH 0          (signature only)
│  → SHOW body OF 'name' DEPTH 1          (+ control flow)
│  → SHOW body OF 'name' DEPTH 99 LIMIT N (full, only if needed)
│
├─ Who uses a symbol?
│  → FIND usages OF 'name' GROUP BY file ORDER BY count DESC
│
├─ What a function calls?
│  → SHOW callees OF 'name'
│
├─ File structure / outline?
│  → SHOW outline OF 'file' [WHERE kind = '...']
│
├─ Class/struct members?
│  → SHOW members OF 'type' [WHERE kind = '...']
│
├─ Context around a definition?
│  → SHOW context OF 'name'
│
├─ File listing / exploration?
│  → FIND files [IN 'path/**'] [WHERE extension = '...']
│
└─ Modify code?
   → BEGIN TRANSACTION 'name'
     CHANGE FILE ... LINES n-m WITH '...'
     VERIFY build 'step'
   COMMIT MESSAGE '...'
```

---

## Anti-Patterns

| Never do this | Do this instead | Why |
|---|---|---|
| `SHOW body OF 'func' DEPTH 99` without LIMIT | `FIND symbols WHERE name = 'func'` → `SHOW LINES n-m OF 'file'` | Large bodies get blocked; FIND gives exact location |
| `SHOW LINES 1-500 OF 'file'` | `SHOW outline OF 'file'` → `SHOW LINES n-m` for specific symbols | Scanning whole files wastes tokens |
| `FIND symbols` (unfiltered) | `FIND symbols WHERE node_kind = '...' WHERE name LIKE '...'` | Unfiltered queries hit the 20-row default cap on large codebases |
| Paginating with OFFSET to read all results | Add more WHERE filters to narrow results | Pagination reads everything; filters find what you need |
| Using grep/find/cat/read_file | Use ForgeQL FIND and SHOW commands | Local workspace may be empty; ForgeQL has the indexed code |
| `GROUP BY` without `HAVING` | `GROUP BY file HAVING count >= N` | Ungrouped results on large codebases produce too many rows |

---

## Narrowing Results

Stack multiple `WHERE` clauses (implicit AND):

```sql
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE name LIKE '%init%'
  WHERE usages >= 5
  IN 'src/**'
  EXCLUDE 'vendor/**'
  ORDER BY usages DESC
  LIMIT 10
```

---

## Two-Step Code Reading Pattern

This is the core workflow — memorize it:

```sql
-- Step 1: Locate the symbol
FIND symbols WHERE name = 'targetFunction'
-- Result: name=targetFunction, path=src/module.cpp, line=142, usages=7

-- Step 2: Read exactly those lines
SHOW LINES 142-160 OF 'src/module.cpp'
```

If you don't know the exact name, use `LIKE`:

```sql
FIND symbols WHERE name LIKE '%target%' WHERE node_kind = 'function_definition'
```

If you need to understand structure before reading:

```sql
SHOW body OF 'targetFunction' DEPTH 0    -- signature only
SHOW body OF 'targetFunction' DEPTH 1    -- control flow skeleton
-- Now you know which section to read
SHOW LINES 155-160 OF 'src/module.cpp'   -- just the relevant section
```
