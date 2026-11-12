#!/usr/bin/env python3
"""PTY 字节透明、双向背压、resize、退出码及信号回归。"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import shlex
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

binary = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='osc7501-pty-') as temp:
    root = Path(temp)
    fixture = root / 'child.py'
    fixture.write_text('''import os, tty, fcntl, termios, struct, hashlib, signal, sys, time
from pathlib import Path
tty.setraw(0)
if len(sys.argv)>1:
 signal.signal(signal.SIGTERM,lambda *_:sys.exit(42))
 signal.signal(signal.SIGHUP,lambda *_:sys.exit(41))
 Path(sys.argv[1]).write_text(str(os.getpid()))
 while True:time.sleep(.1)
size=lambda:struct.unpack('HHHH',fcntl.ioctl(0,termios.TIOCGWINSZ,b' '*8))[:2]
os.write(1,('SIZE0=%s\\n'%str(size())).encode())
payload=b'o'*200000
while payload:
 n=os.write(1,payload);payload=payload[n:]
received=b''
while len(received)<130000:received+=os.read(0,min(8192,130000-len(received)))
os.write(2,('\\nSIZE1=%s\\nHASH=%s\\n'%(str(size()),hashlib.sha256(received).hexdigest())).encode())
sys.exit(37)
''')
    def start(extra=()):
        master, slave = pty.openpty()
        fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',40,110,0,0))
        before=termios.tcgetattr(slave)
        proc=subprocess.Popen([binary,'program-status-run',str(root/'status.json'),'pty-test',shlex.join([sys.executable,str(fixture),*extra])],stdin=slave,stdout=slave,stderr=slave,start_new_session=True)
        os.set_blocking(master,False)
        return proc,master,slave,before
    def restored(slave,before):
        after=termios.tcgetattr(slave)
        # macOS 内核在 raw -> canonical 时设置待重处理输入标志。
        after[3] &= ~getattr(termios,"PENDIN",0)
        expected=list(before);expected[3] &= ~getattr(termios,"PENDIN",0)
        assert after==expected,(expected,after)
    proc,master,slave,before=start()
    out=bytearray();payload=b'i'*130000;sent=0;resized=False
    deadline=time.monotonic()+20
    try:
        while time.monotonic()<deadline:
            read,write,_=select.select([master],[master] if resized and sent<len(payload) else [],[],.1)
            if read:
                try: out.extend(os.read(master,8192))
                except BlockingIOError: pass
            if not resized and b'SIZE0=' in out:
                fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',55,132,0,0));resized=True
            if write:
                try:sent+=os.write(master,payload[sent:sent+4096])
                except BlockingIOError:pass
            if proc.poll() is not None and not read:break
        assert proc.poll()==37, (proc.poll(),bytes(out[-300:]))
        expected=b'SIZE0=(40, 110)\n'+b'o'*200000+('\nSIZE1=(55, 132)\nHASH='+hashlib.sha256(payload).hexdigest()+'\n').encode()
        assert out==expected, (len(out),len(expected),bytes(out[-300:]))
        restored(slave,before)
    finally:
        if proc.poll() is None:proc.terminate();proc.wait(timeout=5)
        os.close(master);os.close(slave)
    pidfile=root/'child.pid'
    for forwarded,expected_exit in [(signal.SIGTERM,42),(signal.SIGHUP,41)]:
        pidfile.unlink(missing_ok=True)
        proc,master,slave,before=start([str(pidfile)])
        try:
            deadline=time.monotonic()+5
            while not pidfile.exists() and time.monotonic()<deadline:time.sleep(.02)
            assert pidfile.exists()
            child_pid=int(pidfile.read_text())
            proc.send_signal(forwarded);assert proc.wait(timeout=5)==expected_exit
            try:os.kill(child_pid,0)
            except ProcessLookupError:pass
            else:raise AssertionError('child process survived proxy termination')
            restored(slave,before)
        finally:
            if proc.poll() is None:proc.kill();proc.wait()
            os.close(master);os.close(slave)
    fixture.write_text("""import os,signal,time,sys
from pathlib import Path
pid=os.fork()
if pid:
 while not Path(sys.argv[1]).exists():time.sleep(.01)
 sys.exit(0)
signal.signal(signal.SIGHUP,signal.SIG_IGN)
signal.signal(signal.SIGTERM,signal.SIG_IGN)
Path(sys.argv[1]).write_text(str(os.getpid()))
while True:time.sleep(.1)
""")
    pidfile.unlink()
    proc,master,slave,before=start([str(pidfile)])
    try:
        assert proc.wait(timeout=5)==0
        child_pid=int(pidfile.read_text())
        deadline=time.monotonic()+3
        while time.monotonic()<deadline:
            # 孤儿可能短暂处于等待 init 回收的 zombie 态。
            status=subprocess.run(['ps','-p',str(child_pid),'-o','stat='],capture_output=True,text=True).stdout.strip()
            if not status or status.startswith('Z'):break
            time.sleep(.02)
        else:raise AssertionError('descendant survived leader exit')
        restored(slave,before)
    finally:
        if proc.poll() is None:proc.kill();proc.wait()
        if pidfile.exists():
            try:os.kill(int(pidfile.read_text()),signal.SIGKILL)
            except ProcessLookupError:pass
        os.close(master);os.close(slave)
    print(json.dumps({'output_bytes':len(out),'input_bytes':sent,'resize':'40x110 -> 55x132','exit_code':37,'sigterm_child_exit':42,'sighup_child_exit':41,'terminal_restored':True,'descendant_cleanup':True}))
