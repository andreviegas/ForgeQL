# Golden test suites (`golden_test`)

Data-driven golden tests for ForgeQL enrichment/query behaviour. Each `*.json` file
in this directory is a **suite** of cases. The runner
(`crates/forgeql/tests/golden_test.rs`) replays each case's query against a frozen
corpus and checks the result against its `assert` block.

## Running

```
# all suites / all cases
FORGEQL_DATA_DIR=/path/to/data cargo test --test golden_test

# one suite (group) — trial names are "<suite>::<case>"
cargo test --test golden_test enrich_is_magic

# one case
cargo test --test golden_test enrich_is_magic::cpp
```

`FORGEQL_DATA_DIR` must point at a ForgeQL data dir with the referenced sources
registered; without it the harness skips (exit 0).

## How it runs

- One MCP server is spawned per test process; `USE` is **memoized per `source.branch`**,
  so cases sharing a corpus pay the `USE` once. Read-only — no transactions.
- Each case is one libtest-mimic trial, run in parallel; a mutex serialises the shared
  server channel. Per-pid session aliases keep concurrent runs (multiple agents) isolated.
- Teardown is automatic: the server is killed and per-run worktrees removed when the run ends.

## Suite schema

```
{
  "suite": "<name>",                 // trial-name prefix
  "description": "<note>",           // ignored by the runner
  "cases": [
    {
      "name": "<case id>",           // trial = "<suite>::<name>"
      "use":  "<source>.<branch>",   // frozen corpus to query
      "fql":  "<ForgeQL query>",     // run verbatim
      "assert": { ... }
    }
  ]
}
```

Suites use the `.json` extension so they are indexed by ForgeQL and editable by node
handle (`CHANGE NODE` / `INSERT NODE`) rather than raw text.

## Assertion vocabulary

| Key | Checks |
|---|---|
| `row_count: N` | exactly N result rows |
| `total: N` | the query's `total` field == N (can exceed `row_count` under `LIMIT`) |
| `all_same: "F"` | every row shares the same value of field `F` |
| `ordered: {by:"F", dir:"asc"\|"desc"}` | rows monotonic by numeric field `F` |
| `distinct: {by:"F", count:N, values:[…]}` | N distinct values of `F`; optional exact set |
| `rows: [ {field:val, …}, … ]` | positional — row *i* matches these fields |
| `same_block: true` | all rows share one block handle |

`F` is a result field (`name`, `line`, `path`, …) or a derived node_id part (below).

## node_id parts (so tests never hard-code churnable ids)

A node_id is `n<sha>.<ordinal>(<offset>)`. Assertions reference its parts, which stay
stable across reindexing (ordinals do not):

| Field | From | Meaning |
|---|---|---|
| `_file` | `<sha>` | the file; falls back to `path` when a row has no node_id (e.g. number rows) |
| `_ordinal` | between `.` and `(` | stable identity slot — **not** source order |
| `_offset` | inside `(…)` | line offset within the node |
| `_block` | id minus `(offset)` | block handle (used by `same_block`) |

`ordered` rejects `by: "_ordinal"` — use `line` for source order.

## Adding a case

The suite file is indexed, so edit it by node handle:

```
SHOW outline OF 'tests/golden/<suite>.json' ALL    -- find the cases array / a case object
INSERT AFTER NODE '<case_object_id>' WITH '{ ... }' -- add a case (mind the trailing comma)
CHANGE NODE '<value_node_id>' WITH '...'            -- tweak one value
```

Capture expected values from a live query first (the corpus is frozen, so they are
stable), then run the single case to confirm.

## Mutation suites (planned)

Cases that exercise `CHANGE NODE` / `DELETE NODE` / `INSERT NODE` will run against a
throwaway **read-write** worktree branched off the frozen corpus and be discarded on
teardown, so the frozen branch is never modified. See `golden_test.rs` once that mode
lands.
