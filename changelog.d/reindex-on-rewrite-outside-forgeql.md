- A file rewritten outside ForgeQL — by a formatter, a build step, an editor —
  after the session had already edited it is now re-indexed by the next
  command that names it, instead of being read and edited through the stale
  index until the next mutation. The command answers the file's current lines
  — a read or `FIND NODE` says so in a `hint`; a mutation carries no hint, its
  boundary diff shows them; an edit whose `IF REV` predates the rewrite is
  refused with `rev_mismatch` where the node's bytes changed (the refusal's
  JSON payload carries a `reindexed` field naming every file the check
  re-indexed before it refused), an edit whose rev still matches (the node
  moved but did not change) lands on the node's current lines, and a form that
  quotes no rev — an EOF append to a whole-file handle, `COPY NODE … TO`,
  `COPY NODES FOUND TO` — is
  checked the same way but has nothing to refuse, so it writes from the
  re-indexed bytes unannounced.
  `SHOW NODE '<id>(n-m)'`, `SHOW NODE … WHERE line >=`,
  `SHOW NODE … WHERE text LIKE`, `SHOW LINES`, `SHOW outline` of a file or a
  handle (a directory or glob outline lists many files and is not checked),
  `SHOW body`, `SHOW context`, `SHOW signature`, `SHOW callees`,
  `SHOW members`, `FIND NODE`, every node-addressed mutation including
  `MOVE NODE` and `COPY NODE`, and the `FOUND` sweeps are all gated the same
  way; a `FIND` over the corpus names no file and is not, so its rows can
  carry a stale line for such a file until that file is read or edited, and
  `UNDO` and `ROLLBACK` are not either — they restore whole files from
  ForgeQL's own snapshots over whatever a rewrite left since.

  The check costs one repeat of the lookup the verb itself runs — a gated
  command resolves its symbol or handle twice, on the same index tier, with
  nothing newly scanned — one `stat` per file a command names, a content hash
  of that file the first time the session names it and whenever its size or
  mtime moved since the last check, and a re-index of that one file when the hash
  differs from the indexed content; after a re-index the naming is repeated
  until a pass re-indexes nothing, since a re-index can change which file a
  symbol resolves to. Mapping an answer's lines to handles then hashes the
  file once more, uncached, so a read that renders handles and a mutation's
  boundary diff pay a second hash of that file. The notice is verified, not
  assumed: the file is hashed again after the re-index, a re-index that does
  not leave it fresh refuses the command with its reason rather than
  answering from the old rows, and a file that could not be hashed is never
  recorded as verified. The check reaches only what the stale index can still
  name — a symbol the rewrite introduced or renamed answers "no symbol
  matches" with nothing re-indexed until the file is read by handle or by
  path; a file created outside ForgeQL has no index to be stale and stays
  unindexed until ForgeQL itself writes it or the next attach rebuilds the
  index (a reconnect re-indexes only the tracked files `git diff HEAD` lists,
  never an untracked one); a file deleted or made unreadable outside ForgeQL
  keeps its rows until ForgeQL next writes or re-indexes it, and a read of it
  fails on the missing bytes. A rewrite that leaves both the size and the
  mtime untouched is not seen until either moves. On the in-memory backend —
  a source with no `.forgeql.yaml` — nothing changes: it stores no per-file
  content id, so it answers from its table until ForgeQL next writes or
  re-indexes the file (the ordinal handles it prints on SHOW rows do not
  resolve through `SHOW NODE`/`FIND NODE` anyway).

  What was wrong: the gate already existed for a file the session had not
  touched, but a file the session had edited carries a per-session segment,
  and that segment was taken as fresh without looking at the disk again. A
  read into the region a reformat had shifted then walked rows whose stored
  start lay past their current end; the subtraction underflowed, which is a
  panic under overflow checks — without them the row's clamped line range is
  empty, so those lines simply carry no handle. That walk now skips such a
  row, and answers nothing off a segment whose content id no longer matches
  the file, whichever kind of segment it is.

- `forgeql-server` now reports an engine panic as the failing request's error,
  as the stdio MCP server already did, instead of letting it unwind through
  the request task — which closed the connection with nothing sent while the
  server kept running. The message is the same on both transports:
  `engine panicked: <the panic's own text>`.

- `SHOW outline OF '<node_id>'` — the subtree form — now answers from the
  session's own rows first, like every other read by handle. It used to
  prefer the committed segment whenever one existed, so after any edit of the
  file in the session (through ForgeQL or, now, re-indexed by the freshness
  gate) the subtree came back at its pre-edit lines and revs for as long as
  the session lived. The by-file and glob forms were not affected.
