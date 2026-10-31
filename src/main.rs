mod cli;

use clap::Parser;
use cli::{Cli, Commands, McpAction, WorkAction};
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

    // Installing touches harness configs only; no database needed.
    match &args.command {
        Some(Commands::Mcp { action }) => return handle_mcp_action(action, args.db.clone()),
        Some(Commands::Install {
            target,
            global,
            model,
            refresh,
            print_config,
            no_mcp,
            no_worker,
            no_instructions,
            no_skill,
            no_permissions,
            hooks,
            opencode_format,
        }) => {
            let parts = memdag::install::Parts {
                mcp: !no_mcp,
                worker: !no_worker,
                instructions: !no_instructions,
                skill: !no_skill,
                permissions: !no_permissions,
                hooks: *hooks,
            };
            let opts = install_options(
                parts,
                model.clone(),
                args.db.clone(),
                (*opencode_format).into(),
            )?;
            return run_install(target, *global, *refresh, *print_config, &opts);
        }
        Some(Commands::Uninstall { target, global }) => {
            return run_uninstall(target, *global, memdag::install::Parts::ALL);
        }
        // A hook must never break the session it runs in: no output and success on any error.
        Some(Commands::Hook { event }) => {
            match event {
                cli::HookEvent::SessionStart => {
                    if let Ok(Some(line)) = upkeep_summary(args.db.clone()) {
                        println!("{line}");
                    }
                }
            }
            return Ok(());
        }
        _ => {}
    }

    let explicit_db = args.db.is_some() || std::env::var_os("MEMDAG_DB").is_some();
    let db_path = args.db.unwrap_or_else(default_db_path);

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let conn = open_connection(&db_path)?;
    // memdag used to keep one database per worktree; all worktrees of a repository now share
    // one. Fold an old worktree database into it the first time that worktree opens it.
    if !explicit_db
        && let Some(old) = memdag::db::legacy_worktree_db(&std::env::current_dir()?, &db_path)
    {
        match memdag::db::merge_legacy_db(&conn, &old) {
            Ok(n) => eprintln!(
                "[memdag] merged {n} memories from this worktree's old database ({}) into the \
                 repository's shared one ({}); the old file is kept as *.merged",
                old.display(),
                db_path.display()
            ),
            Err(e) => eprintln!(
                "[memdag] could not merge the old worktree database {} (will retry): {e:#}",
                old.display()
            ),
        }
    }
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
        Some(Commands::Mcp { .. })
        | Some(Commands::Install { .. })
        | Some(Commands::Uninstall { .. })
        | Some(Commands::Hook { .. }) => unreachable!("handled above"),
        Some(Commands::Serve) => serve(store, repo, &config)?,
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
                if let Some(kw) = store.get_keywords(&mem.id)? {
                    println!("Keywords:    {kw}");
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
                if store.only_in_other_checkout(&mem) {
                    println!(
                        "Checkout:    only in another worktree; its branch may not be merged yet"
                    );
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
        Some(Commands::Work { action }) => run_work(&mut store, action, repo.as_deref(), &config)?,
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
                serve(store, repo, &config)?;
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
        "Imported git: {}, adr: {}, notes: {}, gh: {} ({} changed, {} removed, {} new edges, {} pending)",
        r.git, r.adr, r.notes, r.gh, r.changed, r.pruned, r.edges, r.pending
    );
    for w in &r.warnings {
        eprintln!("warning: {w}");
    }
}

/// `memdag work ...`: hand out, apply, and install bulk upkeep for a cheap worker model.
fn run_work(
    store: &mut MemoryStore,
    action: WorkAction,
    repo: Option<&Path>,
    config: &RepoConfig,
) -> anyhow::Result<()> {
    use memdag::work::{self, WorkTask};
    let parse = |t: &str| t.parse::<WorkTask>().map_err(|e| anyhow::anyhow!(e));
    match action {
        WorkAction::Next { task, limit, json } => {
            let task = parse(&task)?;
            if task == WorkTask::Verify && repo.is_none() {
                anyhow::bail!("Not inside a git repository, so staleness can't be checked");
            }
            auto_refresh(store, repo, config);
            let batch = work::next_batch(store, task, limit)?;
            let n = batch["items"].as_array().map_or(0, Vec::len);
            if json {
                println!("{}", serde_json::to_string_pretty(&batch)?);
            } else if n == 0 {
                eprintln!("Nothing to do for '{task}'.");
            } else {
                print!("{}", work::batch_as_prompt(&batch));
            }
            eprintln!(
                "[memdag] {n} items in this batch, {} more waiting",
                batch["remaining"]
            );
        }
        WorkAction::Submit { file, task } => {
            let text = match file.filter(|f| f.as_os_str() != "-") {
                Some(f) => std::fs::read_to_string(&f)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", f.display()))?,
                None => std::io::read_to_string(std::io::stdin())?,
            };
            let reply = work::parse_reply(&text)?;
            let task = work::reply_task(&reply, task.as_deref().map(parse).transpose()?)?;
            print!("{}", work::submit(store, task, &reply)?);
        }
        // The worker-only subset of `memdag install`.
        WorkAction::Install {
            client,
            global,
            model,
            no_instructions,
            no_permissions,
        } => {
            let parts = memdag::install::Parts {
                worker: true,
                instructions: !no_instructions,
                permissions: !no_permissions,
                ..memdag::install::Parts::NONE
            };
            let opts = install_options(parts, model, None, memdag::install::OpencodeFormat::Auto)?;
            let env = install_env(repo)?;
            let steps = memdag::install::install(client.target(), &env, location(global), &opts)?;
            print_steps(client.target(), &steps);
        }
        WorkAction::Uninstall { client, global } => {
            let parts = memdag::install::Parts {
                worker: true,
                instructions: true,
                permissions: true,
                ..memdag::install::Parts::NONE
            };
            let env = install_env(repo)?;
            let steps =
                memdag::install::uninstall(client.target(), &env, location(global), parts, &[])?;
            print_steps(client.target(), &steps);
        }
        WorkAction::Prompt { name, rounds } => {
            let p = work::host_prompt(&name).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown prompt '{name}' (expected expand_keywords or verify_stale)"
                )
            })?;
            println!("{}", work::host_prompt_text(p.task, rounds.clamp(1, 50)));
        }
    }
    Ok(())
}

