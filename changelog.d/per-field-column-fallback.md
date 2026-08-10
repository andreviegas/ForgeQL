- A segment carrying an enrichment column named after one of a result row's own
  fields no longer stops answering *every* such field from its columns — only
  the colliding name falls back. The two have to be told apart because an
  operator whose type does not match the struct accessor falls through to the
  enrichment map of the built row and finds the shadow column there, which the
  row view would never consult; but a collision on one name says nothing about
  the others, and the whole segment was being sent to the slower path for it.
  No query answer changes — only how early a predicate is decided. Predicates on
  the fields a segment does not shadow are filtered before their rows are built
  again, as they are in every other segment. The effect on a real corpus has not
  been measured; segments carrying such a column are the unusual case.
