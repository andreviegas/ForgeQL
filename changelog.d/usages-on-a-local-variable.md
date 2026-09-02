- A local variable no longer reports a `usages` count. The column is the
  workspace-total number of `role = 'code'` occurrence sites of a row's NAME,
  which is a real dependency count where the name identifies the symbol across
  the workspace and meaningless where it does not: the engine does not resolve
  scoped references, so all 11,964 local bindings named `ret` in a Zephyr
  checkout each reported the same 83,611, and the blast-radius workflow the
  agent docs teach reads that as references. Such a row now carries no value
  for the field at all, and every reader agrees on the absence — it matches no
  `usages` predicate (`= 0` included, so the dead-code recipe no longer sweeps
  in every local binding), it renders an empty metric column instead of `0`,
  and under `ORDER BY usages` it ranks behind every row that has a count in
  both directions rather than tying with one, which is what the comparator did
  before: its numeric branch fires only when both rows answer with a number,
  and `usages` has no string form to fall back on, so a local and a function
  compared equal and a name tie-break decided the page. The rule is written
  once and every reader reaches it from there: the query-time stamp on the
  columnar backend, the row build on the in-memory one, and the in-memory row
  VIEW its own symbol lookup filters through — a reader that builds no row at
  all, and the one the first draft of this change left answering the raw
  count, so `SHOW context OF '<local>' WHERE usages >= 0` resolved a row
  `FIND symbols` had stopped answering for. A file-scope variable and a
  function are untouched; a variable row carrying no `scope` enrichment keeps
  its count on purpose, which today leaves function parameters counted by name.

- `WHERE usages` now answers the same on a symbol lookup as on `FIND`. The path
  that resolves the name for `SHOW body`, `SHOW signature`, `SHOW outline` and
  `SHOW members` read the count two ways and both were wrong: its segment prune
  mapped the field onto the per-segment `usages_count` column, a stale
  always-zero legacy field, and the candidate row it then tested the predicate
  against had never been stamped with the workspace total. So
  `SHOW body OF 'f' WHERE usages > 0` answered "no symbol matches" for a symbol
  `FIND` reports usages for, and `WHERE usages = 0` matched every symbol. The
  `FIND` path had always skipped that prune; the lookup now skips it too and
  stamps the row before reading it.
