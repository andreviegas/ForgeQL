//! Guard enrichment utilities — shared across `collect_nodes()`,
//! `ShadowEnricher`, and `DeclDistanceEnricher`.
//!
//! Provides the `GuardFrame` stack model, `GuardInfo` for mutual-exclusivity
//! checks, and helpers for building and consuming guard frames.

use regex::RegexSet;
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ast::lang::LanguageConfig;

// -----------------------------------------------------------------------
// Group-ID counter
// -----------------------------------------------------------------------

/// Global guard group-ID counter. Relaxed ordering suffices: only
/// uniqueness across rayon threads is needed, not happens-before ordering.
static NEXT_GUARD_GROUP_ID: AtomicU64 = AtomicU64::new(1);

fn next_group_id() -> u64 {
    NEXT_GUARD_GROUP_ID.fetch_add(1, Ordering::Relaxed)
}

// -----------------------------------------------------------------------
// GuardFrame
// -----------------------------------------------------------------------

/// One entry in the per-file guard traversal stack.
///
/// Built by [`build_guard_frame`] for every guard-opening AST node
/// (`preproc_ifdef`, `preproc_if`, `preproc_elif`, `preproc_else`, etc.)
/// encountered during the tree walk in `collect_nodes()`.
pub struct GuardFrame {
    /// Raw condition text (e.g. `"defined(CONFIG_SMP)"` or `"!X"`).
    pub guard_text: String,
    /// Unique ID shared by all arms of the same `if`/`elif`/`else` group.
    pub guard_group_id: u64,
    /// Ordinal within the group: 0 = if, 1 = first elif/else, 2 = second, …
    pub guard_branch: u8,
    /// Guard mechanism: `"preprocessor"` | `"attribute"` | `"build_tag"` |
    /// `"comptime"` | `"heuristic"`.
    pub guard_kind: &'static str,
    /// Identifiers that **must be defined** for this branch.
    pub defines: Vec<String>,
    /// Identifiers that **must be undefined** for this branch.
    pub negates: Vec<String>,
    /// All identifiers mentioned in the condition (superset of defines + negates).
    pub mentions: Vec<String>,
    /// Byte span of the guard-opening AST node; used to pop stale frames
    /// by comparing `node.start_byte() >= frame.guard_byte_range.end`.
    pub guard_byte_range: std::ops::Range<usize>,
}

// -----------------------------------------------------------------------
// GuardInfo — compact identity for exclusivity checks
// -----------------------------------------------------------------------

/// Compact representation of a symbol's innermost guard membership.
#[derive(Clone, Copy)]
pub struct GuardInfo {
    pub guard_group_id: u64,
    pub guard_branch: u8,
    pub guard_kind: &'static str,
}

/// Returns `true` iff `a` and `b` are in structurally exclusive branches:
/// same group, different branch, and neither is `"heuristic"`.
#[must_use]
pub fn are_guards_exclusive(a: &GuardInfo, b: &GuardInfo) -> bool {
    a.guard_kind != "heuristic"
        && b.guard_kind != "heuristic"
        && a.guard_group_id == b.guard_group_id
        && a.guard_branch != b.guard_branch
}

/// Extract `GuardInfo` from a row's pre-computed fields map.
#[must_use]
pub fn guard_info_from_fields<S: std::hash::BuildHasher>(
    fields: &HashMap<String, String, S>,
) -> Option<GuardInfo> {
    let group_id: u64 = fields.get("guard_group_id")?.parse().ok()?;
    let branch: u8 = fields.get("guard_branch")?.parse().ok()?;
    let kind = static_guard_kind(
        fields
            .get("guard_kind")
            .map_or("preprocessor", String::as_str),
    );
    Some(GuardInfo {
        guard_group_id: group_id,
        guard_branch: branch,
        guard_kind: kind,
    })
}

/// Extract `GuardInfo` from the current guard stack.
///
/// Returns the innermost frame's identity, which matches what
/// [`inject_guard_fields`] writes into `guard_group_id` / `guard_branch`.
#[must_use]
pub fn guard_info_from_stack(stack: &[GuardFrame]) -> Option<GuardInfo> {
    let frame = stack.last()?;
    Some(GuardInfo {
        guard_group_id: frame.guard_group_id,
        guard_branch: frame.guard_branch,
        guard_kind: frame.guard_kind,
    })
}

