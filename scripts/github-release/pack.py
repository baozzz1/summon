#!/usr/bin/env python3
"""把一个平台的二进制打成 Release 包。只用标准库,不执行被打包的二进制。

    python3 scripts/github-release/pack.py check --stage <目录> --platform <平台> --stamp <文件> [--sha <提交>]
    python3 scripts/github-release/pack.py pack --stage <目录> --platform <平台> --stamp <文件> \\
        --out <目录> [--sha <提交>]

--stage 里恰是该平台的那几个文件(stamp.json 除外);--stamp 是包里 summon 自报的构建戳
(`summon install-stamp print` 的输出),由构建的一方事先读出。给了 --sha 时构建戳的提交必须是它的前缀。

pack 写出 `summon-<平台>.tar.gz` 与 `summon-<平台>.build.json`:平铺、0755、属主与时间统一,
同一批二进制打出的包逐字节相同。用 Python 打包:GNU 与 BSD tar 的参数不同,macOS 的 tar 还会夹带
._* 扩展属性文件。

macOS 的签名 job 只用这个脚本与 codesign 处理构建 job 交来的二进制,不跑其中任何一个。
"""
import argparse
import gzip
import hashlib
import io
import json
import re
import sys
import tarfile
from pathlib import Path

# 每个平台的包里恰有这些文件;与 lib.sh 的 summon_release_files 一致(测试钉住)。
MEMBERS = {
    'linux-x64': ('summon', 'summon-sidecar'),
    'linux-arm64': ('summon', 'summon-sidecar'),
    'darwin-arm64': ('summon', 'summon-sidecar', 'summon-menubar'),
}
STAMP_NAME = 'stamp.json'
VERSION = re.compile(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)')


class PackError(Exception):
    pass


def load_stamp(path):
    try:
        stamp = json.loads(Path(path).read_text(encoding='utf-8'))
    except (OSError, ValueError) as err:
        raise PackError(f'读不了构建戳 {path}:{err}') from None
    if not isinstance(stamp, dict):
        raise PackError(f'{path} 不是 JSON 对象')
    problems = []
    if not isinstance(stamp.get('version'), str) or not VERSION.fullmatch(stamp['version']):
        problems.append(f'version={stamp.get("version")!r}')
    commit = stamp.get('commit')
    if not isinstance(commit, str) or not re.fullmatch(r'[0-9a-f]{7,40}', commit):
        problems.append(f'commit={commit!r}')
    if type(stamp.get('commit_time')) is not int or stamp['commit_time'] <= 0:
        problems.append(f'commit_time={stamp.get("commit_time")!r}')
    if problems:
        raise PackError('构建戳不对:' + ';'.join(problems))
    return stamp


def check(stage, platform, stamp_path, sha=None):
    """stage 恰是该平台的文件、都是普通文件;构建戳完整且(给了 sha 时)出自这个提交。"""
    if platform not in MEMBERS:
        raise PackError(f'不认识的平台 {platform!r}')
    stage = Path(stage)
    if not stage.is_dir():
        raise PackError(f'{stage} 不是目录')
    names = sorted(p.name for p in stage.iterdir() if p.name != STAMP_NAME)
    if names != sorted(MEMBERS[platform]):
        raise PackError(f'{stage} 里是 {names},应为 {sorted(MEMBERS[platform])}')
    for name in names:
        path = stage / name
        if path.is_symlink() or not path.is_file():
            raise PackError(f'{path} 不是普通文件')
    stamp = load_stamp(stamp_path)
    if sha is not None and not sha.startswith(stamp['commit']):
        raise PackError(f'构建戳的提交 {stamp["commit"]} 不是 {sha[:12]}')
    return stamp


def pack(stage, platform, stamp_path, out, sha=None):
    stamp = check(stage, platform, stamp_path, sha)
    files = MEMBERS[platform]
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    tarball = out / f'summon-{platform}.tar.gz'
    build_json = out / f'summon-{platform}.build.json'
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode='w', format=tarfile.PAX_FORMAT) as tar:
        for name in files:
            path = Path(stage) / name
            entry = tarfile.TarInfo(name)
            entry.size = path.stat().st_size
            entry.mode = 0o755
            entry.mtime = stamp['commit_time']
            entry.uid = entry.gid = 0
            entry.uname = entry.gname = ''
            with open(path, 'rb') as f:
                tar.addfile(entry, f)
    with open(tarball, 'wb') as raw:
        with gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=0) as gz:
            gz.write(buf.getvalue())
    # 读回核对:文件集合与权限。
    with tarfile.open(tarball, 'r:gz') as tar:
        members = tar.getmembers()
    if sorted(m.name for m in members) != sorted(files):
        raise PackError(f'包里的文件不对:{[m.name for m in members]}')
    for m in members:
        if not m.isreg() or m.mode != 0o755:
            raise PackError(f'{m.name} 不是 0755 的普通文件')
    data = tarball.read_bytes()
    record = {
        'platform': platform,
        'version': stamp['version'],
        'commit': stamp['commit'],
        'commit_time': stamp['commit_time'],
        'name': tarball.name,
        'size': len(data),
        'sha256': hashlib.sha256(data).hexdigest(),
    }
    build_json.write_text(json.dumps(record, indent=2) + '\n', encoding='utf-8')
    return record


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest='cmd', required=True)
    for name in ('check', 'pack'):
        cmd = sub.add_parser(name)
        cmd.add_argument('--stage', required=True)
        cmd.add_argument('--platform', required=True)
        cmd.add_argument('--stamp', required=True)
        cmd.add_argument('--sha')
        if name == 'pack':
            cmd.add_argument('--out', required=True)
    args = parser.parse_args(argv)
    if args.sha is not None and not re.fullmatch(r'[0-9a-f]{40}', args.sha):
        print(f'pack: 不是完整的提交号:{args.sha!r}', file=sys.stderr)
        return 2
    try:
        if args.cmd == 'check':
            check(args.stage, args.platform, args.stamp, args.sha)
        else:
            record = pack(args.stage, args.platform, args.stamp, args.out, args.sha)
            print(json.dumps(record, indent=2))
    except PackError as err:
        print(f'pack: {err}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
