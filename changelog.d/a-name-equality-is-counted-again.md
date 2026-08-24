- The workspace index's name postings now hold each file's answer rows rather
  than its raw ones. A file can produce two rows that agree on name, kind and
  line — one addressable node each, one row of any answer — and the collapse
  that makes them one ran everywhere except here, so a name posting carried
  both. Two things read that as an answer rather than as a proposal, and both
  were wrong by the same rows: `WHERE name = '<value>' GROUP BY file` counted
  57 rows in a file holding 48, which is why that shape had to be answered by
  the slower scan instead; and the ascending name page spent one of the places
  it had been asked for on a row that later merged into its neighbour, so a
  `LIMIT` could come back short while the reported total, taken from the stored
  collapsed counts, counted neither of them.

  Both are fixed at the source: the postings are intersected with the same
  canonical row set the kind postings have always been intersected with, and
  `GROUP BY file` counts a name equality again. Name PATTERNS stay with the
  scan at every literal length — the structure serving them over-generates by
  construction, which no amount of literal is going to settle.

  The tiers that only propose candidates propose fewer of them and the row
  filter decides the same rows, and the scan already collapsed what the postings
  no longer carry. Symbol resolution is the one place where a different node
  could come back, and the direction is worth stating plainly: it picks the
  LAST candidate, over rows walked in ascending order, so where a file held such
  a pair the higher row used to win and the lower one wins now. The two agree on
  name, kind and line and can differ in byte range, so the span a
  `SHOW body OF '<name>'` returns could move. Forty function names swept through
  `SHOW signature` before and after came back byte-identical, which is what the
  names people resolve by look like — none of them duplicated. It is evidence
  about those names rather than a property of the ordering.

  This changes the workspace index's stored content, so its schema version
  moves and every commit's index is rebuilt once.
