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

/// Does the already-left-trimmed `line` open with the directive `marker`?
///
/// The word boundary after the marker is what stops `#if` matching an `#ifdef`
/// line. A directive written with space between the sigil and the word
/// (`#  ifdef`) is not recognised; the scan then leaves that region to the
/// grammar's own span rather than guessing at it.
fn starts_with_directive(line: &str, marker: &str) -> bool {
    line.strip_prefix(marker)
        .is_some_and(|rest| !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))
}

/// Byte span of the conditional region `node` opens, taken from the directives
/// themselves rather than from where the grammar decided the node stops.
///
/// tree-sitter consumes block items until it finds a closing directive, so a
/// construct that swallows the real one runs the node on to the *next* close.
/// The C idiom `#ifdef __cplusplus` / `extern "C" {` / `#endif` does exactly
/// that: the brace opens a linkage specification whose body absorbs the
/// `#endif`, and the node then spans two unrelated groups.
///
/// The node therefore over-extends, but it never stops short — which is what
/// lets its own end both bound the scan and stand in as the answer when the
/// directives do not balance. Nothing here repairs a malformed file; it falls
/// back to what the grammar said.
fn region_span(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> std::ops::Range<usize> {
    let span = node.byte_range();
    let openers = config.region_openers();
    let closer = config.region_closer();
    if openers.is_empty() || closer.is_empty() {
        return span;
    }
    let Ok(text) = std::str::from_utf8(&source[span.clone()]) else {
        return span;
    };
    match scan_region_len(text, openers, closer) {
        Some(len) => span.start..span.start + len,
        None => span,
    }
}

/// Length of the balanced region starting at the first line of `text`, or
/// `None` when the directives never balance.
///
/// Split out from [`region_span`] so the walk can be exercised without a parse
/// tree: everything that can go wrong here is about counting lines.
fn scan_region_len(text: &str, openers: &[String], closer: &str) -> Option<usize> {
    let mut depth: usize = 0;
    let mut offset: usize = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if openers.iter().any(|m| starts_with_directive(trimmed, m)) {
            depth += 1;
        } else if starts_with_directive(trimmed, closer) {
            if depth == 0 {
                // A close with nothing open. The opener was not recognised — a
                // directive shape this scan does not read, or a kind the language
                // does not declare — so the region cannot be balanced from here.
                // Bailing hands the caller back the parser's span; ending the
                // region on this line instead would cut it short and strip the
                // guard from rows that are inside it.
                return None;
            }
            depth -= 1;
            if depth == 0 {
                return Some(offset + line.len());
            }
        }
        offset += line.len();
    }
    None
}
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
    // Only a group opener owns a closing directive, so only it can have its end
    // derived from one. An arm begins with `#elif`/`#else`, which is not an
    // opener, so scanning from there would start at depth 0 and let the first
    // balanced region *inside* the arm close it — ending the arm at that inner
    // directive and handing every row after it the group's positive condition
    // instead of the negated arm. Arms keep the grammar's span, unchanged.
    let region = if config.is_elif_kind(kind) || config.is_else_kind(kind) {
        node.byte_range()
    } else {
        region_span(node, source, config)
    };

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
            guard_byte_range: region,
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
            guard_byte_range: region,
        }
    }
}

