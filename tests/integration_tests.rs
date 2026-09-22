use memdag::db::open_in_memory;
use memdag::models::{MemoryKind, MemoryStatus, RelationType, SessionLearning};
use memdag::store::{MemoryStore, RecordOptions, SearchOptions};
use std::process::Command;

#[test]
fn test_database_initialization_pragmas() {
    let conn = open_in_memory().expect("open in memory db");
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode;", [], |r| r.get(0))
        .expect("journal_mode");
    // in-memory sqlite uses memory or wal
    assert!(!journal_mode.is_empty());

    let fk: i32 = conn
        .query_row("PRAGMA foreign_keys;", [], |r| r.get(0))
        .expect("foreign_keys");
    assert_eq!(fk, 1);
}

#[test]
fn test_record_and_atomic_supersession() {
    let conn = open_in_memory().expect("open db");
    let mut store = MemoryStore::new(conn);

    // 1. Record original decision
    let dec1 = store
        .record_memory(RecordOptions {
            id: Some("DEC-001".to_string()),
            kind: MemoryKind::Decision,
            title: "Use Flat Markdown Memory".to_string(),
            body: "Store context in a single CLAUDE.md file.".to_string(),
            tags: Some("markdown flat storage".to_string()),
            supersedes_id: None,
            session_id: Some("sess-1".to_string()),
            embedding: None,
        })
        .expect("record dec1");

    assert_eq!(dec1.id, "DEC-001");
    assert_eq!(dec1.status, MemoryStatus::Active);

    // 2. Record new decision superseding DEC-001
    let dec2 = store
        .record_memory(RecordOptions {
            id: Some("DEC-002".to_string()),
            kind: MemoryKind::Decision,
            title: "Migrate to SQLite WAL + FTS5 Relational DAG".to_string(),
            body:
                "Flat markdown leads to context bloat and zombie context. Use SQLite DAG instead."
                    .to_string(),
            tags: Some("sqlite wal fts5 dag".to_string()),
            supersedes_id: Some("DEC-001".to_string()),
            session_id: Some("sess-2".to_string()),
            embedding: None,
        })
        .expect("record dec2");

    assert_eq!(dec2.id, "DEC-002");
    assert_eq!(dec2.status, MemoryStatus::Active);

    // 3. Verify DEC-001 was automatically marked as superseded
    let (old_mem, _) = store
        .get_memory("DEC-001")
        .expect("get dec1")
        .expect("found");
    assert_eq!(old_mem.status, MemoryStatus::Superseded);

    // 4. Verify supersedes relation edge exists in DEC-002
    let (new_mem, rels) = store
        .get_memory("DEC-002")
        .expect("get dec2")
        .expect("found");
    assert_eq!(new_mem.status, MemoryStatus::Active);
    assert_eq!(rels.len(), 1);
    assert_eq!(rels[0].relation_type, RelationType::Supersedes);
    assert_eq!(rels[0].target_id, "DEC-001");
    assert_eq!(rels[0].target_status, Some(MemoryStatus::Superseded));
}

