#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Compare CLI workloads in isolated synthetic provider stores."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import statistics
import subprocess
import tempfile
import time


def hermes_fixture(root, count):
    root.mkdir()
    with sqlite3.connect(root / "state.db") as connection:
        connection.executescript("""
            CREATE TABLE schema_version (version INTEGER NOT NULL);
            INSERT INTO schema_version VALUES (23);
            CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, title TEXT, cwd TEXT,
              git_branch TEXT, started_at REAL NOT NULL, ended_at REAL, message_count INTEGER DEFAULT 0,
              model TEXT, model_config TEXT, parent_session_id TEXT, input_tokens INTEGER DEFAULT 0,
              output_tokens INTEGER DEFAULT 0, cache_read_tokens INTEGER DEFAULT 0,
              cache_write_tokens INTEGER DEFAULT 0, reasoning_tokens INTEGER DEFAULT 0,
              archived INTEGER DEFAULT 0);
            CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
              role TEXT NOT NULL, content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT,
              effect_disposition TEXT, timestamp REAL NOT NULL, finish_reason TEXT, reasoning TEXT,
              reasoning_content TEXT, active INTEGER NOT NULL DEFAULT 1);
            CREATE TABLE synthetic_padding (payload BLOB);
        """)
        for index in range(count):
            session = f"synthetic-{index}"
            connection.execute("INSERT INTO sessions (id, source, cwd, started_at, message_count) VALUES (?, 'cli', '/workspace/synthetic', 100, 2)", (session,))
            for role, content, timestamp in [("user", f"synthetic request {index}", 101), ("assistant", f"lattice checkpoint {index}", 102)]:
                connection.execute("INSERT INTO messages (session_id, role, content, timestamp) VALUES (?, ?, ?, ?)", (session, role, content, timestamp))
        connection.execute("INSERT INTO synthetic_padding VALUES (zeroblob(?))", (32 * 1024 * 1024,))


def claude_fixture(root, workspace, count):
    directory = root / "projects/synthetic"
    directory.mkdir(parents=True)
    for index in range(count):
        session = f"11111111-1111-4111-8111-{index:012d}"
        records = [
            {"type": "user", "uuid": f"user-{index}", "sessionId": session,
             "cwd": str(workspace), "timestamp": "2026-01-01T00:00:00Z",
             "message": {"role": "user", "content": f"Synthetic request {index}"}},
            {"type": "assistant", "uuid": f"answer-{index}", "sessionId": session,
             "cwd": str(workspace), "timestamp": "2026-01-01T00:00:01Z",
             "message": {"role": "assistant", "content": "lattice checkpoint " + "synthetic context " * 100}},
        ]
        (directory / f"{session}.jsonl").write_text("\n".join(json.dumps(record) for record in records) + "\n")


def environment(root, state):
    # Start with only process prerequisites. No provider settings or credentials are inherited.
    result = {key: os.environ[key] for key in ("SystemRoot", "WINDIR", "TMPDIR", "TEMP", "TMP") if key in os.environ}
    result.update(HOME=str(root / "home"), USERPROFILE=str(root / "home"),
        PATH=os.defpath, OMNISESSION_HOME=str(state), OMNI_NO_UPDATE_CHECK="1",
        CLAUDE_CONFIG_DIR=str(root / "claude"), CODEX_HOME=str(root / "codex"),
        HERMES_HOME=str(root / "hermes"), GROK_HOME=str(root / "grok"),
        ANTIGRAVITY_CLI_HOME=str(root / "antigravity"), CURSOR_IDE_HOME=str(root / "cursor-ide"),
        CURSOR_AGENT_HOME=str(root / "cursor-agent"), CURSOR_CONFIG_DIR=str(root / "cursor"),
        PI_CODING_AGENT_DIR=str(root / "pi"), PI_CODING_AGENT_SESSION_DIR=str(root / "pi/sessions"),
        OMP_SESSION_DIR=str(root / "omp"), OMP_PROFILE="")
    for key in ("CONFIG", "DATA", "STATE", "CACHE"):
        result[f"XDG_{key}_HOME"] = str(root / "xdg" / key.lower())
    for provider in ("CLAUDE", "CODEX", "OPENCODE", "GROK", "HERMES", "ANTIGRAVITY", "PI", "OMP", "CURSOR_AGENT", "CURSOR_IDE"):
        result[f"OMNI_{provider}_BIN"] = str(root / "missing" / provider.lower())
    return result


