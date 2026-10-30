mod cli;

use clap::Parser;
use cli::{Cli, Commands, McpAction, McpClient};
use memdag::db::{default_db_path, open_connection};
use memdag::import::{GhCli, ImportOptions, ImportReport, find_repo_root, import_repo};
use memdag::mcp::McpServer;
use memdag::models::{MemoryKind, MemoryStatus, RelationType, SessionLearning};
use memdag::notes::RepoConfig;
use memdag::store::{MemoryStore, RecordOptions, SearchOptions, format_search_results_with_budget};
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

    // Git-backed sources: the repo containing the cwd, and its memdag.toml.
    let cwd = std::env::current_dir()?;
    let repo = find_repo_root(&cwd);
    let config = match &repo {
        Some(root) => RepoConfig::load(root)?,
        None => RepoConfig::default(),
    };
    if let (Some(root), Some(dir)) = (&repo, &config.write_through_dir) {
        store.set_write_through(Some(root.join(dir)));
    }
    store.set_repo_root(repo.clone());
    // Environment only: a cloned repo's memdag.toml must not be able to run a command.
    store.set_embed_command(memdag::embed::command_from_env());

    match args.command {
        Some(Commands::Mcp { .. }) => unreachable!("handled above"),
        Some(Commands::Serve) => {
            auto_refresh(&mut store, repo.as_deref(), &config);
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
            paths,
            commit,
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
                paths,
                commit,
            })?;

            println!("Recorded memory: [{}] {}", record.id, record.title);
            println!("  Kind:   {}", record.kind);
            println!("  Status: {}", record.status);
            if let Some(tags) = record.tags {
                println!("  Tags:   {}", tags);
            }
            if !record.paths.is_empty() {
                println!(
                    "  Cites:  {} (as of {})",
                    record.paths.join(", "),
                    record
                        .commit
                        .as_deref()
                        .map_or("unknown commit", |c| &c[..c.len().min(7)])
                );
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
            max_tokens,
        }) => {
            let kind_filter = match kind {
                Some(k) => Some(MemoryKind::from_str(&k).map_err(|e| anyhow::anyhow!(e))?),
                None => None,
            };

            auto_refresh(&mut store, repo.as_deref(), &config);
            let results = store.search_memory(SearchOptions {
                query,
                kind: kind_filter,
                include_resolved,
                limit,
            })?;

            let budget = (max_tokens > 0).then_some(max_tokens);
            println!("{}", format_search_results_with_budget(&results, budget));
        }
        Some(Commands::Get { id }) => match store.get_memory(&id)? {
            Some((mem, rels)) => {
                println!("ID:          {}", mem.id);
                println!("Title:       {}", mem.title);
                println!("Kind:        {}", mem.kind);
                println!("Status:      {}", mem.status);
                if let Some(tags) = &mem.tags {
                    println!("Tags:        {}", tags);
                }
                if let Some(sess) = &mem.session_id {
                    println!("Session:     {}", sess);
                }
                if !mem.paths.is_empty() {
                    println!("Cites:       {}", mem.paths.join(", "));
                }
                if let Some(c) = &mem.commit {
                    println!("As of:       {c}");
                }
                let stale = store.stale_paths(&mem);
                if !stale.is_empty() {
                    println!(
                        "STALE:       cited paths changed since: {}",
                        stale.join(", ")
                    );
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
                paths: Vec::new(),
                commit: None,
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
        Some(Commands::Embed) => {
            let n = store.embed_missing()?;
            println!("Embedded {n} memories.");
        }
        Some(Commands::Stale) => {
            if repo.is_none() {
                anyhow::bail!(
                    "Not inside a git repository, so staleness can't be checked (cited paths are repo-relative)"
                );
            }
            let stale = store.list_stale()?;
            if stale.is_empty() {
                println!("No stale memories: no cited path changed since it was recorded.");
            } else {
                for (mem, paths) in stale {
                    println!(
                        "[{}] {} (as of {})",
                        mem.id,
                        mem.title,
                        mem.commit.as_deref().map_or("?", |c| &c[..c.len().min(7)])
                    );
                    for p in paths {
                        println!("    changed: {p}");
                    }
                }
            }
        }
        Some(Commands::Export { path }) => {
            let data = store.export_all()?;
            let json = serde_json::to_string_pretty(&data)?;
            match path {
                Some(path) => {
                    std::fs::write(&path, json)?;
                    println!(
                        "Exported {} memories, {} relations to {}",
                        data.memories.len(),
                        data.relations.len(),
                        path.display()
                    );
                }
                None => println!("{json}"),
            }
        }
        Some(Commands::Import {
            repo: repo_arg,
            git,
            adr,
            notes,
            gh,
            since,
        }) => {
            let root = match repo_arg {
                Some(r) => r,
                None => repo.clone().ok_or_else(|| {
                    anyhow::anyhow!("Not inside a git repository; pass --repo PATH")
                })?,
            };
            let config = RepoConfig::load(&root)?;
            // Bare `import` means the local, git-tracked sources. GitHub comments need an
            // explicit --gh: they are a network call and anyone can comment on an issue.
            let any = git || adr || notes || gh;
            let opts = ImportOptions {
                git: !any || git,
                adr: !any || adr,
                notes: !any || notes,
                gh,
                record_head: since.is_none(),
                since,
                notes_dir: config.notes_dir(),
                ..ImportOptions::all()
            };
            let report = import_repo(&mut store, &root, &opts, &GhCli)?;
            print_report(&report);
        }
        None => {
            // Default behavior: if stdin is not a terminal (e.g. piped or spawned by MCP client), run MCP server.
            // If it is a terminal, print help.
            if !std::io::stdin().is_terminal() {
                auto_refresh(&mut store, repo.as_deref(), &config);
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

fn print_report(r: &ImportReport) {
    println!(
        "Imported git: {}, adr: {}, notes: {}, gh: {} ({} changed, {} new edges, {} pending)",
        r.git, r.adr, r.notes, r.gh, r.changed, r.edges, r.pending
    );
    for w in &r.warnings {
        eprintln!("warning: {w}");
    }
}

/// Re-import local git sources if HEAD moved since the last import. Never fatal: a broken
/// import must not stop search or the MCP server, so failures are only logged to stderr.
fn auto_refresh(store: &mut MemoryStore, repo: Option<&Path>, config: &RepoConfig) {
    let Some(repo) = repo else { return };
    match memdag::import::refresh_if_stale(store, repo, &config.notes_dir()) {
        Ok(Some(r)) => {
            for w in &r.warnings {
                eprintln!("[memdag] auto-import warning: {w}");
            }
            if r.changed > 0 {
                eprintln!(
                    "[memdag] auto-imported {} changed memories from {}",
                    r.changed,
                    repo.display()
                );
            }
            // Changed rows lost their vectors (trigger); refill a bounded number so hybrid
            // search keeps covering imported memories without a long blocking job.
            if r.changed > 0 && store.has_embedder() {
                match store.embed_missing_up_to(50) {
                    Ok(0) => {}
                    Ok(n) => eprintln!("[memdag] embedded {n} imported memories"),
                    Err(e) => eprintln!("[memdag] embedding after import skipped: {e:#}"),
                }
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("[memdag] auto-import failed (continuing): {e:#}"),
    }
}

fn home_dir() -> anyhow::Result<PathBuf> {
    Ok(directories::BaseDirs::new()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .home_dir()
        .to_path_buf())
}

/// `opencode.jsonc` if it exists in `dir`, else `opencode.json`.
fn opencode_file(dir: &Path) -> PathBuf {
    let jsonc = dir.join("opencode.jsonc");
    if jsonc.exists() {
        jsonc
    } else {
        dir.join("opencode.json")
    }
}

/// Default config file for a client preset. Claude Code: project-local `.mcp.json` (cwd) or
/// user-global `~/.claude.json`. OpenCode: `./opencode.json[c]` or
/// `$XDG_CONFIG_HOME/opencode/opencode.json[c]` (default `~/.config/opencode`, on every OS).
/// Antigravity only loads `~/.gemini/config/mcp_config.json`, so it ignores `--global`.
/// Cursor/Windsurf/VS Code use the Claude Code shape in their own files; pass `--path`.
fn mcp_config_path(
    client: McpClient,
    global: bool,
    path: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    if let Some(p) = path {
        return Ok(p);
    }
    Ok(match (client, global) {
        (McpClient::ClaudeCode, true) => home_dir()?.join(".claude.json"),
        (McpClient::ClaudeCode, false) => std::env::current_dir()?.join(".mcp.json"),
        (McpClient::Opencode, true) => {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .map_or_else(|| home_dir().map(|h| h.join(".config")), Ok)?;
            opencode_file(&base.join("opencode"))
        }
        (McpClient::Opencode, false) => opencode_file(&std::env::current_dir()?),
        (McpClient::Antigravity, _) => home_dir()?
            .join(".gemini")
            .join("config")
            .join("mcp_config.json"),
    })
}

fn default_key(client: McpClient) -> &'static str {
    match client {
        McpClient::Opencode => "mcp",
        McpClient::ClaudeCode | McpClient::Antigravity => "mcpServers",
    }
}

/// The server entry in the shape each client expects.
fn mcp_entry(client: McpClient, exe: &Path, db: Option<&Path>) -> serde_json::Value {
    let exe = exe.to_string_lossy();
    match client {
        // https://opencode.ai/docs/mcp-servers/ — local servers take a single argv array.
        McpClient::Opencode => {
            let mut entry = serde_json::json!({
                "type": "local",
                "command": [exe, "serve"],
                "enabled": true,
            });
            if let Some(db) = db {
                entry["environment"] = serde_json::json!({ "MEMDAG_DB": db.to_string_lossy() });
            }
            entry
        }
        McpClient::ClaudeCode | McpClient::Antigravity => {
            let mut entry = serde_json::json!({ "command": exe, "args": ["serve"] });
            if let Some(db) = db {
                entry["env"] = serde_json::json!({ "MEMDAG_DB": db.to_string_lossy() });
            }
            entry
        }
    }
}

/// Strip `//` and `/* */` comments and trailing commas so JSONC (opencode.jsonc) parses
/// as JSON. String contents are left untouched.
fn strip_jsonc(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_str = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            (',', _) => {
                // Drop the comma if the next significant char closes an object/array.
                let rest: String = chars.clone().collect();
                let next = strip_jsonc_lookahead(&rest);
                if !matches!(next, Some('}') | Some(']')) {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// First char after whitespace and comments.
fn strip_jsonc_lookahead(rest: &str) -> Option<char> {
    let mut s = rest;
    loop {
        s = s.trim_start();
        if let Some(r) = s.strip_prefix("//") {
            s = r.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(r) = s.strip_prefix("/*") {
            s = r.split_once("*/").map_or("", |(_, r)| r);
        } else {
            return s.chars().next();
        }
    }
}

/// Load a JSON (or JSONC) object. Returns whether the file had comments, which a rewrite
/// would drop (the caller backs the file up first).
fn load_json_object(path: &Path) -> anyhow::Result<(serde_json::Value, bool)> {
    if !path.exists() {
        return Ok((serde_json::json!({}), false));
    }
    let raw = std::fs::read_to_string(path)?;
    let (value, had_comments) = match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v) => (v, false),
        Err(e) => {
            let stripped = strip_jsonc(&raw);
            let v = serde_json::from_str(&stripped).map_err(|_| {
                anyhow::anyhow!("Failed to parse existing JSON at {}: {}", path.display(), e)
            })?;
            (v, true)
        }
    };
    if !value.is_object() {
        anyhow::bail!(
            "{} does not contain a JSON object at its root",
            path.display()
        );
    }
    Ok((value, had_comments))
}

fn save_json_object(
    path: &Path,
    root: &serde_json::Value,
    had_comments: bool,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if had_comments {
        let backup = PathBuf::from(format!("{}.bak", path.display()));
        std::fs::copy(path, &backup)?;
        eprintln!(
            "Note: {} contained comments, which JSON rewriting drops; original saved as {}",
            path.display(),
            backup.display()
        );
    }
    std::fs::write(path, serde_json::to_string_pretty(root)?)?;
    Ok(())
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
        McpAction::Install {
            global,
            path,
            client,
            key,
        } => {
            let cfg_path = mcp_config_path(*client, *global, path.clone())?;
            let key = key.as_deref().unwrap_or(default_key(*client));
            let (mut root, had_comments) = load_json_object(&cfg_path)?;

            let exe = std::env::current_exe()?;
            warn_if_unstable_binary_path(&exe);
            let entry = mcp_entry(*client, &exe, cli_db.as_deref());

            root.as_object_mut()
                .unwrap()
                .entry(key)
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .ok_or_else(|| {
                    anyhow::anyhow!("'{key}' in {} is not an object", cfg_path.display())
                })?
                .insert("memdag".to_string(), entry);

            save_json_object(&cfg_path, &root, had_comments)?;
            println!(
                "Installed memdag MCP server into {} (key: '{key}')",
                cfg_path.display()
            );
        }
        McpAction::Uninstall {
            global,
            path,
            client,
            key,
        } => {
            let cfg_path = mcp_config_path(*client, *global, path.clone())?;
            let key = key.as_deref().unwrap_or(default_key(*client));
            let (mut root, had_comments) = load_json_object(&cfg_path)?;

            let removed = root
                .get_mut(key)
                .and_then(|s| s.as_object_mut())
                .and_then(|m| m.remove("memdag"))
                .is_some();

            if removed {
                save_json_object(&cfg_path, &root, had_comments)?;
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
mod jsonc_tests {
    use super::strip_jsonc;

    #[test]
    fn strips_comments_and_trailing_commas_but_not_strings() {
        let raw = "{\n  // c\n  \"a\": \"http://x//y\", /* b */\n  \"l\": [1, 2,],\n}\n";
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(raw)).unwrap();
        assert_eq!(v["a"], "http://x//y");
        assert_eq!(v["l"], serde_json::json!([1, 2]));
    }
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
