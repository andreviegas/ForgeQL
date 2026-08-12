- A `COMMIT` no longer rebuilds the index for the commit it just made. It used
  to merge the whole corpus into a new overlay every time — every name, every
  row, changed or not — so committing one edited comment re-indexed everything.
  The overlay is a cache over the segments the commit promotes, and only the
  commit something later attaches to is ever read, so a worktree that commits
  ten times built ten and threw nine away. It is now built by whoever attaches
  to that commit: `USE` on a commit with no overlay builds one, which is what
  the takeover, the review, or the branch landing on main already does.

  Measured on the Linux kernel — 80,426 files, 29,864,281 rows — a `USE`,
  one-node edit and `COMMIT` went from **778 seconds to 80**, and its peak
  memory from **59.1 GiB to 23.6 GiB**. Ten commits in a worktree fall from
  about two hours to about thirteen minutes, plus one build when the work is
  picked up.

  The session keeps answering from its base overlay plus its dirty one, which
  is the same set of rows it was serving a moment earlier: a commit moves
  changes into git, it does not change which rows a query should return. What
  grows instead is the cost of reading them, since rows in the dirty overlay
  sit outside the base overlay's name index and posting lists — a worktree
  that commits a very large number of files will notice its queries slowing
  before it is merged.

  Two details that keep the guarantees intact. Promoting a staged segment now
  links it into the store rather than moving it, so the delta file that
  restores the dirty overlay after a restart still resolves; without that,
  every committed file would revert to its pre-edit rows on the next
  reconnect. And a `COMMIT` still refuses when a segment its index would be
  built from has vanished from the store, naming the files — the build that
  refuses on this now happens after the session that could act on it has gone,
  so the same fault is reported at the commit instead.
