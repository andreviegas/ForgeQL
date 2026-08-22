#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
//! Storage engine abstraction for ForgeQL.
//!
//! This module defines the [`StorageEngine`] trait — a MySQL-handler–style
//! abstraction that decouples all `exec_*` query paths from the concrete
//! `SymbolTable` type. Every backend (legacy in-RAM, future columnar disk
//! store) implements this trait.
//!
//! Also contains the [`SourceProvider`] trait (see [`source_provider`]) that
//! decouples the storage engine from git internals.
//!
//! # Phase 05.4 scope
//!
//! In this phase:
//! - [`LegacyMemoryStorage`] wraps the existing `SymbolTable` and is the only
//!   active backend. All live queries are served by it.
//! - [`StubColumnarStorage`] is a throwaway empty implementation used to validate
//!   the trait shape compiles for a non-legacy backend.
//! - `SHOW` paths reach the legacy table via `Session::index()` which calls
//!   `BackendSet::legacy_storage()`. The `StorageEngine` trait contains no
//!   legacy-specific escape hatches as of Phase 05.4.

pub mod backend_set;
pub mod columnar;
pub mod git_sha1_provider;
pub mod legacy;
pub mod mock_provider;
pub mod source_provider;
pub mod stub;

/// Whole-path handles: `n<hex>` with no ordinal addresses a **file or
/// directory**, not a node inside one.
///
/// This is deliberately backend-independent. A path handle is the fingerprint of
/// a path plus what is on disk — there is no index in it, nothing is stored, and
/// so no `ENRICH_VER` bump is possible or needed. Every backend answers these,
/// and a backend with an index (the columnar one) only uses its catalogs to skip
/// the worktree walk.
pub mod path_node {
    use anyhow::{Result, anyhow};
    use std::path::{Path, PathBuf};

    use crate::result::FindNodeResult;

    /// Minimum hex chars after `n`. Matches `shortest_prefix_len`'s floor:
    /// below it, an ordinary all-hex symbol name (`nadd`, `nbeef`) would parse
    /// as a file handle wherever a name and a node_id are both accepted.
    const MIN_HEX: usize = 12;

    /// The `<hex>` of a bare handle — `None` when the id carries an ordinal and
    /// so addresses a node inside a file.
    #[must_use]
    pub fn bare_hex(node_id: &str) -> Option<&str> {
        let stripped = node_id.strip_prefix('n')?;
        if stripped.contains('.') {
            return None;
        }
        Some(stripped)
    }

    /// Is this string a whole-path handle, as opposed to a path?
    ///
    /// Stricter than [`bare_hex`], which only asks "no ordinal?" of something
    /// already known to be a node id. This one is asked of an argument that
    /// could be *either* — a `TO` destination — so `notes/` must not read as a
    /// handle merely because it starts with `n`.
    #[must_use]
    pub fn is_handle(value: &str) -> bool {
        bare_hex(value).is_some_and(|hex| {
            hex.len() >= MIN_HEX
                && hex.len() <= 64
                && hex.len().is_multiple_of(2)
                && hex.bytes().all(|b| b.is_ascii_hexdigit())
        })
    }

