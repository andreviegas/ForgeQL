//! Free helper functions shared across `engine` sub-modules.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use tracing::info;

use crate::config::ForgeConfig;
use crate::ir::{Clauses, ForgeQLIR, PredicateValue};

// -----------------------------------------------------------------------
// Verify-config loader
// -----------------------------------------------------------------------

/// Load verify configuration for `source_name`, preferring an external sidecar
/// over the in-repo `.forgeql.yaml`.
///
/// **Sidecar path:** `<repo_dir>/<source_name>.forgeql.yaml` (no commit needed)
/// **Fallback:** walk up from `worktree_path` looking for `.forgeql.yaml`
///
/// Returns `(workdir, config)` where `workdir` is the directory from which
/// VERIFY commands run — always the worktree root when the sidecar is used.
pub(crate) fn load_verify_config(
    repo_path: &Path,
    source_name: &str,
    worktree_path: &Path,
) -> Option<(PathBuf, ForgeConfig)> {
    let sidecar = repo_path
        .parent()
        .map(|p| p.join(format!("{source_name}.forgeql.yaml")));
    if let Some(sc) = sidecar.as_deref().filter(|p| p.exists()) {
        info!(%source_name, path = %sc.display(), "using sidecar .forgeql.yaml");
        return ForgeConfig::load(sc)
            .ok()
            .map(|c| (worktree_path.to_path_buf(), c));
    }
    ForgeConfig::find(worktree_path).and_then(|p| {
        let workdir = p.parent().map(Path::to_path_buf)?;
        ForgeConfig::load(&p).ok().map(|c| (workdir, c))
    })
}

// -----------------------------------------------------------------------
// Session ID helpers
// -----------------------------------------------------------------------

/// Generate a time-based session ID for test-only local sessions.
///
/// Production sessions use the alias from `USE … AS 'alias'` as their key.
/// This helper is only needed by `register_local_session` (test feature flag).
#[cfg(feature = "test-helpers")]
pub(crate) fn generate_session_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("s{millis}")
}

/// Extract the `session_id` from `Option<&str>`, failing if absent or empty.
#[allow(clippy::missing_const_for_fn)] // bail! prevents const
pub(crate) fn require_session_id(session_id: Option<&str>) -> Result<&str> {
    match session_id {
        Some(sid) if !sid.is_empty() => Ok(sid),
        _ => bail!("session_id required — run USE <source>.<branch> first"),
    }
}

// -----------------------------------------------------------------------
// Mutation / IR helpers
// -----------------------------------------------------------------------

/// Determine the operation name for a mutation `ForgeQLIR` variant.
pub(crate) const fn mutation_op_name(op: &ForgeQLIR) -> &'static str {
    match op {
        ForgeQLIR::ChangeContent { .. } => "change_content",
        _ => "unknown_mutation",
    }
}

/// Detect the first numeric WHERE predicate on a non-core enrichment field.
///
/// Returns the field name (e.g. `"member_count"`, `"param_count"`) so the
/// compact renderer can show that value instead of `usages`.  Falls back
/// to `ORDER BY` field when no numeric WHERE is present.
pub(crate) fn detect_metric_hint(clauses: &Clauses) -> Option<String> {
    const CORE_FIELDS: &[&str] = &["name", "node_kind", "path", "line", "usages"];

    // Priority 1: numeric WHERE on enrichment field.
    for pred in &clauses.where_predicates {
        if matches!(pred.value, PredicateValue::Number(_))
            && !CORE_FIELDS.contains(&pred.field.as_str())
        {
            return Some(pred.field.clone());
        }
    }

    // Priority 2: ORDER BY an enrichment field.
    if let Some(ref order) = clauses.order_by
        && !CORE_FIELDS.contains(&order.field.as_str())
    {
        return Some(order.field.clone());
    }

    None
}

/// Reject `WHERE text …` on FIND queries — `text` is only available on
/// commands that return source lines (SHOW body, SHOW LINES, SHOW context).
pub(crate) fn reject_text_filter(clauses: &Clauses) -> Result<()> {
    if clauses
        .where_predicates
        .iter()
        .any(|p| p.field == "text" || p.field == "content")
    {
        bail!(
            "WHERE text/content is not available on FIND queries — \
             it only works on commands that return source lines \
             (SHOW body, SHOW LINES, SHOW context)"
        );
    }
    Ok(())
}
