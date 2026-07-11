//! CLI argument definitions and run-mode detection.

use std::io::IsTerminal;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

// -----------------------------------------------------------------------
// CLI structs
// -----------------------------------------------------------------------

/// `ForgeQL` — AST-aware code search and transformation over indexed repos.
#[derive(Parser, Debug)]
#[command(
    name = "forgeql",
    version,
    about,
    long_about = "ForgeQL — AST-aware code search and transformation over indexed repositories.\n\n\
Modes:\n  \
  forgeql run '<FQL>'   Execute one ForgeQL statement and exit\n  \
  forgeql gc            Reclaim disk by deleting stale cache versions\n  \
  forgeql               Interactive REPL (stdin is a terminal)\n  \
  forgeql < file.fql    Pipe mode (stdin is not a terminal)\n  \
  forgeql --mcp         Run as an MCP server over stdio (for AI agents)\n\n\
Run `forgeql <command> --help` for command-specific options (e.g. `forgeql gc --help`)."
)]
pub(crate) struct Cli {
    /// Root directory for bare repos and worktrees (created if absent).
    ///
    /// Global: accepted before or after a subcommand (e.g. `forgeql gc --data-dir …`).
    #[arg(
        short,
        long,
        default_value = "./data",
        env = "FORGEQL_DATA_DIR",
        global = true
    )]
    pub(crate) data_dir: PathBuf,

    /// Run as MCP server over stdio (for AI agents).
    #[arg(long)]
    pub(crate) mcp: bool,

    /// Write a CSV query-log to `{data-dir}/log/{source}.csv`.
    ///
    /// Each executed statement appends one row with timestamp, clipped command
    /// (first 80 chars), lines returned, approximate tokens sent and received.
    #[arg(long)]
    pub(crate) log_queries: bool,

    /// Write a verbose debug trace to `<FILE>` (diagnostic; off unless set).
    ///
    /// Installs the `debug_log!` sink in forgeql-core so instrumented internals
    /// (e.g. ordinal reassignment during reindex) append to the file. Hidden by
    /// default: inert unless this flag is passed at launch.
    #[arg(long, value_name = "FILE")]
    pub(crate) debug: Option<PathBuf>,

    /// Increase verbosity (`-v` = info, `-vv` = debug, `-vvv` = trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub(crate) verbose: u8,

    /// Output format: text (default, human), compact (CSV), json.
    ///
    /// The CLI defaults to human-readable `text`; agents on the MCP surface get
    /// compact CSV independently of this flag. Pass `--format compact` for
    /// token-efficient CSV in scripts.
    #[arg(long, default_value = "text", global = true)]
    pub(crate) format: CliFormat,

    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

/// CLI output format.
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub(crate) enum CliFormat {
    /// Human-friendly terminal output.
    Text,
    /// Token-efficient compact CSV (default).
    #[default]
    Compact,
    /// Full structured JSON.
    Json,
}

/// Available CLI subcommands.
#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
    /// Execute a `ForgeQL` statement and exit.
    Run {
        /// `ForgeQL` statement string (e.g. `"FIND symbols WHERE name LIKE 'set%'"`).
        fql: String,
        /// Session ID from a previous USE command.
        #[arg(long, env = "FORGEQL_SESSION")]
        session: Option<String>,
    },

    /// Delete stale columnar cache version directories to reclaim disk.
    Gc {
        /// Restrict to one source (default: every registered source).
        #[arg(long)]
        source: Option<String>,
        /// Keep the N newest older versions in addition to the current one.
        #[arg(long, default_value_t = 0)]
        keep: usize,
        /// Delete every version, including the current and any newer ones.
        #[arg(long)]
        all: bool,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

// -----------------------------------------------------------------------
// Mode detection
// -----------------------------------------------------------------------

/// Active run-mode chosen at startup.
pub(crate) enum Mode {
    /// MCP server over stdio (`--mcp` flag).
    Mcp,
    /// Interactive REPL (stdin is a TTY, no subcommand).
    Repl,
    /// Pipe mode (stdin is not a TTY, no subcommand).
    Pipe,
    /// One-shot: execute one FQL statement and exit (`run` subcommand).
    OneShot {
        fql: String,
        session: Option<String>,
    },
    /// `gc` subcommand: preview, then delete stale cache version dirs.
    Gc {
        source: Option<String>,
        keep: usize,
        all: bool,
        yes: bool,
    },
}

/// Detect the run mode from CLI arguments using the real stdin TTY state.
pub(crate) fn detect_mode(cli: &Cli) -> Mode {
    detect_mode_impl(cli, std::io::stdin().is_terminal())
}

/// Core mode-detection logic, injectable for unit testing.
///
/// `stdin_is_terminal` is passed explicitly so the function is fully
/// exercisable in unit tests without controlling the real stdin descriptor.
///
/// Priority order:
/// 1. `--mcp` flag → [`Mode::Mcp`]
/// 2. `run` subcommand → [`Mode::OneShot`]
/// 3. stdin is not a terminal → [`Mode::Pipe`]
/// 4. fallback → [`Mode::Repl`]
pub(crate) fn detect_mode_impl(cli: &Cli, stdin_is_terminal: bool) -> Mode {
    if cli.mcp {
        return Mode::Mcp;
    }
    if let Some(Commands::Run {
        ref fql,
        ref session,
    }) = cli.command
    {
        return Mode::OneShot {
            fql: fql.clone(),
            session: session.clone(),
        };
    }

    if let Some(Commands::Gc {
        ref source,
        keep,
        all,
        yes,
    }) = cli.command
    {
        return Mode::Gc {
            source: source.clone(),
            keep,
            all,
            yes,
        };
    }
    if !stdin_is_terminal {
        return Mode::Pipe;
    }
    Mode::Repl
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::panic, reason = "test code")]
mod tests {
    use super::*;