#[test]
fn test_fts5_search_and_1_hop_dag_expansion() {
    let conn = open_in_memory().expect("open db");
    let mut store = MemoryStore::new(conn);

    // Record decision, invariant, and blocker task
    store
        .record_memory(RecordOptions {
            id: Some("INV-001".to_string()),
            kind: MemoryKind::Invariant,
            title: "Zero Daemon Requirement".to_string(),
            body: "Never run external background services; everything must be embedded in-process."
                .to_string(),
            tags: Some("daemon zero-service embedded".to_string()),
            supersedes_id: None,
            session_id: None,
            embedding: None,
        })
        .expect("inv");

    store
        .record_memory(RecordOptions {
            id: Some("DEC-010".to_string()),
            kind: MemoryKind::Decision,
            title: "Concurrency with SQLite WAL Mode".to_string(),
            body: "Configure PRAGMA journal_mode = WAL and busy_timeout = 5000 to allow multi-reader concurrency.".to_string(),
            tags: Some("sqlite wal concurrency timeout".to_string()),
            supersedes_id: None,
            session_id: None,
            embedding: None,
        })
        .expect("dec");

    store
        .record_memory(RecordOptions {
            id: Some("TSK-050".to_string()),
            kind: MemoryKind::Task,
            title: "Benchmark Multi-Process Lock Contention".to_string(),
            body:
                "Run 100 concurrent subagents hitting BEGIN IMMEDIATE to test sqlite lock handling."
                    .to_string(),
            tags: Some("benchmark lock wal contention".to_string()),
            supersedes_id: None,
            session_id: None,
            embedding: None,
        })
        .expect("tsk");

    // Link relations: DEC-010 depends on INV-001; TSK-050 blocks DEC-010
    store
        .link_entities("DEC-010", "INV-001", RelationType::DependsOn)
        .expect("link1");
    store
        .link_entities("DEC-010", "TSK-050", RelationType::Blocks)
        .expect("link2");

    // Search for "concurrency wal"
    let results = store
        .search_memory(SearchOptions {
            query: "concurrency wal".to_string(),
            kind: None,
            include_resolved: false,
            limit: 5,
        })
        .expect("search");

    assert!(!results.is_empty());
    let dec_res = results
        .iter()
        .find(|r| r.memory.id == "DEC-010")
        .expect("DEC-010 found");
    assert_eq!(dec_res.relations.len(), 2);

    let dep = dec_res
        .relations
        .iter()
        .find(|r| r.target_id == "INV-001")
        .expect("dep found");
    assert_eq!(dep.relation_type, RelationType::DependsOn);

    let blk = dec_res
        .relations
        .iter()
        .find(|r| r.target_id == "TSK-050")
        .expect("blk found");
    assert_eq!(blk.relation_type, RelationType::Blocks);
}

#[test]
fn test_session_consolidation_and_ephemeral_purge() {
    let conn = open_in_memory().expect("open db");
    let mut store = MemoryStore::new(conn);

    // Record an ephemeral debug observation
    store
        .record_memory(RecordOptions {
            id: Some("EPH-001".to_string()),
            kind: MemoryKind::Ephemeral,
            title: "Temporary test port 8080 timeout".to_string(),
            body: "Connection failed during test run on port 8080.".to_string(),
            tags: Some("test port ephemeral".to_string()),
            supersedes_id: None,
            session_id: Some("session-abc".to_string()),
            embedding: None,
        })
        .expect("record ephemeral");

    // Verify it exists in db
    let eph = store.get_memory("EPH-001").expect("get").expect("found");
    assert_eq!(eph.0.status, MemoryStatus::Ephemeral);

    // Consolidate session
    let res = store
        .consolidate_session(
            "session-abc",
            vec![SessionLearning {
                title: "Always use dynamic port allocation for mock servers".to_string(),
                body: "Hardcoded ports cause flaky parallel test runs.".to_string(),
                tags: Some("testing port flakiness".to_string()),
                kind: MemoryKind::Invariant,
            }],
            true, // purge ephemeral
        )
        .expect("consolidate");

    assert_eq!(res.learnings_recorded, 1);
    assert_eq!(res.ephemeral_purged, 1);

    // Verify ephemeral entry was purged
    let eph_after = store.get_memory("EPH-001").expect("get");
    assert!(eph_after.is_none());

    // Verify new learning is active
    let (learning, _) = store
        .get_memory(&res.recorded_ids[0])
        .expect("get")
        .expect("found");
    assert_eq!(learning.status, MemoryStatus::Active);
    assert_eq!(learning.kind, MemoryKind::Invariant);
}

