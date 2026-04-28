/// JSON deserialization layer for [`LanguageConfig`].
///
/// Language crates can describe their grammar mapping in a JSON file
/// (grouped by semantic domain — Option B layout) and load it at startup
/// with [`LanguageConfigJson::from_json_bytes`] → [`LanguageConfig`].
///
/// # JSON structure
///
/// ```json
/// {
///   "language": { "name": "cpp", "extensions": [...], "tree_sitter_grammar": "..." },
///   "syntax":   { "root_node": "...", "block": "...", ... },
///   "definitions": { "function_kinds": [...], "type_kinds": [...], ... },
///   "control_flow": { "kinds": [...], "switch_kinds": [...], ... },
///   "statements": { "return": "...", "break": "...", ... },
///   "expressions": { "call": "...", "binary": "...", ... },
///   "types": { "type_descriptor": "...", ... },
///   "literals": { "number_kinds": [...], "string_kinds": [...], ... },
///   "modifiers": { "map": [[...]], "node_kinds": [...], ... },
///   "visibility": { "keywords": [[...]], "default_by_type": [[...]] },
///   "casts": [["raw_kind", "style", "safety"], ...],
///   "comments": { "prefixes": [["prefix", "style"], ...] },
///   "capabilities": { "has_goto": true, ... },
///   "kind_map": { "raw_kind": "fql_kind", ... }
/// }
/// ```
use std::collections::HashMap;

use serde::Deserialize;

use super::lang::LanguageConfig;

// -----------------------------------------------------------------------
// Top-level JSON config
// -----------------------------------------------------------------------

/// Root structure of a language JSON config file.
#[derive(Deserialize)]
pub struct LanguageConfigJson {
    /// Language metadata (name, extensions, grammar).
    pub language: LanguageSection,

    /// Syntax / structural node kinds.
    #[serde(default)]
    pub syntax: SyntaxSection,

    /// Definition node kinds.
    #[serde(default)]
    pub definitions: DefinitionsSection,

    /// Control-flow node kinds.
    #[serde(default)]
    pub control_flow: ControlFlowSection,

    /// Statement node kinds.
    #[serde(default)]
    pub statements: StatementsSection,

    /// Expression node kinds.
    #[serde(default)]
    pub expressions: ExpressionsSection,

    /// Type-related node kinds.
    #[serde(default)]
    pub types: TypesSection,

    /// Literal node kinds and values.
    #[serde(default)]
    pub literals: LiteralsSection,

    /// Modifier detection configuration.
    #[serde(default)]
    pub modifiers: ModifiersSection,

    /// Visibility / access control configuration.
    #[serde(default)]
    pub visibility: VisibilitySection,

    /// Cast detection: `[raw_kind, style, safety]` triples.
    #[serde(default)]
    pub casts: Vec<(String, String, String)>,

    /// Comment style detection.
    #[serde(default)]
    pub comments: CommentsSection,

    /// Language capability flags.
    #[serde(default)]
    pub capabilities: CapabilitiesSection,

    /// Preprocessor / compiler guard configuration.
    #[serde(default)]
    pub guards: GuardsSection,

    /// Macro expansion configuration.
    #[serde(default)]
    pub macros: MacrosSection,

    /// Raw tree-sitter kind → FQL kind mapping.
    #[serde(default)]
    pub kind_map: HashMap<String, String>,
}

// -----------------------------------------------------------------------
// Section structs
// -----------------------------------------------------------------------

/// Language identity metadata.
#[derive(Deserialize)]
pub struct LanguageSection {
    /// Short identifier (e.g. `"cpp"`, `"rust"`).
    pub name: String,

    /// File extensions this language handles (without dots).
    pub extensions: Vec<String>,

    /// Tree-sitter grammar crate name (informational).
    #[serde(default)]
    pub tree_sitter_grammar: String,
}

/// Syntax / structural node kinds.
#[derive(Deserialize, Default)]
pub struct SyntaxSection {
    /// Root node kind (e.g. `"translation_unit"`, `"source_file"`).
    #[serde(default)]
    pub root_node: String,

    /// Scope separator (e.g. `"::"`, `"."`).
    #[serde(default = "default_scope_separator")]
    pub scope_separator: String,

    /// Block/compound statement kind.
    #[serde(default)]
    pub block: String,

