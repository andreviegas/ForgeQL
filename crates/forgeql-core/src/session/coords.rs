//! Session identity — the four dimensions that uniquely identify a `ForgeQL` session.
//!
//! [`SessionCoords`] is the **single source of truth** for deriving:
//!
//! | Derived value      | Format                                                         |
//! |--------------------|----------------------------------------------------------------|
//! | Session map key    | `{user}:{alias}`                                               |
//! | Git session branch | `fql/{user}/{source}/{branch}/{alias}`                         |
//! | Worktree dir name  | `{source}.{safe_branch}.{alias}`                               |
//! | Worktree path      | `{data_dir}/worktrees/{user}/{source}.{safe_branch}.{alias}`   |
//!
//! `safe_branch` replaces `/` with `-` so the worktree directory stays flat.
//!
//! All path and branch derivations must go through this struct. To change the
//! user format (e.g. from the string `"anonymous"` to `"user:1234567"`), update
//! only the construction sites — every downstream path, branch name, and map key
//! follows automatically.

use std::path::{Path, PathBuf};

// -----------------------------------------------------------------------
// SessionCoords
// -----------------------------------------------------------------------

/// The four dimensions that uniquely identify any `ForgeQL` session.
///
/// # User isolation
///
/// `user` is currently always `"anonymous"`.  The field already exists so
/// that wiring up real user identity requires changing only the construction
/// call-sites — the derived strings propagate the change everywhere.
///
/// # SHA-prefix branch references
///
/// `branch` may be a regular branch name (e.g. `"main"`, `"fix/null-check"`)
/// or a short git SHA prefix (e.g. `"a3f9b2c"`).  Use [`is_sha_ref`] to
/// distinguish; `worktree::create` must use `revparse_single` instead of
/// `find_branch` when it returns `true`.
///
/// [`is_sha_ref`]: SessionCoords::is_sha_ref
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionCoords {
    /// Identity of the requesting user.
    ///
    /// Always `"anonymous"` until real authentication is wired in.
    /// Change this field's value at construction time — all derived paths
    /// and branch names will update automatically.
    pub user: String,

    /// Registered source name (bare repo name), e.g. `"pisco-firmware"`.
    pub source: String,

    /// Branch, feature branch, or SHA prefix to check out.
    ///
    /// Examples: `"main"`, `"fix/null-check"`, `"a3f9b2c"` (short SHA).
    pub branch: String,

    /// User-chosen session alias from `USE … AS 'alias'`.
    pub alias: String,
}

impl SessionCoords {
    /// Construct a `SessionCoords` with explicit values for all four fields.
    #[must_use]
    pub fn new(
        user: impl Into<String>,
        source: impl Into<String>,
        branch: impl Into<String>,
        alias: impl Into<String>,
    ) -> Self {
        Self {
            user: user.into(),
            source: source.into(),
            branch: branch.into(),
            alias: alias.into(),
        }
    }

    /// Shorthand for single-user / anonymous deployments.
    ///
    /// Equivalent to `SessionCoords::new("anonymous", source, branch, alias)`.
    /// All current call-sites use this constructor; when real auth is wired,
    /// they migrate to [`new`] and pass the authenticated user identity.
    ///
    /// [`new`]: SessionCoords::new
    #[must_use]
    pub fn anonymous(
        source: impl Into<String>,
        branch: impl Into<String>,
        alias: impl Into<String>,
    ) -> Self {
        Self::new("anonymous", source, branch, alias)
    }

    // -----------------------------------------------------------------------
    // Derived values — paths and branch names
    // -----------------------------------------------------------------------

    /// Key used in the engine's in-memory session map.
    ///
    /// Format: `"{user}:{alias}"`.
    ///
    /// Keying by `(user, alias)` means different users may each maintain their
    /// own session under the same alias without collision.
    #[must_use]
    pub fn map_key(&self) -> String {
        format!("{}:{}", self.user, self.alias)
    }

    /// Git branch created in the bare repo for this session.
    ///
    /// Format: `"fql/{user}/{source}/{branch}/{alias}"`.
    ///
    /// The `fql/` namespace prefix sidesteps the loose-ref collision where a
    /// file at `refs/heads/main` already exists and git cannot create
    /// `refs/heads/main/alias` (a directory where a file already is).
    ///
    /// Including `source` makes the branch name globally unique across sources
    /// when the `ForgeQL` server acts as a shared git remote.
    #[must_use]
    pub fn git_branch(&self) -> String {
        format!(
            "fql/{}/{}/{}/{}",
            self.user, self.source, self.branch, self.alias
        )
    }

