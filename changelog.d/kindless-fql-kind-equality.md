- `WHERE fql_kind = ''` now answers the rows `GROUP BY fql_kind` counts under
  the empty name, and so does `WHERE fql_kind = 'unknown'`, the spelling
  `SHOW outline` renders for the same rows. Both used to return zero — 0 rows
  against 2,169 on this repository, 17,830 on a Zephyr checkout and 349,599 on
  a PyTorch one — while the grouping published those rows as a group of their
  own, so an agent reading the documented rule that an empty answer on this
  field is a fact about the corpus was told something untrue about the only two
  values where it was not.

  Four separate places had to change together, and fixing any three leaves a
  green test suite. The overlay builder now writes a posting for the empty kind
  like any other kind, so the equality is one binary search and one bitmap
  decode — the same cost as `= 'function'` — rather than a lookup that finds no
  entry and is turned into an empty result instead of a scan. The parser spells
  `unknown` to the stored empty value at the one place a written query becomes
  an internal one, so every verb and both storage backends see one value and the
  two spellings cannot drift apart a reader at a time; a `LIKE` or `MATCHES`
  carries a pattern rather than a value and is left as written, matched against
  the spelling the verb rendered. And a row whose kind is empty now reports that
  as the value it is, where every reader used to collapse it to "no value at
  all", which is what the scan needed: such a row previously failed the equality
  AND its negation, so
  `FIND symbols WHERE fql_kind NOT MATCHES '<no match>' GROUP BY fql_kind`
  reported 41 groups where the same grouping without the predicate reported 42.
  A row shape that has no kind column — a `FIND usages` site, which is one line
  of one file — still reports no value and still matches nothing, on either
  spelling, exactly as before. The in-memory backend kept its own copy of the builder's skip, in the secondary index it consults for an equality; that mattered more than a smaller index, because once the index has supplied the candidates the predicate is stripped, so an unindexed value reached no scan that could have decided it and answered 0 rows with a success status. It is indexed and counted like any other value now, and a source with no configuration file answers these two the same way a fully indexed one does.

  The overlay schema version moves to 17. There is no layout change and no
  re-indexing: overlays built by an older binary are missing the new posting, so
  they are rebuilt on first use. `SHOW outline … WHERE fql_kind = 'unknown'`,
  which already worked, still answers the same rows.
