- A `LIMIT` with no `ORDER BY` no longer changes the answer. It used to stop the
  scan after `LIMIT + 1` rows, so `total` came back as the size of the page
  rather than the number of rows that matched, an `OFFSET` paged past rows that
  were never fetched, and raising the limit could surface rows a smaller one had
  not shown. Such a query is now bounded the way an ordered one is — a running
  top-K trim across the whole scan — because the pipeline already sorts by
  `(name, line, path)` when no `ORDER BY` is given, so `LIMIT k` asks for the k
  smallest rows under that ordering rather than for whichever k the scan reached
  first — with one qualification the ordering never settled in the first place:
  where more rows than the page holds compare *equal* on the ordering field and
  on all of `name`, `line` and `path`, which two rows can be while differing in
  `fql_kind`, the bounded partition is unstable and raising the limit may still
  swap which of them is shown. Four cases in the golden suite that recorded the
  old behaviour as a known defect are now enforced instead.

  Two consequences worth stating. The trim needs `k` no greater than 1000, no
  `OFFSET`, no `GROUP BY` and no `HAVING`; outside that gate an unscoped scan
  materialises every matching row and can now be refused by the row budget where
  the old cap let it complete with a wrong answer. And the candidate row IDs a
  scan holds before it builds anything now have a bound of their own, about 537
  million — the same 2 GiB of memory measured against four bytes a row ID
  instead of 1,600 a built row. `FORGEQL_FIND_MAX_ROW_IDS` overrides it and `0`
  disables it, matching `FORGEQL_FIND_MAX_ROWS`.

  The in-memory backend carried the same defect at its own early exit and is
  fixed the same way: it too builds and ranks every matching row, holding the
  retained set to a bounded window instead of keeping whichever rows the scan
  reached first. That change is visible in the suite — a test on that backend
  had been asserting a value appeared in a page of 100 that only a scan-ordered
  page would have held, and it now asks for the value by name, with a sibling
  case asserting that a small LIMIT returns the head of a larger one's page.