    /// Construct a minimal [`Cli`] for unit tests.
    fn make_cli(mcp: bool, command: Option<Commands>) -> Cli {
        Cli {
            data_dir: PathBuf::from("./data"),
            mcp,
            log_queries: false,
            debug: None,
            verbose: 0,
            format: CliFormat::Compact,
            command,
        }
    }

    // ------------------------------------------------------------------
    // CliFormat
    // ------------------------------------------------------------------

    #[test]
    fn cli_format_default_is_compact() {
        assert!(matches!(CliFormat::default(), CliFormat::Compact));
    }

    // ------------------------------------------------------------------
    // detect_mode_impl — Mcp branch (highest priority)
    // ------------------------------------------------------------------

    #[test]
    fn detect_mcp_flag_returns_mcp() {
        let cli = make_cli(true, None);
        assert!(matches!(detect_mode_impl(&cli, true), Mode::Mcp));
    }

    #[test]
    fn detect_mcp_overrides_run_subcommand() {
        // --mcp takes priority even when `run` subcommand is present.
        let cli = make_cli(
            true,
            Some(Commands::Run {
                fql: "FIND symbols".into(),
                session: None,
            }),
        );
        assert!(matches!(detect_mode_impl(&cli, true), Mode::Mcp));
    }

    #[test]
    fn detect_mcp_overrides_pipe_stdin() {
        // --mcp takes priority even when stdin is not a terminal.
        let cli = make_cli(true, None);
        assert!(matches!(detect_mode_impl(&cli, false), Mode::Mcp));
    }

    // ------------------------------------------------------------------
    // detect_mode_impl — OneShot branch
    // ------------------------------------------------------------------

    #[test]
    fn detect_run_subcommand_returns_oneshot() {
        let cli = make_cli(
            false,
            Some(Commands::Run {
                fql: "FIND symbols".into(),
                session: None,
            }),
        );
        assert!(matches!(detect_mode_impl(&cli, true), Mode::OneShot { .. }));
    }

    #[test]
    fn detect_oneshot_carries_fql_string() {
        let cli = make_cli(
            false,
            Some(Commands::Run {
                fql: "FIND symbols WHERE name = 'main'".into(),
                session: None,
            }),
        );
        let Mode::OneShot { fql, .. } = detect_mode_impl(&cli, true) else {
            panic!("expected Mode::OneShot");
        };
        assert_eq!(fql, "FIND symbols WHERE name = 'main'");
    }

    #[test]
    fn detect_oneshot_session_is_none_when_not_provided() {
        let cli = make_cli(
            false,
            Some(Commands::Run {
                fql: "x".into(),
                session: None,
            }),
        );
        let Mode::OneShot { session, .. } = detect_mode_impl(&cli, true) else {
            panic!("expected Mode::OneShot");
        };
        assert!(session.is_none());
    }

    #[test]
    fn detect_oneshot_carries_session_id() {
        let cli = make_cli(
            false,
            Some(Commands::Run {
                fql: "x".into(),
                session: Some("sid-42".into()),
            }),
        );
        let Mode::OneShot { session, .. } = detect_mode_impl(&cli, true) else {
            panic!("expected Mode::OneShot");
        };
        assert_eq!(session.as_deref(), Some("sid-42"));
    }

    #[test]
    fn detect_run_subcommand_wins_over_non_tty_stdin() {
        // `run` subcommand has priority over Pipe mode.
        let cli = make_cli(
            false,
            Some(Commands::Run {
                fql: "x".into(),
                session: None,
            }),
        );
        assert!(matches!(
            detect_mode_impl(&cli, false),
            Mode::OneShot { .. }
        ));
    }

    // ------------------------------------------------------------------
    // detect_mode_impl — Pipe branch
    // ------------------------------------------------------------------

    #[test]
    fn detect_pipe_when_stdin_not_terminal() {
        let cli = make_cli(false, None);
        assert!(matches!(detect_mode_impl(&cli, false), Mode::Pipe));
    }

    // ------------------------------------------------------------------
    // detect_mode_impl — Repl (fallback)
    // ------------------------------------------------------------------

    #[test]
    fn detect_repl_is_fallback_for_tty_no_subcommand() {
        let cli = make_cli(false, None);
        assert!(matches!(detect_mode_impl(&cli, true), Mode::Repl));
    }
}
