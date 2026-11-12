#!/usr/bin/env python3
"""隔离 tmux server 的 OSC 7501 传输回归。python3 ... target/debug/summon"""
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

binary = str(Path(sys.argv[1]).resolve())
tmux = shutil.which('tmux')
server = 'osc7501-test-' + uuid.uuid4().hex

def run(*args, check=True):
    return subprocess.run([tmux, '-L', server, *args], capture_output=True, text=True, check=check)

def wait_for(fn, seconds=8):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            value = fn()
            if value:
                return value
        except (FileNotFoundError, json.JSONDecodeError):
            pass
        time.sleep(.02)
    raise AssertionError('timed out')

with tempfile.TemporaryDirectory(prefix='osc7501-') as temp:
    root = Path(temp)
    snapshot = root / 'status.json'
    receipt = root / 'receipt.json'
    fixture = root / 'fixture.py'
    fixture.write_text('''import os, sys, tty, select, time, json
from pathlib import Path
tty.setraw(0)
os.write(1,b'\\x1b]7501;?\\x1b\\\\\\x1b[c')
reply = os.read(0,100) if select.select([0],[],[],3)[0] else b''
while select.select([0],[],[],.1)[0]: reply += os.read(0,100)
Path(sys.argv[1]).write_text(json.dumps({'reply':reply.hex()}))
for state in ['working','blocked','done']:
 os.write(1, ('\\x1b]7501;state='+state+':kind=question:app=claude-code\\x07').encode())
 time.sleep(.6)
os.write(1,b'\\x1b]7501;?\\x07')
extra = os.read(0,100) if select.select([0],[],[],.4)[0] else b''
Path(sys.argv[1]).write_text(json.dumps({'reply':reply.hex(),'extra':extra.hex()}))
time.sleep(4)
''')
    command = shlex.join([binary, 'program-status-run', str(snapshot), 'generation-test', shlex.join([sys.executable, str(fixture), str(receipt)])])
    try:
        run('-f', '/dev/null', 'new-session', '-d', '-s', 'probe', command)
        wait_for(receipt.exists)
        assert json.loads(receipt.read_text())['reply'].startswith(b'\x1b]7501;?\x1b\\'.hex()), 'OSC reply must precede tmux DA reply'
        observed = []
        for state in ['working', 'blocked', 'done']:
            def state_is():
                obj = json.loads(snapshot.read_text())
                return obj if (obj.get('report') or {}).get('state') == state else None
            obj = wait_for(state_is)
            observed.append((state, obj['sequence']))
        wait_for(lambda: 'extra' in json.loads(receipt.read_text()))
        assert json.loads(receipt.read_text())['extra'] == '', 'query reply must be one-shot'
        assert snapshot.stat().st_size < 4096
        # pipe-pane 可供用户采集同一 pane 的输出。
        pane = run('display-message', '-p', '-t', '=probe:', '#{pane_id}').stdout.strip()
        sink = root / 'existing.txt'
        run('pipe-pane', '-O', '-t', pane, 'cat > ' + shlex.quote(str(sink)))
        assert run('display-message', '-p', '-t', pane, '#{pane_pipe}').stdout.strip() == '1'
        print(json.dumps({'handshake':'fixed response once','states':observed,'pipe_slot':'available','snapshot_bytes':snapshot.stat().st_size}, ensure_ascii=False))
    finally:
        run('kill-server', check=False)
        time.sleep(.2)  # 代理收到 EOF 后完成最后一次原子写入。
