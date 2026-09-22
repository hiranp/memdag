use crate::models::{
    DatabaseStats, Memory, MemoryKind, MemorySearchResult, MemoryStatus, RelatedEntity,
    RelationType, SessionLearning,
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::HashMap;
use std::str::FromStr;

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

    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Record a memory entity with atomic supersession handling.
    pub fn record_memory(&mut self, opts: RecordOptions) -> Result<Memory> {
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
            let emb_str = format!("[{}]", emb.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(","));
            tx.execute(
                "INSERT OR REPLACE INTO memories_vec (id, embedding) VALUES (?1, ?2)",
                params![new_id, emb_str],
            )
            .context("Failed to insert vector embedding into memories_vec")?;
        }

        // Fetch the recorded memory row
        let memory = tx.query_row(
            r#"
            SELECT id, kind, title, body, tags, status, session_id, created_at, updated_at
            FROM memories WHERE id = ?1
            "#,
            params![new_id],
            |row| {
                let kind_str: String = row.get(1)?;
                let status_str: String = row.get(5)?;
                Ok(Memory {
                    id: row.get(0)?,
                    kind: MemoryKind::from_str(&kind_str).unwrap_or(MemoryKind::Decision),
                    title: row.get(2)?,
                    body: row.get(3)?,
                    tags: row.get(4)?,
                    status: MemoryStatus::from_str(&status_str).unwrap_or(MemoryStatus::Active),
                    session_id: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                })
            },
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

    /// Search active memories using FTS5 BM25 with 1-hop DAG expansion.
    pub fn search_memory(&self, opts: SearchOptions) -> Result<Vec<MemorySearchResult>> {
        let fts_query = sanitize_fts5_query(&opts.query);
        if fts_query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let include_resolved = if opts.include_resolved { 1 } else { 0 };
        let kind_str = opts.kind.map(|k| k.as_str().to_string());
        let limit = opts.limit.max(1).min(100) as i64;

        let query_sql = r#"
        WITH matched_memories AS (
            SELECT 
                m.id, m.kind, m.title, m.body, m.tags, m.status, m.session_id, m.created_at, m.updated_at,
                bm25(memories_fts) AS rank
            FROM memories_fts f
            JOIN memories m ON f.rowid = m.rowid
            WHERE memories_fts MATCH :query
              AND (:include_resolved = 1 OR m.status = 'active')
              AND (:kind IS NULL OR m.kind = :kind)
            ORDER BY rank
            LIMIT :limit
        )
        SELECT 
            mm.id, mm.kind, mm.title, mm.body, mm.tags, mm.status, mm.session_id, mm.created_at, mm.updated_at, mm.rank,
            r.relation_type,
            target.id AS related_id,
            target.title AS related_title,
            target.kind AS related_kind,
            target.status AS related_status
        FROM matched_memories mm
        LEFT JOIN memory_relations r ON mm.id = r.source_id
        LEFT JOIN memories target ON r.target_id = target.id
        ORDER BY mm.rank ASC;
        "#;

        let mut stmt = self.conn.prepare(query_sql)?;
        let rows = stmt.query_map(
            rusqlite::named_params! {
                ":query": fts_query,
                ":include_resolved": include_resolved,
                ":kind": kind_str,
                ":limit": limit,
            },
            |row| {
                let id: String = row.get(0)?;
                let kind_str: String = row.get(1)?;
                let title: String = row.get(2)?;
                let body: String = row.get(3)?;
                let tags: Option<String> = row.get(4)?;
                let status_str: String = row.get(5)?;
                let session_id: Option<String> = row.get(6)?;
                let created_at: String = row.get(7)?;
                let updated_at: String = row.get(8)?;
                let rank: f64 = row.get(9)?;

                let relation_type_str: Option<String> = row.get(10)?;
                let related_id: Option<String> = row.get(11)?;
                let related_title: Option<String> = row.get(12)?;
                let related_kind_str: Option<String> = row.get(13)?;
                let related_status_str: Option<String> = row.get(14)?;

                let relation = match (relation_type_str, related_id) {
                    (Some(rel_str), Some(rel_id)) => Some(RelatedEntity {
                        relation_type: RelationType::from_str(&rel_str).unwrap_or(RelationType::References),
                        target_id: rel_id,
                        target_title: related_title,
                        target_kind: related_kind_str.and_then(|k| MemoryKind::from_str(&k).ok()),
                        target_status: related_status_str.and_then(|s| MemoryStatus::from_str(&s).ok()),
                    }),
                    _ => None,
                };

                let memory = Memory {
                    id,
                    kind: MemoryKind::from_str(&kind_str).unwrap_or(MemoryKind::Decision),
                    title,
                    body,
                    tags,
                    status: MemoryStatus::from_str(&status_str).unwrap_or(MemoryStatus::Active),
                    session_id,
                    created_at,
                    updated_at,
                };

                Ok((memory, rank, relation))
            },
        )?;

        // Aggregate rows into MemorySearchResult with grouped relations
        let mut results_map: HashMap<String, (Memory, f64, Vec<RelatedEntity>)> = HashMap::new();
        let mut ordered_ids: Vec<String> = Vec::new();

        for row_result in rows {
            let (memory, rank, relation) = row_result?;
            let mem_id = memory.id.clone();

            if !results_map.contains_key(&mem_id) {
                ordered_ids.push(mem_id.clone());
                results_map.insert(mem_id.clone(), (memory, rank, Vec::new()));
            }

            if let Some(rel) = relation {
                if let Some(entry) = results_map.get_mut(&mem_id) {
                    entry.2.push(rel);
                }
            }
        }

        let mut results = Vec::new();
        for id in ordered_ids {
            if let Some((memory, rank, relations)) = results_map.remove(&id) {
                results.push(MemorySearchResult {
                    memory,
                    rank,
                    relations,
                });
            }
        }

        Ok(results)
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

        tx.commit().context("Failed to commit consolidate_session")?;

        Ok(ConsolidationResult {
            session_id: session_id.to_string(),
            learnings_recorded: recorded_ids.len(),
            ephemeral_purged,
            ephemeral_archived,
            recorded_ids,
        })
    }

    /// Get a single memory by ID along with its immediate relations.
    pub fn get_memory(&self, id: &str) -> Result<Option<(Memory, Vec<RelatedEntity>)>> {
        let memory: Option<Memory> = self
            .conn
            .query_row(
                r#"
                SELECT id, kind, title, body, tags, status, session_id, created_at, updated_at
                FROM memories WHERE id = ?1
                "#,
                params![id],
                |row| {
                    let kind_str: String = row.get(1)?;
                    let status_str: String = row.get(5)?;
                    Ok(Memory {
                        id: row.get(0)?,
                        kind: MemoryKind::from_str(&kind_str).unwrap_or(MemoryKind::Decision),
                        title: row.get(2)?,
                        body: row.get(3)?,
                        tags: row.get(4)?,
                        status: MemoryStatus::from_str(&status_str).unwrap_or(MemoryStatus::Active),
                        session_id: row.get(6)?,
                        created_at: row.get(7)?,
                        updated_at: row.get(8)?,
                    })
                },
            )
            .optional()?;

        let Some(memory) = memory else {
            return Ok(None);
        };

        let mut stmt = self.conn.prepare(
            r#"
            SELECT r.relation_type, target.id, target.title, target.kind, target.status
            FROM memory_relations r
            JOIN memories target ON r.target_id = target.id
            WHERE r.source_id = ?1
            "#,
        )?;

        let relations = stmt
            .query_map(params![id], |row| {
                let rel_str: String = row.get(0)?;
                let rel_id: String = row.get(1)?;
                let title: Option<String> = row.get(2)?;
                let kind_str: Option<String> = row.get(3)?;
                let status_str: Option<String> = row.get(4)?;

                Ok(RelatedEntity {
                    relation_type: RelationType::from_str(&rel_str).unwrap_or(RelationType::References),
                    target_id: rel_id,
                    target_title: title,
                    target_kind: kind_str.and_then(|k| MemoryKind::from_str(&k).ok()),
                    target_status: status_str.and_then(|s| MemoryStatus::from_str(&s).ok()),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Some((memory, relations)))
    }

    /// List memories with optional filtering.
    pub fn list_memories(
        &self,
        status: Option<MemoryStatus>,
        kind: Option<MemoryKind>,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        let status_str = status.map(|s| s.as_str().to_string());
        let kind_str = kind.map(|k| k.as_str().to_string());

        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, kind, title, body, tags, status, session_id, created_at, updated_at
            FROM memories
            WHERE (:status IS NULL OR status = :status)
              AND (:kind IS NULL OR kind = :kind)
            ORDER BY updated_at DESC
            LIMIT :limit
            "#,
        )?;

        let rows = stmt.query_map(
            rusqlite::named_params! {
                ":status": status_str,
                ":kind": kind_str,
                ":limit": limit as i64,
            },
            |row| {
                let kind_str: String = row.get(1)?;
                let status_str: String = row.get(5)?;
                Ok(Memory {
                    id: row.get(0)?,
                    kind: MemoryKind::from_str(&kind_str).unwrap_or(MemoryKind::Decision),
                    title: row.get(2)?,
                    body: row.get(3)?,
                    tags: row.get(4)?,
                    status: MemoryStatus::from_str(&status_str).unwrap_or(MemoryStatus::Active),
                    session_id: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                })
            },
        )?;

        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    /// Get overall database statistics.
    pub fn stats(&self) -> Result<DatabaseStats> {
        let total_memories: i64 = self
            .conn
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
        let total_relations: i64 = self
            .conn
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

/// Sanitize search string into valid FTS5 tokens
pub fn sanitize_fts5_query(input: &str) -> String {
    let tokens: Vec<String> = input
        .split_whitespace()
        .filter_map(|word| {
            let clean: String = word
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if clean.is_empty() {
                None
            } else {
                Some(format!("\"{}\"*", clean))
            }
        })
        .collect();

    if tokens.is_empty() {
        "".to_string()
    } else {
        tokens.join(" ")
    }
}

/// Format search results into a clean markdown document for LLM context windows
pub fn format_search_results_for_llm(results: &[MemorySearchResult]) -> String {
    if results.is_empty() {
        return "No relevant memories found matching the query.".to_string();
    }

    let mut out = String::new();
    out.push_str(&format!("Found {} relevant active memory/DAG entries:\n\n", results.len()));

    for (i, res) in results.iter().enumerate() {
        let mem = &res.memory;
        out.push_str(&format!(
            "### {}. [{}] {} (kind: {}, status: {})\n",
            i + 1,
            mem.id,
            mem.title,
            mem.kind,
            mem.status
        ));

        if let Some(ref tags) = mem.tags {
            if !tags.trim().is_empty() {
                out.push_str(&format!("- **Tags**: {}\n", tags));
            }
        }

        out.push_str(&format!("- **Body**: {}\n", mem.body.trim()));

        if !res.relations.is_empty() {
            out.push_str("- **Relations (1-Hop DAG)**:\n");
            for rel in &res.relations {
                let target_title = rel.target_title.as_deref().unwrap_or("Unknown");
                let target_status = rel
                    .target_status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                out.push_str(&format!(
                    "  - `{}` ➔ [{}] {} ({})\n",
                    rel.relation_type, rel.target_id, target_title, target_status
                ));
            }
        }
        out.push('\n');
    }

    out
}
