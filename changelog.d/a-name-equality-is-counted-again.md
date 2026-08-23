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

  Nothing else moves. The tiers that only propose candidates propose fewer of
  them and the row filter decides the same rows; the scan already collapsed
  what the postings no longer carry; and symbol resolution keeps landing on the
  same node, since the row that survives a collapse is the first of its group
  and so the first candidate a resolver sees. What it can no longer land on is
  a later row that agrees with that one on name, kind and line.

  This changes the workspace index's stored content, so its schema version
  moves and every commit's index is rebuilt once.
