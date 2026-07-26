//! What indexing a snippet actually produces.
//!
//! These cover the rules an agent depends on but cannot see: which runs of
//! comments become one addressable block, how unparseable regions surface as a
//! single error row rather than many, what alias fields a block's members
//! carry, and — the load-bearing one — that a node id keeps pointing at the
//! same construct across an unrelated edit elsewhere in the file.
use crate::ast::enrich::default_enrichers;
use crate::ast::index::*;

use super::util::{index_rust_snippet, index_snippet};
#[test]
fn cpp_struct_coverage_union_enum_typedef() {
    // Named unions, enum constants, and typedef aliases (including the
    // anonymous `typedef enum { .. } Name;` form) must each be indexed with
    // a real fql_kind and an addressable node_id (ordinal).
    let src = concat!(
        "typedef enum { COUNTER_IRQ_NONE, COUNTER_IRQ_ALL } counter_event_t;\n",
        "union acpi_id { unsigned int raw; };\n",
        "typedef unsigned int paddr_t;\n",
        "typedef void (*cb_t)(int);\n",
    );
    let table = index_snippet(src);

    let addressable = |kind: &str, name: &str| {
        table.rows.iter().any(|r| {
            table.fql_kind_of(r) == kind && table.name_of(r) == name && r.ordinal.is_some()
        })
    };

    assert!(
        addressable("union", "acpi_id"),
        "named union must be addressable"
    );
    assert!(
        addressable("type_alias", "counter_event_t"),
        "anonymous typedef enum alias must be addressable"
    );
    assert!(
        addressable("type_alias", "cb_t"),
        "function-pointer typedef alias must be addressable"
    );
    assert!(
        addressable("type_alias", "paddr_t"),
        "scalar typedef alias must be addressable"
    );
    assert!(
        table
            .rows
            .iter()
            .any(|r| table.fql_kind_of(r) == "enumerator" && r.ordinal.is_some()),
        "enum constants must be addressable"
    );
}

#[test]
fn leading_attribute_folds_into_node_span() {
    // A Rust item's span (line / byte_range / rev) should start at its
    // leading `#[...]` attribute, not at the `fn`/`struct` keyword.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("snippet.rs");
    std::fs::write(&file, "#[derive(Clone)]\nstruct Widget;\n").unwrap();

    let mut table = SymbolTable::default();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let enrichers = default_enrichers();
    {
        let mut ctx = IndexContext {
            path: &file,
            language: &crate::ast::lang::RustLanguageInline,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table,
        };
        index_file(&mut parser, &mut ctx, None).unwrap();
    }

    let widget = table
        .rows
        .iter()
        .find(|r| table.name_of(r) == "Widget")
        .expect("struct Widget should be indexed");
    // Line 1 is `#[derive(Clone)]`; line 2 is `struct Widget;`. The attribute
    // folds into the span, so the node reports line 1 and starts at byte 0.
    assert_eq!(widget.line, 1, "span should start at the #[derive] line");
    assert_eq!(
        widget.byte_range.start, 0,
        "byte_range should include the leading attribute"
    );
}

#[test]
fn control_flow_node_parents_its_body() {
    // A statement inside an `if` must parent to the if-node, not jump up to the
    // enclosing function (plan §4.1 branches-as-parents). Engine-level, keyed on
    // config.is_control_flow_kind, so it holds for every language.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("snippet.rs");
    let src = "fn f(x: i32) {\n    if x > 0 {\n        let y = x;\n    }\n}\n";
    std::fs::write(&file, src).unwrap();

    let mut table = SymbolTable::default();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let enrichers = default_enrichers();
    {
        let mut ctx = IndexContext {
            path: &file,
            language: &crate::ast::lang::RustLanguageInline,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table,
        };
        index_file(&mut parser, &mut ctx, None).unwrap();
    }

    let if_ord = table
        .rows
        .iter()
        .find(|r| table.fql_kind_of(r) == "if")
        .and_then(|r| r.ordinal)
        .expect("if node should be indexed with an ordinal");
    let let_row = table
        .rows
        .iter()
        .find(|r| table.name_of(r) == "y")
        .expect("`let y` should be indexed");
    assert_eq!(
        let_row.parent_ordinal, if_ord,
        "statement inside the `if` should parent to the if-node, not the function"
    );
}