    /// Normalize and check a bare hex, or say why it is not a handle.
    pub fn validate_hex(node_id: &str, hex: &str) -> Result<String> {
        let hex = hex.to_ascii_lowercase();
        if hex.len() < MIN_HEX
            || hex.len() > 64
            || !hex.len().is_multiple_of(2)
            || !hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(anyhow!("invalid node_id format: {node_id}"));
        }
        Ok(hex)
    }

    /// Does `rel` fingerprint to something starting with `hex`?
    #[must_use]
    pub fn path_matches_hex(rel: &Path, hex: &str) -> bool {
        let full =
            crate::node_id::hex_prefix(&crate::node_id::sha256_of_path(&rel.to_string_lossy()), 64);
        full.starts_with(hex)
    }

    /// Every file in the worktree, workspace-relative — the same membership
    /// `FIND files` reports, so a directory rev folds exactly the files an agent
    /// can see listed.
    #[must_use]
    pub fn worktree_files(root: &Path) -> Vec<PathBuf> {
        let Ok(workspace) = crate::workspace::Workspace::new(root) else {
            return Vec::new();
        };
        workspace
            .files()
            .filter(|p| !crate::result::FileEntry::is_runtime_artifact(p))
            .map(|abs| abs.strip_prefix(root).unwrap_or(&abs).to_path_buf())
            .collect()
    }

    /// Every directory's rev fingerprint, folded in one walk.
    ///
    /// The single definition of what a directory's rev *is*, because three
    /// callers derive one and they must agree to the bit: the rev `FIND files`
    /// stamps on a directory row, the rev the bulk `IF REV` gate re-derives,
    /// and the rev a bare directory handle resolves to. Two of them disagreeing
    /// does not look like a bug — it looks like the directory changed, so every
    /// mutation on it is refused with a rev_mismatch that re-running the FIND
    /// cannot clear.
    ///
    /// A file's fingerprint folds into every ancestor, and the XOR is
    /// order-free, so a directory's rev is independent of walk order and of
    /// which subtree it was reached through. A directory with no files beneath
    /// it has no entry: its rev is 0, the same value an empty fold yields.
    #[must_use]
    pub fn dir_revs(files: &[PathBuf]) -> std::collections::HashMap<PathBuf, u64> {
        let mut map: std::collections::HashMap<PathBuf, u64> = std::collections::HashMap::new();
        for file in files {
            let folded = crate::node_id::fold_path_rev(0, &file.to_string_lossy());
            for dir in file.ancestors().skip(1) {
                if dir.as_os_str().is_empty() {
                    break;
                }
                *map.entry(dir.to_path_buf()).or_default() ^= folded;
            }
        }
        map
    }

    /// Resolve a bare handle against the worktree itself.
    ///
    /// This is the only place a directory can be found (no catalog lists them,
    /// and an empty one is implied by no file path), and it is also where a file
    /// created this session — before the overlay was rebuilt — turns up.
    pub fn resolve_in_worktree(node_id: &str, hex: &str, root: &Path) -> Result<FindNodeResult> {
        let files = worktree_files(root);
        let mut hits: Vec<(PathBuf, bool)> = files
            .iter()
            .filter(|p| path_matches_hex(p, hex))
            .map(|p| (p.clone(), false))
            .collect();

        if let Ok(workspace) = crate::workspace::Workspace::new(root) {
            hits.extend(
                workspace
                    .dirs()
                    .into_iter()
                    .map(|abs| abs.strip_prefix(root).unwrap_or(&abs).to_path_buf())
                    .filter(|p| path_matches_hex(p, hex))
                    .map(|p| (p, true)),
            );
        }
        hits.sort();
        hits.dedup();

        match hits.len() {
            0 => Err(anyhow!("node_id not found: {node_id}")),
            1 => {
                let (rel, is_dir) = &hits[0];
                if *is_dir {
                    Ok(dir_node(node_id, rel, root, &files))
                } else {
                    file_node(node_id, rel, root)
                }
            }
            // Never guess: the caller may be about to delete it.
            n => Err(anyhow!(
                "ambiguous node_id {node_id}: prefix matches {n} paths: {}",
                hits.iter()
                    .map(|(p, _)| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Lines in a byte buffer: a trailing newline does not open a new line.
    fn count_lines(bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }
        // split() yields runs between newlines: run count - 1 == newline count.
        let newlines = bytes.split(|&b| b == b'\n').count() - 1;
        if bytes.last() == Some(&b'\n') {
            newlines
        } else {
            newlines + 1
        }
    }

    /// Synthesize the node for a whole file.
    pub fn file_node(node_id: &str, rel: &Path, root: &Path) -> Result<FindNodeResult> {
        let abs = root.join(rel);
        let bytes = std::fs::read(&abs).map_err(|e| {
            anyhow!(
                "node_id {node_id} resolves to {} which cannot be read: {e}",
                rel.display()
            )
        })?;
        Ok(FindNodeResult {
            node_id: node_id.to_owned(),
            fql_kind: "file".to_owned(),
            name: rel
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: abs,
            line: 1,
            // An empty file still spans line 1: INSERT BEFORE/AFTER needs a line
            // to land against, and that is the create-then-write bootstrap.
            end_line: count_lines(&bytes).max(1),
            rev: crate::node_id::format_rev(crate::node_id::rev_of_bytes(&bytes)),
            parent_node_id: None,
            first_child_node_id: None,
            next_sibling_node_id: None,
            prev_sibling_node_id: None,
        })
    }

    /// Synthesize the node for a whole directory.
    ///
    /// A directory has no bytes, so its rev is a membership XOR over the paths
    /// of every file underneath it: it moves when a file is added, removed,
    /// renamed or moved anywhere in the subtree, and deliberately does not move
    /// when file content changes. That is what a recursive delete has to be
    /// gated on — that the agent saw the current membership, not that it read
    /// every byte. (Content staleness is the per-file rev's job.)
    #[must_use]
    pub fn dir_node(node_id: &str, rel: &Path, root: &Path, files: &[PathBuf]) -> FindNodeResult {
        // Fold every directory to read one of them: the extra work is one hash
        // per file over a walk that already hashed each path to find this
        // handle, and it buys the guarantee that this rev and the one a listing
        // stamps come from the same line of code rather than two that agree
        // today.
        let rev = dir_revs(files).get(rel).copied().unwrap_or(0);
        FindNodeResult {
            node_id: node_id.to_owned(),
            fql_kind: "dir".to_owned(),
            name: format!(
                "{}/",
                rel.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            path: root.join(rel),
            line: 1,
            // A directory spans no lines. `offset_lines` refuses an `(n-m)`
            // suffix on it rather than underflowing.
            end_line: 0,
            rev: crate::node_id::format_rev_exact(rev),
            parent_node_id: None,
            first_child_node_id: None,
            next_sibling_node_id: None,
            prev_sibling_node_id: None,
        }
    }
}

pub use backend_set::BackendSet;
pub use columnar::overlay::Overlay;
pub use columnar::shadow_writer::ShadowWriteResult;
pub use columnar::{
    ColumnarBuildContext, ColumnarStorage, HashFn, OverlayBuilder, SegmentReader, ShadowWriter,
};
pub use legacy::LegacyMemoryStorage;
pub use source_provider::SourceProvider;
pub use stub::StubColumnarStorage;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::ast::index::{IndexRow, IndexStats, OrdinalTombstones, SymbolTable};
use crate::ir::Clauses;
use crate::result::{FileEntry, FindNodeResult, SymbolMatch};
use crate::workspace::Workspace;

// -----------------------------------------------------------------------
// SymbolLocation — lightweight symbol reference for SHOW resolution
// -----------------------------------------------------------------------

/// Identifies the on-disk location of a single symbol definition.
///
/// Returned by [`StorageEngine::resolve_symbol`] and its variants.
/// Contains enough information to re-read and re-parse the source file
/// without retaining any reference into the storage backend.
///
/// The `exec_show` path obtains a `SymbolLocation`, reads the source bytes
/// from disk, and feeds the row info into the tree-sitter re-parser.
#[derive(Debug, Clone)]
pub struct SymbolLocation {
    /// Absolute path to the source file containing the symbol.
    pub path: PathBuf,
    /// Byte range of the symbol node within the source file.
    pub byte_range: std::ops::Range<usize>,
    /// 1-based start line number of the symbol.
    pub line: usize,
    /// Interned language ID (backend-specific).
    pub language_id: u32,
    /// Raw tree-sitter node kind (e.g. `"function_definition"`).
    /// Used by `show_signature` to determine whether to look for a body node.
    pub node_kind: String,
    /// Pre-resolved enrichment fields for this symbol.
    /// Populated by `row_to_location`; empty for non-legacy backends.
    pub enrichment: HashMap<String, String>,
    /// Content SHA-1 of the source file at resolve time, when known.
    ///
    /// Populated by the columnar backend from `SegmentMeta::content_id`.
    /// The legacy backend always leaves this as `None`.
    /// When `Some`, `get_or_parse_for_show` can skip `read_bytes` on a cache
    /// Content SHA-1 of the source file at resolve time, when known.
    ///
    /// Populated by the columnar backend from `SegmentMeta::content_id`.
    /// The legacy backend always leaves this as `None`.
    /// When `Some`, `get_or_parse_for_show` can skip `read_bytes` on a cache
    /// hit and skip `sha1_of_bytes` on a miss.
    pub blob_sha: Option<[u8; 20]>,
    /// Per-file DFS ordinal used to build `node_id` handles.
    ///
    /// `None` for legacy segments and rows without an assigned ordinal.
    pub ordinal: Option<u32>,
}

// -----------------------------------------------------------------------
// FindPage
// -----------------------------------------------------------------------

/// One page of a `FIND symbols` answer, together with the size of the answer
/// it was cut from.
///
/// `rows` is what the query returns after `OFFSET` and `LIMIT`; `total` is how
/// many rows matched before either of them applied. The two travel together
/// because an agent cannot act on the page without the count: `total ==
/// rows.len()` is the only thing that says "this is all of it", and a `total`
/// silently clipped to the page size says that about every first page.
///
/// Every path reports it honestly, including the name-stream fast paths that
/// read only `limit + offset` keys of the name index: they take their `total`
/// from the deduplicated row counts stored per segment at overlay build time
/// (or a kind bitmap's cardinality), and where those counts cannot speak for
/// the answer — two segments built from one source path — the stream declines
/// and the full scan, which collapses before it counts, answers instead.
#[derive(Debug, Clone, Default)]
pub struct FindPage {
    /// The rows this query answers with, after `OFFSET` and `LIMIT`.
    pub rows: Vec<SymbolMatch>,
    /// How many rows matched, before `OFFSET` and `LIMIT` cut the page.
    pub total: usize,
}

impl FindPage {
    /// A page that is the whole answer — every matched row is present.
    ///
    /// This is the honest construction for any backend that filters and pages
    /// in one pass over rows it already holds, because there `total` really is
    /// the length. A backend that stops reading early must NOT use it.
    #[must_use]
    pub const fn whole(rows: Vec<SymbolMatch>) -> Self {
        Self {
            total: rows.len(),
            rows,
        }
    }

    /// A page cut from a larger answer of `total` rows.
    #[must_use]
    pub const fn of(rows: Vec<SymbolMatch>, total: usize) -> Self {
        Self { rows, total }
    }
}

impl std::ops::Deref for FindPage {
    type Target = Vec<SymbolMatch>;

    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}

impl std::ops::DerefMut for FindPage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rows
    }
}

impl IntoIterator for FindPage {
    type Item = SymbolMatch;
    type IntoIter = std::vec::IntoIter<SymbolMatch>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.into_iter()
    }
}

impl<'a> IntoIterator for &'a FindPage {
    type Item = &'a SymbolMatch;
    type IntoIter = std::slice::Iter<'a, SymbolMatch>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter()
    }
}

