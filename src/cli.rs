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

    /// Wire memdag into your agent harnesses: MCP server entry, cheap-model worker subagent,
    /// instructions, skill and tool approvals. Detects installed harnesses by default.
    Install {
        /// auto (detected harnesses), all, or a comma-separated list of: claude-code,
        /// opencode, codex, cursor, gemini, vscode, kiro, antigravity
        #[arg(long, default_value = "auto")]
        target: String,
        /// User-global configs instead of this project's
        #[arg(long)]
        global: bool,
        /// Worker model (Claude Code default: haiku; others use their own default unless set)
        #[arg(long)]
        model: Option<String>,
        /// Only update harnesses that already have a memdag entry (e.g. after upgrading)
        #[arg(long)]
        refresh: bool,
        /// Print what would be written, change nothing
        #[arg(long)]
        print_config: bool,
        /// Skip the MCP server entry
        #[arg(long)]
        no_mcp: bool,
        /// Skip the memdag-worker subagent
        #[arg(long)]
        no_worker: bool,
        /// Skip the instructions block / rule file
        #[arg(long)]
        no_instructions: bool,
        /// Skip the memdag skill
        #[arg(long)]
        no_skill: bool,
        /// Don't auto-approve the worker's get_work / submit_work
        #[arg(long)]
        no_permissions: bool,
        /// Also add a Claude Code SessionStart hook that mentions waiting upkeep
        #[arg(long)]
        hooks: bool,
        /// OpenCode server shape: auto keeps an existing entry's shape (V2 for a new one), v2
        /// migrates a V1 entry, v1 writes the shape OpenCode V1 can read
        #[arg(long, value_enum, default_value = "auto")]
        opencode_format: OpencodeFormatArg,
    },

    /// Remove everything `memdag install` wrote (only memdag's own files, blocks and keys)
    Uninstall {
        /// auto (detected harnesses), all, or a comma-separated list (see install)
        #[arg(long, default_value = "auto")]
        target: String,
        #[arg(long)]
        global: bool,
    },

    /// Entry points for harness hooks (installed by `memdag install --hooks`)
    Hook {
        #[command(subcommand)]
        event: HookEvent,
    },

    /// Install or remove only the MCP server entry in one client's config
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
        /// authors, via the `gh` CLI (network, slow). Never part of a bare `import`. After the
        /// first run only comments updated since the last `--gh` import are fetched.
        #[arg(long)]
        gh: bool,

        /// With --gh: fetch every comment again and remove memories of deleted comments
        /// (an incremental fetch can't see deletions).
        #[arg(long, requires = "gh")]
        full: bool,

        /// Only scan commits in `<REV>..HEAD` (ADRs and notes are always rescanned)
        #[arg(long)]
        since: Option<String>,
    },
}

/// Agent harnesses, for `--client` (see `memdag install --target` for all of them at once).
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpClient {
    /// `.mcp.json` / `~/.claude.json`
    ClaudeCode,
    /// `opencode.json[c]` / `~/.config/opencode/opencode.json[c]`
    Opencode,
    /// `.codex/config.toml` / `~/.codex/config.toml`
    Codex,
    /// `.cursor/mcp.json` / `~/.cursor/mcp.json`
    Cursor,
    /// `.gemini/settings.json` / `~/.gemini/settings.json`
    Gemini,
    /// GitHub Copilot: `.vscode/mcp.json` / the VS Code user `mcp.json`
    Vscode,
    /// `.kiro/settings/mcp.json` / `~/.kiro/settings/mcp.json`
    Kiro,
    /// `.agents/mcp_config.json` / `~/.gemini/config/mcp_config.json`
    Antigravity,
}

impl McpClient {
    pub fn target(self) -> memdag::install::Target {
        use memdag::install::Target;
        match self {
            McpClient::ClaudeCode => Target::ClaudeCode,
            McpClient::Opencode => Target::Opencode,
            McpClient::Codex => Target::Codex,
            McpClient::Cursor => Target::Cursor,
            McpClient::Gemini => Target::Gemini,
            McpClient::Vscode => Target::Vscode,
            McpClient::Kiro => Target::Kiro,
            McpClient::Antigravity => Target::Antigravity,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum HookEvent {
    /// Print a one-line note about waiting memdag upkeep (Claude Code SessionStart hook)
    SessionStart,
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
        client: McpClient,
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
        client: McpClient,
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

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpencodeFormatArg {
    Auto,
    V1,
    V2,
}

impl From<OpencodeFormatArg> for memdag::install::OpencodeFormat {
    fn from(a: OpencodeFormatArg) -> Self {
        match a {
            OpencodeFormatArg::Auto => Self::Auto,
            OpencodeFormatArg::V1 => Self::V1,
            OpencodeFormatArg::V2 => Self::V2,
        }
    }
}
