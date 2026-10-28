#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Compare session collection and search with synthetic provider stores only."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import signal
import statistics
import subprocess
import tempfile
import time
import uuid


EPOCH = "2026-01-02T03:04:05Z"
WORKSPACE = "synthetic-workspace"
TIME = Path("/usr/bin/time")


def session_id(seed: str) -> str:
    return str(uuid.uuid5(uuid.NAMESPACE_URL, f"omnisession-synthetic-benchmark:{seed}"))


def make_claude_session(path: Path, identity: str, workspace: Path, turns: int, padding: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as output:
        output.write(json.dumps({"type": "summary", "summary": f"Synthetic benchmark archive {identity}"}, separators=(",", ":")) + "\n")
        for turn in range(turns):
            prompt = f"Synthetic benchmark request {turn}"
            answer = f"Synthetic benchmark response {turn} {padding}"
            if turn == 0:
                prompt += " quartzneedle"
            json_line_to(output, {
                "type": "user", "uuid": f"user-{turn}", "sessionId": identity,
                "timestamp": EPOCH, "cwd": str(workspace), "display": prompt,
                "message": {"role": "user", "content": prompt},
            })
            json_line_to(output, {
                "type": "assistant", "uuid": f"assistant-{turn}", "sessionId": identity,
                "timestamp": EPOCH, "cwd": str(workspace),
                "message": {"role": "assistant", "content": [{"type": "text", "text": answer}]},
            })


def json_line_to(output, value: object) -> None:
    output.write(json.dumps(value, separators=(",", ":"), sort_keys=True))
    output.write("\n")


def make_grok_session(root: Path, identity: str, workspace: Path, turns: int, padding: str) -> Path:
    directory = root / "synthetic-project" / identity
    directory.mkdir(parents=True, exist_ok=True)
    summary = {
        "id": identity, "cwd": str(workspace), "num_messages": turns * 2,
        "updated_at": EPOCH, "title": "Synthetic Grok benchmark",
    }
    (directory / "summary.json").write_text(json.dumps(summary, separators=(",", ":")), encoding="utf-8")
    updates = directory / "updates.jsonl"
    with updates.open("w", encoding="utf-8") as output:
        for turn in range(turns):
            prompt = f"Synthetic benchmark request {turn}"
            answer = f"Synthetic benchmark response {turn} {padding}"
            if turn == 0:
                answer += " quartzneedle"
            for kind, text in (("user_message_chunk", prompt), ("agent_message_chunk", answer)):
                json_line_to(output, {
                    "timestamp": EPOCH,
                    "params": {"update": {"sessionUpdate": kind,
                        "content": {"type": "text", "text": text}}},
                })
    return directory / "updates.jsonl"


def make_pi_session(
    path: Path,
    identity: str,
    workspace: Path,
    turns: int,
    padding: str,
    omp: bool,
    branched: bool,
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    current = None
    with path.open("w", encoding="utf-8") as output:
        if omp:
            json_line_to(output, {"type": "title", "title": "Synthetic OMP branch and compaction"})
        json_line_to(output, {
            "type": "session", "version": 3, "id": identity,
            "timestamp": EPOCH, "cwd": str(workspace),
        })
        for turn in range(turns):
            if branched and turn == turns // 2:
                abandoned = f"abandoned-{turn}"
                json_line_to(output, {
                    "type": "message", "id": abandoned, "parentId": current,
                    "timestamp": EPOCH,
                    "message": {"role": "user", "content": "Synthetic abandoned branch"},
                })
                branch = f"branch-summary-{turn}"
                json_line_to(output, {
                    "type": "branch_summary", "id": branch, "parentId": current,
                    "fromId": abandoned, "summary": "Synthetic branch summary",
                    "timestamp": EPOCH,
                })
                compact = f"compaction-{turn}"
                json_line_to(output, {
                    "type": "compaction", "id": compact, "parentId": branch,
                    "firstKeptEntryId": current, "tokensBefore": 4000,
                    "summary": "Synthetic compaction summary", "timestamp": EPOCH,
                })
                current = compact
            user_id = f"user-{turn}"
            json_line_to(output, {
                "type": "message", "id": user_id, "parentId": current,
                "timestamp": EPOCH,
                "message": {"role": "user", "content": f"Synthetic benchmark request {turn}"},
            })
            assistant_id = f"assistant-{turn}"
            json_line_to(output, {
                "type": "message", "id": assistant_id, "parentId": user_id,
                "timestamp": EPOCH,
                "message": {
                    "role": "assistant", "timestamp": 0, "api": "synthetic",
                    "provider": "synthetic", "model": "synthetic", "usage": {},
                    "stopReason": "stop", "content": [{"type": "text", "text": f"Synthetic answer {turn} {padding}"}],
                },
            })
            current = assistant_id


def codex_fixtures(root: Path, count: int, decoys: int, need_wide: bool, need_mixed: bool) -> dict[str, Path]:
    directories = {}
    if need_wide:
        directories["wide"] = root / "codex-wide" / "sessions" / "2026" / "10" / "07"
    if need_mixed:
        directories["mixed"] = root / "codex-mixed" / "sessions" / "2026" / "10" / "07"
    for directory in directories.values():
        directory.mkdir(parents=True, exist_ok=True)
    for index in range(count):
        identity = session_id(f"codex-{index}")
        record = {
            "timestamp": EPOCH, "type": "session_meta",
            "payload": {"id": identity, "cwd": "/synthetic/workspace", "git": {"branch": "fixture"}},
        }
        data = json.dumps(record, separators=(",", ":")) + "\n"
        filename = f"rollout-{index:08d}-{identity}.jsonl"
        for directory in directories.values():
            path = directory / filename
            path.write_text(data, encoding="utf-8")
            os.utime(path, (1_700_000_000, 1_700_000_000))
    if need_mixed:
        for index in range(decoys):
            path = directories["mixed"] / f"unrelated-{index:08d}.tmp"
            path.write_bytes(b"synthetic decoy\n")
            os.utime(path, (1_700_000_000, 1_700_000_000))
    return {name: path.parents[3] for name, path in directories.items()}


def search_fixtures(root: Path, workspace: Path, count: int) -> Path:
    config = root / f"search-{count}" / "config"
    projects = config / "projects" / "synthetic"
    projects.mkdir(parents=True, exist_ok=True)
    for index in range(count):
        identity = session_id(f"search-{count}-{index}")
        title = f"Synthetic benchmark archive {index}"
        if index < 3:
            title += " quartzneedle"
        answer = f"Synthetic response {index}"
        if index % 7 == 0:
            answer += " quartzneedle"
        path = projects / f"{identity}.jsonl"
        with path.open("w", encoding="utf-8") as output:
            json_line_to(output, {"type": "summary", "summary": title})
            json_line_to(output, {
                "type": "user", "uuid": f"user-{index}", "sessionId": identity,
                "timestamp": EPOCH, "cwd": str(workspace), "display": title,
                "message": {"role": "user", "content": title},
            })
            json_line_to(output, {
                "type": "assistant", "uuid": f"assistant-{index}", "sessionId": identity,
                "timestamp": EPOCH, "cwd": str(workspace),
                "message": {"role": "assistant", "content": [{"type": "text", "text": answer}]},
            })
    return config


def tree_digest(roots: list[Path]) -> str:
    digest = hashlib.sha256()
    files = sorted(path for root in roots for path in root.rglob("*") if path.is_file())
    for path in files:
        root = next(root for root in roots if path.is_relative_to(root))
        relative = path.relative_to(root).as_posix().encode()
        digest.update(len(relative).to_bytes(4, "big"))
        digest.update(relative)
        with path.open("rb") as source:
            for block in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(block)
    return digest.hexdigest()


def isolated_environment(temp: Path, workspace: Path, state: Path, claude_config: Path | None = None) -> dict[str, str]:
    home = temp / "isolated-home"
    home.mkdir(exist_ok=True)
    env = {
        "HOME": str(home), "USERPROFILE": str(home), "PATH": os.defpath,
        "TMPDIR": str(temp), "TEMP": str(temp), "TMP": str(temp),
        "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "TERM": "dumb", "NO_COLOR": "1",
        "OMNISESSION_HOME": str(state), "OMNI_NO_UPDATE_CHECK": "1",
        "CLAUDE_CONFIG_DIR": str(claude_config or home / "claude"),
        "CODEX_HOME": str(home / "codex"), "GROK_HOME": str(home / "grok"),
        "ANTIGRAVITY_CLI_HOME": str(home / "antigravity-cli"),
        "ANTIGRAVITY_IDE_HOME": str(home / "antigravity-ide"),
        "ANTIGRAVITY_DESKTOP_HOME": str(home / "antigravity-desktop"),
        "ANTIGRAVITY_IDE_USER_DATA_DIR": str(home / "antigravity-ide-user-data"),
        "PI_CODING_AGENT_DIR": str(home / "pi"),
        "PI_CODING_AGENT_SESSION_DIR": str(home / "pi" / "sessions"),
        "OMP_SESSION_DIR": str(home / "omp"), "HERMES_HOME": str(home / "hermes"),
    }
    for name in ("CONFIG", "DATA", "STATE", "CACHE"):
        env[f"XDG_{name}_HOME"] = str(home / "xdg" / name.lower())
    for provider in ("CLAUDE", "CODEX", "OPENCODE", "GROK", "HERMES", "ANTIGRAVITY", "PI", "OMP", "CURSOR_AGENT", "CURSOR_IDE"):
        env[f"OMNI_{provider}_BIN"] = str(home / "missing-provider" / provider.lower())
    workspace.mkdir(parents=True, exist_ok=True)
    state.mkdir(parents=True, exist_ok=True)
    return env


class Runner:
    def __init__(self, temp: Path, timeout: int, rss_limit_kib: int):
        self.temp = temp
        self.timeout = timeout
        self.rss_limit_kib = rss_limit_kib
        self.sequence = 0

    def invoke(self, command: list[str], env: dict[str, str], cwd: Path) -> tuple[dict, dict]:
        self.sequence += 1
        time_file = self.temp / f"rss-{self.sequence}.txt"
        started = time.perf_counter()
        process = subprocess.Popen(
            [str(TIME), "-f", "%M", "-o", str(time_file), "--", *command],
            cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, start_new_session=True,
        )
        try:
            stdout, stderr = process.communicate(timeout=self.timeout)
        except subprocess.TimeoutExpired as error:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate()
            raise RuntimeError(f"synthetic benchmark process exceeded {self.timeout}s") from error
        wall_ms = (time.perf_counter() - started) * 1000
        if process.returncode:
            error_hash = hashlib.sha256(stderr).hexdigest()
            raise RuntimeError(f"synthetic benchmark process exited {process.returncode}; stderr_sha256={error_hash}")
        try:
            result = json.loads(stdout)
            rss_kib = int(time_file.read_text(encoding="ascii").strip())
        except (json.JSONDecodeError, OSError, ValueError) as error:
            raise RuntimeError(f"synthetic benchmark emitted invalid JSON or timing data: {type(error).__name__}") from error
        if rss_kib > self.rss_limit_kib:
            raise RuntimeError(f"synthetic benchmark exceeded the {self.rss_limit_kib} KiB RSS bound")
        if wall_ms > self.timeout * 1000:
            raise RuntimeError(f"synthetic benchmark exceeded the {self.timeout}s wall bound")
        sample = {"wall_ms": round(wall_ms, 3), "peak_rss_kib": rss_kib}
        if isinstance(result, dict) and "elapsed_ns" in result:
            sample["operation_ns"] = result["elapsed_ns"]
        return result, sample


def canonical_hash(value: object) -> str:
    data = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(data).hexdigest()


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, math.ceil(len(ordered) * fraction) - 1))
    return ordered[index]


