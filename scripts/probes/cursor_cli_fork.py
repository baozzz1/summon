#!/usr/bin/env python3
"""Cursor CLI 原生 /fork 的隔离真机探针。

只使用标准库。真实调用 Cursor 服务，会消耗少量额度；必须显式传
`--confirm-paid-probe`。所有可写 Cursor 状态落临时目录，生产 HOME 仅用于读取登录态。
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
from typing import Any

ANSI = re.compile(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\)|[=>].)")


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def cursor_state_snapshot() -> dict[str, Any]:
    root = Path.home() / ".cursor"
    files = {}
    for name in ("agent-cli-state.json", "cli-config.json", "statsig-cache.json"):
        path = root / name
        if path.is_file():
            files[name] = {"sha256": sha256(path), "size": path.stat().st_size}
    chats = root / "chats"
    chat_ids = sorted(path.name for path in chats.glob("*/*") if path.is_dir())
    return {"files": files, "chat_ids": chat_ids}


def parse_stream_json(stdout: str) -> tuple[str, str]:
    ids: list[str] = []
    result = None
    terminal = None
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        sid = event.get("session_id")
        if isinstance(sid, str) and sid not in ids:
            ids.append(sid)
        if event.get("type") == "result":
            terminal = event.get("subtype")
            result = event.get("result")
    if len(ids) != 1 or terminal != "success" or not isinstance(result, str):
        raise RuntimeError(
            f"stream-json 缺唯一 session/成功终态: ids={ids} terminal={terminal} result={result!r}"
        )
    return ids[0], result


def run_stream(
    binary: Path,
    root: Path,
    work: Path,
    env: dict[str, str],
    prompt: str,
    resume: str | None = None,
) -> tuple[str, str]:
    args = [
        str(binary),
        "--data-dir",
        str(root / "cursor-data"),
        "--workspace",
        str(work),
        "--trust",
        "--mode",
        "ask",
        "-p",
        "--output-format",
        "stream-json",
        "--stream-partial-output",
    ]
    if resume:
        args.append(f"--resume={resume}")
    args.append(prompt)
    completed = subprocess.run(
        args,
        cwd=work,
        env=env,
        text=True,
        capture_output=True,
        timeout=240,
        check=False,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"Cursor stream-json 退出 {completed.returncode}: {completed.stderr[-2000:]}"
        )
    return parse_stream_json(completed.stdout)


def chat_ids(config: Path) -> set[str]:
    return {path.name for path in (config / "chats").glob("*/*") if path.is_dir()}


def read_until(fd: int, transcript: bytearray, needle: str, timeout: float) -> str:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        text = ANSI.sub("", transcript.decode(errors="replace"))
        if needle in text:
            return text
        ready, _, _ = select.select([fd], [], [], min(0.5, deadline - time.monotonic()))
        if not ready:
            continue
        try:
            chunk = os.read(fd, 64 * 1024)
        except OSError:
            break
        if not chunk:
            break
        transcript.extend(chunk)
    text = ANSI.sub("", transcript.decode(errors="replace"))
    raise TimeoutError(f"Cursor TUI 未出现 {needle!r}; tail={text[-1500:]!r}")


def interactive_fork(
    binary: Path,
    root: Path,
    work: Path,
    env: dict[str, str],
    source_id: str,
) -> str:
    before = chat_ids(root / "cursor-config")
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    process = subprocess.Popen(
        [
            str(binary),
            "--data-dir",
            str(root / "cursor-data"),
            "--workspace",
            str(work),
            "--trust",
            f"--resume={source_id}",
        ],
        cwd=work,
        env={**env, "TERM": "xterm-256color"},
        stdin=slave,
        stdout=slave,
        stderr=slave,
        start_new_session=True,
        close_fds=True,
    )
    os.close(slave)
    transcript = bytearray()
    try:
        read_until(master, transcript, "Add a follow-up", 90)
        os.write(master, b"/fork\r")
        read_until(master, transcript, "Fork the current chat into a new session", 20)
        os.write(master, b"\r")
        read_until(master, transcript, "This conversation has been forked", 30)
        after = chat_ids(root / "cursor-config")
        created = sorted(after - before)
        if len(created) != 1 or created[0] == source_id:
            raise RuntimeError(f"/fork 未创建唯一新 session: before={before} after={after}")
        fork_id = created[0]
        os.write(master, b"/quit\r")
        read_until(master, transcript, "Exit", 20)
        os.write(master, b"\r")
        deadline = time.monotonic() + 20
        while process.poll() is None and time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.5)
            if ready:
                try:
                    chunk = os.read(master, 64 * 1024)
                except OSError:
                    chunk = b""
                transcript.extend(chunk)
        if process.poll() is None:
            raise TimeoutError("Cursor TUI /quit 未退出")
        text = ANSI.sub("", transcript.decode(errors="replace"))
        resumed = re.findall(r"agent --resume=?\s*([0-9a-f-]{36})", text)
        if fork_id not in resumed:
            raise RuntimeError(f"退出提示未返回 fork session id: fork={fork_id} hints={resumed}")
        return fork_id
    finally:
        try:
            os.close(master)
        except OSError:
            pass
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGINT)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)


def cleanup_probe_workers(root: Path) -> int:
    """只回收 argv 含本探针根目录的 Cursor worker-server，绝不按进程名广杀。"""
    own = os.getpid()
    found: list[int] = []
    for proc in Path("/proc").iterdir():
        if not proc.name.isdigit() or int(proc.name) == own:
            continue
        try:
            command = (proc / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="ignore")
        except OSError:
            continue
        if str(root) in command and "worker-server" in command:
            found.append(int(proc.name))
    for pid in found:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if not any(Path(f"/proc/{pid}").exists() for pid in found):
            break
        time.sleep(0.1)
    for pid in found:
        if Path(f"/proc/{pid}").exists():
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    return len(found)


def acp_fork_surface(
    binary: Path,
    root: Path,
    work: Path,
    env: dict[str, str],
    source_id: str,
) -> dict[str, Any]:
    """探测 Cursor ACP 是否公开并实现 session/fork，不猜私有 flag。"""
    process = subprocess.Popen(
        [
            str(binary),
            "--data-dir",
            str(root / "cursor-data"),
            "--yolo",
            "acp",
        ],
        cwd=work,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        start_new_session=True,
    )
    if process.stdin is None or process.stdout is None:
        raise RuntimeError("无法建立 Cursor ACP stdio")

    def request(request_id: int, method: str, params: dict[str, Any]) -> dict[str, Any]:
        payload = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }
        process.stdin.write(json.dumps(payload, separators=(",", ":")) + "\n")
        process.stdin.flush()
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            ready, _, _ = select.select(
                [process.stdout], [], [], min(0.5, deadline - time.monotonic())
            )
            if not ready:
                continue
            line = process.stdout.readline()
            if not line:
                stderr = process.stderr.read()[-2000:] if process.stderr else ""
                raise RuntimeError(f"Cursor ACP 提前退出: rc={process.poll()} stderr={stderr}")
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if message.get("id") == request_id:
                return message
            if "id" in message and "method" in message:
                response = {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "error": {"code": -32601, "message": "probe client has no handlers"},
                }
                process.stdin.write(json.dumps(response, separators=(",", ":")) + "\n")
                process.stdin.flush()
        raise TimeoutError(f"Cursor ACP {method} 超时")

    try:
        initialized = request(
            1,
            "initialize",
            {
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": False, "writeTextFile": False},
                    "terminal": False,
                },
                "clientInfo": {"name": "summon-cursor-fork-probe", "version": "1"},
            },
        )
        if "error" in initialized:
            raise RuntimeError(f"Cursor ACP initialize 失败: {initialized['error']}")
        capabilities = initialized.get("result", {}).get("agentCapabilities", {})
        session_capabilities = capabilities.get("sessionCapabilities", {})
        advertised = isinstance(session_capabilities, dict) and "fork" in session_capabilities
        loaded = None
        next_id = 2
        if advertised:
            loaded_response = request(
                next_id,
                "session/load",
                {"sessionId": source_id, "cwd": str(work), "mcpServers": []},
            )
            loaded = "error" not in loaded_response
            next_id += 1
        forked = request(
            next_id,
            "session/fork",
            {"sessionId": source_id, "cwd": str(work), "mcpServers": []},
        )
        error = forked.get("error")
        fork_id = forked.get("result", {}).get("sessionId")
        return {
            "initialize_advertised_fork": advertised,
            "source_loaded_before_fork": loaded,
            "session_fork_error_code": error.get("code") if isinstance(error, dict) else None,
            "session_fork_error": error.get("message") if isinstance(error, dict) else None,
            "session_fork_returned_distinct_id": isinstance(fork_id, str)
            and fork_id != source_id,
            "session_fork_id": fork_id,
        }
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGINT)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cursor-agent", type=Path, required=True)
    parser.add_argument("--confirm-paid-probe", action="store_true")
    args = parser.parse_args()
    if not args.confirm_paid_probe:
        parser.error("真机探针会消耗 Cursor 额度；必须显式传 --confirm-paid-probe")
    binary = args.cursor_agent.resolve(strict=True)
    production_before = cursor_state_snapshot()
    source_marker = f"source-{os.urandom(4).hex()}"
    branch_marker = f"branch-{os.urandom(4).hex()}"
    with tempfile.TemporaryDirectory(prefix="summon-cursor-fork-probe-") as temporary:
        root = Path(temporary)
        config = root / "cursor-config"
        data = root / "cursor-data"
        work = root / "repo"
        for directory in (config, data, work):
            directory.mkdir(mode=0o700)
        user_config = Path.home() / ".cursor/cli-config.json"
        if user_config.is_file():
            shutil.copyfile(user_config, config / "cli-config.json")
            (config / "cli-config.json").chmod(0o600)
        subprocess.run(["git", "init", "-q", str(work)], check=True)
        (work / "README.md").write_text("isolated cursor fork probe\n")
        env = os.environ.copy()
        env["CURSOR_CONFIG_DIR"] = str(config)
        env["CURSOR_DATA_DIR"] = str(data)
        try:
            version = subprocess.run(
                [str(binary), "--version"], text=True, capture_output=True, check=True
            ).stdout.strip()
            source_id, ready = run_stream(
                binary,
                root,
                work,
                env,
                f"Remember SOURCE_MARKER={source_marker}. Reply only SOURCE_READY.",
            )
            if ready.strip() != "SOURCE_READY":
                raise RuntimeError(f"source seed 未确认: {ready!r}")
            fork_id = interactive_fork(binary, root, work, env, source_id)
            sid, inherited = run_stream(
                binary,
                root,
                work,
                env,
                f"Define BRANCH_MARKER={branch_marker}. If SOURCE_MARKER is inherited, reply exactly BRANCH_READY SOURCE={source_marker}",
                fork_id,
            )
            if sid != fork_id or inherited.strip() != f"BRANCH_READY SOURCE={source_marker}":
                raise RuntimeError(f"fork 未继承 source: sid={sid} result={inherited!r}")
            sid, source_result = run_stream(
                binary,
                root,
                work,
                env,
                "Before this prompt, was any BRANCH_MARKER value defined? Reply exactly NO_BRANCH if none was defined; otherwise only its value.",
                source_id,
            )
            if sid != source_id or source_result.strip() != "NO_BRANCH":
                raise RuntimeError(f"source 被 fork 分支污染: sid={sid} result={source_result!r}")
            sid, fork_result = run_stream(
                binary,
                root,
                work,
                env,
                "Reply with the BRANCH_MARKER value only.",
                fork_id,
            )
            if sid != fork_id or fork_result.strip() != branch_marker:
                raise RuntimeError(f"fork 无法续聊: sid={sid} result={fork_result!r}")
            acp = acp_fork_surface(binary, root, work, env, source_id)
            acp_fork_id = acp.pop("session_fork_id")
            acp_semantics = {
                "fork_inherited_source_context": False,
                "source_excluded_fork_context": False,
                "fork_stream_json_resume": False,
            }
            if acp["initialize_advertised_fork"] and isinstance(acp_fork_id, str):
                acp_marker = f"acp-{os.urandom(4).hex()}"
                sid, acp_inherited = run_stream(
                    binary,
                    root,
                    work,
                    env,
                    f"Define ACP_BRANCH_MARKER={acp_marker}. Reply exactly ACP_FORK_READY SOURCE={source_marker}",
                    acp_fork_id,
                )
                acp_semantics["fork_inherited_source_context"] = (
                    sid == acp_fork_id
                    and acp_inherited.strip() == f"ACP_FORK_READY SOURCE={source_marker}"
                )
                acp_semantics["fork_stream_json_resume"] = sid == acp_fork_id
                sid, acp_source = run_stream(
                    binary,
                    root,
                    work,
                    env,
                    "Before this prompt, was ACP_BRANCH_MARKER defined? Reply exactly NO_ACP_BRANCH if not.",
                    source_id,
                )
                acp_semantics["source_excluded_fork_context"] = (
                    sid == source_id and acp_source.strip() == "NO_ACP_BRANCH"
                )
            acp["semantics"] = acp_semantics
            workers = cleanup_probe_workers(root)
            production_after = cursor_state_snapshot()
            integration_ready = (
                acp["initialize_advertised_fork"]
                and acp["source_loaded_before_fork"] is True
                and acp["session_fork_returned_distinct_id"]
                and all(acp_semantics.values())
            )
            summary = {
                "schema": 1,
                "cursor_version": version,
                "cursor_binary_sha256": sha256(binary),
                "source_and_fork_ids_distinct": source_id != fork_id,
                "fork_inherited_source_context": True,
                "source_excluded_branch_context": True,
                "source_stream_json_resume": True,
                "fork_stream_json_resume": True,
                "fork_id_returned_in_exit_hint": True,
                "production_cursor_content_unchanged": production_before == production_after,
                "isolated_worker_processes_reaped": workers,
                "acp": acp,
                "summon_integration_ready": integration_ready,
            }
            print(json.dumps(summary, ensure_ascii=False, indent=2))
            if not integration_ready:
                print(
                    "Cursor 仅有交互 /fork，未提供结构化 fork 接口；禁止接入 summon。",
                    file=sys.stderr,
                )
                return 3
            return 0
        finally:
            cleanup_probe_workers(root)


if __name__ == "__main__":
    raise SystemExit(main())
