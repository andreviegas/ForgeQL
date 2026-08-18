- `FIND symbols` without a `LIMIT` no longer builds every matching row to
  show its default page. The engine used to be asked for the whole answer and
  the twenty-row default was cut afterwards, so a bare `FIND symbols` on a
  three-million-symbol corpus materialised result rows up to the 2 GiB row
  budget and was then refused with the budget error — to show twenty rows.
  The default page is now handed to the engine as the LIMIT an explicit
  `LIMIT 20` would carry, so the query takes the paths that limit already
  takes, under those paths' own gates: the name stream when no clause
  declines it (no WHERE/IN/EXCLUDE, unique source paths), otherwise the
  running top-K trim (no OFFSET, unique source paths); both read and count
  every matching row but hold only the page, and a shape outside both gates
  — an OFFSET under a WHERE, an index with two segments built from one source
  path — materialises exactly as before. Rows and `total` are unchanged — the
  ordering the trim ranks by is the one the full sort used before the cut, and
  both routes count rather than truncate; two golden cases on the
  `forgeql-pub.frozen` golden corpus pin the twenty rows and its whole-corpus
  total (59,636) on each route. Queries with `GROUP BY` or `HAVING` are left
  exactly as written (their rows are aggregates, and a HAVING runs after the
  page is cut, so a
  bound there would change the answer). Measured on the frozen 3M-symbol
  corpus (`bench_mem`, ci profile, branch vs main — the branch also carries
  the in-place postings change, which accounts for the `USE`-only floor
  moving 622 → 380 MB): the bare `FIND symbols` (`full_scan`) went from
  2,365 MB of private heap at peak and a refusal to 380 MB — the `USE` floor —
  and a completed page; `kind_scan` (`WHERE fql_kind = 'if'`, no LIMIT, now
  the trim route) from 852 MB and 634 ms to 384 MB and 298 ms; `name_stream`
  (asks for 20,000 rows with its own LIMIT; untouched here) 172 → 185 ms, inside
  the harness's floor. What did not move: rows and `total` on every route
  (the golden pins above), and any query written with its own LIMIT, GROUP BY or HAVING.
  The budget error text, `doc/syntax.md`, `doc/architecture.md` and the four
  agent docs state the new gate in the same sentence as the old one; the error
  text's stale claim that the `ORDER BY name` stream "reports k as its total"
  is corrected there too — the streams report the stored deduplicated count.
