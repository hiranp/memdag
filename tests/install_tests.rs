//! `memdag install` / `uninstall` across every harness, in throwaway home/project dirs.

use memdag::harness::Change;
use memdag::install::{
    Env, Location, Options, Parts, SKILL_MD, Target, detect, install, is_configured, print_config,
    uninstall,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn env(tmp: &tempfile::TempDir) -> Env {
    let e = Env::under(tmp.path());
    std::fs::create_dir_all(&e.project).unwrap();
    std::fs::create_dir_all(&e.home).unwrap();
    e
}

fn opts(parts: Parts) -> Options {
    Options {
        parts,
        model: None,
        exe: PathBuf::from("/opt/memdag/bin/memdag"),
        on_path: false,
        db: None,
        opencode_v1: false,
    }
}

/// Every file under `dir`, relative, sorted.
fn files(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, d: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().display().to_string());
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, dir, &mut v);
    v.sort();
    v
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&memdag::install::strip_jsonc(
        &std::fs::read_to_string(p).unwrap(),
    ))
    .unwrap()
}

#[test]
fn every_target_installs_idempotently_and_uninstalls_without_a_trace() {
    for loc in [Location::Project, Location::Global] {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(&tmp);
        let o = opts(Parts::ALL);
        for t in Target::ALL {
            let steps = install(t, &e, loc, &o).unwrap();
            assert!(
                steps.iter().all(|s| s.change != Change::Kept),
                "{t:?} {loc:?}: {steps:?}"
            );
            assert!(is_configured(t, &e, loc), "{t:?} {loc:?}");
        }
        for t in Target::ALL {
            let again = install(t, &e, loc, &o).unwrap();
            assert!(
                again.iter().all(|s| s.change == Change::Unchanged),
                "{t:?} {loc:?} second install: {again:?}"
            );
        }
        for t in Target::ALL {
            uninstall(t, &e, loc, Parts::ALL, &[]).unwrap();
            assert!(!is_configured(t, &e, loc), "{t:?} {loc:?}");
        }
        assert_eq!(files(tmp.path()), Vec::<String>::new(), "{loc:?} leftovers");
    }
}

#[test]
fn opencode_jsonc_keeps_comments_and_other_servers_and_gets_the_v2_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    let cfg = e.project.join("opencode.jsonc");
    let original = "{\n  // my servers\n  \"mcp\": {\n    \"other\": { \"type\": \"local\", \"command\": [\"x\"] }, // keep\n  },\n}\n";
    std::fs::write(&cfg, original).unwrap();

    install(
        Target::Opencode,
        &e,
        Location::Project,
        &opts(Parts {
            mcp: true,
            ..Parts::NONE
        }),
    )
    .unwrap();
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(
        text.contains("// my servers") && text.contains("// keep"),
        "{text}"
    );
    let v = read_json(&cfg);
    assert_eq!(v["mcp"]["other"]["command"][0], "x");
    let entry = &v["mcp"]["servers"]["memdag"];
    assert_eq!(entry["type"], "local");
    assert_eq!(
        entry["command"],
        serde_json::json!(["/opt/memdag/bin/memdag", "serve"])
    );
    assert_eq!(entry["codemode"], false);

    uninstall(Target::Opencode, &e, Location::Project, Parts::ALL, &[]).unwrap();
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(
        text.contains("// my servers") && text.contains("// keep"),
        "{text}"
    );
    let v = read_json(&cfg);
    assert!(
        v["mcp"].get("servers").is_none(),
        "empty container removed: {text}"
    );
    assert_eq!(v["mcp"]["other"]["command"][0], "x");
}

#[test]
fn an_opencode_v1_entry_is_migrated_and_v1_can_still_be_chosen() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    let cfg = e.project.join("opencode.json");
    std::fs::write(
        &cfg,
        r#"{"mcp": {"memdag": {"type": "local", "command": ["old", "serve"], "enabled": true}}}"#,
    )
    .unwrap();
    let steps = install(
        Target::Opencode,
        &e,
        Location::Project,
        &opts(Parts {
            mcp: true,
            ..Parts::NONE
        }),
    )
    .unwrap();
    assert!(
        steps
            .iter()
            .any(|s| s.note.as_deref().is_some_and(|n| n.contains("migrated")))
    );
    let v = read_json(&cfg);
    assert!(v["mcp"].get("memdag").is_none());
    assert!(v["mcp"]["servers"]["memdag"].is_object());

    let v1 = Options {
        opencode_v1: true,
        ..opts(Parts {
            mcp: true,
            ..Parts::NONE
        })
    };
    install(Target::Opencode, &e, Location::Project, &v1).unwrap();
    let v = read_json(&cfg);
    assert_eq!(v["mcp"]["memdag"]["enabled"], true);
    assert!(v["mcp"].get("servers").is_none());
}

