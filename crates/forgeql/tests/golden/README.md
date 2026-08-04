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

- A **pool** of MCP servers is spawned per test process; `USE` is **memoized per
  `source.branch` per pool member**, so cases sharing a corpus pay the `USE` once each.
  Read-only — no transactions.
- Pool engines are spawned with `FORGEQL_ALLOW_CHANGE_FILE_INDEXED` **removed**. The
  pre-commit gate exports it for the legacy raw-text phase, and inheriting it would give
  the suites a different `CHANGE FILE` contract under the gate than standalone.
- Each case is one libtest-mimic trial, run in parallel; trials are assigned round-robin
  across the pool and a per-member mutex serialises that member's channel. Session aliases
  carry the pid and the pool index, so neither concurrent runs (multiple agents) nor two
  pool members can collide on one worktree.
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

## Enforcement

Every case may carry an optional `enforcement` key. Omit it for the default.

| Value | Behaviour |
|---|---|
| `"hard"` (or absent) | A failure fails the gate. |
| `"soft"` | The case runs; a failure is printed verbosely after the run and does **not** gate — for pending behaviour you want visible without going red. |
| `"ignore"` | The case is skipped entirely (shown as ignored). |
| `"expect_fail"` | The case asserts behaviour that is **correct but does not hold yet**. A failure is the expected state and does not gate; a **pass fails the gate**. |

`"expect_fail"` additionally requires `expect_fail_reason` — one sentence on why
the asserted behaviour does not hold. It is printed beside each expected failure
and quoted back in the promotion message; an empty one fails the case.

```
{ "name": "elif_negation_accumulates",
  "use": "zephyr-andre.frozen",
  "enforcement": "expect_fail",
  "expect_fail_reason": "#elif arms do not accumulate the negations of preceding arms",
  "fql": "…", "assert": { …correct values… } }
```

The asymmetry is the point. It lets a defect's *correct* behaviour be pinned the
day the defect is diagnosed rather than the day it is fixed, and because an
unexpected pass is hard, the change that fixes the defect is forced to promote
exactly the cases it fixed, in the same commit where they are reviewable. The run
ends with a count of expected failures, so the open defect inventory is visible in
every test run.

One rot to watch: an `expect_fail` case whose asserted value is itself wrong stays
"expected failure" forever and never announces itself. When a fix lands, re-check
that the cases still marked `expect_fail` fail for the reason they state, not for
a new one.

The second rot mode is environmental, and it inverts the usual reading of a
result. An `expect_fail` case counts *any* error as its expected failure,
including "this corpus is not registered here". So in a data dir that is set but
missing a suite's corpus, the hard cases of that suite go red while every
`expect_fail` case in it passes — vacuously. Read a run where the hard cases fail
and the expected failures all "pass" as a missing corpus, not as progress.

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

`total` is read from the top level of the result and, failing that, from inside
`content` — `FIND symbols` reports it in the first place, `FIND files` in the
second. The two also differ in meaning: `FIND files` counts every match, so it
exceeds `row_count` under `LIMIT`, while `FIND symbols` reports the capped count.

`F` is a result field (`name`, `line`, `path`, …) or a derived node_id part (below).

## node_id parts (so tests never hard-code churnable ids)

A node_id is `n<sha>.<ordinal>(<offset>)`. Assertions reference its parts, which stay
stable across reindexing (ordinals do not):

| Field | From | Meaning |
|---|---|---|
| `_file` | `<sha>` | the file; falls back to `path` only on rows with no node_id — see the caveat below |
| `_ordinal` | between `.` and `(` | stable identity slot — **not** source order |
| `_offset` | inside `(…)` | line offset within the node |
| `_block` | id minus `(offset)` | block handle (used by `same_block`) |

`ordered` rejects `by: "_ordinal"` — use `line` for source order.

`all_same`, `ordered` and `distinct` name a field on the **JSON** result row, not
the CSV column header. The row's kind column is `kind` there — `fql_kind` is the
CSV spelling and reads back as null, which silently collapses `distinct` to a
single value and makes the assertion hold for anything. Any `by`/`all_same` name
that is not a derived `_` key should be checked against a real JSON row
(`format=JSON`) before it is trusted.

**`_file` derives the node hex on any row that carries a `node_id`, and only
falls back to `path` on rows that do not.** Which side of that fallback a row
lands on is a property of the engine, not of the assertion, and it moves: a
`FIND usages` row carried no handle until it gained its file's, and now takes
the hex branch. A golden written as `distinct: { by: "_file", values: [...] }`
with path strings therefore passes vacuously one release and fails the next for
a reason unrelated to what it tests. **Assert `path` directly** — it is always
present and always means the file. Reserve `_file` for cases whose subject is
node identity itself.

## Adding a case

The suite file is indexed, so edit it by node handle:

```
SHOW outline OF 'tests/golden/<suite>.json' ALL    -- find the cases array / a case object
INSERT AFTER NODE '<case_object_id>' WITH '{ ... }' -- add a case (mind the trailing comma)
CHANGE NODE '<value_node_id>' WITH '...'            -- tweak one value
```

Capture expected values from a live query first (the corpus is frozen, so they are
stable), then run the single case to confirm.

## Mutation suites (`DELETE NODE` / `CHANGE NODE` / transactions)

Set `"mode": "rw"` on a case to run it in a fresh **read-write** worktree branched off
the corpus (discarded on teardown — the frozen branch is never modified). Such cases use
`steps` instead of a single `fql`:

