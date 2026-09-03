- `SHOW members` no longer omits a decorated method. A grammar routinely puts a
  member inside a node of its own — Python wraps a decorated method in a
  `decorated_definition` with its decorators, C++ wraps a template member in a
  `template_declaration` with its parameter list — and the wrapper, not the
  member, is the direct child of the class body. The classifier tested each
  direct child against the language's field / function / declaration /
  enumerator kinds, so every wrapped member matched none of them and was
  dropped: `torch.nn.Module` declares `to` four times and `state_dict` three,
  and the listing showed one of each. A verb whose whole purpose is
  completeness was silently omitting every `@property`, `@staticmethod`,
  `@classmethod` and `@overload` in every Python class.

  Which kinds are wrappers is DECLARED by the language, in a new
  `member_wrapper_kinds` config key — `decorated_definition` for Python,
  `template_declaration` for C++. The classifier looks through a node of a
  declared wrapper kind for the single member inside it, and a language that
  gains a wrapper adds it there rather than in the engine.

  Inferring the wrappers instead — "a node that names no member and holds
  exactly one member child" — is the obvious design and it is wrong twice. It
  admits a C++ `friend_declaration`, whose single function is not a member of
  the class at all, so a verb that had merely been incomplete would begin
  asserting something untrue; and the only way to keep that out without naming
  wrappers is to forbid unwrapping to a declaration, which then loses a real
  member — a template method declared in the class and defined out of line is a
  `template_declaration` wrapping a `declaration`. Both halves are STRUCTURAL,
  read off the grammars rather than observed: the corpus that was measured holds
  no out-of-line-defined template member and no friend the inferred rule was
  ever run against, so neither was seen listed. What IS measured is that the
  declared rule recovers real C++ members at all — a nested class in that header
  lists 6 methods before the fix and 7 after, the seventh an inline conversion
  operator inside a `template_declaration`. A separate measurement is what
  killed the inferred design outright: an earlier draft listed 24 of `Module`'s
  class attributes as methods, because Python declares `assignment` a
  declaration kind and a class body wraps every attribute in an
  `expression_statement`. Golden cases pin all three outcomes — no member row
  that is not a `def` line, no C++ member row without a parameter list, and no
  member row naming a friend operator.

  A row carries the INNER node's line and text, so a decorated method is listed
  by its `def` line: the decorator says how a method is bound, not what it is.
  A golden case pins that line directly — an overload whose decorator sits one
  line above its `def` must be listed at the `def` — because a count of the
  overloads cannot tell the two lines apart. The handle on that row still
  resolves to the indexed `function_definition` row, whose span is folded back
  to its leading decorator — the index emits no row for the wrapper itself — so
  the listed line and the handle's own start line differ for a decorated method
  exactly as they already do for a C++ field declared under an attribute.

  Two censuses are pinned but NOT claimed equal, because they are not. On
  `torch.nn.Module` the listing reaches 76 methods where the row path
  attributes 81; the 76 is the complete one, and the five extra are
  method-local closures that `enclosing_type` attributes to the class although
  they are not members of it. On a C++ class the listing reaches 78 where the
  row path attributes 72; that delta is a cardinality difference whose
  composition is not yet measured, and it is recorded as such rather than
  guessed — the Python residue was described from inference first and the
  inference ran backwards. Pinning both sides means closing either shows up as
  two numbers moving together, and a regression as one moving alone.

  Rust is exempt by construction rather than by omission: a Rust method is
  declared in an `impl` block and not in the struct body, so `SHOW members` over
  a struct lists its fields and a parity claim there would assert something
  false. That is pinned too.
