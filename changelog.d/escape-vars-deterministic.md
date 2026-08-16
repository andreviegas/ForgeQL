- `escape_vars` no longer spells the same variables in a different order on
  every run. The macro-escape scan iterated a hash set, and the field is emitted
  in discovery order, so one source file produced a different value each time it
  was indexed — enough to change an overlay checksum on a macro-heavy corpus,
  which made a single before/after comparison an unreliable check. The scan now
  iterates a sorted view, as the sibling `escape_kinds` already did.
