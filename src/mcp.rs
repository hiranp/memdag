use crate::models::{MemoryKind, MemoryStatus, RelationType, SessionLearning};
use crate::store::{
    DEFAULT_MAX_TOKENS, MemoryStore, RecordOptions, SearchOptions, format_provenance,
    format_relations, format_search_results_for_llm, format_search_results_with_budget,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant};

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

/// Surfaced via the MCP `initialize` response's `instructions` field. Clients that honor it
/// (Claude Code, Claude Desktop) inject this into the model's context, so the agent knows to
/// use these tools proactively instead of only reacting to explicit user requests.
const AGENT_INSTRUCTIONS: &str = "memdag is your durable project memory and lightweight task tracker. Use it proactively, without waiting to be asked:\n\
- At the start of a task, call search_memory with the task's key terms to recall relevant past decisions, invariants, and open blockers. Output is token-budgeted (max_tokens); if results were omitted, narrow the query or fetch one with get_memory.\n\
- When you make an architectural decision, discover an invariant, or hit a blocker worth remembering, call record_memory (use supersedes_id when replacing an earlier decision). Pass paths (repo-relative files the memory is about): memdag then marks it STALE in search results once those files change, so verify STALE memories against the code before relying on them.\n\
- Track work items as kind='task'. When something blocks a task, record it as kind='blocker' and call link_entities(blocker_id, task_id, 'blocks'); call list_ready to find claimable, unblocked work before starting on something, especially when other agents may be working on the same project.\n\
- When one memory depends on, blocks, or references another, call link_entities to keep the DAG connected.\n\
- Call resolve_memory when a task or blocker is done, with a short note on how it was resolved.\n\
- Before ending a session, call consolidate_session to promote durable learnings and purge scratch/ephemeral notes.\n\
- Bulk upkeep (search keywords for memories; checking STALE memories against the code; finding duplicate or conflicting memories) is work for a cheap model: call get_work, hand the batch to a low-cost subagent (e.g. Claude Haiku or Gemini Flash), then submit_work its JSON. The prompts expand_keywords, verify_stale and consolidate_memories run this end to end.";

/// A JSON array of strings, or a single comma/newline separated string.
fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .split([',', '\n'])
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

pub struct McpServer {
    store: RefCell<MemoryStore>,
    refresh: Option<AutoRefresh>,
}

/// Re-import the repo before read tools, so a long-lived session sees commits made after it
/// started. Throttled: a fresh index costs one `git rev-parse`, but not on every call.
struct AutoRefresh {
    repo: PathBuf,
    notes_dir: PathBuf,
    interval: Duration,
    last: Cell<Option<Instant>>,
}

/// Default minimum gap between two refresh checks in a running server.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Tools whose answers come from the index and should reflect the current checkout.
const READ_TOOLS: &[&str] = &[
    "search_memory",
    "search_vector",
    "get_memory",
    "list_memories",
    "list_ready",
    "get_work",
];

impl McpServer {
    pub fn new(store: MemoryStore) -> Self {
        Self {
            store: RefCell::new(store),
            refresh: None,
        }
    }

    /// Refresh the index from `repo` (at most once per `interval`) before read tools.
    pub fn with_auto_refresh(
        mut self,
        repo: PathBuf,
        notes_dir: PathBuf,
        interval: Duration,
    ) -> Self {
        self.refresh = Some(AutoRefresh {
            repo,
            notes_dir,
            interval,
            last: Cell::new(None),
        });
        self
    }

