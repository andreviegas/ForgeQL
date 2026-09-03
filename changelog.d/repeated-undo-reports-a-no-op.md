- A repeated bare `UNDO` no longer reports a restore it did not perform. Bare
  `UNDO` means `LAST-0` on every call and advances no cursor — deliberately, so
  that retrying a destructive verb after a timeout cannot step further back than
  the first call would have — which means a second `UNDO` addresses the slot the
  first one already restored. It rewrote those same bytes and answered
  `applied: true` with "restored 1 file(s)" naming the file, while the earlier
  mutation it appeared to be reversing stayed applied. An agent reading that had
  every reason to believe it had rolled back twice.

  A restore now skips any file that already holds the slot's bytes, and a call
  that rewrites nothing answers `applied: false` with an empty `files_changed`,
  `edit_count: 0`, and a note saying so and pointing at `UNDO LAST-n` for
  stepping further back — a distinct shape an agent can branch on. The note
  claims only what was checked, that every file the slot covers already holds
  its pre-edit bytes, and not that the tree is back at its pre-mutation state:
  a snapshot entry of zero bytes means both "was empty" and "did not exist", so
  the one mutation that CREATES an empty file is reported as nothing-to-rewrite
  while the created file is still there. Such a call also touches nothing
  downstream: a tree that did not change cannot have made the index, the `FOUND`
  set or a satisfied commit gate stale, so none of them is invalidated. A
  restore that does rewrite bytes behaves as before, except that `edit_count`
  and `files_changed` now count the files actually written rather than every
  file the slot holds.

  `UNDO` with nothing yet mutated in the session was checked at the same time
  and is NOT the same defect: it refuses with "nothing to undo" rather than
  claiming a restore. A test now pins that, so it cannot quietly become an
  empty success.

  Documenting the repeat turned up a second, older inaccuracy in the same
  paragraph and it is corrected too: `UNDO LAST-n` was described as "reversing
  the last n+1 mutations at once". A ring slot holds only the files ITS OWN
  mutation touched, so `LAST-n` rewrites those files and reverses the n+1 most
  recent mutations only where they touched the same files — a newer mutation to
  a different file survives it and needs its own call. A test pins that.
