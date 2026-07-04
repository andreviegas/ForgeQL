/// Language abstraction layer for `ForgeQL`.
///
/// Every supported language implements [`LanguageSupport`] and provides a
/// [`LanguageConfig`] describing its grammar-specific details.  The
/// [`LanguageRegistry`] maps file extensions to language implementations,
/// allowing the indexer and engine to operate language-agnostically.
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------
// FQL Kind constants — universal, language-agnostic symbol categories
// -----------------------------------------------------------------------

// Definition kinds (primary index rows from `extract_name()`)
pub const FQL_FUNCTION: &str = "function";
pub const FQL_CLASS: &str = "class";
pub const FQL_STRUCT: &str = "struct";
pub const FQL_INTERFACE: &str = "interface";
pub const FQL_ENUM: &str = "enum";
pub const FQL_VARIABLE: &str = "variable";
pub const FQL_FIELD: &str = "field";
pub const FQL_COMMENT: &str = "comment";
pub const FQL_IMPORT: &str = "import";
pub const FQL_MACRO: &str = "macro";
pub const FQL_TYPE_ALIAS: &str = "type_alias";
pub const FQL_NAMESPACE: &str = "namespace";

// Expression/literal kinds (produced by enricher `extra_rows()`)
pub const FQL_NUMBER: &str = "number";
pub const FQL_CAST: &str = "cast";
pub const FQL_INCREMENT: &str = "increment";
pub const FQL_COMPOUND_ASSIGN: &str = "compound_assign";
pub const FQL_SHIFT: &str = "shift";

// Control flow kinds (produced by enricher `extra_rows()`)
pub const FQL_IF: &str = "if";
pub const FQL_WHILE: &str = "while";
pub const FQL_FOR: &str = "for";
pub const FQL_SWITCH: &str = "switch";
pub const FQL_DO: &str = "do";

// -----------------------------------------------------------------------
// LanguageConfig — static, data-only description of a language grammar
// -----------------------------------------------------------------------

/// All grammar-specific data that enrichers need to operate
/// language-agnostically.
///
/// Each language crate constructs this via JSON deserialization
/// ([`super::lang_json::LanguageConfigJson::from_json_bytes`]).
/// One block-grouping rule: coalesce a run of adjacent same-kind sibling
/// members into a single synthetic, addressable "block" node spanning the whole
/// run. The block shares the parent of its members and never has children; the
/// member rows are emitted unchanged. Configured per language via the
/// `block_groups` section of the language JSON.
#[derive(Debug, Clone)]
pub struct BlockGroupSpec {
    /// FQL kind of the members to group (e.g. `comment`).
    pub member_fql_kind: String,
    /// FQL kind assigned to the synthetic block node (e.g. `comment_block`).
    pub block_fql_kind: String,
    /// Minimum number of adjacent members before a block is created. A run
    /// shorter than this is left as plain individual members.
    pub min_run: usize,
    /// Optional attribute that splits a run so only members sharing it group
    /// together. Currently supports `comment_style` (so `///` doc runs and `//`
    /// line runs form separate blocks); `None` groups every adjacent member.
    pub split_on_attr: Option<String>,
}
#[expect(
    clippy::struct_excessive_bools,
    reason = "LanguageConfig describes language properties; each bool is semantically distinct"
)]
pub struct LanguageConfig {
    // -- identity --
    /// Root node kind produced by the tree-sitter grammar (e.g.
    /// `"translation_unit"` for C++, `"program"` for TypeScript).
    pub(crate) root_node_kind: String,

    /// Scope resolution separator (e.g. `"::"` for C++, `"."` for most others).
    pub(crate) scope_separator: String,

    // -- node kind sets (raw tree-sitter kinds for enricher internal checks) --
    /// Raw kinds that represent function/method definitions.
    pub(crate) function_raw_kinds: Vec<String>,

    /// Raw kinds that represent type definitions (class, struct, enum, etc.).
    pub(crate) type_raw_kinds: Vec<String>,

    /// Raw kinds that represent any definition (for `has_doc` checks).
    pub(crate) definition_raw_kinds: Vec<String>,

    /// Raw kinds that represent variable/const declarations.
    pub(crate) declaration_raw_kinds: Vec<String>,

    /// Raw kinds that represent member/field declarations.
    pub(crate) field_raw_kinds: Vec<String>,

    /// Raw kind for parameter declarations (e.g. `"parameter_declaration"`).
    pub(crate) parameter_raw_kind: String,

    /// Raw kind for the body of a type (e.g. `"field_declaration_list"`).
    pub(crate) member_body_raw_kind: String,

    /// Raw kinds for members inside a type body.
    pub(crate) member_raw_kinds: Vec<String>,

    /// Raw kinds for owner containers (impl blocks, classes) whose name is
    /// used to build qualified names (e.g. `CachedIndex::save`).
    pub(crate) owner_container_raw_kinds: Vec<String>,

    /// Raw kind for comments.
    pub(crate) comment_raw_kind: String,

    // -- number literals --
    /// Raw kinds that represent number literals.
    pub(crate) number_literal_raw_kinds: Vec<String>,

