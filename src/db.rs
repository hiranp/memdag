use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, ffi};
use std::path::{Path, PathBuf};
use std::sync::Once;

static INIT_SQLITE_VEC: Once = Once::new();

pub fn ensure_sqlite_vec_registered() {
    INIT_SQLITE_VEC.call_once(|| unsafe {
        #[allow(clippy::missing_transmute_annotations)]
        ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

pub const PRAGMA_SQL: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;
"#;

pub const DDL_SQL: &str = r#"
-- Core memory entities (decisions, tasks, bugs, invariant facts)
CREATE TABLE IF NOT EXISTS memories (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('decision', 'task', 'invariant', 'blocker', 'ephemeral')),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    tags TEXT,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'superseded', 'resolved', 'ephemeral')),
    session_id TEXT,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- Relational Directed Graph (Supersession, Dependency, Blocking)
CREATE TABLE IF NOT EXISTS memory_relations (
    source_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation_type TEXT NOT NULL CHECK(relation_type IN ('supersedes', 'depends_on', 'blocks', 'references')),
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (source_id, target_id, relation_type)
);

-- Indexes for performance
CREATE INDEX IF NOT EXISTS idx_memory_relations_source ON memory_relations(source_id);
CREATE INDEX IF NOT EXISTS idx_memory_relations_target ON memory_relations(target_id);
CREATE INDEX IF NOT EXISTS idx_memories_status ON memories(status);
CREATE INDEX IF NOT EXISTS idx_memories_kind ON memories(kind);
CREATE INDEX IF NOT EXISTS idx_memories_session ON memories(session_id);

-- FTS5 Virtual Table for BM25 Search
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    id UNINDEXED,
    title,
    body,
    tags,
    content='memories',
    content_rowid='rowid',
    tokenize='porter unicode61'
);

-- Sync Triggers between Core Table and FTS5 Index
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, id, title, body, tags) 
    VALUES (new.rowid, new.id, new.title, new.body, new.tags);
END;

CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, id, title, body, tags) 
    VALUES ('delete', old.rowid, old.id, old.title, old.body, old.tags);
END;

CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, id, title, body, tags) 
    VALUES ('delete', old.rowid, old.id, old.title, old.body, old.tags);
    INSERT INTO memories_fts(rowid, id, title, body, tags) 
    VALUES (new.rowid, new.id, new.title, new.body, new.tags);
END;

-- Optional Vector Table for Dense Semantic Search
CREATE VIRTUAL TABLE IF NOT EXISTS memories_vec USING vec0(
    id TEXT PRIMARY KEY,
    embedding float[384]
);
"#;

pub fn open_connection<P: AsRef<Path>>(path: P) -> Result<Connection> {
    ensure_sqlite_vec_registered();
    let conn = Connection::open(path.as_ref())
        .with_context(|| format!("Failed to open SQLite database at {:?}", path.as_ref()))?;
    init_pragmas_and_schema(&conn)?;
    Ok(conn)
}

pub fn open_in_memory() -> Result<Connection> {
    ensure_sqlite_vec_registered();
    let conn = Connection::open_in_memory().context("Failed to open in-memory SQLite database")?;
    init_pragmas_and_schema(&conn)?;
    Ok(conn)
}

fn init_pragmas_and_schema(conn: &Connection) -> Result<()> {
    // Pragmas are per-connection state, always cheap to reapply.
    conn.execute_batch(PRAGMA_SQL)
        .context("Failed to set memdag connection pragmas")?;

    // The DDL batch (CREATE TABLE/INDEX/TRIGGER/VIRTUAL TABLE) is idempotent but each
    // statement still takes a schema lock to check "IF NOT EXISTS". Re-running the full
    // batch on every process start creates needless write contention when many short-lived
    // CLI processes (e.g. concurrent subagents) open the same database at once. Skip it once
    // the schema is known to exist.
    let schema_exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'memories'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);

    if !schema_exists {
        conn.execute_batch(DDL_SQL)
            .context("Failed to initialize memdag schema")?;
    }
    Ok(())
}

pub fn default_db_path() -> PathBuf {
    if let Ok(path) = std::env::var("MEMDAG_DB") {
        return PathBuf::from(path);
    }
    if let Some(root) = find_project_root(&std::env::current_dir().unwrap_or_default()) {
        return root.join(".memdag").join("memdag.db");
    }
    if let Some(proj_dirs) = directories::ProjectDirs::from("com", "memdag", "memdag") {
        let data_dir = proj_dirs.data_dir();
        std::fs::create_dir_all(data_dir).ok();
        return data_dir.join("memdag.db");
    }
    PathBuf::from("memdag.db")
}

// ponytail: walk up for a repo root (existing .memdag/ wins over .git so nested
// crates in the same repo share one DB); falls back to the global data dir.
fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start.to_path_buf());
    while let Some(d) = dir {
        if d.join(".memdag").is_dir() || d.join(".git").exists() {
            return Some(d);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

#[cfg(test)]
mod default_db_path_tests {
    use super::find_project_root;
    use std::fs;

    #[test]
    fn finds_git_root_from_nested_dir() {
        let tmp = std::env::temp_dir().join(format!("memdag-test-git-{}", std::process::id()));
        let nested = tmp.join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(tmp.join(".git")).unwrap();

        assert_eq!(find_project_root(&nested), Some(tmp.clone()));
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn prefers_existing_memdag_dir_over_ancestor_git() {
        let tmp = std::env::temp_dir().join(format!("memdag-test-nested-{}", std::process::id()));
        let inner = tmp.join("crate");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::create_dir_all(inner.join(".memdag")).unwrap();

        assert_eq!(find_project_root(&inner), Some(inner.clone()));
        fs::remove_dir_all(&tmp).unwrap();
    }
}