#[test]
fn test_sqlite_vec_extension_operations() {
    let conn = open_in_memory().expect("open db");

    // Check vec_version()
    let version: String = conn
        .query_row("SELECT vec_version()", [], |r| r.get(0))
        .expect("vec_version");
    assert!(!version.is_empty());

    // Insert into memories_vec and run vector distance calculation
    let mut store = MemoryStore::new(conn);
    let embedding = vec![0.1f32; 384];

    let mem = store
        .record_memory(RecordOptions {
            id: Some("VEC-001".to_string()),
            kind: MemoryKind::Decision,
            title: "Vector Memory Test".to_string(),
            body: "Testing sqlite-vec embedded integration.".to_string(),
            tags: Some("vector test".to_string()),
            supersedes_id: None,
            session_id: None,
            embedding: Some(embedding.clone()),
        })
        .expect("record with embedding");

    assert_eq!(mem.id, "VEC-001");

    // Query vec table directly
    let count: i64 = store
        .connection()
        .query_row(
            "SELECT count(*) FROM memories_vec WHERE id = 'VEC-001'",
            [],
            |r| r.get(0),
        )
        .expect("count vec");
    assert_eq!(count, 1);

    // Test KNN vector distance search with sqlite-vec
    let emb_str = format!(
        "[{}]",
        embedding
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let (match_id, distance): (String, f64) = store
        .connection()
        .query_row(
            "SELECT id, distance FROM memories_vec WHERE embedding MATCH ?1 AND k = 1",
            rusqlite::params![emb_str],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("knn match query");

    assert_eq!(match_id, "VEC-001");
    assert!(distance < 0.001);
}

#[test]
fn test_mcp_tool_execution() {
    let conn = open_in_memory().expect("open db");
    let store = MemoryStore::new(conn);
    let mcp = memdag::McpServer::new(store);

    // Call record_memory tool
    let record_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "record_memory",
            "arguments": {
                "kind": "decision",
                "title": "Adopt SQLite WAL + FTS5",
                "body": "Replaces flat CLAUDE.md with a relational DAG.",
                "tags": "sqlite wal fts5 dag",
                "id": "DEC-TEST-001"
            }
        }
    });

    let resp = mcp
        .handle_request(serde_json::from_value(record_req).unwrap())
        .expect("response");
    assert!(resp.error.is_none());
    let res_val = resp.result.unwrap();
    let text = res_val["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Successfully recorded memory [DEC-TEST-001]"));

    // Call search_memory tool
    let search_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "search_memory",
            "arguments": {
                "query": "sqlite wal"
            }
        }
    });

    let resp2 = mcp
        .handle_request(serde_json::from_value(search_req).unwrap())
        .expect("response");
    let res2_val = resp2.result.unwrap();
    let text2 = res2_val["content"][0]["text"].as_str().unwrap();
    assert!(text2.contains("DEC-TEST-001"));
}

#[test]
fn test_mcp_notifications_silence() {
    let conn = open_in_memory().expect("open db");
    let store = MemoryStore::new(conn);
    let mcp = memdag::McpServer::new(store);

    // 1. Standard MCP client initialized notification (no id)
    let init_notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialized"
    });
    let resp = mcp.handle_request(serde_json::from_value(init_notif).unwrap());
    assert!(resp.is_none(), "Notifications MUST NOT receive a response");

    // 2. notifications/initialized
    let init_notif2 = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let resp2 = mcp.handle_request(serde_json::from_value(init_notif2).unwrap());
    assert!(resp2.is_none());

    // 3. Unknown notification with id: null should also never produce an error response
    let unknown_notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/unknown_custom"
    });
    let resp3 = mcp.handle_request(serde_json::from_value(unknown_notif).unwrap());
    assert!(
        resp3.is_none(),
        "Unknown notifications must be silently ignored without error responses"
    );
}

