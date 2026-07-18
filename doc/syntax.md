# ForgeQL Syntax Reference

Authoritative grammar for every ForgeQL command and clause.
Optimized for AI agent consumption — syntax first, advanced patterns second.

---

## Table of Contents

1. [Notation](#notation)
2. [Command Syntax](#command-syntax)
   - [Session Commands](#session-commands)
   - [Maintenance Commands](#maintenance-commands)
   - [FIND Commands](#find-commands)
   - [SHOW Commands](#show-commands)
   - [Editing Commands (node handles)](#editing-commands-node-handles)
   - [FOUND — mutating a whole FIND result](#found--mutating-a-whole-find-result)
   - [Node Addressing](#node-addressing)
   - [Mutation Responses — the diff is the contract](#mutation-responses--the-diff-is-the-contract)
   - [UNDO](#undo)
   - [Transaction Commands](#transaction-commands)
   - [VERIFY, RUN, and Background JOBs](#verify-run-and-background-jobs)
   - [EXPORT PATCH](#export-patch)
   - [SHOW DIFF](#show-diff)
3. [Universal Clauses](#universal-clauses)
4. [Operators and Values](#operators-and-values)
5. [Filterable Fields](#filterable-fields)
   - [Symbol Fields](#symbol-fields)
   - [Outline Fields](#outline-fields)
   - [Member Fields](#member-fields)
   - [File Fields](#file-fields)
   - [Dynamic Fields](#dynamic-fields)
   - [Enrichment Fields](#enrichment-fields)
6. [Structured-Text and Config Formats](#structured-text-and-config-formats)
   - [Block Grouping — one handle over a run of siblings](#block-grouping--one-handle-over-a-run-of-siblings)
   - [Syntax Damage — the `error` kind](#syntax-damage--the-error-kind)
7. [Advanced Patterns](#advanced-patterns)
8. [Raw line and file operations (legacy, non-indexed files)](#raw-line-and-file-operations-legacy-non-indexed-files)

---

## Notation

| Symbol | Meaning |
|---|---|
| `UPPERCASE` | Keyword — write exactly as shown |
| `'string'` or `"string"` | String literal — single or double quotes |
| `N` | Integer literal |
| `n-m` | Inclusive line range, e.g. `10-25` |
| `[ … ]` | Optional element |
| `( A \| B )` | Choose one |
| `…` | Repeatable |

---

## Command Syntax

### Session Commands

```sql
CREATE SOURCE 'name' FROM 'url'

REFRESH SOURCE 'name'

USE source_name.branch AS 'alias'
USE source_name.<commit-hash> AS 'alias'   -- base the session on an immutable commit (7-40 hex chars)

SHOW SOURCES

SHOW BRANCHES
SHOW COMMITS [clauses]    -- this session's commits since its base (newest first)

SHOW VERSION
```

`USE` bases a session on a branch head or, with a 7-40 character hex token in the
branch position, directly on a commit: a local branch of that name is used if one
exists, otherwise the token is resolved as a commit. Every `USE` response reports
the `base_commit` it resolved to (the full hash), so a session can be handed to
another agent by hex and that agent can confirm the exact commit it based on.

`source_name` is an unquoted identifier that may contain hyphens (e.g. `pisco-code`).
`branch` is an unquoted identifier that may contain hyphens (e.g. `main`, `v1_3_0`, `line-budget`).
`alias` is your session's worktree name — single/double-quoted or bare (unquoted).
**The `USE` response returns an opaque `session_id` token** (a composite of user,
source, branch, and alias). Store it exactly as returned and pass it verbatim in
every subsequent `run_fql` call — do not reconstruct it from the alias. If you
lose it, re-issue the same `USE` command with the same alias: you reconnect to
the same worktree and receive the same token.

Sessions start automatically on the first `USE` and persist until the worktree has been
idle — after about 2 hours if it carries no work (no commits over its base and no uncommitted changes), or 48 hours if it does (server-side TTL). There is no explicit disconnect command — multiple
agents can reconnect to the same worktree at any time with the same `USE` command.

Worktree identity uses a composite key: filesystem directory =
`{user}/{source}.{branch}.{alias}`, git branch = `fql/{user}/{source}/{branch}/{alias}`
(under the `fql/` namespace).

`SHOW VERSION` reports the crate version compiled into the running binary (e.g.
`0.114.0`). It reads no source and needs no active session, so it is the quickest
way for an agent to confirm which build of the engine is answering its queries.

---

### Maintenance Commands

```sql
VACUUM [SOURCE 'name'] [KEEP n] [ALL] [APPLY]
```

`VACUUM` reclaims disk space by deleting stale columnar-cache version
directories (`<provider>-v<N>` folders under `forgeql/overlays` and
`forgeql/segments`) that accumulate every time the enrichment version bumps. It
**previews by default** — reporting the in-scope directories grouped into
kept/deleted with per-directory sizes and the total reclaimable — and removes
nothing unless `APPLY` is given.

- Classification keys purely on the parsed `<N>` versus the current enrichment
  version, ignoring the provider prefix (so `git-sha256-v20` is treated exactly
  like `git-sha1-v20`).
- By default only versions **older** than the current one are removed; the
  current version and any newer ones are kept. `KEEP n` retains the `n` newest
  older versions; `ALL` removes every version including the current one (forcing
  a re-index). With no `SOURCE` the command spans every registered source.
- Like `CREATE SOURCE` / `REFRESH SOURCE`, `VACUUM` is admin-only: blocked over
  MCP, admin-token over HTTP. The CLI wrapper is
  `forgeql gc [--source NAME] [--keep N] [--all] [--yes]`, which previews,
  prompts for confirmation, then applies.

---

### FIND Commands

```sql
FIND symbols [clauses]

FIND globals [clauses]

FIND usages OF 'symbol_name' [clauses]

FIND callees OF 'symbol_name' [clauses]

FIND files [clauses]
```

**clauses**: see [Universal Clauses](#universal-clauses).

| Command | Returns |
|---|---|
| `FIND symbols` | All indexed AST nodes. Use `WHERE fql_kind = '...'` to narrow. Every row carries a stable `node_id` and a real workspace-total `usages` count — `ORDER BY usages DESC` and `WHERE usages > N` work. |
| `FIND globals` | Shorthand for `WHERE fql_kind = 'variable'` — file-scope variables, constants, and statics across all supported languages. |
| `FIND usages OF` | One row per usage **site** of the named symbol (name + path + line), read from usage postings collected at index time. Includes occurrences without call parentheses — function-pointer assignments, references, type positions. `GROUP BY file` gives real per-file counts; combine with `IN`/`EXCLUDE`/`WHERE`/`ORDER BY`/`LIMIT`. |
| `FIND callees OF` | Symbols called from inside the named function body. Alias for `SHOW callees OF`. |
| `FIND files` | Files in the worktree. Supports `WHERE name = '…'` / `name LIKE`, `DEPTH`, `ORDER BY size`, etc. ForgeQL runtime artifacts are hidden from the listing. |

> **Use `fql_kind` for all filtering.** It is language-agnostic and portable across C++, Rust, and any future language. Raw `node_kind` values (tree-sitter grammar names) are language-specific and **deprecated**.

Every `FIND` also **arms `FOUND`** — the set its rows describe — and a complete result carries a
`found_rev` row: the master rev that gates a bulk mutation over that set. See
[FOUND — mutating a whole FIND result](#found--mutating-a-whole-find-result).

---

### SHOW Commands

```sql
SHOW body OF 'symbol_name' [DEPTH N] [clauses]

SHOW signature OF 'symbol_name' [clauses]

SHOW outline OF 'file_path' [ALL] [clauses]
SHOW outline OF '<node_id>' [ALL] [clauses]

SHOW members OF 'type_name' [clauses]

SHOW context OF 'symbol_name' [clauses]

SHOW callees OF 'symbol_name' [clauses]

SHOW NODE '<node_id>' [CONTENT | METADATA] [clauses]

SHOW MORE [LAST-k] [HEAD n | TAIL n | n-m] [clauses]
```
| Command | Returns |
|---|---|
| `SHOW body OF` | Source text of a symbol. **Default `DEPTH 0`**: signature only, body replaced by `{ ... }`. `DEPTH 1`+: progressively reveals nested structure. `DEPTH 99`: full source. In CSV output the first column is a **node-relative 1-based `off`set** (not an absolute line) and the node's id is in the header — so you can `CHANGE NODE '<id>'` straight from the read. Absolute line numbers are available in `format=JSON`. |
| `SHOW signature OF` | Declaration line only (return type, name, parameters). |
| `SHOW outline OF` | Structural tree of a file. By default lists only **structural declarations** (functions, classes, structs, enums, traits, unions, namespaces, modules, type aliases, macros); each entry carries a **`depth`** so the compact output reads as an indented tree in source order. `ALL` includes every node; a `WHERE fql_kind = '...'` predicate implies `ALL`. Passing a `<node_id>` instead of a file path scopes the outline to that node's subtree. Supports `ORDER BY`, `LIMIT`, `OFFSET`. |
| `SHOW members OF` | Member declarations of a class/struct/enum: fields, methods, enumerators. Every row carries its `node_id` **and `rev`**, so a member is mutable where you read it. Supports `WHERE fql_kind = '...'`, `ORDER BY`, `LIMIT`, `OFFSET`. |
| `SHOW context OF` | Surrounding lines of a symbol definition. `DEPTH N` controls how many context lines (default 5). |
| `SHOW callees OF` | All symbols called from inside the named function body. |
| `SHOW NODE '<id>'` | `CONTENT` (default) prints the node's source; `METADATA` returns its `FIND NODE` record. A node-relative line offset — `'<id>(n)'` or `'<id>(n-m)'` — narrows `CONTENT` to a single line or inclusive range within the node's own span (1-based). |
| `SHOW MORE` | Pages the session's last buffered output. When a command's output is too large to return inline (e.g. `VERIFY build`), ForgeQL returns a window and buffers the full output; `SHOW MORE` retrieves the rest without re-running the command. |

Every `SHOW` response surfaces each result's `node_id` (and the CSV `off` column is node-relative), so you can chain directly into `CHANGE NODE` without re-reading.

#### SHOW MORE — paged output buffer

Any command whose output exceeds its inline cap is windowed inline and the full
output is buffered server-side (per session). Retrieve the remainder with:

```sql
SHOW MORE                -- the whole buffered output
SHOW MORE HEAD 40        -- the first 40 lines
SHOW MORE TAIL 40        -- the last 40 lines
SHOW MORE 120-240        -- an explicit 1-based inclusive line range
SHOW MORE WHERE text MATCHES 'error|fail'   -- grep the buffer (regex)
SHOW MORE TAIL 80 WHERE text LIKE '%warning%' LIMIT 10
```

Every window form composes with `WHERE text` (`MATCHES` regex or `LIKE`) and
`LIMIT`/`OFFSET`; filtering runs over the windowed lines. Each returned line
keeps its **original buffer index** so a precise follow-up range can be
requested. The buffer is a `LAST-n` ring (5 slots): a bare `SHOW MORE` pages the
most recent buffered output (`LAST-0`), `SHOW MORE LAST-1` the one before it —
so a mutation diff survives a subsequent over-cap SHOW/FIND. The ring lives in
the session worktree and is restored by `ROLLBACK` along with the rest of the
worktree state.

The highest-value use is filtering a long `VERIFY build` log without re-running
the build: `SHOW MORE WHERE text MATCHES 'error|warning'`.

> **Template limitation** — `SHOW callees OF` does not resolve C++ template functions. Use `FIND usages OF 'symbol'` instead.

---

### Editing Commands (node handles)

Node handles are **the** way to edit indexed source. Raw line-range and whole-file
editing of indexed files is disabled (the engine returns guidance pointing here);
the surviving raw-text forms are collected in
[Raw line and file operations](#raw-line-and-file-operations-legacy-non-indexed-files).

```sql
-- Every verb that names an EXISTING node takes IF REV. It is not optional.
CHANGE NODE '<node_id>' IF REV '<rev>' WITH 'new_content'
CHANGE NODE '<node_id>(n-m)' IF REV '<rev>' WITH 'new_content'
CHANGE NODE '<node_id>' IF REV '<rev>' MATCHING [WORD] 'old' WITH 'new'

INSERT (BEFORE | AFTER) NODE '<node_id>' IF REV '<rev>' WITH 'new_content'

DELETE NODE '<node_id>' IF REV '<rev>'
DELETE NODE '<node_id>(n-m)' IF REV '<rev>'

MOVE NODE '<src_id>' IF REV '<rev>' (BEFORE | AFTER) NODE '<dst_id>'
MOVE NODE '<src_id>' IF REV '<rev>' TO '<dir_hex> | <path>'

-- Creation verbs are ungated: a path that does not exist yet has nothing to
-- fingerprint, and appending to a whole-file handle cannot clobber anything.
COPY NODE '<src_id>' TO '<dir_hex> | <path>'
INSERT NODE FOR '<path>'          -- create an empty file
INSERT NODE FOR '<path>/'         -- create a directory
INSERT AFTER NODE '<file_hex>' WITH '...'   -- append at EOF; no rev needed

-- FOUND — every member of the previous FIND result, in one mutation
CHANGE NODES FOUND IF REV '<master>' MATCHING [WORD] 'old' WITH 'new'
DELETE NODES FOUND IF REV '<master>'
MOVE NODES FOUND IF REV '<master>' TO '<dir_hex> | <dir>/'
COPY NODES FOUND TO '<dir_hex> | <dir>/'
```

#### IF REV is mandatory — and free

**The handle and its rev always travel together.** Every row that hands you a
`node_id` hands you its `rev` in the same row — `FIND symbols`, `FIND files`,
`SHOW outline`, `SHOW members`, `SHOW NODE`, `FIND NODE` — and every mutation
hands back the new handle *and* its new rev, so a follow-up edit on the same node
needs no re-read. You never have to fetch a rev; you already have it.

**Why it is required.** A handle is stable: it survives edits, insertions, even
re-parenting, and it never silently comes to mean a different node. That is
exactly what makes the gate necessary. An agent can carry a handle across dozens
of commands and come back to it, and the handle will still resolve — but the code
underneath may have moved. A rev is the SHA-256 of the node's **whole span**, so
an edit to any *child* changes the enclosing node's rev too. Nothing else can tell
you that the node you remember is not the node that is there.

**When a node is removed, its handle is retired — it never transfers to a
look-alike.** Deleting a node, emptying it with `CHANGE … WITH ''`, blanking it
in a `CHANGE NODES FOUND` sweep, or moving it out of a file all free that node's
ordinal. On the reindex that follows, the freed handle is retired rather than
reassigned to a surviving sibling — so a stale handle to a removed construct
fails loudly with `node_not_found` instead of quietly resolving to a look-alike.
This is what upholds the guarantee above between two byte-identical siblings:
they share a rev, so `IF REV` alone cannot tell them apart, and only retiring the
removed one keeps its handle from silently coming to mean the survivor.

A stale rev is refused with `rev_mismatch`, which hands back the node's current
rev, line range, and source — enough to re-target without another read.

| Variant | Effect | Gate |
|---|---|---|
| `CHANGE NODE … WITH …` | Replace the node's entire source span | `IF REV` |
| `CHANGE NODE '<id>(n-m)' WITH …` | Replace only lines n–m within the node (node-relative offset) | `IF REV` |
| `CHANGE NODE … MATCHING …` | Replace pattern occurrences inside the node's span only | `IF REV` |
| `INSERT BEFORE\|AFTER NODE … WITH …` | Insert new lines around the node | `IF REV` |
| `DELETE NODE …` | Delete the node's source span (or lines n–m within it) | `IF REV` |
| `MOVE NODE '<src>' (BEFORE\|AFTER) NODE '<dst>'` | Relocate the node's bytes to the anchor — one atomic plan, no read round-trip | `IF REV` |
| `MOVE NODE '<src>' … TO '<dst>'` | Move or rename: `<dst>` is a directory handle (keeps the basename) or a path. A whole-file source is **unlinked**, not emptied | `IF REV` |
| `COPY NODE '<src>' TO '<dst>'` | Same addressing, source stays put | none — it creates |
| `INSERT NODE FOR '<path>'` | Create an empty file (trailing slash: a directory) and return its handle **and rev** — the one verb that takes a path, because the path does not exist yet | none — it creates |
| `INSERT … NODE '<file_hex>' WITH …` | Prepend at BOF / append at EOF of a whole file | none — it cannot clobber |
| `CHANGE NODES FOUND MATCHING …` | Sweep the replacement across **every member of the previous FIND**, in one plan | `IF REV` (master) |
| `DELETE NODES FOUND` | Delete every member | `IF REV` (master) |
| `MOVE NODES FOUND … TO '<dir>'` | Move every member into a directory, each keeping its basename | `IF REV` (master) |
| `COPY NODES FOUND TO '<dir>'` | Same, sources stay put | none — it creates |

#### FOUND — mutating a whole FIND result

`FIND` **is** the set-selection syntax. A query with precise filters already names the set, so the
bulk verbs address it as `FOUND` rather than carrying a second glob grammar. The rows a FIND
returned are saved in the session, and a complete result carries a **master rev** — a hash over
every member's `(handle, rev)`:

```sql
FIND usages OF 'oldName'                                    -- rows + found_rev: h9c…
CHANGE NODES FOUND IF REV 'h9c…' MATCHING 'oldName' WITH 'newName'

FIND files IN 'legacy/**' WHERE extension = 'c'             -- rows + found_rev: h4b…
MOVE NODES FOUND IF REV 'h4b…' TO 'archive/'
```

Quote the master rev in `IF REV` and the mutation runs only if not one member has moved since you
looked — the set-level extension of the per-node `IF REV` contract. It is re-derived from the live
members at mutation time, so a rev cached at FIND time proves nothing about now. Unlike a
directory's membership rev, it covers **content** as well, because `CHANGE NODES FOUND` edits
content.

The master rev is reported as `found_rev`: a top-level `found_rev` field in
`format=JSON`, and the `found_rev` metadata row in the default CSV. A FIND that
armed no set — a `GROUP BY` aggregate, or a result truncated by its `LIMIT` —
carries no `found_rev`, and every FOUND verb then refuses.

Every member is mutated in **one plan**: one boundary diff, one `UNDO` step, never half-applied.

| Rule | Why |
|---|---|
| A **handle contributes its whole span**; a `FIND usages` row contributes its one line | A symbol row means the function; a usage row means the call site |
| A **truncated FIND issues no master rev**, and every FOUND verb then refuses | `FIND usages` capped at 20 of 500, swept, would rename 20 and report success. Widen the `LIMIT` and look again |
| **Any FIND replaces `FOUND`; any mutation clears it** | A mutation shifts line numbers, so the set no longer points at what you saw |
| A **`GROUP BY` result clears it** | An aggregate row is a count with a filename on it — it addresses nothing |
| A **rev mismatch hands back no new rev** | The set moved; the only safe recovery is to re-run the FIND and see what it looks like now |
| `DELETE`/`MOVE`/`COPY NODES FOUND` need **handles** | Usage sites are lines, not nodes — arm them with `FIND files` or `FIND symbols` |
| `IF REV` is **mandatory** for `CHANGE`/`DELETE`/`MOVE NODES FOUND`, absent from `COPY NODES FOUND` | Destroying N things you cannot see is the one mistake the diff cannot catch afterwards; a copy creates and destroys nothing |

Each refusal above comes back as a structured self-healing payload you match on
by tag — `no_found_set`, `found_truncated`, or `found_refused` (see the `IF REV`
self-healing payloads above) — never an opaque string.

The set is written to `.forgeql-foundset` in the worktree, so it survives a server restart between
the FIND and the mutation. It is re-gated against live revs on use, so restoring it can only
re-offer a target — never authorise a stale one.
#### MOVE NODE

Relocation, not re-authoring. `MOVE NODE` lifts the node's bytes **verbatim** and splices them at
the anchor — the delete and the insert land in **one atomic plan**, so the file is never briefly
missing the node and a failure leaves nothing half-moved. No read round-trip: you never have to
`SHOW NODE` it, hold the text, and re-`INSERT` it yourself.

```sql
-- reorder two functions in the same file
MOVE NODE '<runq_add>' BEFORE NODE '<thread_runq>'

-- lift a helper into another file (the anchor decides where)
MOVE NODE '<helper>' AFTER NODE '<last_include>'
```

Src and dst may be in **different files**. The response carries `new_node_id`: re-parenting changes
`parent_ordinal`, so the moved node earns a fresh handle.

The payload is spliced **verbatim**, and the **source-side removal absorbs the trailing blank
separator**, exactly like `DELETE NODE` — repeated moves out of one file do not accumulate blank
lines. A `lines_removed` slightly larger than the node's span is this absorption, not a clobber.
Line-addressed `MOVE LINES` and offset sub-ranges (`'<id>(n-m)'`) stay byte-exact.

**The engine does not re-indent (P1).** On an indentation-sensitive format the seam is real: a node
lifted from inside a block keeps its original leading whitespace. That is deliberate — guessing the
right indent is exactly the kind of "smart" the engine refuses to be. The boundary diff shows the
seam; close it yourself with `CHANGE NODE '<new_id>(1-n)'`. Where you want to control the indent
from the start, `INSERT` + `DELETE` inside a transaction remains the better tool.

Moving a node **into itself** (an anchor inside the moved span) is refused rather than silently
corrupting the file.

`INTO` is deliberately **not** offered: "first child of a container" has no mechanical definition
that holds across languages, and the engine will not guess one.

#### Heredoc syntax

Every `WITH 'content'` form also accepts a heredoc block as the replacement text:

```sql
-- Replace a whole node
CHANGE NODE '<node_id>' WITH 'fn run(buf: &mut [u8]) { buf.fill(0); }'

-- Splice one line inside a node (node-relative offset)
CHANGE NODE '<node_id>(3)' WITH 'let mut total: u64 = 0;'

-- Insert a new item immediately after a node
INSERT AFTER NODE '<node_id>' WITH 'fn helper() -> u32 { 42 }'

-- Delete a node, guarded by its content rev
DELETE NODE '<node_id>' IF REV 'h0123456789abcdef'
```

| Heredoc rule | Detail |
|---|---|
| Opening tag | `<<TAG` immediately after `WITH` — tag must be **all-uppercase** (e.g. `RUST`, `CODE`, `END`) |
| Closing tag | Must appear on its **own line** with **no leading whitespace**, matching the opening tag exactly |
| Body | May contain any characters — single quotes, double quotes, embedded ForgeQL keywords — without escaping |
| Purpose | Prefer over `'…'` when the replacement contains single quotes (Rust char literals, lifetimes, C-style string escapes) |

---

### Node Addressing

Every indexed symbol has a **stable node handle** — a `node_id` of the form
`n<segment>.<ordinal>` (e.g. `nb1be37eea3f0.0124`). `<segment>` is a hash prefix
of the file path; `<ordinal>` is a per-file counter assigned in source order.
Node ids are content-addressed per file: they survive line drift, unrelated edits
elsewhere in the file, and re-parse — the drift-proof way to target code. Read
once, then mutate by handle instead of by absolute line.

Comments — including doc comments — index as their own addressable nodes
(`comment`, and runs of adjacent comments as `comment_block`), separate from the
item they document, so a doc comment can be edited without touching the code
below it. An item's span and `rev` fold in its contiguous leading attributes
(`#[...]`), so an `IF REV` guard protects the attributes along with the item.

```sql
FIND NODE '<node_id>'                      -- metadata: name, kind, line, end_line, rev, nav
SHOW NODE '<node_id>' [CONTENT | METADATA] -- source (default) or the FIND NODE record
CHANGE NODE '<node_id>' WITH '...'         -- replace the whole node
INSERT (BEFORE | AFTER) NODE '<node_id>' WITH '...'
DELETE NODE '<node_id>' IF REV '<rev>'
MOVE NODE '<src_id>' (BEFORE | AFTER) NODE '<dst_id>'   -- relocate; source removal absorbs trailing blanks
```

`FIND symbols`, `FIND files`, `SHOW outline`, `SHOW members`, and the CSV form of
`SHOW body` all surface node_ids — each with its `rev` — so a handle you can
actually mutate is one read away.

#### Whole-file and whole-directory handles — `n<hex>` with no ordinal

The ordinal is what makes a handle point *inside* a file. Drop it and the handle
addresses the **file itself** — or a **directory**, since a file and a directory
can never share a path:

```sql
FIND files IN 'src/**'                      -- every row carries node_id + rev
FIND NODE '<hex>'                           -- kind = file | dir, plus its rev
SHOW NODE '<hex>'                           -- read the whole file (buffered)
SHOW NODE '<hex>(12-40)'                    -- read lines 12–40 of it
SHOW outline OF '<hex>'                     -- outline the file, or list a directory
INSERT BEFORE NODE '<hex>' WITH '...'       -- prepend at BOF
INSERT AFTER  NODE '<hex>' WITH '...'       -- append at EOF (works on a 0-byte file)
CHANGE NODE '<hex>' IF REV '<rev>' WITH '...'  -- overwrite the whole file
DELETE NODE '<hex>' IF REV '<rev>'          -- delete the file; a dir deletes its subtree
```

`<hex>` is the same path fingerprint the node form uses (≥ 12 hex chars), so
`FIND files` hands you a handle you can act on without a second lookup.

**`IF REV` is mandatory on the destructive whole-path forms** — whole-file
`DELETE` and `CHANGE`, and a whole-file `MOVE` source. A node edit can be
reviewed and corrected afterwards; deleting a file or overwriting all of it
leaves nothing to re-read. The rev is you proving you are acting on what you
actually saw. `SHOW` and `INSERT BEFORE/AFTER` create or read, so they are
ungated.

A **file rev** is the SHA-256 of its bytes. A **directory rev** is a membership
XOR over the paths of every file underneath it, at any depth: it moves when the
subtree gains, loses, or renames a file, and deliberately does *not* move when a
file's content changes. That is what a recursive delete needs to be gated on —
that you saw the current membership, not that you read every byte. Content
staleness is the per-file rev's job.

#### Creating and relocating paths

A handle addresses something that exists. Creation and renaming are the two
operations that cannot start from one — the destination has no fingerprint yet —
so they take a path:

```sql
INSERT NODE FOR 'src/new_module.rs'          -- create an empty file, returns its n<hex>
INSERT NODE FOR 'docs/'                      -- trailing slash: create a directory
MOVE NODE '<hex>' IF REV '<rev>' TO 'src/renamed.rs'   -- rename (source is unlinked)
MOVE NODE '<hex>' IF REV '<rev>' TO '<dir_hex>'        -- move into a directory
COPY NODE '<hex>' TO 'api/v2/'               -- copy, keeping the basename
COPY NODE '<hex>.<ord>' TO 'src/extracted.rs'          -- lift one node into a new file
```

The `TO` argument is a **directory handle** (the source keeps its basename) or a
**path**: a trailing slash — or an existing directory — means "into here",
anything else is the full destination. The destination is never clobbered; if it
exists, the command is refused. `MOVE` with a whole-file source is destructive
(the source file is removed) and takes the mandatory `IF REV`; `COPY` only
creates, so it is ungated. Both return the destination's `node_id` — the handle
is path-derived, so a move earns a new one, while the `rev` is unchanged (same
bytes).

`INSERT NODE FOR` replaces the old file-creation idiom (`COPY LINES 1-1`), and
the pair `INSERT NODE FOR '<path>'` → `INSERT AFTER NODE '<hex>' WITH '…'` is
the create-then-write bootstrap. Note that git does not track empty directories:
one created with a trailing slash exists on disk and is listed by `FIND files`,
but it will not survive a commit/clone round-trip until a file lands in it. The
engine will not invent a `.gitkeep` for you.

Files created inside a transaction are removed by `ROLLBACK`. (They are
untracked until `COMMIT` stages them, so `git reset --hard` used to walk straight
past them and leave them behind.)

#### Node-relative line offsets

A node_id may carry a 1-based line offset **inside the node's own span**, so you
can target one line (or a range) of a node without computing absolute numbers:

```sql
SHOW NODE '<id>(2)'   CONTENT       -- the node's 2nd line
SHOW NODE '<id>(2-4)' CONTENT       -- the node's 2nd–4th lines (inclusive)
CHANGE NODE '<id>(2)'   WITH '...'  -- splice the node's 2nd line
CHANGE NODE '<id>(2-4)' WITH '...'  -- splice the node's 2nd–4th lines
```

Offsets are inclusive and 1-based; an offset past the node's last line is a hard
error. The CSV `off` column from `SHOW body` is exactly this offset, so you can
copy `'<id>(off)'` straight into a `CHANGE NODE`. (Offsets apply to `CONTENT`
only — `SHOW NODE '<id>(n)' METADATA` is rejected.)

#### `rev` and optimistic concurrency (`IF REV`)

Each node carries a content `rev` handle (`h<16-hex>`) reported by `FIND NODE`.
Guard a mutation with `IF REV` to make it a no-op when the node changed since you
read it:

```sql
CHANGE NODE '<node_id>' IF REV 'h0123456789abcdef' WITH '...'
```

The edit applies only when the node's current rev matches; otherwise it is
rejected without touching the file. A rejected guard returns a **self-healing
payload** so you can re-target without another read:

```json
{
  "error": "rev_mismatch",
  "node_id": "<id>",
  "expected": "<the rev you passed>",
  "current_rev": "<the node's actual current rev>",
  "line_start": 10,
  "line_end": 14,
  "current_content": "…the node's current source…"
}
```

The bulk `NODES FOUND` verbs refuse in the same self-healing form — each returns
a JSON object you match on by its `error` tag, carrying a `suggested_next`
string that names the recovery:

- `no_found_set` — no FIND has armed a set this session; run a FIND first.
- `found_truncated` — the arming FIND was capped by `LIMIT`, so no master rev
  was issued; re-run it with a `LIMIT` that covers the whole result.
- `found_refused` — a bulk mutation ran without the mandatory `IF REV`; re-run
  the FIND to read the master rev off its response, then quote it.

A handle that resolves to nothing returns `{"error": "node_not_found", …}` in
the same form. Over MCP — stdio and HTTP alike — every one of these structured
rejections comes back as an error-flagged (`isError`) tool result whose text is
the JSON payload, not a buried protocol error, so the agent parses and acts on
it exactly like an ordinary result.

---

### Mutation Responses — the diff is the contract

Mutations are **mechanical**: the engine splices exactly the bytes you supply and
never auto-corrects syntax — no comma fixing, no `{ }` wrapping, no
re-indentation. What it does instead is show you exactly what happened. Every
successful mutation returns:

| Field | Meaning |
|---|---|
| `new_node_id` | The node's current handle after the edit (an edit can change a node's identity) |
| `lines_written` | Number of source lines the edit wrote |
| `lines_removed` | Number of original source lines the edit overwrote — the **destructive-edit signal**: a large value on a small edit means you clobbered more than intended (e.g. a `CHANGE NODE` on a node whose span covers a whole folded body) |
| boundary diff | A compact diff including the unchanged **context lines directly above and below** the change, so a seam the splice created (a missing separator, an unbalanced brace) is visible immediately |
| `structural_errors` | Present only when the edit left a touched structured-text file unparseable under a **strict, format-native parser** (JSON, YAML, TOML and XML today): the file path, the parser's diagnostic with line/column, and whether the file parsed cleanly *before* this edit. The engine flags the break; the repair is yours. A missing JSON comma or a reshaped YAML indent is caught here even though it leaves no top-level `error` region for tree-sitter to report. |

Every present line of the diff (added + context) carries an inline
`node_id(offset)` handle, so a follow-up correction is a copy-paste
`CHANGE NODE '<id>(off)' WITH '…'` — no re-read round-trip. Read the diff after
**every** mutation: if it shows a seam, the fix is yours to issue; the engine
will not issue it for you.

Structural validation is **detection, never repair** — like every other field
here. A strict parser is asked whether each touched file still parses, before the
edit and after it, and the verdict is reported with the parser's own message.
Formats with no strict validator, and the `.jsonc` dialect (whose comments a JSON
parser would wrongly reject), are never flagged.

---

### UNDO

```sql
UNDO

UNDO LAST-n
```

Every mutation snapshots the pre-edit bytes of the files it touched into a
per-session **undo ring** (10 slots deep). `UNDO` restores the most recent
mutation's pre-edit state; `UNDO LAST-n` restores the state from `n` mutations
further back (reversing the last `n+1` mutations at once). The restore reindexes
the touched files and invalidates the commit gate exactly like a forward
mutation. The ring lives in the session worktree, is excluded from commits, and
dies with the worktree.

---

### Transaction Commands

```sql
BEGIN TRANSACTION 'name'

COMMIT MESSAGE 'message'

ROLLBACK [TRANSACTION 'name']
```

| Command | Effect |
|---|---|
| `BEGIN TRANSACTION` | Create a named git checkpoint. Dirty state is auto-committed first. Checkpoints stack — multiple `BEGIN` calls push; `ROLLBACK` pops. |
| `COMMIT MESSAGE` | Stage all changes and create a git commit. Also accepts a heredoc body (`COMMIT MESSAGE <<MSG … MSG`) for multi-line messages. ForgeQL runtime files are auto-excluded and file deletions are staged correctly. |
| `ROLLBACK` | Revert to the most recent checkpoint, or to a named one (discards later checkpoints). |

**Commit gate:** verify steps in `.forgeql.yaml` may set `commit_gate: true`.
When set, `COMMIT` is refused until that step has passed **since the most recent
mutation** — any edit after a pass re-blocks the commit until the step is re-run.
Several steps may be gated; every gated step must pass. A commit can therefore
never record an unvalidated tree.

---

### VERIFY, RUN, and Background JOBs

```sql
VERIFY build 'step' ['arg']…

RUN 'template' ['arg']…

JOB START 'step' ['arg']…
JOB STATUS '<job-id>'
JOB LIST
```

| Command | Effect |
|---|---|
| `VERIFY build` | Run a named step from `.forgeql.yaml` `verify_steps` and wait for it. The command executes on the background job pool — the engine is never blocked while it runs — but the response is synchronous: `success` + `output`, exactly as before. If the run outlives the step's `timeout_secs`, the response degrades to a `job_started` row with the id to poll. Does **not** auto-rollback on failure. Steps may declare typed positional params (`params: [{ name: target, type: ident }]`); each `$name` in the step's command is substituted after arity and type validation, so a value can never inject shell syntax. |
| `RUN` | Run a named allowlisted command template from `.forgeql.yaml` `run_steps`, waiting the same way as `VERIFY build`. `ident` args substitute into the command; `string` args bind to the subprocess stdin and are never spliced into the shell. |
| `JOB START` | Run a verify step as a detached **background job** — returns a job id immediately instead of blocking the request. Use for long test gates. Accepts the same typed positional args as `VERIFY build`. A `commit_gate: true` step run this way satisfies the commit gate when the job completes — unless an edit happened while it ran, in which case the gate stays blocked (the run tested stale sources). |
| `JOB STATUS` / `JOB LIST` | Poll one job's state and output, or list all jobs. Polling also folds finished gate jobs into the commit gate. Responses carry a `hint` row with the next step (poll again, or the `SHOW MORE` grep recipe on failure). |

Background jobs run through a **bounded worker pool**: at most
`FORGEQL_MAX_CONCURRENT_JOBS` jobs execute at once (default `2`) and the rest
wait `Queued` in a FIFO queue, starting automatically as slots free — a burst of
`JOB START` builds is throttled instead of exhausting machine memory.

VERIFY/RUN/JOB STATUS output that exceeds the inline cap is buffered; page or
grep it with `SHOW MORE` — e.g. `SHOW MORE WHERE text MATCHES '^error|-->'`
triages a compiler log without re-running the build.

**`.forgeql.yaml`** may be in the repo root **or** in the directory directly above it (sidecar, outside the tracked tree):

```yaml
workspace_root: .
verify_steps:
  - name: test
    command: "cmake --build build && ctest --test-dir build"
    timeout_secs: 120
    commit_gate: true     # COMMIT refused until this passes after the last
                          # edit; several gated steps must ALL pass
    weight: medium        # JOB scheduler cost: light | medium | heavy, or
                          # explicit {cores: 4, memory_mb: 4096, max_seconds: 600}
    summary:              # inline output window; full log kept for SHOW MORE
      direction: tail     # head | tail
      lines: 40
  - name: build-one       # typed args: VERIFY build 'build-one' 'core_b1'
    command: "cmake -U PROJECT -D PROJECT=$project -B build && cmake --build build"
    params:
      - name: project
        type: ident       # ident = [A-Za-z0-9_.-]+, substituted for $project;
                          # string = bound to stdin, never spliced
    weight: heavy
run_steps:                # allowlisted templates for RUN '<name>' ['arg']…
  - name: grep-cache
    command: "grep -m1 $key $FORGEQL_BUILD_DIR/CMakeCache.txt"
    params:
      - name: key
        type: ident
    timeout_secs: 30
line_budget:
  initial: 3000           # starting allowance
  ceiling: 9000           # hard ceiling; budget never exceeds this
  recovery_base: 200       # lines credited per recovery (halved on repeats)
  recovery_window_secs: 30
  warning_threshold: 250  # warn agent when budget falls below this
  critical_threshold: 50  # critical state — caps SHOW LINES output
  critical_max_lines: 20  # max lines returned in critical state
  idle_reset_secs: 120    # auto-delete budget file after idle gap; 0 = never
```

Steps and templates are frozen at `USE`, so a later edit cannot tamper with a
command the gate will run.

Every VERIFY/RUN/JOB subprocess receives the **session environment contract**:
`FORGEQL_SESSION_ID` (full token), `FORGEQL_SOURCE`, `FORGEQL_BRANCH`,
`FORGEQL_ALIAS`, `FORGEQL_WORKTREE` (absolute path of the session worktree —
scripts must build *this* tree, never a hardcoded checkout), and
`FORGEQL_BUILD_DIR` (a per-worktree build directory so concurrent sessions
never share build artifacts, e.g. `cargo --target-dir $FORGEQL_BUILD_DIR`).

**Typed parameters:** a step declares positional parameters in its `params`
list; the call site passes values in the same order and the engine validates
count and type before running anything.

```yaml
verify_steps:
  - name: build-one
    command: "cmake -U PROJECT -D PROJECT=tresos/$project -B $FORGEQL_BUILD_DIR && cmake --build $FORGEQL_BUILD_DIR"
    params:
      - name: project
        type: ident
run_steps:
  - name: annotate
    command: "tee -a $FORGEQL_BUILD_DIR/notes.txt"
    params:
      - name: note
        type: string
```

```sql
VERIFY build 'build-one' 'core_b1'
RUN 'annotate' 'free text; quotes & spaces are fine'
```

An `ident` argument must match `[A-Za-z0-9_.-]+` and replaces every `$name`
occurrence in `command` — no shell metacharacter can pass validation, so a
value can never inject shell syntax. A `string` argument is **never** placed
in the command line: all string args are newline-joined in declared order and
bound to the subprocess **stdin**. Wrong arity or a malformed value fails
before the command starts.

**Line budget:** when `line_budget` is present, each session tracks how many source
lines the agent has consumed. Budget status (`remaining/ceiling (delta)`) is returned
in every MCP response via the `line_budget` metadata field. Budget files are persisted
to `.budgets/{source}@{branch}.json` under the ForgeQL data directory. Expired files
are auto-deleted on the next `USE` via `sweep_expired()`.

---

### EXPORT PATCH

```sql
EXPORT PATCH            -- everything this session committed over its base branch
EXPORT PATCH LAST n     -- the last n source-touching commits
```

Writes the session's commits as `git am`-ready mbox files under
`.forgeql-patches/` in the worktree and returns them inline: a header row
with the exported range, one row per file (absolute path, size, **sha256**),
then the concatenated patch text (windowed — page with `SHOW MORE`). Copy a
small patch straight from the response, or fetch the files from the worktree
path; either way, verify the sha256 with `sha256sum` before `git am`.

`ForgeQL` runtime files (`.forgeql-*` at any depth) are excluded from every
patch, so the export is safe mid-transaction: checkpoint commits that touch
only runtime files produce no patch at all, and a commit mixing source with
runtime files exports only its source part — the series still applies in
order with `git am`. `LAST n` counts source-touching commits, so checkpoints
never consume the count. Uncommitted worktree edits belong to no commit and
are never exported; the response says so in a hint when any exist.

The counterpart for *uncommitted* work is [`SHOW DIFF`](#show-diff).

---

### SHOW DIFF

```sql
SHOW DIFF                 -- file map + hunks for every uncommitted change
SHOW DIFF STAT            -- the file map alone (cheapest; no hunk text)
SHOW DIFF [clauses]       -- IN / EXCLUDE / WHERE / ORDER BY / LIMIT
SHOW DIFF OF '<commit>'   -- diff a commit against its first parent (STAT + clauses too)
```

`SHOW DIFF OF '<commit>'` reviews a committed hash — the diff of that commit
against its first parent — from any session of the same source, since the commit
lives in the shared repository. `STAT` and clauses apply exactly as for the
pending diff. Without `OF`, `SHOW DIFF` shows the session worktree's uncommitted
changes.

The session worktree's **uncommitted** diff against `HEAD`, returned inline.
`EXPORT PATCH` covers committed work only, so this is the way to see a change
that has not been committed yet — in particular for a **pre-commit reviewer
agent**, which may have no filesystem access to the worktree at all.

The response leads with the **file map** — one row per changed file
(`status`, `added`, `removed`, `file`) — and then the unified-diff text.

**Untracked files are included**, rendered as whole-file additions: a review
that could not see newly added files would miss the most important part of most
changes. `ForgeQL` runtime files (`.forgeql-*` at any depth) are excluded, as in
`EXPORT PATCH`.

**Clause targets.** Every clause applies to the per-file rows — `path` / `file`,
`name`, `status`, `added`, `removed`, `changed` — *except* `WHERE text`, which
filters the diff's own **lines**, exactly as it does for `SHOW body` and
`SHOW NODE`. Line filtering runs **before** the inline cap, so grepping a
50 000-line diff costs no more than grepping a 50-line one.

Output routes through the `SHOW MORE` ring: the file map arrives inline and the
hunks page from the top.

```sql
-- a reviewer's triage, in three cheap queries
SHOW DIFF STAT                                  -- what changed at all?
SHOW DIFF STAT IN 'crates/forgeql-core/**'      -- was the engine touched?
SHOW DIFF STAT IN 'doc/**'                      -- did the docs move with it?

-- then read only what matters
SHOW DIFF IN 'crates/forgeql-lang-text/**'
SHOW DIFF WHERE text MATCHES '^\+.*(unsafe|unwrap)'
SHOW MORE HEAD 40
```

---

## Universal Clauses

Every command accepts these clauses. Inapplicable clauses are silently ignored.
Multiple `WHERE` clauses combine with implicit AND. `AND` is accepted as a synonym for a repeated `WHERE` (e.g. `WHERE a = 1 AND b > 2`).

Engine applies clauses in this fixed pipeline order, regardless of written order:

```
IN → EXCLUDE → WHERE → GROUP BY → HAVING → ORDER BY → OFFSET → LIMIT
```

```sql
[WHERE field operator value] …
[HAVING field operator value]
[IN 'glob']
[EXCLUDE 'glob']
[ORDER BY field [ASC | DESC]]
[GROUP BY (file | fql_kind)]
[LIMIT N]
[OFFSET N]
[DEPTH N]
```

| Clause | Purpose |
|---|---|
| `WHERE` | Filter rows. Repeatable (implicit AND); `AND` is an accepted synonym for a repeated `WHERE`. Works on all field types including dynamic and enrichment fields. |
| `HAVING` | Filter after `GROUP BY` aggregation. Operates on `count`. |
| `IN` | Restrict to files matching glob pattern. |
| `EXCLUDE` | Remove files matching glob pattern. Repeatable — every `EXCLUDE` clause applies; a row is dropped when **any** pattern matches its path. |
| `ORDER BY` | Sort results. Default `ASC`. Any filterable field including enrichment fields (numeric values like `shadow_count`, `escape_count` sort numerically). |
| `GROUP BY` | Aggregate by field. Adds `count` to each group. |
| `LIMIT` | Maximum rows returned. Implicit cap of 20 when omitted on `FIND`. |
| `OFFSET` | Skip N rows (pagination). |
| `DEPTH` | For `SHOW body`: collapse depth. For `FIND files`: directory tree depth. |

---

## Operators and Values

| Operator | Meaning |
|---|---|
| `=` | Exact equality |
| `!=` | Not equal |
| `LIKE` | SQL wildcard: `%` = any sequence, `_` = any single char (case-insensitive) |
| `NOT LIKE` | Negated LIKE |
| `MATCHES` | Regex match (Rust `regex` crate syntax, case-sensitive by default; use `(?i)` for case-insensitive) |
| `NOT MATCHES` | Negated regex match |
| `>` `>=` `<` `<=` | Numeric comparison |

| Value syntax | Type |
|---|---|
| `'text'` | String (single-quoted) |
| `"text"` | String (double-quoted) |
| `bare_value` | Unquoted string — alphanumeric, `_`, `:`, `-`, `.`, `/` (where quoting is optional) |
| `42` | Integer |
| `-10` | Signed integer |
| `true` / `false` | Boolean (reserved) |

**Quoting rules:** `CHANGE … MATCHING` and `COMMIT MESSAGE` require explicit quotes
(content may contain spaces). `CHANGE FILE` paths require explicit quotes for mutation
safety. All other positions accept bare values or either quote style.

---

## Filterable Fields

### Symbol Fields

Applies to: `FIND symbols`, `FIND usages OF`, `FIND callees OF`

| Field | Type | Description |
|---|---|---|
| `name` | string | Symbol name |
| `fql_kind` | string | Universal kind: `function`, `class`, `struct`, `enum`, `variable`, `field`, etc. |
| `language` | string | Language name: `cpp`, `rust`, `python`, etc. |
| `path` | string | Relative file path (also used by `IN`/`EXCLUDE` globs) |
| `line` | integer | 1-based start line |
| `usages` | integer | Workspace-total usage-site count, aggregated from the reference index at index time. `ORDER BY usages DESC` and `WHERE usages > N` are real queries, not heuristics. |

**Filtered-field projection:** when a `WHERE` clause targets a non-core field —
numeric, string, or boolean (e.g. `WHERE has_assignment_in_condition = 'true'`,
`WHERE member_count > 10`) — that field's value is projected into the output
rows, so the value you filtered on is always visible in the result.

### Outline Fields

Applies to: `SHOW outline OF`

| Field | Type | Description |
|---|---|---|
| `name` | string | Symbol name |
| `kind` | string | Universal kind (`fql_kind` value, e.g. `function`, `class`). Falls back to raw tree-sitter name for unmapped nodes. |
| `path` / `file` | string | Relative file path |
| `line` | integer | 1-based start line |
| `depth` | integer | Nesting depth in the structural tree (0 = top-level). Filterable and sortable. |
| `node_id` | string | Stable node handle (present once the file has been indexed/reindexed). |

### Member Fields

Applies to: `SHOW members OF`

| Field | Type | Description |
|---|---|---|
| `kind` / `type` | string | Member kind (`field`, `method`, `enumerator`) |
| `text` / `declaration` / `name` | string | Declaration text |
| `line` | integer | 1-based line number |

### File Fields

Applies to: `FIND files`

| Field | Type | Description |
|---|---|---|
| `path` / `file` | string | Relative file path. A **directory** row ends in `/` (`src/`) — that trailing slash is the only marker, so `WHERE path LIKE '%/'` lists directories and `NOT LIKE` excludes them. |
| `name` | string | Bare file name (e.g. `Kconfig`, `CMakeLists.txt`). Works with `=`, `LIKE`, `MATCHES`. |
| `extension` / `ext` | string | Extension without `.` (empty for extension-less files and directories) |
| `size` | integer | File size in bytes; for a directory, its number of direct children |
| `depth` | integer | Directory depth from workspace root |
| `node_id` | string | The path's bare-hex handle (`n<hex>`) — on every path row, so a listed file or directory is actionable without a second lookup |
| `rev` | string | Version stamp for the path: a file's is the SHA-256 of its bytes, a directory's is a membership XOR over the paths underneath it. Pass it to `IF REV`. |
| `has_error` | `"true"` / `"false"` | The file did **not parse as its declared language** — it holds at least one `error_scope = 'root'` region. This is the `.c` that is not really C, or the JSON with an unbalanced brace. |
| `error_count` | integer | Number of `root` regions in the file |
| `parse_coverage` | integer | Percent of the file's bytes tree-sitter parsed (0–100) |

All three are **derived on demand**. They cost an index scan, so they are computed only when a
clause names them — a plain `FIND files` never pays for it, and each column appears in the output
only when you asked about it. An unpopulated entry matches neither `has_error = 'true'` nor
`= 'false'`, so a query that never asked can never be misread as a clean bill of health.

**An `error` row is not damage.** tree-sitter parses C **without running the preprocessor**, so it
cannot know that an unknown identifier in declaration-specifier position is a macro:
`static ALWAYS_INLINE void f(void)` yields an `ERROR` beside the return type while `f` itself
indexes perfectly as a `function` with correct boundaries. Zephyr holds **21 681** such regions —
**16 480** of them `nested` inside a node that indexed fine — and essentially none of them is
damage. That is why `has_error` counts **only `root`** regions (207 in Zephyr): a signal that fires
on idiomatic kernel C is not a signal. Use `error_scope` for the raw picture and `parse_coverage`
for magnitude.

Triage a repository before mutating anything in it:

```sql
FIND files   WHERE has_error = 'true'                    -- files that did not parse at all
FIND files   WHERE parse_coverage < 50 ORDER BY parse_coverage ASC   -- mostly-unparsed files
FIND symbols WHERE fql_kind = 'error' WHERE error_scope = 'root'     -- the regions themselves
```

The engine reports where the parse broke and passes no judgement; it never repairs anything (P1) —
it hands you a handle and you do the repair:

```sql
FIND symbols WHERE fql_kind = 'error' WHERE error_scope = 'root'   -- get the node_id
SHOW NODE '<id>'                                                   -- read the region
CHANGE NODE '<id>' WITH '…'                                        -- repair it yourself
```

Ragged CSV rows and duplicate JSON keys are deliberately **not** errors — they parse fine. They
surface through block-group splitting instead.

### Diff Fields

Applies to: `SHOW DIFF`

One row per changed file in the session worktree's uncommitted diff.

| Field | Type | Description |
|---|---|---|
| `path` / `file` | string | Path relative to the worktree root |
| `name` | string | Bare file name |
| `status` | string | `A` added (incl. untracked), `M` modified, `D` deleted, `R` renamed, `T` typechange |
| `added` | integer | Count of `+` lines in this file's hunks |
| `removed` | integer | Count of `-` lines in this file's hunks |
| `changed` | integer | `added + removed` — sort by it to find the biggest edits |

`WHERE text …` does **not** filter these rows: it filters the diff's own source
lines instead (see [Source Line Fields](#source-line-fields)), so a `SHOW DIFF`
can select files by path or size *and* grep their hunks in one statement.

### Source Line Fields

Applies to: `SHOW body OF`, `SHOW LINES n-m OF`, `SHOW context OF`, `SHOW NODE`, and `SHOW DIFF` (where it filters the diff's own lines)

| Field | Type | Description |
|---|---|---|
| `text` | string | Line content (supports `LIKE`, `MATCHES`, `=`) |
| `line` | integer | 1-based line number |
| `marker` | string | Prefix marker (e.g. `+`, `-` in diff output) |

Filtering runs **before** the implicit `DEFAULT_SHOW_LINE_LIMIT` cap, so the full function body is searched even when not all lines are returned.

### Call Graph Fields

Applies to: `SHOW callees OF`

| Field | Type | Description |
|---|---|---|
| `name` | string | Called symbol name |
| `path` / `file` | string | File containing the call |
| `line` | integer | 1-based line number of the call |

### Dynamic Fields

Auto-extracted from tree-sitter grammar. Queryable with `WHERE` without recompiling.

| Field | Availability | Description |
|---|---|---|
| `type` | C/C++ | Return type text |
| `value` | C/C++ | Initial value (`preproc_def`, `init_declarator`) |
| `declarator` | C/C++ | Full declarator with pointer/reference qualifiers |
| `parameters` | C/C++ | Parameter list text |

If a field does not exist on a row, `WHERE` evaluates to false (SQL `NULL` semantics).

**Numeric coercion** — dynamic fields are stored as strings. `WHERE value >= 1000` parses the stored text as an integer; if parsing fails, the predicate silently evaluates to false.

### Enrichment Fields

Computed at index time. Queryable with `WHERE` like any other field.

**Naming convention for enrichment fields:**

| Prefix | Meaning | Example |
|---|---|---|
| `is_` | Intrinsic property of the symbol itself | `is_recursive`, `is_exported`, `is_const`, `is_magic` |
| `has_` | The symbol's body **contains** something | `has_shadow`, `has_escape`, `has_fallthrough`, `has_cast`, `has_todo` |
| `_count` | Numeric count (often paired with `has_` or `is_`) | `shadow_count`, `cast_count`, `recursion_count`, `param_count` |

> **Rule of thumb:** `is_X` describes *what a symbol is*; `has_X` describes *what it contains*.
> For example, a function `is_recursive` (it calls itself) and `has_shadow` (variables inside it shadow outer ones).
#### NamingEnricher

| Field | Applies to | Description |
|---|---|---|
| `naming` | all named symbols | `camelCase`, `PascalCase`, `snake_case`, `UPPER_SNAKE`, `flatcase`, `other` |
| `name_length` | all named symbols | Character count of symbol name |

#### CommentEnricher

| Field | Applies to | Description |
|---|---|---|
| `comment_style` | `comment` | `doc_line` (`///`), `doc_block` (`/** */`), `block` (`/* */`), `line` (`//`) |
| `has_doc` | `function` | `"true"` if preceded by a doc comment |

#### NumberEnricher

| Field | Applies to | Description |
|---|---|---|
| `num_format` | `number` | `dec`, `hex`, `bin`, `oct`, `float`, `scientific` |
| `is_magic` | `number` | `"true"` for unexplained constants (not 0, 1, -1, 2, powers of 2, bitmasks) |
| `num_suffix` | `number` | Type suffix: `u`, `l`, `ll`, `ul`, `ull`, `f`, `ld` |
| `suffix_meaning` | `number` | Semantic meaning of suffix: `unsigned`, `long`, `float`, etc. |
| `has_separator` | `number` | `"true"` if contains digit separators |
| `num_value` | `number` | Raw text of the literal |

#### ControlFlowEnricher

| Field | Applies to | Description |
|---|---|---|
| `condition_tests` | `if`, `while`, `for`, `do` | Number of boolean sub-expressions |
| `paren_depth` | `if`, `while`, `for`, `do` | Max parentheses nesting |
| `condition_text` | `if`, `while`, `for`, `do` | Normalized condition *skeleton* — **operands** (the nouns) are alpha-renamed to `a`, `b`, … for shape comparison, while **operators are kept verbatim** because the operator *is* the signal: `&&`, `\|\|`, `!`, the comparisons, the bitwise ops, and the assignment `=` (the `=`-for-`==` smell) all survive, so `x==5\|\|x==6` → `a==b\|\|a==c` and `if ((x = a + b) > 0)` → `((a=b)>c)`. The one exception is value-only arithmetic (`+ - * / %`) on the right of an assignment, which folds into a single operand (`x = a + b` → `a=b`). NOT raw source text. Grammars without a `condition` field (CMake, Make, C++ range-`for`) name rows by the construct's raw first line instead. |
| `has_catch_all` | `switch` | `"true"` if switch has a catch-all case |
| `catch_all_kind` | `switch` | Kind of catch-all (e.g. `"default"`) when present |
| `for_style` | `for` | `"traditional"` or `"range"` |
| `has_assignment_in_condition` | `if`, `while`, `for` | `"true"` if condition contains `=` (not `==`) |
| `mixed_logic` | `if`, `while`, `for` | `"true"` if `&&` and `\|\|` appear at the same top-level without explicit parentheses (MISRA Rule 12.1) |
| `dup_logic` | `if`, `while`, `for`, `do` | `"true"` if condition contains duplicate sub-expressions in `&&`/`\|\|` chains |
| `branch_count` | `function` | Total control-flow branch points |
| `enclosing_fn` | `if`, `switch`, `for`, `while`, `do` | Name of the containing function — enables `SHOW body OF` directly from a CF-enrichment query result |

#### OperatorEnricher

| Field | Applies to | Description |
|---|---|---|
| `increment_style` | `increment` | `"prefix"` or `"postfix"` |
| `increment_op` | `increment` | `"++"` or `"--"` |
| `compound_op` | `compound_assignment` | `+=`, `-=`, `*=`, `/=`, `%=`, `&=`, `\|=`, `^=`, `<<=`, `>>=` |
| `operand` | `compound_assignment` | Left-hand side text |
| `shift_direction` | `shift_expression` | `"left"` or `"right"` |
| `shift_amount` | `shift_expression` | Right-hand operand text |
| `operator_category` | `increment`, `compound_assignment`, `shift_expression` | `"increment"`, `"arithmetic"`, `"bitwise"`, `"shift"` |

#### MetricsEnricher

| Field | Applies to | Description |
|---|---|---|
| `lines` | `function`, `struct`, `class`, `enum` | Line span |
| `param_count` | `function` | Parameter count |
| `return_count` | `function` | `return` statement count |
| `goto_count` | `function` | `goto` statement count |
| `string_count` | `function` | String literal count |
| `throw_count` | `function` | `throw` statement count |
| `member_count` | `struct`, `class`, `enum` | Member/enumerator count |
| `is_const` | `function`, `variable` | `"true"` if `const` present |
| `is_volatile` | `function`, `variable` | `"true"` if `volatile` present |
| `is_static` | `function` | `"true"` if `static` |
| `is_inline` | `function` | `"true"` if `inline` |
| `is_override` | `function` | `"true"` if `override` |
| `is_final` | `function` | `"true"` if `final` |
| `visibility` | `field` (class members) | `"public"`, `"private"`, `"protected"` |

#### CastEnricher

| Field | Applies to | Description |
|---|---|---|
| `cast_style` | `cast` | `"c_style"` (named C++ casts not indexed in tree-sitter-cpp 0.23) |
| `cast_target_type` | `cast` | Target type text |
| `cast_safety` | `cast` | `"safe"`, `"moderate"`, or `"unsafe"` |
| `has_cast` | `function` | `"true"` if the function body contains any cast expressions |
| `cast_count` | `function` | Number of cast expressions in the body |
#### RedundancyEnricher

| Field | Applies to | Description |
|---|---|---|
| `has_repeated_condition_calls` | `function` | `"true"` if same call in 2+ conditions |
| `repeated_condition_calls` | `function` | Comma-separated function names |
| `null_check_count` | `function` | Count of null-check patterns |
| `duplicate_condition` | `if`, `while`, `for`, `do` | `"true"` if same condition skeleton exists elsewhere in function |

#### ScopeEnricher

| Field | Applies to | Description |
|---|---|---|
| `scope` | `variable` | `"file"` (top-level) or `"local"` (inside function/block) |
| `storage` | `variable` | `"static"`, `"extern"`, or absent |
| `binding_kind` | `variable` | `"function"` or `"variable"` |
| `is_exported` | `variable`, `function` | `"true"` for file-scope declarations without `static` storage (C/C++) or `pub` functions (Rust) |
#### MemberEnricher

| Field | Applies to | Description |
|---|---|---|
| `body_symbol` | `field` (methods) | Qualified name linking to out-of-line definition (e.g. `Class::method`) |
| `member_kind` | `field` | `"method"` or `"field"` |
| `owner_kind` | `field` | `fql_kind` of enclosing type (e.g. `class`, `struct`) |

#### DeclDistanceEnricher

Data-flow enricher that measures how far local variable declarations are from their first use. Excludes parameters, globals, and member variables.

| Field | Applies to | Description |
|---|---|---|
| `decl_distance` | `function` | Sum of (first-use line − declaration line) for locals with distance ≥ 2 |
| `decl_far_count` | `function` | Count of local variables whose first-use is ≥ 2 lines after declaration |
| `has_unused_reassign` | `function` | `"true"` when a local is reassigned before its previous value was read (dead store) |

#### EscapeEnricher

Detects local variables that escape their declaring function — via `return`, address-of (`&`), or pointer/array aliasing.

| Field | Applies to | Description |
|---|---|---|
| `has_escape` | `function` | `"true"` if any local escapes |
| `escape_count` | `function` | Number of distinct escaping locals |
| `escape_vars` | `function` | Comma-separated names of escaping locals |
| `escape_tier` | `function` | Severity: `1` (return), `2` (address-of), `3` (pointer/array alias) |
| `escape_kinds` | `function` | Comma-separated escape mechanisms (e.g. `"return,address_of"`) |

#### ShadowEnricher

Detects variables declared in inner scopes that shadow an outer-scope variable or parameter of the same name.

| Field | Applies to | Description |
|---|---|---|
| `has_shadow` | `function` | `"true"` if any inner variable shadows an outer one |
| `shadow_count` | `function` | Number of shadowing declarations |
| `shadow_vars` | `function` | Comma-separated names of shadowed variables |

> **Note — `#ifdef` blocks:** The ShadowEnricher uses structural guard
> exclusivity (`guard_group_id` + `guard_branch`) to suppress false
> positives from `#ifdef`/`#else` siblings.  Variables declared in
> opposite arms of the same guard group are not reported as shadows.

#### UnusedParamEnricher

Detects function parameters that are never referenced in the function body.

| Field | Applies to | Description |
|---|---|---|
| `has_unused_param` | `function` | `"true"` if any parameter is unused |
| `unused_param_count` | `function` | Number of unused parameters |
| `unused_params` | `function` | Comma-separated names of unused parameters |

#### FallthroughEnricher

Detects switch/case statements where a non-empty case falls through to the next without `break` or `return`. Empty cases (intentional grouping) are not flagged.

| Field | Applies to | Description |
|---|---|---|
| `has_fallthrough` | `function` | `"true"` if any case falls through |
| `fallthrough_count` | `function` | Number of fallthrough cases |

#### RecursionEnricher

Detects direct (single-function) self-recursion. Does not detect mutual recursion (A→B→A).

| Field | Applies to | Description |
|---|---|---|
| `is_recursive` | `function` | `"true"` if the function calls itself |
| `recursion_count` | `function` | Number of self-call sites in the body |

#### TodoEnricher

Detects TODO, FIXME, HACK, and XXX markers in comments inside function bodies. Word-boundary-aware matching avoids false positives.

| Field | Applies to | Description |
|---|---|---|
| `has_todo` | `function` | `"true"` if any marker comment is found |
| `todo_count` | `function` | Total number of marker occurrences |
| `todo_tags` | `function` | Comma-separated, sorted unique tags found (e.g. `"FIXME,TODO"`) |

#### ErrorScopeEnricher

Locates a tree-sitter `ERROR` region and records how much of the file it consumed. Position and
size only — the engine passes no judgement on whether the region is "bad" (P1).

An `ERROR` on its own is a poor danger signal. tree-sitter parses C **without running the
preprocessor**, so `static ALWAYS_INLINE void f(void)` produces an `ERROR` beside the return type
while `f` still indexes correctly as a `function`. Zephyr holds 21 681 `error` regions; 16 480 are
`nested`, and only 207 are `root`.

| Field | Applies to | Description |
|---|---|---|
| `error_scope` | `error` | `"root"` — the ERROR *is* the file: nothing parsed (a `.c` that is not really C). `"file"` — loose at top level, nothing named owns it (usually a file-scope macro the parser could not model). `"nested"` — inside a node the language could name, so an indexed symbol still owns the span and its boundaries are intact. |
| `error_bytes` | `error` | Byte length of the region. Only outermost `ERROR`s are emitted, so spans never overlap and per-file sums are exact — this is what `parse_coverage` is derived from. |

```sql
FIND symbols WHERE fql_kind = 'error' WHERE error_scope = 'root'    -- files that did not parse
FIND symbols WHERE fql_kind = 'error' GROUP BY error_scope ORDER BY count DESC
```

#### GuardEnricher

Tags every symbol inside a C/C++ `#ifdef`/`#if`/`#elif`/`#else` block with
the guard condition that controls its compilation.  Guard fields are injected
into **every** indexed symbol row by `collect_nodes()` — no separate enricher
call is needed.  All seven fields are queryable via `WHERE`, `ORDER BY`, and
`GROUP BY`.

| Field | Applies to | Description |
|---|---|---|
| `guard` | all symbols | Raw guard condition text (e.g. `"defined(CONFIG_SMP)"`, `"!X"`, `"Y && X"`) |
| `guard_defines` | all symbols | Comma-separated symbols that **must be defined** for this branch |
| `guard_negates` | all symbols | Comma-separated symbols that **must be undefined** for this branch |
| `guard_mentions` | all symbols | All symbols mentioned in the condition (superset of defines + negates) |
| `guard_group_id` | all symbols | Unique u64 identifying the `#ifdef`/`#if` block; all arms share the same ID |
| `guard_branch` | all symbols | Ordinal within the group: `0` = if, `1` = first elif/else, `2` = second, … |
| `guard_kind` | all symbols | `"preprocessor"` \| `"attribute"` \| `"build_tag"` \| `"comptime"` \| `"heuristic"` |

**Guard field decomposition rules:**

| Source | `guard` | `guard_defines` | `guard_negates` | `guard_mentions` |
|---|---|---|---|---|
| `#ifdef X` | `"X"` | `"X"` | `""` | `"X"` |
| `#ifndef X` | `"!X"` | `""` | `"X"` | `"X"` |
| `#if defined(A) && defined(B)` | `"defined(A) && defined(B)"` | `"A,B"` | `""` | `"A,B"` |
| `#else` of `#ifdef X` | `"!X"` | `""` | `"X"` | `"X"` |
| Nested `#ifdef X` inside `#ifdef Y` | `"Y && X"` | `"Y,X"` | `""` | `"Y,X"` |

**Example queries:**

```sql
-- All code that REQUIRES CONFIG_BT
FIND symbols WHERE guard_defines LIKE '%CONFIG_BT%'

-- All code compiled when CONFIG_BT is ABSENT
FIND symbols WHERE guard_negates LIKE '%CONFIG_BT%'

-- All code that MENTIONS CONFIG_BT (either direction)
FIND symbols WHERE guard_mentions LIKE '%CONFIG_BT%'

-- Unconditionally compiled code only
FIND symbols WHERE guard = ''

-- Count symbols per guard define
FIND symbols GROUP BY guard ORDER BY count DESC
```

**Structural exclusivity:** Two symbols with the same `guard_group_id` and
different `guard_branch` are definitively mutually exclusive — they are in
opposite arms of the same `#ifdef` block.  The ShadowEnricher and
DeclDistanceEnricher use this fact to eliminate false positives.

#### MacroExpandEnricher

Enriches `macro_call` rows with macro definition metadata and best-effort
single-level expansion text.  Registered after `TodoEnricher` in the enricher
pipeline.  Requires a `MacroTable` populated during the two-pass indexing
pipeline.

| Field | Applies to | Description |
|---|---|---|
| `macro_def_file` | `macro_call` | Source file of the resolved macro definition |
| `macro_def_line` | `macro_call` | 1-based line of the definition |
| `macro_arity` | `macro_call` | Parameter count (`"0"` for object-like macros) |
| `macro_expansion` | `macro_call` | Best-effort single-level expansion text |
| `expanded_reads` | `macro_call` | Local variable names read in expanded text |
| `expanded_has_escape` | `macro_call` | `"true"` if expanded text contains `&local` escape |
| `expansion_depth` | `macro_call` | Expansion nesting depth (currently always `"1"`) |
| `expansion_failed` | `macro_call` | `"true"` when macro resolution fails |
| `expansion_failure_reason` | `macro_call` | Reason for failure (e.g. `"definition not found"`) |

**Supported languages:** C/C++ (`CppMacroExpander`) and Rust (`RustMacroExpander` for `macro_rules!`).

---

## Structured-Text and Config Formats

Structured-text and configuration files are indexed like code: every element
gets a stable `node_id` and the **same commands apply** — `FIND symbols`,
`SHOW NODE`, `CHANGE NODE`, `INSERT BEFORE/AFTER NODE`, `DELETE NODE`. A single
`FIND` sweep returns Makefile rules, CMake calls, and C functions side by side.

| Format | Files | Indexed as |
|---|---|---|
| XML family | `.xml`, `.arxml` (AUTOSAR), `.xdm`/`.epc`/`.epd` (EB tresos), `.ecuc`, `.odx` | Every element is a nested node, named by the cascade below |
| Vector CAN | `.dbc` | `BO_` messages as `object`; `SG_` signals nested as `field`; `VAL_TABLE_`/`VAL_` as `enum`; attributes as `pair`; `EV_` as `variable` |
| TOML | `.toml` (`Cargo.toml`), `.lock` (`Cargo.lock`) | Each `pair` under its key; each `[table]`/`[[table-array]]` by its `name`/`id`/`key` member or header key |
| JSON / YAML | `.json`, `.jsonc`, `.yaml`, `.yml` | `object`/`array`/`pair`. A `pair` is named by its key. A container is named by an identifier-like member (`name`/`id`/`key`/`title`/`alias`), else by its **key-set skeleton** — its sorted keys, comma-joined (`uses`, `name,run`) — so a mapping with no name is still addressable. An `array`/sequence is named after the key of its nearest ancestor pair (`steps`). Names never encode a position: a slot-based name would follow the slot rather than the node, and two siblings would trade `node_id`s when reordered. A run of 8+ adjacent `array` siblings collapses into one `array_block` (below). |
| INI | `.ini`, `.cfg`, `.editorconfig`, `.gitconfig` | `[section]` as `object`; `key = value` nested as `pair` |
| justfile | `justfile` (any casing, with or without dot) | Recipes as `function`; `:=` assignments and `alias` as `variable`; `set` as `pair`; `mod` as `namespace` |
| Make | `Makefile`/`makefile`/`GNUmakefile`, `*.mk` | Rules as `function` named by target list; assignments as `variable`; `define` as `macro`; `ifeq`/`ifdef` as `if` |
| CMake | `CMakeLists.txt`, `*.cmake` | `function()`/`macro()` definitions; every command call as `call_statement`; `if`/`foreach`/`while` as nested control flow |
| Markdown | `.md` | Sections, headings, paragraphs, tables, code blocks — each addressable |
| reStructuredText | `.rst`, `.rest` | Sections by title; paragraphs/list items by text snippet; directives as `macro_call` |

Well-known extensionless file names (`justfile`, `Makefile`, `.editorconfig`, …)
are matched by lowercased file name, leading dot stripped.

**XML element naming cascade** — each element is named by the first rule that
applies:

1. An identifier-like attribute: `name`, `id`, `key`, `title`, or `alias`
   (case-insensitive).
2. The text of a `SHORT-NAME` child element (AUTOSAR containers).
3. The last `/`-segment of a `DEFINITION-REF` child's text — AUTOSAR ECUC
   parameter and reference values become findable by their parameter name
   (e.g. `…/CanIfPublicCfg/CanIfPublicTxBuffering` → `CanIfPublicTxBuffering`).
4. The tag name — anonymous wrapper elements stay addressable as `INSERT`
   anchors.

Attributes are not indexed as separate rows; edit them through their element's
node. The practical effect: ECU-configuration formats that normally require GUI
tooling can be queried by parameter name and edited by node handle:

```sql
FIND symbols WHERE name = 'CanIfPublicTxBuffering' IN 'config/**'
SHOW NODE '<node_id>'
CHANGE NODE '<node_id>(2)' WITH '      <VALUE>true</VALUE>'
```

### Block Grouping — one handle over a run of siblings

A run of adjacent same-kind siblings collapses into a single synthetic,
**childless block node** spanning the whole run. The block is the members'
*sibling*, never their parent; it exists so a whole run can be read, copied,
moved or deleted with one handle. Blank lines between members do not break a run
(they are not tree nodes). Configured per language via `block_groups`.

| Language | Members | Block kind | Min run | Split by |
|---|---|---|---|---|
| Rust | `comment` | `comment_block` | 2 | comment style — a `///` doc run and a `//` line run form **separate** blocks |
| JSON | `array` | `array_block` | 8 | — |

Members keep their own rows and node ids; the block is *added*, nothing is
hidden. Its display label is the first member's snippet plus the run length
(`["g01_name_eq_stopped",  "FIND symbols W… (×201)`).

A run is scanned over **named** siblings, so members separated by anonymous
punctuation still group: JSON array elements are separated by `,` tokens, and
walking raw siblings would break every run at the first comma.

**Why JSON needs it.** A JSON document with no keys anywhere — an array of arrays
of strings, e.g. a test corpus — can be named by nothing, so it indexes to *zero*
rows and is invisible to every `FIND`, `SHOW` and `CHANGE`. Block grouping makes
it addressable: the run becomes one node, and its members are reachable by
node-relative offset.

```sql
-- A block is not a structural declaration, so the DEFAULT outline omits it.
-- `ALL` (or an explicit `WHERE fql_kind`) surfaces it:
SHOW outline OF 'crates/forgeql/tests/corpus.json' ALL
--   2 | array_block | ["g01_name_eq_stopped",  "FIND symbols W… (×201)

SHOW NODE   '<block>' WHERE text MATCHES 'g07_'   -- grep inside; filtering runs before the cap
CHANGE NODE '<block>(42)' WITH '  ["g01_new", "FIND …"],'
DELETE NODE '<block>(40-52)'                      -- drop a contiguous run of entries
```

No new verbs: `'<id>(n)'` and `'<id>(n-m)'` offsets already do the work.

---

### Syntax Damage — the `error` kind

Applies to **every** language, not just structured text.

When tree-sitter cannot parse a span it recovers and produces an `ERROR` node.
Those regions are now indexed as addressable rows with `fql_kind = 'error'`, so a
broken file is no longer **silently, partially indexed**.

```sql
-- triage BEFORE mutating: is the file already broken?
FIND symbols WHERE fql_kind = 'error' GROUP BY file ORDER BY count DESC
FIND symbols WHERE fql_kind = 'error' IN 'config/**'

-- then read and repair by handle
SHOW NODE   '<id>'
CHANGE NODE '<id>' WITH '…'
```

- Only the **outermost** damage is emitted — a nested `ERROR` would report one
  wound as several.
- Zero-width `MISSING` tokens are **not** emitted: a row spanning no bytes could
  be seen but not read or repaired, and a row you cannot act on is worse than no
  row.
- The row's name is the first line of the unparseable text, capped at 60 chars.

**The engine maps the damage; it never repairs it.** `SHOW DIFF`'s boundary diff,
`lines_removed`, and this kind are the same move: make the agent *see*, then let
the agent decide. Note that real-world corpora carry more damage than you would
expect — tree-sitter-c cannot fully parse Zephyr's macro-heavy C, and `error` is
a top-11 kind by count in its `kernel/` tree.

---

## Advanced Patterns

These patterns show ForgeQL capabilities that are non-obvious or combine multiple features.

### Progressive function exploration

`SHOW body` defaults to `DEPTH 0` (signature only). Incrementally reveal structure without reading full source:

```sql
-- Step 1: signature only — understand the interface
SHOW body OF 'PiscoCode::process'

-- Step 2: top-level branches visible — see the control flow
SHOW body OF 'PiscoCode::process' DEPTH 1

-- Step 3: full source when needed
SHOW body OF 'PiscoCode::process' DEPTH 99
```

### Dead code detection pipeline

```sql
-- Unreferenced functions (skip test files)
FIND symbols
  WHERE fql_kind = 'function'
  WHERE usages = 0
  EXCLUDE 'tests/**'
  ORDER BY path ASC

-- Unreferenced macros in headers
FIND symbols
  WHERE fql_kind = 'macro'
  WHERE usages = 0
  IN 'include/**'

-- Dead code behind guards (unreferenced guarded functions)
FIND symbols
  WHERE fql_kind = 'function'
  WHERE guard != ''
  WHERE usages = 0
  EXCLUDE 'test/**'
  ORDER BY lines DESC

-- Symbol distribution (spot bloated files)
FIND symbols
  GROUP BY file
  HAVING count >= 20
  ORDER BY count DESC
```

### Guard analysis pipeline

```sql
-- All code gated on a specific config option
FIND symbols WHERE guard_defines LIKE '%CONFIG_BT%'

-- Code compiled only when a feature is ABSENT
FIND symbols WHERE guard_negates LIKE '%CONFIG_SMP%'

-- Large functions in #else branches (often forgotten)
FIND symbols
  WHERE fql_kind = 'function'
  WHERE guard_branch = '1'
  ORDER BY lines DESC
  LIMIT 15

-- Recursive functions behind guards
FIND symbols
  WHERE is_recursive = 'true'
  WHERE guard != ''
  ORDER BY recursion_count DESC

-- Guard distribution by kind
FIND symbols
  WHERE guard != ''
  GROUP BY guard_kind
  HAVING count >= 1
  ORDER BY count DESC
```

### Code quality audit

```sql
-- Functions longer than 50 lines (refactoring candidates)
FIND symbols
  WHERE fql_kind = 'function'
  WHERE lines >= 50
  ORDER BY lines DESC

-- Complex conditions (4+ sub-tests)
FIND symbols WHERE condition_tests >= 4

-- Switch without default
FIND symbols
  WHERE fql_kind = 'switch'
  WHERE has_catch_all = 'false'

-- Mixed && / || without grouping parentheses
FIND symbols WHERE mixed_logic = 'true'

-- Assignment in condition (likely bug)
FIND symbols WHERE has_assignment_in_condition = 'true'

-- Magic numbers
FIND symbols WHERE is_magic = 'true'

-- C-style casts (modernization targets)
FIND symbols WHERE cast_style = 'c_style'

-- Functions with goto
FIND symbols WHERE goto_count >= 1

-- Duplicated conditions within same function
FIND symbols WHERE duplicate_condition = 'true'

-- Duplicate logic within a single condition (copy-paste bugs)
FIND symbols WHERE dup_logic = 'true'

-- Functions with repeated conditional calls (extract-variable opportunity)
FIND symbols WHERE has_repeated_condition_calls = 'true'

-- Variables declared far from their first use (move declaration closer)
FIND symbols
  WHERE fql_kind = 'function'
  WHERE decl_far_count >= 3
  ORDER BY decl_distance DESC

-- Dead stores (value written but never read before overwrite)
FIND symbols
  WHERE fql_kind = 'function'
  WHERE has_unused_reassign = 'true'

-- Regex search: functions whose name ends with _impl
FIND symbols
  WHERE fql_kind = 'function'
  WHERE name MATCHES '_impl$'

-- Source lines containing TODO/FIXME (case-insensitive)
SHOW body OF 'PiscoCode::run' DEPTH 99
  WHERE text MATCHES '(?i)TODO|FIXME'
```

> **Note — `WHERE text` / `WHERE content` scope:** These predicates are only
> valid on commands that return source lines: `SHOW body`, `SHOW LINES`, and
> `SHOW context`.  Using them on `FIND` queries (symbols, usages, files) will
> return a clear error instead of silently producing 0 results.

> **Tip — exclude test directories:**  Enrichment queries on large codebases
> can be noisy if the results include test harnesses, mocks, and generated
> test code.  Add `EXCLUDE` clauses to focus on production code:
>
> ```sql
> FIND symbols WHERE has_assignment_in_condition = 'true'
>   EXCLUDE '**/testsuite/**'
>   EXCLUDE '**/tests/**'
>   EXCLUDE '**/test/**'
> ```

### Filtered outline and member inspection

`SHOW outline` and `SHOW members` support the full clause pipeline including `WHERE`:

```sql
-- Only enum declarations in a header
SHOW outline OF 'include/config.h'
  WHERE fql_kind = 'enum'

-- Only function definitions in outline
SHOW outline OF 'src/PiscoCode.cpp'
  WHERE fql_kind = 'function'
  ORDER BY line ASC

-- Only field members of a class (skip methods)
SHOW members OF 'PiscoCode'
  WHERE fql_kind = 'field'

-- Paginate a large outline
SHOW outline OF 'include/PiscoCode.h'
  LIMIT 10 OFFSET 20
```

### Usage heat-map and call graph

```sql
-- Which files reference this symbol the most?
FIND usages OF 'PiscoCode::process'
  GROUP BY file
  ORDER BY count DESC

-- What does this function call?
SHOW callees OF 'PiscoCode::process'

-- Top 10 most-referenced functions
FIND symbols
  WHERE fql_kind = 'function'
  ORDER BY usages DESC
  LIMIT 10
```

### The mechanical rename sweep

A rename is a composition of usage sites, not a text substitution: enumerate the
sites, then issue a targeted `CHANGE NODE` per site. Each statement executes
independently — the agent sees every result (and every diff) and decides whether
to proceed.

```sql
-- 1. Checkpoint
BEGIN TRANSACTION 'rename-process'

-- 2. Blast radius — one row per usage SITE (includes non-call references)
FIND usages OF 'PiscoCode::process' GROUP BY file ORDER BY count DESC
FIND usages OF 'PiscoCode::process' LIMIT 50

-- 3. For each site: read the enclosing node, splice the reference by handle
SHOW NODE '<node_id>' WHERE text LIKE '%process%'
CHANGE NODE '<node_id>(off)' WITH '    PiscoCode::run(sample);'
-- …repeat per site; each response's diff confirms the splice

-- 4. Verify the build
VERIFY build 'test'

-- 5a. Success → commit
COMMIT MESSAGE 'rename PiscoCode::process to PiscoCode::run'

-- 5b. Failure → rollback
ROLLBACK TRANSACTION 'rename-process'
```

### Checkpoint stack for phased changes

```sql
-- Phase 1
BEGIN TRANSACTION 'phase-1-rename'
-- …rename sweep as above…
VERIFY build 'test'
COMMIT MESSAGE 'rename OldName to NewName'

-- Phase 2
BEGIN TRANSACTION 'phase-2-add-param'
CHANGE NODE '<declaration_node_id>'
  WITH 'void NewName::run(Buffer& buf, int flags);'
VERIFY build 'test'

-- Phase 2 failed — roll back only phase 2; phase 1 commit preserved
ROLLBACK TRANSACTION 'phase-2-add-param'
```

### SHOW body → CHANGE NODE workflow
`SHOW body` in CSV form surfaces the node's `node_id` (in the header) and a node-relative `off` column, so you can edit by handle without computing absolute line numbers:
```sql
-- Read the function; the CSV header carries its node_id, the off column is node-relative
SHOW body OF 'PiscoCode::process' DEPTH 99

-- Rewrite it by handle — drift-proof, no line numbers to recompute
BEGIN TRANSACTION 'rewrite-process'
CHANGE NODE '<node_id>'
  WITH 'void PiscoCode::run(Buffer& buffer) {
    for (auto& sample : buffer) {
        sample = this->pipeline.apply(sample);
    }
}'
VERIFY build 'test'
COMMIT MESSAGE 'rewrite PiscoCode::run'
```
### File system exploration

```sql
-- Large files (potential split candidates)
FIND files
  WHERE size > 100000
  ORDER BY size DESC
  LIMIT 10

-- Non-source files in src/
FIND files IN 'src/**'
  WHERE extension NOT LIKE 'cpp'
  WHERE extension NOT LIKE 'h'

-- Directory tree 2 levels deep
FIND files DEPTH 2
```

### Compact CSV output (MCP mode)

In MCP mode the default output is compact CSV — token-efficient grouped format.
Pass `format=JSON` for full structured JSON.

All compact output follows a uniform 2-column structure:

```csv
"op",total_count
"group_key","[field1,field2,...]"
"group_value_a","[v1,v2],[v3,v4]"
"group_value_b","[v5,v6]"
"tokens_approx",N
```

**FIND symbols** — grouped by `fql_kind`:
```csv
"find_symbols",8
"fql_kind","[name,path,line,usages]"
"function","[encenderMotor,src/motor_control.cpp,12,7],[apagarMotor,src/motor_control.cpp,28,5]"
"class","[MotorControl,include/motor_control.hpp,5,2]"
```

When a numeric `WHERE` or `ORDER BY` targets an enrichment field, the last
column shows that field's value instead of `usages`:
```csv
-- FIND symbols WHERE member_count > 10
"find_symbols",3
"fql_kind","[name,path,line,member_count]"
"class","[Serial_Protocol,src/Serial_Protocol.h,24,17],[Button,src/buttons.h,31,12]"
"struct","[MpptState,src/SolarCharger.h,57,11]"
```

**FIND usages** — grouped by file:
```csv
"find_usages","encenderMotor",5
"file","[lines]"
"src/motor_control.cpp","45,89"
"include/motor_control.hpp","34"
```

**SHOW outline** — grouped by kind, comments compressed to `len:N`:
```csv
"show_outline","include/types.hpp"
"fql_kind","[name,line]"
"comment","[len:18,1],[len:23,55]"
"type_alias","[int16_t,17],[int32_t,18]"
```

**SHOW members** — grouped by kind:
```csv
"show_members","MotorControl","include/motor_control.hpp"
"type","[declaration,line]"
"field","[uint16_t rpm_setpoint;,28],[bool is_locked;,51]"
"method","[void setRPM(uint16_t);,35]"
```

**SHOW body / lines / context** — 2 columns (line, text):
```csv
"show_body","convertByte2Volts","src/adc.cpp","42-44"
"line","text"
42,"float convertByte2Volts(uint8_t raw) {"
43,"    return raw * 3.3f / 255.0f;"
44,"}"
```

**SHOW signature** — single flat row:
```csv
"show_signature","setPeakLevel","src/signal.cpp",125,"void setPeakLevel(int level)"
```

**SHOW callees** — grouped by file:
```csv
"show_callees","setPWMDuty"
"file","[name,line]"
"src/pwm_driver.cpp","[writePWM,189]"
"src/timer.cpp","[updateTimer,405]"
```

**FIND files** — 2 flat columns. `error_count` and/or `parse_coverage` are appended **only** when
the query names them; a plain `FIND files` stays at two columns and pays nothing:
```csv
"find_files",142
"path","size"
"src/motor_control.cpp",12847
```

Mutations, transactions, and source ops keep their JSON format (already small).


---

## Raw line and file operations (legacy, non-indexed files)

The commands in this chapter operate on **raw byte ranges** and never touch the
index. They are **not** the way to edit indexed source — `CHANGE FILE` on an
indexed file is disabled and returns guidance pointing at the node commands
(`CHANGE NODE`, `INSERT … NODE`, `DELETE NODE`): a node handle survives edits
that shift line numbers; a line range does not. What remains legitimate here:

- editing files ForgeQL does not index (fixtures, generated output, plain text);
- file scaffolding — `COPY LINES` to seed a brand-new file, `MOVE LINES` to
  relocate content across files;
- deleting a file (`CHANGE FILE '<f>' WITH NOTHING` — works on indexed files
  too; `ROLLBACK` restores it).

### SHOW LINES

```sql
SHOW LINES n-m OF 'file_path' [clauses]
```

Returns a verbatim 1-based line range. Combine with `WHERE text MATCHES`/`LIKE` to
grep within the range before the output is returned.

### CHANGE FILE

```sql
CHANGE (FILE | FILES) file_list MATCHING 'old_text' WITH 'new_text'
CHANGE (FILE | FILES) file_list LINES n-m WITH 'new_content'
CHANGE (FILE | FILES) file_list LINES n-m WITH NOTHING
CHANGE FILE 'file_path' WITH 'new_full_content'
CHANGE FILE 'file_path' WITH NOTHING
```

| Variant | Effect |
|---|---|
| `MATCHING … WITH …` | Replace all literal occurrences across matched files |
| `LINES n-m WITH '…'` | Replace a specific line range |
| `LINES n-m WITH NOTHING` | Delete a specific line range |
| `WITH '…'` | Replace entire file content (creates the file if absent) |
| `WITH NOTHING` | **Delete the file** — the removal is staged on `COMMIT`; `ROLLBACK` restores it |

All variants are refused on indexed source files — with guidance to use the
node commands instead — except the whole-file `WITH NOTHING` deletion: naming a
file explicitly for removal is not raw-text editing, and the returned diff shows
the deleted content.

`file_list` is one or more comma-separated single-quoted globs; `FILE` and `FILES`
are interchangeable. Every `WITH 'content'` form also accepts a heredoc block
(`WITH <<TAG … TAG`, tag all-uppercase on its own line) when the replacement text
contains quotes.

### COPY / MOVE LINES

```sql
COPY LINES n-m OF 'src' TO 'dst' [AT LINE k]
MOVE LINES n-m OF 'src' TO 'dst' [AT LINE k]
```

Copies (or moves) source lines `n..=m` into `dst` before line `k`; the range is
appended when `AT LINE k` is omitted. **COPY** leaves `src` untouched; **MOVE**
deletes the range from `src` after inserting. Same-file moves are atomic. A
purely numeric `TO` destination is rejected (write `TO '<path>' AT LINE k`, not
`TO 3`).
