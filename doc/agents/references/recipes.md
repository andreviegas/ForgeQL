# Workflow Recipes

Complete FQL sequences for common tasks. Each recipe shows the exact commands to run in order.

---

## Dead Code Detection

Find unreferenced functions and macros — candidates for removal.

```sql
-- Unreferenced functions (skip test files)
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE usages = 0
  EXCLUDE 'tests/**'
  ORDER BY path ASC
  LIMIT 30

-- Unreferenced macros in headers
FIND symbols
  WHERE node_kind = 'preproc_def'
  WHERE usages = 0
  IN 'include/**'
  ORDER BY path ASC

-- For each candidate, verify it's safe to remove:
SHOW context OF 'functionName'
-- Then check if it's an entry point or callback (may have 0 indexed usages but be used externally)
```

---

## Rename / Refactor Symbol

Safe rename with blast radius analysis and atomic transaction.

```sql
-- Step 1: Find the symbol
FIND symbols WHERE name = 'oldFunction'

-- Step 2: Blast radius — which files and how many references?
FIND usages OF 'oldFunction' GROUP BY file ORDER BY count DESC

-- Step 3: Inspect context if needed
SHOW body OF 'oldFunction' DEPTH 1

-- Step 4: Atomic rename with verification
BEGIN TRANSACTION 'rename-oldFunction'
  CHANGE FILES 'src/**/*.cpp', 'include/**/*.h' MATCHING 'oldFunction' WITH 'newFunction'
  VERIFY build 'test'
COMMIT MESSAGE 'refactor: rename oldFunction → newFunction'
```

---

## Code Smell Audit

Detect common code quality issues using enrichment fields.

```sql
-- Magic numbers (unexplained constants)
FIND symbols WHERE is_magic = 'true' ORDER BY path ASC LIMIT 30

-- Assignment in condition (likely bug)
FIND symbols WHERE has_assignment_in_condition = 'true'

-- Mixed && / || without grouping parentheses
FIND symbols WHERE mixed_logic = 'true'

-- Complex conditions (4+ sub-tests)
FIND symbols WHERE condition_tests >= 4

-- Switch without default
FIND symbols
  WHERE node_kind = 'switch_statement'
  WHERE has_default = 'false'

-- Functions with goto
FIND symbols WHERE goto_count >= 1

-- Duplicated conditions within same function
FIND symbols WHERE duplicate_condition = 'true'

-- C-style casts (modernization targets)
FIND symbols WHERE cast_style = 'c_style'

-- Repeated conditional calls (extract-variable opportunity)
FIND symbols WHERE has_repeated_condition_calls = 'true'
```

---

## Non-Compliance Audit

Check naming conventions, documentation, and function sizing.

```sql
-- Functions not following snake_case
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE naming != 'snake_case'
  WHERE naming != 'PascalCase'
  EXCLUDE 'vendor/**'
  ORDER BY name ASC

-- Undocumented public functions (no preceding doc comment)
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE has_doc = 'false'
  IN 'src/**'
  ORDER BY path ASC
  LIMIT 30

-- Large functions (> 50 lines, refactoring candidates)
FIND symbols
  WHERE node_kind = 'function_definition'
  WHERE lines >= 50
  ORDER BY lines DESC
  LIMIT 20

-- Very long symbol names (> 40 chars)
FIND symbols
  WHERE name_length > 40
  ORDER BY name_length DESC
  LIMIT 10
```

---

## High-Coupling Hotspot Analysis

Find the most-referenced symbols — high coupling = high risk.

```sql
-- Top 10 most-referenced functions
FIND symbols
  WHERE node_kind = 'function_definition'
  ORDER BY usages DESC
  LIMIT 10

-- Symbol distribution per file (spot bloated files)
FIND symbols
  GROUP BY file
  HAVING count >= 20
  ORDER BY count DESC

-- Usage heat-map for a specific symbol
FIND usages OF 'TargetSymbol'
  GROUP BY file
  ORDER BY count DESC
```

---

## File Structure Exploration

Navigate the codebase structure.

```sql
-- All source files
FIND files IN 'src/**' WHERE extension = 'cpp' ORDER BY size DESC

-- Largest files (might need splitting)
FIND files WHERE size > 50000 ORDER BY size DESC LIMIT 10

-- Directory overview (depth-limited)
FIND files DEPTH 1

-- File outline (all top-level symbols)
SHOW outline OF 'src/module.cpp'

-- Only functions in a file
SHOW outline OF 'src/module.cpp' WHERE kind = 'function_definition' ORDER BY line ASC

-- Class member overview
SHOW members OF 'ClassName'
SHOW members OF 'ClassName' WHERE kind = 'field_declaration'
```

---

## Bug Fix Workflow

End-to-end bug fix with transaction safety.

```sql
-- 1. Locate the bug
FIND symbols WHERE name = 'buggyFunction'
SHOW body OF 'buggyFunction' DEPTH 1

-- 2. Check blast radius
FIND usages OF 'buggyFunction' GROUP BY file

-- 3. Trace dependencies
SHOW callees OF 'buggyFunction'

-- 4. Fix atomically
BEGIN TRANSACTION 'fix-buggyFunction'
  CHANGE FILE 'src/module.cpp' LINES 45-67 WITH 'fixed code...'
  VERIFY build 'test'
COMMIT MESSAGE 'fix: handle edge case in buggyFunction'

-- 5. If verification fails:
ROLLBACK
```

---

## Version/Config Update

Multi-file coordinated change.

```sql
BEGIN TRANSACTION 'bump-version'
  CHANGE FILE 'include/version.h' MATCHING 'VERSION "1.0"' WITH 'VERSION "1.1"'
  CHANGE FILE 'src/config.cpp' MATCHING 'VERSION_STR = "1.0"' WITH 'VERSION_STR = "1.1"'
  VERIFY build 'test'
COMMIT MESSAGE 'bump version to 1.1'
```