fn static_guard_kind(s: &str) -> &'static str {
    match s {
        "attribute" => "attribute",
        "build_tag" => "build_tag",
        "comptime" => "comptime",
        "heuristic" => "heuristic",
        _ => "preprocessor",
    }
}

// -----------------------------------------------------------------------
// inject_guard_fields
// -----------------------------------------------------------------------

/// Write guard enrichment fields from `stack` into a row's field map.
///
/// For each unique `guard_group_id`, only the innermost (top-of-stack) frame
/// for that group is used. Guards from different groups are combined with
/// ` && `. The innermost unique frame's `guard_group_id` and `guard_branch`
/// are used for structural exclusivity checks.
///
/// Writes: `guard`, `guard_defines`, `guard_negates`, `guard_mentions`,
/// `guard_group_id`, `guard_branch`, `guard_kind`.
pub fn inject_guard_fields<S: std::hash::BuildHasher>(
    stack: &[GuardFrame],
    fields: &mut HashMap<String, String, S>,
) {
    if stack.is_empty() {
        return;
    }

    // Deduplicate: for each group, keep only the innermost (highest-index) frame.
    // Walk from innermost (rev) and collect the first occurrence of each group.
    let mut seen_groups = std::collections::HashSet::new();
    let mut active: Vec<&GuardFrame> = Vec::new();
    for frame in stack.iter().rev() {
        if seen_groups.insert(frame.guard_group_id) {
            active.push(frame);
        }
    }
    // active[0] = innermost unique; reverse to outermost-first for combined text.
    active.reverse();

    let texts: Vec<&str> = active.iter().map(|f| f.guard_text.as_str()).collect();
    let guard = texts.join(" && ");

    let mut all_defines: Vec<&str> = Vec::new();
    let mut all_negates: Vec<&str> = Vec::new();
    let mut all_mentions: BTreeSet<&str> = BTreeSet::new();
    for frame in &active {
        for d in &frame.defines {
            all_defines.push(d.as_str());
            let _ = all_mentions.insert(d.as_str());
        }
        for n in &frame.negates {
            all_negates.push(n.as_str());
            let _ = all_mentions.insert(n.as_str());
        }
        for m in &frame.mentions {
            let _ = all_mentions.insert(m.as_str());
        }
    }
    // Innermost unique frame: governs guard_group_id / guard_branch.
    let Some(innermost) = active.last() else {
        return;
    };

    drop(fields.insert("guard".into(), guard));
    if !all_defines.is_empty() {
        drop(fields.insert("guard_defines".into(), all_defines.join(",")));
    }
    if !all_negates.is_empty() {
        drop(fields.insert("guard_negates".into(), all_negates.join(",")));
    }
    if !all_mentions.is_empty() {
        let m: Vec<&str> = all_mentions.into_iter().collect();
        drop(fields.insert("guard_mentions".into(), m.join(",")));
    }
    drop(fields.insert(
        "guard_group_id".into(),
        innermost.guard_group_id.to_string(),
    ));
    drop(fields.insert("guard_branch".into(), innermost.guard_branch.to_string()));
    drop(fields.insert("guard_kind".into(), innermost.guard_kind.to_string()));
}

// -----------------------------------------------------------------------
// build_guard_frame
// -----------------------------------------------------------------------

/// Build a [`GuardFrame`] for a guard-opening AST node.
///
/// `stack` is the current guard stack; it is used to inherit the parent
/// group's ID and branch count for `elif`/`else` nodes.
pub fn build_guard_frame(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    stack: &[GuardFrame],
) -> GuardFrame {
    let kind = node.kind();

    if config.is_elif_kind(kind) || config.is_else_kind(kind) {
        // Sibling arm: inherit group from the top of the stack.
        let (group_id, prev_branch) = stack.last().map_or_else(
            || (next_group_id(), 0),
            |f| (f.guard_group_id, f.guard_branch),
        );
        let branch = prev_branch.saturating_add(1);

        let (guard_text, defines, negates, mentions) = if config.is_elif_kind(kind) {
            let cond = field_text(node, source, config.guard_condition_field());
            let (defs, negs, ments) = parse_condition_text(cond);
            (cond.to_string(), defs, negs, ments)
        } else {
            // #else: negate the parent frame's condition.
            stack.last().map_or_else(
                || (String::new(), Vec::new(), Vec::new(), Vec::new()),
                negate_frame,
            )
        };

        GuardFrame {
            guard_text,
            guard_group_id: group_id,
            guard_branch: branch,
            guard_kind: "preprocessor",
            defines,
            negates,
            mentions,
            guard_byte_range: node.byte_range(),
        }
    } else {
        // New guard group (preproc_ifdef, preproc_if, etc.)
        let group_id = next_group_id();
        let (guard_text, defines, negates, mentions) = extract_block_guard(node, source, config);
        GuardFrame {
            guard_text,
            guard_group_id: group_id,
            guard_branch: 0,
            guard_kind: "preprocessor",
            defines,
            negates,
            mentions,
            guard_byte_range: node.byte_range(),
        }
    }
}

