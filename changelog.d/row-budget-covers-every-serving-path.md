- The 2 GiB row budget now covers every path that serves result rows except
  `FIND files`, not only the `FIND symbols` scan over the on-disk index.
  Newly bounded, each with the same refusal and the same
  `FORGEQL_FIND_MAX_ROWS` override: the union of a session's uncommitted rows
  into that scan's answer (previously appended after the check), the
  `FIND usages` site list on both backends (on the on-disk one the check runs
  between matching tiers, so the peak can overshoot the bound by one tier's
  finds), and the in-memory backend's scan, trimmed or not — its running trim
  holds the retained window to a few multiples of the `LIMIT`, which keeps
  any small page clear of the budget, and a `LIMIT` so large that even its
  trimmed window outgrows the budget now refuses exactly as the on-disk
  backend refuses it. `FIND files` deliberately carries no budget: its answer
  is one small row per workspace file, so its size is the workspace's file
  count, never anything a query matches.
