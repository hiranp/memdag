use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "memdag",
    author = "Hiran Patel",
    version = env!("CARGO_PKG_VERSION"),
    about = "Relational DAG & FTS5 memory engine with sqlite-vec for AI coding harnesses",
    long_about = "A zero-daemon, local-first memory engine with SQLite WAL, BM25 FTS5, relational DAG supersession, and stdio Model Context Protocol (MCP) server."
)]
pub struct Cli {
    /// Path to SQLite database file. Defaults to $MEMDAG_DB, or ./.memdag/memdag.db in the
    /// nearest ancestor with .git/.memdag, or the standard application data path
    #[arg(short, long, global = true, env = "MEMDAG_DB")]
    pub db: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the Model Context Protocol (MCP) server over stdio
    Serve,

    /// Install or remove memdag as an MCP server in a client config (project or global scope)
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },

    /// Record a memory (decision, task, invariant, blocker, learning, ephemeral)
    Record {
        /// Memory kind: decision, task, invariant, blocker, learning, ephemeral
        #[arg(short, long)]
        kind: String,

        /// Short title
        #[arg(short, long)]
        title: String,

        /// Detailed body/reasoning
        #[arg(short, long)]
        body: String,

        /// Optional tags/synonyms
        #[arg(long)]
        tags: Option<String>,

        /// Optional ID of an older memory this new memory supersedes
        #[arg(short, long)]
        supersedes: Option<String>,

        /// Optional session identifier
        #[arg(long)]
        session: Option<String>,

        /// Optional explicit ID
        #[arg(long)]
        id: Option<String>,

        /// Repo-relative files this memory is about, comma-separated. The memory is stamped
        /// with the current git HEAD and flagged STALE in search once these paths change.
        #[arg(long, value_delimiter = ',')]
        paths: Vec<String>,

        /// Commit the memory is true as of (hex). Defaults to HEAD when --paths is given.
        #[arg(long)]
        commit: Option<String>,
    },

    /// Link two memories in the DAG
    Link {
        /// Source memory ID
        source_id: String,

        /// Target memory ID
        target_id: String,

        /// Relation type: supersedes, depends_on, blocks, references
        relation: String,
    },

    /// Search memories using BM25 FTS5 with 1-hop DAG expansion
    Search {
        /// Query keywords or phrase
        query: String,

        /// Filter by kind
        #[arg(short, long)]
        kind: Option<String>,

        /// Include resolved and superseded memories
        #[arg(long, default_value_t = false)]
        include_resolved: bool,

        /// Limit number of results
        #[arg(short, long, default_value_t = 10)]
        limit: usize,

        /// Output budget in estimated tokens (~4 chars each); 0 = unlimited
        #[arg(long, default_value_t = memdag::store::DEFAULT_MAX_TOKENS)]
        max_tokens: usize,
    },

    /// Get a specific memory and its immediate DAG relations
    Get {
        /// Memory ID
        id: String,
    },

    /// List memories
    List {
        /// Filter by status: active, superseded, resolved, ephemeral
        #[arg(short, long)]
        status: Option<String>,

        /// Filter by kind
        #[arg(short, long)]
        kind: Option<String>,

        /// Max results
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },

    /// List claimable tasks/blockers: active, with no incoming active `blocks` edge
    Ready {
        /// Max results
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },

    /// Consolidate session learnings and clean up ephemeral memories
    Consolidate {
        /// Session ID
        #[arg(short, long)]
        session: String,

        /// Learning title
        #[arg(short, long)]
        title: String,

        /// Learning body
        #[arg(short, long)]
        body: String,

        /// Learning kind (default: decision)
        #[arg(short, long, default_value = "decision")]
        kind: String,

        /// Tags
        #[arg(long)]
        tags: Option<String>,

        /// Do not purge ephemeral memories (archive them as resolved instead)
        #[arg(long, default_value_t = false)]
        no_purge: bool,
    },

    /// Mark a memory as resolved
    Resolve {
        /// Memory ID
        id: String,

        /// Optional resolution note
        #[arg(short, long)]
        note: Option<String>,
    },

    /// Show database summary statistics
    Stats,

    /// Embed memories that have no vector yet, using $MEMDAG_EMBED_CMD (stdin text -> JSON
    /// array of 384 floats). Enables hybrid BM25 + vector search.
    Embed,

    /// List active memories whose cited paths changed since they were recorded
    Stale,

    /// Export all memories and relations as JSON (to a file, or stdout if no path given)
    Export {
        /// Output file path. Prints to stdout if omitted.
        path: Option<PathBuf>,
    },

    /// Bulk memory upkeep for a cheap worker model (keyword expansion, STALE checks).
    /// memdag never calls a model: it prints the work, you run it on e.g. Haiku or Flash.
    Work {
        #[command(subcommand)]
        action: WorkAction,
    },

    /// Rebuild/refresh the index from git-tracked sources (idempotent upsert by stable id).
    /// With no source flags, imports the local sources (git, adr, notes); GitHub comments need --gh.
    Import {
        /// Repository root. Defaults to the nearest ancestor of the cwd containing `.git`.
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Commits with a `Decision:` trailer -> decision `GIT-<sha7>`
        #[arg(long)]
        git: bool,

        /// `docs/adr/NNNN-*.md` -> decision `ADR-NNNN`
        #[arg(long)]
        adr: bool,

        /// Memory notes (`docs/memory/*.md`, or `[write_through] dir` from memdag.toml)
        #[arg(long)]
        notes: bool,

        /// GitHub issue comments starting with `<!-- handoff -->` from OWNER/MEMBER/COLLABORATOR
        /// authors, via the `gh` CLI (network, slow). Never part of a bare `import`.
        #[arg(long)]
        gh: bool,

        /// Only scan commits in `<REV>..HEAD` (ADRs and notes are always rescanned)
        #[arg(long)]
        since: Option<String>,
    },
}

