//! Columnar storage engine for `ForgeQL` — Phase 03+.
//!
//! This module implements the **write side** of the columnar storage format.
//! It is enabled automatically when a `.forgeql.yaml` is present for the source.
//!
//! # Architecture
//!
//! - [`SegmentBuilder`]: assembles and flushes one segment directory from a
//!   slice of `IndexRow`s that all belong to the same source file.
//! - [`ShadowWriter`]: iterates over a fully-built [`SymbolTable`] and drives
//!   one [`SegmentBuilder`] per source file.
//!
//! # On-disk layout
//!
//! ```text
//! <bare-repo>/forgeql/segments/<provider_id>/<content_id_hex>/
//! ├── header.bin            # 80-byte preamble + column entries
//! ├── col_name_id.bin       # [u32; row_count]
//! ├── col_fql_kind_id.bin   # [u32; row_count]
//! ├── col_line.bin          # [u32; row_count]
//! ├── col_byte_start.bin    # [u32; row_count]
//! ├── col_byte_end.bin      # [u32; row_count]
//! ├── col_usages_count.bin  # [u32; row_count]
//! ├── col_language_id.bin   # [u32; row_count]
//! ├── strings_offsets.bin   # [u32; string_count + 1]
//! ├── strings_data.bin      # UTF-8 bytes, concatenated
//! ├── postings_fql_kind.bin # (kind_id: u32, len: u32, bytes)* per kind
//! ├── name.fst              # fst::Map — name → packed (count | byte_offset<<32)
//! └── name_postings.bin     # flat [u32] row IDs referenced by name.fst
//! ```
//!
//! [`SymbolTable`]: crate::ast::index::SymbolTable

pub mod build_context;
pub mod columnar_storage;
pub mod delta_file;
pub mod dirty_overlay;
pub mod gc;
pub mod manifest;
pub mod overlay;
pub mod overlay_builder;
pub mod overlay_lock;
pub mod overlay_writer;
pub mod segment_builder;
pub mod segment_reader;
pub mod shadow_writer;

pub use build_context::BuildInput;
pub use build_context::ColumnarBuildContext;
pub use columnar_storage::ColumnarStorage;
pub use delta_file::{DeltaFile, StagedEntry};
pub use dirty_overlay::DirtyOverlay;
pub use manifest::Manifest;
pub use overlay_builder::OverlayBuilder;
pub use segment_builder::{
    POSTING_ENRICHMENT_FIELDS, SegmentBuilder, SymbolRow, ZONEMAP_NUMERIC_FIELDS, overlay_budget,
    posting_budget,
};
pub use segment_reader::SegmentReader;
pub use shadow_writer::ShadowWriter;

/// Type-erased, thread-safe hash function for content addressing.
///
/// Wrap a `SourceProvider::hash_content` call behind this type to keep
/// `ShadowWriter` decoupled from the concrete provider type.
/// Example: `Arc::new(|b: &[u8]| git_blob_sha1(b).to_vec())`
pub type HashFn = std::sync::Arc<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync + 'static>;

