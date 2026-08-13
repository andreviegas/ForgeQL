- Attaching to a commit whose merged index was never built no longer pays a
  full corpus merge. A commit now writes a small manifest naming the merged
  index it grew from and the files it changed; the next attach opens that
  index and serves the changes the same way uncommitted edits are served.
  Rows and totals are identical to a full rebuild. Queries that ride the
  name-stream and count fast paths take the slower complete path on such
  commits, exactly as they do in a session with uncommitted edits, and a
  missing or unreadable manifest falls back to the full build — a bad
  manifest can cost time, never rows.
