# ForgeQL Instructions for Claude Code

All source code is accessed exclusively through the ForgeQL MCP server.
The local workspace may be empty — never fall back to local filesystem tools.

## Critical Rules

1. Always start with `USE source.branch` before any query.
2. Never use Bash tools (grep, find, cat, less) for code reading. ForgeQL manages all code access.
3. Never brute-force read code. Use FIND to locate symbols, then SHOW LINES for exact ranges.
4. SHOW commands without LIMIT are blocked beyond 40 lines. Follow the guidance message.

## Query Workflow

The right way to find and read code:

```
1. FIND symbols WHERE ... → get name, file, line number
2. SHOW LINES n-m OF 'file' → read only those lines
```

## Query Strategy

- Find a symbol: `FIND symbols WHERE name LIKE 'pattern' [WHERE fql_kind = '...']`
- Read specific lines: `SHOW LINES n-m OF 'file'`
- Function signature: `SHOW body OF 'name' DEPTH 0`
- Control flow: `SHOW body OF 'name' DEPTH 1`
- Blast radius: `FIND usages OF 'name' GROUP BY file ORDER BY count DESC`
- File structure: `SHOW outline OF 'file'`
- Class members: `SHOW members OF 'type'`
- Call graph: `SHOW callees OF 'name'`
- File listing: `FIND files [IN 'path/**'] [WHERE extension = '...']`

## Efficiency

- All commands accept WHERE, GROUP BY, ORDER BY, LIMIT, OFFSET — combine freely.
- Multiple WHERE clauses combine as AND — stack them to narrow results.
- FIND defaults to 20 rows. Add LIMIT N for more.
- SHOW body defaults to DEPTH 0 (signature only). Increment progressively.
- Format defaults to CSV (~60% fewer tokens).

## Enrichment Fields

Use these in WHERE clauses for code quality analysis:

- `is_magic = 'true'` — magic numbers
- `has_assignment_in_condition = 'true'` — assignment in condition
- `mixed_logic = 'true'` — mixed && / || without grouping
- `condition_tests >= 4` — complex conditions
- `has_catch_all = 'false'` — switch without default
- `goto_count >= 1` — functions with goto
- `lines >= 50` — large functions
- `usages = 0` — dead code candidates
- `has_doc = 'false'` — undocumented functions
- `naming` — camelCase, PascalCase, snake_case, UPPER_SNAKE
