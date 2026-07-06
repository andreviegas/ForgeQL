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