    fn maybe_refresh(&self, store: &mut MemoryStore) {
        let Some(r) = &self.refresh else { return };
        if r.last.get().is_some_and(|t| t.elapsed() < r.interval) {
            return;
        }
        crate::import::auto_refresh(store, &r.repo, &r.notes_dir);
        r.last.set(Some(Instant::now()));
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
                        },
                        "prompts": {
                            "listChanged": false
                        }
                    },
                    "serverInfo": {
                        "name": "memdag",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": AGENT_INSTRUCTIONS
                });
                Some(JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: req_id,
                    result: Some(result),
                    error: None,
                })
            }
            "ping" => Some(JsonRpcResponse {
                jsonrpc: "2.0",
                id: req_id,
                result: Some(json!({})),
                error: None,
            }),
            "prompts/list" => {
                let prompts: Vec<Value> = crate::work::HOST_PROMPTS
                    .iter()
                    .map(|p| {
                        json!({
                            "name": p.name,
                            "description": p.description,
                            "arguments": [{
                                "name": "rounds",
                                "description": format!("Maximum batches to process (default {})", crate::work::DEFAULT_ROUNDS),
                                "required": false
                            }]
                        })
                    })
                    .collect();
                Some(JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: req_id,
                    result: Some(json!({ "prompts": prompts })),
                    error: None,
                })
            }
            "prompts/get" => {
                let params = req.params.unwrap_or(json!({}));
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let Some(prompt) = crate::work::host_prompt(name) else {
                    // A notification (no id) never gets a reply, not even an error.
                    req_id.as_ref()?;
                    return Some(JsonRpcResponse {
                        jsonrpc: "2.0",
                        id: req_id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32602,
                            message: format!("Unknown prompt: {name}"),
                            data: None,
                        }),
                    });
                };
                // Arguments arrive as strings; ignore anything that isn't a small number.
                let rounds = params
                    .pointer("/arguments/rounds")
                    .and_then(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .or_else(|| v.as_u64().map(|n| n.to_string()))
                    })
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .filter(|n| (1..=50).contains(n))
                    .unwrap_or(crate::work::DEFAULT_ROUNDS);
                Some(JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: req_id,
                    result: Some(json!({
                        "description": prompt.description,
                        "messages": [{
                            "role": "user",
                            "content": {
                                "type": "text",
                                "text": crate::work::host_prompt_text(prompt.task, rounds)
                            }
                        }]
                    })),
                    error: None,
                })
            }
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
                // Per JSON-RPC 2.0: no id means notification, MUST NOT reply (incl. unknown ones).
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
        let mut store = self.store.borrow_mut();
        if READ_TOOLS.contains(&name) {
            self.maybe_refresh(&mut store);
        }

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
                let paths = string_list(args.get("paths"));
                let commit = args
                    .get("commit")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);

                let memory = store.record_memory(RecordOptions {
                    id,
                    kind,
                    title,
                    body,
                    tags,
                    supersedes_id,
                    session_id,
                    embedding: None,
                    paths,
                    commit,
                })?;

                let mut msg = format!(
                    "Successfully recorded memory [{}] (kind: {}, status: {})\nTitle: {}",
                    memory.id, memory.kind, memory.status, memory.title
                );
                if !memory.paths.is_empty() {
                    msg.push_str(&format!(
                        "\nCites: {} (as of {})",
                        memory.paths.join(", "),
                        memory
                            .commit
                            .as_deref()
                            .map_or("unknown commit", |c| &c[..c.len().min(7)])
                    ));
                }
                Ok(msg)
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
                // 0 means unlimited; absent means the default budget.
                let max_tokens = match args.get("max_tokens").and_then(|v| v.as_u64()) {
                    Some(0) => None,
                    Some(n) => Some(n as usize),
                    None => Some(DEFAULT_MAX_TOKENS),
                };

                let results = store.search_memory(SearchOptions {
                    query,
                    kind,
                    include_resolved,
                    limit,
                })?;

                Ok(format_search_results_with_budget(&results, max_tokens))
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
                        paths: string_list(item.get("paths")),
                        commit: item
                            .get("commit")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
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
                            "### [{}] {} (kind: {}, status: {})\n- **Body**: {}\n",
                            mem.id,
                            mem.title,
                            mem.kind,
                            mem.status,
                            mem.body.trim()
                        );
                        if let Some(tags) = mem.tags.as_deref().filter(|t| !t.trim().is_empty()) {
                            out.push_str(&format!("- **Tags**: {tags}\n"));
                        }
                        out.push_str(&format_provenance(&mem, &store.stale_paths(&mem)));
                        if store.only_in_other_checkout(&mem) {
                            out.push_str("- **Checkout**: only in another worktree; its branch may not be merged yet\n");
                        }
                        if !rels.is_empty() {
                            out.push_str("- **Relations (1-Hop DAG)**:\n");
                            out.push_str(&format_relations(&rels));
                        }
                        Ok(out)
                    }
                    None => Ok(format!("Memory with ID '{id}' not found.")),
                }
            }
            "list_memories" => {
                let status_filter = args
                    .get("status")
                    .and_then(|v| v.as_str())
                    .filter(|s| *s != "all")
                    .and_then(|s| MemoryStatus::from_str(s).ok());

                let kind_filter = args
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .and_then(|k| MemoryKind::from_str(k).ok());

                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

                let list = store.list_memories(status_filter, kind_filter, limit)?;
                if list.is_empty() {
                    return Ok("No memories found matching criteria.".to_string());
                }
                let mut out = format!("Found {} memories:\n\n", list.len());
                for m in list {
                    out.push_str(&format!(
                        "- `[{}]` ({}) **{}**: {}\n",
                        m.id, m.status, m.title, m.kind
                    ));
                }
                Ok(out)
            }
            "list_ready" => {
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let list = store.list_ready(limit)?;
                if list.is_empty() {
                    return Ok("No claimable tasks/blockers right now.".to_string());
                }
                let mut out = format!("Found {} claimable task(s)/blocker(s):\n\n", list.len());
                for m in list {
                    out.push_str(&format!(
                        "- `[{}]` ({}) **{}**: {}\n",
                        m.id, m.status, m.title, m.kind
                    ));
                }
                Ok(out)
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
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
                let embedding: Vec<f32> = match (args.get("embedding"), args.get("query")) {
                    (Some(Value::Array(vals)), _) => {
                        vals.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect()
                    }
                    (_, Some(Value::String(q))) if !q.trim().is_empty() => {
                        store.embed_text(q).map_err(|e| {
                            anyhow::anyhow!("'query' needs an embedder (set {}, see README) or a 384-float 'embedding': {e}", crate::embed::EMBED_CMD_ENV)
                        })?
                    }
                    _ => anyhow::bail!("Provide 'query' (text) or 'embedding' (384 floats)"),
                };
                if embedding.len() != crate::embed::DIM {
                    anyhow::bail!(
                        "embedding has {} dimensions; expected {}",
                        embedding.len(),
                        crate::embed::DIM
                    );
                }

                let results = store.search_vector(&embedding, limit)?;
                Ok(format_search_results_for_llm(&results))
            }
            "get_work" => {
                let task: crate::work::WorkTask = args
                    .get("task")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!("Missing 'task' (expand, verify or consolidate)")
                    })?
                    .parse()
                    .map_err(|e: String| anyhow::anyhow!(e))?;
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map_or(crate::work::DEFAULT_BATCH, |n| n as usize);
                let batch = crate::work::next_batch(&store, task, limit)?;
                Ok(serde_json::to_string_pretty(&batch)?)
            }
            "submit_work" => {
                let reply = match (args.get("results"), args.get("reply")) {
                    (Some(r @ Value::Array(_)), _) => json!({ "results": r }),
                    (Some(Value::Object(o)), _) => Value::Object(o.clone()),
                    // Some clients send arrays as JSON text.
                    (Some(Value::String(text)), _) => crate::work::parse_reply(text)?,
                    (_, Some(Value::String(text))) => crate::work::parse_reply(text)?,
                    _ => anyhow::bail!(
                        "Provide 'results' (the worker's results array) or 'reply' (its raw text)"
                    ),
                };
                let default = args
                    .get("task")
                    .and_then(|v| v.as_str())
                    .map(str::parse)
                    .transpose()
                    .map_err(|e: String| anyhow::anyhow!(e))?;
                let task = crate::work::reply_task(&reply, default)?;
                let report = crate::work::submit(&mut store, task, &reply)?;
                Ok(report.to_string())
            }
            unknown => anyhow::bail!("Unknown tool: '{}'", unknown),
        }
    }

    fn get_tool_definitions(&self) -> Vec<Value> {
        let mut tools = vec![
            json!({
                "name": "record_memory",
                "description": "Record a new memory node (decision, task, invariant, blocker, learning, ephemeral) with optional atomic DAG supersession. When supersedes_id is provided, automatically marks the target memory as 'superseded' and records a 'supersedes' DAG relation in an atomic transaction. Use kind='task' for work items and kind='blocker' for anything blocking one; link them with link_entities(blocker_id, task_id, 'blocks') and use list_ready to find unblocked work.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["decision", "task", "invariant", "blocker", "learning", "ephemeral"],
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
                        },
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Repo-relative files or directories this memory is about. memdag stamps the current git HEAD and later flags the memory STALE in search results when these paths change."
                        },
                        "commit": {
                            "type": "string",
                            "description": "Optional commit (hex) the memory is true as of. Defaults to HEAD when paths are given."
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
                "description": "Search memories using FTS5 BM25 keyword matching (fused with vector similarity when an embedder is configured) with automatic 1-hop DAG expansion for dependencies, blockers, and superseded items. Results whose cited files changed since they were recorded are marked STALE.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query terms or phrases."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["decision", "task", "invariant", "blocker", "learning", "ephemeral"],
                            "description": "Optional filter by memory kind."
                        },
                        "include_resolved": {
                            "type": "boolean",
                            "description": "Whether to include resolved or superseded memories (default: false)."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of matched nodes to return (default: 10)."
                        },
                        "max_tokens": {
                            "type": "integer",
                            "description": "Output budget in estimated tokens (default: 2000; 0 = unlimited). Results are packed in rank order and the reply says how many were omitted."
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
                                    "paths": {
                                        "type": "array",
                                        "items": { "type": "string" },
                                        "description": "Repo-relative files this learning is about (enables STALE flagging)."
                                    },
                                    "kind": {
                                        "type": "string",
                                        "enum": ["decision", "task", "invariant", "blocker", "learning"],
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
                            "enum": ["decision", "task", "invariant", "blocker", "learning", "ephemeral"],
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
                "name": "list_ready",
                "description": "List claimable tasks/blockers: status='active' with no incoming active 'blocks' edge. Use this instead of list_memories + get_memory-per-row to find unblocked work, especially when coordinating multiple agents on the same project.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of results to return (default: 20)."
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
        ];
        tools.push(json!({
            "name": "get_work",
            "description": "Get a batch of bulk memory upkeep for a cheap worker model: task 'expand' (write search keywords for memories that have none), 'verify' (check STALE memories against the code they cite) or 'consolidate' (judge pairs of similar memories: duplicate, conflict, related or distinct). Returns {instructions, items, remaining}. Hand instructions + items to a low-cost subagent (e.g. Claude Haiku, Gemini Flash), then pass its JSON to submit_work.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "enum": ["expand", "verify", "consolidate"] },
                    "limit": { "type": "integer", "description": "Items per batch (default 20, max 50)." }
                },
                "required": ["task"]
            }
        }));
        tools.push(json!({
            "name": "submit_work",
            "description": "Apply a worker's answers to a get_work batch. Each item is validated on its own; keywords are stored for search, 'valid' verdicts re-baseline a STALE memory, 'outdated' ones record the correction as a new memory superseding the old; consolidate results (keyed by 'pair') resolve duplicates, turn conflicts into tasks and link related memories.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "enum": ["expand", "verify", "consolidate"] },
                    "results": { "type": "array", "items": { "type": "object" }, "description": "The worker's results array." },
                    "reply": { "type": "string", "description": "Alternative to results: the worker's raw reply text (JSON is extracted)." }
                }
            }
        }));
        // Only advertise vector search when it can work: a model cannot produce a 384-float
        // embedding itself, so without an embedder the tool would be unusable.
        if self.store.borrow().has_embedder() {
            tools.push(            json!({
                "name": "search_vector",
                "description": "Search memories by semantic similarity (sqlite-vec KNN). Pass natural-language 'query' text; memdag embeds it with the configured embedder.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Text to find semantically similar memories for."
                        },
                        "embedding": {
                            "type": "array",
                            "items": { "type": "number" },
                            "description": "Alternative to 'query': a precomputed 384-float vector."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Number of nearest neighbors to return (default: 5)."
                        }
                    }
                }
            }));
        }
        tools
    }
}
