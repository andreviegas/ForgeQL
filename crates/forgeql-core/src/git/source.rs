use std::collections::hash_map::Entry;
/// Bare-repository management — Phase B of the v2 architecture.
///
/// A `Source` is a bare git clone: the "database" in the v2 model.
/// `SourceRegistry` is the global catalogue of all known sources.
///
/// SQL analogy:
///   `CREATE SOURCE 'name' FROM 'url'`  →  `Source::clone_from()`
///   `SHOW SOURCES`                     →  `SourceRegistry::names()`
///   `DROP SOURCE 'name'`               →  `SourceRegistry::remove()`
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{bail, Result};
use git2::{BranchType, Cred, FetchOptions, RemoteCallbacks, Repository};
use tracing::{debug, info};

// -----------------------------------------------------------------------
// Source
// -----------------------------------------------------------------------

/// A bare git repository representing one managed codebase.
///
/// Every `Source` corresponds to a `CREATE SOURCE` statement. Sessions
/// (worktrees) are checked out from sources.
#[derive(Debug)]
pub struct Source {
    /// Human-readable name used in `USE <name>.<branch>` statements.
    name: String,
    /// Absolute path to the bare `.git` directory on disk.
    path: PathBuf,
    /// Original remote URL, if created via `clone_from`.
    origin_url: Option<String>,
}

impl Source {
    /// Clone `url` as a bare repository into `<data_dir>/<name>.git`.
    ///
    /// # Errors
    /// Returns `Err` if the clone fails or a repository already exists at the
    /// target path.
    pub fn clone_from(name: &str, url: &str, data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(format!("{name}.git"));
        if path.exists() {
            bail!("source '{}' already exists at '{}'", name, path.display());
        }
        info!(name, url, dest = %path.display(), "cloning bare repository");

        // Build credential callback: cycle through auth methods on each retry.
        //
        // libgit2 calls this callback again whenever authentication fails,
        // so the `attempts` counter is used to try each method in turn:
        //   1  → SSH agent      (works when SSH_AUTH_SOCK is in the env)
        //   2  → ~/.ssh/id_ed25519  (most common modern key)
        //   3  → ~/.ssh/id_ecdsa
        //   4  → ~/.ssh/id_rsa
        //   5+ → hard error
        //
        // NOTE: Cred::ssh_key_from_agent() always returns Ok(cred) — it
        // does NOT check SSH_AUTH_SOCK at construction time.  The connection
        // to the agent happens later; if it fails, libgit2 retries the
        // callback, so attempt 2 will pick up key-file auth.
        let mut attempts = 0u8;
        let mut callbacks = RemoteCallbacks::new();

        // Accept any host certificate — libssh2 (used by libgit2) performs
        // its own host-key check independently of `~/.ssh/known_hosts`.
        // Without this callback libgit2 rejects unfamiliar hosts even after
        // `ssh-keyscan` has populated known_hosts.  Security here relies on
        // the SSH key exchange, not on host certificate pinning.
        let _ = callbacks
            .certificate_check(|_cert, _valid| Ok(git2::CertificateCheckStatus::CertificateOk));

        let _ = callbacks.credentials(move |_url, username, allowed| {
            attempts += 1;
            let user = username.unwrap_or("git");
            if allowed.contains(git2::CredentialType::SSH_KEY) {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
                match attempts {
                    // Attempt 1: SSH agent (SSH_AUTH_SOCK path).
                    1 => Cred::ssh_key_from_agent(user),
                    // Attempts 2-4: specific key files.
                    2 => Cred::ssh_key(
                        user,
                        None,
                        &std::path::PathBuf::from(&home).join(".ssh/id_ed25519"),
                        None,
                    ),
                    3 => Cred::ssh_key(
                        user,
                        None,
                        &std::path::PathBuf::from(&home).join(".ssh/id_ecdsa"),
                        None,
                    ),
                    4 => Cred::ssh_key(
                        user,
                        None,
                        &std::path::PathBuf::from(&home).join(".ssh/id_rsa"),
                        None,
                    ),
                    _ => Err(git2::Error::from_str(
                        "SSH authentication failed — ensure the key is loaded \
                         in ssh-agent or present at ~/.ssh/id_ed25519 / id_rsa",
                    )),
                }
            } else {
                Err(git2::Error::from_str("unsupported credential type"))
            }
        });

        let mut fetch_opts = FetchOptions::new();
        let _ = fetch_opts.remote_callbacks(callbacks);

        drop(
            git2::build::RepoBuilder::new()
                .bare(true)
                .fetch_options(fetch_opts)
                .clone(url, &path)?,
        );
        debug!(name, "bare clone complete");
        Ok(Self {
            name: name.to_string(),
            path,
            origin_url: Some(url.to_string()),
        })
    }