#[test]
fn codex_toml_keeps_comments_and_round_trips_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    let cfg = e.codex_home.join("config.toml");
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    let original =
        "# my codex config\nmodel = \"o4\"\n\n[mcp_servers.other]\ncommand = \"x\" # keep\n";
    std::fs::write(&cfg, original).unwrap();
    let o = Options {
        db: Some(PathBuf::from("/data/m.db")),
        ..opts(Parts {
            mcp: true,
            ..Parts::NONE
        })
    };
    install(Target::Codex, &e, Location::Global, &o).unwrap();
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.starts_with(original), "{text}");
    let doc: toml_edit::DocumentMut = text.parse().unwrap();
    assert_eq!(
        doc["mcp_servers"]["memdag"]["command"].as_str(),
        Some("/opt/memdag/bin/memdag")
    );
    assert_eq!(
        doc["mcp_servers"]["memdag"]["env"]["MEMDAG_DB"].as_str(),
        Some("/data/m.db")
    );

    uninstall(Target::Codex, &e, Location::Global, Parts::ALL, &[]).unwrap();
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
}

#[test]
fn project_configs_say_memdag_when_it_is_on_path_and_global_ones_the_full_path() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    let o = Options {
        on_path: true,
        ..opts(Parts {
            mcp: true,
            ..Parts::NONE
        })
    };
    install(Target::ClaudeCode, &e, Location::Project, &o).unwrap();
    install(Target::ClaudeCode, &e, Location::Global, &o).unwrap();
    install(Target::Antigravity, &e, Location::Project, &o).unwrap();
    let project = read_json(&e.project.join(".mcp.json"));
    assert_eq!(project["mcpServers"]["memdag"]["command"], "memdag");
    assert_eq!(project["mcpServers"]["memdag"]["type"], "stdio");
    let global = read_json(&e.home.join(".claude.json"));
    assert_eq!(
        global["mcpServers"]["memdag"]["command"],
        "/opt/memdag/bin/memdag"
    );
    // Antigravity's app doesn't see the shell PATH, and rejects a `type` field.
    let ag = read_json(&e.project.join(".agents/mcp_config.json"));
    assert_eq!(
        ag["mcpServers"]["memdag"]["command"],
        "/opt/memdag/bin/memdag"
    );
    assert!(ag["mcpServers"]["memdag"].get("type").is_none());
}

#[test]
fn approvals_go_where_each_harness_reads_them() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    let o = opts(Parts::DEFAULT);
    install(Target::ClaudeCode, &e, Location::Project, &o).unwrap();
    let settings = read_json(&e.project.join(".claude/settings.json"));
    assert_eq!(
        settings["permissions"]["allow"],
        serde_json::json!(["mcp__memdag__get_work", "mcp__memdag__submit_work"])
    );
    install(Target::Kiro, &e, Location::Project, &o).unwrap();
    let kiro = read_json(&e.project.join(".kiro/settings/mcp.json"));
    assert_eq!(
        kiro["mcpServers"]["memdag"]["autoApprove"],
        serde_json::json!(["get_work", "submit_work"])
    );
    let cursor = install(Target::Cursor, &e, Location::Project, &o).unwrap();
    assert!(cursor.iter().any(|s| {
        s.note
            .as_deref()
            .is_some_and(|n| n.contains("its own settings"))
    }));

    let none = opts(Parts {
        permissions: false,
        ..Parts::DEFAULT
    });
    let tmp2 = tempfile::tempdir().unwrap();
    let e2 = env(&tmp2);
    install(Target::ClaudeCode, &e2, Location::Project, &none).unwrap();
    assert!(!e2.project.join(".claude/settings.json").exists());
    install(Target::Opencode, &e2, Location::Project, &none).unwrap();
    let worker =
        std::fs::read_to_string(e2.project.join(".opencode/agents/memdag-worker.md")).unwrap();
    assert!(!worker.contains("effect: allow"));
}

#[test]
fn workers_use_each_harness_format() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    let o = Options {
        model: Some("fast-model".into()),
        ..opts(Parts {
            worker: true,
            ..Parts::NONE
        })
    };
    for t in Target::ALL {
        install(t, &e, Location::Project, &o).unwrap();
    }
    let read = |p: &str| std::fs::read_to_string(e.project.join(p)).unwrap();
    assert!(read(".claude/agents/memdag-worker.md").contains("\nmodel: fast-model\n"));
    assert!(read(".cursor/agents/memdag-worker.md").contains("name: memdag-worker\n"));
    assert!(read(".gemini/agents/memdag-worker.md").contains("\nmodel: fast-model\n"));
    assert!(read(".github/agents/memdag-worker.agent.md").contains("model: ['fast-model']"));
    let codex: toml_edit::DocumentMut = read(".codex/agents/memdag-worker.toml").parse().unwrap();
    assert_eq!(codex["name"].as_str(), Some("memdag-worker"));
    assert_eq!(codex["model"].as_str(), Some("fast-model"));
    assert!(
        codex["developer_instructions"]
            .as_str()
            .unwrap()
            .contains("get_work")
    );
    assert!(!e.project.join(".kiro/agents").exists());
}

