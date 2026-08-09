- `RUN`, `VERIFY build` and `JOB START` accept a heredoc for their
  arguments, not only a quoted literal. A quoted argument cannot carry the
  quote that delimits it, and this DSL escapes neither by doubling nor by
  backslash — `''` split the
  argument in two and `\'` failed to parse. Any prose holding both an
  apostrophe and a double quote was therefore unwritable as a step argument
  at any length, which ruled out passing a message, a report, or an issue
  body to a step. The argument was already bound to the step's stdin rather
  than to its command line, so the limit was never size; it was quoting.

      RUN 'file_issue' <<BODY
      `WHERE name = 'x'` answers with a row that carries no handle,
      and the message reads "not found". It doesn't refuse.
      BODY

  All three take arguments through the same binding — `JOB START` submits a
  verify step, so the two spellings of one step now accept the same
  arguments. Quoted arguments are unchanged.

  One constraint worth knowing when the body is prose: a line that is itself
  all-uppercase reads as a closing tag, so a bare `TODO` or `NOTE` line ends
  the body early and the statement is refused for a tag mismatch. Choose a
  tag, or indent the line.