def summarize_samples(samples: list[dict], key: str) -> dict | None:
    values = [sample[key] for sample in samples if key in sample]
    if not values:
        return None
    return {
        "p50": round(statistics.median(values), 3),
        "p95": round(percentile(values, 0.95), 3),
    }


def callgrind_instruction_count(profile_path: Path) -> int:
    with profile_path.open(encoding="utf-8", errors="replace") as profile:
        for line in profile:
            if line.startswith(("summary:", "totals:")):
                counts = line.partition(":")[2].split()
                if not counts:
                    raise RuntimeError("Callgrind profile has an empty instruction count record")
                try:
                    instruction_count = int(counts[0])
                except ValueError as error:
                    raise RuntimeError("Callgrind profile has an invalid instruction count record") from error
                if instruction_count <= 0:
                    raise RuntimeError(
                        "Callgrind profile has no instructions collected; check --profile-function "
                        "against a symbol in this build or omit the filter for whole-process collection"
                    )
                return instruction_count
    raise RuntimeError("Callgrind profile is missing a summary or totals instruction count record")


def available_cpu_affinity() -> list[int] | None:
    get_affinity = getattr(os, "sched_getaffinity", None)
    if get_affinity is None:
        return None
    try:
        return sorted(get_affinity(0))
    except OSError:
        return None


