#!/usr/bin/env python3
"""Does memdag cut corrections and cost? (ADR 0001, "Validation")

Runs each task in `tasks.py` with a coding agent (Claude Code, headless) on a fresh copy of
`fixture/`, under three conditions:

  none                no memory: the agent sees only the code.
  mcp                 the memdag MCP server, holding the memories in `memories.py`.
  installed           the same, plus the instructions `memdag install` adds to CLAUDE.md.
  installed+keywords  the same, after a cheap model expanded the memories' search keywords
                      (`memdag work next expand`), to test the vocabulary gap.

and grades the result with each task's deterministic grader.

The memdag conditions come in two forms: `mcp` only connects the server (the agent sees the
server's own instructions and tool descriptions), while `installed` adds the instructions
block `memdag install` writes to CLAUDE.md. `--bare` doesn't read CLAUDE.md, so the block is
passed with --append-system-prompt; it is taken from `memdag install --print-config`, so it is
exactly what an install writes. A failed grade is one
correction a reviewer would have to ask for.

Isolation: `claude --bare` (no CLAUDE.md discovery, no auto-memory, no hooks, no plugins),
`--strict-mcp-config`, a fresh directory per run, no shell tool. Auth comes from a
minimal settings file built from your ~/.claude/settings.json (API key helper, gateway and
model variables only) in a private temp directory; it is never printed or committed.

    bench/run.py --repeats 3 --model haiku --jobs 4
"""

import argparse
import concurrent.futures as cf
import datetime as dt
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import memories  # noqa: E402
import tasks  # noqa: E402

CONDITIONS = ["none", "mcp", "installed", "installed+keywords"]
DB_FOR = {"mcp": "plain", "installed": "plain", "installed+keywords": "keywords"}
MEMDAG_TOOLS = ["search_memory", "get_memory", "list_memories", "search_vector"]
SETTINGS_KEYS = ("apiKeyHelper", "provider")
ENV_KEYS = (
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
    "REQUESTS_CA_BUNDLE",
)


def bench_settings(private: Path) -> Path:
    """Only what `claude --bare` needs to authenticate, from the user's settings."""
    user = Path.home() / ".claude" / "settings.json"
    src = json.loads(user.read_text()) if user.exists() else {}
    out = {k: src[k] for k in SETTINGS_KEYS if k in src}
    env = {k: v for k, v in src.get("env", {}).items() if k in ENV_KEYS}
    if env:
        out["env"] = env
    path = private / "settings.json"
    path.write_text(json.dumps(out))
    path.chmod(0o600)
    return path


def memdag(binary: Path, db: Path, *args: str, stdin: str | None = None) -> str:
    env = {**os.environ, "MEMDAG_DB": str(db), "MEMDAG_NO_AUTO_IMPORT": "1"}
    out = subprocess.run(
        [str(binary), *args], input=stdin, capture_output=True, text=True, env=env, cwd=db.parent, check=True
    )
    return out.stdout


def build_dbs(binary: Path, work: Path, settings: Path, model: str) -> dict[str, Path]:
    """The memory databases: plain, and with worker keywords."""
    plain = work / "db-memdag" / "memdag.db"
    plain.parent.mkdir(parents=True)
    for m in memories.RULES:
        memdag(binary, plain, "record", "--id", m["id"], "-k", m["kind"], "-t", m["title"], "-b", m["body"], "--tags", m["tags"])
    for kind, title, body in memories.DISTRACTORS:
        memdag(binary, plain, "record", "-k", kind, "-t", title, "-b", body)

    expanded = work / "db-keywords" / "memdag.db"
    expanded.parent.mkdir(parents=True)
    shutil.copy(plain, expanded)
    for _ in range(5):
        batch = memdag(binary, expanded, "work", "next", "expand", "--limit", "25")
        if not batch.strip():
            break
        reply = subprocess.run(
            claude_cmd(settings, model) + ["--output-format", "text", "-p", batch],
            capture_output=True, text=True, cwd=expanded.parent, timeout=300,
        )
        print(memdag(binary, expanded, "work", "submit", "--task", "expand", stdin=reply.stdout).splitlines()[0], file=sys.stderr)
    return {"plain": plain, "keywords": expanded}