    /// Identifier token kind.
    #[serde(default = "default_identifier")]
    pub identifier: String,

    /// Parameter list container kind.
    #[serde(default)]
    pub parameter_list: String,

    /// Grammar field name for declarators.
    #[serde(default)]
    pub declarator_field: String,

    /// Function-type declarator kind.
    #[serde(default)]
    pub function_declarator: String,

    /// Init-declarator wrapper kind.
    #[serde(default)]
    pub init_declarator: String,

    /// Comment node kind.
    #[serde(default)]
    pub comment: String,

    /// Node kinds to skip during indexing.
    #[serde(default)]
    pub skip_node_kinds: Vec<String>,

    /// Identifier node kinds that produce usage sites.
    #[serde(default)]
    pub usage_node_kinds: Vec<String>,

    /// Node kinds that act as statement / expression boundaries.
    #[serde(default)]
    pub statement_boundary_kinds: Vec<String>,

    /// Node kinds that create a new variable scope (for shadow detection).
    #[serde(default)]
    pub scope_creating_kinds: Vec<String>,
}

/// Definition node kinds.
#[derive(Deserialize, Default)]
pub struct DefinitionsSection {
    /// Function/method definition kinds.
    #[serde(default)]
    pub function_kinds: Vec<String>,

    /// Type definition kinds (class, struct, enum, etc.).
    #[serde(default)]
    pub type_kinds: Vec<String>,

    /// All definition kinds (for `has_doc` checks).
    #[serde(default)]
    pub definition_kinds: Vec<String>,

    /// Variable/const declaration kinds.
    #[serde(default)]
    pub declaration_kinds: Vec<String>,

    /// Member/field declaration kinds.
    #[serde(default)]
    pub field_kinds: Vec<String>,

    /// Parameter declaration kind.
    #[serde(default)]
    pub parameter_kind: String,

    /// Type body container kind.
    #[serde(default)]
    pub member_body_kind: String,

    /// Member kinds inside a type body.
    #[serde(default)]
    pub member_kinds: Vec<String>,

    /// Node kinds that act as owner containers for methods/functions.
    /// For Rust: `impl_item`, `trait_item`.  For C++: `class_specifier`, `struct_specifier`.
    #[serde(default)]
    pub owner_container_kinds: Vec<String>,

    /// Declaration kinds that are block-scoped.
    /// Empty = all declaration kinds are block-scoped (default for C++, Rust, Python).
    #[serde(default)]
    pub block_scoped_declaration_kinds: Vec<String>,

    /// Node kinds that act as nested function bodies (lambda, closure, etc.).
    /// The metrics enricher stops DFS at these to avoid inflating counts.
    /// For C++: `["lambda_expression"]`.
    #[serde(default)]
    pub nested_function_body_kinds: Vec<String>,

    /// Parent node kinds indicating a named constant (suppress `is_magic`).
    /// For C++: `["preproc_def", "enumerator", "init_declarator"]`.
    #[serde(default)]
    pub constant_def_parent_kinds: Vec<String>,
}

/// Control-flow node kinds.
#[derive(Deserialize, Default)]
pub struct ControlFlowSection {
    /// All control-flow statement/expression kinds.
    #[serde(default)]
    pub kinds: Vec<String>,

    /// Switch/match statement kinds.
    #[serde(default)]
    pub switch_kinds: Vec<String>,

    /// `[raw_kind, style]` pairs for for-loop variants.
    #[serde(default)]
    pub for_style_map: Vec<(String, String)>,

    /// Condition clause wrapper kind.
    #[serde(default)]
    pub condition_clause: String,

    /// Conditional-branch node kinds (for dead-store branch-depth tracking).
    #[serde(default)]
    pub branch_kinds: Vec<String>,

    /// Loop-construct node kinds (for dead-store branch-depth tracking).
    #[serde(default)]
    pub loop_kinds: Vec<String>,

    /// Exception-handler node kinds.
    #[serde(default)]
    pub exception_handler_kinds: Vec<String>,
}

/// Statement node kinds.
#[derive(Deserialize, Default)]
pub struct StatementsSection {
    /// Return statement kind.
    #[serde(default, rename = "return")]
    pub return_kind: String,

    /// Break statement kind.
    #[serde(default, rename = "break")]
    pub break_kind: String,

