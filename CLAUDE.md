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

## Commit subjects are short — the detail goes below the fold

- **Keep the subject line under 60 characters.** Terminals and editor
  buffers truncate longer ones, and a subject that gets cut mid-word is
  a subject nobody reads. `git log --oneline` is the format these are
  actually read in.
- **Say what changed, in the imperative, without qualifying it.**
  `fix: one place decides whether shedding on rank is sound`, not
  `fix: make sure that only a single place is responsible for deciding
  whether it is sound to shed rows on rank`.
- **The body is for what the subject changes about a reader's
  decisions** — why the obvious fix was wrong, what a measurement
  showed, which invariant now holds. Skip it when there is nothing of
  that kind to say.
- **Reproduction detail and released wording belong in the changelog
  fragment, not the commit body.** The fragment is what ships to users;
  the commit body is what the next contributor reads before touching
  the same code. Writing the same paragraph in both means one of them
  goes stale.

## Versioning — contributors never pick a number

- **Do not touch `Cargo.toml`, `Cargo.lock`, or any version heading in
  `CHANGELOG.md`.** Choosing a version requires knowing everything that will
  ship alongside your change, which only the integrator can see. Two changes
  in flight that both pick a number mean one of them is wrong: this project
  has already burned `0.152.0` and renumbered twice that way.
- **Ship your entry as a fragment.** Add one new file to `changelog.d/`
  describing what changed, in the same self-contained public language the
  commit message uses. A fragment is a new file, so two changes in flight
  never collide on it. See `changelog.d/README.md`.
- **The integrator numbers every merge.** A change that reaches `main` gets a
  version there and then: its fragments are assembled into a dated section of
  `CHANGELOG.md`, the manifests are bumped, and the fragments are deleted, in
  a commit that follows the merge. Waiting for a release instead left ten
  engine changes all answering `0.158.0`, which made `SHOW VERSION` useless
  for telling one build from another — the check that has caught a stale
  binary here twice. Numbering at merge costs nothing and keeps that check
  working, and it does not reintroduce the collision above: merges are
  serialised through one person, so the order is never in doubt.
- **A version is not a release.** Numbering happens at merge; tagging and
  publishing are separate acts.
- Docs-only commits need neither a fragment nor a version bump.

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
