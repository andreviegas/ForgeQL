- `SHOW body`/`context`/`members`/`callees` and `FIND callees OF` now refuse an
  unknown or misspelled `WHERE` field by name, instead of reporting a
  misleading "no symbol matches" or "eliminated by filters" as if the symbol
  itself were the problem. A real field with no satisfying candidate still
  gets the old message, unchanged — only a genuinely unrecognized field name
  is now told apart. Both storage backends share the same field-recognition
  check, and the legacy backend's enrichment-field dictionary has been
  completed with several fields (`is_override`, `is_final`, `macro_expansion`,
  `macro_def_file`, `macro_def_line`, `macro_arity`, `enclosing_type`,
  `owner_kind`, `suffix_meaning`) that were previously recognized only by
  accident, when a workspace's own data happened to carry a matching column.
