# 0006. One installer for every harness

- Status: accepted
- Date: 2026-10-31
- Refs: ADR-0004, ADR-0005, hiranp/memdag#1

## Context

memdag had two partial installers.
- `memdag mcp install` merged an MCP server entry into Claude Code, OpenCode or Antigravity
  configs. It wrote OpenCode's V1 shape, dropped the comments of `opencode.jsonc` (with a
  backup), and left Codex's TOML to the user.
- `memdag work install` wrote the cheap-model worker and an instructions block for Claude
  Code and OpenCode only.

Issue #1 collected what a study of codegraph, Graphify and hief suggested:
- OpenCode V2's server shape with `codemode: false`;
- comment-preserving edits;
- Claude Code tool approvals;
- one target per harness with detect/install/uninstall/print-config and `--refresh`;
- Codex, Cursor, Gemini CLI, Copilot and Kiro;
- installing the skill instead of asking users to symlink it;
- optionally, a session hook.

Each harness's files were checked against its own documentation (OpenCode V2, Claude Code,
Codex, Cursor, Gemini CLI, VS Code, Kiro and Antigravity docs, October 2026). Where only
codegraph's installer source was available, that is noted in the code.

## Decision

`memdag install` / `memdag uninstall` (`src/install.rs`), with one `Target` per harness:
`claude-code`, `opencode`, `codex`, `cursor`, `gemini`, `vscode`, `kiro`, `antigravity`.

- **Parts.** For each target:
  - MCP entry, worker subagent, instructions, skill and approvals, each skippable with a
    `--no-*` flag;
  - an opt-in Claude Code SessionStart hook (`--hooks`, running `memdag hook session-start`,
    which prints one line about waiting upkeep and never fails).

  `--target auto` (the default) picks the harnesses whose config directories exist. Also
  available: `--global` (user-level files), `--print-config` (show, write nothing) and
  `--refresh` (only targets that already have a memdag entry).
- **Editing.**
  - JSON and JSONC are edited through a concrete syntax tree (`jsonc-parser`), so comments,
    trailing commas, formatting and other servers survive.
  - TOML goes through `toml_edit`.
  - A file that doesn't parse is refused, never rewritten. The MCP entry is written first, so
    a refusal leaves nothing half-installed.
  - No-op writes are skipped. Writes are atomic and follow symlinks.
- **Ownership.**
  - Uninstall removes only what memdag wrote: its key (and any container left empty), marked
    blocks, files carrying memdag's marker, and skill folders with memdag's stamp file.
  - Files several targets share (`AGENTS.md`, `.agents/skills/memdag`) are kept while a target
    that isn't being uninstalled still has memdag configured.
  - Files memdag didn't write are never replaced.
- **OpenCode** gets the V2 shape: `mcp.servers.memdag` with `type: local`, an argv `command`,
  `environment` and `codemode: false`. Without `codemode: false`, V2 exposes the tools only
  through Code Mode's JavaScript `execute` tool, which a small worker model handles badly. V2
  still reads V1, but V1 can't read V2, so `--opencode-v1` writes the old shape. Writing
  either shape migrates the other away.
- **Command path.** Project configs say `memdag` when that resolves on `PATH` to the same
  binary, so a committed config works for every collaborator. Global configs use the absolute
  path, as does Antigravity (its macOS app doesn't inherit the shell `PATH`).
- **Workers.** Written wherever a harness has subagent files with a per-agent model:
  Claude Code (`model: haiku` by default), OpenCode, Cursor, Gemini CLI, Codex (TOML) and
  Copilot. Kiro and Antigravity have no documented equivalent; their prompts fall back to the
  harness's own subagents.
- **Approvals**, where a file can grant them, are limited to the worker's two tools:
  - Claude Code: `permissions.allow`;
  - OpenCode: allow rules in the worker file;
  - Kiro: `autoApprove` in the entry.

  Elsewhere the installer says to approve them once in the harness's UI.
- **Existing commands.**
  - `memdag mcp install|uninstall --client <target>` writes or removes only the MCP entry,
    and `--path`/`--key` still target any file (e.g. Windsurf).
  - `memdag work install|uninstall` writes or removes only the worker, instructions and
    approvals.

  Both now run through the same code.

## Not done, and why

- **MCP sampling.** Under sampling the server asks the client's model for completions. It
  would let memdag run batches without a subagent, but Claude Code and OpenCode don't document
  support for it. Every harness that might support it (e.g. VS Code) now has a worker
  subagent anyway. It would also need two-way requests in the stdio server, which today
  answers one request at a time. Revisit if a primary harness ships it.
- **An OpenCode plugin hook.** It would be the counterpart of the Claude Code SessionStart
  hook. OpenCode plugins are JavaScript modules with an API that changed between V1 and V2;
  maintaining one is a cost the one-line reminder doesn't justify. The instructions block
  already tells OpenCode agents about upkeep.

## Consequences

- One command sets up every harness on a machine, and one command removes it.
- Two new dependencies: `jsonc-parser` (with its `cst` feature, which adds no dependencies of
  its own) and `toml_edit`. `serde_json` now preserves key order, so settings files keep
  theirs.
- Harness formats change. The paths and shapes live in one module, with a test per target
  for an idempotent install and a trace-free uninstall.