/// MCP client presets for `memdag mcp install|uninstall --client`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpClient {
    /// `.mcp.json` / `~/.claude.json`, key `mcpServers`
    ClaudeCode,
    /// `opencode.json[c]` / `~/.config/opencode/opencode.json[c]`, key `mcp`
    Opencode,
    /// `~/.gemini/config/mcp_config.json`, key `mcpServers` (always the global file)
    Antigravity,
}

#[derive(Subcommand, Debug)]
pub enum McpAction {
    /// Register memdag as an MCP server (writes/merges an `mcpServers` entry by default)
    Install {
        /// Install into the user-global config instead of the project-local one
        #[arg(long)]
        global: bool,

        /// Explicit config file to merge into (overrides --global's default path)
        #[arg(long)]
        path: Option<PathBuf>,

        /// Client preset: picks the default config path, top-level key and entry shape
        #[arg(long, value_enum, default_value = "claude-code")]
        client: McpClient,

        /// Top-level JSON key servers are nested under (overrides the client's default).
        /// Most clients (Claude Code, Cursor, Windsurf) use "mcpServers"; VS Code uses
        /// "servers" (e.g. for .vscode/mcp.json); OpenCode uses "mcp".
        #[arg(long)]
        key: Option<String>,
    },

    /// Remove memdag's MCP server entry from a client config
    Uninstall {
        /// Remove from the user-global config instead of the project-local one
        #[arg(long)]
        global: bool,

        /// Explicit config file to remove from (overrides --global's default path)
        #[arg(long)]
        path: Option<PathBuf>,

        /// Client preset (see `install --client`)
        #[arg(long, value_enum, default_value = "claude-code")]
        client: McpClient,

        /// Top-level JSON key servers are nested under (see `install --key`).
        #[arg(long)]
        key: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum WorkAction {
    /// Print the next batch as a prompt for a worker model (or as JSON with --json)
    Next {
        /// expand (search keywords) or verify (check STALE memories)
        task: String,
        /// Items in the batch
        #[arg(long, default_value_t = memdag::work::DEFAULT_BATCH)]
        limit: usize,
        /// Print the raw batch JSON instead of a ready-to-send prompt
        #[arg(long)]
        json: bool,
    },
    /// Apply a worker's reply (JSON, code fences and stray prose tolerated)
    Submit {
        /// File with the reply; reads stdin if omitted or "-"
        file: Option<PathBuf>,
        /// Task, if the reply doesn't name one
        #[arg(long)]
        task: Option<String>,
    },
    /// Install the `memdag-worker` subagent (cheap model) and a marked memdag block in the
    /// harness's instructions file (CLAUDE.md / AGENTS.md)
    Install {
        #[arg(long, value_enum, default_value = "claude-code")]
        client: WorkerClient,
        /// User-global files instead of the project's
        #[arg(long)]
        global: bool,
        /// Model for the worker. Claude Code: an alias such as haiku (default) or a model id.
        /// OpenCode: provider/model (no default; the worker then inherits the session's model).
        #[arg(long)]
        model: Option<String>,
        /// Don't touch CLAUDE.md / AGENTS.md
        #[arg(long)]
        no_instructions: bool,
        /// Don't auto-approve the worker's two memdag tools (get_work, submit_work). Claude
        /// Code: `permissions.allow` in .claude/settings.json; OpenCode: rules in the worker
        /// file itself.
        #[arg(long)]
        no_permissions: bool,
    },
    /// Remove what `work install` wrote (only files and blocks memdag marked as its own)
    Uninstall {
        #[arg(long, value_enum, default_value = "claude-code")]
        client: WorkerClient,
        #[arg(long)]
        global: bool,
    },
    /// Print the instructions an agent follows to run a task on a cheap subagent
    Prompt {
        /// expand_keywords or verify_stale
        name: String,
        /// Maximum batches
        #[arg(long, default_value_t = memdag::work::DEFAULT_ROUNDS)]
        rounds: usize,
    },
}

/// Harnesses `memdag work install` can add the worker subagent to.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerClient {
    /// `.claude/agents/memdag-worker.md` + CLAUDE.md
    ClaudeCode,
    /// `.opencode/agents/memdag-worker.md` + AGENTS.md
    Opencode,
}
