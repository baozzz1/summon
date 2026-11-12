"""Controlled Anthropic-protocol fixture; does not authenticate to a model service."""
import http.server
import json
import os
import subprocess
import threading


def verify_model_launch(binary, claude, env, target):
    requests = []
    failures = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if "count_tokens" in self.path:
                content = json.dumps({"input_tokens": 1}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(content)))
                self.end_headers()
                self.wfile.write(content)
                return
            requests.append(body)
            names = {tool.get("name", "") for tool in body.get("tools", [])}
            if "mcp__execution__Bash" not in names or "mcp__summon__invoke" not in names:
                failures.append("required MCP tools absent: " + repr(names))
            if any(not name.startswith(("mcp__execution__", "mcp__summon__")) for name in names):
                failures.append("unexpected cloud tool: " + repr(names))
            has_result = any(
                isinstance(message.get("content"), list)
                and any(block.get("type") == "tool_result" for block in message["content"])
                for message in body.get("messages", [])
            )
            if has_result:
                block = {"type": "text", "text": "MODEL_PROTOCOL_OK"}
                stop = "end_turn"
            else:
                block = {"type": "tool_use", "id": "fixture-tool-1", "name": "mcp__execution__Bash",
                         "input": {"command": "pwd", "description": "verify execution location"}}
                stop = "tool_use"
            message = {"id": "fixture-message", "type": "message", "role": "assistant",
                       "model": body["model"], "content": [block], "stop_reason": stop,
                       "stop_sequence": None, "usage": {"input_tokens": 10, "output_tokens": 5}}
            self.send_response(200)
            if body.get("stream"):
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                def event(kind, content):
                    self.wfile.write(("event: " + kind + "\ndata: " + json.dumps(content) + "\n\n").encode())
                    self.wfile.flush()
                event("message_start", {"type": "message_start", "message": dict(message, content=[], stop_reason=None)})
                empty = dict(block, **({"text": ""} if has_result else {"input": {}}))
                event("content_block_start", {"type": "content_block_start", "index": 0, "content_block": empty})
                delta = ({"type": "text_delta", "text": block["text"]} if has_result else
                         {"type": "input_json_delta", "partial_json": json.dumps(block["input"])})
                event("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": delta})
                event("content_block_stop", {"type": "content_block_stop", "index": 0})
                event("message_delta", {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
                                        "usage": {"output_tokens": 5}})
                event("message_stop", {"type": "message_stop"})
            else:
                content = json.dumps(message).encode()
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(content)))
                self.end_headers()
                self.wfile.write(content)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        fixture_env = {"PATH": os.environ["PATH"], "HOME": env["HOME"],
                       "CLAUDE_CONFIG_DIR": env["HOME"] + "/claude-fixture",
                       "ANTHROPIC_BASE_URL": "http://127.0.0.1:" + str(server.server_port),
                       "ANTHROPIC_API_KEY": "fixture-not-a-credential",
                       "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                       "ENABLE_TOOL_SEARCH": "false"}
        result = subprocess.run(
            [binary, "remote-tools", "launch", "--target", target, "--claude", claude, "--",
             "--bare", "-p", "--model", "sonnet", "--dangerously-skip-permissions", "--max-turns", "3",
             "Report the execution directory."], env=fixture_env,
            capture_output=True, text=True, timeout=60,
        )
        assert result.returncode == 0, result.stderr + result.stdout
        assert "MODEL_PROTOCOL_OK" in result.stdout, result.stdout
        assert not failures, failures
        assert len(requests) >= 2, requests
        tool_results = [block for message in requests[-1]["messages"]
                        if isinstance(message.get("content"), list)
                        for block in message["content"] if block.get("type") == "tool_result"]
        assert tool_results and all(not block.get("is_error") for block in tool_results), tool_results
        assert "/private/tmp" in json.dumps(tool_results) or "/tmp" in json.dumps(tool_results), tool_results
        print("PASS: controlled model endpoint, MCP-only tools, real remote Bash round-trip", flush=True)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
