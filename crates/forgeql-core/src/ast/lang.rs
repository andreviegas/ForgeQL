/// Language abstraction layer for `ForgeQL`.
///
/// Every supported language implements [`LanguageSupport`] and provides a
/// [`LanguageConfig`] describing its grammar-specific details.  The
/// [`LanguageRegistry`] maps file extensions to language implementations,
/// allowing the indexer and engine to operate language-agnostically.
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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
/// language-agnostically.  Each language crate provides a `static`
/// instance of this struct.
pub struct LanguageConfig {
    // -- identity --
    /// Root node kind produced by the tree-sitter grammar (e.g.
    /// `"translation_unit"` for C++, `"program"` for TypeScript).
    pub root_node_kind: &'static str,

    /// Scope resolution separator (e.g. `"::"` for C++, `"."` for most others).
    pub scope_separator: &'static str,

    // -- node kind sets (raw tree-sitter kinds for enricher internal checks) --
    /// Raw kinds that represent function/method definitions.
    pub function_raw_kinds: &'static [&'static str],

    /// Raw kinds that represent type definitions (class, struct, enum, etc.).
    pub type_raw_kinds: &'static [&'static str],

    /// Raw kinds that represent any definition (for `has_doc` checks).
    pub definition_raw_kinds: &'static [&'static str],

    /// Raw kinds that represent variable/const declarations.
    pub declaration_raw_kinds: &'static [&'static str],

    /// Raw kinds that represent member/field declarations.
    pub field_raw_kinds: &'static [&'static str],

    /// Raw kind for parameter declarations (e.g. `"parameter_declaration"`).
    pub parameter_raw_kind: &'static str,

    /// Raw kind for the body of a type (e.g. `"field_declaration_list"`).
    pub member_body_raw_kind: &'static str,

    /// Raw kinds for members inside a type body.
    pub member_raw_kinds: &'static [&'static str],

    /// Raw kind for comments.
    pub comment_raw_kind: &'static str,

    // -- number literals --
    /// Raw kinds that represent number literals.
    pub number_literal_raw_kinds: &'static [&'static str],

    /// Digit group separator (e.g. `Some('\'')` for C++, `Some('_')` for Rust).
    pub digit_separator: Option<char>,

    /// (`suffix_text`, meaning) pairs for number literal suffixes.
    pub number_suffixes: &'static [(&'static str, &'static str)],

    // -- control flow --
    /// Raw kinds that represent control-flow statements indexed by the
    /// control-flow enricher.
    pub control_flow_raw_kinds: &'static [&'static str],

    /// Raw kinds specifically for switch/match statements.
    pub switch_raw_kinds: &'static [&'static str],

    // -- literals --
    /// Null literal values (e.g. `["nullptr", "NULL", "0"]` for C++).
    pub null_literals: &'static [&'static str],

    /// Boolean literal values (e.g. `["true", "false"]`).
    pub boolean_literals: &'static [&'static str],

    // -- comments --
    /// (prefix, `style_name`) pairs for detecting comment styles.
    /// Checked in order — first match wins.
    pub doc_comment_prefixes: &'static [(&'static str, &'static str)],

    // -- modifiers --
    /// (keyword, `field_name`) pairs for modifier detection.
    pub modifier_map: &'static [(&'static str, &'static str)],

    /// Raw node kinds that carry modifier/qualifier keywords.
    pub modifier_node_kinds: &'static [&'static str],

    /// (keyword, visibility) pairs.
    pub visibility_keywords: &'static [(&'static str, &'static str)],

    /// (`raw_kind`, `default_visibility`) pairs — default visibility for
    /// members of each type kind when no explicit access specifier is present.
    pub visibility_default_by_type: &'static [(&'static str, &'static str)],

    // -- casts --
    /// (`raw_kind`, `cast_style`, `cast_safety`) triples for cast detection.
    pub cast_kinds: &'static [(&'static str, &'static str, &'static str)],

    // -- capabilities --
    /// Whether the language has `goto` statements.
    pub has_goto: bool,

    /// Whether the language has `++`/`--` operators.
    pub has_increment_decrement: bool,

    /// Whether the language has implicit truthiness (e.g. `if (ptr)` in C++).
    pub has_implicit_truthiness: bool,

    /// Raw kind for decorator/attribute nodes, if the language has them.
    pub decorator_raw_kind: Option<&'static str>,

    /// Node kinds whose subtrees should be skipped entirely during indexing
    /// (e.g. `["preproc_else", "preproc_elif"]` in C++).
    pub skip_node_kinds: &'static [&'static str],

    /// Identifier node kinds that produce usage sites.
    pub usage_node_kinds: &'static [&'static str],

    // -- declarator structure --
    /// Grammar field name for the declarator child of a definition/declaration
    /// node (e.g. `"declarator"` in C++).
    pub declarator_field_name: &'static str,

    /// Raw kind for a function-type declarator nested inside a declarator tree
    /// (e.g. `"function_declarator"` in C++).
    pub function_declarator_kind: &'static str,

    // -- declaration distance / data-flow (decl_distance enricher) --
    /// Raw kind for the parameter list container node
    /// (e.g. `"parameter_list"` for C++, `"formal_parameters"` for TS/Java).
    pub parameter_list_raw_kind: &'static str,

    /// Raw kind for a simple identifier token
    /// (e.g. `"identifier"` — universal for most tree-sitter grammars).
    pub identifier_raw_kind: &'static str,

    /// Raw kinds for assignment expressions
    /// (e.g. `["assignment_expression"]` for C++/TS/Java, `["assignment"]` for Python).
    pub assignment_raw_kinds: &'static [&'static str],

    /// Raw kinds for update/increment expressions (`++x`, `x--`).
    /// Empty slice for languages without increment/decrement operators.
    pub update_raw_kinds: &'static [&'static str],

    /// Raw kind for an init-declarator wrapper node
    /// (e.g. `"init_declarator"` for C++).  Empty string if the language
    /// does not have this intermediate node.
    pub init_declarator_raw_kind: &'static str,

    /// Raw kind for block/compound statement nodes
    /// (e.g. `"compound_statement"` for C++, `"statement_block"` for TS,
    /// `"block"` for Python/Rust).
    pub block_raw_kind: &'static str,

    // -- escape detection (escape enricher) --
    /// Raw kind for return statements
    /// (e.g. `"return_statement"` for C++/Java/TS, `"return_expression"` for Rust).
    /// Empty string if the language has no explicit return statement kind.
    pub return_statement_raw_kind: &'static str,

    /// Raw kind for the expression node that represents taking-address-of
    /// (e.g. `"pointer_expression"` for C++, `"reference_expression"` for Rust).
    /// Empty string if the language has no address-of operator.
    pub address_of_expression_raw_kind: &'static str,

    /// The textual operator for address-of (e.g. `"&"` for C/C++/Rust/Go/Zig).
    /// Empty string if the language has no address-of operator — the escape
    /// enricher will short-circuit.
    pub address_of_operator: &'static str,

    /// Raw kind for array declarators
    /// (e.g. `"array_declarator"` for C++). Empty string if N/A.
    pub array_declarator_raw_kind: &'static str,

    /// Keywords that mark a local as having static storage duration
    /// (e.g. `["static"]` for C/C++). Empty for languages without this concept.
    pub static_storage_keywords: &'static [&'static str],

    // -- fallthrough detection (fallthrough enricher) --
    /// Raw kind for case/default labels inside a switch/match
    /// (e.g. `"case_statement"` for C++, `"switch_case"` for TS/Java).
    /// Empty string if the language has no switch/case construct.
    pub case_statement_raw_kind: &'static str,

    /// Raw kind for break statements
    /// (e.g. `"break_statement"` for C++/Java/TS).
    /// Empty string if the language has no break statement.
    pub break_statement_raw_kind: &'static str,
}

