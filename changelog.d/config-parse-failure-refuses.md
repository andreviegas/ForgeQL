- A `.forgeql.yaml` that exists and does not parse now refuses the session,
  naming the file and the parse error, instead of being read as no
  configuration at all. Absence is unchanged and is still the designed
  fallback: a source with no such file is answered by the in-memory backend,
  with no columnar index and no verify or run steps.

  The two used to be the same answer. The loader mapped both a missing file and
  an unreadable one to "nothing here", and every caller decides real capability
  from that result — whether to configure the columnar build, which verify and
  run steps exist, what the line budget is. So one mistyped value did not
  produce an error anywhere. It produced a session that reported success:
  `USE` answered with `symbols_indexed 0`, rows came back carrying no `node_id`
  and no `rev`, and every `VERIFY` and `RUN` step failed with "add it under
  `run_steps:`" for a step the file plainly declared. Nothing in that picture
  points at the config, and a session could run a long way before anyone
  suspected it.

  This applies to both files the loader reads — the sidecar
  (`<repo-dir>/<source>.forgeql.yaml`) and the in-repo `.forgeql.yaml` — and to
  every verb that loads one: `USE`, `CREATE SOURCE` and `REFRESH SOURCE`.