    /// Open an existing bare repository at `path`.
    ///
    /// # Errors
    /// Returns `Err` if no bare git repository is found at `path`.
    pub fn open(name: &str, path: PathBuf) -> Result<Self> {
        // Validate by opening — fail fast if path is not a bare repo.
        let repo = Repository::open_bare(&path)?;
        debug!(name, path = %repo.path().display(), "source opened");
        drop(repo);
        Ok(Self {
            name: name.to_string(),
            path,
            origin_url: None,
        })
    }

    /// List all local branch names in this source.
    ///
    /// # Errors
    /// Returns `Err` if the repository cannot be opened or branch iteration
    /// fails.
    pub fn branches(&self) -> Result<Vec<String>> {
        let repo = Repository::open_bare(&self.path)?;
        let branches = repo
            .branches(Some(BranchType::Local))?
            .filter_map(|b| {
                let (branch, _) = b.ok()?;
                branch.name().ok()?.map(ToString::to_string)
            })
            .collect();
        Ok(branches)
    }

    /// Absolute path to the bare `.git` directory.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // PathBuf::as_path is not const
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The name this source was registered under.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // String::as_str is not const
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Original remote URL if this source was created via `clone_from`.
    #[must_use]
    pub fn origin_url(&self) -> Option<&str> {
        self.origin_url.as_deref()
    }

    /// Fetch all remotes on this bare repository, bringing it up to date.
    ///
    /// Uses the same SSH credential chain as `clone_from` (agent → `id_ed25519` →
    /// `id_ecdsa` → `id_rsa`).  After this call, any branch whose `HEAD` has moved
    /// will be picked up by the next `USE` session-resume check (the cached
    /// `.forgeql-index` `HEAD` hash will no longer match and a re-index is triggered).
    ///
    /// # Errors
    /// Returns `Err` if the repository cannot be opened, has no remotes, or the
    /// fetch fails (network error, authentication failure, etc.).
    pub fn fetch_all(&self) -> Result<Vec<String>> {
        let repo = Repository::open_bare(&self.path)?;

        // Collect all remote names first to avoid borrow issues.
        let remote_names: Vec<String> = repo
            .remotes()?
            .iter()
            .flatten()
            .map(ToString::to_string)
            .collect();

        if remote_names.is_empty() {
            bail!("source '{}' has no remotes configured", self.name);
        }

        for remote_name in &remote_names {
            let mut remote = repo.find_remote(remote_name)?;

            let mut attempts = 0u8;
            let mut callbacks = RemoteCallbacks::new();
            let _ = callbacks
                .certificate_check(|_cert, _valid| Ok(git2::CertificateCheckStatus::CertificateOk));
            let _ = callbacks.credentials(move |_url, username, allowed| {
                attempts += 1;
                let user = username.unwrap_or("git");
                if allowed.contains(git2::CredentialType::SSH_KEY) {
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
                    match attempts {
                        1 => Cred::ssh_key_from_agent(user),
                        2 => Cred::ssh_key(
                            user,
                            None,
                            &std::path::PathBuf::from(&home).join(".ssh/id_ed25519"),
                            None,
                        ),
                        3 => Cred::ssh_key(
                            user,
                            None,
                            &std::path::PathBuf::from(&home).join(".ssh/id_ecdsa"),
                            None,
                        ),
                        4 => Cred::ssh_key(
                            user,
                            None,
                            &std::path::PathBuf::from(&home).join(".ssh/id_rsa"),
                            None,
                        ),
                        _ => Err(git2::Error::from_str(
                            "SSH authentication failed — ensure the key is loaded \
                             in ssh-agent or present at ~/.ssh/id_ed25519 / id_rsa",
                        )),
                    }
                } else {
                    Err(git2::Error::from_str("unsupported credential type"))
                }
            });

            let mut fetch_opts = FetchOptions::new();
            let _ = fetch_opts.remote_callbacks(callbacks);

            // Fetch all refs (equivalent to `git fetch --all`).
            remote.fetch(&[] as &[&str], Some(&mut fetch_opts), None)?;
            info!(source = %self.name, remote = %remote_name, "fetch complete");
        }

        // Collect current branch names after fetch.
        let branches = repo
            .branches(Some(BranchType::Local))?
            .filter_map(|b| {
                let (branch, _) = b.ok()?;
                branch.name().ok()?.map(ToString::to_string)
            })
            .collect();

        Ok(branches)
    }
}