    /// Digit group separator (e.g. `Some('\'')` for C++, `Some('_')` for Rust).
    pub(crate) digit_separator: Option<char>,

    /// (`suffix_text`, meaning) pairs for number literal suffixes.
    pub(crate) number_suffixes: Vec<(String, String)>,

    // -- control flow --
    /// Raw kinds that represent control-flow statements indexed by the
    /// control-flow enricher.
    pub(crate) control_flow_raw_kinds: Vec<String>,

    /// Raw kinds specifically for switch/match statements.
    pub(crate) switch_raw_kinds: Vec<String>,

    // -- literals --
    /// Null literal values (e.g. `["nullptr", "NULL", "0"]` for C++).
    pub(crate) null_literals: Vec<String>,

    /// Boolean literal values (e.g. `["true", "false"]`).
    pub(crate) boolean_literals: Vec<String>,

    // -- comments --
    /// (prefix, `style_name`) pairs for detecting comment styles.
    /// Checked in order — first match wins.
    pub(crate) doc_comment_prefixes: Vec<(String, String)>,

    // -- modifiers --
    /// (keyword, `field_name`) pairs for modifier detection.
    pub(crate) modifier_map: Vec<(String, String)>,

    /// Raw node kinds that carry modifier/qualifier keywords.
    pub(crate) modifier_node_kinds: Vec<String>,

    /// (keyword, visibility) pairs.
    pub(crate) visibility_keywords: Vec<(String, String)>,

    /// (`raw_kind`, `default_visibility`) pairs — default visibility for
    /// members of each type kind when no explicit access specifier is present.
    pub(crate) visibility_default_by_type: Vec<(String, String)>,

    // -- casts --
    /// (`raw_kind`, `cast_style`, `cast_safety`) triples for cast detection.
    pub(crate) cast_kinds: Vec<(String, String, String)>,

    /// (`keyword`, `cast_style`, `cast_safety`) triples for named-keyword casts
    /// that tree-sitter parses as `call_expression(template_function(identifier))`.
    /// Used for C++ `static_cast<T>()`, `reinterpret_cast<T>()`, etc.
    pub(crate) named_cast_keywords: Vec<(String, String, String)>,

    // -- capabilities --
    /// Whether the language has `goto` statements.
    pub(crate) has_goto: bool,

    /// Whether the language has `++`/`--` operators.
    pub(crate) has_increment_decrement: bool,

    /// Whether the language has implicit truthiness (e.g. `if (ptr)` in C++).
    pub(crate) has_implicit_truthiness: bool,

    /// Whether function parameters and the function body share the same variable
    /// scope (Python-style).  When `true`, `ShadowEnricher` treats params as
    /// part of the function body's own scope rather than an outer scope, avoiding
    /// false positives on simple parameter reassignments inside `if`/`for` blocks.
    pub(crate) params_share_body_scope: bool,

    /// Raw kind for decorator/attribute nodes, if the language has them.
    pub(crate) decorator_raw_kind: Option<String>,

    /// Node kinds whose subtrees should be skipped entirely during indexing
    /// (e.g. `["preproc_else", "preproc_elif"]` in C++).
    pub(crate) skip_node_kinds: Vec<String>,

    /// Identifier node kinds that produce usage sites.
    pub(crate) usage_node_kinds: Vec<String>,

    /// Node kinds that act as statement / expression boundaries.
    pub(crate) statement_boundary_kinds: Vec<String>,

    // -- declarator structure --
    /// Grammar field name for the declarator child of a definition/declaration
    /// node (e.g. `"declarator"` in C++).
    pub(crate) declarator_field_name: String,

    /// Raw kind for a function-type declarator nested inside a declarator tree
    /// (e.g. `"function_declarator"` in C++).
    pub(crate) function_declarator_kind: String,

    // -- declaration distance / data-flow (decl_distance enricher) --
    /// Raw kind for the parameter list container node
    /// (e.g. `"parameter_list"` for C++, `"formal_parameters"` for TS/Java).
    pub(crate) parameter_list_raw_kind: String,

    /// Raw kind for a simple identifier token
    /// (e.g. `"identifier"` — universal for most tree-sitter grammars).
    pub(crate) identifier_raw_kind: String,

    /// Raw kinds for assignment expressions
    /// (e.g. `["assignment_expression"]` for C++/TS/Java, `["assignment"]` for Python).
    pub(crate) assignment_raw_kinds: Vec<String>,

    /// Raw kinds for update/increment expressions (`++x`, `x--`).
    /// Empty slice for languages without increment/decrement operators.
    pub(crate) update_raw_kinds: Vec<String>,

    /// Raw kind for an init-declarator wrapper node
    /// (e.g. `"init_declarator"` for C++).  Empty string if the language
    /// does not have this intermediate node.
    pub(crate) init_declarator_raw_kind: String,

    /// Raw kind for block/compound statement nodes
    /// (e.g. `"compound_statement"` for C++, `"statement_block"` for TS,
    /// `"block"` for Python/Rust).
    pub(crate) block_raw_kind: String,

    // -- scope / branch awareness (shadow + dead-store enrichers) --
    /// Node kinds that create a new variable scope for shadowing purposes.
    /// C++/Rust: `["compound_statement"]` / `["block"]` (every `{}` block).
    /// Python: only function, class, lambda, and comprehension nodes.
    pub(crate) scope_creating_raw_kinds: Vec<String>,

