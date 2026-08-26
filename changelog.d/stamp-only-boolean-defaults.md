- **`has_todo`, `has_escape`, `has_shadow` and `is_recursive` now answer their
  `'false'` value.** Each is written onto a row only when it holds, so nothing
  stored the negative, nothing selected it, and `WHERE has_todo = 'false'`
  answered an empty page on every corpus — indistinguishable from a claim that
  no function lacks a marker. Two other boolean fields, `has_doc` and
  `is_magic`, stored both values and answered thousands, which made the gap an
  inconsistency rather than a design.

  Nothing new is stored. The index already implied the answer, so it is
  computed: the rows the enricher examined, minus the ones it wrote. Which rows
  those are takes two facts, not one — the applicable KINDS, and the applicable
  LANGUAGES — and both are declared once, in the field table, beside the value
  itself. Every reader derives from that one declaration: the workspace bitmap
  that proposes candidates, the per-segment posting reader, the per-row
  evaluator that settles each one, the `ORDER BY` comparator, the grouping key
  and the group label the result renders. That mattered more than it sounds:
  while the per-segment reader still had its own opinion, every segment that
  posted the field lost all of its rows to a value lookup that could not find a
  value nothing stores, and the answer came back short by an amount that changed
  per field and per corpus.

  **The language half is the part worth reading twice.** An enricher gates on
  more than the node kind. It gates on a language capability the config
  declares, and on the shape of the grammar node it is handed; either can make
  it return before reading a byte. Python declares no address-of operator, so
  escape analysis has never run on a Python function. CMake and Make declare no
  comment kind, so no marker scan has read one of their bodies — and a CMake
  function carries no `body` node at all, so the shadow walk never starts on one
  either. Answering `'false'` for such a row would publish a claim about an
  analysis that did not happen: on PyTorch, "no local escapes" about 138,126 of
  194,114 function rows. They answer neither value instead, exactly as a row of
  an unexamined kind does. `has_escape` is confined to C, C++ and Rust;
  `has_todo`, `is_recursive` and `has_shadow` to those plus Python. The first
  three lists are recomputed from the shipped language configs by a test that
  fails on drift; `has_shadow`'s is recomputed from the shipped grammars by
  another, since no config declares the gate it turns on.

  What moved, per shape, measured on the corpora the tests are grounded on
  (each was zero before):

  | shape | after |
  |---|---|
  | `has_todo = 'false'` | 3,260 here · 94,944 on Zephyr · 190,213 on PyTorch |
  | `has_shadow = 'false'` | 95,368 on Zephyr, 6,124 of them Python |
  | `has_escape = 'false'` | 87,331 on Zephyr · 55,298 on PyTorch, none of them Python |
  | `is_recursive = 'false'` | 95,829 on Zephyr |
  | `has_todo != 'true'` | 3,260 here |
  | `has_todo LIKE 'fal%'`, and the `MATCHES` family | 3,260 here |
  | `GROUP BY has_todo` | the rows that carry nothing were one group; they are now two — the functions the scan read, named `false`, and everything it never examined, still unnamed |

  `ORDER BY` on one of these fields reads the same resolved value, so a row that
  answers `'false'` now sorts with the falses instead of with the rows that
  carry nothing. For a two-valued field that is visible only where the result
  mixes examined and unexamined rows, since `''` and `'false'` both sort below
  `'true'`.

  Two boundaries besides the languages, each stated beside the claim in every
  field table:

  - **The other stamp-only booleans are unchanged.** `has_fallthrough`,
    `is_const`, `expansion_failed`, `expanded_has_escape`, `is_mutable`,
    `is_unsafe`, `is_async` and `is_generic` still answer nothing for
    `'false'`. Each is one row of declaration away; none was added here.
  - **A row of a kind outside the declared set keeps its old behaviour, even
    where the enricher examined it.** The declaration names `fql_kind`s, and a
    language may declare a raw kind a function kind while mapping it to another
    `fql_kind` — cmake does that with `macro_def`. Such a row is stamped when
    the field holds and answers nothing when it does not, exactly as before: 69
    rows on Zephyr, 43 on PyTorch, none here. Covering them would mean putting
    language-specific kind names in the engine.

  What this does NOT model, and must not be read as modelling: how well an
  enricher that DID run reads the code. A walk that misses a position reports
  too few `'true'` rows and the complement then reports too many `'false'` ones
  — the default inherits exactly the accuracy of the value it complements. That
  is a defect in the enricher, fixed in the enricher, and a different thing from
  the never-examined rows the declaration excludes. The clearest live instance:
  Rust's config names one comment kind, so a `/* TODO */` is invisible to the
  marker scan, `has_todo` reports too few `'true'` rows on Rust and `'false'`
  inherits exactly that. Those rows stay answered, because the scan ran on them.
  The CMake rows do not, and the difference is whether anything ran at all.

  Cost, and its tier. The stored value is served from a posting; the default
  cannot be, so the query proposes the rows the declaration speaks for — the
  applicable kind bitmap, narrowed by a language check — and then reads each
  proposed row through the same row view an ordinary residual `WHERE` uses.
  That ceiling is the corpus's function rows minus the languages excluded:
  95,902 row reads for `has_todo` on Zephyr, 89,743 for `has_escape`, narrowed
  further by any `IN`, and paid only by a query that asks for the default.

  The language check itself costs a `u32` sweep of each segment's language
  column — a pass over the corpus's rows, not over its segments, and one no `IN`
  narrows. What the segment-level design saves is the string work: one
  comparison per segment where a per-row reader hashes and compares once per
  row. A segment whose column does not resolve to a single value declines the
  narrowing rather than guessing, and the query falls back to the complete scan.

  The ceiling is stated in rows, not in seconds: the A/B bench harness does not
  run in the environment this was built in, and no bench class covers this
  shape. The tier it names is the one `WHERE has_doc = 'false'` already uses on
  the same corpus, so the shape is not new — only the number of rows it is
  pointed at.

  The value is answered, not stored: a row that answers `has_todo = 'false'`
  still carries no `has_todo` field in the result it returns.