impl<'a> IntoIterator for &'a mut FindPage {
    type Item = &'a mut SymbolMatch;
    type IntoIter = std::slice::IterMut<'a, SymbolMatch>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter_mut()
    }
}

/// One occurrence site, read as the row it would build.
///
/// `FIND usages` pages by whole files, and which files those are cannot be
/// known until the residual `WHERE` and the `ORDER BY` have run. Running them
/// over sites rather than over rows is what lets the sites nobody will see stay
/// unbuilt: a site becomes a [`SymbolMatch`] only once it is inside the page.
///
/// Every arm of its `ClauseTarget` impl answers exactly what [`usage_row`]
/// writes into the row built from the same site, so the filtering and the
/// ordering here decide what the filtering and the ordering there would have
/// decided. `role` is `None` on a backend that does not tag its sites, which is
/// how the row it builds spells the same absence: an empty map.
pub(crate) struct SiteView<'a> {
    /// The queried name, which every occurrence row carries verbatim.
    pub(crate) name: &'a str,
    /// The file the site sits in.
    pub(crate) path: &'a Path,
    /// 1-based source line of the site.
    pub(crate) line: usize,
    /// The occurrence role, where the backend tags one.
    pub(crate) role: Option<&'a str>,
}

/// The row a [`SiteView`] stands for.
///
/// One constructor for both backends, so the row and the view cannot drift
/// apart in one of them.
pub(crate) fn usage_row(site: &SiteView<'_>) -> SymbolMatch {
    SymbolMatch {
        name: site.name.to_string(),
        node_kind: None,
        fql_kind: None,
        language: None,
        path: Some(site.path.to_path_buf()),
        line: Some(site.line),
        usages_count: None,
        fields: site.role.map_or_else(HashMap::new, |role| {
            HashMap::from([("role".to_owned(), role.to_owned())])
        }),
        count: None,
        node_id: None,
        // A usage site is a line, not a node: no handle, so no rev.
        rev: None,
    }
}

/// The delivery bound a `FIND usages` page is cut to: **whole files**.
///
/// A usage site is one line of one file, and the question behind the query is
/// "which files hold this name?", so `LIMIT` counts files and every site of a
/// selected file is delivered. Cutting the site list at a row count instead
/// would report a file as partly used and drop the rest of it with no marker.
/// That is why this is not the row bound `FIND symbols` hands its engine: the
/// two count different things, so they cannot share a gate.
///
/// Absent for a `GROUP BY`, whose rows are aggregates — one per group already,
/// cut by its own `LIMIT` — and which assigns a `count` no site carries.
#[derive(Debug, Clone, Copy)]
pub struct UsageBound {
    /// How many file groups the page renders.
    pub files: usize,
    /// How many file groups `OFFSET` skips before the page starts.
    pub skip: usize,
    /// How many site rows the page renders before it withholds whole files.
    pub site_ceiling: usize,
}

