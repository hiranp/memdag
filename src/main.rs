mod cli;

use clap::Parser;
use cli::{Cli, Commands};
use memdag::db::{default_db_path, open_connection};
use memdag::mcp::McpServer;
use memdag::models::{MemoryKind, MemoryStatus, RelationType, SessionLearning};
use memdag::store::{MemoryStore, RecordOptions, SearchOptions, format_search_results_for_llm};
use std::io::IsTerminal;
use std::str::FromStr;

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    let db_path = args.db.unwrap_or_else(default_db_path);

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let conn = open_connection(&db_path)?;
    let mut store = MemoryStore::new(conn);

    match args.command {
        Some(Commands::Serve) => {
            let mcp = McpServer::new(store);
            mcp.run_stdio()?;
        }
        Some(Commands::Record {
            kind,
            title,
            body,
            tags,
            supersedes,
            session,
            id,
        }) => {
            let memory_kind = MemoryKind::from_str(&kind).map_err(|e| anyhow::anyhow!(e))?;

            let record = store.record_memory(RecordOptions {
                id,
                kind: memory_kind,
                title,
                body,
                tags,
                supersedes_id: supersedes,
                session_id: session,
                embedding: None,
            })?;

            println!("Recorded memory: [{}] {}", record.id, record.title);
            println!("  Kind:   {}", record.kind);
            println!("  Status: {}", record.status);
            if let Some(tags) = record.tags {
                println!("  Tags:   {}", tags);
            }
        }
        Some(Commands::Link {
            source_id,
            target_id,
            relation,
        }) => {
            let rel_type = RelationType::from_str(&relation).map_err(|e| anyhow::anyhow!(e))?;

            store.link_entities(&source_id, &target_id, rel_type)?;
            println!(
                "Linked: [{}] --({})--> [{}]",
                source_id, rel_type, target_id
            );
        }
        Some(Commands::Search {
            query,
            kind,
            include_resolved,
            limit,
        }) => {
            let kind_filter = match kind {
                Some(k) => Some(MemoryKind::from_str(&k).map_err(|e| anyhow::anyhow!(e))?),
                None => None,
            };

            let results = store.search_memory(SearchOptions {
                query,
                kind: kind_filter,
                include_resolved,
                limit,
            })?;

            println!("{}", format_search_results_for_llm(&results));
        }
        Some(Commands::Get { id }) => match store.get_memory(&id)? {
            Some((mem, rels)) => {
                println!("ID:          {}", mem.id);
                println!("Title:       {}", mem.title);
                println!("Kind:        {}", mem.kind);
                println!("Status:      {}", mem.status);
                if let Some(tags) = mem.tags {
                    println!("Tags:        {}", tags);
                }
                if let Some(sess) = mem.session_id {
                    println!("Session:     {}", sess);
                }
                println!("Created At:  {}", mem.created_at);
                println!("Updated At:  {}", mem.updated_at);
                println!("\nBody:\n{}", mem.body);

                if !rels.is_empty() {
                    println!("\nRelations:");
                    for r in rels {
                        let arrow = if r.direction == memdag::models::EdgeDirection::Outgoing {
                            "➔"
                        } else {
                            "◄"
                        };
                        let rel_label = if r.direction == memdag::models::EdgeDirection::Outgoing {
                            r.relation_type.as_str().to_string()
                        } else {
                            r.relation_type.inverse().to_string()
                        };
                        println!(
                            "  - {} {} [{}] {} ({:?})",
                            rel_label,
                            arrow,
                            r.target_id,
                            r.target_title.unwrap_or_default(),
                            r.target_status
                        );
                    }
                }
            }
            None => {
                println!("Memory with ID '{}' not found.", id);
            }
        },
        Some(Commands::List {
            status,
            kind,
            limit,
        }) => {
            let status_filter = match status {
                Some(s) => Some(MemoryStatus::from_str(&s).map_err(|e| anyhow::anyhow!(e))?),
                None => None,
            };
            let kind_filter = match kind {
                Some(k) => Some(MemoryKind::from_str(&k).map_err(|e| anyhow::anyhow!(e))?),
                None => None,
            };

            let list = store.list_memories(status_filter, kind_filter, limit)?;
            if list.is_empty() {
                println!("No memories found.");
            } else {
                for mem in list {
                    println!(
                        "[{}] {:<9} {:<10} {}",
                        mem.id,
                        format!("({})", mem.kind),
                        mem.status,
                        mem.title
                    );
                }
            }
        }
        Some(Commands::Consolidate {
            session,
            title,
            body,
            kind,
            tags,
            no_purge,
        }) => {
            let learning_kind = MemoryKind::from_str(&kind).map_err(|e| anyhow::anyhow!(e))?;

            let learnings = vec![SessionLearning {
                title,
                body,
                tags,
                kind: learning_kind,
            }];

            let summary = store.consolidate_session(&session, learnings, !no_purge)?;
            println!("Consolidated session '{}':", summary.session_id);
            println!("  Learnings recorded:  {}", summary.learnings_recorded);
            println!("  Ephemeral purged:    {}", summary.ephemeral_purged);
            println!("  Ephemeral archived:  {}", summary.ephemeral_archived);
            println!("  Recorded IDs:        {:?}", summary.recorded_ids);
        }
        Some(Commands::Resolve { id, note }) => {
            let updated = store.resolve_memory(&id, note.as_deref())?;
            println!(
                "Resolved memory [{}] {} (status: {})",
                updated.id, updated.title, updated.status
            );
        }
        Some(Commands::Stats) => {
            let stats = store.stats()?;
            println!("Database Statistics ({:?}):", db_path);
            println!("  Total Memories:       {}", stats.total_memories);
            println!("  Active Memories:      {}", stats.active_memories);
            println!("  Superseded Memories:  {}", stats.superseded_memories);
            println!("  Resolved Memories:    {}", stats.resolved_memories);
            println!("  Ephemeral Memories:   {}", stats.ephemeral_memories);
            println!("  Total Relations:      {}", stats.total_relations);
            if !stats.relations_by_type.is_empty() {
                println!("  Relations Breakdown:");
                for (rel, count) in stats.relations_by_type {
                    println!("    - {:<15} {}", rel, count);
                }
            }
        }
        None => {
            // Default behavior: if stdin is not a terminal (e.g. piped or spawned by MCP client), run MCP server.
            // If it is a terminal, print help.
            if !std::io::stdin().is_terminal() {
                let mcp = McpServer::new(store);
                mcp.run_stdio()?;
            } else {
                println!(
                    "memdag v{} - Relational DAG & FTS5 memory engine with sqlite-vec",
                    env!("CARGO_PKG_VERSION")
                );
                println!(
                    "Run 'memdag --help' for CLI usage or 'memdag serve' to start the stdio MCP server."
                );
            }
        }
    }

    Ok(())
}