#[test]
fn comment_run_births_a_childless_block() {
    // Three consecutive `//` comments coalesce into one synthetic
    // `comment_block` that spans the whole run, shares the comments' parent,
    // and has no children of its own.
    let src = "// first line\n// second line\n// third line\nfn f() {}\n";
    let table = index_rust_snippet(src);

    let block = table
        .rows
        .iter()
        .find(|r| table.fql_kind_of(r) == "comment_block")
        .expect("a comment_block should be born for a run of 3 comments");

    let block_text = &src[block.byte_range.clone()];
    assert!(
        block_text.contains("first line"),
        "block should cover the first comment"
    );
    assert!(
        block_text.contains("third line"),
        "block should cover the last comment"
    );

    let block_ord = block.ordinal.expect("block must have an ordinal");
    assert!(
        !table.rows.iter().any(|r| r.parent_ordinal == block_ord),
        "the comment_block must be childless"
    );

    let comment = table
        .rows
        .iter()
        .find(|r| table.fql_kind_of(r) == "comment")
        .expect("individual comment rows should still exist");
    assert_eq!(
        block.parent_ordinal, comment.parent_ordinal,
        "the block is a sibling of its members, sharing their parent"
    );

    let comment_count = table
        .rows
        .iter()
        .filter(|r| table.fql_kind_of(r) == "comment")
        .count();
    assert_eq!(comment_count, 3, "individual comment rows are preserved");
}

#[test]
fn comment_block_bridges_blank_lines() {
    // A blank line between same-style comments does not split the block:
    // blank lines are not tree nodes, so the comments stay adjacent siblings.
    let src = "// a\n// b\n\n// c\n// d\nfn f() {}\n";
    let table = index_rust_snippet(src);

    let blocks: Vec<_> = table
        .rows
        .iter()
        .filter(|r| table.fql_kind_of(r) == "comment_block")
        .collect();
    assert_eq!(blocks.len(), 1, "the blank line must not split the run");
    let block_text = &src[blocks[0].byte_range.clone()];
    assert!(block_text.contains("// a") && block_text.contains("// d"));
}

#[test]
fn comment_block_splits_on_style() {
    // `///` (doc) and `//` (line) runs form separate blocks via split_on_attr.
    let src = "/// doc one\n/// doc two\n// line one\n// line two\nfn f() {}\n";
    let table = index_rust_snippet(src);

    let blocks = table
        .rows
        .iter()
        .filter(|r| table.fql_kind_of(r) == "comment_block")
        .count();
    assert_eq!(blocks, 2, "doc and line comment runs are separate blocks");
}

#[test]
fn single_comment_gets_no_block() {
    // A lone comment (run shorter than min_run = 2) stays a plain comment.
    let src = "// lonely\nfn f() {}\n";
    let table = index_rust_snippet(src);
    assert!(
        !table
            .rows
            .iter()
            .any(|r| table.fql_kind_of(r) == "comment_block"),
        "a single comment must not create a block"
    );
}

#[test]
fn syntax_damage_is_mapped_as_an_error_row() {
    // Until now a broken file was silently, partially indexed: tree-sitter
    // recovered, ERROR subtrees existed, and nothing surfaced them — so an
    // agent could not learn that the file it was about to mutate was already
    // broken. The damage is now an addressable row.
    let src = "fn ok() {}\nfn broken( { let x = ;\nfn also_ok() {}\n";
    let table = index_rust_snippet(src);

    let errors: Vec<_> = table
        .rows
        .iter()
        .filter(|r| table.fql_kind_of(r) == "error")
        .collect();

    assert!(
        !errors.is_empty(),
        "a file tree-sitter could not parse must surface at least one error row"
    );
    // EVERY error row must be addressable: a row spanning no bytes could be
    // seen but not read or repaired (`SHOW NODE` / `CHANGE NODE` need bytes).
    // This is why zero-width MISSING nodes are deliberately not emitted.
    assert!(
        errors.iter().all(|r| !r.byte_range.is_empty()),
        "an error row that spans no bytes cannot be acted on"
    );
}

