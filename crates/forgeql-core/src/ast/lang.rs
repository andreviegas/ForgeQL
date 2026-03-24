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

    // -- recursion detection (recursion enricher) --
    /// Raw kind for function/method call expressions
    /// (e.g. `"call_expression"` for C++/Java/TS, `"call"` for Python).
    /// Empty string if the language has no call expression kind.
    pub call_expression_raw_kind: &'static str,

    // -- metrics (body-level counting) --
    /// Raw kind for goto statements (e.g. `"goto_statement"` for C++).
    /// Empty string if the language has no goto.
    pub goto_statement_raw_kind: &'static str,

    /// Raw kinds for string/char literal nodes
    /// (e.g. `["string_literal", "char_literal"]` for C++,
    /// `["string_literal", "raw_string_literal"]` for Rust).
    pub string_literal_raw_kinds: &'static [&'static str],

    /// Raw kind for throw/raise statements
    /// (e.g. `"throw_statement"` for C++, `""` for Rust which uses `panic!`).
    pub throw_statement_raw_kind: &'static str,

    // -- show/display --
    /// Raw kind for template/generic declarations wrapping a function or type
    /// (e.g. `"template_declaration"` for C++, `""` for Rust).
    pub template_declaration_raw_kind: &'static str,

    /// Raw kind for enumerator/variant members inside an enum body
    /// (e.g. `"enumerator"` for C++, `"enum_variant"` for Rust).
    pub enumerator_raw_kind: &'static str,

    // -- expression analysis (control-flow, redundancy) --
    /// Raw kind for binary arithmetic/comparison expressions
    /// (e.g. `"binary_expression"` — common across most tree-sitter grammars).
    pub binary_expression_raw_kind: &'static str,

    /// Raw kind for logical `&&`/`||` expressions when the grammar has a
    /// separate node kind (e.g. `"logical_expression"` for C++).
    /// Empty string if logical operators produce `binary_expression` nodes.
    pub logical_expression_raw_kind: &'static str,

    // -- cast type extraction --
    /// Raw kind for type-descriptor nodes inside cast expressions
    /// (e.g. `"type_descriptor"` for C++).  Empty if not applicable.
    pub type_descriptor_raw_kind: &'static str,

    /// Raw kind for template/generic argument lists
    /// (e.g. `"template_argument_list"` for C++).  Empty if not applicable.
    pub template_argument_list_raw_kind: &'static str,

    // -- operators --
    /// Raw kinds that may contain shift operators (`<<`, `>>`).
    /// (e.g. `["shift_expression"]` for C++ — may also include
    /// `binary_expression` for grammars that don't distinguish shifts).
    pub shift_expression_raw_kinds: &'static [&'static str],

    /// Synthetic raw kind assigned to compound-assignment rows created by the
    /// operator enricher (e.g. `"compound_assignment"`).
    pub compound_assignment_raw_kind: &'static str,

    // -- for-loop style disambiguation --
    /// (`raw_kind`, `style_name`) pairs for for-loop style detection.
    /// (e.g. `[("for_statement", "traditional"), ("for_range_loop", "range")]`
    /// for C++, `[("for_expression", "range")]` for Rust).
    pub for_style_map: &'static [(&'static str, &'static str)],

    // -- template/generic misparse detection --
    /// Raw kinds whose presence signals a tree-sitter template/generic
    /// misparse (e.g. `>=` mis-parsed as `>` + `=` in C++).
    /// (e.g. `["template_function", "template_type", "template_argument_list"]`).
    /// Empty slice for languages without this issue.
    pub template_misparse_raw_kinds: &'static [&'static str],

    // -- skeleton condition normalization (control-flow enricher) --
    /// Raw kind for field/member access expressions
    /// (e.g. `"field_expression"` for C++/Rust).
    pub field_expression_raw_kind: &'static str,

    /// Raw kind for array/index subscript expressions
    /// (e.g. `"subscript_expression"` for C++, `"index_expression"` for Rust).
    pub subscript_expression_raw_kind: &'static str,

    /// Raw kind for unary expressions (`!x`, `-x`, etc.)
    /// (e.g. `"unary_expression"` — common across most grammars).
    pub unary_expression_raw_kind: &'static str,

    /// Raw kind for parenthesized expressions
    /// (e.g. `"parenthesized_expression"` — common across most grammars).
    pub parenthesized_expression_raw_kind: &'static str,

    /// Raw kind for the condition clause wrapper node
    /// (e.g. `"condition_clause"` for C++).  Empty if not applicable.
    pub condition_clause_raw_kind: &'static str,

    /// Raw kind for comma expressions
    /// (e.g. `"comma_expression"` for C++).  Empty if not applicable.
    pub comma_expression_raw_kind: &'static str,

    /// Raw kind for character literals
    /// (e.g. `"char_literal"` for C++).  Empty if not applicable.
    pub char_literal_raw_kind: &'static str,
}

