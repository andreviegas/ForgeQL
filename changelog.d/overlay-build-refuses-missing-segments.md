- Building an overlay now refuses when a segment it names is missing or
  unreadable, instead of skipping it. The skip produced an overlay that was
  self-consistent and silently smaller — every later session on that commit
  dropped the file's rows from every answer, with nothing anywhere saying so.
  The routes that reach this build hold no other copy of the rows (a COMMIT
  merges the base overlay with the session's staged segments; a rebuild from
  an inline segment map assembles what is already on disk), so the refusal
  names each affected file and where its segment was expected. A COMMIT
  refused this way is not sticky: restore the segment files, or re-index the
  source, and the same COMMIT succeeds.