#[test]
fn a_clean_file_has_no_error_rows() {
    let table = index_rust_snippet("fn f() -> u32 { 42 }\n");
    assert!(
        !table.rows.iter().any(|r| table.fql_kind_of(r) == "error"),
        "a file that parses cleanly must produce no error rows"
    );
}

#[test]
fn nested_damage_reports_one_region_not_many() {
    // A broken region can contain further ERROR nodes. Emitting each would
    // report one wound as several, so only the OUTERMOST is surfaced.
    let src = "fn a() {}\nfn bad( ( ( { ] ) ;\nfn b() {}\n";
    let table = index_rust_snippet(src);

    let errors = table
        .rows
        .iter()
        .filter(|r| table.fql_kind_of(r) == "error")
        .count();
    assert!(
        errors <= 2,
        "nested ERROR nodes must not each emit a row (got {errors})"
    );
}

#[test]
fn block_members_carry_block_alias_fields() {
    // Each member of a block is tagged with the block's 4-digit ordinal
    // (`block_ord`) and its 1-based offset within the block (`block_off`),
    // which powers the FIND/SHOW `block_id(offset)` surfacing.
    let src = "// first\n// second\n// third\nfn f() {}\n";
    let table = index_rust_snippet(src);

    let block_ord = table
        .rows
        .iter()
        .find(|r| table.fql_kind_of(r) == "comment_block")
        .and_then(|r| r.ordinal)
        .expect("comment_block exists");
    let expected_ord = format!("{block_ord:04}");

    let offs: Vec<&str> = table
        .rows
        .iter()
        .filter(|r| table.fql_kind_of(r) == "comment")
        .map(|r| {
            assert_eq!(
                table.field_str(&r.fields, "block_ord"),
                Some(expected_ord.as_str()),
                "each member points at the owning block ordinal"
            );
            table.field_str(&r.fields, "block_off").unwrap_or_default()
        })
        .collect();
    assert_eq!(
        offs,
        ["1", "2", "3"],
        "members are tagged with 1-based offsets within the block"
    );
}

#[test]
fn doc_comment_block_members_get_single_line_offsets() {
    // A `///` doc comment's tree-sitter span includes the trailing newline
    // (end_position at column 0 of the next line); the member offset must
    // still be a single line, not a 2-line range.
    let src = "/// first\n/// second\nfn f() {}\n";
    let table = index_rust_snippet(src);
    let offs: Vec<&str> = table
        .rows
        .iter()
        .filter(|r| table.fql_kind_of(r) == "comment")
        .map(|r| table.field_str(&r.fields, "block_off").unwrap_or_default())
        .collect();
    assert_eq!(
        offs,
        ["1", "2"],
        "doc-comment members must surface as single-line offsets"
    );
}