/// One page of a `FIND usages` answer, with the size of the answer it was cut
/// from and what the cut left out.
///
/// On a site listing — the shape that carries a [`UsageBound`] — `total` is
/// every site that matched and not the page's length, the number a rename
/// campaign measures its progress against, true under an explicit `LIMIT` as
/// much as under the default page. **Under `GROUP BY` it counts groups rather
/// than sites**, and an explicit `LIMIT` clips it, the aggregates being cut
/// before anything counts them; the default page does not clip it, because the
/// caller truncates after taking the count. That is unchanged, and it is the
/// one shape where the sentence above does not hold as written. `withheld` says whether whole files
/// were left out and why, so a partial listing is never a silent one.
#[derive(Debug, Default)]
pub struct UsagePage {
    /// The sites this query renders.
    pub rows: Vec<SymbolMatch>,
    /// How many sites matched, before the file selection cut the page — except
    /// under `GROUP BY`, where it counts groups rather than sites and an
    /// explicit `LIMIT` clips it, those aggregates being cut before anything
    /// counts them. The default page does not clip it.
    pub total: usize,
    /// Which files matched but were not rendered, and why.
    pub withheld: Option<crate::filter::Withheld>,
    /// Files that exist and could not be read, reported as a count.
    pub unread: Option<String>,
}

/// Cut a `FIND usages` page from the sites that matched.
///
/// With a bound, the clause pipeline and the file selection both run over the
/// views, and only the sites inside the page are built. Without one — a
/// `GROUP BY` — the rows are built first, because grouping assigns a `count` a
/// site view has nowhere to put and returns one row per group in any case.
pub(crate) fn usage_page_from_sites(
    mut views: Vec<SiteView<'_>>,
    clauses: &Clauses,
    bound: Option<UsageBound>,
    unread: Option<String>,
) -> UsagePage {
    let Some(bound) = bound else {
        let mut rows: Vec<SymbolMatch> = views.iter().map(usage_row).collect();
        crate::filter::apply_clauses(&mut rows, clauses);
        let total = rows.len();
        return UsagePage {
            rows,
            total,
            withheld: None,
            unread,
        };
    };

    // The count is taken before the page is cut, and from the same pipeline the
    // rows would have gone through: `total` means every matching site whether
    // or not a file holding it was rendered.
    let total = crate::filter::apply_clauses_counted(&mut views, clauses);
    let selected =
        crate::filter::take_file_groups(views, bound.skip, bound.files, bound.site_ceiling);
    UsagePage {
        rows: selected.rows.iter().map(usage_row).collect(),
        total,
        withheld: selected.withheld,
        unread,
    }
}
// -----------------------------------------------------------------------
// -----------------------------------------------------------------------
// StorageEngine trait
// -----------------------------------------------------------------------

