//! `Tree-sitter` parse-tree inspector for `ForgeQL` development.
//!
//! Parses a source file using the same tree-sitter grammar versions that
//! `ForgeQL` uses (pinned via the workspace Cargo.lock) and prints the
//! resulting syntax tree, making it easy to debug grammar behaviour,
//! phantom node issues, and ERROR-recovery edge cases.
//!
//! # Usage
//!
//! ```text
//! ts-debug <file> [options]
//!
//! Options:
//!   --lines N-M       Only show nodes whose start line falls in [N, M]
//!   --kind KIND       Only show nodes of this grammar kind
//!   --errors          Only show ERROR nodes and their named descendants
//!   --numbers         Shorthand for --kind number_literal
//!   --unnamed         Also show unnamed (anonymous) nodes
//!   --depth N         Limit output to N levels deep (default: unlimited)
//!   --help            Print this help message
//! ```
//!
//! # Supported file extensions
//!
//! | Extension            | Grammar           |
//! |----------------------|-------------------|
//! | `.c .cpp .cc .cxx`   | tree-sitter-cpp   |
//! | `.h .hpp .hxx`       | tree-sitter-cpp   |
//! | `.rs`                | tree-sitter-rust  |
//! | `.py`                | tree-sitter-python|

use std::env;
use std::fs;
use std::path::Path;
use std::process;

use tree_sitter::{Language, Node, Parser};

// ─── CLI options ────────────────────────────────────────────────────────────

struct Opts {
    file: String,
    line_range: Option<(usize, usize)>,
    kind_filter: Option<String>,
    errors_only: bool,
    unnamed: bool,
    max_depth: Option<usize>,
}

fn print_help() {
    println!(
        "Usage: ts-debug <file> [options]

Options:
  --lines N-M   Only show nodes whose start line falls in [N, M]
  --kind KIND   Only show nodes of this grammar kind
  --errors      Only show ERROR nodes and their named descendants
  --numbers     Shorthand for --kind number_literal
  --unnamed     Also show unnamed (anonymous) nodes
  --depth N     Limit output to N levels deep (default: unlimited)
  --help        Print this message

Supported extensions:
  .c  .cpp  .cc  .cxx  .h  .hpp  .hxx  → tree-sitter-cpp
  .rs                                   → tree-sitter-rust
  .py                                   → tree-sitter-python"
    );
}

fn parse_opts() -> Opts {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        process::exit(0);
    }

    let mut file = None;
    let mut line_range = None;
    let mut kind_filter = None;
    let mut errors_only = false;
    let mut unnamed = false;
    let mut max_depth = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--lines" => {
                let val = it.next().unwrap_or_else(|| {
                    eprintln!("error: --lines requires an argument, e.g. --lines 160-180");
                    process::exit(1);
                });
                line_range = Some(parse_range(val));
            }
            "--kind" => {
                kind_filter = Some(
                    it.next()
                        .unwrap_or_else(|| {
                            eprintln!("error: --kind requires an argument");
                            process::exit(1);
                        })
                        .clone(),
                );
            }
            "--errors" => errors_only = true,
            "--numbers" => kind_filter = Some("number_literal".to_owned()),
            "--unnamed" => unnamed = true,
            "--depth" => {
                let val = it.next().unwrap_or_else(|| {
                    eprintln!("error: --depth requires an argument");
                    process::exit(1);
                });
                max_depth = Some(val.parse::<usize>().unwrap_or_else(|_| {
                    eprintln!("error: --depth must be a non-negative integer");
                    process::exit(1);
                }));
            }
            other if !other.starts_with('-') => {
                file = Some(other.to_owned());
            }
            other => {
                eprintln!("error: unknown option '{other}'. Use --help for usage.");
                process::exit(1);
            }
        }
    }

    Opts {
        file: file.unwrap_or_else(|| {
            eprintln!("error: no input file specified. Use --help for usage.");
            process::exit(1);
        }),
        line_range,
        kind_filter,
        errors_only,
        unnamed,
        max_depth,
    }
}

fn parse_range(s: &str) -> (usize, usize) {
    let sep = s.find(['-', ':']).unwrap_or_else(|| {
        eprintln!("error: --lines range must be 'N-M' or 'N:M', got '{s}'");
        process::exit(1);
    });
    let lo = s[..sep].parse::<usize>().unwrap_or_else(|_| {
        eprintln!("error: invalid start line in range '{s}'");
        process::exit(1);
    });
    let hi = s[sep + 1..].parse::<usize>().unwrap_or_else(|_| {
        eprintln!("error: invalid end line in range '{s}'");
        process::exit(1);
    });
    (lo, hi)
}

// ─── Language detection ──────────────────────────────────────────────────────

fn detect_language(path: &Path) -> Option<Language> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .as_deref()
    {
        Some("c" | "cpp" | "cc" | "cxx" | "h" | "hpp" | "hxx") => {
            Some(tree_sitter_cpp::LANGUAGE.into())
        }
        Some("rs") => Some(tree_sitter_rust::LANGUAGE.into()),
        Some("py") => Some(tree_sitter_python::LANGUAGE.into()),
        _ => None,
    }
}

