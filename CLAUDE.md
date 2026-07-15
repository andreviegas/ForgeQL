# ForgeQL — contributor & agent conventions

## Commit messages and CHANGELOG are public — self-contained language only

Commit messages and `CHANGELOG.md` are read by people who only see this
public repository. They must stand on their own:

- **Never reference internal tracker IDs or planning labels.** No
  `BUG-NNN`, no slice/step labels (`U1`, `S3`, `R2`, "Step 4",
  "residual"), no names of private planning documents. Those artifacts
  are not in this repository, so such references mean nothing to a
  reader here.
- Describe the observable problem and the fix in plain language: what
  was broken, why it was broken, and what changed. A reader with no
  prior context must understand the entry on its own.
- New code comments follow the same rule: state the invariant or the
  reason, never a ticket number.

## Versioning

- The `Cargo.toml`/`Cargo.lock` version bump and the matching
  `CHANGELOG.md` section ship in the same commit as the change itself —
  never as a separate version-bump commit.
- Docs-only commits do not need a version bump.

## Workflow for agents editing this repository

- Edit indexed source through ForgeQL itself (`run_fql`): locate nodes
  with FIND/SHOW, mutate with CHANGE NODE / INSERT AFTER NODE /
  DELETE NODE, and commit through the DSL's commit statement.
- Run the full test gate (`JOB START 'test-all-before-commit'`) and the
  forgeql-guardian review before every commit.
- Merges to `main` are fast-forward only (`git merge --ff-only`).

## Verifying a change to ForgeQL itself

**A green test gate does not mean your change to index output actually
runs.** Two independent mechanisms produce false greens, and both have
already shipped dead features:

| Check | Why it lies |
|---|---|
| Unit tests | They index fresh tempdir snippets, so they pass *honestly* while the feature is dead in the real engine. They also tend to assert **config** ("does `json.json` declare a block group?") rather than **behaviour** ("does a block row get emitted?"). |
| Corpus golden suites | They are served from the columnar segment cache. Without an `ENRICH_VER` bump they read **pre-change segments** and report the OLD numbers as ✓. |
| forgeql-guardian | Reviews principles, not behaviour. |

The only check that asks the engine a real question about a real file is
to **drive the freshly built binary**:

```sql
JOB START 'build'          -- build the debug binary for THIS worktree; poll JOB STATUS
RUN 'run_fql' 'USE wt.main AS "st"
FIND symbols WHERE fql_kind = "array_block" LIMIT 5
SHOW outline OF "path/to/file.yml" ALL'
```

- The `run_fql` RUN template pipes an FQL script into the debug binary
  with an isolated data dir; the `wt` source registered there is a
  ForgeQL repo snapshot — no `CREATE SOURCE` needed.
- Newlines separate statements inside the single-quoted script; use
  **double quotes** for inner string literals.
- The CLI behind it is **line-based**: heredocs and multi-line `WITH`
  bodies do **not** work through the pipe. For those, drive a throwaway
  MCP session instead (`USE src.branch AS 'probe'`).

This is not hypothetical: the `array_block` kind (0.109.x) shipped
completely dead — green gate, clean review, passing test — because the
block scanner walked `next_sibling()` and JSON's `,` separators
(anonymous siblings) broke every run at the first comma. One
`RUN 'run_fql'` query found it.

**`ENRICH_VER` (`storage/columnar/mod.rs`) must be bumped on EVERY
iteration** of an index-output change, not once per feature — a v(N)
cache built from an earlier draft of *your own* change is exactly as
stale as a v(N-1) one. Missing it is invisible and **looks like
success**. Triggers: `extract_name`, `map_kind`, `kind_map` /
`block_groups` config, `process_node_rows`, `collect_nodes`,
`emit_*_row`, `is_addressable_fql_kind`, ordinal assignment,
`OrdinalRemapper`, any enricher, any new `fql_kind`. Not triggers
(nothing stored changes): parser/DSL verbs, clause filtering,
`compact.rs`, result structs, SHOW MORE, git plumbing, docs.

After bumping, confirm the corpus numbers actually **moved**. If they
didn't, you are still reading a stale cache.