    /// Node kinds that represent conditional branches.
    /// Used to compute branch depth for dead-store and decl-distance enrichers.
    /// Examples: `if_statement`, `else_clause`, `switch_statement`, `case_statement`.
    pub(crate) branch_raw_kinds: Vec<String>,

    /// Node kinds that represent loop constructs.
    /// Loops are treated like branches for depth tracking (a write inside a
    /// loop may be read on the next iteration, so it is not a dead store).
    pub(crate) loop_raw_kinds: Vec<String>,

    /// Node kinds that represent exception handlers (`try`, `catch`, `except`).
    pub(crate) exception_handler_raw_kinds: Vec<String>,

    /// Subset of `declaration_raw_kinds` that are block-scoped.
    /// Empty = all declarations follow block scoping (default for C++, Rust, Python).
    /// For JS/TS: set to `["lexical_declaration"]`; `var` then stays function-scoped.
    pub(crate) block_scoped_declaration_raw_kinds: Vec<String>,
    // -- escape detection (escape enricher) --
    /// Raw kind for return statements
    /// (e.g. `"return_statement"` for C++/Java/TS, `"return_expression"` for Rust).
    /// Empty string if the language has no explicit return statement kind.
    pub(crate) return_statement_raw_kind: String,

    /// Raw kind for the expression node that represents taking-address-of
    /// (e.g. `"pointer_expression"` for C++, `"reference_expression"` for Rust).
    /// Empty string if the language has no address-of operator.
    pub(crate) address_of_expression_raw_kind: String,

    /// The textual operator for address-of (e.g. `"&"` for C/C++/Rust/Go/Zig).
    /// Empty string if the language has no address-of operator — the escape
    /// enricher will short-circuit.
    pub(crate) address_of_operator: String,

    /// Raw kind for array declarators
    /// (e.g. `"array_declarator"` for C++). Empty string if N/A.
    pub(crate) array_declarator_raw_kind: String,

    /// Keywords that mark a local as having static storage duration
    /// (e.g. `["static"]` for C/C++). Empty for languages without this concept.
    pub(crate) static_storage_keywords: Vec<String>,

    // -- fallthrough detection (fallthrough enricher) --
    /// Raw kind for case/default labels inside a switch/match
    /// (e.g. `"case_statement"` for C++, `"switch_case"` for TS/Java).
    /// Empty string if the language has no switch/case construct.
    pub(crate) case_statement_raw_kind: String,

    /// Raw kind for break statements
    /// (e.g. `"break_statement"` for C++/Java/TS).
    /// Empty string if the language has no break statement.
    pub(crate) break_statement_raw_kind: String,

    // -- recursion detection (recursion enricher) --
    /// Raw kind for function/method call expressions
    /// (e.g. `"call_expression"` for C++/Java/TS, `"call"` for Python).
    /// Empty string if the language has no call expression kind.
    pub(crate) call_expression_raw_kind: String,

    // -- metrics (body-level counting) --
    /// Raw kind for goto statements (e.g. `"goto_statement"` for C++).
    /// Empty string if the language has no goto.
    pub(crate) goto_statement_raw_kind: String,

    /// Raw kinds for string/char literal nodes
    /// (e.g. `["string_literal", "char_literal"]` for C++,
    /// `["string_literal", "raw_string_literal"]` for Rust).
    pub(crate) string_literal_raw_kinds: Vec<String>,

    /// Raw kind for the inline content node inside a string literal
    /// (e.g. `"string_content"` for C++ and Rust tree-sitter grammars).
    /// Number literals whose direct parent has this kind are inside a string
    /// and must be excluded from magic-number detection.
    pub(crate) string_content_raw_kind: String,

    /// Raw kind for throw/raise statements
    /// (e.g. `"throw_statement"` for C++, `""` for Rust which uses `panic!`).
    pub(crate) throw_statement_raw_kind: String,

    // -- show/display --
    /// Raw kind for template/generic declarations wrapping a function or type
    /// (e.g. `"template_declaration"` for C++, `""` for Rust).
    pub(crate) template_declaration_raw_kind: String,

    /// Raw kind for enumerator/variant members inside an enum body
    /// (e.g. `"enumerator"` for C++, `"enum_variant"` for Rust).
    pub(crate) enumerator_raw_kind: String,

    // -- expression analysis (control-flow, redundancy) --
    /// Raw kind for binary arithmetic/comparison expressions
    /// (e.g. `"binary_expression"` — common across most tree-sitter grammars).
    pub(crate) binary_expression_raw_kind: String,

    /// Raw kind for logical `&&`/`||` expressions when the grammar has a
    /// separate node kind (e.g. `"logical_expression"` for C++).
    /// Empty string if logical operators produce `binary_expression` nodes.
    pub(crate) logical_expression_raw_kind: String,

    // -- cast type extraction --
    /// Raw kind for type-descriptor nodes inside cast expressions
    /// (e.g. `"type_descriptor"` for C++).  Empty if not applicable.
    pub(crate) type_descriptor_raw_kind: String,