/// The central abstraction over all ForgeQL storage backends.
///
/// All `exec_*` query paths go through this trait. The concrete
/// [`LegacyMemoryStorage`] is the default implementation for Phase 01;
/// a columnar disk-backed engine will be added in later phases.
///
/// Implementors must be `Send + Sync` so sessions can be held in a
/// `HashMap` inside `Arc<Mutex<ForgeQLEngine>>`.
pub trait StorageEngine: Send + Sync + 'static {
    /// Short identifier for the backend, e.g. `"legacy"`, `"columnar"`.
    fn backend_name(&self) -> &'static str;

    // -------- read-only queries ----------------------------------------

    /// Execute a `FIND symbols` query.
    ///
    /// Applies all fast-path index shortcuts, predicate evaluation, ORDER BY,
    /// GROUP BY, OFFSET and LIMIT internally. The caller is responsible for
    /// DEFAULT_QUERY_LIMIT truncation and result formatting.
    ///
    /// Returns the page the clauses asked for and, beside it, how many rows
    /// matched before `OFFSET` and `LIMIT` cut that page. `LIMIT` bounds
    /// delivery, never the search, so an implementation must not answer a
    /// smaller `total` because a smaller page was asked for. The name-index
    /// streams honour this without reading past their page: their `total`
    /// comes from the per-segment deduplicated row counts stored at overlay
    /// build time (a kind-filtered stream reads its bitmap cardinality), and
    /// where those stored counts cannot speak for the answer — two segments
    /// built from one source path — the streams decline and the pipeline,
    /// which collapses before it counts, answers instead.
    fn find_symbols(&self, clauses: &Clauses, root: &Path) -> Result<FindPage>;

    /// Execute a `FIND usages OF 'name'` query.
    ///
    /// Applies glob filtering and the remaining clause pipeline internally, and
    /// cuts the page `bound` asks for — whole files, never a row count. With no
    /// bound (`GROUP BY`) it returns the full result set. Either way
    /// [`UsagePage::total`] counts every site that matched, so an explicit
    /// `LIMIT` never makes the count agree with the page.
    ///
    /// **Complete over the files the workspace tracks**, which is what the
    /// columnar backend below implements; the legacy in-memory backend answers
    /// from its index alone and makes no such claim. Every line of every
    /// in-scope tracked file that holds `name` is a row, so an empty result
    /// means those files do not hold it. The authority is the file's own bytes,
    /// not the index: the postings serve the fast tiers and label what they
    /// find, but a site exists wherever the text says it does, including on
    /// lines no recorder ever tokenised. The files read are those that produced
    /// symbols, those known by path and size alone, those reindexed this
    /// session, and those this session created whose extension no plugin claims
    /// — so a name living only in a `.gitignore` is found too. Two files are
    /// outside that set: one excluded by `.gitignore`, `.ignore` or
    /// `.forgeql-ignore`, which nothing here enumerates and indexing never
    /// adds — unless this session touched it, since a mutation records the
    /// path it wrote without consulting any ignore rule: a path no plugin
    /// claims is then listed and searched until the next commit and not after,
    /// while one whose extension a plugin claims gets a segment instead, and a
    /// segment is carried through the commit, so that file stays in both from
    /// then on — and one that reaches the worktree without passing through
    /// ForgeQL at all, written by a build step say, which is in none of them
    /// until it is indexed. `FIND files` excludes exactly the same two, on the
    /// same terms: the pair answer over one universe. No site is dropped for
    /// being expensive to reach.
    ///
    /// Two boundaries, both declared rather than silent. Binary is not
    /// searched: a NUL byte near the start means these bytes are not text, and
    /// a site in an object file would arm a sweep on bytes no editor should
    /// rewrite. A byte-order mark is believed before that check, so UTF-16 text
    /// is read rather than mistaken for an object file — but only UTF-16, and
    /// only where a mark declares it. UTF-16 written without a mark cannot be
    /// told from a compiled object, and UTF-32 is not decoded at all even when
    /// its mark declares it; both are skipped as binary, which is a boundary
    /// and not a finding about the file. Everything else is decoded leniently,
    /// so one byte in a legacy encoding does not blank the lines around it.
    ///
    /// A UTF-16 site can be found and cannot be rewritten in place. A line
    /// boundary there is not a byte boundary, so splicing UTF-8 into it by
    /// offset would shift every byte after the edit; the mutation is refused
    /// with an error naming the encoding, never attempted. That covers
    /// replacing the file whole through a node handle, because a whole-file
    /// `CHANGE NODE` is lowered to a line range like any other. Replacing every
    /// byte at once is safe and is not refused: `CHANGE FILE '<path>' WITH ...`
    /// does it on a non-indexed file, which is what these mostly are; an
    /// indexed one has to be deleted and written again, or converted outside
    /// ForgeQL. Reading such a line back is bounded too: `SHOW` renders the
    /// file's raw bytes, so the decoding here reaches the site list and not the
    /// display.
    ///
    /// An identifier is matched on token boundaries and anything carrying a
    /// character outside that alphabet is matched literally, so reading the
    /// files widens no name into a substring search.
    ///
    /// [`UsagePage::unread`] is not a budget or a ceiling: it reports a specific
    /// thing that went wrong, how many files exist and could not be read, so the
    /// sites they hold are known to be absent instead of silently so. It carries
    /// the count and not the paths. A path the index lists but the worktree no
    /// longer holds is not that case at all — it has no bytes, so the answer over
    /// it is complete.
    fn find_usages(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
        bound: Option<UsageBound>,
    ) -> Result<UsagePage>;

    /// Execute a FIND NODE id query.
    ///
    /// Resolves a node_id to its current location, rev, and nav links.
    /// Returns `None` when the node cannot be matched (deleted or renamed).
    fn find_node(&self, node_id: &str, root: &Path) -> Result<Option<FindNodeResult>> {
        // A bare `n<hex>` handle addresses a whole file or directory. That needs
        // no index — only the path fingerprint and the worktree — so it is
        // answered here, for every backend, rather than in one of them. A
        // backend that does have catalogs (columnar) overrides this to skip the
        // walk; the answer is the same either way.
        if let Some(hex) = path_node::bare_hex(node_id) {
            let hex = path_node::validate_hex(node_id, hex)?;
            return path_node::resolve_in_worktree(node_id, &hex, root).map(Some);
        }
        Ok(None)
    }

    /// Find the node_id of the first symbol that starts at the given source line.
    ///
    /// Used to locate newly inserted symbols after `INSERT BEFORE|AFTER NODE`.
    /// Returns `None` when no addressable symbol starts at that line, or when
    /// this backend does not maintain a columnar index.
    fn find_node_id_at_line(&self, rel_path: &str, line: usize) -> Option<String> {
        let _ = (rel_path, line);
        None
    }

    /// The innermost indexed node whose byte span contains `byte`.
    ///
    /// `SHOW members` reads its rows from the AST, not the index, and the two do
    /// not agree on a node's *line*: the indexed `field` node starts at the
    /// attribute or doc line above the declaration. Containment is the relation
    /// that actually holds between them — the member's first byte lies inside the
    /// indexed node — so handles are attached by span, not by a fuzzy line match.
    ///
    /// Backends without byte spans return `None`; the row then simply carries no
    /// handle, exactly as before.
    fn find_node_id_at_byte(&self, rel_path: &str, byte: usize) -> Option<String> {
        let _ = (rel_path, byte);
        None
    }

    /// For each 1-based source line in `start..=end`, the innermost indexed node
    /// that *contains* it, as `(node_id, node_start_line)` (`None` for a line
    /// covered by no indexed node). An empty `Vec` means this backend keeps no
    /// usable columnar index, so callers fall back to absolute line numbers.
    /// Drives `SHOW LINES` node-relative offset rendering, where a line's offset
    /// is `line - node_start_line + 1`.
    fn innermost_nodes_for_lines(
        &self,
        rel_path: &str,
        root: &Path,
        start: usize,
        end: usize,
    ) -> Vec<Option<(String, usize)>> {
        let _ = (rel_path, root, start, end);
        Vec::new()
    }

    /// Root-node ordinals whose whole span lies inside `[start, end]` (1-based
    /// lines). A removal over that line range must tombstone exactly these so a
    /// byte-identical sibling cannot adopt a freed handle — the rule is derived
    /// from the touched range, not from which verb touched it. Empty for a
    /// backend that keeps no columnar index.
    fn root_ordinals_within(
        &self,
        rel_path: &str,
        root: &Path,
        start: usize,
        end: usize,
    ) -> Vec<u32> {
        let _ = (rel_path, root, start, end);
        Vec::new()
    }

    /// Whether the indexed segment for `rel_path` still matches the file on
    /// disk (content-addressed freshness check).
    ///
    /// Returns `true` when this backend keeps no content-addressed index — it
    /// has no stale absolute line data to serve — or when the stored segment
    /// hash equals the live file's hash. A `false` result tells the caller to
    /// reindex `rel_path` before trusting any line/byte offset for it, which is
    /// what stops a stale committed segment from corrupting a file on
    /// `CHANGE NODE` (BUG-001) or misresolving `FIND NODE` (BUG-002).
    fn is_path_fresh(&self, _rel_path: &Path, _root: &Path) -> bool {
        true
    }

    /// Return all indexed source files as typed [`FileEntry`] rows.
    ///
    /// When `Some` is returned, `FIND files` skips the filesystem walk and
    /// feeds the entries directly into `filter::apply_clauses`.  The paths
    /// are **relative** to the worktree root; the `depth` field is
    /// pre-populated from `path.components().count()`.
    ///
    /// Returns `None` when this backend does not maintain an indexed file
    /// list — the caller falls back to a workspace filesystem walk.
    fn indexed_files(&self) -> Option<Vec<FileEntry>> {
        None
    }

    // -------- symbol resolution (for SHOW) --------------------------------

    /// Resolve a symbol name to its on-disk location.
    ///
    /// Applies `IN`/`EXCLUDE` and `WHERE` clauses to disambiguate when
    /// multiple candidates exist. Returns `Ok(None)` when the symbol is not
    /// found (the caller may emit a friendly error).
    fn resolve_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>>;

    /// Like [`resolve_symbol`] but prefers type definitions with members.
    fn resolve_type_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>>;

    /// Like [`resolve_symbol`] but follows the `body_symbol` redirect field.
    fn resolve_body_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>>;

    // -------- aggregates --------------------------------------------------

    /// Return a reference to the pre-aggregated [`IndexStats`], if the index
    /// has been built.
    fn index_stats(&self) -> Option<&IndexStats>;

    // -------- lifecycle ---------------------------------------------------

    /// Build a fresh index from all files in `workspace`.
    ///
    /// After a successful call `has_index()` returns `true`.
    fn build(&mut self, workspace: &Workspace) -> Result<()>;

    /// Incrementally re-index the given paths after a mutation.
    ///
    /// Deleted files are purged; modified files are re-parsed.
    fn reindex_files(&mut self, paths: &[PathBuf]) -> Result<()>;

    /// Like [`reindex_files`](Self::reindex_files), but tombstones the given
    /// per-path removed **root** ordinals so a byte-identical surviving sibling
    /// cannot adopt a just-deleted node's ordinal. Backends that do
    /// not remap ordinals inherit this default, which ignores the tombstones.
    fn reindex_files_tombstoned(
        &mut self,
        paths: &[PathBuf],
        _tombstones: &OrdinalTombstones,
    ) -> Result<()> {
        self.reindex_files(paths)
    }

    /// Remove all rows originating from a single source file.
    fn purge_file(&mut self, path: &Path) -> Result<()>;

    /// Persist the in-memory index to `<worktree_path>/.forgeql-index`.
    ///
    /// `commit_hash` and `source_name` are stored in the cache header so
    /// that `load_from_cache` can validate freshness on the next resume.
    fn persist_to_cache(
        &mut self,
        worktree_path: &Path,
        commit_hash: &str,
        source_name: &str,
    ) -> Result<()>;

    /// Attempt to load the index from `<worktree_path>/.forgeql-index`.
    ///
    /// Returns `true` on a cache hit (the cached commit matches `head_oid`
    /// and the source name matches), `false` when the cache is absent or
    /// stale (caller should call `build` instead).
    fn load_from_cache(
        &mut self,
        worktree_path: &Path,
        head_oid: &str,
        source_name: &str,
    ) -> Result<bool>;

    /// Drop the in-memory index without saving.
    ///
    /// Used by `ROLLBACK` so the next `resume_index` reads the freshly
    /// restored `.forgeql-index` from disk.
    fn drop_stored_index(&mut self);

    /// Return `true` when an index has been built or loaded from cache.
    /// Return `true` when an index has been built or loaded from cache.
    fn has_index(&self) -> bool;

    /// Flush the dirty overlay to the on-disk `.forgeql-columnar-delta` file.
    ///
    /// Called by `BEGIN TRANSACTION` before `git::stage_and_commit` so the
    /// checkpoint snapshot includes an up-to-date delta file.
    ///
    /// The default no-op is correct for the legacy backend.  `ColumnarStorage`
    /// overrides this to call `DeltaFile::save`.
    fn flush_delta(&mut self) -> Result<()> {
        Ok(())
    }

    /// Reload the dirty overlay from the on-disk `.forgeql-columnar-delta` file.
    ///
    /// Called after `ROLLBACK` (the delta is restored by `git reset --hard`)
    /// and on session reconnect (via `warm_or_open`).
    ///
    /// The default no-op is correct for the legacy backend.  `ColumnarStorage`
    /// overrides this to call `DeltaFile::load`.
    fn reload_dirty_from_delta(&mut self) -> Result<()> {
        Ok(())
    }

    /// Drain the source paths whose staged delta state was dropped by the last
    /// delta load (previous-generation delta after an `ENRICH_VER` upgrade, or
    /// a missing staging segment). The caller must re-index these files or
    /// their pre-edit base rows stay visible.
    ///
    /// The default empty list is correct for the legacy backend, which has no
    /// delta file.
    fn take_pending_reindex_paths(&mut self) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Promote staging segments and build a new overlay for `new_commit_oid`.
    ///
    /// Called by `exec_commit` after the git commit succeeds.  The default
    /// no-op is correct for the legacy backend.  `ColumnarStorage` overrides
    /// this to promote staged segments to the bare-repo store and rebuild the
    /// overlay via `OverlayBuilder::from_merge`.
    fn commit_dirty(&mut self, _new_commit_oid: &str, _ctx: &ColumnarBuildContext) -> Result<()> {
        Ok(())
    }
    // -------- SHOW helpers ------------------------------------------------

    /// Locate a symbol definition by name, returning its file path and line.
    ///
    /// Used by `show_callees` to annotate each callee with its definition
    /// location. Returns `None` when the name is not found or the backend
    /// does not support definition lookup.
    fn locate_definition(&self, _name: &str) -> Option<(PathBuf, usize)> {
        None
    }

    /// Render `SHOW outline OF file` as a JSON value.
    ///
    /// Delegates to the backend symbol rows so `exec_show` does not need to
    /// hold a `&SymbolTable` reference and can work across all backends.
    /// `all = false` returns only structural declarations (functions, types,
    /// namespaces, …); `all = true` returns every node. A `node_id` passed as
    /// `file` scopes the outline to that node's subtree.
    fn show_outline_for_file(
        &self,
        workspace: &Workspace,
        file: &str,
        all: bool,
    ) -> Result<serde_json::Value>;
}

