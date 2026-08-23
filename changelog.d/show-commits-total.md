- `SHOW COMMITS LIMIT 5` answered `total: 5` however many commits the session
  had made. The count was taken after the clauses had already cut the page, so
  every first page claimed to be the whole answer. It is now taken before the
  cut: `total` is every commit the clauses matched, and a `total` above the row
  count means there are commits the page did not show. Only an explicit `LIMIT`
  or `OFFSET` was affected — the implicit default page was already counted
  before it truncated. This closes the `SHOW COMMITS` total defect the previous
  release recorded as pinned rather than fixed.