```
{
  "name": "delete_and_rollback",
  "use": "forgeql-pub.frozen",
  "mode": "rw",
  "steps": [
    { "fql": "FIND symbols IN '<file>' WHERE name='foo' LIMIT 1",
      "assert": { "row_count": 1 }, "capture": { "A": "results.0.node_id" } },
    { "fql": "BEGIN TRANSACTION 'txn'", "assert": { "field": { "name": "txn" } } },
    { "fql": "DELETE NODE '${A}'", "assert": { "applied": true } },
    { "fql": "FIND symbols IN '<file>' WHERE name='foo' LIMIT 1", "assert": { "row_count": 0 } },
    { "fql": "ROLLBACK", "assert": { "field": { "name": "txn" } } },
    { "fql": "FIND symbols IN '<file>' WHERE name='foo' LIMIT 1", "assert": { "row_count": 1 } }
  ]
}
```

- `steps` run in order in one session. `capture` pulls a value (a dotted path into the
  step result, e.g. `results.0.node_id`) into a `${var}` substituted in later steps — so
  node_ids are resolved at runtime, never hard-coded.
- Result-step asserts: `applied`, `diff_contains`, `files_changed`, `field` (top-level
  equality, e.g. a rollback's `name`), `pointer` (JSON-pointer), and `error: true` (the
  step is expected to fail, e.g. `ROLLBACK` with no open transaction).
- **`error: true` alone accepts *any* failure**, including a typo in the query — it says
  the step failed, not why. Pair it with `error_contains: "<substring of the message>"`
  whenever the case exists to pin one specific refusal, or the case passes for reasons
  it was never written to cover.

**Nested transactions** are just more steps: each `BEGIN` pushes a checkpoint stack, a
bare `ROLLBACK` pops the innermost, and `ROLLBACK 'name'` pops to that level. See the
`node_mutations` suite for `DELETE`/`CHANGE NODE` + nested-rollback examples.

## Compound probe suites (`probes_*.json`)

The `enrich_*.json` suites pin **one field at a time** on small fixtures — they localise a
regression to a single enricher. Probe suites do the opposite job: each case stacks many
clauses and enrichment fields into **one query over a frozen real-world corpus**, narrowed
to 1-4 rows, then chains `SHOW` steps that address the located node and pin its inner
structure.

```
FIND symbols WHERE fql_kind = 'function'
  WHERE lines >= 100 WHERE lines <= 150      -- metrics enricher
  WHERE is_recursive = 'true'                -- recursion enricher
  WHERE has_escape = 'true'                  -- escape enricher
  WHERE naming = 'snake_case'                -- naming enricher
  IN 'subsys/**'                             -- glob expansion
                                             -- => exactly 1 row
  -> capture node_id, then
SHOW outline OF '${FN}' WHERE fql_kind = 'if' WHERE depth = 2   -- nested-if structure
SHOW NODE '${FN}' WHERE text LIKE '%goto unlock%'               -- line filter + offsets
```

One case therefore crosses the parser, clause filtering, glob expansion, the columnar scan,
four enrichers, node addressing, subtree outline and line-level filtering. A change to any of
those paths moves a pinned value and the case fails.

That breadth is the point, and it is also the trade-off: **a probe tells you something moved,
not which enricher moved it.** Localisation is the `enrich_*` suites' job. Probes are the
sensitivity layer — they are what catches an index-output change that a fresh-tempdir unit
test and a stale segment cache would both report as green.

Probes deliberately pin `total`/`row_count` as well as row contents, so an **additive** change
(a new `fql_kind`, newly indexed rows) trips them too. That is a feature: such a change should
be reviewed and the pins updated on purpose, never absorbed silently.

Probes are read-only and share the memoized per-corpus session, so they cost **no worktrees**.
Never hard-code a `node_id` in a probe — `capture` it from step 1 and interpolate `${VAR}`, so
the case survives ordinal churn.

### Writing a probe: two traps

**Do not pin `total` on a query that carries an explicit `LIMIT`.** An explicit `LIMIT`
truncates the reported `total` (`LIMIT 3` on a 30-row match reports `total: 3`), so a `total`
pin there asserts nothing and would mask the row set growing. Pin `total` only on unlimited
queries — where it is the true match count and therefore a real tripwire. `probes_pytorch`'s
`GROUP BY` case pins `row_count` and rows only, for exactly this reason.

**Enrichers are not uniform across languages, so calibrate per corpus rather than porting a
predicate stack.** Known asymmetries the suites depend on:

| Field | Note |
|---|---|
| `naming` | Not emitted on Python function rows (it does fire on e.g. CMake rows) |
| `has_escape` | C/C++ only — in pytorch it matches `torch/csrc/**`, never the `.py` tree |
| `dup_logic` | No Python matches in the frozen pytorch corpus |

That is why the Python cases carry `EXCLUDE 'torch/csrc/**'`: without it a "Python" probe
silently drifts onto pytorch's C++ sources.

Pairing a control-flow field with the wrong `fql_kind` is the other silent failure —
`condition_tests`, `mixed_logic`, `paren_depth` and `dup_logic` live on `if`/`while`/`for`/`do`
rows, **not** on `function`. `WHERE fql_kind = 'function' WHERE condition_tests >= 4` returns
zero rows rather than erroring.