    /// Goto statement kind.
    #[serde(default)]
    pub goto: String,

    /// Throw/raise statement kind.
    #[serde(default)]
    pub throw: String,

    /// Case/label statement kind.
    #[serde(default)]
    pub case: String,

    /// Assignment expression kinds.
    #[serde(default)]
    pub assignment_kinds: Vec<String>,

    /// Update/increment expression kinds.
    #[serde(default)]
    pub update_kinds: Vec<String>,
}

/// Expression node kinds.
#[derive(Deserialize, Default)]
pub struct ExpressionsSection {
    /// Call expression kind.
    #[serde(default)]
    pub call: String,

    /// Binary expression kind.
    #[serde(default)]
    pub binary: String,

    /// Logical expression kind (if separate from binary).
    #[serde(default)]
    pub logical: String,

    /// Unary expression kind.
    #[serde(default)]
    pub unary: String,

    /// Field/member access expression kind.
    #[serde(default)]
    pub field_access: String,

    /// Subscript/index expression kind.
    #[serde(default)]
    pub subscript: String,

    /// Parenthesized expression kind.
    #[serde(default = "default_parenthesized")]
    pub parenthesized: String,

    /// Comma expression kind.
    #[serde(default)]
    pub comma: String,

    /// Address-of expression kind.
    #[serde(default)]
    pub address_of: String,

    /// Address-of operator text (e.g. `"&"`).
    #[serde(default)]
    pub address_of_operator: String,

    /// Shift expression kinds.
    #[serde(default)]
    pub shift_kinds: Vec<String>,

    /// Compound assignment kind.
    #[serde(default)]
    pub compound_assignment: String,
}

/// Type-related node kinds.
#[derive(Deserialize, Default)]
pub struct TypesSection {
    /// Type descriptor kind.
    #[serde(default)]
    pub type_descriptor: String,

    /// Template/generic argument list kind.
    #[serde(default)]
    pub template_argument_list: String,

    /// Template/generic declaration kind.
    #[serde(default)]
    pub template_declaration: String,

    /// Array declarator kind.
    #[serde(default)]
    pub array_declarator: String,

    /// Enumerator/variant member kind.
    #[serde(default)]
    pub enumerator: String,

    /// Template misparse indicator kinds.
    #[serde(default)]
    pub template_misparse_kinds: Vec<String>,

    /// Decorator/attribute kind.
    #[serde(default)]
    pub decorator: Option<String>,
}

/// Literal node kinds and values.
#[derive(Deserialize, Default)]
pub struct LiteralsSection {
    /// Number literal kinds.
    #[serde(default)]
    pub number_kinds: Vec<String>,

    /// String literal kinds.
    #[serde(default)]
    pub string_kinds: Vec<String>,

    /// Character literal kind.
    #[serde(default)]
    pub char_kind: String,

    /// Null literal values.
    #[serde(default)]
    pub null_values: Vec<String>,

    /// Boolean literal values.
    #[serde(default)]
    pub boolean_values: Vec<String>,

    /// Digit group separator character (e.g. `"'"` for C++, `"_"` for Rust).
    /// Deserialized as a single-character string; `None` if absent.
    #[serde(default)]
    pub digit_separator: Option<String>,

    /// `[suffix, meaning]` pairs for number literal suffixes.
    #[serde(default)]
    pub number_suffixes: Vec<(String, String)>,
}

/// Modifier detection configuration.
#[derive(Deserialize, Default)]
pub struct ModifiersSection {
    /// `[keyword, field_name]` pairs for modifier detection.
    #[serde(default)]
    pub map: Vec<(String, String)>,

    /// Node kinds that carry modifier/qualifier keywords.
    #[serde(default)]
    pub node_kinds: Vec<String>,

    /// Keywords for static storage duration.
    #[serde(default)]
    pub static_storage_keywords: Vec<String>,
}

/// Visibility / access control configuration.
#[derive(Deserialize, Default)]
pub struct VisibilitySection {
    /// `[keyword, visibility]` pairs.
    #[serde(default)]
    pub keywords: Vec<(String, String)>,

    /// `[type_kind, default_visibility]` pairs.
    #[serde(default)]
    pub default_by_type: Vec<(String, String)>,
}

