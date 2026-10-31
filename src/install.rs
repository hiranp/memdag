//! `memdag install` / `memdag uninstall`: wire memdag into agent harnesses.
//!
//! One target per harness. For each, memdag can write:
//!
//! - the **MCP server entry** in the harness's config (JSON/JSONC edited in place, so comments
//!   and formatting survive; TOML for Codex);
//! - the **`memdag-worker` subagent** on a cheap model, where the harness has subagent files
//!   with a per-agent model;
//! - **instructions**: a marked block in its always-on instructions file, or a rule/steering
//!   file of its own;
//! - the **memdag skill** (`SKILL.md`) in a skills folder it reads;
//! - **approvals** for the worker's two tools, where a file can grant them;
//! - optionally a Claude Code **SessionStart hook** that mentions waiting upkeep.
//!
//! Locations come from each harness's own documentation (see `docs/adr/0006-*.md`). Every
//! write is skipped when nothing would change, refuses a file it can't parse, and uninstall
//! removes only what memdag wrote.

use crate::harness::{self, Change, Harness};
use anyhow::{Context, Result, bail};
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The memdag skill, shipped inside the binary so an install never depends on a checkout.
pub const SKILL_MD: &str = include_str!("../.agents/skills/memdag/SKILL.md");
/// Marks a skill folder memdag installed (uninstall removes only those).
const SKILL_STAMP: &str = ".memdag-skill";
/// The MCP server's name in every config.
pub const SERVER: &str = "memdag";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    ClaudeCode,
    Opencode,
    Codex,
    Cursor,
    Gemini,
    Vscode,
    Kiro,
    Antigravity,
}

impl Target {
    pub const ALL: [Target; 8] = [
        Target::ClaudeCode,
        Target::Opencode,
        Target::Codex,
        Target::Cursor,
        Target::Gemini,
        Target::Vscode,
        Target::Kiro,
        Target::Antigravity,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Target::ClaudeCode => "claude-code",
            Target::Opencode => "opencode",
            Target::Codex => "codex",
            Target::Cursor => "cursor",
            Target::Gemini => "gemini",
            Target::Vscode => "vscode",
            Target::Kiro => "kiro",
            Target::Antigravity => "antigravity",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Target::ClaudeCode => "Claude Code",
            Target::Opencode => "OpenCode",
            Target::Codex => "Codex CLI",
            Target::Cursor => "Cursor",
            Target::Gemini => "Gemini CLI",
            Target::Vscode => "GitHub Copilot (VS Code)",
            Target::Kiro => "Kiro",
            Target::Antigravity => "Antigravity",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "claude-code" | "claude" => Target::ClaudeCode,
            "opencode" => Target::Opencode,
            "codex" => Target::Codex,
            "cursor" => Target::Cursor,
            "gemini" | "gemini-cli" => Target::Gemini,
            "vscode" | "copilot" | "vs-code" => Target::Vscode,
            "kiro" => Target::Kiro,
            "antigravity" => Target::Antigravity,
            _ => return None,
        })
    }

    fn harness(self) -> Option<Harness> {
        match self {
            Target::ClaudeCode => Some(Harness::ClaudeCode),
            Target::Opencode => Some(Harness::Opencode),
            Target::Codex => Some(Harness::Codex),
            Target::Cursor => Some(Harness::Cursor),
            Target::Gemini => Some(Harness::Gemini),
            Target::Vscode => Some(Harness::Copilot),
            // No documented subagent files with a per-agent model.
            Target::Kiro | Target::Antigravity => None,
        }
    }
}

