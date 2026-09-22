use crate::models::{MemoryKind, RelationType, SessionLearning};
use crate::store::{MemoryStore, RecordOptions, SearchOptions, format_search_results_for_llm};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

pub struct McpServer {
    store: Arc<Mutex<MemoryStore>>,
}

impl McpServer {
    pub fn new(store: MemoryStore) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
        }
    }

    pub fn run_stdio(&self) -> Result<()> {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        let reader = stdin.lock();

        eprintln!(
            "[memdag-mcp] Starting Stdio MCP Server (PID: {})",
            std::process::id()
        );

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[memdag-mcp] Stdin error: {}", e);
                    break;
                }
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
                Ok(req) => req,
                Err(err) => {
                    eprintln!("[memdag-mcp] JSON parse error: {}", err);
                    let err_resp = JsonRpcResponse {
                        jsonrpc: "2.0",
                        id: None,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32700,
                            message: format!("Parse error: {}", err),
                            data: None,
                        }),
                    };
                    let resp_str = serde_json::to_string(&err_resp)?;
                    writeln!(stdout, "{}", resp_str)?;
                    stdout.flush()?;
                    continue;
                }
            };

            let response = self.handle_request(request);
            if let Some(resp) = response {
                let resp_str = serde_json::to_string(&resp)?;
                writeln!(stdout, "{}", resp_str)?;
                stdout.flush()?;
            }
        }

        eprintln!("[memdag-mcp] Shutting down Stdio MCP Server");
        Ok(())
    }

    pub fn handle_request(&self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        let req_id = req.id.clone();

        match req.method.as_str() {
            "initialize" => {
                let result = json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {
                            "listChanged": false
                        }
                    },
                    "serverInfo": {
                        "name": "memdag",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                });
                Some(JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: req_id,
                    result: Some(result),
                    error: None,
                })
            }
            "notifications/initialized"
            | "initialized"
            | "notifications/cancelled"
            | "$/cancelRequest" => {
                // MCP notifications, no response required per JSON-RPC 2.0
                None
            }
            "ping" => Some(JsonRpcResponse {
                jsonrpc: "2.0",
                id: req_id,
                result: Some(json!({})),
                error: None,
            }),
            "tools/list" => {
                let tools = self.get_tool_definitions();
                Some(JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: req_id,
                    result: Some(json!({ "tools": tools })),
                    error: None,
                })
            }
            "tools/call" => {
                let params = req.params.unwrap_or(json!({}));
                let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

                match self.execute_tool(tool_name, arguments) {
                    Ok(tool_content) => {
                        let result = json!({
                            "content": [
                                {
                                    "type": "text",
                                    "text": tool_content
                                }
                            ],
                            "isError": false
                        });
                        Some(JsonRpcResponse {
                            jsonrpc: "2.0",
                            id: req_id,
                            result: Some(result),
                            error: None,
                        })
                    }
                    Err(err) => {
                        let result = json!({
                            "content": [
                                {
                                    "type": "text",
                                    "text": format!("Error executing tool '{}': {}", tool_name, err)
                                }
                            ],
                            "isError": true
                        });
                        Some(JsonRpcResponse {
                            jsonrpc: "2.0",
                            id: req_id,
                            result: Some(result),
                            error: None,
                        })
                    }
                }
            }
            unknown => {
                // Per JSON-RPC 2.0: If req_id is None, this is a notification, MUST NOT reply.
                req_id.as_ref()?;
                Some(JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: req_id,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32601,
                        message: format!("Method not found: {}", unknown),
                        data: None,
                    }),
                })
            }
        }
    }

    fn execute_tool(&self, name: &str, args: Value) -> Result<String> {
        let mut store = self.store.lock().unwrap();

        match name {
            "record_memory" => {
                let kind_str = args
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'kind' parameter"))?;
                let kind = MemoryKind::from_str(kind_str).map_err(|e| anyhow::anyhow!(e))?;

                let title = args
                    .get("title")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'title' parameter"))?
                    .to_string();

                let body = args
                    .get("body")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'body' parameter"))?
                    .to_string();

                let tags = args
                    .get("tags")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let supersedes_id = args
                    .get("supersedes_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let session_id = args
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let id = args.get("id").and_then(|v| v.as_str()).map(str::to_string);

                let memory = store.record_memory(RecordOptions {
                    id,
                    kind,
                    title,
                    body,
                    tags,
                    supersedes_id,
                    session_id,
                    embedding: None,
                })?;

                Ok(format!(
                    "Successfully recorded memory [{}] (kind: {}, status: {})\nTitle: {}",
                    memory.id, memory.kind, memory.status, memory.title
                ))
            }
            "link_entities" => {
                let source_id = args
                    .get("source_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'source_id'"))?;
                let target_id = args
                    .get("target_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'target_id'"))?;
                let rel_str = args
                    .get("relation_type")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'relation_type'"))?;
                let relation_type =
                    RelationType::from_str(rel_str).map_err(|e| anyhow::anyhow!(e))?;

                store.link_entities(source_id, target_id, relation_type)?;
                Ok(format!(
                    "Successfully linked: [{}] --({})--> [{}]",
                    source_id, relation_type, target_id
                ))
            }
            "search_memory" => {
                let query = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let kind = args
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .and_then(|k| MemoryKind::from_str(k).ok());
                let include_resolved = args
                    .get("include_resolved")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

                let results = store.search_memory(SearchOptions {
                    query,
                    kind,
                    include_resolved,
                    limit,
                })?;

                Ok(format_search_results_for_llm(&results))
            }
            "consolidate_session" => {
                let session_id = args
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'session_id'"))?;

                let learnings_val = args
                    .get("learnings")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'learnings' array"))?;

                let mut learnings = Vec::new();
                for item in learnings_val {
                    let title = item
                        .get("title")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("Learning missing 'title'"))?
                        .to_string();
                    let body = item
                        .get("body")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("Learning missing 'body'"))?
                        .to_string();
                    let tags = item
                        .get("tags")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let kind = item
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .and_then(|k| MemoryKind::from_str(k).ok())
                        .unwrap_or(MemoryKind::Decision);

                    learnings.push(SessionLearning {
                        title,
                        body,
                        tags,
                        kind,
                    });
                }

                let purge_ephemeral = args
                    .get("purge_ephemeral")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);

                let summary = store.consolidate_session(session_id, learnings, purge_ephemeral)?;

                Ok(format!(
                    "Session '{}' consolidated successfully:\n- Permanent learnings recorded: {}\n- Ephemeral items purged: {}\n- Ephemeral items archived: {}\n- Recorded IDs: {:?}",
                    summary.session_id,
                    summary.learnings_recorded,
                    summary.ephemeral_purged,
                    summary.ephemeral_archived,
                    summary.recorded_ids
                ))
            }
            "get_memory" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'id' parameter"))?;

                match store.get_memory(id)? {
                    Some((mem, rels)) => {
                        let mut out = format!(
                            "### [{}] {} (kind: {}, status: {})
- **Body**: {}
",
                            mem.id,
                            mem.title,
                            mem.kind,
                            mem.status,
                            mem.body.trim()
                        );
                        if let Some(tags) = mem.tags.as_deref().filter(|t| !t.trim().is_empty()) {
                            out.push_str(&format!("- **Tags**: {}\n", tags));
                        }
                        if !rels.is_empty() {
                            out.push_str(
                                "- **Relations (1-Hop DAG)**:
",
                            );
                            for r in &rels {
                                let title = r.target_title.as_deref().unwrap_or("Unknown");
                                let status = r
                                    .target_status
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| "unknown".to_string());
                                match r.direction {
                                    crate::models::EdgeDirection::Outgoing => {
                                        out.push_str(&format!(
                                            "  - `{}` ➔ [{}] {} ({})
",
                                            r.relation_type, r.target_id, title, status
                                        ));
                                    }
                                    crate::models::EdgeDirection::Incoming => {
                                        out.push_str(&format!(
                                            "  - `{}` ◄ [{}] {} ({})
",
                                            r.relation_type.inverse(),
                                            r.target_id,
                                            title,
                                            status
                                        ));
                                    }
                                }
                            }
                        }
                        Ok(out)
                    }
                    None => Ok(format!("Memory with ID '{}' not found.", id)),
                }
            }
            "list_memories" => {
                let status_filter = args.get("status").and_then(|v| v.as_str()).and_then(|s| {
                    if s == "all" {
                        None
                    } else {
                        crate::models::MemoryStatus::from_str(s).ok()
                    }
                });

                let kind_filter = args
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .and_then(|k| crate::models::MemoryKind::from_str(k).ok());

                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

                let list = store.list_memories(status_filter, kind_filter, limit)?;
                if list.is_empty() {
                    Ok("No memories found matching criteria.".to_string())
                } else {
                    let mut out = format!(
                        "Found {} memories:

",
                        list.len()
                    );
                    for m in list {
                        out.push_str(&format!(
                            "- `[{}]` ({}) **{}**: {}
",
                            m.id, m.status, m.title, m.kind
                        ));
                    }
                    Ok(out)
                }
            }
            "resolve_memory" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'id' parameter"))?;

                let note = args.get("note").and_then(|v| v.as_str());
                let updated = store.resolve_memory(id, note)?;
                Ok(format!(
                    "Successfully resolved memory [{}] '{}' (status: {})",
                    updated.id, updated.title, updated.status
                ))
            }
            "search_vector" => {
                let emb_vals = args
                    .get("embedding")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'embedding' float array"))?;

                let embedding: Vec<f32> = emb_vals
                    .iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect();

                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

                let results = store.search_vector(&embedding, limit)?;
                if results.is_empty() {
                    Ok("No vector matches found.".to_string())
                } else {
                    let mut out = format!(
                        "Found {} vector nearest neighbors:

",
                        results.len()
                    );
                    for (i, (mem, dist, rels)) in results.iter().enumerate() {
                        out.push_str(&format!(
                            "### {}. [{}] {} (distance: {:.4}, kind: {})
- **Body**: {}
",
                            i + 1,
                            mem.id,
                            mem.title,
                            dist,
                            mem.kind,
                            mem.body.trim()
                        ));
                        if !rels.is_empty() {
                            out.push_str(
                                "- **Relations**:
",
                            );
                            for r in rels {
                                let title = r.target_title.as_deref().unwrap_or("Unknown");
                                out.push_str(&format!(
                                    "  - `{}` [{}] {}
",
                                    r.relation_type, r.target_id, title
                                ));
                            }
                        }
                    }
                    Ok(out)
                }
            }
            unknown => anyhow::bail!("Unknown tool: '{}'", unknown),
        }
    }

    fn get_tool_definitions(&self) -> Vec<Value> {
        vec![
            json!({
                "name": "record_memory",
                "description": "Record a new memory node (decision, task, invariant, blocker, ephemeral) with optional atomic DAG supersession. When supersedes_id is provided, automatically marks the target memory as 'superseded' and records a 'supersedes' DAG relation in an atomic transaction.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["decision", "task", "invariant", "blocker", "ephemeral"],
                            "description": "The category of memory."
                        },
                        "title": {
                            "type": "string",
                            "description": "Short, clear summary title."
                        },
                        "body": {
                            "type": "string",
                            "description": "Detailed markdown explanation, reasoning, architecture decision, or instructions."
                        },
                        "tags": {
                            "type": "string",
                            "description": "Space/comma-separated synonyms, keywords, or error codes (e.g. 'sqlite wal timeout retry') to prevent FTS5 vocabulary mismatch."
                        },
                        "supersedes_id": {
                            "type": "string",
                            "description": "If provided, atomically marks this previous memory ID as 'superseded' and adds a 'supersedes' relation edge."
                        },
                        "session_id": {
                            "type": "string",
                            "description": "Optional identifier for the current agent work session."
                        },
                        "id": {
                            "type": "string",
                            "description": "Optional custom ID (e.g. 'DEC-2026-001'). Generated automatically if omitted."
                        }
                    },
                    "required": ["kind", "title", "body"]
                }
            }),
            json!({
                "name": "link_entities",
                "description": "Create a directed relation between two memories in the DAG (depends_on, blocks, references, supersedes).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {
                            "type": "string",
                            "description": "The ID of the source memory node."
                        },
                        "target_id": {
                            "type": "string",
                            "description": "The ID of the target memory node."
                        },
                        "relation_type": {
                            "type": "string",
                            "enum": ["depends_on", "blocks", "references", "supersedes"],
                            "description": "Directed relationship type."
                        }
                    },
                    "required": ["source_id", "target_id", "relation_type"]
                }
            }),
            json!({
                "name": "search_memory",
                "description": "Search memories using FTS5 BM25 keyword matching with automatic 1-hop DAG expansion for dependencies, blockers, and superseded items.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query terms or phrases."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["decision", "task", "invariant", "blocker", "ephemeral"],
                            "description": "Optional filter by memory kind."
                        },
                        "include_resolved": {
                            "type": "boolean",
                            "description": "Whether to include resolved or superseded memories (default: false)."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of matched nodes to return (default: 10)."
                        }
                    },
                    "required": ["query"]
                }
            }),
            json!({
                "name": "consolidate_session",
                "description": "Consolidate session learnings into permanent memories while purging or archiving ephemeral session observations.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "The session ID being consolidated."
                        },
                        "learnings": {
                            "type": "array",
                            "description": "List of permanent learnings or decisions derived from the session.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "title": { "type": "string" },
                                    "body": { "type": "string" },
                                    "tags": { "type": "string" },
                                    "kind": {
                                        "type": "string",
                                        "enum": ["decision", "task", "invariant", "blocker"],
                                        "default": "decision"
                                    }
                                },
                                "required": ["title", "body"]
                            }
                        },
                        "purge_ephemeral": {
                            "type": "boolean",
                            "description": "If true (default), completely deletes ephemeral items for this session. If false, archives them as resolved."
                        }
                    },
                    "required": ["session_id", "learnings"]
                }
            }),
            json!({
                "name": "get_memory",
                "description": "Retrieve a specific memory by ID along with its full body and bidirectional 1-hop DAG relations (outgoing and incoming dependencies, blockers, and supersessions).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "The memory ID to retrieve."
                        }
                    },
                    "required": ["id"]
                }
            }),
            json!({
                "name": "list_memories",
                "description": "List stored memories with optional status and kind filters.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["active", "superseded", "resolved", "ephemeral", "all"],
                            "description": "Filter by status (default: all active)."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["decision", "task", "invariant", "blocker", "ephemeral"],
                            "description": "Optional filter by memory kind."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of memories to return (default: 20)."
                        }
                    }
                }
            }),
            json!({
                "name": "resolve_memory",
                "description": "Mark an active task, blocker, or observation as resolved, with an optional resolution note.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "The memory ID to mark as resolved."
                        },
                        "note": {
                            "type": "string",
                            "description": "Optional note describing the resolution."
                        }
                    },
                    "required": ["id"]
                }
            }),
            json!({
                "name": "search_vector",
                "description": "Search memories by semantic similarity using sqlite-vec KNN on float[384] embeddings.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "embedding": {
                            "type": "array",
                            "items": { "type": "number" },
                            "description": "Dense vector float array of dimension 384."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Number of nearest neighbors to return (default: 5)."
                        }
                    },
                    "required": ["embedding"]
                }
            }),
        ]
    }
}