    /// Raw kind for template/generic argument lists
    /// (e.g. `"template_argument_list"` for C++).  Empty if not applicable.
    pub(crate) template_argument_list_raw_kind: String,

    // -- operators --
    /// Raw kinds that may contain shift operators (`<<`, `>>`).
    /// (e.g. `["shift_expression"]` for C++ — may also include
    /// `binary_expression` for grammars that don't distinguish shifts).
    pub(crate) shift_expression_raw_kinds: Vec<String>,

    /// Synthetic raw kind assigned to compound-assignment rows created by the
    /// operator enricher (e.g. `"compound_assignment"`).
    pub(crate) compound_assignment_raw_kind: String,

    // -- for-loop style disambiguation --
    /// (`raw_kind`, `style_name`) pairs for for-loop style detection.
    /// (e.g. `[("for_statement", "traditional"), ("for_range_loop", "range")]`
    /// for C++, `[("for_expression", "range")]` for Rust).
    pub(crate) for_style_map: Vec<(String, String)>,

    // -- template/generic misparse detection --
    /// Raw kinds whose presence signals a tree-sitter template/generic
    /// misparse (e.g. `>=` mis-parsed as `>` + `=` in C++).
    /// (e.g. `["template_function", "template_type", "template_argument_list"]`).
    /// Empty slice for languages without this issue.
    pub(crate) template_misparse_raw_kinds: Vec<String>,

    // -- skeleton condition normalization (control-flow enricher) --
    /// Raw kind for field/member access expressions
    /// (e.g. `"field_expression"` for C++/Rust).
    pub(crate) field_expression_raw_kind: String,

    /// Raw kind for array/index subscript expressions
    /// (e.g. `"subscript_expression"` for C++, `"index_expression"` for Rust).
    pub(crate) subscript_expression_raw_kind: String,

    /// Raw kind for unary expressions (`!x`, `-x`, etc.)
    /// (e.g. `"unary_expression"` — common across most grammars).
    pub(crate) unary_expression_raw_kind: String,

    /// Raw kind for parenthesized expressions
    /// (e.g. `"parenthesized_expression"` — common across most grammars).
    pub(crate) parenthesized_expression_raw_kind: String,

    /// Raw kind for the condition clause wrapper node
    /// (e.g. `"condition_clause"` for C++).  Empty if not applicable.
    pub(crate) condition_clause_raw_kind: String,

    /// Raw kind for comma expressions
    /// (e.g. `"comma_expression"` for C++).  Empty if not applicable.
    pub(crate) comma_expression_raw_kind: String,

    /// Raw kind for character literals
    /// (e.g. `"char_literal"` for C++).  Empty if not applicable.
    pub(crate) char_literal_raw_kind: String,

    // -- guards --
    /// Node kinds that open a guarded block (e.g. `preproc_ifdef`, `preproc_if`).
    pub(crate) block_guard_kinds: Vec<String>,
    /// Node kinds representing `#elif` branches.
    pub(crate) elif_kinds: Vec<String>,
    /// Node kinds representing `#else` branches.
    pub(crate) else_kinds: Vec<String>,
    /// Grammar field name for the guard condition expression.
    pub(crate) guard_condition_field: String,
    /// Grammar field name for the macro identifier child in `ifdef`/`ifndef`.
    pub(crate) guard_name_field: String,
    /// Token text that marks the negated guard variant (e.g. `"#ifndef"`).
    pub(crate) negate_ifdef_variant: String,
    /// Attribute name for item-level guards (e.g. `"cfg"` for Rust).
    pub(crate) item_guard_attribute: String,
    /// Regex for file-level guard comments (e.g. Go build tags).
    pub(crate) file_guard_pattern: String,
    /// Regex for OS/arch extraction from file suffix.
    pub(crate) file_guard_suffix_pattern: String,
    /// Node kinds for comptime conditional blocks (e.g. Zig).
    pub(crate) comptime_guard_kinds: Vec<String>,
    /// Regex patterns for compile-time guard detection in `if` conditions.
    pub(crate) builtin_guard_patterns: Vec<String>,
    /// Regex patterns for heuristic environment guards.
    pub(crate) env_guard_patterns: Vec<String>,
    /// Regex for directory-based source set extraction (Kotlin).
    pub(crate) source_set_pattern: String,

    // -- macros --
    /// Token texts that prefix macro definitions (e.g. `["#define"]` for C/C++).
    pub(crate) macro_def_markers: Vec<String>,
    /// Raw tree-sitter kinds for macro definitions
    /// (e.g. `["preproc_function_def", "preproc_def"]` for C/C++).
    pub(crate) macro_def_kinds: Vec<String>,
    /// Raw kind for macro invocations (e.g. `"macro_invocation"` for C++).
    /// Empty string when the language has no distinct invocation node kind.
    pub(crate) macro_invocation_kind: String,
    /// Grammar field name for the macro parameter list.
    pub(crate) macro_parameters_field: String,
    /// Grammar field name for the macro body/value.
    pub(crate) macro_value_field: String,