/// Comment style detection.
#[derive(Deserialize, Default)]
pub struct CommentsSection {
    /// `[prefix, style]` pairs, checked in order.
    #[serde(default)]
    pub prefixes: Vec<(String, String)>,
}

/// Language capability flags.
#[derive(Deserialize, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct CapabilitiesSection {
    /// Has `goto` statements.
    #[serde(default)]
    pub has_goto: bool,

    /// Has `++`/`--` operators.
    #[serde(default)]
    pub has_increment_decrement: bool,

    pub has_implicit_truthiness: bool,

    /// Whether function parameters and the function body share the same variable
    /// scope (Python-style).  Set to `true` for Python and similar languages.
    /// Default: `false` (C++/Rust style — params are an outer scope).
    #[serde(default)]
    pub params_share_body_scope: bool,
}

/// Preprocessor / compiler guard configuration.
#[derive(Deserialize, Default)]
pub struct GuardsSection {
    /// Node kinds that open a guarded block (e.g. `preproc_ifdef`, `preproc_if`).
    #[serde(default)]
    pub block_guard_kinds: Vec<String>,

    /// Node kinds representing `#elif` branches.
    #[serde(default)]
    pub elif_kinds: Vec<String>,

    /// Node kinds representing `#else` branches.
    #[serde(default)]
    pub else_kinds: Vec<String>,

    /// Grammar field name for the guard condition expression.
    #[serde(default)]
    pub condition_field: String,

    /// Grammar field name for the macro identifier child in `ifdef`/`ifndef`.
    #[serde(default)]
    pub name_field: String,

    /// Token text that marks the negated guard variant (e.g. `"#ifndef"`).
    #[serde(default)]
    pub negate_ifdef_variant: String,

    /// Attribute name for item-level guards (e.g. `"cfg"` for Rust).
    #[serde(default)]
    pub item_guard_attribute: String,

    /// Regex for file-level guard comments (e.g. Go build tags).
    #[serde(default)]
    pub file_guard_pattern: String,

    /// Regex for OS/arch extraction from file suffix.
    #[serde(default)]
    pub file_guard_suffix_pattern: String,

    /// Node kinds for comptime conditional blocks (e.g. Zig).
    #[serde(default)]
    pub comptime_guard_kinds: Vec<String>,

    /// Regex patterns for compile-time guard detection in `if` conditions.
    #[serde(default)]
    pub builtin_guard_patterns: Vec<String>,

    /// Regex patterns for heuristic environment guards.
    #[serde(default)]
    pub env_guard_patterns: Vec<String>,

    /// Regex for directory-based source set extraction (Kotlin).
    #[serde(default)]
    pub source_set_pattern: String,
}

// -----------------------------------------------------------------------
// MacrosSection
// -----------------------------------------------------------------------

/// Macro expansion configuration for a language.
///
/// All fields default to empty/blank so languages without macro support
/// can omit the entire `"macros"` section from their JSON config.
#[derive(Deserialize, Default)]
pub struct MacrosSection {
    /// Token texts that prefix macro definitions (e.g. `["#define"]` for C/C++).
    #[serde(default)]
    pub def_markers: Vec<String>,

    /// Raw tree-sitter kinds for macro definitions
    /// (e.g. `["preproc_function_def", "preproc_def"]` for C/C++).
    #[serde(default)]
    pub def_kinds: Vec<String>,

    /// Raw kind for macro invocations (e.g. `"macro_invocation"` for C++).
    #[serde(default)]
    pub invocation_kind: String,

    /// Grammar field name for the macro parameter list.
    #[serde(default)]
    pub parameters_field: String,

    /// Grammar field name for the macro body/value.
    #[serde(default)]
    pub value_field: String,
}

// -----------------------------------------------------------------------
// Default value functions for #[serde(default = "...")]
// Default value functions for #[serde(default = "...")]
// -----------------------------------------------------------------------

fn default_scope_separator() -> String {
    ".".to_owned()
}

fn default_identifier() -> String {
    "identifier".to_owned()
}

fn default_parenthesized() -> String {
    "parenthesized_expression".to_owned()
}

// -----------------------------------------------------------------------
// Conversion: LanguageConfigJson → LanguageConfig
// -----------------------------------------------------------------------

