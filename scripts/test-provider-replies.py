#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Check the first Claude and Codex replies after transferring the same conversation."""

import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


TURNS = [
    ("user", "Remember the synthetic orchard."),
    ("assistant", "The orchard has six pear trees."),
    ("user", "Record the next synthetic observation."),
    ("assistant", "The pear trees have blossomed."),
    ("user", "Add the final synthetic observation."),
    ("assistant", "The pears are ready to pick."),
]
MARKERS = re.compile("|".join(re.escape(text) for _, text in TURNS))


def check_turns(messages, expected, stage):
    """Check ordering, roles and duplicates while allowing provider-generated context."""
    found = []
    for role, text in messages:
        found.extend((role, marker) for marker in MARKERS.findall(text))
    if found != expected:
        raise AssertionError(f"{stage}: expected {expected!r}, got {found!r}")


def codex_reply(text):
    item = {"type": "message", "id": "msg_fixture", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]}
    response = {"object": "response", "id": "resp_fixture", "model": "gpt-5"}
    return [
        {"type": "response.created", "response": {**response, "status": "in_progress", "output": []}},
        {"type": "response.output_item.done", "output_index": 0, "item": item},
        {"type": "response.completed", "response": {**response, "status": "completed", "output": [item]}},
    ]


def claude_reply(text, model):
    message = {"id": "msg_fixture", "type": "message", "model": model, "role": "assistant",
               "content": [], "stop_reason": None, "stop_sequence": None,
               "usage": {"input_tokens": 20, "output_tokens": 0}}
    return [
        {"type": "message_start", "message": message},
        {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
        {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}},
        {"type": "content_block_stop", "index": 0},
        {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 8}},
        {"type": "message_stop"},
    ]


class Model(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        path = self.path.split("?")[0]
        if path not in {"/v1/responses", "/v1/messages"}:
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", 0))
        if not 0 < length <= 4 * 1024 * 1024:
            self.send_error(413)
            return
        body = json.loads(self.rfile.read(length))
        if path == "/v1/responses":
            messages = body["input"]
            events = codex_reply(TURNS[3][1])
        else:
            messages = body["messages"]
            events = claude_reply(TURNS[5][1], body["model"])
        history = []
        for message in messages:
            content = message.get("content", [])
            if isinstance(content, str):
                history.append((message.get("role"), content))
            else:
                history.extend((message.get("role"), part["text"]) for part in content if "text" in part)
        self.server.requests.append((path, history))
        wire = "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(wire)))
        self.end_headers()
        self.wfile.write(wire)


def check_replies(root, server, omni, codex, claude):
    home, workspace = root / "home", root / "workspace"
    codex_home, claude_home = root / "codex", root / "claude"
    for path in [home, workspace, codex_home, claude_home, root / "tmp"]:
        path.mkdir()
    base_url = f"http://127.0.0.1:{server.server_port}"
    env = dict(
        PATH=os.environ["PATH"], HOME=str(home), TMPDIR=str(root / "tmp"),
        GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
        CODEX_HOME=str(codex_home), CLAUDE_CONFIG_DIR=str(claude_home), OMNISESSION_HOME=str(root / "omni"),
        OMNI_CODEX_BIN=codex, OMNI_CLAUDE_BIN=claude, OMNI_NO_UPDATE_CHECK="1",
        ANTHROPIC_BASE_URL=base_url, ANTHROPIC_API_KEY="loopback-fixture", CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1",
        HTTP_PROXY="http://127.0.0.1:1", HTTPS_PROXY="http://127.0.0.1:1", ALL_PROXY="http://127.0.0.1:1",
        NO_PROXY="127.0.0.1,localhost",
    )
    (codex_home / "config.toml").write_text(
        'model = "gpt-5"\nmodel_provider = "fixture"\n[model_providers.fixture]\n'
        f'name = "Fixture"\nbase_url = "{base_url}/v1"\nwire_api = "responses"\n'
        'requires_openai_auth = false\nsupports_websockets = false\n'
    )
    source = str(uuid.uuid4())
    project = claude_home / "projects" / "synthetic"
    project.mkdir(parents=True)
    parent = None
    with (project / f"{source}.jsonl").open("w") as transcript:
        for role, text in TURNS[:2]:
            entry = str(uuid.uuid4())
            message = {"role": role, "content": [{"type": "text", "text": text}]}
            record = {"type": role, "uuid": entry, "parentUuid": parent, "sessionId": source,
                      "timestamp": "2026-01-01T00:00:00Z", "cwd": str(workspace), "message": message}
            print(json.dumps(record), file=transcript)
            parent = entry

    def run(*args):
        result = subprocess.run(args, env=env, cwd=workspace, stdin=subprocess.DEVNULL,
                                capture_output=True, text=True, timeout=90)
        if result.returncode:
            detail = result.stderr.strip() or result.stdout.strip()
            raise RuntimeError(f"{Path(args[0]).name} exited {result.returncode}: {detail[-2000:]}")
        return result.stdout

    def fork(source_ref, target):
        output = run(omni, "fork", source_ref, "--in", target, "--materialize-only")
        match = re.search(rf"^Created and verified ({target}:[\w-]+)\.$", output, re.MULTILINE)
        if not match:
            raise AssertionError(f"{source_ref} fork omitted the verified {target} ID")
        return match[1]

    def check_saved(ref, expected):
        events = json.loads(run(omni, "--json", "show", ref))["events"]
        messages = [(event["kind"].removeprefix("message_"), event["payload"]["text"])
                    for event in events if event["kind"] in {"message_user", "message_assistant"}]
        check_turns(messages, expected, f"{ref.split(':')[0]} saved history")

    def check_request(path, expected):
        requests = [history for endpoint, history in server.requests if endpoint == path]
        if len(requests) != 1:
            raise AssertionError(f"expected one model request to {path}, got {len(requests)}")
        check_turns(requests[0], expected, path)

    run("git", "init", "--quiet")
    codex_ref = fork(f"claude:{source}", "codex")
    output = run(codex, "exec", "resume", "--json", codex_ref.split(":")[1], TURNS[2][1])
    events = [json.loads(line) for line in output.splitlines()]
    replies = [e["item"]["text"] for e in events
               if e["type"] == "item.completed" and e["item"]["type"] == "agent_message"]
    if replies != [TURNS[3][1]]:
        raise AssertionError(f"unexpected Codex reply: {replies!r}")
    check_request("/v1/responses", TURNS[:3])
    check_saved(codex_ref, TURNS[:4])

    claude_ref = fork(codex_ref, "claude")
    output = run(claude, "--bare", "--print", "--output-format", "json", "--model", "sonnet",
                 "--strict-mcp-config", "--tools", "", "--resume", claude_ref.split(":")[1], TURNS[4][1])
    if json.loads(output)["result"] != TURNS[5][1]:
        raise AssertionError("unexpected Claude reply")
    check_request("/v1/messages", TURNS[:5])
    check_saved(claude_ref, TURNS)
    check_saved(fork(claude_ref, "codex"), TURNS)


def main():
    with tempfile.TemporaryDirectory(prefix="omni-replies-") as directory:
        with ThreadingHTTPServer(("127.0.0.1", 0), Model) as server:
            server.requests = []
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                check_replies(Path(directory).resolve(), server, *sys.argv[1:])
            finally:
                server.shutdown()
                thread.join()


if __name__ == "__main__":
    main()
