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
    /// Path to SQLite database file. Defaults to $MEMDAG_DB or standard application data path
    #[arg(short, long, global = true, env = "MEMDAG_DB")]
    pub db: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the Model Context Protocol (MCP) server over stdio
    Serve,

    /// Record a memory (decision, task, invariant, blocker, ephemeral)
    Record {
        /// Memory kind: decision, task, invariant, blocker, ephemeral
        #[arg(short, long)]
        kind: String,

        /// Short title
        #[arg(short, long)]
        title: String,

        /// Detailed body/reasoning
        #[arg(short, long)]
        body: String,

        /// Optional tags/synonyms
        #[arg(short, long)]
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
        #[arg(short, long)]
        tags: Option<String>,

        /// Do not purge ephemeral memories (archive them as resolved instead)
        #[arg(long, default_value_t = false)]
        no_purge: bool,
    },

    /// Show database summary statistics
    Stats,
}