// -----------------------------------------------------------------------
// Private helpers
// -----------------------------------------------------------------------

/// Return the source text of the named field child, or `""`.
fn field_text<'a>(node: tree_sitter::Node<'_>, source: &'a [u8], field: &str) -> &'a str {
    if field.is_empty() {
        return "";
    }
    node.child_by_field_name(field)
        .and_then(|child| source.get(child.byte_range()))
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("")
}

/// Return the full source text of a node.
fn node_src<'a>(source: &'a [u8], node: tree_sitter::Node<'_>) -> &'a str {
    source
        .get(node.byte_range())
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("")
}

/// Extract guard info from a block-guard node (`preproc_ifdef`, `preproc_if`).
///
/// Returns `(guard_text, defines, negates, mentions)`.
fn extract_block_guard(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> (String, Vec<String>, Vec<String>, Vec<String>) {
    let name_field = config.guard_name_field();

    if !name_field.is_empty()
        && let Some(name_child) = node.child_by_field_name(name_field)
    {
        let ident = node_src(source, name_child).trim().to_string();
        if !ident.is_empty() {
            // Detect negation: first child token text matches negate_ifdef_variant.
            let negate_marker = config.negate_ifdef_variant();
            let is_negated = !negate_marker.is_empty()
                && node
                    .child(0)
                    .is_some_and(|t| node_src(source, t).trim() == negate_marker);

            return if is_negated {
                (
                    format!("!{ident}"),
                    Vec::new(),
                    vec![ident.clone()],
                    vec![ident],
                )
            } else {
                (ident.clone(), vec![ident.clone()], Vec::new(), vec![ident])
            };
        }
    }

    // Fallback: read `condition_field` (preproc_if and similar).
    let cond = field_text(node, source, config.guard_condition_field());
    let (defs, negs, ments) = parse_condition_text(cond);
    (cond.to_string(), defs, negs, ments)
}

/// Produce the `else` complement of a parent frame.
///
/// Returns `(guard_text, defines, negates, mentions)`.
fn negate_frame(parent: &GuardFrame) -> (String, Vec<String>, Vec<String>, Vec<String>) {
    let guard_text = if parent.guard_text.is_empty() {
        String::new()
    } else if parent.guard_text.starts_with('!') && !parent.guard_text.starts_with("!(") {
        // Simple `!X` → `X`
        parent.guard_text[1..].to_string()
    } else if parent.guard_text.contains(' ') {
        format!("!({})", parent.guard_text)
    } else {
        format!("!{}", parent.guard_text)
    };
    // Defines become negates and vice-versa; mentions stay the same.
    (
        guard_text,
        parent.negates.clone(),
        parent.defines.clone(),
        parent.mentions.clone(),
    )
}

/// Parse a `#if`/`#elif` condition expression into `(defines, negates, mentions)`.
///
/// Conservative rules:
/// - `defined(X)` → defines, mentions
/// - `!defined(X)` → negates, mentions
/// - `defined(A) && defined(B)` → defines = [A, B]
/// - `defined(A) || defined(B)` → defines = [] (ambiguous), mentions = [A, B]
fn parse_condition_text(cond: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let has_or = cond.contains("||");
    let mut defines = Vec::new();
    let mut negates = Vec::new();
    let mut mentions = Vec::new();
    let mut pos = 0;

    while pos < cond.len() {
        let Some(rel) = cond[pos..].find("defined") else {
            break;
        };
        let def_pos = pos + rel;

        // Content after "defined", with leading whitespace stripped.
        let rest = &cond[def_pos + 7..];
        let rest_trimmed = rest.trim_start();

        if !rest_trimmed.starts_with('(') {
            pos = def_pos + 7;
            continue;
        }

        let inner = &rest_trimmed[1..]; // after '('
        let Some(close) = inner.find(')') else {
            pos = def_pos + 7;
            continue;
        };
        let ident = inner[..close].trim();

        if !ident.is_empty() && ident.chars().all(|c| c.is_alphanumeric() || c == '_') {
            let before = cond[..def_pos].trim_end();
            let is_negated = before.ends_with('!');
            let ident = ident.to_string();
            mentions.push(ident.clone());
            if is_negated {
                negates.push(ident);
            } else if !has_or {
                defines.push(ident);
            }
        }

        // Advance past ')': position of '(' in rest + 1 (inner) + close + 1 (past ')')
        let lead_ws = rest.len() - rest_trimmed.len();
        pos = def_pos + 7 + lead_ws + 1 + close + 1;
    }

    (defines, negates, mentions)
}

// -----------------------------------------------------------------------
// Item-level attribute guard extraction (e.g. Rust `#[cfg(...)]`)
// -----------------------------------------------------------------------

/// Scan the preceding named siblings of `node` for `attribute_item` nodes
/// whose attribute identifier matches `attr_name` (e.g. `"cfg"` for Rust).
///
/// Returns one [`GuardFrame`] per matching attribute, in document order
/// (outermost / topmost attribute first).  Stops scanning as soon as a
/// non-`attribute_item` named sibling is reached.
#[must_use]
pub fn collect_attribute_guard_frames(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    attr_name: &str,
) -> Vec<GuardFrame> {
    let mut frames = Vec::new();
    let mut cursor = node.prev_named_sibling();
    while let Some(sib) = cursor {
        if sib.kind() != "attribute_item" {
            break;
        }
        if let Some(frame) = attribute_item_to_guard_frame(sib, source, attr_name) {
            frames.push(frame);
        }
        cursor = sib.prev_named_sibling();
    }
    // Reverse: collected in reverse document order (innermost first),
    // so reverse to put outermost attribute first — matching stack ordering.
    frames.reverse();
    frames
}

/// Try to extract a [`GuardFrame`] from a single `attribute_item` node.
///
/// Returns `None` if the first attribute identifier does not match `attr_name`.
///
/// Tree-sitter-rust `attribute_item` layout:
/// ```text
/// attribute_item
///   attribute
///     identifier   <- must equal attr_name (e.g. "cfg")
///     token_tree   <- "(test)", "(feature = \"std\")", "(not(test))", ...
/// ```
fn attribute_item_to_guard_frame(
    attr_item: tree_sitter::Node<'_>,
    source: &[u8],
    attr_name: &str,
) -> Option<GuardFrame> {
    let attribute = attr_item
        .named_child(0)
        .filter(|n| n.kind() == "attribute")?;
    let ident_node = attribute.named_child(0)?;
    if node_src(source, ident_node) != attr_name {
        return None;
    }
    // token_tree text includes the surrounding parens: "(test)" -- strip them.
    let args_text = attribute
        .named_child(1)
        .map(|tt| {
            let raw = node_src(source, tt);
            raw.trim_start_matches('(')
                .trim_end_matches(')')
                .trim()
                .to_string()
        })
        .unwrap_or_default();

    let (guard_text, defines, negates, mentions) = parse_cfg_condition(&args_text);
    Some(GuardFrame {
        guard_text,
        guard_group_id: next_group_id(),
        guard_branch: 0,
        guard_kind: "attribute",
        defines,
        negates,
        mentions,
        guard_byte_range: attr_item.byte_range(),
    })
}

/// Parse a Rust `cfg(...)` inner condition into `(text, defines, negates, mentions)`.
///
/// Conservative rules:
/// - `not(X)` -> negates = [X], `guard_text` = "!X"
/// - `all(A, B, ...)` -> defines each simple identifier in the list
/// - `any(A, B, ...)` -> mentions only (ambiguous -- either branch may be active)
/// - `X` (bare identifier) -> defines = [X]
/// - `key = "value"` -> defines = [key]
/// - Anything complex -> text only, empty lists
fn parse_cfg_condition(cond: &str) -> (String, Vec<String>, Vec<String>, Vec<String>) {
    let cond = cond.trim();
    if cond.is_empty() {
        return (String::new(), Vec::new(), Vec::new(), Vec::new());
    }
    if let Some(not_inner) = strip_cfg_wrapper(cond, "not") {
        let trimmed = not_inner.trim();
        let id = cfg_simple_ident(trimmed);
        let text = format!("!{trimmed}");
        return if id.is_empty() {
            (text, Vec::new(), Vec::new(), Vec::new())
        } else {
            (text, Vec::new(), vec![id.clone()], vec![id])
        };
    }
    if let Some(all_inner) = strip_cfg_wrapper(cond, "all") {
        let parts = split_cfg_top_level(all_inner);
        let mut defines: Vec<String> = Vec::new();
        let mut mentions: Vec<String> = Vec::new();
        for p in &parts {
            let id = cfg_extract_key(p.trim());
            if !id.is_empty() {
                defines.push(id.clone());
                mentions.push(id);
            }
        }
        return (cond.to_string(), defines, Vec::new(), mentions);
    }
    if let Some(any_inner) = strip_cfg_wrapper(cond, "any") {
        let parts = split_cfg_top_level(any_inner);
        let mut mentions: Vec<String> = Vec::new();
        for p in &parts {
            let id = cfg_extract_key(p.trim());
            if !id.is_empty() {
                mentions.push(id);
            }
        }
        return (cond.to_string(), Vec::new(), Vec::new(), mentions);
    }
    // Bare identifier or `key = "value"`.
    let id = cfg_extract_key(cond);
    if id.is_empty() {
        (cond.to_string(), Vec::new(), Vec::new(), Vec::new())
    } else {
        (cond.to_string(), vec![id.clone()], Vec::new(), vec![id])
    }
}

/// Strip a `name(...)` wrapper, returning the inner text, or `None`.
fn strip_cfg_wrapper<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    s.strip_prefix(name)
        .and_then(|r| r.strip_prefix('('))
        .and_then(|r| r.strip_suffix(')'))
}