/// Parse `auto`, `all` or a comma-separated list of target ids.
pub fn parse_targets(spec: &str, env: &Env, loc: Location) -> Result<Vec<Target>> {
    match spec.trim() {
        "auto" | "" => Ok(Target::ALL
            .into_iter()
            .filter(|t| detect(*t, env, loc))
            .collect()),
        "all" => Ok(Target::ALL.to_vec()),
        list => list
            .split(',')
            .map(|id| {
                Target::from_id(id).with_context(|| {
                    format!(
                        "unknown target '{id}' (expected auto, all, or some of: {})",
                        Target::ALL.map(Target::id).join(", ")
                    )
                })
            })
            .collect(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// Files in the current project (shareable through git).
    Project,
    /// The user's own config, for every project.
    Global,
}

/// Where things live. Built from the real environment, or set explicitly in tests.
#[derive(Debug, Clone)]
pub struct Env {
    pub project: PathBuf,
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME`, or `~/.config` (OpenCode uses it on every OS).
    pub config_home: PathBuf,
    /// `$CODEX_HOME`, or `~/.codex`.
    pub codex_home: PathBuf,
    /// VS Code's per-user settings directory.
    pub vscode_user: PathBuf,
}

impl Env {
    pub fn current(project: PathBuf) -> Result<Self> {
        let home = directories::BaseDirs::new()
            .context("could not determine the home directory")?
            .home_dir()
            .to_path_buf();
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let config_home = var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let vscode_user = if cfg!(target_os = "macos") {
            home.join("Library/Application Support/Code/User")
        } else if cfg!(windows) {
            var("APPDATA")
                .unwrap_or_else(|| home.join("AppData/Roaming"))
                .join("Code/User")
        } else {
            config_home.join("Code/User")
        };
        Ok(Self {
            project,
            codex_home: var("CODEX_HOME").unwrap_or_else(|| home.join(".codex")),
            config_home,
            vscode_user,
            home,
        })
    }

    /// Everything under one directory: `<root>/project` and `<root>/home`.
    pub fn under(root: &Path) -> Self {
        let home = root.join("home");
        Self {
            project: root.join("project"),
            config_home: home.join(".config"),
            codex_home: home.join(".codex"),
            vscode_user: home.join(".config/Code/User"),
            home,
        }
    }
}

/// Which pieces to write or remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parts {
    pub mcp: bool,
    pub worker: bool,
    pub instructions: bool,
    pub skill: bool,
    pub permissions: bool,
    /// Opt-in: a Claude Code SessionStart hook.
    pub hooks: bool,
}

impl Parts {
    /// What `memdag install` writes by default (everything but hooks).
    pub const DEFAULT: Parts = Parts {
        mcp: true,
        worker: true,
        instructions: true,
        skill: true,
        permissions: true,
        hooks: false,
    };
    pub const ALL: Parts = Parts {
        hooks: true,
        ..Parts::DEFAULT
    };
    pub const NONE: Parts = Parts {
        mcp: false,
        worker: false,
        instructions: false,
        skill: false,
        permissions: false,
        hooks: false,
    };
}

#[derive(Debug, Clone)]
pub struct Options {
    pub parts: Parts,
    /// Worker model; each harness has its own default (Claude Code: haiku).
    pub model: Option<String>,
    /// Absolute path of the memdag binary.
    pub exe: PathBuf,
    /// Whether plain `memdag` resolves to `exe` on PATH. Project files then say `memdag`, so
    /// they work for everyone who clones the repository.
    pub on_path: bool,
    /// Pin the database (`MEMDAG_DB`) in the server entry.
    pub db: Option<PathBuf>,
    /// Which OpenCode server shape to write.
    pub opencode: OpencodeFormat,
}

/// OpenCode's server entry shape. V2 (`mcp.servers.<name>`) reads V1 (`mcp.<name>`), but V1
/// can't read V2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpencodeFormat {
    /// Keep the shape an existing memdag entry has; V2 for a new one.
    Auto,
    V1,
    /// V2, migrating an existing V1 entry.
    V2,
}

/// One file install/uninstall touched (or would touch).
#[derive(Debug)]
pub struct Step {
    pub part: &'static str,
    pub path: PathBuf,
    pub change: Change,
    pub note: Option<String>,
}

fn step(part: &'static str, path: PathBuf, change: Change) -> Step {
    Step {
        part,
        path,
        change,
        note: None,
    }
}

// ------------------------------------------------------------------ locations

/// The config file holding the MCP entry, and how the entry is nested in it.
struct McpSpec {
    path: PathBuf,
    format: Format,
}

enum Format {
    /// JSON/JSONC, the entry at `keys.. -> "memdag"`.
    Json(Vec<&'static str>),
    /// TOML `[mcp_servers.memdag]`.
    Toml,
}

fn first_existing(candidates: &[PathBuf], default: PathBuf) -> PathBuf {
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .unwrap_or(default)
}

fn opencode_config(env: &Env, loc: Location) -> PathBuf {
    match loc {
        Location::Project => {
            let p = &env.project;
            first_existing(
                &[
                    p.join(".opencode/opencode.jsonc"),
                    p.join(".opencode/opencode.json"),
                    p.join("opencode.jsonc"),
                    p.join("opencode.json"),
                ],
                p.join("opencode.json"),
            )
        }
        Location::Global => {
            let d = env.config_home.join("opencode");
            first_existing(
                &[d.join("opencode.jsonc"), d.join("opencode.json")],
                d.join("opencode.json"),
            )
        }
    }
}

/// Antigravity moved its global config; use the old file only where the new one isn't set up.
fn antigravity_global(env: &Env) -> PathBuf {
    let legacy = env.home.join(".gemini/antigravity/mcp_config.json");
    if legacy.is_file() && !env.home.join(".gemini/config").is_dir() {
        legacy
    } else {
        env.home.join(".gemini/config/mcp_config.json")
    }
}

fn mcp_spec(t: Target, env: &Env, loc: Location, opencode_v1: bool) -> McpSpec {
    let (p, h) = (&env.project, &env.home);
    let project = loc == Location::Project;
    let json = |path: PathBuf, keys: Vec<&'static str>| McpSpec {
        path,
        format: Format::Json(keys),
    };
    match t {
        Target::ClaudeCode => json(
            if project {
                p.join(".mcp.json")
            } else {
                h.join(".claude.json")
            },
            vec!["mcpServers"],
        ),
        Target::Opencode => json(
            opencode_config(env, loc),
            if opencode_v1 {
                vec!["mcp"]
            } else {
                vec!["mcp", "servers"]
            },
        ),
        Target::Codex => McpSpec {
            path: if project {
                p.join(".codex/config.toml")
            } else {
                env.codex_home.join("config.toml")
            },
            format: Format::Toml,
        },
        Target::Cursor => json(
            if project {
                p.join(".cursor/mcp.json")
            } else {
                h.join(".cursor/mcp.json")
            },
            vec!["mcpServers"],
        ),
        Target::Gemini => json(
            if project {
                p.join(".gemini/settings.json")
            } else {
                h.join(".gemini/settings.json")
            },
            vec!["mcpServers"],
        ),
        Target::Vscode => json(
            if project {
                p.join(".vscode/mcp.json")
            } else {
                env.vscode_user.join("mcp.json")
            },
            vec!["servers"],
        ),
        Target::Kiro => json(
            if project {
                p.join(".kiro/settings/mcp.json")
            } else {
                h.join(".kiro/settings/mcp.json")
            },
            vec!["mcpServers"],
        ),
        Target::Antigravity => json(
            if project {
                p.join(".agents/mcp_config.json")
            } else {
                antigravity_global(env)
            },
            vec!["mcpServers"],
        ),
    }
}

fn worker_path(t: Target, env: &Env, loc: Location) -> Option<PathBuf> {
    let base = |project: &str, global: PathBuf| match loc {
        Location::Project => env.project.join(project),
        Location::Global => global,
    };
    let name = harness::WORKER_NAME;
    Some(match t {
        Target::ClaudeCode => {
            base(".claude/agents", env.home.join(".claude/agents")).join(format!("{name}.md"))
        }
        Target::Opencode => base(".opencode/agents", env.config_home.join("opencode/agents"))
            .join(format!("{name}.md")),
        Target::Codex => {
            base(".codex/agents", env.codex_home.join("agents")).join(format!("{name}.toml"))
        }
        Target::Cursor => {
            base(".cursor/agents", env.home.join(".cursor/agents")).join(format!("{name}.md"))
        }
        Target::Gemini => {
            base(".gemini/agents", env.home.join(".gemini/agents")).join(format!("{name}.md"))
        }
        Target::Vscode => base(".github/agents", env.home.join(".copilot/agents"))
            .join(format!("{name}.agent.md")),
        Target::Kiro | Target::Antigravity => return None,
    })
}

/// Where the always-on instructions go.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Instructions {
    /// A marked block in a shared file (`AGENTS.md`, `CLAUDE.md`, ...).
    Block(PathBuf),
    /// A file of memdag's own, with the harness's front matter.
    File(PathBuf, String),
}

impl Instructions {
    fn path(&self) -> &Path {
        match self {
            Instructions::Block(p) | Instructions::File(p, _) => p,
        }
    }
}

fn own_file(front_matter: &str) -> String {
    format!(
        "---\n{front_matter}---\n{}\n\n{}\n",
        harness::FILE_MARKER,
        harness::instructions_body()
    )
}

fn instructions(t: Target, env: &Env, loc: Location) -> Option<Instructions> {
    let (p, h) = (&env.project, &env.home);
    let project = loc == Location::Project;
    let block = |proj: PathBuf, global: PathBuf| {
        Some(Instructions::Block(if project { proj } else { global }))
    };
    match t {
        Target::ClaudeCode => block(p.join("CLAUDE.md"), h.join(".claude/CLAUDE.md")),
        Target::Opencode => block(
            p.join("AGENTS.md"),
            env.config_home.join("opencode/AGENTS.md"),
        ),
        Target::Codex => block(p.join("AGENTS.md"), env.codex_home.join("AGENTS.md")),
        Target::Gemini => block(p.join("GEMINI.md"), h.join(".gemini/GEMINI.md")),
        Target::Vscode => block(
            p.join(".github/copilot-instructions.md"),
            h.join(".copilot/copilot-instructions.md"),
        ),
        // Cursor's user rules live in its settings UI, not a file.
        Target::Cursor => project.then(|| {
            Instructions::File(
                p.join(".cursor/rules/memdag.mdc"),
                own_file(
                    "description: memdag project memory and task tracking\nalwaysApply: true\n",
                ),
            )
        }),
        Target::Kiro => Some(Instructions::File(
            if project {
                p.join(".kiro/steering/memdag.md")
            } else {
                h.join(".kiro/steering/memdag.md")
            },
            own_file("inclusion: always\n"),
        )),
        Target::Antigravity => Some(Instructions::File(
            if project {
                p.join(".agents/rules/memdag.md")
            } else {
                h.join(".gemini/config/rules/memdag.md")
            },
            own_file("trigger: always_on\n"),
        )),
    }
}

fn skill_dir(t: Target, env: &Env, loc: Location) -> PathBuf {
    let (p, h) = (&env.project, &env.home);
    let project = loc == Location::Project;
    let pick = |proj: &str, global: PathBuf| if project { p.join(proj) } else { global };
    match t {
        // Claude Code and Kiro don't read the shared `.agents/skills`.
        Target::ClaudeCode => pick(".claude/skills/memdag", h.join(".claude/skills/memdag")),
        Target::Kiro => pick(".kiro/skills/memdag", h.join(".kiro/skills/memdag")),
        Target::Opencode => pick(
            ".agents/skills/memdag",
            env.config_home.join("opencode/skills/memdag"),
        ),
        Target::Antigravity => pick(
            ".agents/skills/memdag",
            h.join(".gemini/config/skills/memdag"),
        ),
        Target::Codex | Target::Cursor | Target::Gemini | Target::Vscode => {
            pick(".agents/skills/memdag", h.join(".agents/skills/memdag"))
        }
    }
}

fn claude_settings(env: &Env, loc: Location) -> PathBuf {
    match loc {
        Location::Project => env.project.join(".claude/settings.json"),
        Location::Global => env.home.join(".claude/settings.json"),
    }
}

// --------------------------------------------------------------------- detect

/// Whether `t` is in use at `loc`: for the project, only by markers in the project itself
/// (a harness merely installed on this machine is no reason to add its files to a repository
/// teammates share); globally, by its user config directory.
pub fn detect(t: Target, env: &Env, loc: Location) -> bool {
    let (p, h) = (&env.project, &env.home);
    let any = |paths: &[PathBuf]| paths.iter().any(|x| x.exists());
    match (t, loc) {
        (Target::ClaudeCode, Location::Project) => {
            any(&[p.join(".claude"), p.join(".mcp.json"), p.join("CLAUDE.md")])
        }
        (Target::ClaudeCode, Location::Global) => any(&[h.join(".claude"), h.join(".claude.json")]),
        (Target::Opencode, Location::Project) => any(&[
            p.join(".opencode"),
            p.join("opencode.json"),
            p.join("opencode.jsonc"),
        ]),
        (Target::Opencode, Location::Global) => any(&[env.config_home.join("opencode")]),
        (Target::Codex, Location::Project) => any(&[p.join(".codex")]),
        (Target::Codex, Location::Global) => env.codex_home.exists(),
        (Target::Cursor, Location::Project) => any(&[p.join(".cursor")]),
        (Target::Cursor, Location::Global) => any(&[h.join(".cursor")]),
        (Target::Gemini, Location::Project) => any(&[p.join(".gemini"), p.join("GEMINI.md")]),
        (Target::Gemini, Location::Global) => any(&[h.join(".gemini/settings.json")]),
        // Plenty of repositories have .vscode/ without using Copilot's MCP support.
        (Target::Vscode, Location::Project) => any(&[
            p.join(".vscode/mcp.json"),
            p.join(".github/copilot-instructions.md"),
            p.join(".github/agents"),
        ]),
        (Target::Vscode, Location::Global) => env.vscode_user.exists(),
        (Target::Kiro, Location::Project) => any(&[p.join(".kiro")]),
        (Target::Kiro, Location::Global) => any(&[h.join(".kiro")]),
        (Target::Antigravity, Location::Project) => {
            any(&[p.join(".agents/mcp_config.json"), p.join(".agents/rules")])
        }
        (Target::Antigravity, Location::Global) => {
            any(&[h.join(".gemini/config"), h.join(".gemini/antigravity")])
        }
    }
}

/// Whether `t` has a memdag MCP entry at `loc`.
pub fn is_configured(t: Target, env: &Env, loc: Location) -> bool {
    let specs = if t == Target::Opencode {
        vec![mcp_spec(t, env, loc, false), mcp_spec(t, env, loc, true)]
    } else {
        vec![mcp_spec(t, env, loc, false)]
    };
    specs.iter().any(|spec| {
        let Ok(text) = std::fs::read_to_string(&spec.path) else {
            return false;
        };
        match &spec.format {
            Format::Json(keys) => json_lookup(&text, keys).is_some_and(|v| v.get(SERVER).is_some()),
            Format::Toml => text
                .parse::<toml_edit::DocumentMut>()
                .ok()
                .and_then(|d| d.get("mcp_servers")?.get(SERVER).map(|_| ()))
                .is_some(),
        }
    })
}

// -------------------------------------------------------------------- entries

/// The command written into a config. Project files say `memdag` when that resolves on PATH,
/// so a committed config works for every collaborator; global files (and Antigravity, whose
/// macOS app doesn't see the shell's PATH) use the absolute path.
fn command(t: Target, loc: Location, opts: &Options) -> String {
    if loc == Location::Project && opts.on_path && t != Target::Antigravity {
        "memdag".into()
    } else {
        opts.exe.to_string_lossy().into_owned()
    }
}

/// Which OpenCode shape to write at `loc`, and whether to remove the other one.
fn opencode_shape(env: &Env, loc: Location, opts: &Options) -> (bool, bool) {
    let path = opencode_config(env, loc);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let has = |keys: &[&str]| json_lookup(&text, keys).is_some_and(|o| o.get(SERVER).is_some());
    let (v1, v2) = (has(&["mcp"]), has(&["mcp", "servers"]));
    match opts.opencode {
        OpencodeFormat::V1 => (true, v2),
        OpencodeFormat::V2 => (false, v1),
        // Keep a working V1 entry as it is: V1 OpenCode can't read V2's shape.
        OpencodeFormat::Auto if v1 && !v2 => (true, false),
        OpencodeFormat::Auto => (false, v1),
    }
}

/// The server entry in the shape each harness documents.
fn mcp_entry(t: Target, loc: Location, opts: &Options, opencode_v1: bool) -> Value {
    let cmd = command(t, loc, opts);
    let env = opts
        .db
        .as_ref()
        .map(|db| json!({ "MEMDAG_DB": db.to_string_lossy() }));
    let mut e = match t {
        // https://opencode.ai/v2/docs/mcp-servers: one argv array; `codemode: false` keeps the
        // tools native instead of reachable only from Code Mode scripts, which a small worker
        // model handles poorly.
        Target::Opencode if opencode_v1 => {
            json!({ "type": "local", "command": [cmd, "serve"], "enabled": true })
        }
        Target::Opencode => {
            json!({ "type": "local", "command": [cmd, "serve"], "codemode": false })
        }
        Target::ClaudeCode | Target::Cursor | Target::Vscode => {
            json!({ "type": "stdio", "command": cmd, "args": ["serve"] })
        }
        // Gemini, Kiro and Codex infer stdio; Antigravity rejects a `type` field.
        Target::Gemini | Target::Kiro | Target::Antigravity | Target::Codex => {
            json!({ "command": cmd, "args": ["serve"] })
        }
    };
    if let Some(env) = env {
        let key = if t == Target::Opencode {
            "environment"
        } else {
            "env"
        };
        e[key] = env;
    }
    // Kiro approves tools in the server entry itself.
    if t == Target::Kiro && opts.parts.permissions && opts.parts.worker {
        e["autoApprove"] = json!(["get_work", "submit_work"]);
    }
    e
}

// --------------------------------------------------------------- JSON editing

/// Strip `//` and `/* */` comments and trailing commas so JSONC parses as JSON (strings are
/// left untouched). Used only to compare values; edits go through the comment-preserving CST.
pub fn strip_jsonc(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_str = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            (',', _) => {
                let rest: String = chars.clone().collect();
                if !matches!(trailing_lookahead(&rest), Some('}') | Some(']')) {
                    out.push(',');
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn trailing_lookahead(rest: &str) -> Option<char> {
    let mut s = rest;
    loop {
        s = s.trim_start();
        if let Some(r) = s.strip_prefix("//") {
            s = r.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(r) = s.strip_prefix("/*") {
            s = r.split_once("*/").map_or("", |(_, r)| r);
        } else {
            return s.chars().next();
        }
    }
}

/// The object at `keys` in a JSON/JSONC document, if every level exists.
fn json_lookup(text: &str, keys: &[&str]) -> Option<Value> {
    let mut v: Value = serde_json::from_str(&strip_jsonc(text)).ok()?;
    for k in keys {
        v = v.get(*k)?.clone();
    }
    v.is_object().then_some(v)
}

fn to_cst(v: &Value) -> CstInputValue {
    match v {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(a) => CstInputValue::Array(a.iter().map(to_cst).collect()),
        Value::Object(o) => {
            CstInputValue::Object(o.iter().map(|(k, v)| (k.clone(), to_cst(v))).collect())
        }
    }
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn parse_cst(path: &Path, text: &str) -> Result<CstRootNode> {
    let text = if text.trim().is_empty() { "{}\n" } else { text };
    let root = CstRootNode::parse(text, &ParseOptions::default()).map_err(|e| {
        anyhow::anyhow!(
            "{} is not valid JSON/JSONC ({e}); left unchanged. Fix it, or pass --print-config and edit it by hand",
            path.display()
        )
    })?;
    if root.object_value().is_none() {
        bail!("{} is not a JSON object; left unchanged", path.display());
    }
    Ok(root)
}

/// memdag's server entry merged into the one already there. memdag owns `type`, `command`
/// and `args` and its own environment variable; everything else the user added (other
/// environment variables, timeouts, tool lists, a pinned `MEMDAG_DB` when `--db` isn't given)
/// stays. Flags a user may have flipped on purpose (`enabled`, `disabled`, `codemode`) are
/// only filled in when absent, and approval lists are extended, not replaced.
fn merge_entry(existing: Option<&Value>, ours: &Value) -> Value {
    let (Some(Value::Object(old)), Value::Object(new)) = (existing, ours) else {
        return ours.clone();
    };
    let mut out = old.clone();
    for (k, v) in new {
        match k.as_str() {
            "enabled" | "disabled" | "codemode" => {
                out.entry(k.clone()).or_insert_with(|| v.clone());
            }
            "env" | "environment" => match (out.get_mut(k), v) {
                (Some(Value::Object(dst)), Value::Object(src)) => {
                    for (a, b) in src {
                        dst.insert(a.clone(), b.clone());
                    }
                }
                _ => {
                    out.insert(k.clone(), v.clone());
                }
            },
            "autoApprove" => match (out.get_mut(k), v) {
                (Some(Value::Array(dst)), Value::Array(src)) => {
                    for x in src {
                        if !dst.contains(x) {
                            dst.push(x.clone());
                        }
                    }
                }
                _ => {
                    out.insert(k.clone(), v.clone());
                }
            },
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// Set `keys.. -> name` to `value` merged into any existing entry (see [`merge_entry`]) in a
/// JSON/JSONC file, keeping comments and formatting. Only the keys that change are touched,
/// so comments inside the entry survive too.
fn json_set(path: &Path, keys: &[&str], name: &str, value: &Value) -> Result<Change> {
    let old = read_optional(path)?;
    let existing = old
        .as_deref()
        .and_then(|t| json_lookup(t, keys))
        .and_then(|o| o.get(name).cloned());
    let merged = merge_entry(existing.as_ref(), value);
    if existing.as_ref() == Some(&merged) {
        return Ok(Change::Unchanged);
    }
    let root = parse_cst(path, old.as_deref().unwrap_or(""))?;
    let mut obj: CstObject = root.object_value().expect("checked in parse_cst");
    for k in keys {
        obj = obj.object_value_or_create(k).with_context(|| {
            format!(
                "`{k}` in {} is not an object; left unchanged",
                path.display()
            )
        })?;
    }
    let prop = obj.get(name);
    match (prop.as_ref().and_then(|p| p.object_value()), &existing) {
        (Some(entry), Some(Value::Object(before))) => {
            for (k, v) in merged.as_object().expect("merged an object") {
                if before.get(k) == Some(v) {
                    continue;
                }
                match entry.get(k) {
                    Some(p) => p.set_value(to_cst(v)),
                    None => {
                        entry.append(k, to_cst(v));
                    }
                }
            }
        }
        _ => match prop {
            Some(p) => p.set_value(to_cst(&merged)),
            None => {
                obj.append(name, to_cst(&merged));
            }
        },
    }
    let mut text = root.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    harness::replace_file(path, &text)?;
    Ok(if old.is_some() {
        Change::Updated
    } else {
        Change::Created
    })
}

/// Remove `keys.. -> name`, then any parent object left empty, keeping everything else. A
/// file left as an empty object is deleted.
fn json_remove(path: &Path, keys: &[&str], name: &str) -> Result<Change> {
    let Some(text) = read_optional(path)? else {
        return Ok(Change::NotFound);
    };
    if json_lookup(&text, keys).is_none_or(|o| o.get(name).is_none()) {
        return Ok(Change::NotFound);
    }
    let root = parse_cst(path, &text)?;
    let mut chain: Vec<CstObject> = vec![root.object_value().expect("checked in parse_cst")];
    for k in keys {
        let next = chain
            .last()
            .and_then(|o| o.object_value(k))
            .with_context(|| format!("`{k}` vanished from {}", path.display()))?;
        chain.push(next);
    }
    if let Some(prop) = chain.last().and_then(|o| o.get(name)) {
        prop.remove();
    }
    // Drop containers memdag's entry was the last thing in, innermost first.
    for depth in (0..keys.len()).rev() {
        if chain[depth + 1].properties().is_empty()
            && let Some(prop) = chain[depth].get(keys[depth])
        {
            prop.remove();
        } else {
            break;
        }
    }
    if chain[0].properties().is_empty() {
        std::fs::remove_file(path)?;
    } else {
        let mut out = root.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        harness::replace_file(path, &out)?;
    }
    Ok(Change::Removed)
}

// --------------------------------------------------------------- TOML editing

fn toml_doc(path: &Path, text: &str) -> Result<toml_edit::DocumentMut> {
    text.parse::<toml_edit::DocumentMut>()
        .map_err(|e| anyhow::anyhow!("{} is not valid TOML ({e}); left unchanged", path.display()))
}

/// `[mcp_servers.memdag]` in a Codex config, keeping comments and everything else.
fn toml_set(path: &Path, value: &Value) -> Result<Change> {
    let old = read_optional(path)?;
    let text = toml_render(path, old.as_deref().unwrap_or(""), value)?;
    if old.as_deref() == Some(text.as_str()) {
        return Ok(Change::Unchanged);
    }
    harness::write_if_changed(path, &text)
}

/// A TOML item as JSON, for the simple values memdag compares (strings, numbers, booleans,
/// arrays and tables of them). None for anything else, which memdag never touches.
fn toml_to_json(item: &toml_edit::Item) -> Option<Value> {
    fn value(v: &toml_edit::Value) -> Option<Value> {
        Some(match v {
            toml_edit::Value::String(s) => json!(s.value()),
            toml_edit::Value::Integer(i) => json!(i.value()),
            toml_edit::Value::Float(f) => json!(f.value()),
            toml_edit::Value::Boolean(b) => json!(b.value()),
            toml_edit::Value::Array(a) => Value::Array(a.iter().map(value).collect::<Option<_>>()?),
            toml_edit::Value::InlineTable(t) => Value::Object(
                t.iter()
                    .map(|(k, v)| Some((k.to_string(), value(v)?)))
                    .collect::<Option<_>>()?,
            ),
            _ => return None,
        })
    }
    match item {
        toml_edit::Item::Value(v) => value(v),
        toml_edit::Item::Table(t) => Some(Value::Object(
            t.iter()
                .filter_map(|(k, v)| Some((k.to_string(), toml_to_json(v)?)))
                .collect(),
        )),
        _ => None,
    }
}

fn json_to_toml(v: &Value) -> Option<toml_edit::Value> {
    Some(match v {
        Value::String(s) => s.as_str().into(),
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64()?.into(),
        },
        Value::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for x in a {
                arr.push(json_to_toml(x)?);
            }
            arr.into()
        }
        _ => return None,
    })
}

/// `text` with memdag's `[mcp_servers.memdag]` keys merged in (see [`merge_entry`]); the
/// user's other keys, comments and formatting stay.
fn toml_render(path: &Path, text: &str, value: &Value) -> Result<String> {
    let mut doc = toml_doc(path, text)?;
    let servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            toml_edit::Item::Table(t)
        })
        .as_table_mut()
        .with_context(|| format!("`mcp_servers` in {} is not a table", path.display()))?;
    let existing = servers.get(SERVER).and_then(toml_to_json);
    let merged = merge_entry(existing.as_ref(), value);
    let entry = servers
        .entry(SERVER)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .with_context(|| {
            format!(
                "`mcp_servers.{SERVER}` in {} is not a table",
                path.display()
            )
        })?;
    for (k, v) in merged.as_object().expect("merged an object") {
        if existing.as_ref().and_then(|e| e.get(k)) == Some(v) {
            continue;
        }
        match v {
            // Sub-tables (env): set memdag's keys inside, keep the user's.
            Value::Object(map) => {
                if entry.get(k).and_then(|i| i.as_table_like()).is_none() {
                    entry.insert(k, toml_edit::Item::Table(toml_edit::Table::new()));
                }
                let sub = entry
                    .get_mut(k)
                    .and_then(|i| i.as_table_like_mut())
                    .expect("just ensured");
                for (sk, sv) in map {
                    if let Some(tv) = json_to_toml(sv) {
                        sub.insert(sk, toml_edit::Item::Value(tv));
                    }
                }
            }
            _ => {
                if let Some(tv) = json_to_toml(v) {
                    entry.insert(k, toml_edit::Item::Value(tv));
                }
            }
        }
    }
    Ok(doc.to_string())
}

fn toml_remove(path: &Path) -> Result<Change> {
    let Some(text) = read_optional(path)? else {
        return Ok(Change::NotFound);
    };
    let mut doc = toml_doc(path, &text)?;
    let Some(servers) = doc.get_mut("mcp_servers").and_then(|t| t.as_table_mut()) else {
        return Ok(Change::NotFound);
    };
    if servers.remove(SERVER).is_none() {
        return Ok(Change::NotFound);
    }
    if servers.is_empty() {
        doc.remove("mcp_servers");
    }
    let out = doc.to_string();
    if out.trim().is_empty() {
        std::fs::remove_file(path)?;
    } else {
        harness::replace_file(path, &out)?;
    }
    Ok(Change::Removed)
}

// ------------------------------------------------------------- small helpers

/// Write a file that is entirely memdag's, refusing to replace one that isn't.
fn write_owned(path: &Path, text: &str) -> Result<Change> {
    if let Some(existing) = read_optional(path)?
        && !harness::is_generated(&existing)
    {
        bail!(
            "{} exists and wasn't written by memdag; remove or rename it first",
            path.display()
        );
    }
    harness::write_if_changed(path, text)
}

fn remove_owned(path: &Path) -> Result<Change> {
    match read_optional(path)? {
        None => Ok(Change::NotFound),
        Some(t) if !harness::is_generated(&t) => Ok(Change::Kept),
        Some(_) => {
            std::fs::remove_file(path)?;
            if let Some(dir) = path.parent() {
                std::fs::remove_dir(dir).ok();
            }
            Ok(Change::Removed)
        }
    }
}

fn install_skill(dir: &Path) -> Result<Change> {
    if dir
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
        || (dir.join("SKILL.md").exists() && !dir.join(SKILL_STAMP).exists())
    {
        return Ok(Change::Kept);
    }
    let change = harness::write_if_changed(&dir.join("SKILL.md"), SKILL_MD)?;
    harness::write_if_changed(
        &dir.join(SKILL_STAMP),
        &format!(
            "Installed by memdag {}; `memdag uninstall` removes this skill.\n",
            env!("CARGO_PKG_VERSION")
        ),
    )?;
    Ok(change)
}

fn remove_skill(dir: &Path) -> Result<Change> {
    if !dir.join(SKILL_STAMP).exists() {
        return Ok(if dir.join("SKILL.md").exists() {
            Change::Kept
        } else {
            Change::NotFound
        });
    }
    std::fs::remove_file(dir.join("SKILL.md")).ok();
    std::fs::remove_file(dir.join(SKILL_STAMP))?;
    std::fs::remove_dir(dir).ok();
    Ok(Change::Removed)
}

/// The SessionStart hook command memdag adds to Claude Code settings. Claude Code runs it
/// through a shell (bash on Windows too), so a path is single-quoted and uses forward
/// slashes; a path with spaces or backslashes would otherwise break.
fn hook_command(cmd: &str) -> String {
    if cmd == "memdag" {
        return "memdag hook session-start".into();
    }
    let path = if cfg!(windows) {
        cmd.replace('\\', "/")
    } else {
        cmd.to_string()
    };
    format!("'{}' hook session-start", path.replace('\'', r"'\''"))
}

fn is_memdag_hook(v: &Value) -> bool {
    v.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains("memdag") && c.ends_with("hook session-start"))
}

/// Remove memdag's hooks from SessionStart groups, dropping a group only when nothing else is
/// left in it. Returns whether anything was removed.
fn strip_memdag_hooks(groups: &mut Vec<Value>) -> bool {
    let mut removed = false;
    for g in groups.iter_mut() {
        if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
            let before = hs.len();
            hs.retain(|h| !is_memdag_hook(h));
            removed |= hs.len() != before;
        }
    }
    groups.retain(|g| g["hooks"].as_array().is_none_or(|hs| !hs.is_empty()));
    removed
}

fn edit_settings(
    path: &Path,
    f: impl FnOnce(&mut serde_json::Map<String, Value>) -> bool,
) -> Result<Change> {
    let old = read_optional(path)?;
    let mut root = match &old {
        None => serde_json::Map::new(),
        Some(t) => match serde_json::from_str::<Value>(t) {
            Ok(Value::Object(o)) => o,
            _ => bail!("{} is not a JSON object; left unchanged", path.display()),
        },
    };
    if !f(&mut root) {
        return Ok(if old.is_some() {
            Change::Unchanged
        } else {
            Change::NotFound
        });
    }
    if root.is_empty() {
        std::fs::remove_file(path).ok();
        return Ok(Change::Removed);
    }
    let mut text = serde_json::to_string_pretty(&root)?;
    text.push('\n');
    harness::write_if_changed(path, &text)
}

fn add_session_hook(path: &Path, cmd: &str) -> Result<Change> {
    let wanted = json!({ "type": "command", "command": hook_command(cmd), "timeout": 10 });
    edit_settings(path, |root| {
        let groups = root
            .entry("hooks")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .map(|h| h.entry("SessionStart").or_insert_with(|| json!([])));
        let Some(Value::Array(groups)) = groups else {
            return false;
        };
        let present = groups
            .iter()
            .any(|g| g["hooks"].as_array().is_some_and(|hs| hs.contains(&wanted)));
        if present {
            return false;
        }
        // Replace an older memdag hook (e.g. a different binary path) rather than adding one,
        // without touching the user's hooks that share its group.
        strip_memdag_hooks(groups);
        groups.push(json!({ "hooks": [wanted] }));
        true
    })
}

fn remove_session_hook(path: &Path) -> Result<Change> {
    if !path.exists() {
        return Ok(Change::NotFound);
    }
    let change = edit_settings(path, |root| {
        let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
            return false;
        };
        let Some(groups) = hooks.get_mut("SessionStart").and_then(Value::as_array_mut) else {
            return false;
        };
        if !strip_memdag_hooks(groups) {
            return false;
        }
        if groups.is_empty() {
            hooks.remove("SessionStart");
        }
        if hooks.is_empty() {
            root.remove("hooks");
        }
        true
    })?;
    Ok(match change {
        Change::Unchanged => Change::NotFound,
        Change::Updated | Change::Created => Change::Removed,
        c => c,
    })
}

// --------------------------------------------------------------------- public

/// The model an existing generated worker file pins, if any.
fn existing_model(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    if !harness::is_generated(&text) {
        return None;
    }
    text.lines().find_map(|l| {
        let v = l
            .strip_prefix("model:")
            .or_else(|| l.strip_prefix("model ="))?
            .trim()
            .trim_matches(['[', ']', '\'', '"'])
            .to_string();
        // Claude Code's default and Cursor's "inherit" aren't pins.
        (!v.is_empty() && v != "inherit").then_some(v)
    })
}

/// Which parts of memdag `t` has at `loc` right now (what `--refresh` updates).
pub fn installed_parts(t: Target, env: &Env, loc: Location) -> Parts {
    let generated =
        |p: &Path| std::fs::read_to_string(p).is_ok_and(|text| harness::is_generated(&text));
    let worker = worker_path(t, env, loc).is_some_and(|p| generated(&p));
    let instructions = instructions(t, env, loc).is_some_and(|i| match i {
        Instructions::Block(p) => std::fs::read_to_string(p)
            .is_ok_and(|text| text.lines().any(|l| l.trim_end() == harness::BLOCK_START)),
        Instructions::File(p, _) => generated(&p),
    });
    let settings = std::fs::read_to_string(claude_settings(env, loc)).unwrap_or_default();
    let (permissions, hooks) = match t {
        Target::ClaudeCode => (
            harness::CLAUDE_ALLOW.iter().all(|r| settings.contains(r)),
            settings.contains("hook session-start"),
        ),
        Target::Opencode => (
            worker_path(t, env, loc)
                .and_then(|p| std::fs::read_to_string(p).ok())
                .is_some_and(|w| w.contains("effect: allow")),
            false,
        ),
        Target::Kiro => (
            std::fs::read_to_string(mcp_spec(t, env, loc, false).path)
                .is_ok_and(|c| c.contains("autoApprove")),
            false,
        ),
        _ => (false, false),
    };
    Parts {
        mcp: is_configured(t, env, loc),
        worker,
        instructions,
        skill: skill_dir(t, env, loc).join(SKILL_STAMP).exists(),
        permissions,
        hooks,
    }
}

/// Whether `t` has memdag at `loc` in a file of its own (its MCP entry, worker or own rule
/// file). Shared files (`AGENTS.md`, `.agents/skills`) don't count, since they can't say
/// which harness they are there for.
pub fn is_installed(t: Target, env: &Env, loc: Location) -> bool {
    let p = installed_parts(t, env, loc);
    let own_rule =
        matches!(instructions(t, env, loc), Some(Instructions::File(..))) && p.instructions;
    p.mcp || p.worker || own_rule
}

fn same_file(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// Write the selected parts for `t`. The MCP entry goes first: it's the file most likely to
/// be refused (unparseable), and refusing before anything else was written leaves nothing
/// half-installed.
pub fn install(t: Target, env: &Env, loc: Location, opts: &Options) -> Result<Vec<Step>> {
    harness::check_model(opts.model.as_deref())?;
    let parts = opts.parts;
    let mut steps = Vec::new();

    if parts.mcp {
        let (v1, migrate) = if t == Target::Opencode {
            opencode_shape(env, loc, opts)
        } else {
            (false, false)
        };
        let spec = mcp_spec(t, env, loc, v1);
        let entry = mcp_entry(t, loc, opts, v1);
        let change = match &spec.format {
            Format::Json(keys) => json_set(&spec.path, keys, SERVER, &entry)?,
            Format::Toml => toml_set(&spec.path, &entry)?,
        };
        let mut s = step("MCP server", spec.path.clone(), change);
        if t == Target::Codex && loc == Location::Project {
            s.note = Some(
                "Codex reads a project's .codex/config.toml only once the project is trusted"
                    .into(),
            );
        }
        steps.push(s);
        // Move an OpenCode entry from the other shape, so it isn't defined twice.
        if t == Target::Opencode && migrate {
            let other = mcp_spec(t, env, loc, !v1);
            if let Format::Json(keys) = &other.format
                && other.path == spec.path
                && json_remove(&other.path, keys, SERVER)? == Change::Removed
            {
                let mut s = step("MCP server (old shape)", other.path, Change::Removed);
                s.note = Some(if v1 {
                    "moved from mcp.servers.memdag (V2) to mcp.memdag (V1)".into()
                } else {
                    "migrated mcp.memdag (V1) to mcp.servers.memdag (V2)".into()
                });
                steps.push(s);
            }
        }
    }

    if parts.permissions && t == Target::ClaudeCode {
        let path = claude_settings(env, loc);
        let change = harness::allow_worker_tools(&path)?;
        steps.push(step("approve worker tools", path, change));
    }

    // Harnesses whose approvals live in their UI rather than a file memdag could edit.
    if parts.permissions
        && parts.worker
        && !matches!(t, Target::ClaudeCode | Target::Opencode | Target::Kiro)
    {
        steps.push(Step {
            part: "approve worker tools",
            path: PathBuf::new(),
            change: Change::Unchanged,
            note: Some(format!(
                "{} keeps tool approvals in its own settings; allow memdag's get_work and submit_work there once",
                t.name()
            )),
        });
    }

    if parts.worker {
        match (t.harness(), worker_path(t, env, loc)) {
            (Some(h), Some(path)) => {
                // Keep a model the user pinned earlier unless --model says otherwise.
                let kept = opts.model.is_none().then(|| existing_model(&path)).flatten();
                let model = opts.model.as_deref().or(kept.as_deref());
                let text = h.worker_file(model, parts.permissions);
                let change = write_owned(&path, &text)?;
                let mut s = step("worker subagent", path, change);
                if model.is_none() && t != Target::ClaudeCode {
                    s.note = Some(format!(
                        "no --model given: the worker uses {}'s default model; pin a Haiku- or Flash-class one with --model",
                        t.name()
                    ));
                }
                steps.push(s);
            }
            _ => steps.push(Step {
                part: "worker subagent",
                path: PathBuf::new(),
                change: Change::Unchanged,
                note: Some(format!(
                    "{} has no subagent files with a per-agent model; the prompts fall back to its own subagents",
                    t.name()
                )),
            }),
        }
    }

    if parts.instructions
        && let Some(instr) = instructions(t, env, loc)
    {
        let change = match &instr {
            Instructions::Block(p) => harness::upsert_block(p)?,
            Instructions::File(p, text) => write_owned(p, text)?,
        };
        steps.push(step("instructions", instr.path().to_path_buf(), change));
    }

    if parts.skill {
        let dir = skill_dir(t, env, loc);
        let change = install_skill(&dir)?;
        let mut s = step("skill", dir, change);
        if s.change == Change::Kept {
            s.note = Some("a skill that memdag didn't install is already there; left alone".into());
        }
        steps.push(s);
    }

    if parts.hooks && t == Target::ClaudeCode {
        let path = claude_settings(env, loc);
        let change = add_session_hook(&path, &command(t, loc, opts))?;
        steps.push(step("SessionStart hook", path, change));
    }

    Ok(steps)
}

/// Remove the selected parts for `t`. Files another still-configured target shares (one
/// `AGENTS.md`, one `.agents/skills/memdag`) are kept while that target needs them.
pub fn uninstall(
    t: Target,
    env: &Env,
    loc: Location,
    parts: Parts,
    still_configured: &[Target],
) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    let shared_with = |path: &Path, f: &dyn Fn(Target) -> Option<PathBuf>| {
        still_configured
            .iter()
            .find(|o| **o != t && f(**o).is_some_and(|p| same_file(&p, path)))
            .copied()
    };

    if parts.mcp {
        let shapes: &[bool] = if t == Target::Opencode {
            &[false, true]
        } else {
            &[false]
        };
        for &v1 in shapes {
            let spec = mcp_spec(t, env, loc, v1);
            let change = match &spec.format {
                Format::Json(keys) => json_remove(&spec.path, keys, SERVER)?,
                Format::Toml => toml_remove(&spec.path)?,
            };
            if change != Change::NotFound || !v1 {
                steps.push(step("MCP server", spec.path, change));
            }
        }
    }
    if parts.permissions && t == Target::ClaudeCode {
        let path = claude_settings(env, loc);
        steps.push(step(
            "approve worker tools",
            path.clone(),
            harness::remove_worker_tools(&path)?,
        ));
    }
    if parts.worker
        && let Some(path) = worker_path(t, env, loc)
    {
        let change = remove_owned(&path)?;
        steps.push(step("worker subagent", path, change));
    }
    if parts.instructions
        && let Some(instr) = instructions(t, env, loc)
    {
        let path = instr.path().to_path_buf();
        let other = shared_with(&path, &|o| {
            instructions(o, env, loc).map(|i| i.path().to_path_buf())
        });
        let mut s = match (&instr, other) {
            (_, Some(o)) => {
                let mut s = step("instructions", path, Change::Kept);
                s.note = Some(format!("still used by {}", o.name()));
                s
            }
            (Instructions::Block(p), None) => {
                step("instructions", path.clone(), harness::remove_block(p)?)
            }
            (Instructions::File(p, _), None) => {
                step("instructions", path.clone(), remove_owned(p)?)
            }
        };
        if s.change == Change::Kept && s.note.is_none() {
            s.note = Some("not written by memdag; left alone".into());
        }
        steps.push(s);
    }
    if parts.skill {
        let dir = skill_dir(t, env, loc);
        match shared_with(&dir, &|o| Some(skill_dir(o, env, loc))) {
            Some(o) => {
                let mut s = step("skill", dir, Change::Kept);
                s.note = Some(format!("still used by {}", o.name()));
                steps.push(s);
            }
            None => {
                let change = remove_skill(&dir)?;
                steps.push(step("skill", dir, change));
            }
        }
    }
    if parts.hooks && t == Target::ClaudeCode {
        let path = claude_settings(env, loc);
        steps.push(step(
            "SessionStart hook",
            path.clone(),
            remove_session_hook(&path)?,
        ));
    }
    Ok(steps)
}

/// The MCP entry for `t` in an explicit file and/or under an explicit key
/// (`memdag mcp install --path/--key`). A key may be dotted (`mcp.servers`).
pub fn install_entry_at(
    t: Target,
    env: &Env,
    loc: Location,
    opts: &Options,
    path: Option<&Path>,
    key: Option<&str>,
) -> Result<(PathBuf, Change)> {
    let v1 = t == Target::Opencode && opencode_shape(env, loc, opts).0;
    let spec = mcp_spec(t, env, loc, v1);
    let path = path.map(Path::to_path_buf).unwrap_or(spec.path);
    let entry = mcp_entry(t, loc, opts, v1);
    let change = match (key, &spec.format) {
        (Some(k), _) => json_set(&path, &k.split('.').collect::<Vec<_>>(), SERVER, &entry)?,
        (None, Format::Json(keys)) => json_set(&path, keys, SERVER, &entry)?,
        (None, Format::Toml) => toml_set(&path, &entry)?,
    };
    Ok((path, change))
}

/// Inverse of [`install_entry_at`].
pub fn remove_entry_at(
    t: Target,
    env: &Env,
    loc: Location,
    path: Option<&Path>,
    key: Option<&str>,
) -> Result<(PathBuf, Change)> {
    let spec = mcp_spec(t, env, loc, false);
    let path = path.map(Path::to_path_buf).unwrap_or(spec.path);
    let change = match (key, &spec.format) {
        (Some(k), _) => json_remove(&path, &k.split('.').collect::<Vec<_>>(), SERVER)?,
        (None, Format::Json(keys)) => json_remove(&path, keys, SERVER)?,
        (None, Format::Toml) => toml_remove(&path)?,
    };
    Ok((path, change))
}

/// What `install` would write, without writing anything.
pub fn print_config(t: Target, env: &Env, loc: Location, opts: &Options) -> Result<String> {
    harness::check_model(opts.model.as_deref())?;
    let parts = opts.parts;
    let mut out = format!(
        "# {} ({})\n",
        t.name(),
        if loc == Location::Project {
            "project"
        } else {
            "global"
        }
    );
    if parts.mcp {
        let v1 = t == Target::Opencode && opencode_shape(env, loc, opts).0;
        let spec = mcp_spec(t, env, loc, v1);
        let entry = mcp_entry(t, loc, opts, v1);
        let shown = match &spec.format {
            Format::Json(keys) => {
                let mut v = json!({ SERVER: entry });
                for k in keys.iter().rev() {
                    v = json!({ *k: v });
                }
                serde_json::to_string_pretty(&v)?
            }
            Format::Toml => toml_render(&spec.path, "", &entry)?,
        };
        out.push_str(&format!(
            "\n## MCP server: {}\n{shown}\n",
            spec.path.display()
        ));
    }
    if parts.permissions && t == Target::ClaudeCode {
        out.push_str(&format!(
            "\n## approve worker tools: {} (permissions.allow)\n{}\n",
            claude_settings(env, loc).display(),
            harness::CLAUDE_ALLOW.join(", ")
        ));
    }
    if parts.worker
        && let (Some(h), Some(path)) = (t.harness(), worker_path(t, env, loc))
    {
        out.push_str(&format!(
            "\n## worker subagent: {}\n{}",
            path.display(),
            h.worker_file(opts.model.as_deref(), parts.permissions)
        ));
    }
    if parts.instructions
        && let Some(instr) = instructions(t, env, loc)
    {
        let text = match &instr {
            Instructions::Block(_) => harness::INSTRUCTIONS_BLOCK.to_string(),
            Instructions::File(_, text) => text.clone(),
        };
        out.push_str(&format!(
            "\n## instructions: {}\n{text}",
            instr.path().display()
        ));
    }
    if parts.skill {
        out.push_str(&format!(
            "\n## skill: {}/SKILL.md ({} bytes)\n",
            skill_dir(t, env, loc).display(),
            SKILL_MD.len()
        ));
    }
    if parts.hooks && t == Target::ClaudeCode {
        out.push_str(&format!(
            "\n## SessionStart hook: {}\n{}\n",
            claude_settings(env, loc).display(),
            hook_command(&command(t, loc, opts))
        ));
    }
    Ok(out)
}
