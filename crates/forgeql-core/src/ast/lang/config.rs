//! Query methods for [`super::LanguageConfig`].
use super::{BlockGroupSpec, LanguageConfig};
impl LanguageConfig {
    // -- kind membership tests (slice fields) --------------------------

    /// Is this a function/method definition kind?
    #[must_use]
    pub fn is_function_kind(&self, kind: &str) -> bool {
        self.function_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this a type definition kind (class, struct, enum, etc.)?
    #[must_use]
    pub fn is_type_kind(&self, kind: &str) -> bool {
        self.type_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this any definition kind (for `has_doc` checks)?
    #[must_use]
    pub fn is_definition_kind(&self, kind: &str) -> bool {
        self.definition_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this a variable/const declaration kind?
    #[must_use]
    pub fn is_declaration_kind(&self, kind: &str) -> bool {
        self.declaration_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this a member/field declaration kind?
    #[must_use]
    pub fn is_field_kind(&self, kind: &str) -> bool {
        self.field_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this a member kind inside a type body?
    #[must_use]
    pub fn is_member_kind(&self, kind: &str) -> bool {
        self.member_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this a number literal kind?
    #[must_use]
    pub fn is_number_literal_kind(&self, kind: &str) -> bool {
        self.number_literal_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this a control-flow statement kind?
    #[must_use]
    pub fn is_control_flow_kind(&self, kind: &str) -> bool {
        self.control_flow_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this a switch/match statement kind?
    #[must_use]
    pub fn is_switch_kind(&self, kind: &str) -> bool {
        self.switch_raw_kinds.iter().any(|s| s == kind)
    }

    /// Does this kind carry modifier/qualifier keywords?
    #[must_use]
    pub fn is_modifier_node_kind(&self, kind: &str) -> bool {
        self.modifier_node_kinds.iter().any(|s| s == kind)
    }

    /// Is this an assignment expression kind?
    #[must_use]
    pub fn is_assignment_kind(&self, kind: &str) -> bool {
        self.assignment_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this an update/increment expression kind?
    #[must_use]
    pub fn is_update_kind(&self, kind: &str) -> bool {
        self.update_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this a string/char literal kind?
    #[must_use]
    pub fn is_string_literal_kind(&self, kind: &str) -> bool {
        self.string_literal_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this the raw kind for inline string content (child of a string literal)?
    #[must_use]
    pub fn is_string_content_kind(&self, kind: &str) -> bool {
        !self.string_content_raw_kind.is_empty() && self.string_content_raw_kind == kind
    }

    /// Is this a string/char literal kind whose subtree is guaranteed to contain
    /// only terminal tokens (no embedded expression children like f-string
    /// interpolations)?  Grammars that set `string_content_raw_kind` use a
    /// dedicated leaf token for string content, making their string literals
    /// opaque from an enrichment perspective.  Languages like Python that omit
    /// `string_content_raw_kind` may embed real expressions (f-strings) and
    /// must still be descended into.
    #[must_use]
    pub fn is_opaque_string_kind(&self, kind: &str) -> bool {
        !self.string_content_raw_kind.is_empty()
            && (self.is_string_literal_kind(kind) || self.is_char_literal_kind(kind))
    }

    /// Should this kind be skipped during indexing?
    #[must_use]
    pub fn is_skip_kind(&self, kind: &str) -> bool {
        self.skip_node_kinds.iter().any(|s| s == kind)
    }

    /// Is this a guard-opening block kind (e.g. `preproc_ifdef`, `preproc_if`)?
    #[must_use]
    pub fn is_block_guard_kind(&self, kind: &str) -> bool {
        self.block_guard_kinds.iter().any(|s| s == kind)
    }

    /// Is this an `#elif` guard branch kind?
    #[must_use]
    pub fn is_elif_kind(&self, kind: &str) -> bool {
        self.elif_kinds.iter().any(|s| s == kind)
    }

    /// Is this an `#else` guard branch kind?
    #[must_use]
    pub fn is_else_kind(&self, kind: &str) -> bool {
        self.else_kinds.iter().any(|s| s == kind)
    }

    /// Grammar field name for the guard condition expression.
    #[must_use]
    pub fn guard_condition_field(&self) -> &str {
        &self.guard_condition_field
    }

    /// Grammar field name for the macro identifier child in `ifdef`/`ifndef`.
    #[must_use]
    pub fn guard_name_field(&self) -> &str {
        &self.guard_name_field
    }

    /// Attribute name for item-level guards (e.g. `"cfg"` for Rust).
    #[must_use]
    pub fn item_guard_attribute(&self) -> &str {
        &self.item_guard_attribute
    }

    /// Returns `true` if this language has any guard configuration set.
    #[must_use]
    pub const fn has_guard_support(&self) -> bool {
        !self.block_guard_kinds.is_empty()
            || !self.item_guard_attribute.is_empty()
            || !self.file_guard_pattern.is_empty()
            || !self.comptime_guard_kinds.is_empty()
            || !self.env_guard_patterns.is_empty()
    }

    /// Token text for the negated guard variant (e.g. `"#ifndef"`).
    #[must_use]
    pub fn negate_ifdef_variant(&self) -> &str {
        &self.negate_ifdef_variant
    }

    /// Regex for OS/arch extraction from file suffix.
    #[must_use]
    pub fn file_guard_suffix_pattern(&self) -> &str {
        &self.file_guard_suffix_pattern
    }

    /// Regex patterns for compile-time guard detection in `if` conditions.
    #[must_use]
    pub fn builtin_guard_patterns(&self) -> &[String] {
        &self.builtin_guard_patterns
    }

    /// Regex patterns for heuristic environment guard detection in `if` conditions.
    #[must_use]
    pub fn env_guard_patterns(&self) -> &[String] {
        &self.env_guard_patterns
    }

    /// Regex for directory-based source set extraction.
    #[must_use]
    pub fn source_set_pattern(&self) -> &str {
        &self.source_set_pattern
    }

    // -- macro config accessors ----------------------------------------

    /// Token texts that prefix macro definitions (e.g. `["#define"]`).
    #[must_use]
    pub fn macro_def_markers(&self) -> &[String] {
        &self.macro_def_markers
    }

    /// Raw tree-sitter kinds for macro definitions.
    #[must_use]
    pub fn macro_def_kinds(&self) -> &[String] {
        &self.macro_def_kinds
    }

    /// Raw kind for macro invocations.  Empty string when not applicable.
    #[must_use]
    pub fn macro_invocation_kind(&self) -> &str {
        &self.macro_invocation_kind
    }

    /// Grammar field name for the macro parameter list.
    #[must_use]
    pub fn macro_parameters_field(&self) -> &str {
        &self.macro_parameters_field
    }

    /// Grammar field name for the macro body/value.
    #[must_use]
    pub fn macro_value_field(&self) -> &str {
        &self.macro_value_field
    }

    /// Whether this language has macro-expansion support configured.
    #[must_use]
    pub const fn has_macro_support(&self) -> bool {
        !self.macro_def_kinds.is_empty()
    }

    /// Is this a usage-site identifier kind?
    #[must_use]
    pub fn is_usage_node_kind(&self, kind: &str) -> bool {
        self.usage_node_kinds.iter().any(|s| s == kind)
    }

    /// Is this a statement / expression boundary kind?
    ///
    /// Used to stop upward tree traversal in data-flow analysis and to
    /// identify real statements inside case bodies.
    #[must_use]
    pub fn is_statement_boundary_kind(&self, kind: &str) -> bool {
        self.statement_boundary_kinds.iter().any(|s| s == kind)
    }

    /// Is this a shift expression kind?
    #[must_use]
    pub fn is_shift_expression_kind(&self, kind: &str) -> bool {
        self.shift_expression_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this a template/generic misparse indicator kind?
    #[must_use]
    pub fn is_template_misparse_kind(&self, kind: &str) -> bool {
        self.template_misparse_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this text a null literal for this language?
    #[must_use]
    pub fn is_null_literal(&self, text: &str) -> bool {
        self.null_literals.iter().any(|s| s == text)
    }

    /// Is this text a boolean literal for this language?
    #[must_use]
    pub fn is_boolean_literal(&self, text: &str) -> bool {
        self.boolean_literals.iter().any(|s| s == text)
    }

    /// Is this text a static-storage keyword?
    #[must_use]
    pub fn is_static_storage_keyword(&self, text: &str) -> bool {
        self.static_storage_keywords.iter().any(|s| s == text)
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

    /// Is this a scope-creating kind (opens a new variable scope)?
    #[must_use]
    pub fn is_scope_creating_kind(&self, kind: &str) -> bool {
        self.scope_creating_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this a conditional-branch kind?
    #[must_use]
    pub fn is_branch_kind(&self, kind: &str) -> bool {
        self.branch_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this a loop-construct kind?
    #[must_use]
    pub fn is_loop_kind(&self, kind: &str) -> bool {
        self.loop_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this an exception-handler kind?
    #[must_use]
    pub fn is_exception_handler_kind(&self, kind: &str) -> bool {
        self.exception_handler_raw_kinds.iter().any(|s| s == kind)
    }

    /// Is this declaration kind block-scoped?
    /// When `block_scoped_declaration_raw_kinds` is empty every declaration
    /// kind is treated as block-scoped (correct for C++, Rust, Python).
    #[must_use]
    pub fn is_block_scoped_declaration_kind(&self, kind: &str) -> bool {
        if self.block_scoped_declaration_raw_kinds.is_empty() {
            self.is_declaration_kind(kind)
        } else {
            self.block_scoped_declaration_raw_kinds
                .iter()
                .any(|s| s == kind)
        }
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

    /// Does this language have `goto` statements?
    #[must_use]
    pub const fn has_goto_statement(&self) -> bool {
        self.has_goto
    }

    /// Do function parameters share the same variable scope as the function body?
    ///
    /// `true` for Python-style languages where params are just the first
    /// assignments in the function scope, not a separate outer scope.
    #[must_use]
    pub const fn params_share_body_scope(&self) -> bool {
        self.params_share_body_scope
    }

    /// Does the language coerce non-boolean values to truth in conditions?
    #[must_use]
    pub const fn has_implicit_truth(&self) -> bool {
        self.has_implicit_truthiness
    }

    /// Raw kind for decorator/attribute nodes (if any).
    #[must_use]
    pub fn decorator_kind(&self) -> Option<&str> {
        self.decorator_raw_kind.as_deref()
    }

    // -- value accessors -----------------------------------------------

    // Slice accessors — return the raw-kind slices for callers that need
    // to pass them to helpers or build `Vec` collections (e.g. engine.rs
    // `field_to_kinds_for_config`).

    /// Raw kinds for function/method definitions.
    #[must_use]
    pub fn function_kinds(&self) -> &[String] {
        &self.function_raw_kinds
    }

    /// Raw kinds for type definitions.
    #[must_use]
    pub fn type_kinds(&self) -> &[String] {
        &self.type_raw_kinds
    }

    /// Raw kinds for owner containers (impl blocks, classes) that can
    /// enclose methods.  Used to compute `enclosing_type` on function rows.
    #[must_use]
    pub fn owner_container_kinds(&self) -> &[String] {
        &self.owner_container_raw_kinds
    }

    /// Raw kinds for any definition (function, type, variable, etc.).
    #[must_use]
    pub fn definition_kinds(&self) -> &[String] {
        &self.definition_raw_kinds
    }

    /// Raw kinds for variable/const declarations.
    #[must_use]
    pub fn declaration_kinds(&self) -> &[String] {
        &self.declaration_raw_kinds
    }

    /// Raw kinds for field/member declarations.
    #[must_use]
    pub fn field_kinds(&self) -> &[String] {
        &self.field_raw_kinds
    }

    /// Raw kinds for number literal nodes.
    #[must_use]
    pub fn number_literal_kinds(&self) -> &[String] {
        &self.number_literal_raw_kinds
    }

    /// Raw kinds for update/increment expressions.
    #[must_use]
    pub fn update_kinds(&self) -> &[String] {
        &self.update_raw_kinds
    }

    /// Raw kinds for shift expressions.
    #[must_use]
    pub fn shift_expression_kinds(&self) -> &[String] {
        &self.shift_expression_raw_kinds
    }

    /// Raw kinds for control-flow statements.
    #[must_use]
    pub fn control_flow_kinds(&self) -> &[String] {
        &self.control_flow_raw_kinds
    }

    /// Block-grouping rules configured for this language (may be empty).
    #[must_use]
    pub fn block_groups(&self) -> &[BlockGroupSpec] {
        &self.block_groups
    }

    /// The block-grouping rule whose member kind matches `fql_kind`, if any.
    #[must_use]
    pub fn block_group_for_member(&self, fql_kind: &str) -> Option<&BlockGroupSpec> {
        self.block_groups
            .iter()
            .find(|g| g.member_fql_kind == fql_kind)
    }

    /// Raw kinds for switch/match statements.
    #[must_use]
    pub fn switch_kinds(&self) -> &[String] {
        &self.switch_raw_kinds
    }

    /// Null literal values.
    #[must_use]
    pub fn null_literal_values(&self) -> &[String] {
        &self.null_literals
    }

    /// Cast kind triples: `(raw_kind, cast_style, cast_safety)`.
    #[must_use]
    pub fn cast_kind_triples(&self) -> &[(String, String, String)] {
        &self.cast_kinds
    }

    // Single-kind string accessors.

    /// Raw kind for comments.
    #[must_use]
    pub fn comment_kind(&self) -> &str {
        &self.comment_raw_kind
    }

    /// Synthetic raw kind for compound-assignment rows.
    #[must_use]
    pub fn compound_assignment_kind(&self) -> &str {
        &self.compound_assignment_raw_kind
    }

    /// Raw kind for call expressions.
    #[must_use]
    pub fn call_expression_kind(&self) -> &str {
        &self.call_expression_raw_kind
    }

    /// Raw kind for template/generic declarations.
    #[must_use]
    pub fn template_declaration_kind(&self) -> &str {
        &self.template_declaration_raw_kind
    }

    /// Raw kind for block/compound-statement nodes.
    #[must_use]
    pub fn block_kind(&self) -> &str {
        &self.block_raw_kind
    }

    /// Raw slice of scope-creating node kinds.
    #[must_use]
    pub fn scope_creating_kinds(&self) -> &[String] {
        &self.scope_creating_raw_kinds
    }

    /// Raw slice of conditional-branch node kinds.
    #[must_use]
    pub fn branch_kinds(&self) -> &[String] {
        &self.branch_raw_kinds
    }

    /// Raw slice of loop-construct node kinds.
    #[must_use]
    pub fn loop_kinds(&self) -> &[String] {
        &self.loop_raw_kinds
    }

    /// Raw slice of exception-handler node kinds.
    #[must_use]
    pub fn exception_handler_kinds(&self) -> &[String] {
        &self.exception_handler_raw_kinds
    }
    /// Raw kind for parameter-list container nodes.
    #[must_use]
    pub fn parameter_list_kind(&self) -> &str {
        &self.parameter_list_raw_kind
    }

    /// Raw kind for parameter declarations.
    #[must_use]
    pub fn parameter_kind(&self) -> &str {
        &self.parameter_raw_kind
    }

    /// Raw kind for binary expressions.
    #[must_use]
    pub fn binary_expression_kind(&self) -> &str {
        &self.binary_expression_raw_kind
    }

    /// Raw kind for array declarators.
    #[must_use]
    pub fn array_declarator_kind(&self) -> &str {
        &self.array_declarator_raw_kind
    }

    /// Raw kind for address-of expressions.
    #[must_use]
    pub fn address_of_expression_kind(&self) -> &str {
        &self.address_of_expression_raw_kind
    }

    /// Raw kind for return statements.
    #[must_use]
    pub fn return_statement_kind(&self) -> &str {
        &self.return_statement_raw_kind
    }

    /// Raw kind for goto statements.
    #[must_use]
    pub fn goto_statement_kind(&self) -> &str {
        &self.goto_statement_raw_kind
    }

    /// Raw kind for throw/raise statements.
    #[must_use]
    pub fn throw_statement_kind(&self) -> &str {
        &self.throw_statement_raw_kind
    }

    /// Raw kinds for string literal nodes.
    #[must_use]
    pub fn string_literal_kinds(&self) -> &[String] {
        &self.string_literal_raw_kinds
    }

    /// Node kinds that act as nested function-like bodies.
    /// The metrics enricher stops DFS at these nodes so return/goto/string/throw
    /// counts are not inflated by lambdas or other nested function bodies.
    #[must_use]
    pub fn nested_function_body_kinds(&self) -> &[String] {
        &self.nested_function_body_raw_kinds
    }

    /// Parent node kinds that indicate a number literal is a named constant
    /// (should NOT be flagged as `is_magic`).
    #[must_use]
    pub fn constant_def_parent_kinds(&self) -> &[String] {
        &self.constant_def_parent_raw_kinds
    }

    // Capability / misc accessors.

    /// Whether the language has `++`/`--` operators.
    #[must_use]
    pub const fn has_increment_decrement_ops(&self) -> bool {
        self.has_increment_decrement
    }

    /// Digit group separator character (e.g. `'` for C++, `_` for Rust).
    #[must_use]
    pub const fn digit_sep(&self) -> Option<char> {
        self.digit_separator
    }

    /// Suffix table: `(suffix, meaning)` pairs.
    #[must_use]
    pub fn number_suffix_table(&self) -> &[(String, String)] {
        &self.number_suffixes
    }

    /// Doc-comment prefix table: `(prefix, style)` pairs.
    #[must_use]
    pub fn doc_comment_prefix_table(&self) -> &[(String, String)] {
        &self.doc_comment_prefixes
    }

    /// Scope-resolution separator (e.g. `"::"` for C++, `"."` for others).
    #[must_use]
    pub fn scope_sep(&self) -> &str {
        &self.scope_separator
    }

    /// Grammar field name for the declarator child.
    #[must_use]
    pub fn declarator_field(&self) -> &str {
        &self.declarator_field_name
    }

    /// Raw kind for function-type declarators.
    #[must_use]
    pub fn function_declarator(&self) -> &str {
        &self.function_declarator_kind
    }

    /// Textual operator for address-of (e.g. `"&"`).
    #[must_use]
    pub fn address_of_op(&self) -> &str {
        &self.address_of_operator
    }

    // -- lookup methods ------------------------------------------------

    /// Look up cast info by raw kind.  Returns `(style, safety)`.
    #[must_use]
    pub fn cast_info(&self, kind: &str) -> Option<(&str, &str)> {
        self.cast_kinds
            .iter()
            .find(|(rk, _, _)| rk == kind)
            .map(|(_, style, safety)| (style.as_str(), safety.as_str()))
    }

    /// Look up named-cast info by keyword text (e.g. `"static_cast"`).
    /// Returns `(style, safety)` for cast keywords that tree-sitter parses as
    /// `call_expression(template_function(identifier))` rather than a distinct
    /// cast node kind.
    #[must_use]
    pub fn named_cast_info(&self, keyword: &str) -> Option<(&str, &str)> {
        self.named_cast_keywords
            .iter()
            .find(|(kw, _, _)| kw == keyword)
            .map(|(_, style, safety)| (style.as_str(), safety.as_str()))
    }

    /// Look up for-loop style by raw kind.
    #[must_use]
    pub fn for_style(&self, kind: &str) -> Option<&str> {
        self.for_style_map
            .iter()
            .find(|(rk, _)| rk == kind)
            .map(|(_, style)| style.as_str())
    }

    /// Look up the enrichment field name for a modifier keyword.
    #[must_use]
    pub fn modifier_field_for(&self, keyword: &str) -> Option<&str> {
        self.modifier_map
            .iter()
            .find(|(kw, _)| kw == keyword)
            .map(|(_, field)| field.as_str())
    }

    /// Look up visibility for an access-specifier keyword (exact match).
    #[must_use]
    pub fn visibility_for_keyword(&self, keyword: &str) -> Option<&str> {
        self.visibility_keywords
            .iter()
            .find(|(kw, _)| kw == keyword)
            .map(|(_, vis)| vis.as_str())
    }

    /// Look up visibility from node text that *contains* a keyword.
    /// Useful when the node text is e.g. `"public:"` and the keyword is `"public"`.
    #[must_use]
    pub fn visibility_for_text(&self, text: &str) -> Option<&str> {
        self.visibility_keywords
            .iter()
            .find(|(kw, _)| text.contains(kw.as_str()))
            .map(|(_, vis)| vis.as_str())
    }

    /// Look up default visibility for a type kind.
    #[must_use]
    pub fn default_visibility_for_type(&self, type_kind: &str) -> Option<&str> {
        self.visibility_default_by_type
            .iter()
            .find(|(rk, _)| rk == type_kind)
            .map(|(_, vis)| vis.as_str())
    }

    /// Detect comment style from comment text (first-prefix-wins).
    #[must_use]
    pub fn detect_comment_style(&self, text: &str) -> Option<&str> {
        self.doc_comment_prefixes
            .iter()
            .find(|(prefix, _)| text.starts_with(prefix.as_str()))
            .map(|(_, style)| style.as_str())
    }

    /// Look up meaning for a number literal suffix.
    #[must_use]
    pub fn number_suffix_meaning(&self, suffix: &str) -> Option<&str> {
        self.number_suffixes
            .iter()
            .find(|(s, _)| s == suffix)
            .map(|(_, meaning)| meaning.as_str())
    }

    /// Root node kind for the grammar (e.g. `"translation_unit"`, `"source_file"`).
    #[must_use]
    pub fn root_kind(&self) -> &str {
        &self.root_node_kind
    }

    /// Look up a universal FQL kind for a raw tree-sitter kind, using the
    /// data-driven `kind_map` loaded from the language JSON config.
    ///
    /// Returns `None` for raw kinds that have no mapping.
    #[must_use]
    pub fn kind_map_lookup(&self, raw_kind: &str) -> Option<&str> {
        self.kind_map.get(raw_kind).map(String::as_str)
    }
}
