use crate::embed;
use crate::import::{git, git_head};
use crate::models::{
    DatabaseStats, EdgeDirection, ExportData, Memory, MemoryKind, MemoryRelation,
    MemorySearchResult, MemoryStatus, RelatedEntity, RelationType, SessionLearning,
};
use crate::notes::{
    Note, RESOLVED_SUFFIX, clean_provenance, is_valid_commit, is_valid_note_id, is_valid_repo_path,
    join_paths, join_tags, parse_note, render_resolution, resolution_suffix, slugify, split_paths,
    split_tags, write_new_file_atomic,
};
use crate::provenance;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// The 13 memory columns, in the order `row_to_memory` expects.
const MEMORY_COLS: &str = "id, kind, title, body, tags, status, session_id, created_at, updated_at, \
     paths, commit_sha, fingerprints, source";
/// Index of the extra score column (BM25 rank / vector distance) selected after the memory
/// columns.
const SCORE_COL: usize = 13;
/// Memories waiting for worker keywords.
const NEEDS_KEYWORDS: &str = "keywords IS NULL AND kind != 'ephemeral' AND status != 'superseded'";
/// BM25 column weights for (id, title, body, tags, keywords). Worker-generated keywords are
/// guesses about how someone might search, so a match there counts half as much as one in
/// the memory's own text.
const BM25_WEIGHTS: &str = "1.0, 1.0, 1.0, 1.0, 0.5";
/// The kinds a consolidate worker compares: claims that should not be duplicated or
/// contradict each other.
const DURABLE_KINDS: &str = "kind IN ('decision', 'invariant', 'learning')";
/// Durable memories scanned for consolidation candidates per batch (newest first).
pub const CONSOLIDATE_SCAN: usize = 300;
/// Matches considered per memory when looking for consolidation candidates.
pub const CONSOLIDATE_HITS: usize = 2;
/// Title words too common to suggest two memories are about the same thing.
const CONSOLIDATE_STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "not", "are", "was", "use", "uses", "using", "from", "that",
    "this", "into", "when", "all", "any", "can", "has", "have", "our", "its", "but", "per", "via",
    "always", "never", "should", "must", "instead",
];

/// Whether a memory takes part in consolidation: active and of a durable kind.
fn is_consolidatable(m: &Memory) -> bool {
    m.status == MemoryStatus::Active
        && matches!(
            m.kind,
            MemoryKind::Decision | MemoryKind::Invariant | MemoryKind::Learning
        )
}

