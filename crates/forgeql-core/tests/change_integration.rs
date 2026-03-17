//! Integration tests for the `CHANGE FILE[S]` transform.
//!
//! Tests round-trip: plan → apply → verify file contents using
//! `motor_control` fixtures in a temp workspace.
//!
//! Run with: `cargo test -p forgeql-core --test change_integration`
#![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

use std::fs;
use std::path::PathBuf;

use forgeql_core::{
    ast::index::SymbolTable, context::RequestContext, ir::ChangeTarget,
    transforms::change::ChangeFiles, workspace::Workspace,
};
use tempfile::tempdir;

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures")
}

/// Copy fixtures into a temp dir and return the workspace.
fn setup_workspace() -> (tempfile::TempDir, Workspace, SymbolTable) {
    let dir = tempdir().expect("tempdir");
    let src = fixtures_dir();

    fs::copy(
        src.join("motor_control.h"),
        dir.path().join("motor_control.h"),
    )
    .expect("copy .h");
    fs::copy(
        src.join("motor_control.cpp"),
        dir.path().join("motor_control.cpp"),
    )
    .expect("copy .cpp");

    let ws = Workspace::new(dir.path()).expect("new workspace");
    let idx = SymbolTable::build(&ws).expect("build index");
    (dir, ws, idx)
}

fn ctx() -> RequestContext {
    RequestContext::admin()
}

// -----------------------------------------------------------------------
// MATCHING mode
// -----------------------------------------------------------------------

#[test]
fn change_matching_replaces_unique_occurrence() {
    let (_dir, ws, idx) = setup_workspace();
    let cpp_rel = "motor_control.h";
    let abs_path = ws.root().join(cpp_rel);

    // Identify a pattern known to occur exactly once in the header.
    let original = fs::read_to_string(&abs_path).expect("read");
    let pattern = "#pragma once";
    assert!(original.contains(pattern));

    let c = ChangeFiles::new(
        vec![cpp_rel.to_string()],
        ChangeTarget::Matching {
            pattern: pattern.to_string(),
            replacement: "#pragma once\n// ForgeQL was here".to_string(),
        },
    );
    let plan = c.plan(&ctx(), &ws, &idx).expect("plan");
    assert!(!plan.is_empty());
    plan.apply().expect("apply");

    let after = fs::read_to_string(&abs_path).expect("read after");
    assert!(after.contains("// ForgeQL was here"));
    assert!(!after.contains("#pragma once\n#pragma once")); // only once
}

// -----------------------------------------------------------------------
// LINES mode
// -----------------------------------------------------------------------

#[test]
fn change_lines_replaces_range() {
    let (_dir, ws, idx) = setup_workspace();
    let cpp_rel = "motor_control.h";
    let abs_path = ws.root().join(cpp_rel);

    // Replace lines 1-2 with a single line.
    let c = ChangeFiles::new(
        vec![cpp_rel.to_string()],
        ChangeTarget::Lines {
            start: 1,
            end: 2,
            content: "// replaced by CHANGE LINES\n".to_string(),
        },
    );
    let plan = c.plan(&ctx(), &ws, &idx).expect("plan");
    assert_eq!(plan.edit_count(), 1);
    plan.apply().expect("apply");

    let after = fs::read_to_string(&abs_path).expect("read after");
    assert!(after.starts_with("// replaced by CHANGE LINES\n"));
}

// -----------------------------------------------------------------------
// WITH (create/overwrite) mode
// -----------------------------------------------------------------------

#[test]
fn change_with_content_creates_new_file() {
    let (_dir, ws, idx) = setup_workspace();
    let new_file = "brand_new.cpp";
    let abs_path = ws.root().join(new_file);
    assert!(!abs_path.exists());

    let c = ChangeFiles::new(
        vec![new_file.to_string()],
        ChangeTarget::WithContent {
            content: "// new file\nint main() { return 0; }\n".to_string(),
        },
    );
    let plan = c.plan(&ctx(), &ws, &idx).expect("plan");
    assert_eq!(plan.edit_count(), 1);
    plan.apply().expect("apply");

    assert!(abs_path.exists());
    let content = fs::read_to_string(&abs_path).expect("read");
    assert!(content.contains("int main()"));
}

#[test]
fn change_with_content_overwrites_existing_file() {
    let (_dir, ws, idx) = setup_workspace();
    let cpp_rel = "motor_control.h";
    let abs_path = ws.root().join(cpp_rel);

    let c = ChangeFiles::new(
        vec![cpp_rel.to_string()],
        ChangeTarget::WithContent {
            content: "// completely replaced\n".to_string(),
        },
    );
    let plan = c.plan(&ctx(), &ws, &idx).expect("plan");
    plan.apply().expect("apply");

    let after = fs::read_to_string(&abs_path).expect("read");
    assert_eq!(after, "// completely replaced\n");
}

// -----------------------------------------------------------------------
// DELETE mode
// -----------------------------------------------------------------------

#[test]
fn change_delete_empties_file() {
    let (_dir, ws, idx) = setup_workspace();
    let cpp_rel = "motor_control.h";
    let abs_path = ws.root().join(cpp_rel);
    assert!(abs_path.exists());

    let c = ChangeFiles::new(vec![cpp_rel.to_string()], ChangeTarget::Delete);
    let plan = c.plan(&ctx(), &ws, &idx).expect("plan");
    let result = plan.apply().expect("apply");

    // File is emptied (apply writes empty content).
    let after = fs::read_to_string(&abs_path).expect("read");
    assert!(after.is_empty());

    // Rollback restores it.
    result.rollback().expect("rollback");
    let restored = fs::read_to_string(&abs_path).expect("read");
    assert!(!restored.is_empty());
}

// -----------------------------------------------------------------------
// Multi-file validation
// -----------------------------------------------------------------------

#[test]
fn change_multi_file_with_content_rejected() {
    let (_dir, ws, idx) = setup_workspace();
    let c = ChangeFiles::new(
        vec!["a.cpp".to_string(), "b.cpp".to_string()],
        ChangeTarget::WithContent {
            content: "x".to_string(),
        },
    );
    assert!(c.plan(&ctx(), &ws, &idx).is_err());
}