/// Maintain a mini guard stack in lock-step with a tree walk.
///
/// Pops frames whose byte scope the node has left, then pushes one when the
/// node opens a guard.  Shared by the dead-store, shadow, and indexing walks.
pub fn update_guard_stack(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    stack: &mut Vec<GuardFrame>,
) {
    if !config.has_guard_support() {
        return;
    }
    let kind = node.kind();
    while let Some(top) = stack.last() {
        if node.start_byte() >= top.guard_byte_range.end {
            drop(stack.pop());
        } else {
            break;
        }
    }
    if config.is_block_guard_kind(kind) || config.is_elif_kind(kind) || config.is_else_kind(kind) {
        let frame = build_guard_frame(node, source, config, stack);
        stack.push(frame);
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
    decorator_kind: &str,
) -> Vec<GuardFrame> {
    let mut frames = Vec::new();
    if decorator_kind.is_empty() {
        return frames;
    }
    let mut cursor = node.prev_named_sibling();
    while let Some(sib) = cursor {
        if sib.kind() != decorator_kind {
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

#[cfg(test)]
mod tests {
    use super::*;

    // -- strip_cfg_wrapper -----------------------------------------------

    #[test]
    fn strip_cfg_wrapper_feature() {
        // cfg(feature = "alloc") → feature = "alloc"
        let res = strip_cfg_wrapper(r#"cfg(feature = "alloc")"#, "cfg");
        assert_eq!(res, Some(r#"feature = "alloc""#));
    }

    #[test]
    fn strip_cfg_wrapper_all() {
        let res = strip_cfg_wrapper("all(unix, target_os = \"linux\")", "all");
        assert_eq!(res, Some("unix, target_os = \"linux\""));
    }

    #[test]
    fn strip_cfg_wrapper_no_match() {
        let res = strip_cfg_wrapper("not(feature = \"x\")", "cfg");
        assert!(res.is_none());
    }

    #[test]
    fn strip_cfg_wrapper_missing_paren() {
        let res = strip_cfg_wrapper("cfg feature", "cfg");
        assert!(res.is_none());
    }

    // -- cfg_extract_key ------------------------------------------------

    #[test]
    fn cfg_extract_key_simple_key_only() {
        assert_eq!(cfg_extract_key("unix"), "unix");
    }

    #[test]
    fn cfg_extract_key_key_value_pair() {
        assert_eq!(cfg_extract_key("feature = \"alloc\""), "feature");
    }

    #[test]
    fn cfg_extract_key_with_underscores() {
        assert_eq!(cfg_extract_key("target_os = \"linux\""), "target_os");
    }

    #[test]
    fn cfg_extract_key_empty_is_empty() {
        assert_eq!(cfg_extract_key(""), "");
    }

    #[test]
    fn cfg_extract_key_invalid_chars_is_empty() {
        // Keys with invalid characters return empty string.
        assert_eq!(cfg_extract_key("not(unix)"), "");
    }

    // -- cfg_simple_ident -----------------------------------------------

    #[test]
    fn cfg_simple_ident_bare_word() {
        assert_eq!(cfg_simple_ident("unix"), "unix");
    }

    #[test]
    fn cfg_simple_ident_with_value_is_empty() {
        // Has '=' → not a simple ident.
        assert_eq!(cfg_simple_ident("feature = \"x\""), "");
    }

    #[test]
    fn cfg_simple_ident_empty_is_empty() {
        assert_eq!(cfg_simple_ident(""), "");
    }

    #[test]
    fn cfg_simple_ident_with_parens_is_empty() {
        assert_eq!(cfg_simple_ident("all(unix)"), "");
    }

    #[test]
    fn cfg_simple_ident_trims_whitespace() {
        assert_eq!(cfg_simple_ident("  unix  "), "unix");
    }

    // -- split_cfg_top_level --------------------------------------------

    #[test]
    fn split_cfg_top_level_single_element() {
        let parts = split_cfg_top_level("unix");
        assert_eq!(parts, vec!["unix"]);
    }

    #[test]
    fn split_cfg_top_level_two_elements() {
        let parts = split_cfg_top_level("unix, windows");
        assert_eq!(parts, vec!["unix", " windows"]);
    }

    #[test]
    fn split_cfg_top_level_nested_parens_skip_inner_comma() {
        // "all(a, b), unix" — the comma inside all(...) must not split.
        let parts = split_cfg_top_level("all(a, b), unix");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "all(a, b)");
        assert_eq!(parts[1].trim(), "unix");
    }

    #[test]
    fn split_cfg_top_level_empty_string() {
        let parts = split_cfg_top_level("");
        assert_eq!(parts, vec![""]);
    }

    // -- parse_condition_text ---------------------------------------------

    #[test]
    fn parse_condition_splits_defined_from_negated() {
        let (defines, negates, mentions) = parse_condition_text("defined(A) && !defined(B)");
        assert_eq!(defines, ["A"]);
        assert_eq!(negates, ["B"]);
        assert_eq!(mentions, ["A", "B"]);
    }

    #[test]
    fn parse_condition_withholds_positives_under_a_top_level_or() {
        // Under a top-level `||` no single identifier is required, so positive
        // terms are deliberately kept out of `defines`. `mentions` still lists
        // them, which is why guard_mentions is the field to filter on when a
        // condition is disjunctive.
        let (defines, negates, mentions) = parse_condition_text("defined(A) || defined(B)");
        assert!(defines.is_empty());
        assert!(negates.is_empty());
        assert_eq!(mentions, ["A", "B"]);
    }

    #[test]
    fn parse_condition_ignores_defined_without_parentheses() {
        let (defines, negates, mentions) = parse_condition_text("defined A && B");
        assert!(defines.is_empty());
        assert!(negates.is_empty());
        assert!(mentions.is_empty());
    }

    #[test]
    fn parse_condition_of_a_bare_macro_name_finds_nothing() {
        // `#ifdef X` never reaches this function: its identifier comes from the
        // directive's name field, not from condition parsing.
        let (defines, _, mentions) = parse_condition_text("X");
        assert!(defines.is_empty());
        assert!(mentions.is_empty());
    }

    // -- negate_frame ------------------------------------------------------

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn test_frame(text: &str, defines: &[&str], negates: &[&str], mentions: &[&str]) -> GuardFrame {
        GuardFrame {
            guard_text: text.to_string(),
            guard_group_id: 1,
            guard_branch: 0,
            guard_kind: "preprocessor",
            defines: strs(defines),
            negates: strs(negates),
            mentions: strs(mentions),
            guard_byte_range: 0..0,
        }
    }

    #[test]
    fn negate_of_a_bare_ifdef_is_not_parenthesised() {
        // `#ifdef X` / `#else` must read `!X`, never `!(X)`.
        let (text, defines, negates, mentions) =
            negate_frame(&test_frame("X", &["X"], &[], &["X"]));
        assert_eq!(text, "!X");
        assert!(defines.is_empty());
        assert_eq!(negates, ["X"]);
        assert_eq!(mentions, ["X"]);
    }

    #[test]
    fn negate_wraps_a_compound_condition_and_swaps_the_lists() {
        let (text, defines, negates, _) = negate_frame(&test_frame(
            "defined(A) && !defined(B)",
            &["A"],
            &["B"],
            &["A", "B"],
        ));
        assert_eq!(text, "!(defined(A) && !defined(B))");
        assert_eq!(defines, ["B"]);
        assert_eq!(negates, ["A"]);
    }

    #[test]
    fn negate_of_an_already_negated_frame_keeps_its_parentheses() {
        let (text, ..) = negate_frame(&test_frame("!(A || B)", &[], &[], &[]));
        assert_eq!(text, "!(!(A || B))");
    }

    #[test]
    fn negate_of_a_disjunction_does_not_decompose_today() {
        // Characterisation, not endorsement: !(A || B) is equivalent to
        // !A && !B, so both identifiers belong in the negated frame's
        // `negates`. They are absent because a disjunction contributes no
        // `defines` to swap. When that is restored this assertion flips, and
        // the matching end-to-end case is promoted at the same time.
        let (text, _, negates, mentions) = negate_frame(&test_frame(
            "defined(A) || defined(B)",
            &[],
            &[],
            &["A", "B"],
        ));
        assert_eq!(text, "!(defined(A) || defined(B))");
        assert!(negates.is_empty());
        assert_eq!(mentions, ["A", "B"]);
    }

    #[test]
    fn negate_of_an_empty_frame_stays_empty() {
        let (text, defines, negates, mentions) = negate_frame(&test_frame("", &[], &[], &[]));
        assert!(text.is_empty());
        assert!(defines.is_empty());
        assert!(negates.is_empty());
        assert!(mentions.is_empty());
    }

    // -- inject_guard_fields -----------------------------------------------

    #[test]
    fn inject_on_an_empty_stack_writes_nothing() {
        let mut fields: HashMap<String, String> = HashMap::new();
        inject_guard_fields(&[], &mut fields);
        assert!(fields.is_empty());
    }

    #[test]
    fn inject_joins_nested_groups_outermost_first() {
        let mut outer = test_frame("A", &["A"], &[], &["A"]);
        outer.guard_group_id = 7;
        let mut inner = test_frame("B", &["B"], &[], &["B"]);
        inner.guard_group_id = 8;

        let mut fields: HashMap<String, String> = HashMap::new();
        inject_guard_fields(&[outer, inner], &mut fields);

        assert_eq!(fields["guard"], "A && B");
        assert_eq!(fields["guard_defines"], "A,B");
        assert_eq!(fields["guard_group_id"], "8");
        assert_eq!(fields["guard_branch"], "0");
    }

    #[test]
    fn inject_keeps_only_the_innermost_arm_of_one_group() {
        // tree-sitter nests `#elif` arms, so every arm of a chain is on the
        // stack at once; the row must describe the arm it actually sits in.
        let mut if_arm = test_frame("A", &["A"], &[], &["A"]);
        if_arm.guard_group_id = 9;
        let mut elif_arm = test_frame("B", &["B"], &[], &["B"]);
        elif_arm.guard_group_id = 9;
        elif_arm.guard_branch = 1;

        let mut fields: HashMap<String, String> = HashMap::new();
        inject_guard_fields(&[if_arm, elif_arm], &mut fields);

        assert_eq!(fields["guard"], "B");
        assert_eq!(fields["guard_branch"], "1");
        assert_eq!(fields["guard_group_id"], "9");
    }

    // -- are_guards_exclusive ----------------------------------------------

    fn info(group: u64, branch: u8, kind: &'static str) -> GuardInfo {
        GuardInfo {
            guard_group_id: group,
            guard_branch: branch,
            guard_kind: kind,
        }
    }

    #[test]
    fn same_group_different_branch_is_exclusive() {
        assert!(are_guards_exclusive(
            &info(3, 0, "preprocessor"),
            &info(3, 1, "preprocessor")
        ));
    }

    #[test]
    fn same_group_same_branch_is_not_exclusive() {
        assert!(!are_guards_exclusive(
            &info(3, 1, "preprocessor"),
            &info(3, 1, "preprocessor")
        ));
    }

    #[test]
    fn different_groups_are_not_exclusive() {
        assert!(!are_guards_exclusive(
            &info(3, 0, "preprocessor"),
            &info(4, 1, "preprocessor")
        ));
    }

    #[test]
    fn a_heuristic_guard_is_never_exclusive() {
        // Heuristic frames are inferred, not read off a directive, so they
        // carry no exclusivity claim.
        assert!(!are_guards_exclusive(
            &info(3, 0, "heuristic"),
            &info(3, 1, "preprocessor")
        ));
    }

    // -- scan_region_len ----------------------------------------------------

    fn c_openers() -> Vec<String> {
        vec![
            "#if".to_string(),
            "#ifdef".to_string(),
            "#ifndef".to_string(),
        ]
    }

    #[test]
    fn scan_stops_at_the_matching_close() {
        let text = "#ifdef A\nint x;\n#endif\nint y;\n";
        let len = scan_region_len(text, &c_openers(), "#endif").expect("balanced");
        assert_eq!(&text[..len], "#ifdef A\nint x;\n#endif\n");
    }

    #[test]
    fn scan_counts_nesting_and_ignores_inner_closes() {
        let text = "#if A\n#ifdef B\nint x;\n#endif\n#endif\ntail\n";
        let len = scan_region_len(text, &c_openers(), "#endif").expect("balanced");
        assert_eq!(&text[..len], "#if A\n#ifdef B\nint x;\n#endif\n#endif\n");
    }

    #[test]
    fn scan_ends_the_extern_c_group_at_its_own_endif() {
        // The defect this exists for: the grammar runs the node on to the second
        // group's `#endif` because `extern "C" {` swallows the first one.
        let text = concat!(
            "#ifdef __cplusplus\n",
            "extern \"C\" {\n",
            "#endif\n",
            "int api(void);\n",
            "#ifdef __cplusplus\n",
            "}\n",
            "#endif\n"
        );
        let len = scan_region_len(text, &c_openers(), "#endif").expect("balanced");
        assert_eq!(&text[..len], "#ifdef __cplusplus\nextern \"C\" {\n#endif\n");
    }

    #[test]
    fn scan_does_not_confuse_if_with_ifdef() {
        // `#ifdef` starts with `#if`; without a word boundary the opener would
        // be counted twice and the region would never balance.
        let text = "#ifdef A\n#endif\n";
        let len = scan_region_len(text, &["#if".to_string()], "#endif");
        assert_eq!(len, None, "#if must not match an #ifdef line");
    }

    #[test]
    fn scan_tolerates_indentation_and_trailing_comments() {
        let text = "  #ifdef A\n\tint x;\n  #endif /* A */\n";
        let len = scan_region_len(text, &c_openers(), "#endif").expect("balanced");
        assert_eq!(len, text.len());
    }

    #[test]
    fn scan_gives_up_on_an_unbalanced_region() {
        // No close at all, and a nested region that closes but does not settle
        // the outer one. Both must return None so the caller keeps the
        // grammar's span rather than inventing an end.
        assert_eq!(
            scan_region_len("#ifdef A\nint x;\n", &c_openers(), "#endif"),
            None
        );
        assert_eq!(
            scan_region_len("#if A\n#ifdef B\n#endif\n", &c_openers(), "#endif"),
            None
        );
    }

    #[test]
    fn scan_handles_a_region_with_no_trailing_newline() {
        let text = "#ifdef A\n#endif";
        let len = scan_region_len(text, &c_openers(), "#endif").expect("balanced");
        assert_eq!(len, text.len());
    }

    #[test]
    fn a_spaced_directive_is_not_recognised() {
        // Documented limit: `#  ifdef` is a valid directive that this scan does
        // not see. It yields None rather than a wrong end, so the caller falls
        // back to the grammar's span.
        assert_eq!(
            scan_region_len("#  ifdef A\n#  endif\n", &c_openers(), "#endif"),
            None
        );
    }

    #[test]
    fn scanning_from_a_non_opener_closes_on_the_first_nested_region() {
        // Why `build_guard_frame` must not derive an end for `#elif`/`#else`
        // arms. An arm's first line is not an opener, so the walk starts at
        // depth 0 and the first *balanced* region inside the arm takes depth
        // 1 -> 0 and ends it there — far short of the arm's real extent.
        //
        // The consequence is worse than a short range: the arm frame pops early,
        // dedup then keeps the group's `#if` frame, and every row after the
        // nested region reports the positive condition instead of the negated
        // arm. Arms therefore keep the grammar's span.
        let text = "#else\n#ifdef B\nint b;\n#endif\nint c;\n#endif\n";
        let len = scan_region_len(text, &c_openers(), "#endif").expect("closes early");
        assert_eq!(&text[..len], "#else\n#ifdef B\nint b;\n#endif\n");
    }
}
