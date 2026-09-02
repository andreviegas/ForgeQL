- `FIND usages OF` now says when it skipped a file for its bytes. The scan
  passes over anything that is not text it decodes — binary, UTF-32 even when
  its mark declares it, and UTF-16 written without a mark, which is
  indistinguishable from binary. That boundary is deliberate and documented,
  but the response's `hint` counted only files that failed to OPEN, so a name
  living only in a skipped file came back as a confident zero with nothing to
  say a candidate had never been read. The two causes are now reported
  together in that one hint, each with its own count and named reason.

  The count covers every source the scan draws candidates from — indexed
  segments, file-only entries, a session's dirty segments, and the
  non-indexed files a session created — not by four separate additions but
  because those sources are collected into one path list and read by one
  loop, so a source cannot be added later and quietly go uncounted. The
  in-memory backend reads no files at all, answering from its index alone, so
  it has nothing to count and makes no such claim.

  The count is about what the scan did, so it includes any binary file the
  workspace holds — an object file, an image, and ForgeQL's own index blobs
  where a data directory sits inside the worktree root. That is honest rather
  than noisy: such a file embeds symbol names, which is exactly why its sites
  must not be swept, and the whole point of the count is to say that bytes
  holding the name may have gone unread.
