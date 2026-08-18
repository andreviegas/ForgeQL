- A query whose page held a name longer than 120 bytes ending in a multi-byte
  character crashed the process. The display truncation cut the name at byte
  120 while counting its length in bytes, and a byte index that lands inside a
  character is not a legal place to split a Rust string. Indexing ForgeQL's own
  repository was enough to hit it: `FIND symbols ORDER BY line DESC` and
  `FIND symbols ORDER BY name OFFSET 40` both paged in a documentation line
  ending in an em dash. The cut is now made in characters, as the documented
  behaviour always said. Reachable from the server as well as the CLI, though
  it cost one request rather than the daemon.
