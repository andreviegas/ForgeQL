# Contributing to ForgeQL

Thank you for your interest in ForgeQL! This project is open to contributions from
anyone, and I especially welcome Rust developers who can help improve the codebase.

## Background

I'm Andre Viegas, a C/C++ embedded developer. I designed ForgeQL as a proof of concept
— the idea, architecture, language design, and testing strategy are mine, but the Rust
implementation was generated entirely by AI. This means the code works and passes a
strict quality gate (`clippy::pedantic`, 257 tests, zero warnings), but it may not
always follow idiomatic Rust conventions that an experienced Rustacean would use.

**This is where you come in.**

## How You Can Help

- **Idiomatic Rust** — spot patterns that could be more natural in Rust (lifetime
  management, error handling, trait design, iterator chains, etc.)
- **Performance** — the indexer and clause pipeline work, but there are certainly
  opportunities for optimization
- **Multi-language support** — ForgeQL currently indexes C/C++ via tree-sitter.
  Adding Python, Rust, TypeScript, or other grammars requires only a small
  `extract_name()` function per language (~20 lines)
- **New features** — see the issue tracker for ideas, or propose your own
- **Documentation** — better examples, tutorials, typo fixes — all welcome
- **Testing** — more integration tests, edge cases, real-world validation

## Development Setup

```bash
git clone https://github.com/andreviegas/ForgeQL.git
cd ForgeQL
cargo build
cargo test
```

The project pins Rust 1.94.0 via `rust-toolchain.toml`. Rustup will install it
automatically on first build.

## Quality Gate

Every PR must pass before merge:

```bash
cargo clippy --workspace --all-targets   # zero warnings
cargo test --workspace                    # all tests pass
cargo build --release                     # clean release build
```

The workspace uses `clippy::pedantic` and `clippy::nursery` lint levels.
No `unwrap()` in library code (`crates/forgeql-core/src/`). Tests may use
`unwrap()` and `expect()`.

## Submitting Changes

1. Fork the repository
2. Create a feature branch (`git checkout -b my-feature`)
3. Make your changes and ensure the quality gate passes
4. Open a pull request with a clear description of what changed and why

Small, focused PRs are preferred over large sweeping changes. If you're planning
something significant, open an issue first to discuss the approach.

## Code of Conduct

Be respectful, be constructive, be kind. We're all here to learn and build
something useful together.

## License

By contributing, you agree that your contributions will be licensed under the
Apache License 2.0, the same license as the project.
