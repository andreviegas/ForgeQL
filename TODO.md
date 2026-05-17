# ForgeQL — TODO

## Session architecture

- [x] Wire `SessionCoords` into `exec_source.rs`: replace inline alias≠branch guard,
  `budget_branch` derivation, hardcoded `"anonymous"`, worktree dir construction, and git branch
  format; changed silent cross-source alias collision to a hard error; replaced all 5 ad-hoc
  `data_dir.join("worktrees")` sites with `SessionCoords::worktrees_root()`
- [ ] Add `resolve_commit()` helper in `worktree.rs` (try `find_branch` first, fall back to
  `revparse_single`) to support short-SHA `USE` references
- [x] Change session map key from bare alias to `"{user}:{source}:{branch}:{alias}"`; add
  `user_id: &str` to `execute()`; fix `require_session` — map key is now the full
  four-field `SessionCoords::map_key()` token; `execute()` accepts `Option<&SessionCoords>`;
  MCP/CLI callers decode via `SessionCoords::from_session_id()` before entering the engine
- [ ] Thread real user identity from MCP connection into `execute()`;
  add `user_id: Option<String>` to `RunFqlParams` once JWT/API-key auth is wired in
- [ ] **Opaque wire session token** — return a UUID or HMAC instead of the raw
  `user:source:branch:alias` map key to clients; do this together with JWT auth so the
  two land in the same PR. Preferred approach: HMAC(server-secret, map\_key) — no
  side-map, survives restarts, zero persistence changes to `.forgeql-session`; downside
  is a required server secret (env var). UUID variant needs the token stored in the
  sentinel for warm reconnect and a `token_map: HashMap<String,String>` kept in sync
  with session lifecycle (create / drop / eviction). Defer until auth is implemented.
- [x] Replace `prune_orphaned_worktrees` + `try_auto_reconnect` with a single
  `restore_sessions_from_disk()` called once at MCP startup; extend `.forgeql-session`
  sentinel with `source`/`branch`/`alias`/`user` so warm sessions are restored without
  git traversal; fixed the always-false live-session guard bug in the process

## Session lifecycle

- [ ] `SHOW SESSIONS` command — list active sessions (alias, source, branch, user, last-active)
- [ ] `USE REFRESHED` syntax — fetch from remote before opening the worktree
- [ ] Session TTL dirty flag in `.forgeql-session` sentinel: `dirty=true` prevents GC eviction
  until the session is explicitly dropped

## Query language

- [ ] Full-text index — surface identifiers inside **comments and string literals**
  (invisible to the AST index) and in non-code files (`.cmake`, `.md`, etc.);
  merge results into `FIND usages`; add a `FIND text` grep-style command

## Session startup

- [x] Defer index loading at startup — restore only session metadata from sentinel files; load the columnar index on the first `USE` command for that session
- [x] Fix checkpoint empty-stack write — remove the checkpoint file after a full ROLLBACK instead of writing an unvalidatable empty record, eliminating spurious startup warnings

## Overlay — mmap zero-copy (RAM sharing)

See `data/future-plans/mmap-zero-copy-overlay-plan.md` for the full plan.

- [x] Replace the overlay parity unit test with a golden-value integration test against the frozen `zephyr-andre.zephyr-main` branch
- [ ] Switch overlay open from full-file heap read to mmap — eliminate the per-session heap copy of the entire overlay file
- [ ] Make the FST and name-postings zero-copy slice views into the mmap — no heap duplication when multiple sessions share the same commit
- [ ] New FQOV v3 binary format — replace the bincode payload with a TOC-based layout so all structures (row table, bitmaps, FST) are zero-copy; delegates all RAM management to the OS page cache

## Miscellaneous

- [ ] Per-user engine isolation when lock contention becomes measurable (each user gets their
  own `ForgeQLEngine` instance; `Arc<TokioMutex<Engine>>` becomes a pool)
- [ ] `ALL_CAPS` naming consistency audit for enumerators and constants
