- **The two older counted `GROUP BY` routes now answer what the scan answers.**
  `GROUP BY fql_kind` and `GROUP BY file` are read from stored counts instead of
  by building every matching row. Three ways that produced a number no row had
  been consulted for are fixed, all at query time; nothing stored changes.

  **A counted `GROUP BY file` no longer counts what nothing verified.** It
  intersected whatever candidate bitmap the index tiers proposed with each
  segment's row range and cleared the residual `WHERE` before delivering, so no
  candidate was ever tested against its own row. On a Zephyr checkout
  `FIND symbols WHERE name LIKE '%ab%' GROUP BY file ORDER BY count DESC`
  reported 32,612 files where 11,099 hold a matching name, and gave its top file
  76,310 rows where 203 match — more than that file's whole deduplicated row
  count of 67,595, because the name postings carry each segment's raw rows. The
  one predicate now admitted is `fql_kind = '<value>'`, whose postings are
  intersected with each segment's canonical rows when the index is built and are
  the only tier that both verifies and deduplicates. Every other shape is
  answered by the scan, which decides a row by reading it. No `name` predicate
  is admitted at any literal length — the trigram index over-generates by
  construction, and nothing at query time can settle a candidate, since the
  canonical row set is stored as a per-segment count and not as a set. That is
  slower than the wrong answer was, and on a large enough corpus such a query
  can now be refused by the result budget where it used to return a number.
  Admitting a `name` predicate again means intersecting the name postings with
  the canonical rows at index build, which changes stored index output.

  **A counted `GROUP BY fql_kind` no longer drops the rows that have no kind.**
  The kind postings skip the empty kind when the index is built, so those rows
  were in no group and nothing reported that they had gone: 2,169 of 59,636 rows
  on this repository's own corpus, 41 groups where the scan finds 42. They are
  now the remainder — the canonical row total of the selected segments, less the
  rows the kinds account for — which is the group the scan keys by the empty
  string. Where that subtraction cannot hold, the query goes to the scan rather
  than carry a count derived from a contradiction.

  **A `HAVING` or `ORDER BY` naming a field a group row does not carry is
  answered by the scan.** A counted group row carries the grouped value and its
  count and nothing else, so both routes evaluated such a predicate against rows
  that could not answer it and returned an empty set with full confidence:
  `FIND symbols GROUP BY fql_kind HAVING lines >= 2` answered nothing against
  the scan's six groups. Both now hand the shape back, which is the test the
  enrichment grouping already applied.

  No fixed bench class covers a `GROUP BY`, so the figures above are answers
  rather than timings, taken through the engine on the frozen Zephyr and
  ForgeQL corpora.
