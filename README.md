# ForgeQL

> **Declarative code transformation for the era of AI-assisted development.**

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

## The Problem

Every day, developers and AI agents face a common challenge: **making the same code change across multiple files**, reliably and safely.

- Rename a symbol across dozens of files
- Update a pattern in hundreds of locations
- Translate documentation
- Apply a new code standard to the entire project

Today, most solutions use **text-based tools** (regex, sed, grep) that:
- ❌ Don't understand code structure or syntax
- ❌ Break string literals and comments inadvertently
- ❌ Offer no rollback if something goes wrong
- ❌ Require manual verification across files

This is error-prone, slow, and doesn't scale.

## The Vision

ForgeQL is a **declarative, code-aware transformation server** where you describe *what* you want to change, not *how* to change it.

Think of it as **SQL for source code**: a simple, expressive language that lets you declare transformations over your entire codebase, with safety guarantees built in.

## How It Works

Instead of brittle regex commands, you describe transformations clearly:

```
Rename a symbol across your codebase
Migrate code patterns to new syntax
Translate documentation
Apply new coding standards
```

The server:
- ✅ Understands code structure (syntax-aware)
- ✅ Performs multi-file changes atomically
- ✅ Can verify changes with your build system
- ✅ Integrates with version control
- ✅ Exposes a clean API for AI agents and tools

## Key Characteristics

- **Language-agnostic**: Works across different programming languages
- **Server-based**: Runs as a service; clients connect via standard protocols
- **Declarative DSL**: A purpose-built query language for code transformations
- **Atomic & safe**: All changes succeed together or roll back completely
- **AI-native**: Designed to be called by AI agents and automation tools

## License

Apache License 2.0 — see [LICENSE](LICENSE).
