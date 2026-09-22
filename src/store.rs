use crate::models::{
    DatabaseStats, EdgeDirection, Memory, MemoryKind, MemorySearchResult, MemoryStatus,
    RelatedEntity, RelationType, SessionLearning,
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::str::FromStr;

/// The 9 memory columns, in the order `row_to_memory` expects.
const MEMORY_COLS: &str = "id, kind, title, body, tags, status, session_id, created_at, updated_at";

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

pub struct MemoryStore {
    conn: Connection,
}

impl MemoryStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Record a memory entity with atomic supersession handling.
    pub fn record_memory(&mut self, opts: RecordOptions) -> Result<Memory> {
        check_for_secrets(&opts.title, &opts.body, opts.tags.as_deref())?;

        let new_id = opts.id.unwrap_or_else(|| {
            let short_uuid = &uuid::Uuid::new_v4().to_string()[..8];
            format!("{}-{}", opts.kind.prefix(), short_uuid)
        });

        let status = if opts.kind == MemoryKind::Ephemeral {
            MemoryStatus::Ephemeral
        } else {
            MemoryStatus::Active
        };

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
            INSERT INTO memories (id, kind, title, body, tags, status, session_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                new_id,
                opts.kind.as_str(),
                opts.title,
                opts.body,
                opts.tags,
                status.as_str(),
                opts.session_id,
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

    /// Search active memories using FTS5 BM25 with bidirectional 1-hop DAG expansion.
    /// An empty or untokenizable query falls back to the most recently updated memories.
    pub fn search_memory(&self, opts: SearchOptions) -> Result<Vec<MemorySearchResult>> {
        let fts_query = sanitize_fts5_query(&opts.query);
        let limit = opts.limit.clamp(1, 100);
        let status = if opts.include_resolved {
            None
        } else {
            Some(MemoryStatus::Active)
        };

        let ranked: Vec<(Memory, f64)> = if fts_query.is_empty() {
            self.list_memories(status, opts.kind, limit)?
                .into_iter()
                .map(|m| (m, 0.0))
                .collect()
        } else {
            let mut stmt = self.conn.prepare(&format!(
                r#"
                SELECT m.{MEMORY_COLS_M}, bm25(memories_fts) AS rank
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

            stmt.query_map(
                rusqlite::named_params! {
                    ":query": fts_query,
                    ":include_resolved": opts.include_resolved as i64,
                    ":kind": opts.kind.map(|k| k.as_str()),
                    ":limit": limit as i64,
                },
                |row| Ok((row_to_memory(row)?, row.get::<_, f64>(9)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?
        };

        // ponytail: one relations query per hit (<=100). Single JOIN if that ever shows up in a profile.
        ranked
            .into_iter()
            .map(|(memory, rank)| {
                Ok(MemorySearchResult {
                    relations: self.get_memory_relations(&memory.id)?,
                    memory,
                    rank,
                })
            })
            .collect()
    }

    /// Consolidate session insights: insert permanent learnings and purge or archive ephemeral entries.
    pub fn consolidate_session(
        &mut self,
        session_id: &str,
        learnings: Vec<SessionLearning>,
        purge_ephemeral: bool,
    ) -> Result<ConsolidationResult> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("Failed to begin immediate transaction for consolidate_session")?;

        let mut recorded_ids = Vec::new();
        for learning in &learnings {
            check_for_secrets(&learning.title, &learning.body, learning.tags.as_deref())?;

            let short_uuid = &uuid::Uuid::new_v4().to_string()[..8];
            let new_id = format!("{}-{}", learning.kind.prefix(), short_uuid);

            tx.execute(
                r#"
                INSERT INTO memories (id, kind, title, body, tags, status, session_id)
                VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)
                "#,
                params![
                    new_id,
                    learning.kind.as_str(),
                    learning.title,
                    learning.body,
                    learning.tags,
                    session_id,
                ],
            )
            .context("Failed to insert consolidated session learning")?;

            recorded_ids.push(new_id);
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
    pub fn resolve_memory(&mut self, id: &str, note: Option<&str>) -> Result<Memory> {
        let suffix = note
            .map(|n| format!("\n\n[Resolved]: {n}"))
            .unwrap_or_default();
        self.conn
            .query_row(
                &format!(
                    "UPDATE memories SET status = 'resolved', body = body || ?2,
                     updated_at = CURRENT_TIMESTAMP WHERE id = ?1
                     RETURNING {MEMORY_COLS}"
                ),
                params![id, suffix],
                row_to_memory,
            )
            .optional()
            .context("Failed to resolve memory")?
            .ok_or_else(|| anyhow::anyhow!("Memory '{}' not found", id))
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
                |row| Ok((row_to_memory(row)?, row.get::<_, f64>(9)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;

        hits.into_iter()
            .map(|(memory, rank)| {
                Ok(MemorySearchResult {
                    relations: self.get_memory_relations(&memory.id)?,
                    memory,
                    rank,
                })
            })
            .collect()
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

/// Case-insensitive substrings that flag likely secrets/credentials before they get written
/// into a memory. Not exhaustive, catches common API key/token prefixes and PEM blocks.
/// ponytail: hardcoded denylist; move to a config file if false positives pile up.
const SECRET_PATTERNS: &[&str] = &[
    "-----begin ", // PEM private keys/certs
    "sk-",         // OpenAI-style secret keys
    "ghp_",
    "gho_",
    "github_pat_", // GitHub tokens
    "xoxb-",
    "xoxp-", // Slack tokens
    "aws_secret_access_key",
    "akia", // AWS access key id prefix
    "aiza", // Google API key prefix
    "api_key=",
    "apikey=",
    "api-key:",
    "authorization: bearer",
];

/// Reject recording a memory whose title/body/tags contain an obvious secret pattern.
fn check_for_secrets(title: &str, body: &str, tags: Option<&str>) -> Result<()> {
    let haystack = format!("{title}\n{body}\n{}", tags.unwrap_or("")).to_lowercase();
    if let Some(pat) = SECRET_PATTERNS.iter().find(|p| haystack.contains(*p)) {
        bail!(
            "Refusing to record memory: content matches secret pattern '{}'. Redact it before retrying.",
            pat.trim()
        );
    }
    Ok(())
}

/// Map a row of `MEMORY_COLS` (optionally prefixed) into a `Memory`.
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

/// Format search results (FTS or vector) as markdown for LLM context windows.
pub fn format_search_results_for_llm(results: &[MemorySearchResult]) -> String {
    if results.is_empty() {
        return "No relevant memories found matching the query.".to_string();
    }

    let mut out = format!("Found {} relevant memory/DAG entries:\n\n", results.len());
    for (i, res) in results.iter().enumerate() {
        let mem = &res.memory;
        out.push_str(&format!(
            "### {}. [{}] {} (kind: {}, status: {}, score: {:.4})\n",
            i + 1,
            mem.id,
            mem.title,
            mem.kind,
            mem.status,
            res.rank
        ));
        if let Some(tags) = mem.tags.as_deref().filter(|t| !t.trim().is_empty()) {
            out.push_str(&format!("- **Tags**: {tags}\n"));
        }
        out.push_str(&format!("- **Body**: {}\n", mem.body.trim()));
        if !res.relations.is_empty() {
            out.push_str("- **Relations (1-Hop DAG)**:\n");
            out.push_str(&format_relations(&res.relations));
        }
        out.push('\n');
    }
    out
}