    // -- nested function body kinds (metrics enricher) --
    /// Node kinds that act as nested function-like bodies.
    /// The metrics enricher stops DFS recursion at these nodes so that
    /// return/goto/string/throw counts are not inflated by lambdas.
    /// For C++: `["lambda_expression"]`.
    pub(crate) nested_function_body_raw_kinds: Vec<String>,

    // -- named constant parent kinds (numbers enricher) --
    /// Parent node kinds that indicate a number literal is a named constant
    /// and therefore should NOT be flagged as `is_magic`.
    /// For C++: `["preproc_def", "enumerator", "init_declarator"]`.
    pub(crate) constant_def_parent_raw_kinds: Vec<String>,

    /// Raw tree-sitter kind → FQL kind mapping used by the data-driven
    /// `map_kind` implementation. Built from the `kind_map` section of
    /// the language JSON config.
    pub(crate) kind_map: HashMap<String, String>,

    /// Block-grouping rules (`block_groups` section of the language JSON). Each
    /// coalesces a run of same-kind sibling members into one synthetic, childless
    /// "block" node spanning the run — see `BlockGroupSpec`.
    pub(crate) block_groups: Vec<BlockGroupSpec>,
}

mod config;

// -----------------------------------------------------------------------
// MacroDef — single preprocessor / text macro definition
// -----------------------------------------------------------------------

/// A single macro definition extracted during the first indexing pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacroDef {
    /// Macro name.
    pub name: String,
    /// Parameter names for function-like macros.
    /// `None` for object-like (constant) macros.
    pub params: Option<Vec<String>>,
    /// Expansion body text (post-`\` line-continuation joining).
    pub body: String,
    /// Source file that contains this definition.
    pub file: std::path::PathBuf,
    /// 1-based source line of the definition.
    pub line: u32,
    /// Guard group id from the guard stack at the definition site, if any.
    pub guard_group_id: Option<u64>,
    /// Guard branch text at the definition site, if any.
    pub guard_branch: Option<String>,
}

// -----------------------------------------------------------------------
// MacroExpander trait
// -----------------------------------------------------------------------

/// Language-specific macro expansion strategy.
///
/// Language crates implement this trait to enable the two-pass
/// macro-expansion pipeline.  The default [`LanguageSupport::macro_expander`]
/// returns `None`, meaning no expansion for that language.
pub trait MacroExpander: Send + Sync {
    /// Extract a macro definition from a definition AST node.
    ///
    /// Returns `None` when `node` is not a supported definition kind.
    fn extract_def(
        &self,
        node: tree_sitter::Node<'_>,
        source: &[u8],
        config: &LanguageConfig,
    ) -> Option<MacroDef>;

    /// Extract argument texts from a macro invocation node.
    fn extract_args(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String>;

    /// Substitute parameter names with argument values in an expansion body.
    fn substitute(&self, body: &str, params: &[String], args: &[String]) -> String;

    /// Wrap expanded source text so it can be re-parsed as a standalone statement.
    fn wrap_for_reparse<'a>(&self, expanded: &'a str) -> std::borrow::Cow<'a, str>;
}

// -----------------------------------------------------------------------
// LanguageSupport trait
// LanguageSupport trait
// -----------------------------------------------------------------------

/// Implemented by each language crate to provide grammar-specific behaviour.
pub trait LanguageSupport: Send + Sync {
    /// Short identifier (e.g. `"cpp"`, `"typescript"`).
    fn name(&self) -> &'static str;

    /// File extensions this language handles (without dots).
    fn extensions(&self) -> &'static [&'static str];

    /// The tree-sitter `Language` object for parsing.
    fn tree_sitter_language(&self) -> tree_sitter::Language;

    /// Extract a human-readable name from an AST node.
    ///
    /// Returns `None` for nodes that should not produce index rows.
    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String>;

    /// Map a raw tree-sitter node kind to a universal FQL kind.
    ///
    /// Returns `None` for kinds that have no universal mapping (their
    /// `fql_kind` will be set to `""`).
    fn map_kind(&self, raw_kind: &str) -> Option<&'static str>;

    /// Static language configuration used by enrichers.
    fn config(&self) -> &'static LanguageConfig;

    /// Optional macro expander for two-pass expansion.
    ///
    /// Returns `None` by default — languages without macro-expansion support
    /// do not need to override this method.
    fn macro_expander(&self) -> Option<&dyn MacroExpander> {
        None
    }
}

// -----------------------------------------------------------------------
// Shared helpers for LanguageSupport implementations
// -----------------------------------------------------------------------

/// Extract the UTF-8 text of a tree-sitter node from the source buffer.
///
/// Returns an empty string if the byte range is not valid UTF-8.
/// This is the canonical helper that all language crates should use
/// inside their [`LanguageSupport::extract_name`] implementations.
#[must_use]
pub fn node_text(source: &[u8], node: tree_sitter::Node<'_>) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

// -----------------------------------------------------------------------
// LanguageRegistry — maps file extensions to language implementations
// -----------------------------------------------------------------------

/// Registry of all loaded language implementations.
///
/// The binary crate builds this at startup and passes it through the
/// engine to the indexer.  `forgeql-core` itself has no language
/// implementations — they come from external crates like
/// `forgeql-lang-cpp`.
pub struct LanguageRegistry {
    by_extension: HashMap<&'static str, Arc<dyn LanguageSupport>>,
}

impl std::fmt::Debug for LanguageRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let exts: Vec<&&str> = self.by_extension.keys().collect();
        f.debug_struct("LanguageRegistry")
            .field("extensions", &exts)
            .finish()
    }
}

