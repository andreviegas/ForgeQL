- A bare `LIMIT` is documented as what it is: delivery paging, not a way to bound
  an oversized scan. Without an `ORDER BY` a `LIMIT` stops the scan early rather
  than paging its result, so an `OFFSET` pages past rows that were never fetched
  and which rows come back depends on the limit itself. Four `expect_fail` cases
  in `crates/forgeql/tests/golden/clause_pipeline.json` pin that defect. An
  `ORDER BY <field> LIMIT k` — with `k` no greater than 1000, no `OFFSET` and no
  `GROUP BY` — does not have that problem: it scans every segment and a running
  top-K trim bounds the working set, so it is the ordered form, not the bare one,
  that the result-budget refusal now offers as a remedy. Outside that gate
  nothing is trimmed. The ordered form has a separate hole, described in its own
  entry: the trim sheds rows before duplicates are collapsed, so the page can
  come back short.
- Related, and stated at each of those sites: under any explicit `LIMIT` the
  `total` that `FIND symbols` reports is the number of rows returned, not the
  number that matched, because the `LIMIT` is applied before the total is taken.
  `FIND usages` deliberately differs — its `total` is the true site count, which
  is what a rename campaign measures progress against.