#[test]
fn control_flow_body_preserves_sibling_node_ids_across_unrelated_edit() {
    // §4.1 must not break node-id survival across an unrelated edit (the NID08
    // "if node-ids survive line drift" property, at unit scope).
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("snippet.rs");
    let src_a = "fn f(x: i32) {\n    if x > 0 {\n        g();\n    }\n    if x < 0 {\n        h();\n    }\n}\n";
    std::fs::write(&file, src_a).unwrap();

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let enrichers = default_enrichers();

    let mut table_a = SymbolTable::default();
    {
        let mut ctx = IndexContext {
            path: &file,
            language: &crate::ast::lang::RustLanguageInline,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table_a,
        };
        index_file(&mut parser, &mut ctx, None).unwrap();
    }

    let lt_if_ordinal = |t: &SymbolTable| -> u32 {
        t.rows
            .iter()
            .find(|r| t.fql_kind_of(r) == "if" && t.name_of(r).contains('<'))
            .and_then(|r| r.ordinal)
            .expect("the `x < 0` if should be indexed with an ordinal")
    };
    let before = lt_if_ordinal(&table_a);

    let mut hints = Vec::new();
    for row in &table_a.rows {
        let Some(ordinal) = row.ordinal else {
            continue;
        };
        let fields = table_a.resolve_fields(&row.fields);
        hints.push(OrdinalHint {
            name: table_a.name_of(row).to_string(),
            fql_kind: table_a.fql_kind_of(row).to_string(),
            parent_ordinal: row.parent_ordinal,
            guard_group_id: fields.get("guard_group_id").cloned(),
            guard_branch: fields.get("guard_branch").cloned(),
            first_body_statement_fingerprint: fields
                .get("first_body_statement_fingerprint")
                .cloned(),
            content_hash: fields.get("content_hash").cloned(),
            ordinal,
        });
    }

    let src_b = format!("// drift marker\n{src_a}");
    std::fs::write(&file, &src_b).unwrap();
    let mut table_b = SymbolTable::default();
    {
        let mut ctx = IndexContext {
            path: &file,
            language: &crate::ast::lang::RustLanguageInline,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: Some(OrdinalRemapper::from_previous(hints)),
            table: &mut table_b,
        };
        index_file(&mut parser, &mut ctx, None).unwrap();
    }
    let after = lt_if_ordinal(&table_b);

    assert_eq!(
        before, after,
        "the `x < 0` if node-id must survive an unrelated edit above the function"
    );
}

#[test]
fn mod_and_type_alias_declarations_are_addressable() {
    // Regression: `pub mod x;` and `type X = …;` were indexed as
    // `namespace`/`type_alias` symbols but never received an ordinal/node_id,
    // so they could not be edited through the node API (CHANGE NODE /
    // INSERT NODE). Both are semantic items and must be addressable, like
    // `struct`/`enum`/`import`.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("snippet.rs");
    let src = "pub mod alpha;\nmod beta;\ntype Handle = u32;\nfn f() {}\n";
    std::fs::write(&file, src).unwrap();

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let enrichers = default_enrichers();
    let mut table = SymbolTable::default();
    {
        let mut ctx = IndexContext {
            path: &file,
            language: &crate::ast::lang::RustLanguageInline,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table,
        };
        index_file(&mut parser, &mut ctx, None).unwrap();
    }

    let addressable = |kind: &str, expected: usize| {
        let rows: Vec<_> = table
            .rows
            .iter()
            .filter(|r| table.fql_kind_of(r) == kind)
            .collect();
        assert_eq!(rows.len(), expected, "expected {expected} `{kind}` row(s)");
        for row in rows {
            assert!(
                row.ordinal.is_some(),
                "`{kind}` declaration '{}' must be addressable (carry an ordinal)",
                table.name_of(row)
            );
        }
    };
    addressable("namespace", 2);
    addressable("type_alias", 1);
}

