#!/usr/bin/env python3
"""发布工作流要的几项信息:版本号、tag 的种类、Release 正文。只用标准库。

    python3 scripts/github-release/release_info.py version
    python3 scripts/github-release/release_info.py check-tag --tag v1.2.3 --sha <提交>
    python3 scripts/github-release/release_info.py notes --version 1.2.3

check-tag 输出 tag 的种类:
    release    `v<版本>`:版本必须是纯数字的 x.y.z,且等于 crates/summon/Cargo.toml 的版本
    rehearsal  `test-<提交号前缀>`(7 到 40 位):前缀必须是这个提交的,只走演练、停在草稿
其它形态失败。两种 tag 都必须在 main 上,由工作流用 git 查。
正文取 RELEASE_NOTES.md 里该版本那一节(`## <版本> · <日期>` 到下一个 `## `),没有这一节就写一句
通用说明。
"""
import argparse
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RELEASE_TAG = re.compile(r'v((?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))')
REHEARSAL_TAG = re.compile(r'test-([0-9a-f]{7,40})')


class InfoError(Exception):
    pass


def read_version(root=ROOT):
    path = root / 'crates/summon/Cargo.toml'
    data = tomllib.loads(path.read_text(encoding='utf-8'))
    version = data.get('package', {}).get('version')
    if not isinstance(version, str) or not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+\S*', version):
        raise InfoError(f'{path} 的 [package].version 不是版本号:{version!r}')
    return version


def tag_kind(tag, sha, version):
    """'release' 或 'rehearsal';tag 与提交、版本对不上就抛 InfoError。"""
    if not re.fullmatch(r'[0-9a-f]{40}', sha or ''):
        raise InfoError(f'不是完整的提交号:{sha!r}')
    match = RELEASE_TAG.fullmatch(tag or '')
    if match:
        # 已装的启动器只认纯 x.y.z 的版本号,发版 tag 与 Cargo.toml 的版本都必须是这个形态。
        if match.group(1) != version:
            raise InfoError(f'tag {tag} 与 crates/summon/Cargo.toml 的版本 {version} 不符')
        return 'release'
    match = REHEARSAL_TAG.fullmatch(tag or '')
    if match:
        if not sha.startswith(match.group(1)):
            raise InfoError(f'演练 tag {tag} 不是提交 {sha} 的前缀')
        return 'rehearsal'
    raise InfoError(f'不认识的 tag:{tag!r}(发版用 v<x.y.z>,演练用 test-<提交号前缀>)')


def release_notes(version, text):
    """RELEASE_NOTES.md 里该版本那一节的正文(不含标题行);没有这一节返回 None。"""
    lines = text.splitlines()
    heading = re.compile(rf'^## {re.escape(version)}(?:\s|$)')
    for start, line in enumerate(lines):
        if heading.match(line):
            body = []
            for line in lines[start + 1:]:
                if line.startswith('## '):
                    break
                body.append(line)
            notes = '\n'.join(body).strip()
            return notes or None
    return None


def notes_or_default(version, root=ROOT):
    path = root / 'RELEASE_NOTES.md'
    text = path.read_text(encoding='utf-8') if path.exists() else ''
    notes = release_notes(version, text)
    if notes is None:
        return f'summon {version}。变更说明见仓库里的 RELEASE_NOTES.md。'
    return notes


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest='cmd', required=True)
    sub.add_parser('version')
    tag = sub.add_parser('check-tag')
    tag.add_argument('--tag', required=True)
    tag.add_argument('--sha', required=True)
    notes = sub.add_parser('notes')
    notes.add_argument('--version', required=True)
    args = parser.parse_args(argv)
    try:
        if args.cmd == 'version':
            print(read_version())
        elif args.cmd == 'check-tag':
            print(tag_kind(args.tag, args.sha, read_version()))
        else:
            print(notes_or_default(args.version))
    except InfoError as err:
        print(f'release_info: {err}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