// -----------------------------------------------------------------------
// LanguageConfig — query methods
//
// These methods encapsulate field access patterns used by enrichers and
// other consumers.  During migration, consumers will switch from direct
// field access (`config.function_raw_kinds.contains(…)`) to these
// methods (`config.is_function_kind(…)`).  Once migration is complete,
// the fields will become private and internal storage can change freely.
// -----------------------------------------------------------------------

impl LanguageConfig {
    // -- kind membership tests (slice fields) --------------------------

    /// Is this a function/method definition kind?
    #[must_use]
    pub fn is_function_kind(&self, kind: &str) -> bool {
        self.function_raw_kinds.contains(&kind)
    }

    /// Is this a type definition kind (class, struct, enum, etc.)?
    #[must_use]
    pub fn is_type_kind(&self, kind: &str) -> bool {
        self.type_raw_kinds.contains(&kind)
    }

    /// Is this any definition kind (for `has_doc` checks)?
    #[must_use]
    pub fn is_definition_kind(&self, kind: &str) -> bool {
        self.definition_raw_kinds.contains(&kind)
    }

    /// Is this a variable/const declaration kind?
    #[must_use]
    pub fn is_declaration_kind(&self, kind: &str) -> bool {
        self.declaration_raw_kinds.contains(&kind)
    }

    /// Is this a member/field declaration kind?
    #[must_use]
    pub fn is_field_kind(&self, kind: &str) -> bool {
        self.field_raw_kinds.contains(&kind)
    }

    /// Is this a member kind inside a type body?
    #[must_use]
    pub fn is_member_kind(&self, kind: &str) -> bool {
        self.member_raw_kinds.contains(&kind)
    }

    /// Is this a number literal kind?
    #[must_use]
    pub fn is_number_literal_kind(&self, kind: &str) -> bool {
        self.number_literal_raw_kinds.contains(&kind)
    }

    /// Is this a control-flow statement kind?
    #[must_use]
    pub fn is_control_flow_kind(&self, kind: &str) -> bool {
        self.control_flow_raw_kinds.contains(&kind)
    }

    /// Is this a switch/match statement kind?
    #[must_use]
    pub fn is_switch_kind(&self, kind: &str) -> bool {
        self.switch_raw_kinds.contains(&kind)
    }

    /// Does this kind carry modifier/qualifier keywords?
    #[must_use]
    pub fn is_modifier_node_kind(&self, kind: &str) -> bool {
        self.modifier_node_kinds.contains(&kind)
    }

    /// Is this an assignment expression kind?
    #[must_use]
    pub fn is_assignment_kind(&self, kind: &str) -> bool {
        self.assignment_raw_kinds.contains(&kind)
    }

    /// Is this an update/increment expression kind?
    #[must_use]
    pub fn is_update_kind(&self, kind: &str) -> bool {
        self.update_raw_kinds.contains(&kind)
    }

    /// Is this a string/char literal kind?
    #[must_use]
    pub fn is_string_literal_kind(&self, kind: &str) -> bool {
        self.string_literal_raw_kinds.contains(&kind)
    }

    /// Should this kind be skipped during indexing?
    #[must_use]
    pub fn is_skip_kind(&self, kind: &str) -> bool {
        self.skip_node_kinds.contains(&kind)
    }

    /// Is this a usage-site identifier kind?
    #[must_use]
    pub fn is_usage_node_kind(&self, kind: &str) -> bool {
        self.usage_node_kinds.contains(&kind)
    }

    /// Is this a shift expression kind?
    #[must_use]
    pub fn is_shift_expression_kind(&self, kind: &str) -> bool {
        self.shift_expression_raw_kinds.contains(&kind)
    }

