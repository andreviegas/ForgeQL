# Changelog fragments

One file per change, instead of everyone editing `CHANGELOG.md`.

## Why

Two changes in flight both want to describe themselves. If both edit the
changelog they collide on the same lines, and if both pick a version number
one of the two numbers is wrong — this project has already burned `0.152.0`
and renumbered twice that way. A fragment is a new file, so no two changes
ever touch the same one, and nobody has to know what anyone else is calling
their release.

## Writing one

Add a file here named `<short-slug>.md`. No front matter, no version, no
date — just the entry as it should read in the changelog:

    # changelog.d/column-range-once.md
    - `SHOW outline` no longer rebuilds a file's node table for every row.
      A 12,000-line file took 1.7 s to answer a two-row query; it now takes
      the time the two rows cost.

Write it for someone who does not have this repository's history in their
head: what was wrong, what changed, and what they will now observe. The same
rule as commit messages — no internal identifiers, no planning labels, no
references to documents that live outside this repository.

State the measured result if there is one, and say plainly what did **not**
move. A release note that implies a win it cannot support is worse than one
that claims nothing.

## What happens to it

At release the integrator assembles every fragment into a dated, versioned
section of `CHANGELOG.md`, bumps `Cargo.toml` and `Cargo.lock`, and deletes
the fragments in the same commit. Contributors never choose a version
number and never edit a version heading — the version depends on everything
released together, which only the integrator can see.