#[test]
fn test_bidirectional_dag_relations() {
    let conn = open_in_memory().expect("open db");
    let mut store = MemoryStore::new(conn);

    store
        .record_memory(RecordOptions {
            id: Some("DEC-A".to_string()),
            kind: MemoryKind::Decision,
            title: "Decision A".to_string(),
            body: "Body A".to_string(),
            tags: None,
            supersedes_id: None,
            session_id: None,
            embedding: None,
        })
        .expect("rec A");

    store
        .record_memory(RecordOptions {
            id: Some("TSK-B".to_string()),
            kind: MemoryKind::Task,
            title: "Task B".to_string(),
            body: "Body B".to_string(),
            tags: None,
            supersedes_id: None,
            session_id: None,
            embedding: None,
        })
        .expect("rec B");

    // Link: DEC-A blocks TSK-B
    store
        .link_entities("DEC-A", "TSK-B", RelationType::Blocks)
        .expect("link");

    // 1. Inspect source (DEC-A): Outgoing relation
    let (dec_a, rels_a) = store.get_memory("DEC-A").expect("get A").expect("found");
    assert_eq!(dec_a.id, "DEC-A");
    assert_eq!(rels_a.len(), 1);
    assert_eq!(rels_a[0].target_id, "TSK-B");
    assert_eq!(rels_a[0].relation_type, RelationType::Blocks);
    assert_eq!(rels_a[0].direction, memdag::models::EdgeDirection::Outgoing);

    // 2. Inspect target (TSK-B): Incoming relation (bidirectional awareness!)
    let (tsk_b, rels_b) = store.get_memory("TSK-B").expect("get B").expect("found");
    assert_eq!(tsk_b.id, "TSK-B");
    assert_eq!(rels_b.len(), 1);
    assert_eq!(rels_b[0].target_id, "DEC-A");
    assert_eq!(rels_b[0].relation_type, RelationType::Blocks);
    assert_eq!(rels_b[0].direction, memdag::models::EdgeDirection::Incoming);
}

#[test]
fn test_mcp_expanded_tools() {
    let conn = open_in_memory().expect("open db");
    let mut store = MemoryStore::new(conn);

    // Record an active task
    store
        .record_memory(RecordOptions {
            id: Some("TSK-001".to_string()),
            kind: MemoryKind::Task,
            title: "Investigate Memory Leaks".to_string(),
            body: "Check long-lived Tokio task spawned handles.".to_string(),
            tags: Some("tokio leak memory".to_string()),
            supersedes_id: None,
            session_id: None,
            embedding: Some(vec![0.2f32; 384]),
        })
        .expect("rec");

    let mcp = memdag::McpServer::new(store);

    // 1. Test get_memory tool
    let get_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "get_memory",
            "arguments": { "id": "TSK-001" }
        }
    });
    let resp = mcp
        .handle_request(serde_json::from_value(get_req).unwrap())
        .expect("resp");
    let text = resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.contains("TSK-001"));
    assert!(text.contains("Investigate Memory Leaks"));

    // 2. Test list_memories tool
    let list_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "tools/call",
        "params": {
            "name": "list_memories",
            "arguments": { "status": "active" }
        }
    });
    let resp2 = mcp
        .handle_request(serde_json::from_value(list_req).unwrap())
        .expect("resp");
    let text2 = resp2.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text2.contains("TSK-001"));

    // 3. Test resolve_memory tool
    let resolve_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 12,
        "method": "tools/call",
        "params": {
            "name": "resolve_memory",
            "arguments": {
                "id": "TSK-001",
                "note": "Fixed by storing JoinSet and aborting on drop."
            }
        }
    });
    let resp3 = mcp
        .handle_request(serde_json::from_value(resolve_req).unwrap())
        .expect("resp");
    let text3 = resp3.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text3.contains("Successfully resolved memory [TSK-001]"));

    // 4. Test search_vector tool
    let vec_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 13,
        "method": "tools/call",
        "params": {
            "name": "search_vector",
            "arguments": {
                "embedding": vec![0.2f32; 384],
                "limit": 1
            }
        }
    });
    let resp4 = mcp
        .handle_request(serde_json::from_value(vec_req).unwrap())
        .expect("resp");
    let text4 = resp4.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text4.contains("TSK-001"));
}

#[test]
fn test_empty_query_fallback() {
    let conn = open_in_memory().expect("open db");
    let mut store = MemoryStore::new(conn);

    store
        .record_memory(RecordOptions {
            id: Some("DEC-FALLBACK".to_string()),
            kind: MemoryKind::Decision,
            title: "Fallback Test".to_string(),
            body: "Should be returned when query is empty".to_string(),
            tags: None,
            supersedes_id: None,
            session_id: None,
            embedding: None,
        })
        .expect("rec");

    // Empty query
    let results = store
        .search_memory(SearchOptions {
            query: "".to_string(),
            kind: None,
            include_resolved: false,
            limit: 5,
        })
        .expect("search empty");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].memory.id, "DEC-FALLBACK");
}

