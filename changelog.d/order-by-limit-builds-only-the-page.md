- `ORDER BY <field> LIMIT k` no longer builds every matching row of a file in
  order to throw almost all of them away. A file whose rows the running top-K
  trim was going to shed now ranks them from its own stored columns, read in
  place, and builds only the ones that survive. The rows returned do not change:
  the ranking uses the same comparator that sorts the built rows, at the same
  threshold and keeping the same number, so what it sheds is what the trim would
  have shed on the very next statement. Rows the comparator ranks equal are the
  one exception, and only in the sense they always were — where more rows tie on
  the `ORDER BY` field and on `name`, `line` and `path` than a page can hold,
  which of them it shows was never decided by the ordering and still is not.
- The choice is made per file, so one file cannot switch it off for the rest. It
  is made only where a file can rank by every field that comparator reads — the
  `ORDER BY` field and the `name`, `line`, `path` tie-breakers — and where no
  predicate is left to run once a row exists. `ORDER BY usages` is therefore
  still built first, because the stored count is a stale zero that the workspace
  count replaces during materialisation, as is any ordering by a field a file
  stores under a name of its own. A field a file simply does not carry is fine:
  it reads as absent both before and after a row is built, and ranks the same
  either way.
- The `k` no greater than 1000, no `OFFSET`, no `GROUP BY` and no `HAVING` gate
  is unchanged, and so is the caveat that duplicate rows are collapsed after the
  page is chosen.
