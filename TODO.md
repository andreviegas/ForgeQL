# ForgeQL — TODO

## Session architecture

- [x] Wire `SessionCoords` into `exec_source.rs`: replace inline alias≠branch guard,
  `budget_branch` derivation, hardcoded `"anonymous"`, worktree dir construction, and git branch
  format; changed silent cross-source alias collision to a hard error; replaced all 5 ad-hoc
  `data_dir.join("worktrees")` sites with `SessionCoords::worktrees_root()`
- [ ] Add `resolve_commit()` helper in `worktree.rs` (try `find_branch` first, fall back to
  `revparse_single`) to support short-SHA `USE` references
- [ ] Change session map key from bare alias to `"{user}:{alias}"`; add `user_id: &str`
  to `execute()`; fix `require_session`
- [ ] Thread real user identity from MCP connection into `execute()`;
  add `user_id: Option<String>` to `RunFqlParams`
- [x] Replace `prune_orphaned_worktrees` + `try_auto_reconnect` with a single
  `restore_sessions_from_disk()` called once at MCP startup; extend `.forgeql-session`
  sentinel with `source`/`branch`/`alias`/`user` so warm sessions are restored without
  git traversal; fixed the always-false live-session guard bug in the process

## Session lifecycle

- [ ] `DROP SESSION 'alias'` command — explicit teardown, removes worktree, clears budget
- [ ] `SHOW SESSIONS` command — list active sessions (alias, source, branch, user, last-active)
- [ ] `USE REFRESHED` syntax — fetch from remote before opening the worktree
- [ ] Session TTL dirty flag in `.forgeql-session` sentinel: `dirty=true` prevents GC eviction
  until the session is explicitly dropped

## Query language

- [ ] Full-text index — surface identifiers inside **comments and string literals**
  (invisible to the AST index) and in non-code files (`.cmake`, `.md`, etc.);
  merge results into `FIND usages`; add a `FIND text` grep-style command

## Miscellaneous

- [ ] Per-user engine isolation when lock contention becomes measurable (each user gets their
  own `ForgeQLEngine` instance; `Arc<TokioMutex<Engine>>` becomes a pool)
- [ ] `ALL_CAPS` naming consistency audit for enumerators and constants
