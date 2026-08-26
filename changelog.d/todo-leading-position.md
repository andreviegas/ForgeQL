- **A TODO on the first line of a function body is now found.** The marker scan
  walked the function's body subtree only. A grammar that delimits blocks by
  indentation does not open the block until the first *statement*, so a comment
  written as the first line of the body is parsed as a sibling of the block
  rather than a child of it — and a body-only walk never reached it. Every
  function whose body opened with a marker answered no `has_todo`, no
  `todo_count` and no `todo_tags`; because those fields are written only when a
  marker is found, such a function was missing from `has_todo = 'true'` and from
  `has_todo = 'false'` alike. A marker anywhere after the first statement was
  found normally, which is why the gap went unnoticed.

  The scan now covers the body **plus any comment attached directly to the
  function node**, which is where the leading comment lands. That admits a
  second position with it: a comment written between the signature and the
  body, which sits in the same place in a brace-delimited grammar and was
  unseen for the same reason. A marker on the LAST line of a body needed
  nothing — the block has opened by then, so it was always inside.

  Three comments stay outside, and the field tables state all three in the same
  cell as the claim: one *preceding* the function, which is its doc comment; one
  between a *decorator* and the definition it decorates, which belongs to the
  wrapper even though the row's span folds back over it; and any style the
  language does not declare as its comment kind — in Rust that means a
  `/* TODO */` is still missed, in every position.

  What moved, on the corpora the tests are grounded on:

  | corpus | `has_todo = 'true'` before | after |
  |---|---|---|
  | PyTorch | 3,398 | 3,763 |
  | Zephyr | 951 | 958 |
  | this repository, at the snapshot the tests read | 9 | 9 |

  One PyTorch file went from 8 functions to 12. The snapshot of this repository
  that the tests read does not move at all, and that is the shape of the defect
  rather than a gap in the check: the snapshot holds no Python, so the leading
  position cannot arise in it, and its C, C++ and Rust write no marker between
  a signature and a body either. The working tree does move, because this change
  adds fixtures that put a marker in both of the newly reachable positions on
  purpose. Zephyr moves by seven, and a partitioned count says where: its Python
  goes 37 → 44, which is all of them.

  This changes stored index output, so the enrichment generation is bumped and
  every corpus re-indexes once on first use.
