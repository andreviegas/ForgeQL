- The ascending name-stream shapes — a bare `LIMIT` and `ORDER BY name` — no
  longer fall back to the complete scan in a session with uncommitted edits,
  or on a commit attached through its chain manifest. Every edited file's
  index carries its own sorted name list, so the stream merges those with the
  shared index name by name, skips the shared rows the edits replaced, and
  reports a total that counts the merged answer. The descending and
  kind-filtered stream shapes still take the complete scan in such sessions.
