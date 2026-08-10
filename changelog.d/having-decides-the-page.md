- **A `HAVING` predicate now decides which rows a page contains, rather than
  which of an already-chosen page survive.** `FIND symbols … HAVING … ORDER BY
  name LIMIT k` returned the wrong rows, silently: ordering by name walks the
  name index and stops once it holds `k` rows, and `HAVING` is applied after
  that, so the answer was the first `k` rows by name minus those failing the
  predicate instead of the first `k` rows by name that satisfy it. Rows that
  qualified were never fetched, nothing was truncated in the reply, and no error
  was raised. A limit far larger than the answer truncated it just the same, and
  reversing the direction changed which rows came back at all — on one corpus a
  seven-row answer came back as three ascending and one descending.
- The same omission was in three more places, found by asking where else the
  engine stops reading early: the running top-K trim, which sheds rows mid-scan
  by the ORDER BY; the segment fetch cap, which stops opening segments once it
  holds `LIMIT + 1` rows and so broke the same query shape with no `ORDER BY` at
  all — a seven-row answer came back as none at `LIMIT 3` and as two at
  `LIMIT 1001`; and the in-memory backend's own early exit, which had the same
  gap. That backend answers every query for a source with no `.forgeql.yaml`,
  since no columnar index is built for one, so the gap was reachable in ordinary
  use rather than only through an explicit choice of backend. All are now gated
  on `HAVING` being absent through one shared predicate, so the two backends
  cannot drift apart on it again and the next stage added after paging has a
  single place to declare itself.
- One gap in the coverage, stated because it would otherwise be invisible: the
  in-memory backend's fix is not pinned by a test. The query-level cases run
  against the columnar backend — every corpus they can use carries a
  `.forgeql.yaml`, and installing a columnar index drops the in-memory table — so
  removing that one condition leaves the whole suite green. The fix is derived
  from the code path rather than demonstrated by a failing case, and the test
  file says so where a reader will meet it.
- **This is the third defect of one shape**, and worth naming: an early exit that
  assumes `LIMIT` bounds the answer while a later stage still filters. The other
  two are still open, each pinned as a known defect rather than fixed. A bare
  `LIMIT` with no `ORDER BY` truncates the scan instead of paging it, so the
  reported `total` falls short and an `OFFSET` pages past rows never fetched —
  four `expect_fail` cases in
  `crates/forgeql/tests/golden/clause_pipeline.json`. And duplicate rows are
  collapsed only after the top-K trim and the name-index streams have stopped
  reading, so an `ORDER BY … LIMIT k` page can come back shorter than `k` — an
  ignored test, `crates/forgeql-core/tests/topk_trim_before_dedupe.rs`. Each was
  found separately, and each cost a separate investigation to attribute. Anything
  that stops reading early has to account for every stage that still removes
  rows.