/// Extract the key portion from a cfg predicate: `key` or `key = "value"`.
///
/// Returns an empty string if the key is not a valid identifier.
fn cfg_extract_key(s: &str) -> String {
    let key = s.split('=').next().unwrap_or("").trim();
    if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_') {
        key.to_string()
    } else {
        String::new()
    }
}

/// Extract a bare cfg identifier (no `=` allowed).
fn cfg_simple_ident(s: &str) -> String {
    let s = s.trim();
    if !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_') {
        s.to_string()
    } else {
        String::new()
    }
}

/// Split a comma-separated cfg argument list, respecting nested parentheses.
fn split_cfg_top_level(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                result.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    result.push(&s[start..]);
    result
}

// -----------------------------------------------------------------------
// Heuristic env-guard detection (Python `if TYPE_CHECKING:`, etc.)
// -----------------------------------------------------------------------

/// Try to build a heuristic [`GuardFrame`] for a Python-style `if` node
/// whose condition text matches one of the pre-compiled `env_guard_patterns`.
///
/// Returns `None` if no pattern matches or the condition is empty.
/// The resulting frame always has `guard_kind = "heuristic"`.
#[must_use]
pub fn build_env_guard_frame(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    patterns: &RegexSet,
) -> Option<GuardFrame> {
    let cond_field = config.guard_condition_field();
    let cond_text = if cond_field.is_empty() {
        node.named_child(0).map_or("", |c| node_src(source, c))
    } else {
        field_text(node, source, cond_field)
    };
    if cond_text.is_empty() || !patterns.is_match(cond_text) {
        return None;
    }
    let guard_text = cond_text.to_string();
    let id = cfg_simple_ident(cond_text.trim());
    let (defines, mentions) = if id.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        (vec![id.clone()], vec![id])
    };
    Some(GuardFrame {
        guard_text,
        guard_group_id: next_group_id(),
        guard_branch: 0,
        guard_kind: "heuristic",
        defines,
        negates: Vec::new(),
        mentions,
        guard_byte_range: node.byte_range(),
    })
}
