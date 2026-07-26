//! The C++ constructs the indexer is expected to recognise.
//!
//! One test per node kind — functions bare and qualified, member functions and
//! data members, structs, enums, preprocessor definitions and includes — plus
//! the two that check a member declaration is linked to its out-of-line
//! definition and that identifier tokens are recorded as usage sites.

use super::util::index_snippet;
#[test]
fn indexes_function_definition() {
    let table = index_snippet("void processSignal(int speed) { return; }");
    let row = table.find_def("processSignal").expect("indexed");
    assert_eq!(table.node_kind_of(row), "function_definition");
    assert_eq!(row.line, 1);
}

#[test]
fn indexes_qualified_function_definition() {
    let table = index_snippet("class Motor { void setup(); };\nvoid Motor::setup() { return; }");
    assert!(
        table.find_def("Motor::setup").is_some(),
        "qualified method should be indexed under its full name"
    );
    // The member declaration inside the class body is also indexed
    // under the bare name as a field_declaration.
    let decl = table
        .find_def("setup")
        .expect("member declaration should be indexed");
    assert_eq!(table.node_kind_of(decl), "field_declaration");
}

#[test]
fn bare_and_qualified_functions_coexist() {
    let table =
        index_snippet("void setup() {}\nclass Motor { void setup(); };\nvoid Motor::setup() {}");
    // find_def returns the last row for a name — the field_declaration
    // from the class body comes after the bare function_definition.
    let last = table.find_def("setup").expect("setup");
    assert_eq!(table.node_kind_of(last), "field_declaration");
    // The bare function_definition is still in the table.
    let has_bare_def = table
        .rows
        .iter()
        .any(|r| table.name_of(r) == "setup" && table.node_kind_of(r) == "function_definition");
    assert!(has_bare_def, "bare function_definition should exist");
    let qualified = table.find_def("Motor::setup").expect("qualified setup");
    assert_eq!(table.node_kind_of(qualified), "function_definition");
}

#[test]
fn indexes_member_function_declaration() {
    let table = index_snippet(
        "class SignalSequencer {\n  void loadSignalCode(int code);\n  int getValue() const;\n};",
    );
    let load = table
        .find_def("loadSignalCode")
        .expect("member declaration indexed");
    assert_eq!(table.node_kind_of(load), "field_declaration");
    let get = table
        .find_def("getValue")
        .expect("member declaration indexed");
    assert_eq!(table.node_kind_of(get), "field_declaration");
}

#[test]
fn indexes_member_data_field() {
    let table = index_snippet("struct Point { int x; double y; };");
    let x = table.find_def("x").expect("data member indexed");
    assert_eq!(table.node_kind_of(x), "field_declaration");
    let y = table.find_def("y").expect("data member indexed");
    assert_eq!(table.node_kind_of(y), "field_declaration");
}

#[test]
fn indexes_struct_specifier() {
    let table = index_snippet("struct Motor { int speed; };");
    let row = table.find_def("Motor").expect("indexed");
    assert_eq!(table.node_kind_of(row), "struct_specifier");
}

#[test]
fn indexes_preproc_def() {
    let table = index_snippet("#define BAUD_RATE 9600");
    let row = table.find_def("BAUD_RATE").expect("indexed");
    assert_eq!(table.node_kind_of(row), "preproc_def");
}

#[test]
fn indexes_enum_specifier() {
    let table = index_snippet("enum class State { Idle, Running };");
    let row = table.find_def("State").expect("indexed");
    assert_eq!(table.node_kind_of(row), "enum_specifier");
}

#[test]
fn indexes_preproc_include() {
    let table = index_snippet("#include <stdint.h>");
    let row = table.find_def("stdint.h").expect("indexed");
    assert_eq!(table.node_kind_of(row), "preproc_include");
}

#[test]
fn usage_sites_indexed_for_identifier_tokens() {
    let table = index_snippet("void foo() { foo(); }");
    let sites = table.find_usages("foo");
    assert!(!sites.is_empty(), "foo should have usage sites");
}

#[test]
fn member_method_declaration_carries_body_symbol() {
    let table =
        index_snippet("class Motor { void setup(int speed); };\nvoid Motor::setup(int speed) {}");
    let decl = table.find_def("setup").expect("member declaration indexed");
    assert_eq!(table.node_kind_of(decl), "field_declaration");
    assert_eq!(
        table.strings.field_str(&decl.fields, "body_symbol"),
        Some("Motor::setup"),
        "body_symbol must point to the qualified name"
    );
}

#[test]
fn data_member_has_no_body_symbol() {
    let table = index_snippet("struct Point { int x; double y; };");
    let x = table.find_def("x").expect("data member indexed");
    assert!(
        table.strings.field_str(&x.fields, "body_symbol").is_none(),
        "data members should not have body_symbol"
    );
}