impl LanguageRegistry {
    /// Create a new registry from a list of language implementations.
    #[must_use]
    pub fn new(languages: Vec<Arc<dyn LanguageSupport>>) -> Self {
        let mut by_extension = HashMap::new();
        for lang in languages {
            for &ext in lang.extensions() {
                drop(by_extension.insert(ext, Arc::clone(&lang)));
            }
        }
        Self { by_extension }
    }

    /// Look up the language for a file path by its extension.
    ///
    /// When the extension is missing or unclaimed, the file name — lowercased,
    /// with any leading dot stripped — is tried against the same key table. A
    /// language that lists `"justfile"` in [`LanguageSupport::extensions`]
    /// therefore claims `justfile`, `.justfile`, `Justfile`, and `x.justfile`
    /// alike, and one that lists `"cmakelists.txt"` claims `CMakeLists.txt`
    /// without owning the `txt` extension. The registry itself knows nothing
    /// about any specific file name — plugins declare their own keys.
    #[must_use]
    pub fn language_for_path(&self, path: &Path) -> Option<Arc<dyn LanguageSupport>> {
        if let Some(lang) = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| self.by_extension.get(ext))
        {
            return Some(Arc::clone(lang));
        }
        let name = path.file_name()?.to_str()?;
        let key = name.trim_start_matches('.').to_ascii_lowercase();
        self.by_extension.get(key.as_str()).cloned()
    }

    /// Look up the language for a file extension string.
    #[must_use]
    pub fn language_for_extension(&self, ext: &str) -> Option<Arc<dyn LanguageSupport>> {
        self.by_extension.get(ext).cloned()
    }

    /// Return all registered language implementations (deduplicated by name).
    #[must_use]
    pub fn languages(&self) -> Vec<Arc<dyn LanguageSupport>> {
        let mut seen = HashMap::new();
        for lang in self.by_extension.values() {
            let _ = seen.entry(lang.name()).or_insert_with(|| Arc::clone(lang));
        }
        seen.into_values().collect()
    }

    /// Return all language configs from registered languages.
    #[must_use]
    pub fn configs(&self) -> Vec<&'static LanguageConfig> {
        self.languages().iter().map(|l| l.config()).collect()
    }
}

#[cfg(any(test, feature = "test-helpers"))]
mod inline;

