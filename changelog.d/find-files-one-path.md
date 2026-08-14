- A complete `FIND files` result no longer takes a different route from a
  truncated one. Arming the result set's master rev re-derived every member
  through the engine — and each directory member's lookup walked the whole
  worktree, so an unbounded listing of a 95,000-file corpus burned upward of
  fifteen minutes after the rows themselves were ready in seconds, while
  `LIMIT 90000` (truncated, so never armed) returned in under forty. The set
  is now armed from the revs already carried by the served rows, for every
  FIND verb; the fresh per-member derivation remains where it belongs, in
  the `IF REV` verification before a bulk mutation. The total is counted in
  the same single clause pass that filters the rows — no clone, no second
  pass — and `LIMIT` only windows the end of that one pipeline. Without
  `LIMIT`, `FIND files` now serves the standard 20-row FIND page with the
  honest total, so the previously unbounded verb pages like every other
  FIND. A directory-heavy scaling test pins that a complete listing costs
  the same route as a truncated one, and a parity test pins that a master
  rev armed from served rows verifies against the fresh lookup.