def resolve_profiler_lib(value: str | None) -> Path | None:
    if not value:
        return None
    try:
        profiler_lib = Path(value).resolve(strict=True)
    except (OSError, RuntimeError) as error:
        raise ValueError("VALGRIND_LIB must point to an existing profiler library directory") from error
    if not profiler_lib.is_dir():
        raise ValueError("VALGRIND_LIB must point to an existing profiler library directory")
    return profiler_lib


def compare_case(
    runner: Runner,
    name: str,
    commands: dict[str, list[str]],
    environments: dict[str, dict[str, str]],
    workspace: Path,
    repeats: int,
    warmups: int,
    expectation: dict,
    input_bytes: int,
) -> dict:
    expected = None
    samples = []
    for trial in range(warmups + repeats):
        order = ("baseline", "candidate") if trial % 2 == 0 else ("candidate", "baseline")
        current = {}
        for variant in order:
            result, metrics = runner.invoke(commands[variant], environments[variant], workspace)
            stable = {key: value for key, value in result.items() if key != "elapsed_ns"}
            candidates = result.get("index", {}).get("candidates")
            actual_count = result.get("count", candidates)
            if actual_count is None or actual_count < expectation.get("min_count", 1):
                raise RuntimeError(f"{name} returned fewer than the expected synthetic records")
            if "count" in expectation and actual_count != expectation["count"]:
                raise RuntimeError(f"{name} returned an unexpected synthetic record count")
            if "hits" in expectation and len(result.get("results", [])) != expectation["hits"]:
                raise RuntimeError(f"{name} returned an unexpected synthetic search result count")
            if "has_more" in expectation and result.get("has_more") != expectation["has_more"]:
                raise RuntimeError(f"{name} returned an unexpected has_more value")
            if expected is None:
                expected = stable
            elif canonical_hash(stable) != canonical_hash(expected):
                raise RuntimeError(f"{name} output differs between baseline and candidate")
            current[variant] = (result, metrics)
        if trial >= warmups:
            for variant in order:
                result, metrics = current[variant]
                samples.append({"trial": trial - warmups + 1, "variant": variant, **metrics})
    medians = {}
    percentiles = {}
    for variant in ("baseline", "candidate"):
        selected = [sample for sample in samples if sample["variant"] == variant]
        medians[variant] = {
            "wall_ms": round(statistics.median(sample["wall_ms"] for sample in selected), 3),
            "peak_rss_kib": int(statistics.median(sample["peak_rss_kib"] for sample in selected)),
        }
        percentiles[variant] = {
            "wall_ms": summarize_samples(selected, "wall_ms"),
            "peak_rss_kib": summarize_samples(selected, "peak_rss_kib"),
        }
        if "operation_ns" in selected[0]:
            medians[variant]["operation_ns"] = int(statistics.median(sample["operation_ns"] for sample in selected))
            percentiles[variant]["operation_ns"] = summarize_samples(selected, "operation_ns")
    return {
        "name": name, "result_sha256": canonical_hash(expected),
        "snapshot_sha256": expected.get("sha256"),
        "count": expected.get("count", expected.get("index", {}).get("candidates", len(expected.get("results", [])))),
        "input_bytes": input_bytes, "has_more": expected.get("has_more"),
        "medians": medians, "percentiles": percentiles, "samples": samples,
    }