/// An FTS5 query matching any significant word of `title` (3+ characters, not a stopword).
fn title_or_query(title: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for word in title.split_whitespace() {
        let clean: String = word
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect::<String>()
            .to_lowercase();
        if clean.chars().count() >= 3
            && clean.chars().any(char::is_alphanumeric)
            && !CONSOLIDATE_STOPWORDS.contains(&clean.as_str())
            && !words.contains(&clean)
        {
            words.push(clean);
        }
    }
    words
        .iter()
        .map(|w| format!("\"{w}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}
/// Reciprocal Rank Fusion constant (Cormack et al.; also what Hindsight uses).
const RRF_K: f64 = 60.0;
/// Default output budget for search results, in estimated tokens.
pub const DEFAULT_MAX_TOKENS: usize = 2000;
/// Relations listed per result before collapsing into "+N more".
const MAX_RELATIONS_SHOWN: usize = 6;

#[derive(Debug, Clone)]
pub struct RecordOptions {
    pub id: Option<String>,
    pub kind: MemoryKind,
    pub title: String,
    pub body: String,
    pub tags: Option<String>,
    pub supersedes_id: Option<String>,
    pub session_id: Option<String>,
    pub embedding: Option<Vec<f32>>,
    /// Repo-relative files this memory is about (enables staleness checks).
    pub paths: Vec<String>,
    /// Commit the memory was recorded at; defaults to the repo's HEAD when `paths` is set.
    pub commit: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub query: String,
    pub kind: Option<MemoryKind>,
    pub include_resolved: bool,
    pub limit: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            query: String::new(),
            kind: None,
            include_resolved: false,
            limit: 10,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConsolidationResult {
    pub session_id: String,
    pub learnings_recorded: usize,
    pub ephemeral_purged: usize,
    pub ephemeral_archived: usize,
    pub recorded_ids: Vec<String>,
}

/// A session learning after id/provenance resolution, ready to insert.
struct PreparedLearning {
    id: String,
    title: String,
    body: String,
    tags: Option<String>,
    paths: Vec<String>,
    commit: Option<String>,
    fingerprints: Vec<String>,
    source: Option<&'static str>,
}

pub struct MemoryStore {
    conn: Connection,
    write_through: Option<PathBuf>,
    repo_root: Option<PathBuf>,
    embed_cmd: Option<String>,
    /// Whether `embed_cmd` has been compared with the embedder recorded in the database.
    embedder_checked: Cell<bool>,
}

/// `memdag_meta` key holding the embed command that produced the stored vectors.
const EMBEDDER_META_KEY: &str = "embedder";

/// Kinds that are durable enough to become git-tracked note files under write-through.
pub fn writes_through(kind: MemoryKind) -> bool {
    matches!(
        kind,
        MemoryKind::Decision | MemoryKind::Invariant | MemoryKind::Learning | MemoryKind::Blocker
    )
}

impl MemoryStore {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn,
            write_through: None,
            repo_root: None,
            embed_cmd: None,
            embedder_checked: Cell::new(false),
        }
    }

    /// Git repository used to stamp new memories with HEAD and to detect stale ones.
    pub fn set_repo_root(&mut self, root: Option<PathBuf>) {
        self.repo_root = root;
    }

    /// Shell command that turns stdin text into a 384-float JSON vector (see [`embed`]).
    pub fn set_embed_command(&mut self, cmd: Option<String>) {
        self.embed_cmd = cmd.filter(|c| !c.trim().is_empty());
        self.embedder_checked.set(false);
    }

    /// The embed command, after making sure the stored vectors came from it. Vectors from
    /// different embedders live in different spaces, so mixing them makes KNN meaningless.
    /// On a mismatch every stored vector is dropped (`memdag embed` recomputes them). The
    /// command string is the identity: change it only when the model changes.
    fn embedder(&self) -> Option<&str> {
        let cmd = self.embed_cmd.as_deref()?;
        if !self.embedder_checked.get() {
            if let Err(e) = self.check_embedder_identity(cmd) {
                eprintln!("[memdag] could not check embedder identity: {e:#}");
            }
            self.embedder_checked.set(true);
        }
        Some(cmd)
    }

    fn check_embedder_identity(&self, cmd: &str) -> Result<()> {
        let recorded: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM memdag_meta WHERE key = ?1",
                params![EMBEDDER_META_KEY],
                |r| r.get(0),
            )
            .optional()?;
        if recorded.as_deref() == Some(cmd) {
            return Ok(());
        }
        if recorded.is_some() {
            let dropped = self.conn.execute("DELETE FROM memories_vec", [])?;
            if dropped > 0 {
                eprintln!(
                    "[memdag] {} changed since the stored vectors were computed; dropped {dropped} \
                     vectors (run `memdag embed` to recompute them)",
                    embed::EMBED_CMD_ENV
                );
            }
        }
        // No record yet: vectors from before identities were tracked are adopted as-is.
        self.conn.execute(
            "INSERT INTO memdag_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![EMBEDDER_META_KEY, cmd],
        )?;
        Ok(())
    }

    /// Key of the repository note-backed rows belong to (see `memories.origin_repo`); shared
    /// by all of its worktrees.
    fn origin_for(&self, source: Option<&str>) -> Option<String> {
        source?;
        self.repo_root
            .as_deref()
            .map(crate::checkout::repository_key)
    }

    /// Key of this checkout, which owns the note-backed rows it writes.
    fn owner_for(&self, source: Option<&str>) -> Option<String> {
        source?;
        self.repo_root.as_deref().map(crate::checkout::worktree_key)
    }

    /// True for an imported or note-backed memory whose source exists only in other worktrees
    /// of the repository (a note on a branch that isn't checked out here). Its cited paths
    /// describe that branch, so it is never STALE-checked against this checkout's files.
    pub fn only_in_other_checkout(&self, memory: &Memory) -> bool {
        if memory.source.is_none() {
            return false;
        }
        let Some(here) = self.repo_root.as_deref().map(crate::checkout::worktree_key) else {
            return false;
        };
        self.conn
            .query_row(
                "SELECT count(*) > 0 AND sum(worktree = ?2) = 0 FROM memory_owners WHERE id = ?1",
                params![memory.id, here],
                |r| r.get::<_, Option<bool>>(0),
            )
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    pub fn has_embedder(&self) -> bool {
        self.embed_cmd.is_some()
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Enable write-through: durable memories (see [`writes_through`]) are first written to
    /// `<dir>/<id>.md` (append-only, never rewritten) and then indexed from that file.
    pub fn set_write_through(&mut self, dir: Option<PathBuf>) {
        self.write_through = dir;
    }

    pub fn write_through_dir(&self) -> Option<&Path> {
        self.write_through.as_deref()
    }

    fn id_exists(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM memories WHERE id = ?1", params![id], |_| {
                Ok(true)
            })
            .optional()?
            .unwrap_or(false))
    }

    /// `<PREFIX>-YYYY-MM-DD-<slug>`, suffixed `-2`, `-3`... if taken in the DB or on disk.
    fn generate_note_id(&self, kind: MemoryKind, title: &str, dir: &Path) -> Result<String> {
        let date: String = self
            .conn
            .query_row("SELECT date('now')", [], |r| r.get(0))?;
        let base = format!("{}-{date}-{}", kind.prefix(), slugify(title));
        let mut id = base.clone();
        let mut n = 1;
        while self.id_exists(&id)? || dir.join(format!("{id}.md")).exists() {
            n += 1;
            id = format!("{base}-{n}");
        }
        Ok(id)
    }

    /// Write the note file for a write-through memory and return what the DB should index:
    /// the id plus title/body/tags exactly as they parse back from the file, so a later
    /// rebuild-from-git yields an identical row.
    ///
    /// `note.id` may be empty, in which case an id is generated from kind/date/title.
    fn write_note(&self, dir: &Path, mut note: Note) -> Result<(Note, PathBuf)> {
        if note.id.is_empty() {
            note.id = self.generate_note_id(note.kind, &note.title, dir)?;
        }
        let id = note.id.clone();
        if !is_valid_note_id(&id) {
            bail!(
                "Memory id '{id}' can't be used as a write-through file name (use letters, digits, '-', '_', '.')"
            );
        }
        if self.id_exists(&id)? {
            bail!("Memory '{id}' already exists");
        }
        if let Some(old) = note.supersedes.as_deref()
            && !self.id_exists(old)?
        {
            bail!("Target supersedes_id '{}' does not exist", old);
        }
        let text = note.render();
        let parsed = parse_note(&text, &id)?;

        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create write-through dir {}", dir.display()))?;
        let path = dir.join(format!("{id}.md"));
        // Notes are append-only: never overwrite an existing file.
        write_new_file_atomic(&path, text.as_bytes())
            .with_context(|| format!("Failed to create note {}", path.display()))?;
        Ok((parsed, path))
    }

    /// Record a memory entity with atomic supersession handling.
    pub fn record_memory(&mut self, mut opts: RecordOptions) -> Result<Memory> {
        check_for_secrets(&opts.title, &opts.body, opts.tags.as_deref())?;
        let mut fingerprints = Vec::new();
        self.stamp_provenance(&mut opts.paths, &mut opts.commit, &mut fingerprints)?;
        let mut source: Option<&'static str> = None;

        let mut written: Option<PathBuf> = None;
        if let Some(dir) = self
            .write_through
            .clone()
            .filter(|_| writes_through(opts.kind))
        {
            let draft = Note {
                id: opts.id.take().unwrap_or_default(),
                kind: opts.kind,
                title: opts.title.clone(),
                body: opts.body.clone(),
                tags: opts.tags.as_deref().map(split_tags).unwrap_or_default(),
                supersedes: opts.supersedes_id.clone(),
                refs: Vec::new(),
                paths: opts.paths.clone(),
                commit: opts.commit.clone(),
                fingerprints: fingerprints.clone(),
            };
            let (note, path) = self.write_note(&dir, draft)?;
            opts.id = Some(note.id);
            opts.title = note.title;
            opts.body = note.body;
            opts.tags = join_tags(&note.tags);
            opts.paths = note.paths;
            opts.commit = note.commit;
            fingerprints = note.fingerprints;
            // The note file now backs this row, so the importer may refresh it.
            source = Some("note");
            written = Some(path);
        }

        let had_embedding = opts.embedding.is_some();
        let result = self.insert_memory(opts, &fingerprints, source);
        if result.is_err()
            && let Some(path) = written
        {
            std::fs::remove_file(path).ok();
        }
        if let Ok(memory) = &result
            && !had_embedding
            && memory.kind != MemoryKind::Ephemeral
        {
            self.embed_best_effort(memory);
        }
        result
    }

    /// Validate user-supplied provenance, then (for a memory that cites paths and names no
    /// explicit commit) stamp the repo's HEAD and fingerprint the cited files. An explicit
    /// `commit` means the caller knows the baseline, so no fingerprints are taken and
    /// staleness falls back to diffing against that commit. Memories without paths are left
    /// untouched (no git call).
    fn stamp_provenance(
        &self,
        paths: &mut Vec<String>,
        commit: &mut Option<String>,
        fingerprints: &mut Vec<String>,
    ) -> Result<()> {
        let (clean, explicit) = clean_provenance(paths, commit.as_deref())?;
        *paths = clean;
        fingerprints.clear();
        if explicit.is_some() || paths.is_empty() {
            *commit = explicit;
            return Ok(());
        }
        *commit = self.repo_root.as_deref().and_then(git_head);
        if let Some(root) = self.repo_root.as_deref()
            && let Some(fps) = provenance::fingerprints(root, paths)
        {
            *fingerprints = fps;
        }
        Ok(())
    }

    /// Embed a memory if an embedder is configured. Never fails the write: an embedder
    /// outage must not stop an agent from recording a decision. `memdag embed` backfills.
    fn embed_best_effort(&self, memory: &Memory) {
        let Some(cmd) = self.embedder() else { return };
        let text = embed::memory_text(&memory.title, &memory.body);
        match embed::embed(cmd, &text).and_then(|e| self.upsert_embedding(&memory.id, &e)) {
            Ok(()) => {}
            Err(e) => eprintln!(
                "[memdag] could not embed {} (run `memdag embed` later): {e:#}",
                memory.id
            ),
        }
    }

    fn upsert_embedding(&self, id: &str, embedding: &[f32]) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO memories_vec (id, embedding) VALUES (?1, ?2)",
                params![id, embedding_to_json(embedding)],
            )
            .context("Failed to store vector embedding")?;
        Ok(())
    }

    /// Embed every non-ephemeral memory that has no vector yet. Stops at the first embedder
    /// failure (so a dead embedder doesn't burn a timeout per row). Returns how many were
    /// embedded.
    pub fn embed_missing(&mut self) -> Result<usize> {
        self.embed_missing_up_to(usize::MAX)
    }

    /// Like [`Self::embed_missing`] but embeds at most `limit` memories, so an automatic
    /// refresh never turns into a long blocking job.
    pub fn embed_missing_up_to(&mut self, limit: usize) -> Result<usize> {
        let cmd = self
            .embedder()
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("{} is not set", embed::EMBED_CMD_ENV))?;
        let pending: Vec<(String, String, String)> = self
            .conn
            .prepare(
                "SELECT m.id, m.title, m.body FROM memories m
                 WHERE m.kind != 'ephemeral'
                   AND NOT EXISTS (SELECT 1 FROM memories_vec v WHERE v.id = m.id)
                 ORDER BY m.created_at
                 LIMIT ?1",
            )?
            .query_map(params![limit.min(i64::MAX as usize) as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<_, _>>()?;
        let mut done = 0;
        for (id, title, body) in pending {
            let emb = embed::embed(&cmd, &embed::memory_text(&title, &body))
                .with_context(|| format!("embedding {id}"))?;
            self.upsert_embedding(&id, &emb)?;
            done += 1;
        }
        Ok(done)
    }

    fn insert_memory(
        &mut self,
        opts: RecordOptions,
        fingerprints: &[String],
        source: Option<&str>,
    ) -> Result<Memory> {
        let new_id = opts.id.unwrap_or_else(|| {
            let short_uuid = &uuid::Uuid::new_v4().to_string()[..8];
            format!("{}-{}", opts.kind.prefix(), short_uuid)
        });

        let status = if opts.kind == MemoryKind::Ephemeral {
            MemoryStatus::Ephemeral
        } else {
            MemoryStatus::Active
        };

        let origin = self.origin_for(source);
        let owner = self.owner_for(source);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("Failed to begin immediate transaction for record_memory")?;

        // If supersedes_id is provided, verify it exists first
        if let Some(ref old_id) = opts.supersedes_id {
            let exists: bool = tx
                .query_row(
                    "SELECT 1 FROM memories WHERE id = ?1",
                    params![old_id],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);

            if !exists {
                bail!("Target supersedes_id '{}' does not exist", old_id);
            }

            // Mark old entity as superseded
            tx.execute(
                "UPDATE memories SET status = 'superseded', updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
                params![old_id],
            )
            .with_context(|| format!("Failed to update status for superseded memory '{}'", old_id))?;
        }

        // Insert new entity FIRST so foreign keys can reference it
        tx.execute(
            r#"
            INSERT INTO memories (id, kind, title, body, tags, status, session_id, paths, commit_sha, fingerprints, source, origin_repo)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            "#,
            params![
                new_id,
                opts.kind.as_str(),
                opts.title,
                opts.body,
                opts.tags,
                status.as_str(),
                opts.session_id,
                join_paths(&opts.paths),
                opts.commit,
                join_paths(fingerprints),
                source,
                origin,
            ],
        )
        .context("Failed to insert memory record")?;

        // If supersedes_id was provided, insert 'supersedes' relation edge
        if let Some(ref old_id) = opts.supersedes_id {
            tx.execute(
                "INSERT OR REPLACE INTO memory_relations (source_id, target_id, relation_type) VALUES (?1, ?2, 'supersedes')",
                params![new_id, old_id],
            )
            .context("Failed to insert supersedes edge into memory_relations")?;
        }

        // If embedding is supplied, insert into sqlite-vec table
        if let Some(emb) = opts.embedding {
            tx.execute(
                "INSERT OR REPLACE INTO memories_vec (id, embedding) VALUES (?1, ?2)",
                params![new_id, embedding_to_json(&emb)],
            )
            .context("Failed to insert vector embedding into memories_vec")?;
        }

        if let Some(w) = &owner {
            tx.execute(
                "INSERT OR IGNORE INTO memory_owners (id, worktree) VALUES (?1, ?2)",
                params![new_id, w],
            )?;
        }

        // Fetch the recorded memory row
        let memory = tx.query_row(
            &format!("SELECT {MEMORY_COLS} FROM memories WHERE id = ?1"),
            params![new_id],
            row_to_memory,
        )?;

        tx.commit()
            .context("Failed to commit transaction for record_memory")?;

        Ok(memory)
    }

    /// Link two memories with a directed relationship.
    pub fn link_entities(
        &mut self,
        source_id: &str,
        target_id: &str,
        relation_type: RelationType,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("Failed to begin immediate transaction for link_entities")?;

        let source_exists: bool = tx
            .query_row(
                "SELECT 1 FROM memories WHERE id = ?1",
                params![source_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);

        if !source_exists {
            bail!("Source entity '{}' not found", source_id);
        }

        let target_exists: bool = tx
            .query_row(
                "SELECT 1 FROM memories WHERE id = ?1",
                params![target_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);

        if !target_exists {
            bail!("Target entity '{}' not found", target_id);
        }

        tx.execute(
            r#"
            INSERT OR REPLACE INTO memory_relations (source_id, target_id, relation_type)
            VALUES (?1, ?2, ?3)
            "#,
            params![source_id, target_id, relation_type.as_str()],
        )
        .context("Failed to insert relation")?;

        tx.commit().context("Failed to commit link_entities")?;
        Ok(())
    }

    /// Search active memories with FTS5 BM25 and bidirectional 1-hop DAG expansion.
    /// When an embedder is configured, BM25 and vector hits are fused with Reciprocal Rank
    /// Fusion and `rank` carries the fused score (higher is better); otherwise `rank` is the
    /// BM25 value. An empty or untokenizable query falls back to the most recently updated
    /// memories.
    pub fn search_memory(&self, opts: SearchOptions) -> Result<Vec<MemorySearchResult>> {
        let fts_query = sanitize_fts5_query(&opts.query);
        let limit = opts.limit.clamp(1, 100);

        let query_embedding = if fts_query.is_empty() {
            None
        } else {
            self.embed_query(&opts.query)
        };

        let ranked: Vec<(Memory, f64)> = if fts_query.is_empty() {
            let status = if opts.include_resolved {
                None
            } else {
                Some(MemoryStatus::Active)
            };
            self.list_memories(status, opts.kind, limit)?
                .into_iter()
                .map(|m| (m, 0.0))
                .collect()
        } else if let Some(emb) = query_embedding {
            // Over-fetch each channel so fusion has room to reorder.
            let pool = (limit * 3).min(300);
            let bm25 = self.bm25_hits(&fts_query, &opts, pool)?;
            let vector = self.vector_hits(&emb, &opts, pool)?;
            fuse_ranked(bm25, vector, limit)
        } else {
            self.bm25_hits(&fts_query, &opts, limit)?
        };

        // ponytail: one relations query per hit (<=100). Single JOIN if that ever shows up in a profile.
        self.finish_results(ranked)
    }

    /// Embed `text` with the configured embedder (errors if none is configured).
    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        let cmd = self
            .embedder()
            .ok_or_else(|| anyhow::anyhow!("{} is not set", embed::EMBED_CMD_ENV))?;
        embed::embed(cmd, text)
    }

    /// Embed a search query, degrading to BM25-only if the embedder is absent or failing.
    fn embed_query(&self, query: &str) -> Option<Vec<f32>> {
        let cmd = self.embedder()?;
        match embed::embed(cmd, query) {
            Ok(e) => Some(e),
            Err(e) => {
                eprintln!("[memdag] embedder unavailable, using keyword search only: {e:#}");
                None
            }
        }
    }

    fn bm25_hits(
        &self,
        fts_query: &str,
        opts: &SearchOptions,
        limit: usize,
    ) -> Result<Vec<(Memory, f64)>> {
        let mut stmt = self.conn.prepare(&format!(
            r#"
            SELECT m.{MEMORY_COLS_M}, bm25(memories_fts, {BM25_WEIGHTS}) AS rank
            FROM memories_fts f
            JOIN memories m ON f.rowid = m.rowid
            WHERE memories_fts MATCH :query
              AND (:include_resolved = 1 OR m.status = 'active')
              AND (:kind IS NULL OR m.kind = :kind)
            ORDER BY rank
            LIMIT :limit
            "#,
            MEMORY_COLS_M = MEMORY_COLS.replace(", ", ", m.")
        ))?;

        let hits = stmt
            .query_map(
                rusqlite::named_params! {
                    ":query": fts_query,
                    ":include_resolved": opts.include_resolved as i64,
                    ":kind": opts.kind.map(|k| k.as_str()),
                    ":limit": limit as i64,
                },
                |row| Ok((row_to_memory(row)?, row.get::<_, f64>(SCORE_COL)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(hits)
    }

    /// KNN hits honouring the same status/kind filters as BM25. sqlite-vec picks the `k`
    /// nearest first and the filters apply afterwards, so fewer than `k` rows can return.
    fn vector_hits(
        &self,
        embedding: &[f32],
        opts: &SearchOptions,
        k: usize,
    ) -> Result<Vec<(Memory, f64)>> {
        let mut stmt = self.conn.prepare(&format!(
            r#"
            SELECT m.{MEMORY_COLS_M}, v.distance
            FROM memories_vec v
            JOIN memories m ON v.id = m.id
            WHERE v.embedding MATCH :embedding AND v.k = :k
              AND (:include_resolved = 1 OR m.status = 'active')
              AND (:kind IS NULL OR m.kind = :kind)
            ORDER BY v.distance ASC
            "#,
            MEMORY_COLS_M = MEMORY_COLS.replace(", ", ", m.")
        ))?;
        let hits = stmt
            .query_map(
                rusqlite::named_params! {
                    ":embedding": embedding_to_json(embedding),
                    ":k": k as i64,
                    ":include_resolved": opts.include_resolved as i64,
                    ":kind": opts.kind.map(|kd| kd.as_str()),
                },
                |row| Ok((row_to_memory(row)?, row.get::<_, f64>(SCORE_COL)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(hits)
    }

    /// Attach 1-hop relations and staleness to ranked hits.
    fn finish_results(&self, ranked: Vec<(Memory, f64)>) -> Result<Vec<MemorySearchResult>> {
        ranked
            .into_iter()
            .map(|(memory, rank)| {
                Ok(MemorySearchResult {
                    other_checkout: self.only_in_other_checkout(&memory),
                    relations: self.get_memory_relations(&memory.id)?,
                    stale_paths: self.stale_paths(&memory),
                    memory,
                    rank,
                })
            })
            .collect()
    }

    /// Cited paths whose content changed since the memory was recorded. Uses the stored
    /// content fingerprints; for memories that only have a `commit` (hand-written notes, or
    /// an explicit `--commit`) it falls back to `git diff` against that commit, which
    /// includes uncommitted edits. Empty when the memory cites nothing, there is no repo,
    /// or no baseline is comparable: no signal, never a false alarm. Everything read from
    /// note files is re-validated before it reaches `git`.
    pub fn stale_paths(&self, memory: &Memory) -> Vec<String> {
        let Some(root) = self.repo_root.as_deref() else {
            return Vec::new();
        };
        if memory.paths.is_empty() || self.only_in_other_checkout(memory) {
            return Vec::new();
        }
        if memory.paths.is_empty() || !memory.paths.iter().all(|p| is_valid_repo_path(p)) {
            return Vec::new();
        }
        if !memory.fingerprints.is_empty()
            && let Some(changed) = provenance::changed(root, &memory.paths, &memory.fingerprints)
        {
            return changed;
        }
        let Some(commit) = memory.commit.as_deref().filter(|c| is_valid_commit(c)) else {
            return Vec::new();
        };
        // --literal-pathspecs: a cited path like ":(exclude)x" must be a file name, not magic.
        let mut args = vec![
            "--literal-pathspecs",
            "diff",
            "--name-only",
            "--no-renames",
            commit,
            "--",
        ];
        args.extend(memory.paths.iter().map(String::as_str));
        match git(root, &args) {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Active memories whose cited paths changed since they were recorded.
    pub fn list_stale(&self) -> Result<Vec<(Memory, Vec<String>)>> {
        let candidates = self
            .conn
            .prepare(&format!(
                "SELECT {MEMORY_COLS} FROM memories
                 WHERE status = 'active' AND paths IS NOT NULL
                   AND (fingerprints IS NOT NULL OR commit_sha IS NOT NULL)
                 ORDER BY updated_at DESC"
            ))?
            .query_map([], row_to_memory)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(candidates
            .into_iter()
            .filter_map(|m| {
                let stale = self.stale_paths(&m);
                (!stale.is_empty()).then_some((m, stale))
            })
            .collect())
    }

    /// Consolidate session insights: insert permanent learnings and purge or archive ephemeral entries.
    pub fn consolidate_session(
        &mut self,
        session_id: &str,
        learnings: Vec<SessionLearning>,
        purge_ephemeral: bool,
    ) -> Result<ConsolidationResult> {
        let mut prepared: Vec<PreparedLearning> = Vec::with_capacity(learnings.len());
        let mut written: Vec<PathBuf> = Vec::new();
        let result = (|| -> Result<ConsolidationResult> {
            for learning in &learnings {
                check_for_secrets(&learning.title, &learning.body, learning.tags.as_deref())?;
                let mut paths = learning.paths.clone();
                let mut commit = learning.commit.clone();
                let mut fingerprints = Vec::new();
                self.stamp_provenance(&mut paths, &mut commit, &mut fingerprints)?;
                match self
                    .write_through
                    .clone()
                    .filter(|_| writes_through(learning.kind))
                {
                    Some(dir) => {
                        let draft = Note {
                            id: String::new(),
                            kind: learning.kind,
                            title: learning.title.clone(),
                            body: learning.body.clone(),
                            tags: learning.tags.as_deref().map(split_tags).unwrap_or_default(),
                            supersedes: None,
                            refs: Vec::new(),
                            paths,
                            commit,
                            fingerprints,
                        };
                        let (note, path) = self.write_note(&dir, draft)?;
                        written.push(path);
                        prepared.push(PreparedLearning {
                            id: note.id,
                            title: note.title,
                            body: note.body,
                            tags: join_tags(&note.tags),
                            paths: note.paths,
                            commit: note.commit,
                            fingerprints: note.fingerprints,
                            source: Some("note"),
                        });
                    }
                    None => {
                        let short_uuid = &uuid::Uuid::new_v4().to_string()[..8];
                        prepared.push(PreparedLearning {
                            id: format!("{}-{}", learning.kind.prefix(), short_uuid),
                            title: learning.title.clone(),
                            body: learning.body.clone(),
                            tags: learning.tags.clone(),
                            paths,
                            commit,
                            fingerprints,
                            source: None,
                        });
                    }
                }
            }
            self.consolidate_tx(session_id, &learnings, &prepared, purge_ephemeral)
        })();
        if result.is_err() {
            for path in written {
                std::fs::remove_file(path).ok();
            }
        } else if self.embed_cmd.is_some() {
            for (learning, p) in learnings.iter().zip(&prepared) {
                if learning.kind != MemoryKind::Ephemeral {
                    let stub = Memory {
                        id: p.id.clone(),
                        kind: learning.kind,
                        title: p.title.clone(),
                        body: p.body.clone(),
                        tags: p.tags.clone(),
                        status: MemoryStatus::Active,
                        session_id: Some(session_id.to_string()),
                        created_at: String::new(),
                        updated_at: String::new(),
                        paths: Vec::new(),
                        commit: None,
                        fingerprints: Vec::new(),
                        source: None,
                    };
                    self.embed_best_effort(&stub);
                }
            }
        }
        result
    }

    fn consolidate_tx(
        &mut self,
        session_id: &str,
        learnings: &[SessionLearning],
        prepared: &[PreparedLearning],
        purge_ephemeral: bool,
    ) -> Result<ConsolidationResult> {
        let origin = self.origin_for(Some("note"));
        let owner = self.owner_for(Some("note"));
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("Failed to begin immediate transaction for consolidate_session")?;

        let mut recorded_ids = Vec::new();
        for (learning, p) in learnings.iter().zip(prepared) {
            tx.execute(
                r#"
                INSERT INTO memories (id, kind, title, body, tags, status, session_id, paths, commit_sha, fingerprints, source, origin_repo)
                VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?7, ?8, ?9, ?10, ?11)
                "#,
                params![
                    p.id,
                    learning.kind.as_str(),
                    p.title,
                    p.body,
                    p.tags,
                    session_id,
                    join_paths(&p.paths),
                    p.commit,
                    join_paths(&p.fingerprints),
                    p.source,
                    p.source.and(origin.as_deref()),
                ],
            )
            .context("Failed to insert consolidated session learning")?;
            if p.source.is_some()
                && let Some(w) = &owner
            {
                tx.execute(
                    "INSERT OR IGNORE INTO memory_owners (id, worktree) VALUES (?1, ?2)",
                    params![p.id, w],
                )?;
            }

            recorded_ids.push(p.id.clone());
        }

        let mut ephemeral_purged = 0;
        let mut ephemeral_archived = 0;

        if purge_ephemeral {
            ephemeral_purged = tx
                .execute(
                    r#"
                    DELETE FROM memories 
                    WHERE session_id = ?1 AND (status = 'ephemeral' OR kind = 'ephemeral')
                    "#,
                    params![session_id],
                )
                .context("Failed to purge ephemeral memories for session")?;
        } else {
            ephemeral_archived = tx
                .execute(
                    r#"
                    UPDATE memories SET status = 'resolved', updated_at = CURRENT_TIMESTAMP
                    WHERE session_id = ?1 AND (status = 'ephemeral' OR kind = 'ephemeral')
                    "#,
                    params![session_id],
                )
                .context("Failed to archive ephemeral memories for session")?;
        }

        tx.commit()
            .context("Failed to commit consolidate_session")?;

        Ok(ConsolidationResult {
            session_id: session_id.to_string(),
            learnings_recorded: recorded_ids.len(),
            ephemeral_purged,
            ephemeral_archived,
            recorded_ids,
        })
    }

    /// Delete ephemeral memories older than `max_age_secs`, regardless of session.
    /// Covers sessions that crashed or exited without calling `consolidate_session`.
    pub fn purge_expired_ephemeral(&mut self, max_age_secs: i64) -> Result<usize> {
        self.conn
            .execute(
                "DELETE FROM memories \
                 WHERE (status = 'ephemeral' OR kind = 'ephemeral') \
                   AND created_at < datetime('now', ?1)",
                params![format!("-{} seconds", max_age_secs)],
            )
            .context("Failed to purge expired ephemeral memories")
    }

    /// Get all 1-hop relations (outgoing and incoming) for a memory ID.
    pub fn get_memory_relations(&self, id: &str) -> Result<Vec<RelatedEntity>> {
        let mut stmt = self.conn.prepare(
            r#"
            -- Outgoing relations
            SELECT r.relation_type, target.id, target.title, target.kind, target.status, 'outgoing' AS direction
            FROM memory_relations r
            JOIN memories target ON r.target_id = target.id
            WHERE r.source_id = ?1

            UNION ALL

            -- Incoming relations
            SELECT r.relation_type, source.id, source.title, source.kind, source.status, 'incoming' AS direction
            FROM memory_relations r
            JOIN memories source ON r.source_id = source.id
            WHERE r.target_id = ?1
            "#,
        )?;

        let relations = stmt
            .query_map(params![id], |row| {
                let rel_str: String = row.get(0)?;
                let rel_id: String = row.get(1)?;
                let title: Option<String> = row.get(2)?;
                let kind_str: Option<String> = row.get(3)?;
                let status_str: Option<String> = row.get(4)?;
                let dir_str: String = row.get(5)?;

                let direction = if dir_str == "incoming" {
                    EdgeDirection::Incoming
                } else {
                    EdgeDirection::Outgoing
                };

                Ok(RelatedEntity {
                    relation_type: RelationType::from_str(&rel_str)
                        .unwrap_or(RelationType::References),
                    target_id: rel_id,
                    target_title: title,
                    target_kind: kind_str.and_then(|k| MemoryKind::from_str(&k).ok()),
                    target_status: status_str.and_then(|s| MemoryStatus::from_str(&s).ok()),
                    direction,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(relations)
    }

    /// Get a single memory by ID along with its bidirectional 1-hop relations.
    pub fn get_memory(&self, id: &str) -> Result<Option<(Memory, Vec<RelatedEntity>)>> {
        let memory = self
            .conn
            .query_row(
                &format!("SELECT {MEMORY_COLS} FROM memories WHERE id = ?1"),
                params![id],
                row_to_memory,
            )
            .optional()?;

        let Some(memory) = memory else {
            return Ok(None);
        };

        let relations = self.get_memory_relations(id)?;
        Ok(Some((memory, relations)))
    }

    /// Mark a memory as resolved, appending an optional resolution note to the body.
    /// Memories with no worker-generated keywords yet, newest first, plus how many are
    /// waiting in total. Ephemeral and superseded memories are never expanded.
    pub fn memories_needing_keywords(&self, limit: usize) -> Result<(Vec<Memory>, usize)> {
        const WHERE: &str = NEEDS_KEYWORDS;
        let total: i64 = self.conn.query_row(
            &format!("SELECT count(*) FROM memories WHERE {WHERE}"),
            [],
            |r| r.get(0),
        )?;
        let items = self
            .conn
            .prepare(&format!(
                "SELECT {MEMORY_COLS} FROM memories WHERE {WHERE} ORDER BY updated_at DESC LIMIT ?1"
            ))?
            .query_map(params![limit as i64], row_to_memory)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((items, total as usize))
    }

    /// Whether `id` is in the keyword queue (see [`Self::memories_needing_keywords`]).
    pub fn needs_keywords(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT 1 FROM memories WHERE id = ?1 AND {NEEDS_KEYWORDS}"),
                params![id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// Store search keywords for a memory (replacing any). An empty list marks it as done so
    /// it isn't queued again. Keywords live only in the index: they are dropped when the
    /// memory's title or body is rewritten, and a rebuild from git starts without them.
    pub fn set_keywords(&mut self, id: &str, keywords: &[String]) -> Result<()> {
        let joined = keywords.join(", ");
        check_for_secrets("", &joined, None)?;
        let n = self.conn.execute(
            "UPDATE memories SET keywords = ?2
             WHERE id = ?1 AND kind != 'ephemeral' AND status != 'superseded'",
            params![id, joined],
        )?;
        if n == 0 {
            bail!("Memory '{id}' not found, or ephemeral/superseded");
        }
        Ok(())
    }

    /// Worker-generated keywords for a memory, if any.
    pub fn get_keywords(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT keywords FROM memories WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .filter(|k| !k.is_empty()))
    }

    /// Pairs of active durable memories that look like duplicates or contradictions of each
    /// other and haven't been reviewed yet, each ordered (smaller id, larger id), plus how many
    /// such pairs the scan found. Bounded: one FTS query for each of the newest
    /// [`CONSOLIDATE_SCAN`] durable memories.
    pub fn consolidation_candidates(&self, limit: usize) -> Result<(Vec<(Memory, Memory)>, usize)> {
        let mems = self
            .conn
            .prepare(&format!(
                "SELECT {MEMORY_COLS} FROM memories WHERE status = 'active' AND {DURABLE_KINDS}
                 ORDER BY updated_at DESC, rowid DESC LIMIT ?1"
            ))?
            .query_map(params![CONSOLIDATE_SCAN as i64], row_to_memory)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut seen = std::collections::HashSet::new();
        let mut pairs = Vec::new();
        for m in &mems {
            for hit in self.consolidation_hits(m)? {
                let (a, b) = if m.id < hit.id {
                    (m.clone(), hit)
                } else {
                    (hit, m.clone())
                };
                if seen.insert((a.id.clone(), b.id.clone())) {
                    pairs.push((a, b));
                }
            }
        }
        let total = pairs.len();
        pairs.truncate(limit);
        Ok((pairs, total))
    }

    /// The best (at most [`CONSOLIDATE_HITS`]) unreviewed active durable memories matching any
    /// word of `memory`'s title, best first. Empty for a memory that isn't active and durable.
    pub fn consolidation_hits(&self, memory: &Memory) -> Result<Vec<Memory>> {
        if !is_consolidatable(memory) {
            return Ok(Vec::new());
        }
        let query = title_or_query(&memory.title);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let hits = self
            .conn
            .prepare(&format!(
                "SELECT m.{MEMORY_COLS_M} FROM memories_fts f JOIN memories m ON f.rowid = m.rowid
                 WHERE memories_fts MATCH ?1 AND m.id != ?2
                   AND m.status = 'active' AND m.{DURABLE_KINDS}
                   AND NOT EXISTS (SELECT 1 FROM reviewed_pairs r
                       WHERE (r.a = ?2 AND r.b = m.id) OR (r.a = m.id AND r.b = ?2))
                 ORDER BY bm25(memories_fts, {BM25_WEIGHTS}) LIMIT ?3",
                MEMORY_COLS_M = MEMORY_COLS.replace(", ", ", m.")
            ))?
            .query_map(
                params![query, memory.id, CONSOLIDATE_HITS as i64],
                row_to_memory,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(hits)
    }

    /// Whether `a` and `b` are a current consolidation candidate: both active and durable,
    /// not reviewed, and one is among the other's best matches.
    pub fn is_consolidation_candidate(&self, a: &Memory, b: &Memory) -> Result<bool> {
        if a.id == b.id || !is_consolidatable(a) || !is_consolidatable(b) {
            return Ok(false);
        }
        Ok(self.consolidation_hits(a)?.iter().any(|m| m.id == b.id)
            || self.consolidation_hits(b)?.iter().any(|m| m.id == a.id))
    }

    /// The verdict recorded for a pair of memories, in either order.
    pub fn pair_review(&self, a: &str, b: &str) -> Result<Option<String>> {
        let (a, b) = if a < b { (a, b) } else { (b, a) };
        Ok(self
            .conn
            .query_row(
                "SELECT verdict FROM reviewed_pairs WHERE a = ?1 AND b = ?2",
                params![a, b],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Record that a pair has been judged, so it isn't offered for consolidation again.
    pub fn mark_pair_reviewed(&mut self, a: &str, b: &str, verdict: &str) -> Result<()> {
        let (a, b) = if a < b { (a, b) } else { (b, a) };
        self.conn
            .execute(
                "INSERT OR REPLACE INTO reviewed_pairs (a, b, verdict) VALUES (?1, ?2, ?3)",
                params![a, b, verdict],
            )
            .context("Failed to record the reviewed pair")?;
        Ok(())
    }

    /// Someone checked a STALE memory against the current code and it still holds: take the
    /// current content of its cited paths as the new baseline. A write-through note is
    /// append-only, so for one of those a new note with the same content and fresh
    /// fingerprints supersedes it (and survives a rebuild); other memories are re-stamped in
    /// place. Returns the memory that now carries the claim.
    pub fn confirm_memory(&mut self, id: &str) -> Result<Memory> {
        let (mem, _) = self
            .get_memory(id)?
            .ok_or_else(|| anyhow::anyhow!("Memory '{id}' not found"))?;
        if mem.status != MemoryStatus::Active {
            bail!("Memory '{id}' is {}, not active", mem.status);
        }
        if mem.paths.is_empty() {
            bail!("Memory '{id}' cites no paths, so there is nothing to re-baseline");
        }
        let note_backed = mem.source.as_deref() == Some("note")
            && self
                .write_through
                .as_deref()
                .is_some_and(|d| d.join(format!("{id}.md")).is_file());
        if note_backed {
            return self.record_memory(RecordOptions {
                id: None,
                kind: mem.kind,
                title: mem.title,
                body: mem.body,
                tags: mem.tags,
                supersedes_id: Some(id.to_string()),
                session_id: None,
                embedding: None,
                paths: mem.paths,
                commit: None,
            });
        }
        // An imported row is rewritten from its source on the next import, so a re-baseline in
        // the database alone would be undone and the memory would turn stale again.
        if let Some(src) = mem.source.as_deref() {
            bail!(
                "Memory '{id}' is imported from {src}; it can only be re-baselined through its \
                 source (for notes, enable write-through so a superseding note can be written)"
            );
        }
        let (mut paths, mut commit, mut fingerprints) = (mem.paths.clone(), None, Vec::new());
        self.stamp_provenance(&mut paths, &mut commit, &mut fingerprints)?;
        if fingerprints.is_empty() {
            bail!("Could not fingerprint the cited paths of '{id}' (is this a git repository?)");
        }
        self.conn
            .query_row(
                &format!(
                    "UPDATE memories SET fingerprints = ?2, commit_sha = ?3,
                     updated_at = CURRENT_TIMESTAMP WHERE id = ?1 RETURNING {MEMORY_COLS}"
                ),
                params![id, join_paths(&fingerprints), commit],
                row_to_memory,
            )
            .context("Failed to re-baseline memory")
    }

    pub fn resolve_memory(&mut self, id: &str, note: Option<&str>) -> Result<Memory> {
        let note = note.map(str::trim).filter(|n| !n.is_empty());
        if let Some(n) = note {
            check_for_secrets("", n, None)?;
        }

        // A note-backed memory is rebuilt from its file, so a database-only status change
        // would be lost on the next rebuild. Record the resolution next to the note (the note
        // itself stays append-only) and let the importer merge it.
        let sidecar = self.resolution_sidecar_path(id)?;
        if let Some(path) = &sidecar {
            write_new_file_atomic(path, render_resolution(id, note).as_bytes()).with_context(
                || {
                    format!(
                        "Failed to record the resolution of '{id}' in {}",
                        path.display()
                    )
                },
            )?;
        }

        let result = self
            .conn
            .query_row(
                &format!(
                    "UPDATE memories SET status = 'resolved', body = body || ?2,
                     updated_at = CURRENT_TIMESTAMP WHERE id = ?1
                     RETURNING {MEMORY_COLS}"
                ),
                params![id, resolution_suffix(note)],
                row_to_memory,
            )
            .optional()
            .context("Failed to resolve memory")
            .and_then(|m| m.ok_or_else(|| anyhow::anyhow!("Memory '{}' not found", id)));
        if result.is_err()
            && let Some(path) = &sidecar
        {
            std::fs::remove_file(path).ok();
        }
        result
    }

    /// `<notes_dir>/<id>.resolved.md` when `id` is backed by a write-through note file, else
    /// None. Errors if that memory was already resolved (resolutions are append-only too).
    fn resolution_sidecar_path(&self, id: &str) -> Result<Option<PathBuf>> {
        let Some(dir) = self.write_through.as_deref() else {
            return Ok(None);
        };
        if !is_valid_note_id(id) || !dir.join(format!("{id}.md")).is_file() {
            return Ok(None);
        }
        let source: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT source FROM memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        if source.flatten().as_deref() != Some("note") {
            return Ok(None);
        }
        let path = dir.join(format!("{id}{RESOLVED_SUFFIX}.md"));
        if path.exists() {
            bail!("Memory '{id}' is already resolved ({})", path.display());
        }
        Ok(Some(path))
    }

    /// Search memories by dense vector embedding using sqlite-vec KNN.
    /// The `rank` field of each result carries the vector distance (lower is closer).
    pub fn search_vector(
        &self,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let mut stmt = self.conn.prepare(&format!(
            r#"
            SELECT m.{MEMORY_COLS_M}, v.distance
            FROM memories_vec v
            JOIN memories m ON v.id = m.id
            WHERE v.embedding MATCH ?1 AND v.k = ?2
            ORDER BY v.distance ASC
            "#,
            MEMORY_COLS_M = MEMORY_COLS.replace(", ", ", m.")
        ))?;

        let hits = stmt
            .query_map(
                params![embedding_to_json(embedding), limit.clamp(1, 100) as i64],
                |row| Ok((row_to_memory(row)?, row.get::<_, f64>(SCORE_COL)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;

        self.finish_results(hits)
    }

    /// List memories with optional filtering.
    pub fn list_memories(
        &self,
        status: Option<MemoryStatus>,
        kind: Option<MemoryKind>,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        let mut stmt = self.conn.prepare(&format!(
            r#"
            SELECT {MEMORY_COLS} FROM memories
            WHERE (:status IS NULL OR status = :status)
              AND (:kind IS NULL OR kind = :kind)
            ORDER BY updated_at DESC
            LIMIT :limit
            "#
        ))?;

        let list = stmt
            .query_map(
                rusqlite::named_params! {
                    ":status": status.map(|s| s.as_str()),
                    ":kind": kind.map(|k| k.as_str()),
                    ":limit": limit as i64,
                },
                row_to_memory,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(list)
    }

    /// Active tasks/blockers with no incoming active `blocks` edge — the multi-agent "ready
    /// queue": one indexed query instead of list_memories + get_memory-per-row to check
    /// blocked_by relations.
    pub fn list_ready(&self, limit: usize) -> Result<Vec<Memory>> {
        let mut stmt = self.conn.prepare(&format!(
            r#"
            SELECT {MEMORY_COLS} FROM memories m
            WHERE m.status = 'active'
              AND m.kind IN ('task', 'blocker')
              AND NOT EXISTS (
                  SELECT 1 FROM memory_relations r
                  JOIN memories b ON r.source_id = b.id
                  WHERE r.target_id = m.id
                    AND r.relation_type = 'blocks'
                    AND b.status = 'active'
              )
            ORDER BY m.created_at ASC
            LIMIT :limit
            "#
        ))?;

        let list = stmt
            .query_map(
                rusqlite::named_params! { ":limit": limit as i64 },
                row_to_memory,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(list)
    }

    /// Get overall database statistics.
    /// Dump every memory and relation as a plain struct, for a JSON sidecar export
    /// (e.g. to commit alongside the DB, or migrate data between machines).
    pub fn export_all(&self) -> Result<ExportData> {
        let memories = self
            .conn
            .prepare(&format!(
                "SELECT {MEMORY_COLS} FROM memories ORDER BY created_at"
            ))?
            .query_map([], row_to_memory)?
            .collect::<Result<Vec<_>, _>>()?;

        let relations = self
            .conn
            .prepare(
                "SELECT source_id, target_id, relation_type, created_at \
                 FROM memory_relations ORDER BY created_at",
            )?
            .query_map([], |row| {
                let relation_type: String = row.get(2)?;
                Ok(MemoryRelation {
                    source_id: row.get(0)?,
                    target_id: row.get(1)?,
                    relation_type: RelationType::from_str(&relation_type)
                        .unwrap_or(RelationType::References),
                    created_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ExportData {
            memories,
            relations,
        })
    }

    pub fn stats(&self) -> Result<DatabaseStats> {
        let total_memories: i64 =
            self.conn
                .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))?;
        let active_memories: i64 = self.conn.query_row(
            "SELECT count(*) FROM memories WHERE status = 'active'",
            [],
            |r| r.get(0),
        )?;
        let superseded_memories: i64 = self.conn.query_row(
            "SELECT count(*) FROM memories WHERE status = 'superseded'",
            [],
            |r| r.get(0),
        )?;
        let resolved_memories: i64 = self.conn.query_row(
            "SELECT count(*) FROM memories WHERE status = 'resolved'",
            [],
            |r| r.get(0),
        )?;
        let ephemeral_memories: i64 = self.conn.query_row(
            "SELECT count(*) FROM memories WHERE status = 'ephemeral'",
            [],
            |r| r.get(0),
        )?;
        let total_relations: i64 =
            self.conn
                .query_row("SELECT count(*) FROM memory_relations", [], |r| r.get(0))?;

        let mut stmt = self.conn.prepare(
            "SELECT relation_type, count(*) FROM memory_relations GROUP BY relation_type",
        )?;
        let rel_counts = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<(String, i64)>, _>>()?;

        Ok(DatabaseStats {
            total_memories,
            active_memories,
            superseded_memories,
            resolved_memories,
            ephemeral_memories,
            total_relations,
            relations_by_type: rel_counts,
        })
    }
}

/// Token-shaped secrets: `(prefix, minimum characters after it, label)`. A prefix only counts
/// at a word boundary, and the token body must be long enough to be a real credential, so
/// ordinary words like "task-queue", "risk-based" or "disk-backed" don't trip `sk-`.
const TOKEN_PREFIXES: &[(&str, usize, &str)] = &[
    ("sk-", 16, "API secret key (sk-)"),
    ("sk_live_", 16, "Stripe live key"),
    ("sk_test_", 16, "Stripe test key"),
    ("ghp_", 30, "GitHub token"),
    ("gho_", 30, "GitHub token"),
    ("ghu_", 30, "GitHub token"),
    ("ghs_", 30, "GitHub token"),
    ("github_pat_", 30, "GitHub token"),
    ("glpat-", 16, "GitLab token"),
    ("xoxb-", 20, "Slack token"),
    ("xoxp-", 20, "Slack token"),
    ("xoxa-", 20, "Slack token"),
    ("npm_", 30, "npm token"),
    ("AKIA", 16, "AWS access key id"),
    ("AIza", 35, "Google API key"),
];

/// Names that, followed by `=` or `:` and a long value, indicate an inline credential.
const SECRET_NAMES: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "secret_key",
    "client_secret",
    "aws_secret_access_key",
    "password",
    "passwd",
    "authorization: bearer",
];

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Characters allowed in an inline credential value.
fn is_value_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "+/_-.=~".contains(c)
}

fn boundary_before(text: &str, idx: usize) -> bool {
    text[..idx]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_ascii_alphanumeric())
}

/// A description of the first secret-looking thing in `text`, never the secret itself.
/// Heuristic denylist, not a scanner: it catches accidental paste-ins of common credential
/// shapes, not deliberately obfuscated ones.
/// ponytail: hardcoded; move to a config file if false positives pile up.
fn find_secret(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase(); // ASCII-only, so byte offsets match `text`
    if lower.contains("-----begin ") {
        return Some("PEM private key or certificate");
    }

    for (prefix, min_len, label) in TOKEN_PREFIXES {
        for (idx, _) in text.match_indices(prefix) {
            if !boundary_before(text, idx) {
                continue;
            }
            let body = text[idx + prefix.len()..]
                .chars()
                .take_while(|c| is_token_char(*c))
                .count();
            if body >= *min_len {
                return Some(label);
            }
        }
    }

    // JWT: eyJ<b64>.<b64>
    for (idx, _) in text.match_indices("eyJ") {
        if !boundary_before(text, idx) {
            continue;
        }
        let rest = &text[idx..];
        let head = rest.chars().take_while(|c| is_token_char(*c)).count();
        if head >= 10 && rest[head..].starts_with('.') {
            let tail = rest[head + 1..]
                .chars()
                .take_while(|c| is_token_char(*c))
                .count();
            if tail >= 10 {
                return Some("JSON Web Token");
            }
        }
    }

    for name in SECRET_NAMES {
        for (idx, _) in lower.match_indices(name) {
            let after = text[idx + name.len()..].trim_start_matches([' ', '\t']);
            let after = if *name == "authorization: bearer" {
                after
            } else {
                match after.strip_prefix(['=', ':']) {
                    Some(v) => v.trim_start_matches([' ', '\t', '"', '\'']),
                    None => continue,
                }
            };
            if after.chars().take_while(|c| is_value_char(*c)).count() >= 12 {
                return Some("inline credential (key/password assignment)");
            }
        }
    }
    None
}

/// Reject recording a memory whose title/body/tags contain an obvious secret.
pub(crate) fn check_for_secrets(title: &str, body: &str, tags: Option<&str>) -> Result<()> {
    let haystack = format!("{title}\n{body}\n{}", tags.unwrap_or(""));
    if let Some(what) = find_secret(&haystack) {
        bail!(
            "Refusing to record memory: content matches secret pattern '{what}'. Redact it before retrying."
        );
    }
    Ok(())
}

/// Map a row starting with the `MEMORY_COLS` columns into a `Memory`.
fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        kind: MemoryKind::from_str(&row.get::<_, String>(1)?).unwrap_or(MemoryKind::Decision),
        title: row.get(2)?,
        body: row.get(3)?,
        tags: row.get(4)?,
        status: MemoryStatus::from_str(&row.get::<_, String>(5)?).unwrap_or(MemoryStatus::Active),
        session_id: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        paths: split_paths(row.get::<_, Option<String>>(9)?.as_deref()),
        commit: row.get(10)?,
        fingerprints: split_paths(row.get::<_, Option<String>>(11)?.as_deref()),
        source: row.get(12)?,
    })
}

/// sqlite-vec takes vectors as a JSON array string.
fn embedding_to_json(embedding: &[f32]) -> String {
    format!(
        "[{}]",
        embedding
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Reciprocal Rank Fusion over ranked id lists: `sum(1 / (k + rank))`, rank starting at 1.
/// Rank-based, so BM25 scores and vector distances need no calibration. Ties keep first
/// appearance order.
pub fn rrf_scores(lists: &[Vec<String>]) -> Vec<(String, f64)> {
    let mut scores: Vec<(String, f64)> = Vec::new();
    for list in lists {
        for (i, id) in list.iter().enumerate() {
            let add = 1.0 / (RRF_K + (i + 1) as f64);
            match scores.iter_mut().find(|(sid, _)| sid == id) {
                Some((_, s)) => *s += add,
                None => scores.push((id.clone(), add)),
            }
        }
    }
    scores.sort_by(|a, b| b.1.total_cmp(&a.1));
    scores
}

/// Fuse BM25 and vector hits into one ranking of at most `limit` memories.
fn fuse_ranked(
    bm25: Vec<(Memory, f64)>,
    vector: Vec<(Memory, f64)>,
    limit: usize,
) -> Vec<(Memory, f64)> {
    let lists = [
        bm25.iter().map(|(m, _)| m.id.clone()).collect::<Vec<_>>(),
        vector.iter().map(|(m, _)| m.id.clone()).collect::<Vec<_>>(),
    ];
    let mut by_id: std::collections::HashMap<String, Memory> = std::collections::HashMap::new();
    for (m, _) in bm25.into_iter().chain(vector) {
        by_id.entry(m.id.clone()).or_insert(m);
    }
    rrf_scores(&lists)
        .into_iter()
        .take(limit)
        .filter_map(|(id, score)| by_id.remove(&id).map(|m| (m, score)))
        .collect()
}

/// Rough token estimate (about 4 characters per token); no tokenizer dependency.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Sanitize search string into valid FTS5 tokens
pub fn sanitize_fts5_query(input: &str) -> String {
    input
        .split_whitespace()
        .filter_map(|word| {
            let clean: String = word
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            (!clean.is_empty()).then(|| format!("\"{clean}\"*"))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Render 1-hop DAG relations as direction-aware markdown bullets.
pub fn format_relations(relations: &[RelatedEntity]) -> String {
    relations
        .iter()
        .map(|rel| {
            let (label, arrow) = match rel.direction {
                EdgeDirection::Outgoing => (rel.relation_type.as_str(), "\u{2794}"),
                EdgeDirection::Incoming => (rel.relation_type.inverse(), "\u{25c4}"),
            };
            format!(
                "  - `{}` {} [{}] {} ({})\n",
                label,
                arrow,
                rel.target_id,
                rel.target_title.as_deref().unwrap_or("Unknown"),
                rel.target_status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
            )
        })
        .collect()
}

/// Provenance lines for a memory: what it cites, and a STALE warning if those paths moved.
pub fn format_provenance(memory: &Memory, stale_paths: &[String]) -> String {
    let mut out = String::new();
    if !memory.paths.is_empty() {
        let at = memory
            .commit
            .as_deref()
            .map(|c| format!(" (as of {})", &c[..c.len().min(7)]))
            .unwrap_or_default();
        out.push_str(&format!("- **Cites**: {}{at}\n", memory.paths.join(", ")));
    }
    if !stale_paths.is_empty() {
        out.push_str(&format!(
            "- **STALE**: cited paths changed since this was recorded: {}. Verify against the code before relying on it.\n",
            stale_paths.join(", ")
        ));
    }
    out
}

/// Format search results (FTS or vector) as markdown for LLM context windows, unbounded.
pub fn format_search_results_for_llm(results: &[MemorySearchResult]) -> String {
    format_search_results_with_budget(results, None)
}

/// Collapse a title to one line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Escape lines that start with `#` so memory text can't open a fake `### n. [ID]` result
/// block (or any heading) in the model's context.
fn neutralize_headings(body: &str) -> String {
    body.lines()
        .map(|l| {
            if l.trim_start().starts_with('#') {
                format!("\\{l}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One result as markdown. `body_chars` truncates the body (used when a single result must
/// be squeezed into the budget).
fn render_result(index: usize, res: &MemorySearchResult, body_chars: Option<usize>) -> String {
    let mem = &res.memory;
    // Imported rows come from files and comments anyone with repo access can write, so say
    // where they came from and keep their text from forging result blocks.
    let mut source = mem
        .source
        .as_deref()
        .map(|s| format!(", source: {s}"))
        .unwrap_or_default();
    // A decision on another worktree's branch may never be merged; say so.
    if res.other_checkout {
        source.push_str(", only in another worktree");
    }
    let mut out = format!(
        "### {}. [{}] {} (kind: {}, status: {}{source}, score: {:.4})\n",
        index,
        mem.id,
        one_line(&mem.title),
        mem.kind,
        mem.status,
        res.rank
    );
    if let Some(tags) = mem.tags.as_deref().filter(|t| !t.trim().is_empty()) {
        out.push_str(&format!("- **Tags**: {tags}\n"));
    }
    out.push_str(&format_provenance(mem, &res.stale_paths));
    let body = &neutralize_headings(mem.body.trim());
    match body_chars {
        Some(limit) if body.chars().count() > limit => {
            let cut: String = body.chars().take(limit).collect();
            out.push_str(&format!(
                "- **Body**: {}\u{2026} [truncated; call get_memory for the full text]\n",
                cut.trim_end()
            ));
        }
        _ => out.push_str(&format!("- **Body**: {body}\n")),
    }
    if !res.relations.is_empty() {
        out.push_str("- **Relations (1-Hop DAG)**:\n");
        out.push_str(&format_relations(
            &res.relations[..res.relations.len().min(MAX_RELATIONS_SHOWN)],
        ));
        if res.relations.len() > MAX_RELATIONS_SHOWN {
            out.push_str(&format!(
                "  - (+{} more; call get_memory for all)\n",
                res.relations.len() - MAX_RELATIONS_SHOWN
            ));
        }
    }
    out.push('\n');
    out
}

/// Pack results, in rank order, into an estimated-token budget (`None` = unlimited).
/// Always returns at least the top result (body truncated if it alone exceeds the budget)
/// and reports how many lower-ranked results were left out.
pub fn format_search_results_with_budget(
    results: &[MemorySearchResult],
    max_tokens: Option<usize>,
) -> String {
    if results.is_empty() {
        return "No relevant memories found matching the query.".to_string();
    }

    // Reserve room for the header and the omitted-results footer.
    const OVERHEAD_TOKENS: usize = 60;
    let mut blocks: Vec<String> = Vec::new();
    let mut used = OVERHEAD_TOKENS;
    for (i, res) in results.iter().enumerate() {
        let block = render_result(i + 1, res, None);
        let cost = estimate_tokens(&block);
        match max_tokens {
            Some(budget) if used + cost > budget => {
                if blocks.is_empty() {
                    // Top result alone is too big: keep it, shortened to what fits.
                    let fixed = estimate_tokens(&render_result(i + 1, res, Some(0)));
                    let room = budget.saturating_sub(used + fixed).max(25);
                    blocks.push(render_result(i + 1, res, Some(room * 4)));
                }
                break;
            }
            _ => {
                used += cost;
                blocks.push(block);
            }
        }
    }

    let shown = blocks.len();
    let mut out = match max_tokens {
        Some(budget) if shown < results.len() => format!(
            "Found {} relevant memory/DAG entries (showing top {shown} within a ~{budget}-token budget):\n\n",
            results.len()
        ),
        _ => format!("Found {} relevant memory/DAG entries:\n\n", results.len()),
    };
    for b in &blocks {
        out.push_str(b);
    }
    if shown < results.len() {
        out.push_str(&format!(
            "({} lower-ranked result(s) omitted. Narrow the query, raise max_tokens, or fetch one with get_memory.)\n",
            results.len() - shown
        ));
    }
    out
}