impl LanguageConfigJson {
    /// Parse a JSON byte slice into a [`LanguageConfigJson`].
    ///
    /// # Errors
    ///
    /// Returns a `serde_json::Error` if the JSON is malformed or missing
    /// required fields.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Convert this JSON config into a [`LanguageConfig`].
    ///
    /// Extracts the `digit_separator` from a single-character string into
    /// `Option<char>` (as expected by `LanguageConfig`).
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn into_language_config(self) -> LanguageConfig {
        let digit_separator = self
            .literals
            .digit_separator
            .as_deref()
            .and_then(|s| s.chars().next());

        LanguageConfig {
            root_node_kind: self.syntax.root_node,
            scope_separator: self.syntax.scope_separator,
            function_raw_kinds: self.definitions.function_kinds,
            type_raw_kinds: self.definitions.type_kinds,
            definition_raw_kinds: self.definitions.definition_kinds,
            declaration_raw_kinds: self.definitions.declaration_kinds,
            field_raw_kinds: self.definitions.field_kinds,
            parameter_raw_kind: self.definitions.parameter_kind,
            member_body_raw_kind: self.definitions.member_body_kind,
            member_raw_kinds: self.definitions.member_kinds,
            owner_container_raw_kinds: self.definitions.owner_container_kinds,
            comment_raw_kind: self.syntax.comment,
            number_literal_raw_kinds: self.literals.number_kinds,
            digit_separator,
            number_suffixes: self.literals.number_suffixes,
            control_flow_raw_kinds: self.control_flow.kinds,
            switch_raw_kinds: self.control_flow.switch_kinds,
            null_literals: self.literals.null_values,
            boolean_literals: self.literals.boolean_values,
            doc_comment_prefixes: self.comments.prefixes,
            modifier_map: self.modifiers.map,
            modifier_node_kinds: self.modifiers.node_kinds,
            visibility_keywords: self.visibility.keywords,
            visibility_default_by_type: self.visibility.default_by_type,
            cast_kinds: self.casts,
            has_goto: self.capabilities.has_goto,
            has_increment_decrement: self.capabilities.has_increment_decrement,
            has_implicit_truthiness: self.capabilities.has_implicit_truthiness,
            params_share_body_scope: self.capabilities.params_share_body_scope,
            decorator_raw_kind: self.types.decorator,
            skip_node_kinds: self.syntax.skip_node_kinds,
            usage_node_kinds: self.syntax.usage_node_kinds,
            statement_boundary_kinds: self.syntax.statement_boundary_kinds,
            declarator_field_name: self.syntax.declarator_field,
            function_declarator_kind: self.syntax.function_declarator,
            parameter_list_raw_kind: self.syntax.parameter_list,
            identifier_raw_kind: self.syntax.identifier,
            assignment_raw_kinds: self.statements.assignment_kinds,
            update_raw_kinds: self.statements.update_kinds,
            init_declarator_raw_kind: self.syntax.init_declarator,
            block_raw_kind: self.syntax.block,
            scope_creating_raw_kinds: self.syntax.scope_creating_kinds,
            branch_raw_kinds: self.control_flow.branch_kinds,
            loop_raw_kinds: self.control_flow.loop_kinds,
            exception_handler_raw_kinds: self.control_flow.exception_handler_kinds,
            block_scoped_declaration_raw_kinds: self.definitions.block_scoped_declaration_kinds,
            return_statement_raw_kind: self.statements.return_kind,
            address_of_expression_raw_kind: self.expressions.address_of,
            address_of_operator: self.expressions.address_of_operator,
            array_declarator_raw_kind: self.types.array_declarator,
            static_storage_keywords: self.modifiers.static_storage_keywords,
            case_statement_raw_kind: self.statements.case,
            break_statement_raw_kind: self.statements.break_kind,
            call_expression_raw_kind: self.expressions.call,
            goto_statement_raw_kind: self.statements.goto,
            string_literal_raw_kinds: self.literals.string_kinds,
            throw_statement_raw_kind: self.statements.throw,
            template_declaration_raw_kind: self.types.template_declaration,
            enumerator_raw_kind: self.types.enumerator,
            binary_expression_raw_kind: self.expressions.binary,
            logical_expression_raw_kind: self.expressions.logical,
            type_descriptor_raw_kind: self.types.type_descriptor,
            template_argument_list_raw_kind: self.types.template_argument_list,
            shift_expression_raw_kinds: self.expressions.shift_kinds,
            compound_assignment_raw_kind: self.expressions.compound_assignment,
            for_style_map: self.control_flow.for_style_map,
            template_misparse_raw_kinds: self.types.template_misparse_kinds,
            field_expression_raw_kind: self.expressions.field_access,
            subscript_expression_raw_kind: self.expressions.subscript,
            unary_expression_raw_kind: self.expressions.unary,
            parenthesized_expression_raw_kind: self.expressions.parenthesized,
            condition_clause_raw_kind: self.control_flow.condition_clause,
            comma_expression_raw_kind: self.expressions.comma,
            char_literal_raw_kind: self.literals.char_kind,
            block_guard_kinds: self.guards.block_guard_kinds,
            elif_kinds: self.guards.elif_kinds,
            else_kinds: self.guards.else_kinds,
            guard_condition_field: self.guards.condition_field,
            guard_name_field: self.guards.name_field,
            negate_ifdef_variant: self.guards.negate_ifdef_variant,
            item_guard_attribute: self.guards.item_guard_attribute,
            file_guard_pattern: self.guards.file_guard_pattern,
            file_guard_suffix_pattern: self.guards.file_guard_suffix_pattern,
            comptime_guard_kinds: self.guards.comptime_guard_kinds,
            builtin_guard_patterns: self.guards.builtin_guard_patterns,
            env_guard_patterns: self.guards.env_guard_patterns,
            source_set_pattern: self.guards.source_set_pattern,
            macro_def_markers: self.macros.def_markers,
            macro_def_kinds: self.macros.def_kinds,
            macro_invocation_kind: self.macros.invocation_kind,
            macro_parameters_field: self.macros.parameters_field,
            macro_value_field: self.macros.value_field,
            nested_function_body_raw_kinds: self.definitions.nested_function_body_kinds,
            constant_def_parent_raw_kinds: self.definitions.constant_def_parent_kinds,
            kind_map: self.kind_map,
        }
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cpp_json() {
        let json = include_bytes!("../../../forgeql-lang-cpp/config/cpp.json");
        let parsed = LanguageConfigJson::from_json_bytes(json).expect("cpp.json should parse");
        assert_eq!(parsed.language.name, "cpp");
        assert_eq!(parsed.language.extensions.len(), 8);
        assert_eq!(parsed.syntax.root_node, "translation_unit");
        assert_eq!(parsed.syntax.scope_separator, "::");
        assert_eq!(
            parsed.definitions.function_kinds,
            vec!["function_definition"]
        );
        assert!(parsed.capabilities.has_goto);
        assert_eq!(
            parsed.kind_map.get("function_definition"),
            Some(&"function".to_owned())
        );
    }

    #[test]
    fn convert_cpp_json_to_config() {
        let json = include_bytes!("../../../forgeql-lang-cpp/config/cpp.json");
        let parsed = LanguageConfigJson::from_json_bytes(json).expect("cpp.json should parse");
        let config = parsed.into_language_config();

        assert_eq!(config.root_kind(), "translation_unit");
        assert_eq!(config.scope_sep(), "::");
        assert!(config.is_function_kind("function_definition"));
        assert!(config.is_type_kind("class_specifier"));
        assert!(!config.function_kinds().is_empty());
        assert_eq!(
            config.kind_map_lookup("function_definition"),
            Some("function")
        );
        assert_eq!(config.kind_map_lookup("if_statement"), Some("if"));
        assert_eq!(config.kind_map_lookup("unknown_thing"), None);
    }

    #[test]
    fn defaults_applied_for_missing_fields() {
        // When individual fields are present but empty, serde field defaults apply.
        let minimal_json = r#"{
            "language": {
                "name": "test",
                "extensions": ["test"]
            },
            "syntax": {},
            "expressions": {},
            "kind_map": {}
        }"#;
        let parsed: LanguageConfigJson =
            serde_json::from_str(minimal_json).expect("minimal JSON should parse");
        assert_eq!(parsed.syntax.identifier, "identifier");
        assert_eq!(parsed.syntax.scope_separator, ".");
        assert_eq!(parsed.expressions.parenthesized, "parenthesized_expression");
        assert!(!parsed.capabilities.has_goto);
        assert!(parsed.casts.is_empty());
    }
}