    /// Filesystem-safe directory name for this session's worktree.
    ///
    /// Format: `"{source}.{safe_branch}.{alias}"`.
    ///
    /// All `/` characters in every component are replaced with `-` so the
    /// resulting name is a single flat directory entry with no nesting.
    #[must_use]
    pub fn worktree_dir(&self) -> String {
        let safe_source = self.source.replace('/', "-");
        let safe_branch = self.branch.replace('/', "-");
        let safe_alias = self.alias.replace('/', "-");
        format!("{safe_source}.{safe_branch}.{safe_alias}")
    }

    /// Absolute path to this session's worktree directory.
    ///
    /// Format: `{data_dir}/worktrees/{user}/{worktree_dir}`.
    #[must_use]
    pub fn worktree_path(&self, data_dir: &Path) -> PathBuf {
        Self::user_worktrees_root(data_dir, &self.user).join(self.worktree_dir())
    }

    /// Root directory that holds **all** users' worktrees.
    ///
    /// Format: `{data_dir}/worktrees`.
    ///
    /// Prefer this over ad-hoc `data_dir.join("worktrees")` so that the
    /// layout can be changed in a single place.
    #[must_use]
    pub fn worktrees_root(data_dir: &Path) -> PathBuf {
        data_dir.join("worktrees")
    }

    /// Per-user worktree root for a given user identity.
    ///
    /// Format: `{data_dir}/worktrees/{user}`.
    #[must_use]
    pub fn user_worktrees_root(data_dir: &Path, user: &str) -> PathBuf {
        Self::worktrees_root(data_dir).join(user)
    }

    // -----------------------------------------------------------------------
    // Predicates
    // -----------------------------------------------------------------------

    /// Returns `true` when `branch` looks like a git SHA prefix rather than a
    /// named branch.
    ///
    /// Heuristic: every character is an ASCII hex digit, the string is at least
    /// 7 characters long (the conventional short-SHA length that git uses in
    /// `git log --oneline`), and there is no `/` (branch names usually contain
    /// letters and slashes).  False positives in practice are negligible.
    ///
    /// When this returns `true`, `worktree::create` must resolve the commit
    /// via `repo.revparse_single(&branch)` rather than `find_branch`.
    /// `git2::revparse_single` resolves short prefixes the same way
    /// `git rev-parse` does, and will work unchanged when git moves to SHA-256.
    #[must_use]
    pub fn is_sha_ref(&self) -> bool {
        self.branch.len() >= 7
            && !self.branch.contains('/')
            && self.branch.chars().all(|c| c.is_ascii_hexdigit())
    }

    // -----------------------------------------------------------------------
    // Budget helper
    // -----------------------------------------------------------------------

    /// The budget-file key for this session.
    ///
    /// For trunk branches (`main` / `master`) the budget is keyed by the alias
    /// so that each feature off trunk gets its own budget envelope.
    /// For all other branches the branch name itself is the key.
    ///
    /// This centralises the logic that was previously inlined in `exec_source.rs`.
    #[must_use]
    pub fn budget_branch(&self) -> &str {
        if matches!(self.branch.as_str(), "main" | "master") {
            &self.alias
        } else {
            &self.branch
        }
    }

    // -----------------------------------------------------------------------
    // Validation
    // -----------------------------------------------------------------------

    /// Validate that the alias is usable as a session identifier.
    ///
    /// Returns `Err` when `alias == branch` — this is meaningless (the session
    /// branch would be `fql/user/source/main/main`) and creates ambiguity in
    /// the budget key.
    ///
    /// # Errors
    /// Returns `Err(String)` with a human-readable explanation when the alias
    /// equals the source branch name.
    pub fn validate(&self) -> Result<(), String> {
        if self.alias == self.branch {
            return Err(format!(
                "alias '{}' must differ from the source branch '{}'",
                self.alias, self.branch
            ));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Parsing — inverse of worktree_dir (used in auto-reconnect)
    // -----------------------------------------------------------------------

    /// Reconstruct `SessionCoords` from a worktree directory name.
    ///
    /// The directory name format is `{source}.{safe_branch}.{alias}`.
    /// `user`, `source`, and `alias` must be known upfront to strip the
    /// prefix and suffix and isolate the branch component.
    ///
    /// **Lossiness:** the recovered `branch` has `/` replaced with `-`
    /// (because [`worktree_dir`] flattens slashes). Callers should verify the
    /// branch against the bare repo's actual branches when lossless recovery
    /// is needed.
    ///
    /// Returns `None` if `dir_name` does not match the expected pattern.
    ///
    /// [`worktree_dir`]: SessionCoords::worktree_dir
    #[must_use]
    pub fn from_dir_name(user: &str, source: &str, alias: &str, dir_name: &str) -> Option<Self> {
        let prefix = format!("{source}.");
        let suffix = format!(".{alias}");
        let safe_branch = dir_name.strip_prefix(&prefix)?.strip_suffix(&suffix)?;
        Some(Self {
            user: user.to_string(),
            source: source.to_string(),
            branch: safe_branch.to_string(),
            alias: alias.to_string(),
        })
    }
}
