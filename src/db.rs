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

// busy_timeout must come first: switching to WAL needs a lock, and without a busy handler a
// concurrent first open fails immediately with SQLITE_BUSY instead of waiting.
pub const PRAGMA_SQL: &str = r#"
PRAGMA busy_timeout = 5000;
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;
"#;

/// Bump when the schema changes; `init_pragmas_and_schema` migrates older databases up to it.
pub const SCHEMA_VERSION: i64 = 6;

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
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    paths TEXT,
    commit_sha TEXT,
    fingerprints TEXT,
    source TEXT,
    origin_repo TEXT,
    keywords TEXT
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
    INSERT INTO memories_fts(rowid, id, title, body, tags, keywords)
    VALUES (new.rowid, new.id, new.title, new.body, new.tags, new.keywords);
END;

CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, id, title, body, tags, keywords)
    VALUES ('delete', old.rowid, old.id, old.title, old.body, old.tags, old.keywords);
END;

CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, id, title, body, tags, keywords)
    VALUES ('delete', old.rowid, old.id, old.title, old.body, old.tags, old.keywords);
    INSERT INTO memories_fts(rowid, id, title, body, tags, keywords)
    VALUES (new.rowid, new.id, new.title, new.body, new.tags, new.keywords);
END;
"#;

/// The v1 indexes and FTS triggers, frozen: v1 still has the four-column FTS table, so a
/// database left at v1..v3 (e.g. by a crash mid-upgrade) must keep triggers that match it.
/// `migrate_to_v4` replaces them along with the table.
const V1_DEPENDENTS_SQL: &str = r#"
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

/// Keep `memories_vec` honest: a deleted memory must not leave an orphan vector that
/// occupies a KNN slot, and a new title or rewritten body invalidates its vector (`memdag embed`
/// recomputes missing ones). Separate from the FTS triggers so v1 databases can add them.
const VEC_TRIGGERS_SQL: &str = r#"
CREATE TRIGGER IF NOT EXISTS memories_vec_ad AFTER DELETE ON memories BEGIN
    DELETE FROM memories_vec WHERE id = old.id;
END;
-- Appending to the body (resolve_memory adds a resolution note) keeps the vector: the
-- memory is still about the same thing. A new title or a rewritten body does not.
CREATE TRIGGER IF NOT EXISTS memories_vec_au AFTER UPDATE OF title, body ON memories
WHEN old.title IS NOT new.title
  OR substr(new.body, 1, length(old.body)) IS NOT old.body BEGIN
    DELETE FROM memories_vec WHERE id = old.id;
END;
"#;

/// v5: which checkouts (worktrees) contain the source of an imported or note-backed row. A
/// row stays while any worktree still has its note, ADR or commit; an import only drops its
/// own worktree's claim. v3 recorded the importing checkout in `origin_repo`, so that becomes
/// the first owner.
const V5_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS memory_owners (
    id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    worktree TEXT NOT NULL,
    PRIMARY KEY (id, worktree)
);
CREATE INDEX IF NOT EXISTS idx_memory_owners_worktree ON memory_owners(worktree);
INSERT OR IGNORE INTO memory_owners (id, worktree)
    SELECT id, origin_repo FROM memories WHERE source IS NOT NULL AND origin_repo IS NOT NULL;
"#;

/// v6: pairs of memories a consolidate worker has judged (duplicate, conflict, related,
/// distinct), so a pair is offered once. `a` is the smaller id.
const V6_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS reviewed_pairs (
    a TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    b TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    verdict TEXT NOT NULL,
    reviewed_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (a, b)
);
CREATE INDEX IF NOT EXISTS idx_reviewed_pairs_b ON reviewed_pairs(b);
"#;

/// The FTS index. `keywords` holds search terms an LLM worker generated for the memory
/// (see `work.rs`); it lives in the same index so one query can match body and keywords.
const FTS_TABLE_SQL: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    id UNINDEXED,
    title,
    body,
    tags,
    keywords,
    content='memories',
    content_rowid='rowid',
    tokenize='porter unicode61'
);
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
{FTS_TABLE_SQL}

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