def instructions_block(binary: Path) -> str:
    """The block `memdag install` writes to CLAUDE.md, from the binary itself."""
    out = subprocess.run(
        [str(binary), "install", "--print-config", "--target", "claude-code", "--global",
         "--no-mcp", "--no-worker", "--no-skill", "--no-permissions"],
        capture_output=True, text=True, check=True,
    ).stdout
    start, end = out.index("<!-- MEMDAG_START -->"), out.index("<!-- MEMDAG_END -->")
    return out[start:end + len("<!-- MEMDAG_END -->")]


def claude_cmd(settings: Path, model: str) -> list[str]:
    return [
        "claude", "--bare", "--settings", str(settings), "--model", model,
        "--no-session-persistence", "--strict-mcp-config",
    ]


def run_one(args, settings: Path, dbs: dict, binary: Path, task, cond: str, rep: int) -> dict:
    name, prompt, rule, grader = task
    work = Path(tempfile.mkdtemp(prefix=f"bench-{name}-{cond}-", dir=args.workdir))
    shutil.copytree(HERE / "fixture", work / "repo")
    repo = work / "repo"
    cmd = claude_cmd(settings, args.model) + [
        "--output-format", "stream-json", "--verbose",
        "--permission-mode", "acceptEdits",
        "--max-turns", str(args.max_turns),
        "--max-budget-usd", str(args.max_budget),
        "--allowedTools", "Read", "Edit", "Write", "Glob", "Grep",
    ]
    if cond != "none":
        db = work / "memdag.db"
        shutil.copy(dbs[DB_FOR[cond]], db)
        cfg = work / "mcp.json"
        cfg.write_text(json.dumps({"mcpServers": {"memdag": {
            "type": "stdio", "command": str(binary), "args": ["serve"],
            "env": {"MEMDAG_DB": str(db), "MEMDAG_NO_AUTO_IMPORT": "1"},
        }}}))
        cmd += [f"mcp__memdag__{t}" for t in MEMDAG_TOOLS] + ["--mcp-config", str(cfg)]
        if cond.startswith("installed"):
            cmd += ["--append-system-prompt", args.instructions]
    cmd += ["-p", prompt + "\n\nMake the change in this repository; don't ask questions."]

    started = time.time()
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, cwd=repo, timeout=args.timeout)
        lines = out.stdout.splitlines()
    except subprocess.TimeoutExpired as e:
        lines = (e.stdout or b"").decode(errors="replace").splitlines() if isinstance(e.stdout, bytes) else (e.stdout or "").splitlines()
    wall = time.time() - started

    tool_calls: dict[str, int] = {}
    result = {}
    for line in lines:
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "assistant":
            for block in ev.get("message", {}).get("content", []):
                if block.get("type") == "tool_use":
                    tool_calls[block["name"]] = tool_calls.get(block["name"], 0) + 1
        elif ev.get("type") == "result":
            result = ev
    passed, detail = grader(repo)
    usage = result.get("usage", {})
    row = {
        "task": name, "rule": rule, "condition": cond, "repeat": rep, "model": args.model,
        "passed": passed, "detail": detail,
        "cost_usd": result.get("total_cost_usd"),
        "turns": result.get("num_turns"),
        "duration_s": round(result.get("duration_ms", wall * 1000) / 1000, 1),
        "input_tokens": usage.get("input_tokens", 0) + usage.get("cache_read_input_tokens", 0) + usage.get("cache_creation_input_tokens", 0),
        "output_tokens": usage.get("output_tokens", 0),
        "memdag_calls": sum(v for k, v in tool_calls.items() if k.startswith("mcp__memdag__")),
        "recalled": rule in json.dumps(lines_with_tool_results(lines)),
        "error": result.get("subtype") if result.get("is_error") else (None if result else "no result"),
    }
    if not args.keep:
        shutil.rmtree(work, ignore_errors=True)
    return row


def lines_with_tool_results(lines: list[str]) -> list:
    """memdag tool results the agent received (to see whether the rule was in front of it)."""
    found = []
    for line in lines:
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "user":
            for block in ev.get("message", {}).get("content", []) or []:
                if isinstance(block, dict) and block.get("type") == "tool_result":
                    found.append(block.get("content"))
    return found


