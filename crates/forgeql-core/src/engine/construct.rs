//! Bringing an engine up, and finding what a previous run left behind.
//!
//! Construction is where a data directory becomes a working engine: the
//! worktree root is created if absent, and `data_dir` is scanned so that
//! sources registered by an earlier process are available again without being
//! re-declared. Everything here runs once, before any command does.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tracing::{info, warn};

use crate::{
    ast::lang::LanguageRegistry, engine::ForgeQLEngine, git::source::SourceRegistry,
    session::SessionCoords,
};

impl ForgeQLEngine {
    /// Create a new engine rooted at `data_dir`.
    ///
    /// Creates the `<data_dir>/worktrees/` directory if it does not exist.
    /// Call [`restore_sessions_from_disk()`](Self::restore_sessions_from_disk)
    /// once at MCP server startup to prune expired worktrees and restore live
    /// sessions into memory.  In CLI modes (REPL, pipe, one-shot) do not call
    /// it — worktrees persist across invocations and sessions should not be
    /// re-indexed on every invocation.
    ///
    /// # Errors
    /// Returns `Err` if the worktree directory cannot be created.
    pub fn new(data_dir: PathBuf, lang_registry: Arc<LanguageRegistry>) -> Result<Self> {
        std::fs::create_dir_all(SessionCoords::worktrees_root(&data_dir))?;
        info!(dir = %data_dir.display(), "engine: data directory ready");

        let mut registry = SourceRegistry::new(data_dir.clone());
        Self::discover_existing_sources(&data_dir, &mut registry);

        let engine = Self {
            registry,
            sessions: HashMap::new(),
            pending_sessions: HashMap::new(),
            data_dir,
            commands_served: 0,
            lang_registry,
            jobs: Arc::new(crate::jobs::JobRegistry::from_env()),
            pending_gate_jobs: Vec::new(),
            coach: None,
        };
        Ok(engine)
    }

    /// Scan `data_dir` for existing `*.git` bare repositories and register them.
    ///
    /// This makes sources survive process restarts without requiring
    /// `CREATE SOURCE` again — the bare repo on disk is the source of truth.
    fn discover_existing_sources(data_dir: &Path, registry: &mut SourceRegistry) {
        let entries = match std::fs::read_dir(data_dir) {
            Ok(entries) => entries,
            Err(err) => {
                warn!(%err, "cannot scan data_dir for existing sources");
                return;
            }
        };

        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(source_name) = name.strip_suffix(".git") else {
                continue;
            };
            if source_name.is_empty() {
                continue;
            }

            match registry.register(source_name, path.clone()) {
                Ok(source) => {
                    info!(
                        name = source.name(),
                        path = %source.path().display(),
                        "discovered existing source",
                    );
                }
                Err(err) => {
                    warn!(
                        name = source_name,
                        %err,
                        "failed to register discovered source",
                    );
                }
            }
        }
    }
}