/// One line about waiting upkeep, for the SessionStart hook; None when there is none.
fn upkeep_summary(db: Option<PathBuf>) -> anyhow::Result<Option<String>> {
    let path = db.unwrap_or_else(default_db_path);
    if !path.exists() {
        return Ok(None);
    }
    let mut store = MemoryStore::new(open_connection(&path)?);
    let repo = find_repo_root(&std::env::current_dir()?);
    store.set_repo_root(repo.clone());
    let keywords = store.memories_needing_keywords(0)?.1;
    let stale = if repo.is_some() {
        store.list_stale()?.len()
    } else {
        0
    };
    if keywords == 0 && stale == 0 {
        return Ok(None);
    }
    let mut parts = Vec::new();
    let mut tasks = Vec::new();
    if keywords > 0 {
        parts.push(format!("{keywords} memories have no search keywords yet"));
        tasks.push("`expand`");
    }
    if stale > 0 {
        parts.push(format!("{stale} are STALE"));
        tasks.push("`verify`");
    }
    Ok(Some(format!(
        "memdag: {}. When convenient, start the memdag-worker subagent (task {}); it runs on a cheap model.",
        parts.join(", "),
        tasks.join(" / ")
    )))
}

fn location(global: bool) -> memdag::install::Location {
    if global {
        memdag::install::Location::Global
    } else {
        memdag::install::Location::Project
    }
}

/// Project files go at the repository root (or the current directory outside git).
fn install_env(repo: Option<&Path>) -> anyhow::Result<memdag::install::Env> {
    let cwd = std::env::current_dir()?;
    let project = repo
        .map(Path::to_path_buf)
        .or_else(|| find_repo_root(&cwd))
        .unwrap_or(cwd);
    memdag::install::Env::current(project)
}

fn install_options(
    parts: memdag::install::Parts,
    model: Option<String>,
    db: Option<PathBuf>,
    opencode: memdag::install::OpencodeFormat,
) -> anyhow::Result<memdag::install::Options> {
    let current = std::env::current_exe()?;
    // Write the path PATH finds (e.g. ~/.local/bin/memdag), not wherever a symlink resolves
    // to: a versioned install dir or build output can vanish on upgrade. Canonical paths are
    // only compared, never written (on Windows they carry a \\?\ prefix).
    let on_path = path_hit(&current);
    let exe = on_path.clone().unwrap_or(current);
    warn_if_unstable_binary_path(&exe);
    Ok(memdag::install::Options {
        parts,
        model,
        on_path: on_path.is_some(),
        exe,
        db,
        opencode,
    })
}

/// The `PATH` entry that plain `memdag` resolves to, if it is this very binary.
fn path_hit(exe: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "memdag.exe"
    } else {
        "memdag"
    };
    let canon = |p: &Path| p.canonicalize().ok();
    let me = canon(exe)?;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|c| c.is_file())
        .filter(|c| canon(c).as_ref() == Some(&me))
}

fn print_steps(t: memdag::install::Target, steps: &[memdag::install::Step]) {
    println!("{}:", t.name());
    for s in steps {
        if s.path.as_os_str().is_empty() {
            if let Some(n) = &s.note {
                println!("  {}: {n}", s.part);
            }
            continue;
        }
        println!("  {:?}: {} ({})", s.change, s.path.display(), s.part);
        if let Some(n) = &s.note {
            println!("    note: {n}");
        }
    }
}