/// Enrichment logic version — embedded in the segment, overlay, and manifest
/// storage paths.
///
/// **Bump this constant on every enrichment logic change** (new enricher, bug
/// fix, field rename).  The new version namespace is created automatically on
/// the next `USE`; the old one is orphaned and will be removed by the GC sprint.
///
/// History:
///   1 — initial columnar engine (v0.49.0)
///   2 — `condition_tests` clause counting fix (v0.49.1)
///   3 — `has_fallthrough` annotation suppression (v0.49.3)
///   4 — `lines` clipping for absorbed function_definition (v0.49.10, partial)
///   5 — `lines` clipping extended: preproc_ifdef + ERROR/DEVICE_API (v0.49.10)
///   6 — columnar overlay split into per-blob `.bin` files (v0.49.4)
///   7 — `is_magic` semantics fixed; numbers in string literals excluded (v0.50.2)
///   8 — FQOV v3: TOC-based binary overlay replaces bincode serialization (v0.50.11)
///   9 — `POSTING_ENRICHMENT_FIELDS` expansion: string-enum and boolean enrichment
///       fields now stored as per-field posting blobs in segments, enabling fast
///       WHERE/ORDER BY without full row materialization (v0.50.12)
///  10 — stable `node_id` handles introduced via persisted ordinals
///  11 — ordinal remapper improvements for reindex stability
///  12 — Phase A node_id policy gate: only addressable `fql_kind`s receive ordinals
///  13 — B-prep: col_parent_ordinal, col_rev, col_first/next/prev_sibling_ordinal as
///          typed columns; parent_ordinal promoted from enrichment string to u32 field
///  14 — branches-as-parents: control-flow nodes (if/while/for/switch/do) become
///          parents of their body statements, so node_ids nest under the branch
///          rather than the enclosing function (plan §4.1)
///  15 — block grouping: synthetic childless block nodes (e.g. `comment_block`)
///          span a run of same-kind sibling members, sharing the parent of the
///          members instead of nesting under it (Stage 1: comments)
///  16 — block grouping Stage 2: block members carry `block_ord`/`block_off`
///          fields so FIND/SHOW surface them as `block_id(offset)`
///  17 — block grouping: block rows carry a `content_hash` field so the reindex
///          remapper can keep block node ids stable across sibling-block edits
///  18 — block grouping: clamp a block member offset to its last content line
///          so a one-line doc/block comment surfaces as a single offset
///  19 — has_doc skips leading attribute/decorator siblings, so a documented
///          item with an interposed `#[...]` attribute is still detected as documented
///  20 — comment_block rows carry a `block_label` field (first-member snippet +
///          member count) for SHOW outline display; identity name stays `comment_block`
///  21 — CMake/Make control-flow rows: `control_flow` config sections added, and
///          control-flow rows from grammars without a `condition` field are named
///          by the construct's first line (previously nameless → unfindable).
///          v20 segments for .cmake/Makefile files lack these rows entirely —
///          this is the constant to bump when a change alters WHICH ROWS a file
///          produces (segments cache per blob under `{provider}-v{ENRICH_VER}/`;
///          the overlay SCHEMA_VERSION alone does not re-index cached segments).
///  22 — BUG-019: C and Rust shift expressions now resolve to
///          `fql_kind = "shift_expression"` (config-only: `shift_kinds` +
///          `kind_map` entries mirrored from cpp.json); v21 segments carry
///          those rows with an empty fql_kind.
///  23 — segments gain `usages_fst` / `usages_postings` blobs
///          (identifier text → 1-based source lines, the reference index).
///          v22 segments lack the blobs, so readers would silently report
///          zero usages — the bump forces a full re-index.
///  24 — AUTOSAR ECUC parameter/reference values (XML family) are now named
///          by their DEFINITION-REF's last path segment instead of the bare
///          tag name; v23 segments carry those rows named
///          "ECUC-NUMERICAL-PARAM-VALUE" etc., unfindable by parameter name.
///  25 — C and C++ `union` types, `typedef` aliases (scalar, function-pointer,
///          and the anonymous `typedef struct/enum { … } Name;` forms), and
///          enum constants are now indexed as `union` / `type_alias` /
///          `enumerator` rows with node ids; v24 segments lack those rows, so
///          the bump forces a full re-index.
///  26 — C and C++ struct/class/union/enum *references* and forward
///          declarations (`struct Foo *p;`, `struct Foo;`) are no longer
///          indexed as type symbols — only the definition (which carries a
///          body) is. This lets `SHOW members` and type resolution reach the
///          definition instead of a bodyless reference; v25 segments carry
///          the spurious reference rows.
///  27, 28 — Consumed during development of 29 and never released. Dev caches
///          at those versions hold drafts of the changes below and must not be
///          trusted. (Bumping on EVERY iteration of an indexing change is not
///          optional: a v(N) cache built from an earlier draft of your own
///          change is exactly as stale as a v(N-1) cache, and reusing it makes
///          the test suite pass against code that never ran.)
///  29 — Four changes to index output, none of which v26 segments have:
///          (a) JSON/YAML containers with no identifier member are now named by
///              their key-set skeleton, and arrays by their nearest ancestor
///              pair's key — v26 segments emit no row at all for those nodes,
///              so their children are reparented onto the wrong ancestor;
///          (b) a run of 8+ adjacent JSON `array` siblings now emits an
///              `array_block` row — v26 segments lack it, leaving a keyless
///              JSON document with zero addressable rows;
///          (c) block runs are scanned over *named* siblings, so members
///              separated by anonymous punctuation (JSON's `,`) group at all —
///              before this, a 201-element array scanned as a run of ONE and no
///              block was ever emitted;
///          (d) tree-sitter `ERROR` regions now emit `error` rows, so a broken
///              file is no longer silently, partially indexed. Zero-width
///              `MISSING` tokens are deliberately NOT emitted: a row spanning
///              no bytes could be seen but not read or repaired.
///   30 — BURNED. Consumed mid-session by an abandoned draft that made `error`
///        addressable (which shifts ordinals). That draft's run wrote v30
///        segments; the code was then reverted, but the segments survive. Any
///        later change reusing 30 silently reads those poisoned ordinals — it
///        cost a full gate run to find. A version is spent the moment ANY build
///        writes segments under it, released or not.
///   31 — `error` rows carry `error_scope` (`root` / `file` / `nested`) and
///        `error_bytes`. A raw tree-sitter ERROR is a terrible danger signal:
///        tree-sitter parses C without running the preprocessor, so `static
///        ALWAYS_INLINE void f(void)` errors on the return type while `f` still
///        indexes correctly. Zephyr has ~74k such regions and essentially none
///        is damage. Position + size let `parse_coverage` separate a healthy
///        macro-heavy header (~1.0) from a file whose extension lies (~0).
///        `error` remains absent from `is_addressable_fql_kind`, so this adds
///        FIELDS only — no ordinals are consumed and no node_id moves.
///   32 — `error` rows are ADDRESSABLE. Emitted since 29, they were never in
///        `is_addressable_fql_kind`, so every broken region came back with an
///        empty `node_id`: findable, never repairable by handle — half of what
///        the kind exists for. This CONSUMES ordinals, so node_ids shift in
///        every file holding an error. Landing it required first porting every
///        hardcoded node_id pin out of `tests/golden.json` into the v2 suite,
///        which captures handles at run time (`tests/golden/node_addressing.json`).
///   33 — `condition_text` KEEPS an assignment `=` inside a condition and folds
///        the assigned value to one operand, so `if ((x = a + b) > 0)` reads
///        `((a=b)>c)`, not the old `((a)>b)`. v32 segments hold the pre-fix
///        skeleton that drops the `=` and contradicts `has_assignment_in_condition`.
///   34 — a node-removal verb (`DELETE NODE` / `MOVE NODE` away) now tombstones
///        the removed root ordinal in the reindex remapper, so a byte-identical
///        surviving sibling keeps its own ordinal instead of adopting the
///        deleted node's. A v33 segment built during such a reindex holds the
///        pre-fix (re-keyed) ordinals for that post-removal content.
///   35 — a segment is keyed by **(path, content)**, not by content alone: the
///        filename gained a source-path component. The indexing result depends on
///        the file bytes *and* on which parser the path selects, so two
///        byte-identical files with different extensions must not share a segment.
///        v34 segment/overlay trees are laid out under the old content-only names
///        and cannot be resolved by the new key.
///   36 — a guard region ends where its own closing directive is, not where the
///        grammar stopped the guard node. A construct that swallows the closing
///        directive used to run the region on to the next one, so v35 segments
///        carry a conjunct on rows that were never inside the region.
///   37 — that end is derived for group openers only. An `#elif`/`#else` arm does
///        not begin with an opening directive, so deriving its end let the first
///        balanced region inside the arm close it early; v36 segments can report
///        a group's positive condition on rows that belong to a negated arm.
///   38 — an arm carries the negations of the arms before it, a conjunct holding
///        a top-level `||` is parenthesised before joining, negating a pure
///        disjunction decomposes by De Morgan, and a condition's whitespace is
///        normalised. Every guard string in a chain, and every `guard_negates`
///        derived from one, differs from v37. **Do not trust a v38 generation:**
///        the fold read each earlier arm's *accumulated* terms rather than its
///        own, so a long `#elif` chain doubled its lists per arm — wrong values
///        on chain-heavy files, and enough memory to be killed part-way through.
///   39 — that fold reads each arm's own terms, and the accumulated lists are
///        deduped. Sizes are bounded by a chain's distinct identifiers instead
///        of by its arm count.
///   40 — a conjunct is bracketed only where composition can reassociate it. v39
///        double-bracketed a negated disjunction and bracketed a lone condition
///        that no `&&` was joining, so its guard strings carry parentheses the
///        source never had.
///   41 — a leading `!` is stripped only when it governs the whole condition. In
///        v40 an arm whose condition began `!defined(X) && …` negated to
///        `defined(X) && …`, asserting both operands — the opposite of that arm
///        being false — and every later arm in the chain inherited it.
///   42 — a preprocessor guard reaches every row inside its region, not only the
///        declaration-like ones. Expression and control-flow rows previously
///        carried none, so pairing a guard with `is_magic`, `has_catch_all` or a
///        control-flow field matched nothing; v41 segments hold no guard columns
///        for those kinds, and their ordinal keys carry no guard component.
///   43 — no change to what a row holds. v42 was cut before the guard fields
///        were hoisted out of the per-extra-row loop, so this generation exists
///        to make the corpus prove the hoist emits the same rows rather than
///        answering from segments the previous draft wrote.
///   44 — `guard_group_id` is derived, not counted. It was a process-global
///        counter, so a cached segment kept the previous run's numbering while
///        freshly indexed files restarted at 1: the same group answered to two
///        numbers, and the re-index ordinal key that compares them stopped
///        matching after any restart. It is now a hash of the repo-relative
///        path and the opening directive's byte offset, so every generation of
///        the same content agrees. Every stored value changes.
///   45 — the guard kind joins the group-ID hash. One node can open both a
///        block-guard frame and an env-guard frame — `update_guard_stack`
///        pushes them in two independent branches — and both mint from that
///        node's own byte offset. Under v44 the two frames shared an identity
///        with `guard_branch` 0 on each, so the exclusivity test would have
///        read two unrelated guards as opposite arms of one block. No shipped
///        config declares a kind as both, so this is reachable by config
///        rather than live today.
///   46 — `#ifdef` / `#if` / `#elif` become addressable `guard` rows. The
///        `#ifdef` rows already existed with an empty kind; they now carry
///        `fql_kind = "guard"` and, being addressable, consume an ordinal.
///        `#if` / `#elif` emitted no row at all and now emit one named by the
///        normalised condition. Both change stored rows and shift sibling
///        ordinals in every C/C++ file holding a directive.
///   47 — comments contribute occurrence sites. Every identifier token written
///        inside a node kind listed in a language's `mention_text_kinds` is
///        stored in a new `mentions_<role>_fst` / `_postings` blob pair, keyed
///        by the role that kind carries. No existing blob moves and no row
///        changes, but a segment written before this generation holds no
///        mention postings at all, so `FIND usages` would silently under-report
///        comment sites when read from one.
///   48 — string literals join the mention layer. The C/C++/Rust/Python
///        configs add their string kinds to `mention_text_kinds` under role
///        `string`, so every file holding a string emits a new
///        `mentions_string_*` blob pair. Purely additive — no existing blob,
///        row or ordinal changes — but a v47 segment carries no string
///        postings, so reading one would under-report `role = 'string'`.
///   49 — C++ raw string literals join the string role. `cpp.json` modelled
///        only `string_literal`, so `R"(...)"` contents emitted nothing while
///        the role was advertised for C++. Adds postings, changes nothing else.
///   50 — probe generation: CMake `mention_text_kinds` carried throwaway roles
///        while the grammar's argument node kinds were identified live. No
///        release ever served it.
///   51 — CMake call arguments become `role = 'config'`. `cmake.json` maps the
///        `argument` wrapper, which covers unquoted, quoted and bracket forms
///        in one entry and structurally excludes `line_comment` — a comment is
///        a sibling inside `argument_list`, never a child of `argument`, so
///        mapping the list instead would have mis-tagged comments as config.
///        Every CMake file gains a `mentions_config_*` blob pair.
///   52 — probe generation: Markdown and reStructuredText prose kinds carried
///        throwaway roles while the block containers were identified. Never
///        served: the key was written at the wrong nesting level and the
///        deserializer ignored it, so the generation emitted nothing.
///   53 — probe generation: same mapping, still mis-placed. Retired once
///        `mention_text_kinds` was moved inside the `syntax` block, where the
///        other languages declare it.
///   54 — superseded: the same mapping also declared `setext_heading`, which
///        holds its text in a child `paragraph` and therefore double-counted
///        every token in a setext heading. Never released.
///   55 — Markdown and reStructuredText prose becomes `role = 'doc'`.
///        `md.json` maps `paragraph` and `atx_heading`; `setext_heading` is
///        excluded because it holds its text in a child `paragraph`, while an
///        atx heading holds an `inline` node and cannot overlap one.
///        `rst.json` maps `paragraph` and `title`. Container kinds that
///        *contain* paragraphs — list items, block quotes, directives, fields,
///        sections — are deliberately unmapped: their prose is already covered
///        by the paragraph inside them, and mapping both would emit each token
///        twice. Every Markdown and reStructuredText file gains a
///        `mentions_doc_*` blob pair.
///   56 — YAML, TOML and JSON scalar *values* become `role = 'config'`.
///        The keys are deliberately excluded: they are already the pair rows'
///        names on the symbols side, so tagging them too would answer "where
///        is this name written?" twice for the same byte range. YAML and JSON
///        distinguish key from value by tree-sitter *field label*, so their
///        rules carry `when_field: "value"` and the walk tracks the nearest
///        labelled edge — a key nested inside a value resets it. TOML labels
///        nothing, but its keys are `bare_key`/`quoted_key`/`dotted_key` while
///        its values are `string`/`integer`/…, so a plain kind rule suffices.
///        Only leaf scalars are mapped; mapping a wrapper as well would emit
///        each token twice. All three also set `mention_token_extra_chars` to
///        `-`, so `ubuntu-latest` stays one token instead of two.
///   57 — a C/C++ include path becomes a usage site under its own name. The
///        path is recorded as ONE token holding the whole text, trimmed of
///        the characters outside `[A-Za-z0-9_]` that delimit it, because
///        `identifier_tokens` would otherwise shred `zephyr/pm/device.h` into
///        `zephyr`, `pm`, `device`, `h` — and the queried path is a substring
///        of none of them. Only the angle-bracket form is claimed: the quoted
///        form's node kind is `string_literal`, shared with every other string
///        in the language, and one kind carries one rule.
///   58 — Kconfig files are indexed. `config X` / `menuconfig X` become
///        `variable` rows named `X` — the definition site of a build flag,
///        which had no addressable row at all before — and every `symbol` is a
///        usage site, so `depends on X`, `select X` and `if X` all answer
///        `FIND usages OF 'X'`. Only those two kinds are given a name-child
///        rule, and a row is emitted only for a named node, so nothing else in
///        a Kconfig file becomes a row. `if` is deliberately left out: naming
///        the guard after its bare `symbol` condition would report a flag's own
///        name twice in the file that defines it, and `if` nests the entries it
///        guards, so it would re-parent them.
///   59 — a Kconfig `config` entry is a `macro`, not a `variable`. A flag
///        becomes a preprocessor macro in the generated header, so `macro` is
///        what it is; `variable` also made flags masquerade as C variables in
///        every kind-filtered query, and left them out of a default
///        `SHOW outline`, which lists `macro` but excludes `variable` so that C
///        locals do not flood it. Same rows, different bucket: the corpus total
///        is unchanged and the counts move from one kind to the other.
///   60 — YAML and JSON comments are named by their own raw text and therefore
///        emit `comment` rows. They emitted none before: the kind map named the
///        kind, but the shared structured-text naming ladder returned no name,
///        and a row is emitted only for a named node. The new rows are purely
///        additive — a comment is a leaf, so nothing re-parents — but they
///        shift the ordinals of every node following them in the same file, so
///        a v59 segment both misses rows and mis-keys handles. YAML also
///        records comment-role mentions now, which a v59 segment cannot carry.
///   61 — YAML declares a block group, so a run of 2+ adjacent comments is
///        surfaced as one addressable `comment_block` sibling. The block is a
///        new addressable row, so it consumes an ordinal and shifts every
///        following node in the file — the same class of change as v60, and a
///        v60 segment mis-keys handles for exactly that reason. Members keep
///        their own rows and gain a block address; nothing is hidden.
///   62 — rows in a config file carry `key_path`, the dotted chain of enclosing
///        `pair` keys, with the row's own key appended when it is itself a
///        pair. A new stored column, so a v61 segment cannot answer a
///        `WHERE key_path` filter at all — the field resolves only when some
///        indexed row carries it. No row moves and no ordinal shifts: this
///        adds a column, not a node.
///   63 — C, C++, Python and Rust declare sibling-run block groups. C/C++: a
///        run of 2+ adjacent comments (split by `comment_style`, so a `/*`
///        paragraph never merges with a `//` one), `#include` runs
///        (`include_block`), `#define` runs (`macro_block`), and for C++ a
///        typedef/using run (`type_alias_block`). Python: comment runs and
///        import runs (`import_block`). Rust: `use` runs join the existing
///        comment group. Every block is a new addressable row, so it consumes
///        an ordinal and shifts every following node in its file — a v62
///        segment mis-keys handles wherever a run exists, which in C corpora
///        is nearly every file.
///   64 — the guard set fields (`guard_defines`, `guard_mentions`,
///        `guard_negates`), `guard_group_id` and `key_path` are written to
///        each segment's enrichment posting index. They were previously
///        excluded from it: their distinct-value counts (9k–46k corpus-wide,
///        measured on a 3,062,139-symbol corpus) blew the single global budget of 8
///        values per file, which exists for the handful-of-values enums. That
///        budget is now per field. A v63 segment carries no postings blob for
///        any of the five, so on one every query on them is a full scan —
///        which is why this is a version bump and not a query-side change.
pub const ENRICH_VER: u32 = 64;