#[test]
fn shared_files_stay_while_another_harness_still_uses_them() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    let o = opts(Parts::DEFAULT);
    install(Target::Opencode, &e, Location::Project, &o).unwrap();
    install(Target::Codex, &e, Location::Project, &o).unwrap();
    let steps = uninstall(
        Target::Opencode,
        &e,
        Location::Project,
        Parts::ALL,
        &[Target::Codex],
    )
    .unwrap();
    assert!(
        steps.iter().filter(|s| s.change == Change::Kept).count() >= 2,
        "{steps:?}"
    );
    assert!(
        std::fs::read_to_string(e.project.join("AGENTS.md"))
            .unwrap()
            .contains("MEMDAG_START")
    );
    assert!(e.project.join(".agents/skills/memdag/SKILL.md").exists());
    uninstall(Target::Codex, &e, Location::Project, Parts::ALL, &[]).unwrap();
    assert!(!e.project.join("AGENTS.md").exists());
    assert!(!e.project.join(".agents/skills/memdag").exists());
}

#[test]
fn files_memdag_did_not_write_are_never_replaced_or_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    let rule = e.project.join(".cursor/rules/memdag.mdc");
    std::fs::create_dir_all(rule.parent().unwrap()).unwrap();
    std::fs::write(&rule, "my own rule\n").unwrap();
    assert!(
        install(
            Target::Cursor,
            &e,
            Location::Project,
            &opts(Parts {
                instructions: true,
                ..Parts::NONE
            })
        )
        .is_err()
    );
    assert_eq!(std::fs::read_to_string(&rule).unwrap(), "my own rule\n");

    let skill = e.project.join(".claude/skills/memdag");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "my skill\n").unwrap();
    let steps = install(
        Target::ClaudeCode,
        &e,
        Location::Project,
        &opts(Parts {
            skill: true,
            ..Parts::NONE
        }),
    )
    .unwrap();
    assert_eq!(steps[0].change, Change::Kept);
    uninstall(Target::ClaudeCode, &e, Location::Project, Parts::ALL, &[]).unwrap();
    assert_eq!(
        std::fs::read_to_string(skill.join("SKILL.md")).unwrap(),
        "my skill\n"
    );

    let agent = e.project.join(".claude/agents/memdag-worker.md");
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    std::fs::write(&agent, "---\nname: memdag-worker\n---\nmine\n").unwrap();
    assert!(
        install(
            Target::ClaudeCode,
            &e,
            Location::Project,
            &opts(Parts {
                worker: true,
                ..Parts::NONE
            })
        )
        .is_err()
    );
    uninstall(Target::ClaudeCode, &e, Location::Project, Parts::ALL, &[]).unwrap();
    assert!(agent.exists());
}

#[test]
fn an_unparseable_config_stops_the_install_before_anything_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    std::fs::write(e.project.join(".mcp.json"), "{ not json").unwrap();
    let err = install(Target::ClaudeCode, &e, Location::Project, &opts(Parts::ALL)).unwrap_err();
    assert!(format!("{err:#}").contains("left unchanged"), "{err:#}");
    assert_eq!(files(&e.project), vec![".mcp.json"]);
}

#[test]
fn print_config_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    for t in Target::ALL {
        let text = print_config(t, &e, Location::Global, &opts(Parts::ALL)).unwrap();
        assert!(text.contains("memdag"), "{t:?}");
    }
    assert_eq!(files(tmp.path()), Vec::<String>::new());
}

#[test]
fn detection_follows_each_harness_config_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    assert!(Target::ALL.iter().all(|t| !detect(*t, &e)));
    std::fs::create_dir_all(e.home.join(".cursor")).unwrap();
    std::fs::create_dir_all(e.project.join(".kiro")).unwrap();
    let found: Vec<Target> = Target::ALL.into_iter().filter(|t| detect(*t, &e)).collect();
    assert_eq!(found, vec![Target::Cursor, Target::Kiro]);
}

#[test]
fn the_session_hook_is_added_once_and_removed_with_its_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    std::fs::create_dir_all(e.project.join(".claude")).unwrap();
    std::fs::write(
        e.project.join(".claude/settings.json"),
        r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "other"}]}]}}"#,
    )
    .unwrap();
    let o = opts(Parts {
        hooks: true,
        ..Parts::NONE
    });
    install(Target::ClaudeCode, &e, Location::Project, &o).unwrap();
    let again = install(Target::ClaudeCode, &e, Location::Project, &o).unwrap();
    assert_eq!(again[0].change, Change::Unchanged);
    let v = read_json(&e.project.join(".claude/settings.json"));
    let groups = v["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(
        groups[1]["hooks"][0]["command"],
        "/opt/memdag/bin/memdag hook session-start"
    );

    uninstall(Target::ClaudeCode, &e, Location::Project, Parts::ALL, &[]).unwrap();
    let v = read_json(&e.project.join(".claude/settings.json"));
    assert_eq!(
        v["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "other"
    );
}

#[test]
fn the_installed_skill_is_the_shipped_one() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(&tmp);
    install(
        Target::Gemini,
        &e,
        Location::Global,
        &opts(Parts {
            skill: true,
            ..Parts::NONE
        }),
    )
    .unwrap();
    let skill = std::fs::read_to_string(e.home.join(".agents/skills/memdag/SKILL.md")).unwrap();
    assert_eq!(skill, SKILL_MD);
    assert!(skill.starts_with("---\nname: memdag\n"));
}
