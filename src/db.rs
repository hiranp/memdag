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

/// Bump when the schema changes; `init_pragmas_and_schema` migrates older databases up to it.
pub const SCHEMA_VERSION: i64 = 2;

/// `CREATE TABLE` for the core memories table, parameterised on its name so the v1
/// migration can build a replacement table before swapping it in.
fn memories_table_sql(name: &str) -> String {
    format!(
        r#"
CREATE TABLE IF NOT EXISTS {name} (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('decision', 'task', 'invariant', 'blocker', 'learning', 'ephemeral')),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    tags TEXT,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'superseded', 'resolved', 'ephemeral')),
    session_id TEXT,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
"#
    )
}

/// Indexes and FTS sync triggers that hang off `memories` (dropped along with the table).
const MEMORIES_DEPENDENTS_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_memories_status ON memories(status);
CREATE INDEX IF NOT EXISTS idx_memories_kind ON memories(kind);
CREATE INDEX IF NOT EXISTS idx_memories_session ON memories(session_id);

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
"#;

const REST_DDL_SQL: &str = r#"
-- Relational Directed Graph (Supersession, Dependency, Blocking)
CREATE TABLE IF NOT EXISTS memory_relations (
    source_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation_type TEXT NOT NULL CHECK(relation_type IN ('supersedes', 'depends_on', 'blocks', 'references')),
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (source_id, target_id, relation_type)
);

CREATE INDEX IF NOT EXISTS idx_memory_relations_source ON memory_relations(source_id);
CREATE INDEX IF NOT EXISTS idx_memory_relations_target ON memory_relations(target_id);

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

-- Optional Vector Table for Dense Semantic Search
CREATE VIRTUAL TABLE IF NOT EXISTS memories_vec USING vec0(
    id TEXT PRIMARY KEY,
    embedding float[384]
);
"#;

/// Tables added in schema v1 for the git-backed importer.
const V1_DDL_SQL: &str = r#"
-- Small key/value store (e.g. last imported git HEAD per repo)
CREATE TABLE IF NOT EXISTS memdag_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Edges named by imported sources whose target hasn't been imported yet; retried on every import
CREATE TABLE IF NOT EXISTS pending_relations (
    source_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    relation_type TEXT NOT NULL,
    PRIMARY KEY (source_id, target_id, relation_type)
);
"#;

/// Columns added in schema v2: provenance for staleness checks. Nullable, so existing rows
/// and `INSERT`s that omit them stay valid; no table rebuild needed.
const V2_DDL_SQL: &str = r#"
ALTER TABLE memories ADD COLUMN paths TEXT;
ALTER TABLE memories ADD COLUMN commit_sha TEXT;
"#;

/// The full schema for a fresh database.
pub fn ddl_sql() -> String {
    format!(
        "{}{REST_DDL_SQL}{MEMORIES_DEPENDENTS_SQL}{V1_DDL_SQL}{V2_DDL_SQL}",
        memories_table_sql("memories")
    )
}

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
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;\n{}\nPRAGMA user_version = {SCHEMA_VERSION};\nCOMMIT;",
            ddl_sql()
        ))
        .context("Failed to initialize memdag schema")?;
        return Ok(());
    }

    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        migrate_to_v1(conn).context("Failed to migrate memdag schema to v1")?;
    }
    if version < 2 {
        migrate_to_v2(conn).context("Failed to migrate memdag schema to v2")?;
    }
    Ok(())
}

/// v1 -> v2: add the provenance columns. Re-checks the version under the write lock
/// because another process may have migrated while we waited.
fn migrate_to_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let step = (|| -> Result<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 2 {
            conn.execute_batch(V2_DDL_SQL)?;
            conn.execute_batch("PRAGMA user_version = 2;")?;
        }
        Ok(())
    })();
    match step {
        Ok(()) => conn.execute_batch("COMMIT;")?,
        Err(e) => {
            conn.execute_batch("ROLLBACK;").ok();
            return Err(e);
        }
    }
    Ok(())
}

