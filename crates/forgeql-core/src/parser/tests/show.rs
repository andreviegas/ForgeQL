//! Parser tests for the code-exposure `SHOW` verbs: `body`, `context`,
//! `signature`, `outline`, `members`, `callees` and `LINES`, plus the
//! `SHOW MORE` window forms.
//!
//! `SHOW SOURCES` / `BRANCHES` / `VERSION` / `STATS` read session state rather
//! than code, and live in `sources.rs`.

use crate::parser::*;

// -----------------------------------------------------------------------
// SHOW commands (Code Exposure API)
// -----------------------------------------------------------------------

#[test]
fn parse_show_context_minimal() {
    let ops = parse("SHOW context OF 'setPeakLevel'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowContext {
            symbol, clauses, ..
        } => {
            assert_eq!(symbol, "setPeakLevel");
            assert!(clauses.in_glob.is_none());
            assert!(clauses.depth.is_none());
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_context_with_file_and_lines() {
    // IN 'file' → clauses.in_glob; LINES 10 → clauses.depth
    let ops = parse("SHOW context OF 'setPeakLevel' IN 'src/signal.cpp' LINES 10").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowContext {
            symbol, clauses, ..
        } => {
            assert_eq!(symbol, "setPeakLevel");
            assert_eq!(clauses.in_glob.as_deref(), Some("src/signal.cpp"));
            assert_eq!(clauses.depth, Some(10));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_signature() {
    let ops = parse("SHOW signature OF 'setPeakLevel'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowSignature { symbol, .. } => assert_eq!(symbol, "setPeakLevel"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_outline() {
    let ops = parse("SHOW outline OF 'src/signal.cpp'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowOutline { file, .. } => assert_eq!(file, "src/signal.cpp"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_members() {
    let ops = parse("SHOW members OF 'SignalController'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowMembers { symbol, .. } => assert_eq!(symbol, "SignalController"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_body_no_depth() {
    let ops = parse("SHOW body OF 'processSignal'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowBody {
            symbol, clauses, ..
        } => {
            assert_eq!(symbol, "processSignal");
            assert!(clauses.depth.is_none());
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_body_with_depth() {
    let ops = parse("SHOW body OF 'processSignal' DEPTH 2").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowBody {
            symbol, clauses, ..
        } => {
            assert_eq!(symbol, "processSignal");
            assert_eq!(clauses.depth, Some(2));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_callees() {
    let ops = parse("SHOW callees OF 'processSignal'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowCallees { symbol, .. } => assert_eq!(symbol, "processSignal"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_more_bare_is_full() {
    let ops = parse("SHOW MORE").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowMore {
            window, clauses, ..
        } => {
            assert_eq!(*window, crate::ir::ShowMoreWindow::Full);
            assert!(clauses.where_predicates.is_empty());
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_more_head_tail_range() {
    use crate::ir::ShowMoreWindow;
    let head = parse("SHOW MORE HEAD 20").unwrap();
    let tail = parse("SHOW MORE TAIL 15").unwrap();
    let range = parse("SHOW MORE 120-240").unwrap();
    match &head[0] {
        ForgeQLIR::ShowMore { window, .. } => assert_eq!(*window, ShowMoreWindow::Head(20)),
        _ => panic!("wrong variant"),
    }
    match &tail[0] {
        ForgeQLIR::ShowMore { window, .. } => assert_eq!(*window, ShowMoreWindow::Tail(15)),
        _ => panic!("wrong variant"),
    }
    match &range[0] {
        ForgeQLIR::ShowMore { window, .. } => {
            assert_eq!(*window, ShowMoreWindow::Range(120, 240));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_more_last_n() {
    use crate::ir::ShowMoreWindow;
    // Bare SHOW MORE defaults to LAST-0.
    match &parse("SHOW MORE").unwrap()[0] {
        ForgeQLIR::ShowMore { last, window, .. } => {
            assert_eq!(*last, 0);
            assert_eq!(*window, ShowMoreWindow::Full);
        }
        _ => panic!("wrong variant"),
    }
    // LAST-n selector alone.
    match &parse("SHOW MORE LAST-2").unwrap()[0] {
        ForgeQLIR::ShowMore { last, window, .. } => {
            assert_eq!(*last, 2);
            assert_eq!(*window, ShowMoreWindow::Full);
        }
        _ => panic!("wrong variant"),
    }
    // LAST-n composes with a range window — the atomic LAST-<n> token never
    // collides with the range hyphen.
    match &parse("SHOW MORE LAST-1 1-1000").unwrap()[0] {
        ForgeQLIR::ShowMore { last, window, .. } => {
            assert_eq!(*last, 1);
            assert_eq!(*window, ShowMoreWindow::Range(1, 1000));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_more_window_then_where_composes() {
    // WHERE must apply after every window form, not just bare SHOW MORE.
    let ops = parse("SHOW MORE TAIL 40 WHERE text MATCHES 'error|fail'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowMore {
            window, clauses, ..
        } => {
            assert_eq!(*window, crate::ir::ShowMoreWindow::Tail(40));
            assert_eq!(clauses.where_predicates.len(), 1);
            assert_eq!(clauses.where_predicates[0].field, "text");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_more_range_then_where_and_limit() {
    let ops = parse("SHOW MORE 1-400 WHERE text LIKE '%warning%' LIMIT 10").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowMore {
            window, clauses, ..
        } => {
            assert_eq!(*window, crate::ir::ShowMoreWindow::Range(1, 400));
            assert_eq!(clauses.where_predicates.len(), 1);
            assert_eq!(clauses.limit, Some(10));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_lines() {
    let ops = parse("SHOW LINES 10-25 OF 'src/signal_controller.cpp'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowLines {
            file,
            start_line,
            end_line,
            ..
        } => {
            assert_eq!(file, "src/signal_controller.cpp");
            assert_eq!(*start_line, 10);
            assert_eq!(*end_line, 25);
        }
        _ => panic!("wrong variant"),
    }
}

// ── Relaxed quoting (bare symbol names) ─────────────────────────────────

#[test]
fn parse_show_body_bare_name() {
    // SHOW body OF symbol_name (no quotes).
    let ops = parse("SHOW body OF sweep_expired").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowBody { symbol, .. } => {
            assert_eq!(symbol, "sweep_expired");
        }
        _ => panic!("wrong variant"),
    }
}
