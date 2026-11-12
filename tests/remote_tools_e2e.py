#!/usr/bin/env python3
"""Opt-in two-machine test; bootstrap SSH is test setup, feature traffic is outbound.

Pass --bootstrap-json as a JSON argv array ending with the execution host. The test
uses a temporary cloud HOME and execution-side temporary files only. Both binaries
must already be built. No daemon, installed binary, or user configuration changes.
"""
import argparse
import json
import os
from pathlib import Path
import select
import shlex
import signal
import subprocess
import tempfile
import time


class Mcp:
    def __init__(self, binary, env, target):
        self.process = subprocess.Popen(
            [binary, "remote-tools", "proxy", "--target", target],
            env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, bufsize=0,
        )
        self.buffer = b""
        self.sequence = 0
        self.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                 "clientInfo": {"name": "summon-test", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, message):
        self.process.stdin.write(json.dumps(message).encode() + b"\n")

    def call(self, method, params):
        self.sequence += 1
        self.send({"jsonrpc": "2.0", "id": self.sequence, "method": method, "params": params})
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if b"\n" not in self.buffer:
                ready, _, _ = select.select([self.process.stdout], [], [], 1)
                if not ready:
                    continue
                data = os.read(self.process.stdout.fileno(), 65536)
                if not data:
                    raise AssertionError("proxy EOF: " + self.process.stderr.read().decode())
                self.buffer += data
                continue
            line, self.buffer = self.buffer.split(b"\n", 1)
            message = json.loads(line)
            if message.get("id") == self.sequence:
                return message
        raise AssertionError("MCP request timeout")

    def tool(self, name, arguments, success=True):
        response = self.call("tools/call", {"name": name, "arguments": arguments})
        if success:
            assert "error" not in response, response
            assert not response["result"].get("isError"), response
        return response

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)


def text(response):
    return "\n".join(item.get("text", "") for item in response["result"].get("content", []))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--client-binary", required=True)
    parser.add_argument("--client-claude", required=True)
    parser.add_argument("--cloud-host", required=True)
    parser.add_argument("--bootstrap-json", required=True)
    parser.add_argument("--model-claude", help="Also verify Claude launch using a local controlled model endpoint")
    args = parser.parse_args()
    bootstrap = json.loads(args.bootstrap_json)
    target = "e2e-tools"
    with tempfile.TemporaryDirectory(prefix="srt-") as directory:
        root = Path(directory)
        config = root / ".summon" / "config.yaml"
        config.parent.mkdir()
        config.write_text("execution_targets:\n  e2e-tools:\n    provider: claude-code\n")
        wrapper = root / "server"
        wrapper.write_text("#!/bin/sh\nexec env HOME=" + shlex.quote(directory) + " "
                           + shlex.quote(args.binary) + ' "$@"\n')
        wrapper.chmod(0o700)
        env = dict(os.environ, HOME=directory)
        remote = [args.client_binary, "remote-tools", "connect", args.cloud_host,
                  "--name", target, "--remote-summon", str(wrapper),
                  "--claude", args.client_claude, "--working-dir", "/tmp"]
        # PID is only the test-owned foreground client; no daemon/session control.
        command = "printf '%s\\n' $$; exec " + shlex.join(remote)
        client = subprocess.Popen(bootstrap + [command], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        client_pid = int(client.stdout.readline())
        sessions = []
        fixture = None
        try:
            for _ in range(60):
                result = subprocess.run([args.binary, "remote-tools", "list"], env=env,
                                        capture_output=True, text=True, timeout=12)
                status = json.loads(result.stdout)
                if status and status[0]["online"]:
                    print("online:", status[0]["status"], flush=True)
                    break
                if client.poll() is not None:
                    raise AssertionError(client.stderr.read().decode())
                time.sleep(0.25)
            else:
                raise AssertionError("client did not register")
            if args.model_claude:
                from remote_tools_model_fixture import verify_model_launch
                verify_model_launch(args.binary, args.model_claude, env, target)
            first = Mcp(args.binary, env, target)
            sessions.append(first)
            second = Mcp(args.binary, env, target)
            sessions.append(second)
            tools = first.call("tools/list", {})["result"]["tools"]
            assert {t["name"] for t in tools} <= {"Read", "Edit", "Write", "Bash", "Glob", "Grep", "NotebookEdit"}
            # MCP Bash returns provider-native content, including possible JSON text.
            result = first.tool("Bash", {"command": "mktemp -d /tmp/summon-tools-e2e.XXXXXX", "description": "create test fixture"})
            raw = text(result)
            import re
            fixture = re.search(r"/tmp/summon-tools-e2e\.[A-Za-z0-9]+", raw).group(0)
            path = fixture + "/proof.txt"
            first.tool("Bash", {"command": "printf 'alpha\\n' > " + shlex.quote(path), "description": "write test fixture"})
            first.tool("Read", {"file_path": path})
            edited = first.tool("Edit", {"file_path": path, "old_string": "alpha", "new_string": "beta"})
            assert not json.loads(text(edited)).get("contentNotInModelContext"), edited
            unread = second.tool("Edit", {"file_path": path, "old_string": "beta", "new_string": "beta-second"}, success=False)
            # Providers may reject unread edits or flag missing read context in their result.
            if not (unread.get("error") or unread["result"].get("isError")):
                assert json.loads(text(unread))["contentNotInModelContext"], unread
            first.tool("Bash", {"command": "printf 'gamma\\n' >> " + shlex.quote(path), "description": "append fixture"})
            assert "beta" in text(first.tool("Read", {"file_path": path}))
            assert "gamma" in text(second.tool("Read", {"file_path": path}))
            assert "error" in first.tool("Agent", {}, success=False)
            background = []
            for session in sessions:
                result = session.tool("Bash", {"command": "sleep 300 & printf 'CHILD=%s\\n' \"$!\"", "description": "test supervised background child"})
                background.append(int(re.search(r"CHILD=(\d+)", text(result)).group(1)))
            first.close()
            listing = subprocess.run(bootstrap + ["ps -o stat= -p " + str(background[0])], capture_output=True, text=True, timeout=15)
            assert not listing.stdout.strip() or listing.stdout.strip().startswith("Z"), listing.stdout
            second.tool("Read", {"file_path": path})
            second.tool("Bash", {"command": "rm " + shlex.quote(path) + " && rmdir " + shlex.quote(fixture), "description": "clean test fixture"})
            fixture = None
            subprocess.run(bootstrap + ["kill -TERM " + str(client_pid)], check=True, timeout=15)
            client.wait(timeout=15)
            second.process.wait(timeout=10)
            assert second.process.returncode != 0, "disconnect must report unknown outcome"
            listing = subprocess.run(bootstrap + ["ps -o stat= -p " + str(background[1])], capture_output=True, text=True, timeout=15)
            assert not listing.stdout.strip() or listing.stdout.strip().startswith("Z"), listing.stdout
            result = subprocess.run([args.binary, "remote-tools", "list"], env=env,
                                    capture_output=True, text=True, timeout=12)
            assert not json.loads(result.stdout)[0]["online"]
            print("PASS: Read/Edit/Bash, state isolation, tool allowlist, one-session EOF, outbound disconnect, background child cleanup", flush=True)
        finally:
            for session in sessions:
                session.close()
            if client.poll() is None:
                subprocess.run(bootstrap + ["kill -TERM " + str(client_pid)], timeout=15)
                client.wait(timeout=15)
            if fixture:
                print("test fixture retained for inspection:", fixture)


if __name__ == "__main__":
    main()
