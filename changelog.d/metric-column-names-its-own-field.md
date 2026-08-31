- The CSV metric column now carries the value of the field its header names,
  for the four fields written onto a row only when they hold — `has_todo`,
  `has_escape`, `has_shadow`, `is_recursive`. A row answering one of them by
  its declared default stores nothing, and the column read that absence as "no
  value" and fell through to `usages`, so
  `FIND symbols WHERE has_todo = 'false'` rendered a usage count under a
  `has_todo` header — `0` and `3` for two rows whose answer was `false` both
  times. The rows and the `total` were right throughout, and JSON was correct;
  only the label was wrong, in the format that is the default and the one most
  answers are read in.

  A row the declaration does not speak for — a `cmake` function, which no
  comment-scanning enricher examines — renders an empty value rather than
  borrowing the number that follows it. Stamping the default on every row
  would have contradicted the population the query itself selects, which is
  what makes the field's counts add up to the corpus.