/// Columns added in schema v2: provenance for staleness checks and the source of imported
/// rows. All nullable, so existing rows and `INSERT`s that omit them stay valid; databases
/// created before v2 get them via `ALTER TABLE ADD COLUMN` (no table rebuild).
const V2_COLUMNS: &[&str] = &["paths", "commit_sha", "fingerprints", "source"];

/// v3: `origin_repo` (canonical path of the repo an imported/note-backed row came from) lets
/// an import prune rows whose source disappeared without touching rows from another repo
/// that shares the same database.
const V3_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_memories_origin ON memories(source, origin_repo);
-- Vectors orphaned before the v2 delete trigger existed.
DELETE FROM memories_vec WHERE id NOT IN (SELECT id FROM memories);
"#;

/// The full schema for a fresh database.
pub fn ddl_sql() -> String {
    format!(
        "{}{}{MEMORIES_DEPENDENTS_SQL}{VEC_TRIGGERS_SQL}{V1_DDL_SQL}{V3_SQL}{V5_SQL}{V6_SQL}",
        memories_table_sql("memories"),
        REST_DDL_SQL.replace("{FTS_TABLE_SQL}", FTS_TABLE_SQL)
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
    let schema_exists = memories_table_exists(conn)?;

    if !schema_exists {
        // Re-check under the write lock: another process may have created the schema while we
        // waited for it, in which case we must not run (or fail on) the creation batch.
        conn.execute_batch("BEGIN IMMEDIATE;")
            .context("Failed to lock database for schema creation")?;
        let created = (|| -> Result<bool> {
            if memories_table_exists(conn)? {
                return Ok(false);
            }
            conn.execute_batch(&ddl_sql())?;
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
            Ok(true)
        })();
        match created {
            Ok(true) => {
                conn.execute_batch("COMMIT;")
                    .context("Failed to initialize memdag schema")?;
                return Ok(());
            }
            Ok(false) => {
                conn.execute_batch("ROLLBACK;").ok();
            }
            Err(e) => {
                conn.execute_batch("ROLLBACK;").ok();
                return Err(e).context("Failed to initialize memdag schema");
            }
        }
    }

    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        anyhow::bail!(
            "Database schema is version {version} but this memdag only understands up to \
             {SCHEMA_VERSION}; upgrade memdag instead of opening it with an older build"
        );
    }
    if version < 1 {
        migrate_to_v1(conn).context("Failed to migrate memdag schema to v1")?;
    }
    if version < 2 {
        migrate_to_v2(conn).context("Failed to migrate memdag schema to v2")?;
    }
    if version < 3 {
        migrate_to_v3(conn).context("Failed to migrate memdag schema to v3")?;
    }
    if version < 4 {
        migrate_to_v4(conn).context("Failed to migrate memdag schema to v4")?;
    }
    if version < 5 {
        migrate_simple(conn, 5, V5_SQL).context("Failed to migrate memdag schema to v5")?;
    }
    if version < 6 {
        migrate_simple(conn, 6, V6_SQL).context("Failed to migrate memdag schema to v6")?;
    }
    Ok(())
}

fn memories_table_exists(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'memories'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false))
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let found = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|c| c == column);
    Ok(found)
}