fn language_label(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .as_deref()
    {
        Some("c" | "cpp" | "cc" | "cxx" | "h" | "hpp" | "hxx") => "tree-sitter-cpp",
        Some("rs") => "tree-sitter-rust",
        Some("py") => "tree-sitter-python",
        _ => "unknown",
    }
}

// ─── Tree walker ─────────────────────────────────────────────────────────────

struct Walker<'a> {
    src: &'a [u8],
    opts: &'a Opts,
    /// True if we are currently inside (or are) an ERROR subtree.
    in_error_subtree: bool,
}

impl<'a> Walker<'a> {
    fn walk(&mut self, node: Node<'a>, depth: usize) {
        // Depth cap.
        if self.opts.max_depth.is_some_and(|max| depth > max) {
            return;
        }

        let line = node.start_position().row + 1; // 1-based

        // Line-range filter — skip subtrees that can't overlap.
        if let Some((lo, hi)) = self.opts.line_range {
            let node_end_line = node.end_position().row + 1;
            if node_end_line < lo || line > hi {
                return; // subtree entirely outside range
            }
        }

        let was_in_error = self.in_error_subtree;
        if node.is_error() {
            self.in_error_subtree = true;
        }

        let should_print = self.should_print(node, depth, line);
        if should_print {
            self.print_node(node, depth, line);
        }

        // Recurse into children.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, depth + 1);
        }

        self.in_error_subtree = was_in_error;
    }

    fn should_print(&self, node: Node, _depth: usize, line: usize) -> bool {
        // Named filter (default: skip unnamed).
        if !self.opts.unnamed && !node.is_named() {
            return false;
        }

        // Line-range filter on the node's own start line.
        if self
            .opts
            .line_range
            .is_some_and(|(lo, hi)| line < lo || line > hi)
        {
            return false;
        }

        // errors-only: only print if we are inside (or are) an ERROR node.
        if self.opts.errors_only && !self.in_error_subtree && !node.is_error() {
            return false;
        }

        // Kind filter.
        if self
            .opts
            .kind_filter
            .as_deref()
            .is_some_and(|k| node.kind() != k)
        {
            return false;
        }

        true
    }

    fn print_node(&self, node: Node, depth: usize, line: usize) {
        let indent = "  ".repeat(depth);

        // Text preview — up to 60 chars, newlines replaced with ↵.
        let raw = &self.src[node.byte_range()];
        let preview: String = std::str::from_utf8(raw)
            .unwrap_or("‹binary›")
            .chars()
            .take(60)
            .map(|c| if c == '\n' { '↵' } else { c })
            .collect();

        // Parent kind.
        let parent_kind = node.parent().map_or("", |p| p.kind());

        // Markers.
        let mut markers = String::new();
        if node.is_error() {
            markers.push_str("  [ERROR]");
        }
        if node.is_missing() {
            markers.push_str("  [MISSING]");
        }
        if !node.is_named() {
            markers.push_str("  [unnamed]");
        }
        if node.parent().is_some_and(|p| p.is_error()) {
            markers.push_str("  [in-ERROR]");
        } else if self.in_error_subtree && !node.is_error() {
            markers.push_str("  [in-ERROR-subtree]");
        }

        // Column info.
        let col = node.start_position().column + 1;

        println!(
            "{indent}L{line}:{col:<4}  {kind:<35}  {preview:?}  parent={parent_kind}{markers}",
            kind = node.kind(),
        );
    }
}

// ─── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let opts = parse_opts();

    let path = Path::new(&opts.file);
    let lang = detect_language(path).unwrap_or_else(|| {
        eprintln!(
            "error: unsupported file extension '{ext}'.\n\
             Supported: .c .cpp .cc .cxx .h .hpp .hxx .rs .py",
            ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("(none)")
        );
        process::exit(1);
    });

    let src = fs::read(path).unwrap_or_else(|e| {
        eprintln!("error: cannot read {}: {e}", path.display());
        process::exit(1);
    });

    let mut parser = Parser::new();
    parser.set_language(&lang).unwrap_or_else(|e| {
        eprintln!("error: failed to initialise parser: {e}");
        process::exit(1);
    });

    let tree = parser.parse(&src, None).unwrap_or_else(|| {
        eprintln!("error: parse returned None (empty source?)");
        process::exit(1);
    });

    // ── Header ──────────────────────────────────────────────────────────────
    let root = tree.root_node();
    println!("File:        {}", path.display());
    println!("Grammar:     {}", language_label(path));
    println!("Bytes:       {}", src.len());
    println!("Has errors:  {}", root.has_error());
    if let Some((lo, hi)) = opts.line_range {
        println!("Line filter: {lo}-{hi}");
    }
    if let Some(ref k) = opts.kind_filter {
        println!("Kind filter: {k}");
    }
    println!("{}", "─".repeat(72));

    // ── Walk ────────────────────────────────────────────────────────────────
    let mut walker = Walker {
        src: &src,
        opts: &opts,
        in_error_subtree: false,
    };
    walker.walk(root, 0);
}
