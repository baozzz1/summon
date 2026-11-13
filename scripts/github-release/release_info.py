#!/usr/bin/env python3
"""发布工作流要的几项信息:版本号、Release 的 tag、Release 正文。只用标准库。

    python3 scripts/github-release/release_info.py version
    python3 scripts/github-release/release_info.py tag --ref-name main --sha <提交>
    python3 scripts/github-release/release_info.py notes --version 1.2.3

tag:`main` 上是 `v<版本>`;其它分支是 `test-<提交前 7 位>`,只用于演练,不会转为发布。
正文取 RELEASE_NOTES.md 里该版本那一节(`## <版本> · <日期>` 到下一个 `## `),没有这一节就写一句
通用说明。
"""
import argparse
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RELEASE_BRANCH = 'main'


class InfoError(Exception):
    pass


def read_version(root=ROOT):
    path = root / 'crates/summon/Cargo.toml'
    data = tomllib.loads(path.read_text(encoding='utf-8'))
    version = data.get('package', {}).get('version')
    if not isinstance(version, str) or not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+\S*', version):
        raise InfoError(f'{path} 的 [package].version 不是版本号:{version!r}')
    return version


def release_tag(version, ref_name, sha):
    if not re.fullmatch(r'[0-9a-f]{40}', sha or ''):
        raise InfoError(f'不是完整的提交号:{sha!r}')
    if ref_name == RELEASE_BRANCH:
        return f'v{version}'
    return f'test-{sha[:7]}'


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
    tag = sub.add_parser('tag')
    tag.add_argument('--ref-name', required=True)
    tag.add_argument('--sha', required=True)
    notes = sub.add_parser('notes')
    notes.add_argument('--version', required=True)
    args = parser.parse_args(argv)
    try:
        if args.cmd == 'version':
            print(read_version())
        elif args.cmd == 'tag':
            print(release_tag(read_version(), args.ref_name, args.sha))
        else:
            print(notes_or_default(args.version))
    except InfoError as err:
        print(f'release_info: {err}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
