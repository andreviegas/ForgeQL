- The golden suites now refuse to run against a corpus that has no on-disk
  index for the current index generation, instead of judging the answers a
  fallback produced. A session whose index will not open does not fail: it
  answers from the complete in-memory index, and the two do not agree on the
  counted `GROUP BY` routes, on node handles, or on several refusal texts — so
  the cases failed on row values with nothing in any diff to say which had
  written them. One probe per corpus at startup now stops the run with a
  message naming the corpora to re-index. It costs no extra session: the probe
  rides the one the first cases would have opened anyway.