// -----------------------------------------------------------------------
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
    #[must_use]
    pub fn language_for_path(&self, path: &Path) -> Option<Arc<dyn LanguageSupport>> {
        let ext = path.extension()?.to_str()?;
        self.by_extension.get(ext).cloned()
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
}

// -----------------------------------------------------------------------
// CppLanguageInline — test-only in-crate C++ implementation
//
// The production C++ support lives in `forgeql-lang-cpp`.  This inline
// duplicate stays here behind `#[cfg(any(test, feature = "test-helpers"))]`
// so that forgeql-core's own unit and integration tests can build a
// LanguageRegistry without depending on the external crate.
// -----------------------------------------------------------------------

/// Test-only inline C++ language support.
///
/// For production use, depend on `forgeql-lang-cpp::CppLanguage` instead.
#[cfg(any(test, feature = "test-helpers"))]
pub struct CppLanguageInline;

/// Static configuration for C/C++ (test-only).
#[cfg(any(test, feature = "test-helpers"))]
pub static CPP_CONFIG: LanguageConfig = LanguageConfig {
    root_node_kind: "translation_unit",
    scope_separator: "::",

    function_raw_kinds: &["function_definition"],
    type_raw_kinds: &["class_specifier", "struct_specifier", "enum_specifier"],
    definition_raw_kinds: &[
        "function_definition",
        "class_specifier",
        "struct_specifier",
        "enum_specifier",
    ],
    declaration_raw_kinds: &["declaration"],
    field_raw_kinds: &["field_declaration"],
    parameter_raw_kind: "parameter_declaration",
    member_body_raw_kind: "field_declaration_list",
    member_raw_kinds: &["field_declaration"],
    comment_raw_kind: "comment",

    number_literal_raw_kinds: &["number_literal"],
    digit_separator: Some('\''),
    number_suffixes: &[
        ("ull", "unsigned_long_long"),
        ("ull", "unsigned_long_long"),
        ("ul", "unsigned_long"),
        ("ll", "long_long"),
        ("uz", "unsigned_size"),
        ("u", "unsigned"),
        ("l", "long"),
        ("z", "size"),
        ("f", "float"),
    ],

    control_flow_raw_kinds: &[
        "if_statement",
        "while_statement",
        "for_statement",
        "for_range_loop",
        "switch_statement",
        "do_statement",
    ],
    switch_raw_kinds: &["switch_statement"],

    null_literals: &["nullptr", "NULL", "0"],
    boolean_literals: &["true", "false"],

    doc_comment_prefixes: &[
        ("/**", "doc_block"),
        ("///", "doc_line"),
        ("/*", "block"),
        ("//", "line"),
    ],

    modifier_map: &[
        ("const", "is_const"),
        ("static", "is_static"),
        ("virtual", "is_virtual"),
        ("inline", "is_inline"),
        ("extern", "is_extern"),
        ("volatile", "is_volatile"),
        ("mutable", "is_mutable"),
        ("constexpr", "is_constexpr"),
        ("explicit", "is_explicit"),
        ("override", "is_override"),
        ("final", "is_final"),
    ],
    modifier_node_kinds: &[
        "type_qualifier",
        "storage_class_specifier",
        "virtual_specifier",
    ],
    visibility_keywords: &[
        ("public", "public"),
        ("private", "private"),
        ("protected", "protected"),
    ],
    visibility_default_by_type: &[
        ("class_specifier", "private"),
        ("struct_specifier", "public"),
    ],

    cast_kinds: &[
        ("cast_expression", "c_style", "unsafe"),
        ("static_cast_expression", "static_cast", "safe"),
        ("reinterpret_cast_expression", "reinterpret_cast", "unsafe"),
        ("const_cast_expression", "const_cast", "moderate"),
        ("dynamic_cast_expression", "dynamic_cast", "safe"),
    ],

    has_goto: true,
    has_increment_decrement: true,
    has_implicit_truthiness: true,
    decorator_raw_kind: None,
    skip_node_kinds: &["preproc_else", "preproc_elif"],
    usage_node_kinds: &["identifier", "field_identifier", "type_identifier"],
    declarator_field_name: "declarator",
    function_declarator_kind: "function_declarator",

    parameter_list_raw_kind: "parameter_list",
    identifier_raw_kind: "identifier",
    assignment_raw_kinds: &["assignment_expression"],
    update_raw_kinds: &["update_expression"],
    init_declarator_raw_kind: "init_declarator",
    block_raw_kind: "compound_statement",

    return_statement_raw_kind: "return_statement",
    address_of_expression_raw_kind: "pointer_expression",
    address_of_operator: "&",
    array_declarator_raw_kind: "array_declarator",
    static_storage_keywords: &["static"],

    case_statement_raw_kind: "case_statement",
    break_statement_raw_kind: "break_statement",
};