def summarize(rows: list[dict], out: Path) -> str:
    def stats(rs):
        costs = [r["cost_usd"] or 0 for r in rs]
        return {
            "runs": len(rs),
            "pass": sum(r["passed"] for r in rs) / len(rs) if rs else 0,
            "corrections": sum(not r["passed"] for r in rs) / len(rs) if rs else 0,
            "cost": statistics.mean(costs) if rs else 0,
            "turns": statistics.mean(r["turns"] or 0 for r in rs) if rs else 0,
            "secs": statistics.mean(r["duration_s"] for r in rs) if rs else 0,
            "tokens": statistics.mean(r["input_tokens"] + r["output_tokens"] for r in rs) if rs else 0,
            "memdag": statistics.mean(r["memdag_calls"] for r in rs) if rs else 0,
            "recalled": sum(r.get("recalled", False) for r in rs) / len(rs) if rs else 0,
        }

    lines = ["| Condition | Runs | Pass rate | Corrections / task | Cost / task | Tokens / task | Turns | Seconds | memdag calls | Rule recalled |",
             "|---|---|---|---|---|---|---|---|---|---|"]
    for c in CONDITIONS:
        rs = [r for r in rows if r["condition"] == c]
        if not rs:
            continue
        s = stats(rs)
        lines.append(f"| {c} | {s['runs']} | {s['pass']:.0%} | {s['corrections']:.2f} | ${s['cost']:.4f} | {s['tokens']:,.0f} | {s['turns']:.1f} | {s['secs']:.0f} | {s['memdag']:.1f} | {s['recalled']:.0%} |")
    lines += ["", "| Task (rule) | " + " | ".join(CONDITIONS) + " |", "|---|" + "---|" * len(CONDITIONS)]
    for name, _, rule, _ in tasks.TASKS:
        cells = []
        for c in CONDITIONS:
            rs = [r for r in rows if r["task"] == name and r["condition"] == c]
            cells.append(f"{sum(r['passed'] for r in rs)}/{len(rs)}" if rs else "-")
        lines.append(f"| {name} ({rule}) | " + " | ".join(cells) + " |")
    text = "\n".join(lines) + "\n"
    out.write_text(text)
    return text


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--model", default="haiku")
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--conditions", default=",".join(CONDITIONS))
    ap.add_argument("--tasks", default="", help="comma-separated task names (default: all)")
    ap.add_argument("--memdag", default=str(HERE.parent / "target" / "release" / "memdag"))
    ap.add_argument("--max-turns", type=int, default=25)
    ap.add_argument("--max-budget", type=float, default=0.5)
    ap.add_argument("--timeout", type=int, default=420)
    ap.add_argument("--workdir", default=None, help="where run directories go (default: system temp)")
    ap.add_argument("--keep", action="store_true", help="keep run directories")
    args = ap.parse_args()

    binary = Path(args.memdag).resolve()
    if not binary.exists():
        sys.exit(f"memdag binary not found at {binary}; build it (cargo build --release) or pass --memdag")
    args.instructions = instructions_block(binary)
    conds = [c for c in args.conditions.split(",") if c]
    chosen = [t for t in tasks.TASKS if not args.tasks or t[0] in args.tasks.split(",")]

    private = Path(tempfile.mkdtemp(prefix="memdag-bench-"))
    private.chmod(0o700)
    try:
        settings = bench_settings(private)
        dbs = build_dbs(binary, private, settings, args.model) if set(conds) - {"none"} else {}
        jobs = [(t, c, r) for r in range(args.repeats) for t in chosen for c in conds]
        rows = []
        with cf.ThreadPoolExecutor(args.jobs) as ex:
            futs = {ex.submit(run_one, args, settings, dbs, binary, t, c, r): (t[0], c, r) for t, c, r in jobs}
            for f in cf.as_completed(futs):
                row = f.result()
                rows.append(row)
                print(f"{row['task']:<11} {row['condition']:<9} #{row['repeat']} {'PASS' if row['passed'] else 'FAIL'}  ${row['cost_usd'] or 0:.4f}  memdag={row['memdag_calls']}  {row['detail'][:70]}", file=sys.stderr)
    finally:
        shutil.rmtree(private, ignore_errors=True)

    stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    results = HERE / "results"
    results.mkdir(exist_ok=True)
    raw = results / f"{stamp}-{args.model}.jsonl"
    raw.write_text("".join(json.dumps(r) + "\n" for r in sorted(rows, key=lambda r: (r["task"], r["condition"], r["repeat"]))))
    print(summarize(rows, results / f"{stamp}-{args.model}.md"))
    print(f"raw results: {raw}", file=sys.stderr)


if __name__ == "__main__":
    main()