/// The filename used for the columnar delta file in the repository root.
pub const DELTA_FILE_NAME: &str = ".forgeql-columnar-delta";

/// The folder name used for columnar staging segments.
pub const STAGING_DIR_NAME: &str = ".forgeql-staging";

/// Stable key for a source path, used to name a segment.
///
/// Derived from the path **relative to the worktree root**: the segment store is
/// shared by every worktree of a bare repo, so an absolute path would key the
/// same file differently per worktree and defeat reuse.
pub(crate) fn path_key(source_path: &std::path::Path) -> String {
    crate::node_id::hex_prefix(
        &crate::node_id::sha256_of_path(&source_path.to_string_lossy()),
        12,
    )
}

/// Reduce a source path to the worktree-relative form that keys a segment.
///
/// The segment store is shared by every worktree of a bare repo, so a key must
/// never embed an absolute path: the same file would key differently per
/// worktree and every worktree would re-index the whole tree.
///
/// Callers pass either an absolute path inside the worktree or a path already
/// reduced to the relative form — the latter is returned unchanged, since it is
/// already the key. Only an **absolute** path outside the worktree has no sound
/// key, and that is a wiring mistake, so it is reported rather than hidden.
pub(crate) fn segment_source_rel<'a>(
    source_path: &'a std::path::Path,
    worktree_root: &std::path::Path,
) -> &'a std::path::Path {
    if let Ok(rel) = source_path.strip_prefix(worktree_root) {
        return rel;
    }
    if source_path.is_relative() {
        return source_path;
    }
    tracing::warn!(
        path = %source_path.display(),
        root = %worktree_root.display(),
        "segment key: absolute source path is not under the worktree root; \
         keying by it defeats reuse across worktrees"
    );
    debug_assert!(false, "absolute source path is not under the worktree root");
    source_path
}
/// Location of a segment within a versioned provider directory, keyed by
/// **(path, content)** rather than by content alone.
///
/// A segment caches an indexing result, and that result is a function of the
/// file's bytes *and* of the parser its path selects. Content alone cannot name
/// it: two byte-identical files with different extensions parse to different
/// trees, and two identical-bytes files can carry different node ordinals.
/// Sharing one segment between them serves one file the other file's data.
///
/// Keeps the git-style 2-char fan-out on the content hash to avoid flat
/// directories, and disambiguates within a shard by `path_key`.
#[must_use]
pub fn segment_rel_path(source_path: &std::path::Path, hex_content_id: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(&hex_content_id[..2]).join(format!(
        "{}-{}.fqsf",
        &hex_content_id[2..],
        path_key(source_path)
    ))
}
/// Encode a byte slice as a lowercase hex string.
pub(crate) fn bytes_to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}