def run(binary, arguments, root, state):
    started = time.perf_counter()
    output = subprocess.run([str(binary), "--json", *arguments], cwd=root / "workspace",
        env=environment(root, state), capture_output=True, text=True, check=True, timeout=300)
    return (time.perf_counter() - started) * 1000, json.loads(output.stdout)


def source_digest(root, provider):
    digest = hashlib.sha256()
    for path in sorted((root / provider).rglob("*")):
        if path.is_file():
            digest.update(str(path.relative_to(root)).encode())
            with path.open("rb") as source:
                for chunk in iter(lambda: source.read(1024 * 1024), b""):
                    digest.update(chunk)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("a", type=Path, help="Baseline release binary")
    parser.add_argument("b", type=Path, help="Candidate release binary")
    parser.add_argument("--workload", choices=("hermes", "cached"), default="hermes")
    parser.add_argument("--sessions", type=int, help="Default: 40 Hermes or 1,000 Claude sessions")
    parser.add_argument("--repeats", type=int, default=7)
    args = parser.parse_args()
    count = args.sessions if args.sessions is not None else (40 if args.workload == "hermes" else 1000)
    if count < 1 or args.repeats < 1:
        parser.error("Session and repeat counts must be positive.")
    binaries = {"A": args.a.resolve(strict=True), "B": args.b.resolve(strict=True)}
    samples = {"A": [], "B": []}
    with tempfile.TemporaryDirectory(prefix="omni-benchmark-") as directory:
        root = Path(directory).resolve()
        (root / "workspace").mkdir()
        (root / "home").mkdir()
        provider = "hermes" if args.workload == "hermes" else "claude"
        if provider == "hermes":
            hermes_fixture(root / provider, count)
        else:
            claude_fixture(root / provider, root / "workspace", count)
        before = source_digest(root, provider)
        if provider == "claude":
            _, seeded = run(binaries["B"], ["index", "--provider", provider], root, root / "state")
            assert seeded["indexed"] == count and seeded["failed"] == 0, seeded
        expected_sessions = None
        for trial in range(args.repeats):
            for label in (("A", "B") if trial % 2 == 0 else ("B", "A")):
                if provider == "hermes":
                    state = root / f"state-{label}-{trial}"
                    arguments = ["index", "--provider", provider]
                else:
                    state = root / "state"
                    arguments = ["search", "lattice", "--provider", provider, "--limit", "10",
                        "--no-index" if label == "A" else "--cached"]
                elapsed, output = run(binaries[label], arguments, root, state)
                if provider == "hermes":
                    assert output["indexed"] == count and output["failed"] == 0, output
                else:
                    sessions = [result["session"] for result in output["results"]]
                    assert len(sessions) == min(count, 10), output
                    if expected_sessions is None:
                        expected_sessions = sessions
                    assert sessions == expected_sessions, output
                samples[label].append(elapsed)
        assert source_digest(root, provider) == before, "Provider source changed."
    result = {"workload": args.workload, "sessions": count, "repeats": args.repeats,
        "source_unchanged": True, "binaries": {label: hashlib.sha256(path.read_bytes()).hexdigest() for label, path in binaries.items()}}
    result["timings"] = {label: {"median_ms": round(statistics.median(values), 3),
        "min_ms": round(min(values), 3), "max_ms": round(max(values), 3),
        "samples_ms": [round(value, 3) for value in values]} for label, values in samples.items()}
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
