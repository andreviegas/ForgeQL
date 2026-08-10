- The bound on how much one `FIND symbols` may materialise is now stated as the
  memory it is meant to protect — 2 GiB of result rows — rather than as a row
  count. It had been a bare five million rows, chosen when a result row was much
  smaller and never revisited as the row grew; measured against today's rows
  that authorised roughly 7.5 GB, against a budget of 2 GiB. The count is now
  derived from the budget and the working cost of a row, which works out at
  about 1.34 million, and a test pins the derivation so the next growth of the
  row moves the bound instead of quietly widening what a query may spend.
- **This lowers the effective ceiling about 3.7x, so a scan that used to
  complete can now be refused.** That is a reachability change rather than a
  restatement, and where the new line falls on a multi-million-symbol corpus has
  not been measured. The case to watch is a `GROUP BY` that no fast path
  accepts: its answer is a handful of rows, but it materialises every matching
  row to get there. `FORGEQL_FIND_MAX_ROWS` raises the bound in rows and `0`
  disables it.
- The refusal is unchanged in kind — past the bound a query has always been
  refused rather than truncated, which is what keeps a partial answer from
  passing for a complete one — and it now names two remedies that hold. Narrow
  the scan with `IN 'path/**'` or a more selective `WHERE`; or order it, since an
  `ORDER BY <field> LIMIT k` with `k` no greater than 1000, no `OFFSET` and no
  `GROUP BY` still scans every segment while a running top-K trim holds the
  working set to a few thousand rows, so that form returns the true top K and
  never reaches the default bound. Outside that gate nothing is trimmed and the
  query is refused as before. The ordered form has one hole of its own, described
  in its own entry: the trim sheds rows before duplicates are collapsed, so the
  page can come back short.
- The bound is now documented, in `doc/syntax.md` and in each of the agent
  guides. It was enforced but written down nowhere, so the first an agent knew
  of it was a refusal citing an environment variable it had never been told
  about.
- What the bound does not cover, stated wherever it is claimed: it is enforced
  on the `FIND symbols` scan over the on-disk index and nowhere else — and there
  it is tested once per segment rather than once per row, so the real peak is
  the budget plus the one segment being materialised. `FIND usages` builds its
  rows in one step on both backends, `FIND files` pushes one entry per file with
  no bound at all, the in-memory backend materialises its whole result before any
  clause applies, and a session's uncommitted rows are unioned in after the
  check. A query answered by one of those can still exhaust host memory.
