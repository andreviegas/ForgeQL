- A `USE` whose columnar index refuses to open now says so in its own response
  message, instead of only in the server log. The message carries the
  refusal's repair ("restore that file, or index this source from scratch"),
  which previously never reached the agent that had to act on it; a `USE`
  that resumes such a session repeats the note for as long as the session
  serves from the fallback, and a later attach that opens the columnar index
  cleanly drops it.
- The in-memory fallback behind that message now actually holds the index.
  With columnar configured, the index build emits segments inline and
  deliberately returns an empty in-memory table — correct while the columnar
  open succeeds, and exactly wrong on the one path that falls back to it: a
  session whose columnar open failed answered every query with zero rows
  while `USE` reported success. The fallback path now rebuilds the in-memory
  index for real before serving.
