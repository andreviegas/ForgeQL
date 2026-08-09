- Sessions reading the same commit now share one copy of the committed index.
  Opening a session decoded that commit's overlay and one reader per indexed
  file, and every session on the commit repeated the whole of that work.
  Measured on a large repository with three sessions open on one commit in a
  single process, the second and later sessions each cost 617 MB of private
  heap at peak; they now cost 2-3 MB. A few sessions warming together has been
  enough to exhaust the memory on the machine serving them.

  The overlay and its readers cannot change once opened, and every file behind
  them is addressed by its content, so they are now opened once per commit and
  handed to each session, behind a per-commit gate so that sessions starting
  together decode once between them rather than once each. The background
  warmer's "is this commit already built?" is answered from that same state,
  where it previously decoded an index only to discard it.

  A session's own uncommitted edits are untouched by this: they were never part
  of the shared state, and still take precedence over it at query time.

  Opening those sessions is also faster, for the same reason: three sessions on
  one commit previously performed three decodes and now perform one, and the
  measured time to open them fell to roughly a third.

  The saving is per *additional* session on a commit, so a benchmark that opens
  a single session cannot see this change at all. Nothing is held back for a
  session that has not arrived either: an entry lives exactly as long as some
  session is holding it.