#[cfg(any(test, feature = "test-helpers"))]
impl LanguageSupport for CppLanguageInline {
    fn name(&self) -> &'static str {
        "cpp"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["cpp", "c", "cc", "cxx", "h", "hpp", "hxx", "ino"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_cpp::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        // Structural nodes that are part of a declarator tree should never
        // produce their own index rows.
        if node.kind() == "qualified_identifier" {
            return None;
        }

        // Universal: most grammars expose a "name" field on definition nodes.
        if let Some(name_node) = node.child_by_field_name("name") {
            let text = cpp_node_text(source, name_node);
            if !text.is_empty() {
                return Some(text);
            }
        }

        match node.kind() {
            "function_definition" => node
                .child_by_field_name("declarator")
                .and_then(cpp_find_function_name)
                .map(|n| cpp_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "preproc_include" => node
                .child_by_field_name("path")
                .map(|n| {
                    cpp_node_text(source, n)
                        .trim_matches(|c: char| c == '"' || c == '<' || c == '>')
                        .to_string()
                })
                .filter(|s| !s.is_empty()),

            "declaration" => {
                let decl = node.child_by_field_name("declarator")?;
                if cpp_contains_function_declarator(decl) {
                    return None;
                }
                cpp_find_function_name(decl)
                    .map(|n| cpp_node_text(source, n))
                    .filter(|s| !s.is_empty())
            }

            "field_declaration" => node
                .child_by_field_name("declarator")
                .and_then(cpp_find_function_name)
                .map(|n| cpp_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "parameter_declaration" => node
                .child_by_field_name("declarator")
                .and_then(cpp_find_function_name)
                .map(|n| cpp_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "comment" => {
                let text = cpp_node_text(source, node);
                if text.is_empty() { None } else { Some(text) }
            }

            _ => None,
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        match raw_kind {
            // Definition kinds
            "function_definition" => Some(FQL_FUNCTION),
            "class_specifier" => Some(FQL_CLASS),
            "struct_specifier" => Some(FQL_STRUCT),
            "enum_specifier" => Some(FQL_ENUM),
            "declaration" | "parameter_declaration" => Some(FQL_VARIABLE),
            "field_declaration" => Some(FQL_FIELD),
            "comment" => Some(FQL_COMMENT),
            "preproc_include" => Some(FQL_IMPORT),
            "preproc_def" | "preproc_function_def" => Some(FQL_MACRO),
            "type_definition" | "alias_declaration" => Some(FQL_TYPE_ALIAS),
            "namespace_definition" => Some(FQL_NAMESPACE),

            // Expression/literal kinds (from enricher extra_rows)
            "number_literal" => Some(FQL_NUMBER),
            "cast_expression"
            | "static_cast_expression"
            | "reinterpret_cast_expression"
            | "const_cast_expression"
            | "dynamic_cast_expression" => Some(FQL_CAST),
            "update_expression" => Some(FQL_INCREMENT),

            // Control flow kinds (from enricher extra_rows)
            "if_statement" => Some(FQL_IF),
            "while_statement" => Some(FQL_WHILE),
            "for_statement" | "for_range_loop" => Some(FQL_FOR),
            "switch_statement" => Some(FQL_SWITCH),
            "do_statement" => Some(FQL_DO),

            _ => None,
        }
    }

    fn config(&self) -> &'static LanguageConfig {
        &CPP_CONFIG
    }
}

// -----------------------------------------------------------------------
// C++ helper functions (test-only — production impl in forgeql-lang-cpp)
// -----------------------------------------------------------------------

#[cfg(any(test, feature = "test-helpers"))]
fn cpp_node_text(source: &[u8], node: tree_sitter::Node<'_>) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

#[cfg(any(test, feature = "test-helpers"))]
fn cpp_find_function_name(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    match node.kind() {
        "identifier"
        | "field_identifier"
        | "destructor_name"
        | "operator_name"
        | "qualified_identifier" => Some(node),
        "function_declarator"
        | "pointer_declarator"
        | "reference_declarator"
        | "abstract_function_declarator" => node
            .child_by_field_name("declarator")
            .and_then(cpp_find_function_name),
        _ => {
            for i in 0..node.named_child_count() {
                if let Some(found) = node.named_child(i).and_then(cpp_find_function_name) {
                    return Some(found);
                }
            }
            None
        }
    }
}

#[cfg(any(test, feature = "test-helpers"))]
fn cpp_contains_function_declarator(node: tree_sitter::Node<'_>) -> bool {
    if node.kind() == "function_declarator" {
        return true;
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i)
            && cpp_contains_function_declarator(child)
        {
            return true;
        }
    }
    false
}

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
        assert!(!config.skip_node_kinds.is_empty());
        assert!(!config.usage_node_kinds.is_empty());
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
