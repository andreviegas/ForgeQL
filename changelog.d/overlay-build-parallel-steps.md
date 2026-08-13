- The overlay build runs its enrichment-bitmap step, its name-index merge and
  its trigram pass in parallel instead of back to back. The three read the
  same immutable inputs and write different outputs, so the result is
  unchanged; the wall-clock cost of the group drops toward the cost of the
  slowest member. Peak build memory does not drop and can rise: the steps
  now hold their working memory at the same time instead of one after
  another.