    /// Is this a template/generic misparse indicator kind?
    #[must_use]
    pub fn is_template_misparse_kind(&self, kind: &str) -> bool {
        self.template_misparse_raw_kinds.contains(&kind)
    }

    /// Is this text a null literal for this language?
    #[must_use]
    pub fn is_null_literal(&self, text: &str) -> bool {
        self.null_literals.contains(&text)
    }

    /// Is this text a boolean literal for this language?
    #[must_use]
    pub fn is_boolean_literal(&self, text: &str) -> bool {
        self.boolean_literals.contains(&text)
    }

    /// Is this text a static-storage keyword?
    #[must_use]
    pub fn is_static_storage_keyword(&self, text: &str) -> bool {
        self.static_storage_keywords.contains(&text)
    }

    // -- single-kind equality tests ------------------------------------

    /// Is this the root node kind for the grammar?
    #[must_use]
    pub fn is_root_kind(&self, kind: &str) -> bool {
        self.root_node_kind == kind
    }

    /// Is this a parameter declaration kind?
    #[must_use]
    pub fn is_parameter_kind(&self, kind: &str) -> bool {
        self.parameter_raw_kind == kind
    }

    /// Is this the member-body (type body) kind?
    #[must_use]
    pub fn is_member_body_kind(&self, kind: &str) -> bool {
        self.member_body_raw_kind == kind
    }

    /// Is this a comment kind?
    #[must_use]
    pub fn is_comment_kind(&self, kind: &str) -> bool {
        self.comment_raw_kind == kind
    }

    /// Is this a block/compound-statement kind?
    #[must_use]
    pub fn is_block_kind(&self, kind: &str) -> bool {
        self.block_raw_kind == kind
    }

    /// Is this an identifier kind?
    #[must_use]
    pub fn is_identifier_kind(&self, kind: &str) -> bool {
        self.identifier_raw_kind == kind
    }

    /// Is this an init-declarator wrapper kind?
    #[must_use]
    pub fn is_init_declarator_kind(&self, kind: &str) -> bool {
        self.init_declarator_raw_kind == kind
    }

    /// Is this a return-statement kind?
    #[must_use]
    pub fn is_return_statement_kind(&self, kind: &str) -> bool {
        self.return_statement_raw_kind == kind
    }

    /// Is this an address-of expression kind?
    #[must_use]
    pub fn is_address_of_expression_kind(&self, kind: &str) -> bool {
        self.address_of_expression_raw_kind == kind
    }

    /// Is this a case/default label kind?
    #[must_use]
    pub fn is_case_statement_kind(&self, kind: &str) -> bool {
        self.case_statement_raw_kind == kind
    }

    /// Is this a break-statement kind?
    #[must_use]
    pub fn is_break_statement_kind(&self, kind: &str) -> bool {
        self.break_statement_raw_kind == kind
    }

    /// Is this a call-expression kind?
    #[must_use]
    pub fn is_call_expression_kind(&self, kind: &str) -> bool {
        self.call_expression_raw_kind == kind
    }

    /// Is this a goto-statement kind?
    #[must_use]
    pub fn is_goto_statement_kind(&self, kind: &str) -> bool {
        self.goto_statement_raw_kind == kind
    }

    /// Is this a throw-statement kind?
    #[must_use]
    pub fn is_throw_statement_kind(&self, kind: &str) -> bool {
        self.throw_statement_raw_kind == kind
    }

    /// Is this a template/generic declaration kind?
    #[must_use]
    pub fn is_template_declaration_kind(&self, kind: &str) -> bool {
        self.template_declaration_raw_kind == kind
    }

    /// Is this an enumerator/variant kind?
    #[must_use]
    pub fn is_enumerator_kind(&self, kind: &str) -> bool {
        self.enumerator_raw_kind == kind
    }

    /// Is this a binary expression kind?
    #[must_use]
    pub fn is_binary_expression_kind(&self, kind: &str) -> bool {
        self.binary_expression_raw_kind == kind
    }

    /// Is this a logical expression kind?
    /// Returns `false` if the language has no separate logical expression node.
    #[must_use]
    pub fn is_logical_expression_kind(&self, kind: &str) -> bool {
        !self.logical_expression_raw_kind.is_empty() && self.logical_expression_raw_kind == kind
    }

