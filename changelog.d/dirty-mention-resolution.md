- Symbol resolution in a session with uncommitted edits no longer lets an
  edited file that merely mentions a name outrank the file that declares it.
  Previously, once any edited file carried the name as a bare reference,
  `SHOW members OF 'Type'` — and `SHOW body` / `SHOW context` / `SHOW
  callees`, which share the resolver — answered from that file and failed
  with "AST node not found" for a symbol that plainly exists elsewhere.
  A declaration edited in-session still wins exactly as before; only bare
  reference rows now rank behind the persistent index instead of ahead of it.
