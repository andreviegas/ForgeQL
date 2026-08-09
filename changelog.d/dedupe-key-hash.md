- `FIND symbols` no longer copies each candidate row's name, kind, path and line
  into an owned key while it removes duplicates. Collapsing duplicates means
  comparing every candidate against the ones already kept, and it did that by
  building a set of owned keys -- three heap allocations per candidate row, held
  for the whole pass on top of the result set it was scanning. It now stores a
  64-bit hash of those same four fields and confirms each hash match against the
  fields themselves, so a collision costs a comparison rather than changing the
  answer. The answer is unchanged for every input.

  Measured on a 2.9-million-symbol corpus with `FIND symbols`, which builds
  every row, over two runs: peak private memory 7,219 MB before against
  6,912 MB after, and 7,225 before against 6,915 after -- a saving of 307 MB
  and 310 MB. The same query ran 17.5 s before against 15.3 s after, and
  16.6 s before against 15.0 s after. Run-to-run variation on the "before"
  figures is larger than the gap between the two savings -- 6 MB against 3 MB
  for memory, 0.9 s against 0.6 s for time -- so read the pair of deltas rather
  than either single run.

  Both results are for a query that materialises every row. The memory saving
  was flat on a 20,000-row query -- 2,550 MB against 2,548 MB -- which is what
  a per-row saving of this size looks like at that scale; the time was not
  measured there, so nothing is claimed about it.