    /// Is this a type-descriptor kind?
    #[must_use]
    pub fn is_type_descriptor_kind(&self, kind: &str) -> bool {
        self.type_descriptor_raw_kind == kind
    }

    /// Is this a template-argument-list kind?
    #[must_use]
    pub fn is_template_argument_list_kind(&self, kind: &str) -> bool {
        self.template_argument_list_raw_kind == kind
    }

    /// Is this a field/member-access expression kind?
    #[must_use]
    pub fn is_field_expression_kind(&self, kind: &str) -> bool {
        self.field_expression_raw_kind == kind
    }

    /// Is this a subscript/index expression kind?
    #[must_use]
    pub fn is_subscript_expression_kind(&self, kind: &str) -> bool {
        self.subscript_expression_raw_kind == kind
    }

    /// Is this a unary expression kind?
    #[must_use]
    pub fn is_unary_expression_kind(&self, kind: &str) -> bool {
        self.unary_expression_raw_kind == kind
    }

    /// Is this a parenthesized expression kind?
    #[must_use]
    pub fn is_parenthesized_expression_kind(&self, kind: &str) -> bool {
        self.parenthesized_expression_raw_kind == kind
    }

    /// Is this a condition-clause wrapper kind?
    #[must_use]
    pub fn is_condition_clause_kind(&self, kind: &str) -> bool {
        self.condition_clause_raw_kind == kind
    }

    /// Is this a comma-expression kind?
    #[must_use]
    pub fn is_comma_expression_kind(&self, kind: &str) -> bool {
        self.comma_expression_raw_kind == kind
    }

    /// Is this a char-literal kind?
    #[must_use]
    pub fn is_char_literal_kind(&self, kind: &str) -> bool {
        self.char_literal_raw_kind == kind
    }

    /// Is this a parameter-list kind?
    #[must_use]
    pub fn is_parameter_list_kind(&self, kind: &str) -> bool {
        self.parameter_list_raw_kind == kind
    }

    /// Is this an array-declarator kind?
    #[must_use]
    pub fn is_array_declarator_kind(&self, kind: &str) -> bool {
        self.array_declarator_raw_kind == kind
    }

    // -- feature capability checks -------------------------------------

    /// Does this language have address-of expressions?
    #[must_use]
    pub const fn has_address_of(&self) -> bool {
        !self.address_of_expression_raw_kind.is_empty()
    }

    /// Does this language have call expressions?
    #[must_use]
    pub const fn has_call_expression(&self) -> bool {
        !self.call_expression_raw_kind.is_empty()
    }

    /// Does this language have case/switch labels?
    #[must_use]
    pub const fn has_case_statement(&self) -> bool {
        !self.case_statement_raw_kind.is_empty()
    }

    /// Does this language have comments?
    #[must_use]
    pub const fn has_comment(&self) -> bool {
        !self.comment_raw_kind.is_empty()
    }

    /// Does this language have static-storage keywords?
    #[must_use]
    pub const fn has_static_storage(&self) -> bool {
        !self.static_storage_keywords.is_empty()
    }

    /// Does this language have array declarators?
    #[must_use]
    pub const fn has_array_declarator(&self) -> bool {
        !self.array_declarator_raw_kind.is_empty()
    }

    /// Does this language have a separate logical expression node?
    #[must_use]
    pub const fn has_logical_expression(&self) -> bool {
        !self.logical_expression_raw_kind.is_empty()
    }

    /// Does this language have template/generic declarations?
    #[must_use]
    pub const fn has_template_declaration(&self) -> bool {
        !self.template_declaration_raw_kind.is_empty()
    }

    /// Does this language have enumerator/variant members?
    #[must_use]
    pub const fn has_enumerator(&self) -> bool {
        !self.enumerator_raw_kind.is_empty()
    }

    // -- value accessors -----------------------------------------------

    /// Scope-resolution separator (e.g. `"::"` for C++, `"."` for others).
    #[must_use]
    pub const fn scope_sep(&self) -> &str {
        self.scope_separator
    }

    /// Grammar field name for the declarator child.
    #[must_use]
    pub const fn declarator_field(&self) -> &str {
        self.declarator_field_name
    }

    /// Raw kind for function-type declarators.
    #[must_use]
    pub const fn function_declarator(&self) -> &str {
        self.function_declarator_kind
    }

