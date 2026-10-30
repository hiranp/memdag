//! Optional embeddings via an external command (no bundled model).
//!
//! `MEMDAG_EMBED_CMD` names a shell command that reads text on stdin and prints a
//! [`DIM`]-float JSON array (or `{"embedding": [...]}`) on stdout. memdag never ships or
//! downloads a model: pick any local embedder (Ollama, a script, another tool).
//!
//! The command comes from the environment only. It is deliberately not readable from the
//! repo-level `memdag.toml`, because a cloned repository must not be able to make memdag
//! execute a command.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

/// Dimension of the `memories_vec` table.
pub const DIM: usize = 384;
pub const EMBED_CMD_ENV: &str = "MEMDAG_EMBED_CMD";
const TIMEOUT: Duration = Duration::from_secs(20);
/// Text sent to the embedder is capped; embedding models truncate long inputs anyway.
const MAX_INPUT_CHARS: usize = 8000;

/// The configured embed command, if any.
pub fn command_from_env() -> Option<String> {
    std::env::var(EMBED_CMD_ENV)
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
}

/// Parse an embedder's stdout into a validated vector.
pub fn parse_embedding(raw: &str) -> Result<Vec<f32>> {
    let value: serde_json::Value =
        serde_json::from_str(raw.trim()).context("embed command output is not JSON")?;
    let array = match &value {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(o) => o
            .get("embedding")
            .and_then(|v| v.as_array())
            .context("embed command JSON object has no \"embedding\" array")?,
        _ => bail!("embed command output must be a JSON array of numbers"),
    };
    let vec: Vec<f32> = array
        .iter()
        .map(|v| {
            v.as_f64()
                .map(|f| f as f32)
                .filter(|f| f.is_finite())
                .context("embedding contains a non-numeric or non-finite value")
        })
        .collect::<Result<_>>()?;
    if vec.len() != DIM {
        bail!(
            "embedding has {} dimensions; memdag's vector table needs exactly {DIM}",
            vec.len()
        );
    }
    Ok(vec)
}

/// After a failure the embedder is skipped for this long, so a dead command costs one
/// timeout, not one per search/write in a long-lived MCP server.
const COOLDOWN: Duration = Duration::from_secs(60);
/// How long to wait for output after the process has exited (a surviving grandchild can
/// keep the pipe open).
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Per-command failure times; keyed by command so unrelated embedders don't affect each other.
static FAILED_AT: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

fn in_cooldown(cmd: &str) -> bool {
    let guard = FAILED_AT.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .and_then(|m| m.get(cmd))
        .is_some_and(|t| t.elapsed() < COOLDOWN)
}

fn record_failure(cmd: &str) {
    let mut guard = FAILED_AT.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .insert(cmd.to_string(), Instant::now());
}

/// Run `cmd` through the platform shell with `text` on stdin. Bounded by a timeout so a
/// wedged embedder can't hang the MCP server, and skipped for a cooldown after a failure.
pub fn embed(cmd: &str, text: &str) -> Result<Vec<f32>> {
    if in_cooldown(cmd) {
        bail!(
            "{EMBED_CMD_ENV} failed recently; skipping it for {}s",
            COOLDOWN.as_secs()
        );
    }
    let result = run_with_timeout(cmd, text, TIMEOUT);
    if result.is_err() {
        record_failure(cmd);
    }
    result
}

/// Kill the whole process group on unix (the shell's pipeline children would otherwise
/// survive `kill()` of just `sh`); fall back to killing the child elsewhere.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{}", child.id())])
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// [`embed`] without the failure cooldown and with an explicit timeout.
pub fn run_with_timeout(cmd: &str, text: &str, timeout: Duration) -> Result<Vec<f32>> {
    let text: String = text.chars().take(MAX_INPUT_CHARS).collect();

    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c.process_group(0); // own group, so kill_tree can reach the whole pipeline
        c
    };

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start {EMBED_CMD_ENV}"))?;

    // A dedicated writer thread avoids a deadlock when the embedder streams output before
    // it has consumed all of its input.
    let mut stdin = child.stdin.take().context("embed command has no stdin")?;
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(text.as_bytes());
    });
    let mut stdout = child.stdout.take().context("embed command has no stdout")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_tree(&mut child);
            let _ = child.wait();
            bail!("{EMBED_CMD_ENV} timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let _ = writer.join();
    // Bounded wait: a grandchild holding the pipe open must not block us forever.
    let out = rx
        .recv_timeout(DRAIN_TIMEOUT)
        .map_err(|_| anyhow::anyhow!("{EMBED_CMD_ENV} output did not close after exit"))?;
    if !status.success() {
        bail!("{EMBED_CMD_ENV} exited with {status}");
    }
    parse_embedding(&out)
}

/// The text embedded for a memory: title plus body, so both are searchable by meaning.
pub fn memory_text(title: &str, body: &str) -> String {
    format!("{title}\n\n{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arr(n: usize) -> String {
        format!("[{}]", vec!["0.5"; n].join(","))
    }

    #[test]
    fn accepts_array_and_object_forms() {
        assert_eq!(parse_embedding(&arr(DIM)).unwrap().len(), DIM);
        let obj = format!("{{\"embedding\": {}}}", arr(DIM));
        assert_eq!(parse_embedding(&obj).unwrap().len(), DIM);
    }

    #[test]
    fn rejects_wrong_dimension_and_garbage() {
        assert!(
            parse_embedding(&arr(3))
                .unwrap_err()
                .to_string()
                .contains("384")
        );
        assert!(parse_embedding("not json").is_err());
        assert!(parse_embedding("{\"nope\": 1}").is_err());
        assert!(parse_embedding(&arr(DIM).replacen("0.5", "\"x\"", 1)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runs_a_command_and_enforces_failure() {
        let ok = format!("printf '%s' '{}'", arr(DIM));
        assert_eq!(embed(&ok, "hello").unwrap().len(), DIM);
        assert!(embed("exit 3", "x").is_err());
        assert!(embed("printf '[1,2]'", "x").is_err());
    }
}