fn run_install(
    spec: &str,
    global: bool,
    refresh: bool,
    print_config: bool,
    opts: &memdag::install::Options,
) -> anyhow::Result<()> {
    use memdag::install::{self, Parts, Target};
    let env = install_env(None)?;
    let loc = location(global);
    let mut targets = install::parse_targets(spec, &env, loc)?;
    if refresh {
        let base = if spec == "auto" {
            Target::ALL.to_vec()
        } else {
            targets
        };
        targets = base
            .into_iter()
            .filter(|t| install::is_installed(*t, &env, loc))
            .collect();
    }
    if targets.is_empty() {
        let hint = match (refresh, loc) {
            (true, _) => "has memdag installed to refresh".to_string(),
            (false, memdag::install::Location::Project) => format!(
                "uses this project yet ({}); pass --target (e.g. --target claude-code,opencode) or --global",
                env.project.display()
            ),
            (false, memdag::install::Location::Global) => "is installed for this user".to_string(),
        };
        anyhow::bail!("No harness {hint}");
    }
    for t in targets {
        // A refresh rewrites only what is already there, so it never adds files the user
        // didn't ask for (e.g. to a harness that had only an MCP entry).
        let opts = if refresh {
            let have = install::installed_parts(t, &env, loc);
            let want = opts.parts;
            let parts = Parts {
                mcp: have.mcp && want.mcp,
                worker: have.worker && want.worker,
                instructions: have.instructions && want.instructions,
                skill: have.skill && want.skill,
                permissions: have.permissions && want.permissions,
                hooks: have.hooks,
            };
            install::Options {
                parts,
                ..opts.clone()
            }
        } else {
            opts.clone()
        };
        if print_config {
            println!("{}", install::print_config(t, &env, loc, &opts)?);
        } else {
            print_steps(t, &install::install(t, &env, loc, &opts)?);
        }
    }
    Ok(())
}

fn run_uninstall(spec: &str, global: bool, parts: memdag::install::Parts) -> anyhow::Result<()> {
    use memdag::install::{self, Target};
    let env = install_env(None)?;
    let loc = location(global);
    let targets = install::parse_targets(spec, &env, loc)?;
    // Shared files (one AGENTS.md, one .agents/skills/memdag) stay while a harness that
    // isn't being uninstalled still has memdag installed.
    let remaining: Vec<Target> = Target::ALL
        .into_iter()
        .filter(|t| !targets.contains(t) && install::is_installed(*t, &env, loc))
        .collect();
    for t in targets {
        print_steps(t, &install::uninstall(t, &env, loc, parts, &remaining)?);
    }
    Ok(())
}

/// Run the stdio MCP server. The index is refreshed at startup and, while the server runs,
/// before read tools whenever HEAD has moved.
fn serve(mut store: MemoryStore, repo: Option<PathBuf>, config: &RepoConfig) -> anyhow::Result<()> {
    auto_refresh(&mut store, repo.as_deref(), config);
    let mut mcp = McpServer::new(store);
    if let Some(repo) = repo {
        mcp = mcp.with_auto_refresh(
            repo,
            config.notes_dir(),
            memdag::mcp::DEFAULT_REFRESH_INTERVAL,
        );
    }
    mcp.run_stdio()
}

/// Re-import local git sources if HEAD moved since the last import. Never fatal: a broken
/// import must not stop search or the MCP server, so failures are only logged to stderr.
fn auto_refresh(store: &mut MemoryStore, repo: Option<&Path>, config: &RepoConfig) {
    if let Some(repo) = repo {
        memdag::import::auto_refresh(store, repo, &config.notes_dir());
    }
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
             and rerun `memdag install` from there to avoid this.",
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
    use memdag::install::{self, Parts};
    let mcp_only = Parts {
        mcp: true,
        ..Parts::NONE
    };
    match action {
        McpAction::Install {
            global,
            path,
            client,
            key,
        } => {
            let opts = install_options(
                mcp_only,
                None,
                cli_db,
                memdag::install::OpencodeFormat::Auto,
            )?;
            let t = client.target();
            let env = install_env(None)?;
            match (path, key) {
                (None, None) => {
                    print_steps(t, &install::install(t, &env, location(*global), &opts)?)
                }
                _ => {
                    let (path, change) = install::install_entry_at(
                        t,
                        &env,
                        location(*global),
                        &opts,
                        path.as_deref(),
                        key.as_deref(),
                    )?;
                    println!("{change:?}: {} (MCP server)", path.display());
                }
            }
        }
        McpAction::Uninstall {
            global,
            path,
            client,
            key,
        } => {
            let t = client.target();
            let env = install_env(None)?;
            match (path, key) {
                (None, None) => print_steps(
                    t,
                    &install::uninstall(t, &env, location(*global), mcp_only, &[])?,
                ),
                _ => {
                    let (path, change) = install::remove_entry_at(
                        t,
                        &env,
                        location(*global),
                        path.as_deref(),
                        key.as_deref(),
                    )?;
                    println!("{change:?}: {} (MCP server)", path.display());
                }
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