    /// Textual operator for address-of (e.g. `"&"`).
    #[must_use]
    pub const fn address_of_op(&self) -> &str {
        self.address_of_operator
    }

    // -- lookup methods ------------------------------------------------

    /// Look up cast info by raw kind.  Returns `(style, safety)`.
    #[must_use]
    pub fn cast_info(&self, kind: &str) -> Option<(&str, &str)> {
        self.cast_kinds
            .iter()
            .find(|(rk, _, _)| *rk == kind)
            .map(|(_, style, safety)| (*style, *safety))
    }

    /// Look up for-loop style by raw kind.
    #[must_use]
    pub fn for_style(&self, kind: &str) -> Option<&str> {
        self.for_style_map
            .iter()
            .find(|(rk, _)| *rk == kind)
            .map(|(_, style)| *style)
    }

    /// Look up the enrichment field name for a modifier keyword.
    #[must_use]
    pub fn modifier_field_for(&self, keyword: &str) -> Option<&str> {
        self.modifier_map
            .iter()
            .find(|(kw, _)| *kw == keyword)
            .map(|(_, field)| *field)
    }

    /// Look up visibility for an access-specifier keyword.
    #[must_use]
    pub fn visibility_for_keyword(&self, keyword: &str) -> Option<&str> {
        self.visibility_keywords
            .iter()
            .find(|(kw, _)| *kw == keyword)
            .map(|(_, vis)| *vis)
    }

    /// Look up default visibility for a type kind.
    #[must_use]
    pub fn default_visibility_for_type(&self, type_kind: &str) -> Option<&str> {
        self.visibility_default_by_type
            .iter()
            .find(|(rk, _)| *rk == type_kind)
            .map(|(_, vis)| *vis)
    }

    /// Detect comment style from comment text (first-prefix-wins).
    #[must_use]
    pub fn detect_comment_style(&self, text: &str) -> Option<&str> {
        self.doc_comment_prefixes
            .iter()
            .find(|(prefix, _)| text.starts_with(prefix))
            .map(|(_, style)| *style)
    }

    /// Look up meaning for a number literal suffix.
    #[must_use]
    pub fn number_suffix_meaning(&self, suffix: &str) -> Option<&str> {
        self.number_suffixes
            .iter()
            .find(|(s, _)| *s == suffix)
            .map(|(_, meaning)| *meaning)
    }
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

    /// Return all language configs from registered languages.
    #[must_use]
    pub fn configs(&self) -> Vec<&'static LanguageConfig> {
        self.languages().iter().map(|l| l.config()).collect()
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

    call_expression_raw_kind: "call_expression",

    goto_statement_raw_kind: "goto_statement",
    string_literal_raw_kinds: &["string_literal", "char_literal"],
    throw_statement_raw_kind: "throw_statement",

    template_declaration_raw_kind: "template_declaration",
    enumerator_raw_kind: "enumerator",

    binary_expression_raw_kind: "binary_expression",
    logical_expression_raw_kind: "logical_expression",

    type_descriptor_raw_kind: "type_descriptor",
    template_argument_list_raw_kind: "template_argument_list",

    shift_expression_raw_kinds: &["shift_expression"],
    compound_assignment_raw_kind: "compound_assignment",

    for_style_map: &[
        ("for_statement", "traditional"),
        ("for_range_loop", "range"),
    ],

    template_misparse_raw_kinds: &[
        "template_function",
        "template_type",
        "template_argument_list",
    ],

    field_expression_raw_kind: "field_expression",
    subscript_expression_raw_kind: "subscript_expression",
    unary_expression_raw_kind: "unary_expression",
    parenthesized_expression_raw_kind: "parenthesized_expression",
    condition_clause_raw_kind: "condition_clause",
    comma_expression_raw_kind: "comma_expression",
    char_literal_raw_kind: "char_literal",
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
        assert!(cfg.is_skip_kind("preproc_else"));
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
        // cast_info
        assert_eq!(
            cfg.cast_info("cast_expression"),
            Some(("c_style", "unsafe"))
        );
        assert_eq!(
            cfg.cast_info("static_cast_expression"),
            Some(("static_cast", "safe"))
        );
        assert_eq!(cfg.cast_info("unknown"), None);
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
