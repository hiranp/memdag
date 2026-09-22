mod cli;

use clap::Parser;
use cli::{Cli, Commands, McpAction};
use memdag::db::{default_db_path, open_connection};
use memdag::mcp::McpServer;
use memdag::models::{MemoryKind, MemoryStatus, RelationType, SessionLearning};
use memdag::store::{MemoryStore, RecordOptions, SearchOptions, format_search_results_for_llm};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::str::FromStr;

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    if let Some(Commands::Mcp { action }) = &args.command {
        return handle_mcp_action(action, args.db.clone());
    }

    let db_path = args.db.unwrap_or_else(default_db_path);

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let conn = open_connection(&db_path)?;
    let mut store = MemoryStore::new(conn);

    // Sweep ephemeral memories left behind by sessions that crashed before consolidating.
    // ponytail: fixed default, override via MEMDAG_EPHEMERAL_TTL_SECS if it needs tuning.
    let ttl_secs = std::env::var("MEMDAG_EPHEMERAL_TTL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24 * 60 * 60);
    store.purge_expired_ephemeral(ttl_secs)?;

    match args.command {
        Some(Commands::Mcp { .. }) => unreachable!("handled above"),
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
                    print!("\nRelations:\n{}", memdag::store::format_relations(&rels));
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
        Some(Commands::Ready { limit }) => {
            let list = store.list_ready(limit)?;
            if list.is_empty() {
                println!("No claimable tasks/blockers right now.");
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

/// Default location for a client's MCP config: project-local `.mcp.json` (cwd) or the
/// user-global `~/.claude.json`, which Claude Code and most MCP clients read the same
/// `{"mcpServers": {...}}` shape from. Cursor/Windsurf/VS Code use dedicated files with the
/// same shape; pass `--path` to target those directly.
fn mcp_config_path(global: bool, path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(p) = path {
        return Ok(p);
    }
    if global {
        let home = directories::BaseDirs::new()
            .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
            .home_dir()
            .to_path_buf();
        Ok(home.join(".claude.json"))
    } else {
        Ok(std::env::current_dir()?.join(".mcp.json"))
    }
}

fn load_json_object(path: &Path) -> anyhow::Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let raw = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        anyhow::anyhow!("Failed to parse existing JSON at {}: {}", path.display(), e)
    })?;
    if !value.is_object() {
        anyhow::bail!(
            "{} does not contain a JSON object at its root",
            path.display()
        );
    }
    Ok(value)
}

/// Warn (not block) if the binary being registered lives somewhere likely to disappear or
/// move: not on PATH, or inside a Downloads/tmp/cargo-build directory. The MCP config will
/// keep pointing at this exact file path, so if it's deleted or moved, the server stops
/// launching until `memdag mcp install` is rerun from a stable location.
fn warn_if_unstable_binary_path(exe: &Path) {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let path_dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
    if let Some(reason) = unstable_binary_reason(exe, &path_dirs) {
        eprintln!(
            "Warning: registering memdag from {}, which is {reason}. \
             If this file is moved or deleted, the MCP server entry will stop working.\n\
             Move the binary to a stable location on PATH (e.g. ~/.local/bin on Linux/macOS) \
             and rerun `memdag mcp install` from there to avoid this.",
            exe.display(),
        );
    }
}

/// Pure check behind `warn_if_unstable_binary_path`, taking PATH dirs explicitly so it's
/// testable without mutating the process-global PATH env var.
fn unstable_binary_reason(exe: &Path, path_dirs: &[PathBuf]) -> Option<&'static str> {
    let on_path = exe
        .parent()
        .is_some_and(|dir| path_dirs.iter().any(|p| p == dir));
    if !on_path {
        return Some("not on PATH");
    }

    let path_str = exe.to_string_lossy().to_lowercase();
    let looks_ephemeral = [
        "/downloads/",
        "/tmp/",
        "\\temp\\",
        "/target/debug/",
        "/target/release/",
    ]
    .iter()
    .any(|marker| path_str.contains(marker));
    if looks_ephemeral {
        return Some("a temp/build/downloads directory");
    }
    None
}

fn handle_mcp_action(action: &McpAction, cli_db: Option<PathBuf>) -> anyhow::Result<()> {
    match action {
        McpAction::Install { global, path, key } => {
            let cfg_path = mcp_config_path(*global, path.clone())?;
            let mut root = load_json_object(&cfg_path)?;

            let exe = std::env::current_exe()?;
            warn_if_unstable_binary_path(&exe);
            let mut entry = serde_json::json!({
                "command": exe.to_string_lossy(),
                "args": ["serve"],
            });
            if let Some(db) = cli_db {
                entry["env"] = serde_json::json!({ "MEMDAG_DB": db.to_string_lossy() });
            }

            root.as_object_mut()
                .unwrap()
                .entry(key.as_str())
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .ok_or_else(|| {
                    anyhow::anyhow!("'{key}' in {} is not an object", cfg_path.display())
                })?
                .insert("memdag".to_string(), entry);

            if let Some(parent) = cfg_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&cfg_path, serde_json::to_string_pretty(&root)?)?;
            println!(
                "Installed memdag MCP server into {} (key: '{key}')",
                cfg_path.display()
            );
        }
        McpAction::Uninstall { global, path, key } => {
            let cfg_path = mcp_config_path(*global, path.clone())?;
            let mut root = load_json_object(&cfg_path)?;

            let removed = root
                .get_mut(key.as_str())
                .and_then(|s| s.as_object_mut())
                .and_then(|m| m.remove("memdag"))
                .is_some();

            if removed {
                std::fs::write(&cfg_path, serde_json::to_string_pretty(&root)?)?;
                println!(
                    "Removed memdag MCP server entry from {}",
                    cfg_path.display()
                );
            } else {
                println!("No memdag MCP server entry found in {}", cfg_path.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod unstable_path_tests {
    use super::unstable_binary_reason;
    use std::path::PathBuf;

    #[test]
    fn on_path_and_stable_dir_is_fine() {
        let stable_dir = PathBuf::from("/home/user/.local/bin");
        let exe = stable_dir.join("memdag");
        assert_eq!(unstable_binary_reason(&exe, &[stable_dir]), None);
    }

    #[test]
    fn off_path_is_flagged() {
        let exe = PathBuf::from("/home/user/.local/bin/memdag");
        assert_eq!(
            unstable_binary_reason(&exe, &[PathBuf::from("/usr/bin")]),
            Some("not on PATH")
        );
    }

    #[test]
    fn downloads_dir_is_flagged_even_if_on_path() {
        let downloads = PathBuf::from("/home/user/Downloads");
        let exe = downloads.join("memdag");
        assert_eq!(
            unstable_binary_reason(&exe, &[downloads]),
            Some("a temp/build/downloads directory")
        );
    }
}