def profile_case(
    name: str,
    profiler: Path,
    tool: str,
    function_pattern: str | None,
    profiler_lib: Path | None,
    output_dir: Path,
    command: list[str],
    environment: dict[str, str],
    cwd: Path,
    expected_hash: str,
    expectation: dict,
    timeout: int,
    rss_limit_kib: int,
) -> dict:
    output_path = output_dir / f"{name}.{tool}"
    if output_path.exists():
        raise RuntimeError(f"profile output already exists: {output_path.name}")
    if tool == "callgrind":
        profiler_args = [
            f"--callgrind-out-file={output_path}",
            "--tool=callgrind",
            "--error-exitcode=97",
        ]
        if function_pattern is not None:
            profiler_args.extend(("--collect-atstart=no", f"--toggle-collect={function_pattern}"))
    else:
        profiler_args = [
            f"--massif-out-file={output_path}",
            "--tool=massif",
            "--time-unit=i",
            "--error-exitcode=97",
        ]
    # VALGRIND_LIB is the one opt-in host setting needed by an extracted profiler.
    # Provider settings and credentials stay excluded by isolated_environment().
    profile_environment = environment.copy()
    if profiler_lib:
        profile_environment["VALGRIND_LIB"] = str(profiler_lib)
    time_file = output_dir / f"{name}.rss.txt"
    started = time.perf_counter()
    process = subprocess.Popen(
        [str(TIME), "-f", "%M", "-o", str(time_file), "--", str(profiler),
         *profiler_args, "--", *command],
        cwd=cwd,
        env=profile_environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired as error:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.communicate()
        raise RuntimeError(f"{name} profile exceeded {timeout}s") from error
    wall_ms = (time.perf_counter() - started) * 1000
    if process.returncode:
        error_hash = hashlib.sha256(stderr).hexdigest()
        raise RuntimeError(
            f"{name} profiler exited {process.returncode}; stderr_sha256={error_hash}"
        )
    try:
        result = json.loads(stdout)
        rss_kib = int(time_file.read_text(encoding="ascii").strip())
    except (json.JSONDecodeError, OSError, ValueError) as error:
        raise RuntimeError(f"{name} profile emitted invalid JSON or timing data") from error
    stable_result = {key: value for key, value in result.items() if key != "elapsed_ns"}
    actual_count = result.get("count", result.get("index", {}).get("candidates"))
    if actual_count is None or actual_count < expectation.get("min_count", 1):
        raise RuntimeError(f"{name} profile returned fewer than the expected synthetic records")
    if "count" in expectation and actual_count != expectation["count"]:
        raise RuntimeError(f"{name} profile returned an unexpected synthetic record count")
    if "hits" in expectation and len(result.get("results", [])) != expectation["hits"]:
        raise RuntimeError(f"{name} profile returned an unexpected search result count")
    if "has_more" in expectation and result.get("has_more") != expectation["has_more"]:
        raise RuntimeError(f"{name} profile returned an unexpected has_more value")
    if canonical_hash(stable_result) != expected_hash:
        raise RuntimeError(f"{name} profiler run changed the candidate's stable output")
    if not output_path.is_file() or output_path.stat().st_size == 0:
        raise RuntimeError(f"{name} profiler did not produce a nonempty profile")
    instruction_count = None
    if tool == "callgrind":
        try:
            instruction_count = callgrind_instruction_count(output_path)
        except RuntimeError as error:
            raise RuntimeError(f"{name} {error}") from error
    if rss_kib > rss_limit_kib:
        raise RuntimeError(f"{name} profile exceeded the {rss_limit_kib} KiB RSS bound")
    profile_report = {
        "case": name,
        "tool": tool,
        "profile_file": output_path.name,
        "profile_bytes": output_path.stat().st_size,
        "stable_result_sha256": canonical_hash(stable_result),
        "wall_ms_instrumented": round(wall_ms, 3),
        "peak_rss_kib_instrumented": rss_kib,
        "timeout_seconds": timeout,
        "rss_limit_kib": rss_limit_kib,
    }
    if instruction_count is not None:
        profile_report["instruction_count"] = instruction_count
    return profile_report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-probe", type=Path, required=True)
    parser.add_argument("--candidate-probe", type=Path, required=True)
    parser.add_argument("--baseline-omni", type=Path, required=True)
    parser.add_argument("--candidate-omni", type=Path, required=True)
    parser.add_argument("--profile", choices=("smoke", "release"), default="smoke")
    parser.add_argument("--case", action="append", default=[], help="Run only named case(s); repeat this flag to select more")
    parser.add_argument("--repeats", type=int)
    parser.add_argument("--warmups", type=int)
    parser.add_argument("--output", type=Path, help="Write the content-free JSON report to this file")
    parser.add_argument("--profile-tool", choices=("callgrind", "massif"),
        help="Profile the candidate separately with Valgrind; select explicit --case workloads")
    parser.add_argument("--profile-function",
        help="Callgrind symbol pattern to collect (requires --profile-tool callgrind)")
    parser.add_argument("--profiler", help="Valgrind executable path or executable name on PATH")
    parser.add_argument("--profile-output-dir", type=Path,
        help="New or empty directory for Valgrind profiles and RSS measurements")
    parser.add_argument("--profile-timeout", type=int, default=600,
        help="Maximum time per instrumented workload in seconds (default: 600)")
    parser.add_argument("--profile-max-rss-mib", type=int, default=2048,
        help="Maximum RSS per instrumented workload in MiB (default: 2048)")
    args = parser.parse_args()
    variants = {
        "baseline": {"probe": args.baseline_probe.resolve(strict=True), "omni": args.baseline_omni.resolve(strict=True)},
        "candidate": {"probe": args.candidate_probe.resolve(strict=True), "omni": args.candidate_omni.resolve(strict=True)},
    }
    if not TIME.is_file():
        parser.error("Linux /usr/bin/time is required to record per-process peak RSS")
    if args.repeats is not None and args.repeats < 1:
        parser.error("--repeats must be positive")
    if args.warmups is not None and args.warmups < 0:
        parser.error("--warmups cannot be negative")
    profiler = None
    profiler_lib = None
    profile_output_dir = None
    if args.profile_tool:
        if not args.case:
            parser.error("--profile-tool requires one or more explicit --case selections")
        if not args.profiler or not args.profile_output_dir:
            parser.error("--profile-tool requires --profiler and --profile-output-dir")
        profiler_name = str(args.profiler)
        profiler_path = Path(profiler_name)
        if not profiler_path.is_absolute():
            located = shutil.which(profiler_name)
            if not located:
                parser.error("--profiler must resolve to an executable path")
            profiler_path = Path(located)
        try:
            profiler = profiler_path.resolve(strict=True)
        except OSError:
            parser.error("--profiler must resolve to an executable file")
        if not profiler.is_file() or not os.access(profiler, os.X_OK):
            parser.error("--profiler must resolve to an executable file")
        if args.profile_timeout < 1 or args.profile_timeout > 3600:
            parser.error("--profile-timeout must be between 1 and 3600 seconds")
        if args.profile_max_rss_mib < 128 or args.profile_max_rss_mib > 32768:
            parser.error("--profile-max-rss-mib must be between 128 and 32768")
        try:
            profiler_lib = resolve_profiler_lib(os.environ.get("VALGRIND_LIB"))
        except ValueError as error:
            parser.error(str(error))
        profile_output_dir = args.profile_output_dir.resolve()
        if profile_output_dir.exists() and not profile_output_dir.is_dir():
            parser.error("--profile-output-dir must be a directory")
        if profile_output_dir.is_dir() and any(profile_output_dir.iterdir()):
            parser.error("--profile-output-dir must be new or empty")
    elif args.profiler or args.profile_output_dir:
        parser.error("--profiler and --profile-output-dir require --profile-tool")
    if args.profile_function is not None:
        if not args.profile_function.strip():
            parser.error("--profile-function must be a nonempty symbol pattern")
        if args.profile_tool != "callgrind":
            parser.error("--profile-function requires --profile-tool callgrind")

    release = args.profile == "release"
    turns = 12_000 if release else 2_000
    pi_turns = 10_000 if release else 2_000
    pi_many_short_turns = 30_000 if release else 10_000
    codex_files = 40_000 if release else 12_050
    decoys = 20_000 if release else 2_000
    search_counts = (10_000,) if release else (1_000,)
    repeats = args.repeats or (5 if release else 2)
    warmups = args.warmups if args.warmups is not None else 1
    timeout = 600 if release else 120
    rss_limit_kib = (2 * 1024 * 1024) if release else (512 * 1024)
    search_queries = {
        "broad": ("benchmark", {"hits": 20, "has_more": True}),
        "and": ("benchmark archive", {"hits": 20, "has_more": True}),
        "metadata-and-conversation": ("quartzneedle", {"hits": 20, "has_more": True}),
        "mixed-clause": ('benchmark "Synthetic response"', {"hits": 20, "has_more": True}),
        "no-match": ("noresulttoken", {"hits": 0, "has_more": False}),
    }
    available = {"claude-long", "grok-long", "pi-linear", "pi-branch-compaction", "pi-many-short",
        "omp-linear", "omp-branch-compaction", "omp-many-short", "codex-wide", "codex-mixed"}
    available.update(f"search-{kind}-{count}" for kind in search_queries for count in search_counts)
    selected = set(args.case) or available
    unknown = selected - available
    if unknown:
        parser.error(f"unknown --case value(s): {', '.join(sorted(unknown))}")
    if profile_output_dir:
        if not profile_output_dir.exists():
            profile_output_dir.mkdir(mode=0o700, parents=True)

    with tempfile.TemporaryDirectory(prefix="omni-session-bench-") as temporary:
        root = Path(temporary)
        work = root / "workspace"
        work.mkdir()
        inputs = root / "inputs"
        inputs.mkdir()
        padding = "synthetic-payload " * (640 if release else 224)
        case_specs = {}
        search_roots = {}

        if "claude-long" in selected:
            provider_root = inputs / "claude" / "projects"
            identity = session_id("claude-long")
            source = provider_root / "synthetic" / f"{identity}.jsonl"
            make_claude_session(source, identity, work, turns, padding)
            case_specs["claude-long"] = ("read", "claude", provider_root, source, identity,
                {"count": turns * 2}, source.parent)

        if "grok-long" in selected:
            provider_root = inputs / "grok"
            identity = session_id("grok-long")
            source = make_grok_session(provider_root, identity, work, turns, padding)
            case_specs["grok-long"] = ("read", "grok", provider_root, source, identity,
                {"count": turns * 2}, source.parent)

        for name, provider in (("pi-linear", "pi"), ("pi-branch-compaction", "pi"),
            ("pi-many-short", "pi"), ("omp-linear", "omp"),
            ("omp-branch-compaction", "omp"), ("omp-many-short", "omp")):
            if name not in selected:
                continue
            provider_root = inputs / provider / name / "sessions"
            identity = f"{provider}-synthetic-{name}"
            source = provider_root / "--synthetic-workspace--" / f"2026-01-02T03-04-05-000Z_{identity}.jsonl"
            branch = name.endswith("branch-compaction")
            turns_for_case = pi_many_short_turns if name.endswith("many-short") else pi_turns
            padding_for_case = "" if name.endswith("many-short") else padding
            make_pi_session(source, identity, work, turns_for_case, padding_for_case,
                provider == "omp", branch)
            expected_events = 3 * turns_for_case if not branch else 3 * (turns_for_case - turns_for_case // 2) + 4
            case_specs[name] = ("read", provider, provider_root, source, identity,
                {"count": expected_events}, source.parent)

        codex_wanted = {name.removeprefix("codex-") for name in selected if name.startswith("codex-")}
        if codex_wanted:
            codex_roots = codex_fixtures(inputs, codex_files, decoys,
                "wide" in codex_wanted, "mixed" in codex_wanted)
            for shape, provider_root in codex_roots.items():
                case_specs[f"codex-{shape}"] = ("codex-list", None, provider_root, None, None,
                    {"count": 10_000}, provider_root)

        requested_search = {name for name in selected if name.startswith("search-")}
        requested_scales = {
            count for count in search_counts
            if any(name.endswith(f"-{count}") for name in requested_search)
        }
        for count in requested_scales:
            config = search_fixtures(inputs, work, count)
            search_roots[count] = config
            for kind, (query, expected) in search_queries.items():
                name = f"search-{kind}-{count}"
                if name in selected:
                    case_specs[name] = ("search", query, config, work, count,
                        {"count": count, **expected}, config / "projects")

        source_roots = [inputs]
        input_digest_before = tree_digest(source_roots)
        runner = Runner(root, timeout, rss_limit_kib)

        index_samples = []
        profile_specs = {}
        profile_reports = []
        # Prepare one separate synthetic search index per binary before timing cached search.
        for count, config in search_roots.items():
            for variant in ("baseline", "candidate"):
                state = root / f"state-{count}-{variant}"
                env = isolated_environment(root, work, state, config)
                command = [str(variants[variant]["omni"]), "--json", "index", "--provider", "claude"]
                summary, metrics = runner.invoke(command, env, work)
                index_samples.append({"session_count": count, "variant": variant, **metrics})
                if summary.get("failed") != 0 or summary.get("indexed") != count:
                    raise RuntimeError("synthetic search index did not index the expected session count")

        results = []
        for name in sorted(selected):
            kind, selector, provider_root, source, identity, expectation, input_root = case_specs[name]
            commands, environments = {}, {}
            for variant, binary in variants.items():
                if kind == "read":
                    command = [str(binary["probe"]), "read", selector, str(provider_root), str(source), identity]
                    env = isolated_environment(root, work, root / f"unused-{variant}")
                elif kind == "codex-list":
                    command = [str(binary["probe"]), "codex-list", str(provider_root)]
                    env = isolated_environment(root, work, root / f"unused-{variant}")
                else:
                    count = expectation["count"]
                    command = [str(binary["omni"]), "--json", "search", selector, "--provider", "claude",
                        "--project", str(work), "--limit", "20", "--cached"]
                    env = isolated_environment(root, work, root / f"state-{count}-{variant}", provider_root)
                commands[variant] = command
                environments[variant] = env
            input_bytes = sum(path.stat().st_size for path in input_root.rglob("*") if path.is_file())
            result = compare_case(runner, name, commands, environments, work,
                repeats, warmups, expectation, input_bytes)
            results.append(result)
            if args.profile_tool and name in selected:
                profile_specs[name] = (
                    commands["candidate"], environments["candidate"],
                    expectation, result["result_sha256"],
                )

        if args.profile_tool and profiler and profile_output_dir:
            for name in sorted(profile_specs):
                command, environment, expectation, expected_hash = profile_specs[name]
                fixture_before = tree_digest(source_roots)
                profile_reports.append(profile_case(
                    name=name,
                    profiler=profiler,
                    tool=args.profile_tool,
                    function_pattern=args.profile_function,
                    profiler_lib=profiler_lib,
                    output_dir=profile_output_dir,
                    command=command,
                    environment=environment,
                    cwd=work,
                    expected_hash=expected_hash,
                    expectation=expectation,
                    timeout=args.profile_timeout,
                    rss_limit_kib=args.profile_max_rss_mib * 1024,
                ))
                if tree_digest(source_roots) != fixture_before:
                    raise RuntimeError(f"{name} profiling changed synthetic provider input files")

        input_digest_after = tree_digest(source_roots)
        if input_digest_before != input_digest_after:
            raise RuntimeError("synthetic provider input files changed during benchmark")

        report = {
            "schema": 1, "profile": args.profile,
            "fixture_sha256_before": input_digest_before,
            "fixture_sha256_after": input_digest_after,
            "fixture_bytes": sum(path.stat().st_size for path in inputs.rglob("*") if path.is_file()),
            "limits": {"max_process_seconds": timeout, "max_rss_kib": rss_limit_kib},
            "search_index_setup_samples": index_samples,
            "variants": {
                name: {key: hashlib.sha256(path.read_bytes()).hexdigest() for key, path in binary.items()}
                for name, binary in variants.items()
            },
            "cases": results,
            "profiler": None if not args.profile_tool else {
                "tool": args.profile_tool,
                "executable": profiler.name if profiler else None,
                "function_pattern": args.profile_function,
                "timeout_seconds": args.profile_timeout,
                "max_rss_kib": args.profile_max_rss_mib * 1024,
                "output_directory": profile_output_dir.name if profile_output_dir else None,
                "cases": profile_reports,
            },
            "environment": {
                "os": platform.system(),
                "kernel": platform.release(),
                "architecture": platform.machine(),
                "cpu_count": os.cpu_count(),
                "cpu_affinity": available_cpu_affinity(),
                "python": platform.python_version(),
            },
            "timing_policy": "paired medians and raw samples are advisory; output digest and resource caps gate success",
        }
        encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(encoded, encoding="utf-8")
        print(encoded, end="")


if __name__ == "__main__":
    main()
