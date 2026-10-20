# Mockingbird agent plugin

Everything a coding agent needs to install Mockingbird, connect an LLM, and create, serve and self-host mock APIs:

- `skills/`: the agent guides (`SKILL.md`), shared by every agent.
- `bin/mockingbird-mcp`: starts `mockingbird mcp`, installing Mockingbird first (`bin/install.sh`, a copy of the repository's `install.sh`) when it is missing.
- Manifests for each plugin system: `.claude-plugin/plugin.json` + `.mcp.json` (Claude Code), `plugin.json` + `mcp.json` (the portable Agent Plugins format, read by Codex and Cursor), `.cursor-plugin/plugin.json` (Cursor).

Install it, or set Mockingbird up in another agent, as described in [docs/agents.md](../../docs/agents.md).