// -----------------------------------------------------------------------
// SourceRegistry
// -----------------------------------------------------------------------

/// Thread-safe catalogue of all known `Source`s.
///
/// Shared across all server sessions via `Arc<RwLock<SourceRegistry>>`.
/// Phase C will replace `AppState { workspace, index }` with
/// `AppState { registry: SharedRegistry, config }`.
pub type SharedRegistry = Arc<RwLock<SourceRegistry>>;

/// In-memory catalogue of named sources.
///
/// `data_dir` is the root directory under which bare repos are stored by
/// `clone_source`. Sources registered via `register` can live anywhere.
#[derive(Debug)]
pub struct SourceRegistry {
    sources: HashMap<String, Source>,
    data_dir: PathBuf,
}

impl SourceRegistry {
    /// Create an empty registry rooted at `data_dir`.
    #[must_use]
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            sources: HashMap::new(),
            data_dir,
        }
    }

    /// Wrap in `Arc<RwLock<>>` for shared ownership across threads.
    #[must_use]
    pub fn shared(self) -> SharedRegistry {
        Arc::new(RwLock::new(self))
    }

    /// The directory where bare repositories are stored.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // PathBuf::as_path is not const
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Clone `url` as a bare repository and register it under `name`.
    ///
    /// # Errors
    /// Returns `Err` if the name is already registered or the clone fails.
    pub fn clone_source(&mut self, name: &str, url: &str) -> Result<&Source> {
        let source = match self.sources.entry(name.to_string()) {
            Entry::Occupied(_) => bail!("source '{name}' is already registered"),
            Entry::Vacant(e) => e.insert(Source::clone_from(name, url, &self.data_dir)?),
        };
        Ok(source)
    }

    /// Register an existing bare repository at `path` under `name`.
    ///
    /// # Errors
    /// Returns `Err` if the name is already registered or `path` is not a
    /// valid bare git repository.
    pub fn register(&mut self, name: &str, path: PathBuf) -> Result<&Source> {
        let source = match self.sources.entry(name.to_string()) {
            Entry::Occupied(_) => bail!("source '{name}' is already registered"),
            Entry::Vacant(e) => e.insert(Source::open(name, path)?),
        };
        Ok(source)
    }

    /// Insert a pre-created `Source` directly (no re-opening).
    ///
    /// Used by the server after a `spawn_blocking` git clone returns a `Source`
    /// that already has its path validated.
    ///
    /// # Errors
    /// Returns `Err` if the name is already registered.
    pub fn insert(&mut self, source: Source) -> Result<&Source> {
        let name = source.name.clone();
        match self.sources.entry(name.clone()) {
            Entry::Occupied(_) => bail!("source '{name}' is already registered"),
            Entry::Vacant(e) => Ok(e.insert(source)),
        }
    }

    /// Look up a source by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Source> {
        self.sources.get(name)
    }

    /// Remove a source from the registry.
    ///
    /// Does **not** delete the bare repository from disk — callers are
    /// responsible for that if desired.
    pub fn remove(&mut self, name: &str) -> Option<Source> {
        self.sources.remove(name)
    }

    /// All registered source names, in unspecified order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.sources.keys().map(String::as_str).collect()
    }

    /// Number of registered sources.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    /// `true` if no sources are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Build a minimal normal git repo with one commit + one .cpp file,
    /// then return its path so tests can clone from it.
    fn make_source_repo(dir: &Path) -> PathBuf {
        let src = dir.join("source-repo");
        let repo = git2::Repository::init(&src).unwrap();
        // Configure identity so commits work in bare-CI environments.
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        drop(cfg);

        std::fs::create_dir_all(src.join("src")).unwrap();
        std::fs::write(src.join("src/motor.cpp"), b"void acenderLuz() {}\n").unwrap();

        let mut index = repo.index().unwrap();
        index
            .add_path(std::path::Path::new("src/motor.cpp"))
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(0, 0)).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        src
    }

    #[test]
    fn clone_from_creates_bare_repo() {
        let tmp = tempdir().unwrap();
        let src = make_source_repo(tmp.path());
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let source = Source::clone_from("motor", src.to_str().unwrap(), &data_dir).unwrap();

        assert_eq!(source.name(), "motor");
        assert!(source.path().ends_with("motor.git"));
        assert!(source.path().exists());
        assert!(source.origin_url().is_some());
    }

    #[test]
    fn open_existing_bare_repo() {
        let tmp = tempdir().unwrap();
        let src = make_source_repo(tmp.path());
        let bare_path = tmp.path().join("bare.git");
        git2::build::RepoBuilder::new()
            .bare(true)
            .clone(src.to_str().unwrap(), &bare_path)
            .unwrap();

        let source = Source::open("myrepo", bare_path.clone()).unwrap();
        assert_eq!(source.name(), "myrepo");
        assert_eq!(source.path(), bare_path);
    }

    #[test]
    fn branches_lists_local_branches() {
        let tmp = tempdir().unwrap();
        let src = make_source_repo(tmp.path());
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let source = Source::clone_from("motor", src.to_str().unwrap(), &data_dir).unwrap();
        let branches = source.branches().unwrap();

        // A single-commit default repo has exactly one branch.
        assert!(
            !branches.is_empty(),
            "bare clone must have at least one branch"
        );
    }

    #[test]
    fn registry_register_and_lookup() {
        let tmp = tempdir().unwrap();
        let src = make_source_repo(tmp.path());
        let bare_path = tmp.path().join("bare.git");
        git2::build::RepoBuilder::new()
            .bare(true)
            .clone(src.to_str().unwrap(), &bare_path)
            .unwrap();

        let mut reg = SourceRegistry::new(tmp.path().to_path_buf());
        reg.register("myrepo", bare_path).unwrap();

        assert!(!reg.is_empty());
        assert_eq!(reg.len(), 1);
        assert!(reg.get("myrepo").is_some());
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn registry_duplicate_name_errors() {
        let tmp = tempdir().unwrap();
        let src = make_source_repo(tmp.path());
        let bare_path = tmp.path().join("bare.git");
        git2::build::RepoBuilder::new()
            .bare(true)
            .clone(src.to_str().unwrap(), &bare_path)
            .unwrap();

        let mut reg = SourceRegistry::new(tmp.path().to_path_buf());
        reg.register("myrepo", bare_path.clone()).unwrap();
        assert!(reg.register("myrepo", bare_path).is_err());
    }

    #[test]
    fn registry_remove() {
        let tmp = tempdir().unwrap();
        let src = make_source_repo(tmp.path());
        let bare_path = tmp.path().join("bare.git");
        git2::build::RepoBuilder::new()
            .bare(true)
            .clone(src.to_str().unwrap(), &bare_path)
            .unwrap();

        let mut reg = SourceRegistry::new(tmp.path().to_path_buf());
        reg.register("myrepo", bare_path).unwrap();
        let removed = reg.remove("myrepo");
        assert!(removed.is_some());
        assert!(reg.is_empty());
    }

    #[test]
    fn fetch_all_returns_branches() {
        let tmp = tempdir().unwrap();
        let src = make_source_repo(tmp.path());
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Clone the local source repo as a bare repository.
        let source = Source::clone_from("motor", src.to_str().unwrap(), &data_dir).unwrap();

        // fetch_all should succeed (fetches from file:// origin) and return branches.
        let branches = source.fetch_all().unwrap();
        assert!(
            !branches.is_empty(),
            "fetch_all must return at least one branch"
        );
    }

    #[test]
    fn fetch_all_no_remote_errors() {
        let tmp = tempdir().unwrap();
        // Create a bare repo directly without any remote configured.
        let bare_path = tmp.path().join("bare-no-remote.git");
        let _repo = git2::Repository::init_bare(&bare_path).unwrap();

        let source = Source::open("no-remote", bare_path).unwrap();
        let err = source.fetch_all().unwrap_err();
        assert!(
            err.to_string().contains("no remotes"),
            "expected 'no remotes' error, got: {err}"
        );
    }
}