#[test]
fn test_record_memory_rejects_secret_patterns() {
    let conn = open_in_memory().expect("open db");
    let mut store = MemoryStore::new(conn);

    let err = store
        .record_memory(RecordOptions {
            id: Some("DEC-SECRET".to_string()),
            kind: MemoryKind::Decision,
            title: "Rotate keys".to_string(),
            body: "New key is sk-abcdef1234567890".to_string(),
            tags: None,
            supersedes_id: None,
            session_id: None,
            embedding: None,
        })
        .expect_err("should refuse to record a memory containing a secret pattern");
    assert!(err.to_string().contains("secret pattern"));

    // Nothing should have been written.
    assert!(
        store
            .get_memory("DEC-SECRET")
            .expect("get")
            .is_none()
    );
}

#[test]
fn test_purge_expired_ephemeral() {
    let conn = open_in_memory().expect("open db");
    let mut store = MemoryStore::new(conn);

    store
        .record_memory(RecordOptions {
            id: Some("EPH-OLD".to_string()),
            kind: MemoryKind::Ephemeral,
            title: "Stale scratch note".to_string(),
            body: "Left behind by a crashed session.".to_string(),
            tags: None,
            supersedes_id: None,
            session_id: Some("crashed-session".to_string()),
            embedding: None,
        })
        .expect("record ephemeral");

    // Backdate created_at so it looks older than the TTL.
    store
        .connection()
        .execute(
            "UPDATE memories SET created_at = datetime('now', '-2 days') WHERE id = 'EPH-OLD'",
            [],
        )
        .expect("backdate");

    // A 1-hour TTL should purge it.
    let purged = store
        .purge_expired_ephemeral(60 * 60)
        .expect("purge expired");
    assert_eq!(purged, 1);
    assert!(store.get_memory("EPH-OLD").expect("get").is_none());
}

#[test]
fn test_purge_expired_ephemeral_keeps_recent() {
    let conn = open_in_memory().expect("open db");
    let mut store = MemoryStore::new(conn);

    store
        .record_memory(RecordOptions {
            id: Some("EPH-FRESH".to_string()),
            kind: MemoryKind::Ephemeral,
            title: "Fresh scratch note".to_string(),
            body: "Still within TTL.".to_string(),
            tags: None,
            supersedes_id: None,
            session_id: Some("live-session".to_string()),
            embedding: None,
        })
        .expect("record ephemeral");

    let purged = store
        .purge_expired_ephemeral(24 * 60 * 60)
        .expect("purge expired");
    assert_eq!(purged, 0);
    assert!(store.get_memory("EPH-FRESH").expect("get").is_some());
}

#[test]
fn test_mcp_install_and_uninstall_cli() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("mcp-config.json");

    let bin = env!("CARGO_BIN_EXE_memdag");

    let install = Command::new(bin)
        .args(["mcp", "install", "--path"])
        .arg(&cfg_path)
        .output()
        .expect("run install");
    assert!(install.status.success());

    let raw = std::fs::read_to_string(&cfg_path).expect("read config");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse config");
    assert_eq!(value["mcpServers"]["memdag"]["args"][0], "serve");
    assert!(value["mcpServers"]["memdag"]["command"].is_string());

    let uninstall = Command::new(bin)
        .args(["mcp", "uninstall", "--path"])
        .arg(&cfg_path)
        .output()
        .expect("run uninstall");
    assert!(uninstall.status.success());

    let raw_after = std::fs::read_to_string(&cfg_path).expect("read config");
    let value_after: serde_json::Value = serde_json::from_str(&raw_after).expect("parse config");
    assert!(value_after["mcpServers"].get("memdag").is_none());
}