#[cfg(any(test, feature = "test-helpers"))]
pub use inline::{
    CppLanguageInline, PythonLanguageInline, RustLanguageInline, cpp_config, python_config,
    rust_config,
};
// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpp_map_kind_covers_all_definition_kinds() {
        let lang = CppLanguageInline;
        assert_eq!(lang.map_kind("function_definition"), Some("function"));
        assert_eq!(lang.map_kind("class_specifier"), Some("class"));
        assert_eq!(lang.map_kind("struct_specifier"), Some("struct"));
        assert_eq!(lang.map_kind("enum_specifier"), Some("enum"));
        assert_eq!(lang.map_kind("declaration"), Some("variable"));
        assert_eq!(lang.map_kind("field_declaration"), Some("field"));
        assert_eq!(lang.map_kind("comment"), Some("comment"));
        assert_eq!(lang.map_kind("preproc_include"), Some("import"));
        assert_eq!(lang.map_kind("preproc_def"), Some("macro"));
        assert_eq!(lang.map_kind("type_definition"), Some("type_alias"));
        assert_eq!(lang.map_kind("namespace_definition"), Some("namespace"));
    }

    #[test]
    fn cpp_map_kind_covers_expression_kinds() {
        let lang = CppLanguageInline;
        assert_eq!(lang.map_kind("number_literal"), Some("number"));
        assert_eq!(lang.map_kind("cast_expression"), Some("cast"));
        assert_eq!(lang.map_kind("static_cast_expression"), Some("cast"));
        assert_eq!(lang.map_kind("update_expression"), Some("increment"));
    }

    #[test]
    fn cpp_map_kind_covers_control_flow_kinds() {
        let lang = CppLanguageInline;
        assert_eq!(lang.map_kind("if_statement"), Some("if"));
        assert_eq!(lang.map_kind("while_statement"), Some("while"));
        assert_eq!(lang.map_kind("for_statement"), Some("for"));
        assert_eq!(lang.map_kind("for_range_loop"), Some("for"));
        assert_eq!(lang.map_kind("switch_statement"), Some("switch"));
        assert_eq!(lang.map_kind("do_statement"), Some("do"));
    }

    #[test]
    fn cpp_map_kind_returns_none_for_unknown() {
        let lang = CppLanguageInline;
        assert_eq!(lang.map_kind("translation_unit"), None);
        assert_eq!(lang.map_kind("compound_statement"), None);
    }

    #[test]
    fn registry_resolves_cpp_extensions() {
        let registry = LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]);

        for ext in ["cpp", "c", "cc", "cxx", "h", "hpp", "hxx", "ino"] {
            let path = std::path::PathBuf::from(format!("test.{ext}"));
            let lang = registry.language_for_path(&path);
            assert!(lang.is_some(), "extension {ext} should resolve");
            assert_eq!(lang.as_ref().map(|l| l.name()), Some("cpp"));
        }
    }

    #[test]
    fn registry_returns_none_for_unknown_extension() {
        let registry = LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]);
        let path = std::path::PathBuf::from("test.rs");
        assert!(registry.language_for_path(&path).is_none());
    }

    #[test]
    fn registry_falls_back_to_file_name_for_extensionless_paths() {
        // A registered key doubles as a well-known file name: extensionless
        // paths match by lowercased name with any leading dot stripped.
        let registry = LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]);
        for name in ["cpp", ".cpp", "CPP", "dir/.cpp"] {
            let path = std::path::PathBuf::from(name);
            assert!(
                registry.language_for_path(&path).is_some(),
                "extensionless '{name}' should resolve by file name"
            );
        }
        // An unclaimed extension falls back to the (unclaimed) full name → None.
        let path = std::path::PathBuf::from("other.unknown");
        assert!(registry.language_for_path(&path).is_none());
    }

    #[test]
    fn registry_falls_back_to_full_file_name_when_extension_is_unclaimed() {
        // A key containing a dot claims a well-known full file name (the
        // CMakeLists.txt case) without claiming the bare extension.
        let registry = LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]);
        // CppLanguageInline claims "cpp"; "weird.cpp" resolves via extension,
        // and a full-name key would resolve via the fallback — prove the
        // fallback consults the same table by using a claimed key as a name.
        let path = std::path::PathBuf::from("dir/CPP.unclaimed-ext");
        assert!(
            registry.language_for_path(&path).is_none(),
            "unclaimed extension + unclaimed full name must stay None"
        );
        let path = std::path::PathBuf::from("dir/cpp");
        assert!(registry.language_for_path(&path).is_some());
    }

    #[test]
    fn registry_language_for_extension() {
        let registry = LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]);
        assert!(registry.language_for_extension("cpp").is_some());
        assert!(registry.language_for_extension("py").is_none());
    }

    #[test]
    fn registry_languages_deduplicates() {
        let registry = LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]);
        let languages = registry.languages();
        assert_eq!(languages.len(), 1);
        assert_eq!(languages[0].name(), "cpp");
    }

    #[test]
    fn cpp_config_is_consistent() {
        let config = CppLanguageInline.config();
        assert_eq!(config.root_node_kind, "translation_unit");
        assert_eq!(config.scope_separator, "::");
        assert!(!config.function_raw_kinds.is_empty());
        assert!(!config.type_raw_kinds.is_empty());
        // skip_node_kinds may be empty if all formerly-skipped nodes are handled by guard config
        assert!(!config.usage_node_kinds.is_empty());
    }

    #[test]
    fn query_methods_kind_membership() {
        let cfg = CppLanguageInline.config();
        // slice-based membership
        assert!(cfg.is_function_kind("function_definition"));
        assert!(!cfg.is_function_kind("class_specifier"));
        assert!(cfg.is_type_kind("class_specifier"));
        assert!(cfg.is_type_kind("struct_specifier"));
        assert!(cfg.is_definition_kind("function_definition"));
        assert!(cfg.is_declaration_kind("declaration"));
        assert!(cfg.is_field_kind("field_declaration"));
        assert!(cfg.is_member_kind("field_declaration"));
        assert!(cfg.is_number_literal_kind("number_literal"));
        assert!(cfg.is_control_flow_kind("if_statement"));
        assert!(cfg.is_switch_kind("switch_statement"));
        assert!(cfg.is_modifier_node_kind("type_qualifier"));
        assert!(cfg.is_assignment_kind("assignment_expression"));
        assert!(cfg.is_update_kind("update_expression"));
        assert!(cfg.is_string_literal_kind("string_literal"));
        assert!(!cfg.is_skip_kind("preproc_else")); // now traversed as a guard branch
        assert!(cfg.is_usage_node_kind("identifier"));
        assert!(cfg.is_shift_expression_kind("shift_expression"));
        assert!(cfg.is_template_misparse_kind("template_function"));
        assert!(cfg.is_null_literal("nullptr"));
        assert!(cfg.is_boolean_literal("true"));
        assert!(cfg.is_static_storage_keyword("static"));
        // negative cases
        assert!(!cfg.is_skip_kind("function_definition"));
        assert!(!cfg.is_null_literal("42"));
    }

    #[test]
    fn query_methods_single_kind() {
        let cfg = CppLanguageInline.config();
        assert!(cfg.is_root_kind("translation_unit"));
        assert!(cfg.is_parameter_kind("parameter_declaration"));
        assert!(cfg.is_member_body_kind("field_declaration_list"));
        assert!(cfg.is_comment_kind("comment"));
        assert!(cfg.is_block_kind("compound_statement"));
        assert!(cfg.is_identifier_kind("identifier"));
        assert!(cfg.is_init_declarator_kind("init_declarator"));
        assert!(cfg.is_return_statement_kind("return_statement"));
        assert!(cfg.is_address_of_expression_kind("pointer_expression"));
        assert!(cfg.is_case_statement_kind("case_statement"));
        assert!(cfg.is_break_statement_kind("break_statement"));
        assert!(cfg.is_call_expression_kind("call_expression"));
        assert!(cfg.is_goto_statement_kind("goto_statement"));
        assert!(cfg.is_throw_statement_kind("throw_statement"));
        assert!(cfg.is_template_declaration_kind("template_declaration"));
        assert!(cfg.is_enumerator_kind("enumerator"));
        assert!(cfg.is_binary_expression_kind("binary_expression"));
        assert!(cfg.is_logical_expression_kind("logical_expression"));
        assert!(cfg.is_parameter_list_kind("parameter_list"));
        assert!(cfg.is_char_literal_kind("char_literal"));
        // negative
        assert!(!cfg.is_root_kind("program"));
        assert!(!cfg.is_block_kind("block"));
    }

    #[test]
    fn query_methods_feature_checks() {
        let cfg = CppLanguageInline.config();
        assert!(cfg.has_address_of());
        assert!(cfg.has_call_expression());
        assert!(cfg.has_case_statement());
        assert!(cfg.has_comment());
        assert!(cfg.has_static_storage());
        assert!(cfg.has_logical_expression());
        assert!(cfg.has_template_declaration());
        assert!(cfg.has_enumerator());
        assert!(cfg.has_goto);
        assert!(cfg.has_increment_decrement);
        assert!(cfg.has_implicit_truthiness);
    }

    #[test]
    fn query_methods_accessors() {
        let cfg = CppLanguageInline.config();
        assert_eq!(cfg.scope_sep(), "::");
        assert_eq!(cfg.declarator_field(), "declarator");
        assert_eq!(cfg.function_declarator(), "function_declarator");
        assert_eq!(cfg.address_of_op(), "&");
    }

    #[test]
    fn query_methods_lookups() {
        let cfg = CppLanguageInline.config();
        // cast_info (direct node-kind casts)
        assert_eq!(
            cfg.cast_info("cast_expression"),
            Some(("c_style", "unsafe"))
        );
        assert_eq!(
            cfg.cast_info("static_cast_expression"),
            Some(("static_cast", "safe"))
        );
        assert_eq!(cfg.cast_info("unknown"), None);
        // named_cast_info (keyword-based, for tree-sitter-cpp 0.23 call_expression style)
        assert_eq!(
            cfg.named_cast_info("static_cast"),
            Some(("static_cast", "safe"))
        );
        assert_eq!(
            cfg.named_cast_info("dynamic_cast"),
            Some(("dynamic_cast", "safe"))
        );
        assert_eq!(
            cfg.named_cast_info("const_cast"),
            Some(("const_cast", "moderate"))
        );
        assert_eq!(
            cfg.named_cast_info("reinterpret_cast"),
            Some(("reinterpret_cast", "unsafe"))
        );
        assert_eq!(cfg.named_cast_info("unknown_cast"), None);
        // for_style
        assert_eq!(cfg.for_style("for_statement"), Some("traditional"));
        assert_eq!(cfg.for_style("for_range_loop"), Some("range"));
        assert_eq!(cfg.for_style("while_statement"), None);
        // modifier_field_for
        assert_eq!(cfg.modifier_field_for("const"), Some("is_const"));
        assert_eq!(cfg.modifier_field_for("virtual"), Some("is_virtual"));
        assert_eq!(cfg.modifier_field_for("unknown"), None);
        // visibility
        assert_eq!(cfg.visibility_for_keyword("public"), Some("public"));
        assert_eq!(cfg.visibility_for_keyword("private"), Some("private"));
        assert_eq!(cfg.visibility_for_keyword("unknown"), None);
        // default visibility for type
        assert_eq!(
            cfg.default_visibility_for_type("class_specifier"),
            Some("private")
        );
        assert_eq!(
            cfg.default_visibility_for_type("struct_specifier"),
            Some("public")
        );
        // comment style
        assert_eq!(cfg.detect_comment_style("/** doc */"), Some("doc_block"));
        assert_eq!(cfg.detect_comment_style("/// doc"), Some("doc_line"));
        assert_eq!(cfg.detect_comment_style("/* block */"), Some("block"));
        assert_eq!(cfg.detect_comment_style("// line"), Some("line"));
        // number suffix
        assert_eq!(cfg.number_suffix_meaning("f"), Some("float"));
        assert_eq!(cfg.number_suffix_meaning("ull"), Some("unsigned_long_long"));
        assert_eq!(cfg.number_suffix_meaning("xyz"), None);
    }

    #[test]
    fn cpp_extract_name_via_trait() {
        let lang = CppLanguageInline;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang.tree_sitter_language())
            .expect("set language");

        let source = b"void processSignal(int speed) { return; }";
        let tree = parser.parse(source, None).expect("parse");
        let root = tree.root_node();

        // Walk to find the function_definition node.
        let func_node = root.child(0).expect("function_definition");
        assert_eq!(func_node.kind(), "function_definition");

        let name = lang.extract_name(func_node, source);
        assert_eq!(name.as_deref(), Some("processSignal"));
    }
}
