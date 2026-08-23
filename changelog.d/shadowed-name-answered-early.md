- A `WHERE` on `name`, `fql_kind`, `language`, `path` or `line` is now tested
  against a segment's stored columns even when that segment also carries an
  enrichment column of the same name. It used to be handed to the filter that
  runs after the rows are built, on the grounds that such a column shadowed the
  field — which covered almost everything: an enrichment column named `name` is
  a tree-sitter grammar field on essentially every definition node, so it exists
  on **292 of the 293** segments a `WHERE name …` scan selects on this
  repository, and on **7,267 of 9,278** on a 3-million-symbol Zephyr tree. On
  those segments every matching row was built before the predicate that would
  discard it had run.

  Nothing shadowed anything. A built row reads those five names from its own
  struct through one of its two field accessors, and from its enrichment map
  through the other — `WHERE name = 'x'` reads the struct, `WHERE name = 42`
  reads the column — so the reader now derives which accessor a predicate will
  use, from the operator and the value type, and follows it to whichever of the
  two the built row would have read. Answers are unchanged, and pinned that way:
  the same query is asserted against the same total on both routes.

  No timing is claimed here; what is measured is how many segments stop
  building rows they were about to discard, which is the figure above.