#[test]
fn block_node_ids_survive_deletion_of_a_sibling_block() {
    // Deleting one comment block must not churn another block's node id.
    // Blocks share the constant `comment_block` name, so the reindex relies
    // on the content_hash field to tell them apart.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("snippet.rs");
    let src_a = "// aaa one\n// aaa two\nfn first() {}\n// bbb one\n// bbb two\nfn second() {}\n";
    std::fs::write(&file, src_a).unwrap();

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let enrichers = default_enrichers();

    let mut table_a = SymbolTable::default();
    {
        let mut ctx = IndexContext {
            path: &file,
            language: &crate::ast::lang::RustLanguageInline,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table_a,
        };
        index_file(&mut parser, &mut ctx, None).unwrap();
    }

    // The "bbb" block is the comment_block with the larger start line.
    let bbb_ordinal = table_a
        .rows
        .iter()
        .filter(|r| table_a.fql_kind_of(r) == "comment_block")
        .max_by_key(|r| r.line)
        .and_then(|r| r.ordinal)
        .expect("the bbb comment_block should be indexed");

    let mut hints = Vec::new();
    for row in &table_a.rows {
        let Some(ordinal) = row.ordinal else {
            continue;
        };
        let fields = table_a.resolve_fields(&row.fields);
        hints.push(OrdinalHint {
            name: table_a.name_of(row).to_string(),
            fql_kind: table_a.fql_kind_of(row).to_string(),
            parent_ordinal: row.parent_ordinal,
            guard_group_id: fields.get("guard_group_id").cloned(),
            guard_branch: fields.get("guard_branch").cloned(),
            first_body_statement_fingerprint: fields
                .get("first_body_statement_fingerprint")
                .cloned(),
            content_hash: fields.get("content_hash").cloned(),
            ordinal,
        });
    }

    // Drop the aaa block entirely; bbb is then the only block.
    let src_b = "fn first() {}\n// bbb one\n// bbb two\nfn second() {}\n";
    std::fs::write(&file, src_b).unwrap();
    let mut table_b = SymbolTable::default();
    {
        let mut ctx = IndexContext {
            path: &file,
            language: &crate::ast::lang::RustLanguageInline,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: Some(OrdinalRemapper::from_previous(hints)),
            table: &mut table_b,
        };
        index_file(&mut parser, &mut ctx, None).unwrap();
    }

    let after = table_b
        .rows
        .iter()
        .find(|r| table_b.fql_kind_of(r) == "comment_block")
        .and_then(|r| r.ordinal)
        .expect("the bbb comment_block should survive");

    assert_eq!(
        bbb_ordinal, after,
        "deleting the aaa block must not churn the bbb block's node id"
    );
}

#[test]
fn block_node_id_survives_editing_its_own_member() {
    // Editing a comment inside a block must not churn that block's node id,
    // even when a sibling block shares the same parent (BUG-021).
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("snippet.rs");
    let src_a = "// aaa one\n// aaa two\nfn first() {}\n// bbb one\n// bbb two\nfn second() {}\n";
    std::fs::write(&file, src_a).unwrap();

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let enrichers = default_enrichers();

    let mut table_a = SymbolTable::default();
    {
        let mut ctx = IndexContext {
            path: &file,
            language: &crate::ast::lang::RustLanguageInline,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table_a,
        };
        index_file(&mut parser, &mut ctx, None).unwrap();
    }

    // bbb is the comment_block with the larger start line.
    let bbb_ordinal = table_a
        .rows
        .iter()
        .filter(|r| table_a.fql_kind_of(r) == "comment_block")
        .max_by_key(|r| r.line)
        .and_then(|r| r.ordinal)
        .expect("the bbb comment_block should be indexed");

    let mut hints = Vec::new();
    for row in &table_a.rows {
        let Some(ordinal) = row.ordinal else {
            continue;
        };
        let fields = table_a.resolve_fields(&row.fields);
        hints.push(OrdinalHint {
            name: table_a.name_of(row).to_string(),
            fql_kind: table_a.fql_kind_of(row).to_string(),
            parent_ordinal: row.parent_ordinal,
            guard_group_id: fields.get("guard_group_id").cloned(),
            guard_branch: fields.get("guard_branch").cloned(),
            first_body_statement_fingerprint: fields
                .get("first_body_statement_fingerprint")
                .cloned(),
            content_hash: fields.get("content_hash").cloned(),
            ordinal,
        });
    }

    // Edit bbb's second member; both blocks' first members are untouched.
    let src_b =
        "// aaa one\n// aaa two\nfn first() {}\n// bbb one\n// bbb CHANGED\nfn second() {}\n";
    std::fs::write(&file, src_b).unwrap();
    let mut table_b = SymbolTable::default();
    {
        let mut ctx = IndexContext {
            path: &file,
            language: &crate::ast::lang::RustLanguageInline,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: Some(OrdinalRemapper::from_previous(hints)),
            table: &mut table_b,
        };
        index_file(&mut parser, &mut ctx, None).unwrap();
    }

    let after = table_b
        .rows
        .iter()
        .filter(|r| table_b.fql_kind_of(r) == "comment_block")
        .max_by_key(|r| r.line)
        .and_then(|r| r.ordinal)
        .expect("the bbb comment_block should survive");

    assert_eq!(
        bbb_ordinal, after,
        "editing bbb's own content must not churn its node id"
    );
}
