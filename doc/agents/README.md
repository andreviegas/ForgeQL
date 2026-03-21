# ForgeQL Agent Integration

Distributable agent configuration files that teach AI coding agents how to use ForgeQL correctly.

## The Problem

AI agents connected to the ForgeQL MCP server often drift mid-task:
- They fall back to local filesystem tools (grep, find, cat) even though the workspace may be empty
- They brute-force read code with `SHOW LINES 1-500` instead of using `FIND symbols WHERE`
- They dump entire function bodies instead of using progressive disclosure

## The Solution

These files **lock the agent to ForgeQL tools only** and provide behavioral rules, query strategies, and workflow recipes that prevent drift.

---

## Quick Start

### Prerequisites

1. ForgeQL binary installed and accessible
2. MCP server configured in your editor (see your editor's MCP docs)
3. Source registered: `echo "CREATE SOURCE 'myproject' FROM 'https://...'" | forgeql --data-dir /path/to/data`

### VS Code (GitHub Copilot)

Copy the agent file and reference docs to your project:

```bash
mkdir -p .github/agents/references
cp doc/agents/forgeql.agent.md .github/agents/
cp doc/agents/references/*.md .github/agents/references/
```

The agent will appear in Copilot's agent picker as **"ForgeQL code explorer"**.

**Key feature:** `tools: [forgeql/*]` in the frontmatter restricts the agent to ForgeQL MCP tools only — it cannot fall back to grep/find/cat.

### Claude Code

Copy the instructions file to your project root:

```bash
cp doc/agents/claude-code/CLAUDE.md .
```

Or use the platform-agnostic version:

```bash
cp doc/agents/AGENTS.md .
```

### Cursor

Copy the rules file to your project root:

```bash
cp doc/agents/cursor/.cursorrules .
```

---

## Verification

After installing, verify the agent works correctly:

1. Open your editor and invoke the ForgeQL agent
2. Ask it to run `SHOW SOURCES` — it should use ForgeQL MCP tools, not terminal commands
3. Ask it to "find all functions with more than 50 lines" — it should use:
   ```sql
   FIND symbols WHERE node_kind = 'function_definition' WHERE lines >= 50 ORDER BY lines DESC
   ```
   Not grep or find commands.

---

## File Overview

```
doc/agents/
├── forgeql.agent.md              # VS Code Copilot Custom Agent (tools locked)
├── AGENTS.md                     # Platform-agnostic workspace instructions
├── README.md                     # This file
├── references/
│   ├── query-strategy.md         # Decision tree + anti-patterns
│   ├── recipes.md                # Workflow templates (dead code, refactoring, audits)
│   └── syntax-quick-ref.md       # Condensed command and field reference
├── claude-code/
│   └── CLAUDE.md                 # Claude Code adapter
└── cursor/
    └── .cursorrules              # Cursor adapter
```

### What each file does

| File | Platform | Tool Lock | Purpose |
|---|---|---|---|
| `forgeql.agent.md` | VS Code Copilot | **Yes** (`tools: [forgeql/*]`) | Full agent with behavioral rules + reference doc links |
| `AGENTS.md` | VS Code / Claude Code | No | Workspace-level instructions (both platforms read this) |
| `CLAUDE.md` | Claude Code | No | Claude Code native format |
| `.cursorrules` | Cursor | No | Cursor native format |

**Note:** Only `forgeql.agent.md` enforces tool restriction via frontmatter. The other formats don't support tool locking — they rely on strong behavioral instructions + the MCP server's built-in `with_instructions()` guidance.

---

## How It Works

Three layers of defense against agent drift:

1. **Tool restriction** (`forgeql.agent.md`): The agent literally cannot call grep/find/cat — only `forgeql/*` tools are available.

2. **Behavioral instructions** (all files): Clear rules like "never fall back to local filesystem" and the two-step workflow: FIND → SHOW LINES.

3. **MCP server blocking** (built into ForgeQL): SHOW commands returning more than 40 lines without explicit LIMIT are blocked with a guidance message. This teaches the agent on first contact, even without any agent files installed.