/// v1 -> v2: add the provenance columns. Re-checks the version under the write lock
/// because another process may have migrated while we waited.
fn migrate_to_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let step = (|| -> Result<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 2 {
            // The v1 table rebuild already includes these columns, so add only what's missing.
            for col in V2_COLUMNS {
                if !has_column(conn, "memories", col)? {
                    conn.execute_batch(&format!("ALTER TABLE memories ADD COLUMN {col} TEXT;"))?;
                }
            }
            // The triggers reference memories_vec; real v0/v1 databases have it, but don't
            // depend on that (same definition as REST_DDL_SQL).
            conn.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS memories_vec USING vec0(
                     id TEXT PRIMARY KEY, embedding float[384]);",
            )?;
            conn.execute_batch(VEC_TRIGGERS_SQL)?;
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

/// v2 -> v3: add `origin_repo` and drop orphan vectors. Rows imported before v3 have no
/// origin; the next import of their repo claims them.
fn migrate_to_v3(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let step = (|| -> Result<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 3 {
            if !has_column(conn, "memories", "origin_repo")? {
                conn.execute_batch("ALTER TABLE memories ADD COLUMN origin_repo TEXT;")?;
            }
            conn.execute_batch(V3_SQL)?;
            conn.execute_batch("PRAGMA user_version = 3;")?;
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

/// A migration that is one idempotent SQL batch, run under the write lock with the version
/// re-checked (another process may have migrated while we waited).
fn migrate_simple(conn: &Connection, to: i64, sql: &str) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let step = (|| -> Result<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < to {
            conn.execute_batch(sql)?;
            conn.execute_batch(&format!("PRAGMA user_version = {to};"))?;
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

/// Columns copied when merging an old per-worktree database (see [`merge_legacy_db`]).
const MERGE_COLS: &str = "id, kind, title, body, tags, status, session_id, created_at, updated_at, \
     paths, commit_sha, fingerprints, source, origin_repo, keywords";

/// Merge an old per-worktree database into the repository's shared one: the memories that
/// were recorded directly (tasks, blockers, local decisions; `source IS NULL`) and the
/// relations between memories that exist in the shared database afterwards. Imported rows
/// are not copied; they are rebuilt from git. The old file is renamed `*.merged`, so this
/// happens once. Returns how many memories were added.
///
/// The old file is claimed by renaming it first, so two processes starting at the same time
/// can't both merge it.
pub fn merge_legacy_db(shared: &Connection, legacy: &Path) -> Result<usize> {
    // Bring the old database to the current schema and flush its WAL into the main file, so
    // renaming the file alone moves everything.
    {
        let old = open_connection(legacy)?;
        old.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    }
    let name = legacy
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let claimed = legacy.with_file_name(format!("{name}.merging-{}", std::process::id()));
    std::fs::rename(legacy, &claimed)
        .with_context(|| format!("claiming {} for merging", legacy.display()))?;
    for suffix in ["-wal", "-shm"] {
        std::fs::remove_file(legacy.with_file_name(format!("{name}{suffix}"))).ok();
    }

    let merged = (|| -> Result<usize> {
        shared.execute("ATTACH DATABASE ?1 AS legacy", [claimed.to_string_lossy()])?;
        let copy = (|| -> Result<usize> {
            shared.execute_batch("BEGIN IMMEDIATE;")?;
            let step = (|| -> Result<usize> {
                let n = shared.execute(
                    &format!(
                        "INSERT OR IGNORE INTO main.memories ({MERGE_COLS})
                         SELECT {MERGE_COLS} FROM legacy.memories WHERE source IS NULL"
                    ),
                    [],
                )?;
                shared.execute(
                    "INSERT OR IGNORE INTO main.memory_relations
                         (source_id, target_id, relation_type, created_at)
                     SELECT r.source_id, r.target_id, r.relation_type, r.created_at
                     FROM legacy.memory_relations r
                     WHERE EXISTS (SELECT 1 FROM main.memories m WHERE m.id = r.source_id)
                       AND EXISTS (SELECT 1 FROM main.memories m WHERE m.id = r.target_id)",
                    [],
                )?;
                Ok(n)
            })();
            match step {
                Ok(n) => {
                    shared.execute_batch("COMMIT;")?;
                    Ok(n)
                }
                Err(e) => {
                    shared.execute_batch("ROLLBACK;").ok();
                    Err(e)
                }
            }
        })();
        shared.execute_batch("DETACH DATABASE legacy;").ok();
        copy
    })();

    match &merged {
        Ok(_) => {
            std::fs::rename(&claimed, legacy.with_file_name(format!("{name}.merged"))).ok();
        }
        // Put it back so the next start retries.
        Err(_) => {
            std::fs::rename(&claimed, legacy).ok();
        }
    }
    merged
}

/// v3 -> v4: add `memories.keywords` and rebuild the FTS index with a `keywords` column (an
/// FTS5 table can't gain a column in place). The index is derived data, so dropping and
/// rebuilding it loses nothing.
fn migrate_to_v4(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let step = (|| -> Result<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 4 {
            if !has_column(conn, "memories", "keywords")? {
                conn.execute_batch("ALTER TABLE memories ADD COLUMN keywords TEXT;")?;
            }
            conn.execute_batch(&format!(
                "DROP TRIGGER IF EXISTS memories_ai;
                 DROP TRIGGER IF EXISTS memories_ad;
                 DROP TRIGGER IF EXISTS memories_au;
                 DROP TABLE IF EXISTS memories_fts;
                 {FTS_TABLE_SQL}
                 {MEMORIES_DEPENDENTS_SQL}
                 INSERT INTO memories_fts(memories_fts) VALUES ('rebuild');
                 PRAGMA user_version = 4;"
            ))?;
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
            {V1_DEPENDENTS_SQL}
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
    if let Some(path) = project_db_path(&std::env::current_dir().unwrap_or_default()) {
        return path;
    }
    if let Some(proj_dirs) = directories::ProjectDirs::from("com", "memdag", "memdag") {
        let data_dir = proj_dirs.data_dir();
        std::fs::create_dir_all(data_dir).ok();
        return data_dir.join("memdag.db");
    }
    PathBuf::from("memdag.db")
}

/// The project database for `start`: the shared database of the repository it is in (one for
/// all of its worktrees, see [`crate::checkout::shared_db_path`]), or `<dir>/.memdag/memdag.db`
/// for the nearest directory that has a `.memdag/` of its own.
pub fn project_db_path(start: &Path) -> Option<PathBuf> {
    let root = find_project_root(start)?;
    if root.join(".git").exists()
        && let Some(shared) = crate::checkout::shared_db_path(&root)
    {
        return Some(shared);
    }
    Some(root.join(".memdag").join("memdag.db"))
}

/// A database that a linked worktree used before worktrees shared one (memdag ≤ 0.3 kept a
/// database per checkout), if it still needs merging into `shared`.
pub fn legacy_worktree_db(start: &Path, shared: &Path) -> Option<PathBuf> {
    let root = find_project_root(start)?;
    let old = root.join(".memdag").join("memdag.db");
    (crate::checkout::is_linked_worktree(&root) && old.is_file() && old != shared).then_some(old)
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

    #[test]
    fn migrates_v2_database_adding_origin_and_dropping_orphan_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v2.db");
        let zeros = format!("[{}]", vec!["0"; 384].join(","));
        let conn = open_connection(&path).unwrap();
        conn.execute_batch(
            "DROP INDEX idx_memories_origin;
             ALTER TABLE memories DROP COLUMN origin_repo;
             DROP TRIGGER memories_ai; DROP TRIGGER memories_ad; DROP TRIGGER memories_au;
             DROP TABLE memories_fts;
             CREATE VIRTUAL TABLE memories_fts USING fts5(id UNINDEXED, title, body, tags,
                 content='memories', content_rowid='rowid', tokenize='porter unicode61');
             ALTER TABLE memories DROP COLUMN keywords;
             PRAGMA user_version = 2;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memories_vec (id, embedding) VALUES ('GONE-1', ?1)",
            [&zeros],
        )
        .unwrap();
        drop(conn);

        let conn = open_connection(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(has_column(&conn, "memories", "origin_repo").unwrap());
        assert!(has_column(&conn, "memories", "keywords").unwrap());
        let orphans: i64 = conn
            .query_row("SELECT count(*) FROM memories_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0);
    }

    #[test]
    fn migrates_v5_database_adding_reviewed_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v5.db");
        let conn = open_connection(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE reviewed_pairs;
             INSERT INTO memories (id, kind, title, body) VALUES ('DEC-1', 'decision', 'a', 'b');
             INSERT INTO memories (id, kind, title, body) VALUES ('DEC-2', 'decision', 'c', 'd');
             PRAGMA user_version = 5;",
        )
        .unwrap();
        drop(conn);

        let conn = open_connection(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        conn.execute(
            "INSERT INTO reviewed_pairs (a, b, verdict) VALUES ('DEC-1', 'DEC-2', 'distinct')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM memories WHERE id = 'DEC-2'", [])
            .unwrap();
        let left: i64 = conn
            .query_row("SELECT count(*) FROM reviewed_pairs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0, "cascades with the memory");
    }
}