/// v0 -> v1: widen the `kind` CHECK constraint to allow `learning` (SQLite can't ALTER a
/// CHECK, so rebuild the table preserving rowids, which keeps the external-content FTS index
/// valid) and add the importer's meta/pending tables.
fn migrate_to_v1(conn: &Connection) -> Result<()> {
    // foreign_keys must be off across the table swap, and it is a no-op inside a transaction.
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let result = (|| -> Result<()> {
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        // Another process may have migrated while we waited for the write lock.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version >= 1 {
            conn.execute_batch("COMMIT;")?;
            return Ok(());
        }
        let migrate = format!(
            "{}
            INSERT INTO memories_v1 (rowid, id, kind, title, body, tags, status, session_id, created_at, updated_at)
                SELECT rowid, id, kind, title, body, tags, status, session_id, created_at, updated_at FROM memories;
            DROP TABLE memories;
            ALTER TABLE memories_v1 RENAME TO memories;
            {MEMORIES_DEPENDENTS_SQL}
            INSERT INTO memories_fts(memories_fts) VALUES ('rebuild');
            {V1_DDL_SQL}
            PRAGMA user_version = 1;
            COMMIT;",
            memories_table_sql("memories_v1")
        );
        if let Err(e) = conn.execute_batch(&migrate) {
            conn.execute_batch("ROLLBACK;").ok();
            return Err(e.into());
        }
        Ok(())
    })();
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    result
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

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// The pre-v1 schema, verbatim enough to exercise the migration (old CHECK, no meta tables).
    const V0_DDL: &str = r#"
CREATE TABLE memories (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('decision', 'task', 'invariant', 'blocker', 'ephemeral')),
    title TEXT NOT NULL, body TEXT NOT NULL, tags TEXT,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'superseded', 'resolved', 'ephemeral')),
    session_id TEXT,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE memory_relations (
    source_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation_type TEXT NOT NULL CHECK(relation_type IN ('supersedes', 'depends_on', 'blocks', 'references')),
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (source_id, target_id, relation_type)
);
CREATE VIRTUAL TABLE memories_fts USING fts5(id UNINDEXED, title, body, tags,
    content='memories', content_rowid='rowid', tokenize='porter unicode61');
CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, id, title, body, tags) VALUES (new.rowid, new.id, new.title, new.body, new.tags);
END;
INSERT INTO memories (id, kind, title, body) VALUES ('DEC-1', 'decision', 'Use WAL', 'concurrency');
INSERT INTO memories (id, kind, title, body) VALUES ('TSK-1', 'task', 'Ship', 'release');
INSERT INTO memory_relations (source_id, target_id, relation_type) VALUES ('TSK-1', 'DEC-1', 'depends_on');
"#;

    #[test]
    fn migrates_v0_database_preserving_rows_edges_and_fts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        ensure_sqlite_vec_registered();
        Connection::open(&path)
            .unwrap()
            .execute_batch(V0_DDL)
            .unwrap();

        let conn = open_connection(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        conn.execute(
            "INSERT INTO memories (id, kind, title, body) VALUES ('LRN-1', 'learning', 'l', 'b')",
            [],
        )
        .unwrap();
        let edges: i64 = conn
            .query_row("SELECT count(*) FROM memory_relations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(edges, 1);
        let hit: String = conn
            .query_row(
                "SELECT m.id FROM memories_fts f JOIN memories m ON f.rowid = m.rowid \
                 WHERE memories_fts MATCH 'concurrency'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hit, "DEC-1");
        let meta: i64 = conn
            .query_row("SELECT count(*) FROM memdag_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(meta, 0);

        // Cascade still works after the swap (FK re-pointed at the new table).
        conn.execute("DELETE FROM memories WHERE id = 'DEC-1'", [])
            .unwrap();
        let edges: i64 = conn
            .query_row("SELECT count(*) FROM memory_relations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(edges, 0);
        drop(conn);

        // Reopening a migrated DB is a no-op.
        open_connection(&path).unwrap();
    }
}