pub(crate) fn row_to_location(row: &IndexRow, table: &SymbolTable) -> SymbolLocation {
    SymbolLocation {
        path: table.path_of(row).to_path_buf(),
        byte_range: row.byte_range.clone(),
        line: row.line,
        language_id: row.language_id,
        node_kind: table.node_kind_of(row).to_string(),
        enrichment: table.strings.resolve_fields(&row.fields),
        blob_sha: None,
        ordinal: row.ordinal,
    }
}
// -----------------------------------------------------------------------
// Phase 01 integration tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        SiteView, StorageEngine, SymbolLocation, UsageBound, usage_page_from_sites, usage_row,
    };
    use crate::{
        ir::Clauses,
        storage::{
            mock_provider::MockProvider,
            source_provider::{ContentId, SourceProvider},
            stub::StubColumnarStorage,
        },
    };

    // --- StubColumnarStorage (trait shape) ---

    #[test]
    fn stub_backend_name() {
        let s = StubColumnarStorage;
        assert_eq!(s.backend_name(), "stub");
    }

    #[test]
    fn stub_has_no_index() {
        let s = StubColumnarStorage;
        assert!(!s.has_index());
    }

    #[test]
    fn stub_find_symbols_returns_empty() {
        let s = StubColumnarStorage;
        let clauses = Clauses::default();
        let root = Path::new("/tmp");
        let results = s.find_symbols(&clauses, root).expect("should not error");
        assert!(results.is_empty());
    }

    #[test]
    fn stub_find_usages_returns_empty() {
        let s = StubColumnarStorage;
        let clauses = Clauses::default();
        let root = Path::new("/tmp");
        let page = s
            .find_usages("foo", &clauses, root, None)
            .expect("should not error");
        assert!(page.rows.is_empty());
    }

    /// A site view has to answer every clause field exactly as the row built
    /// from it answers it, because `FIND usages` filters and orders the views
    /// and then builds only the survivors: a field the two disagree on is a
    /// missing result, not a slow one.
    ///
    /// The probe list is the row's own declared fields plus the one name an
    /// occurrence row writes into its open map and one no row carries, so a
    /// field added to the row without being added to the view fails here.
    #[test]
    fn a_site_view_reads_every_field_as_the_row_it_builds() {
        use crate::filter::ClauseTarget as _;
        use crate::result::SymbolMatch;

        let probes: Vec<&str> = <SymbolMatch as crate::filter::ClauseTarget>::STR_FIELDS
            .iter()
            .chain(<SymbolMatch as crate::filter::ClauseTarget>::NUM_FIELDS)
            .copied()
            .chain(["role", "line", "lines", "naming", "no_such_field"])
            .collect();

        // A role that parses as a number as well as one that does not, and a
        // backend that tags none at all.
        for role in [Some("code"), Some("42"), None] {
            let view = SiteView {
                name: "needle",
                path: Path::new("src/lib.rs"),
                line: 7,
                role,
            };
            let row = usage_row(&view);

            for field in &probes {
                assert_eq!(
                    view.field_str(field),
                    row.field_str(field),
                    "field_str({field}) differs with role {role:?}"
                );
                assert_eq!(
                    view.field_num(field),
                    row.field_num(field),
                    "field_num({field}) differs with role {role:?}"
                );
            }
            assert_eq!(view.path(), row.path(), "path() differs");
        }
    }

    /// The page is whole files and the count is every site, and the two are
    /// independent: a site in a file the page does not render is still counted.
    #[test]
    fn a_bounded_page_renders_whole_files_and_still_counts_the_rest() {
        let sites = [
            (Path::new("a.rs"), 1),
            (Path::new("a.rs"), 2),
            (Path::new("b.rs"), 3),
            (Path::new("c.rs"), 4),
            (Path::new("c.rs"), 5),
        ];
        let views = || {
            sites
                .iter()
                .map(|&(path, line)| SiteView {
                    name: "needle",
                    path,
                    line,
                    role: Some("code"),
                })
                .collect::<Vec<_>>()
        };

        let page = usage_page_from_sites(
            views(),
            &Clauses::default(),
            Some(UsageBound {
                files: 2,
                skip: 0,
                site_ceiling: 1_000,
            }),
            None,
        );
        assert_eq!(page.rows.len(), 3, "two whole files, not two rows");
        assert_eq!(page.total, 5, "every site counts, rendered or not");
        assert_eq!(page.withheld, Some(crate::filter::Withheld::Limit));

        // No files asked for: nothing is built, and the count is still the
        // whole answer. This is the shape the zero-symbols hint queries with.
        let counted = usage_page_from_sites(
            views(),
            &Clauses::default(),
            Some(UsageBound {
                files: 0,
                skip: 0,
                site_ceiling: 0,
            }),
            None,
        );
        assert!(counted.rows.is_empty());
        assert_eq!(counted.total, 5);

        // No bound at all: the whole set, and nothing withheld.
        let whole = usage_page_from_sites(views(), &Clauses::default(), None, None);
        assert_eq!(whole.rows.len(), 5);
        assert_eq!(whole.total, 5);
        assert_eq!(whole.withheld, None);
    }

    #[test]
    fn stub_resolve_symbol_returns_none() {
        let s = StubColumnarStorage;
        let clauses = Clauses::default();
        let root = Path::new("/tmp");
        let loc: Option<SymbolLocation> = s
            .resolve_symbol("foo", &clauses, root)
            .expect("should not error");
        assert!(loc.is_none());
    }

    #[test]
    fn stub_persist_and_load_are_noops() {
        let mut s = StubColumnarStorage;
        s.persist_to_cache(Path::new("/tmp"), "abc123", "test")
            .expect("persist noop");
        let loaded = s
            .load_from_cache(Path::new("/tmp"), "abc123", "test")
            .expect("load noop");
        assert!(!loaded, "stub always returns false for load");
    }

    // --- MockProvider (SourceProvider shape) ---

    #[test]
    fn mock_provider_insert_and_read() {
        let mut p = MockProvider::default();
        let id = p.insert(b"hello");
        let bytes = p.read_content(&id).expect("blob must exist");
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn mock_provider_hash_is_deterministic() {
        let p = MockProvider::default();
        let id1 = p.hash_content(b"world");
        let id2 = p.hash_content(b"world");
        assert_eq!(id1.hex(), id2.hex(), "same bytes must produce same id");
    }

    #[test]
    fn mock_provider_walk_snapshot() {
        let mut p = MockProvider::default();
        let id = p.insert(b"fn foo() {}");
        p.add_snapshot("snap-a", vec![(PathBuf::from("src/foo.rs"), id.clone())]);
        p.set_current("snap-a");

        let snap = p
            .current_snapshot(Path::new("/repo"))
            .expect("current snap");
        let entries: Vec<_> = p
            .walk_snapshot(&snap)
            .expect("walk ok")
            .map(|r| r.expect("entry ok"))
            .collect();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, PathBuf::from("src/foo.rs"));
        assert_eq!(entries[0].1.hex(), id.hex());
    }

    #[test]
    fn mock_provider_changed_paths() {
        let mut p = MockProvider::default();
        let id_a = p.insert(b"v1");
        let id_b = p.insert(b"v2");
        let id_c = p.insert(b"new");

        p.add_snapshot(
            "snap-1",
            vec![
                (PathBuf::from("a.rs"), id_a.clone()),
                (PathBuf::from("b.rs"), id_b),
            ],
        );
        p.add_snapshot(
            "snap-2",
            vec![
                (PathBuf::from("a.rs"), id_a),
                (PathBuf::from("b.rs"), id_c.clone()),
                (PathBuf::from("c.rs"), id_c),
            ],
        );

        let from = MockProvider::mock_snapshot("snap-1");
        let to = MockProvider::mock_snapshot("snap-2");

        let changed = p.changed_paths(&from, &to).expect("changed paths ok");
        assert!(changed.contains(&PathBuf::from("b.rs")));
        assert!(changed.contains(&PathBuf::from("c.rs")));
        assert!(!changed.contains(&PathBuf::from("a.rs")));
    }

    #[test]
    fn stub_satisfies_dyn_trait_bound() {
        let _: Box<dyn StorageEngine> = Box::new(StubColumnarStorage);
    }

    // --- directory revs ---

    #[test]
    fn a_directory_rev_folds_every_file_beneath_it_at_any_depth() {
        let files = [
            PathBuf::from("src/a.rs"),
            PathBuf::from("src/deep/b.rs"),
            PathBuf::from("other/c.rs"),
        ];
        let revs = super::path_node::dir_revs(&files);
        let fold = |paths: &[&str]| {
            paths
                .iter()
                .fold(0u64, |acc, p| crate::node_id::fold_path_rev(acc, p))
        };
        assert_eq!(
            revs.get(Path::new("src")).copied(),
            Some(fold(&["src/a.rs", "src/deep/b.rs"])),
            "a directory covers its whole subtree, not just its own children"
        );
        assert_eq!(
            revs.get(Path::new("src/deep")).copied(),
            Some(fold(&["src/deep/b.rs"]))
        );
        // The root is not a member of any listing, so it is not folded either.
        assert_eq!(revs.get(Path::new("")).copied(), None);
        assert_eq!(
            revs.get(Path::new("nothing")).copied(),
            None,
            "an unrelated path folds nothing"
        );
    }

    #[test]
    fn dir_node_reports_the_same_rev_a_listing_stamps() {
        // The three derivations of a directory rev — a listing's stamp, the
        // bulk IF REV gate, and this handle resolve — must agree to the bit or
        // every mutation on a directory is refused with a rev_mismatch that
        // re-running the FIND cannot clear. They agree because they are one
        // function; this asserts the handle path really routes through it.
        let files = [PathBuf::from("src/a.rs"), PathBuf::from("src/deep/b.rs")];
        let expected = super::path_node::dir_revs(&files)
            .get(Path::new("src"))
            .copied()
            .expect("src holds files");
        let node = super::path_node::dir_node(
            "nabcdef012345",
            Path::new("src"),
            Path::new("/nonexistent-root"),
            &files,
        );
        assert_eq!(node.rev, crate::node_id::format_rev_exact(expected));
    }

    #[test]
    fn a_directory_with_no_files_beneath_it_revs_as_zero() {
        let node = super::path_node::dir_node(
            "nabcdef012345",
            Path::new("empty"),
            Path::new("/nonexistent-root"),
            &[PathBuf::from("src/a.rs")],
        );
        assert_eq!(node.rev, crate::node_id::format_rev_exact(0));
    }
}
