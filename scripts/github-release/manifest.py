#!/usr/bin/env python3
"""把各平台的 `summon-<平台>.build.json` 汇总成 GitHub Release 的 `manifest.json`。

    python3 scripts/github-release/manifest.py --published-at 2026-01-02T03:04:05Z \\
        --out manifest.json summon-linux-x64.build.json summon-linux-arm64.build.json ...

输出:
    {"version", "commit", "commit_time", "published_at",
     "assets": {"<平台>": {"name", "size", "sha256"}}}

升级只认 manifest:它没列出的平台就当这一版对该平台不存在。所以各输入必须出自同一个提交:版本、
提交时间完全一致,提交号按前缀相同(短 sha 的长度随仓库对象数变,CI 的浅克隆常是 7 位、完整克隆
8 位;与 summon 比较构建戳的判据相同),manifest 取其中最长的那个。平台不得重复,发布用的三个平台
缺一个就失败;`--allow-missing-platforms` 只给演练用。只用标准库。
"""
import argparse
import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

REQUIRED_PLATFORMS = ('linux-x64', 'linux-arm64', 'darwin-arm64')
EXACT_FIELDS = ('version', 'commit_time')
SHA256 = re.compile(r'[0-9a-f]{64}')
COMMIT = re.compile(r'[0-9a-f]{7,40}')
VERSION = re.compile(r'[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?')
PUBLISHED_AT = re.compile(r'\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z')


class ManifestError(Exception):
    pass


def same_commit(a, b):
    """同一个提交:两个短 sha 一个是另一个的前缀。"""
    return a.startswith(b) or b.startswith(a)


def check_build(build, source):
    """一份 .build.json 的形状;不对就抛 ManifestError,说明是哪份、哪个字段。"""
    if not isinstance(build, dict):
        raise ManifestError(f'{source}:不是 JSON 对象')
    platform = build.get('platform')
    if platform not in REQUIRED_PLATFORMS:
        raise ManifestError(f'{source}:不认识的平台 {platform!r}')
    checks = {
        'version': lambda v: isinstance(v, str) and VERSION.fullmatch(v),
        'commit': lambda v: isinstance(v, str) and COMMIT.fullmatch(v),
        'commit_time': lambda v: type(v) is int and v > 0,
        'name': lambda v: v == f'summon-{platform}.tar.gz',
        'size': lambda v: type(v) is int and v > 0,
        'sha256': lambda v: isinstance(v, str) and SHA256.fullmatch(v),
    }
    for field, ok in checks.items():
        if not ok(build.get(field)):
            raise ManifestError(f'{source}:{field}={build.get(field)!r} 不合法')


def build_manifest(builds, published_at, require_all=True):
    """builds 是 [(来源, .build.json 内容)];返回 manifest 字典。"""
    if not builds:
        raise ManifestError('没有输入')
    if not PUBLISHED_AT.fullmatch(published_at or ''):
        raise ManifestError(f'published_at={published_at!r} 不是 YYYY-MM-DDTHH:MM:SSZ')
    assets = {}
    first_source, first = builds[0]
    # 与已见过的最长提交号比:此前的都是它的前缀,与它前缀相同就与此前每一份都前缀相同,
    # 结论不依赖输入顺序。
    commit, commit_source = None, None
    for source, build in builds:
        check_build(build, source)
        for field in EXACT_FIELDS:
            if build[field] != first[field]:
                raise ManifestError(
                    f'构建戳不一致:{source} 的 {field}={build[field]!r},'
                    f'{first_source} 的是 {first[field]!r}')
        if commit is not None and not same_commit(build['commit'], commit):
            raise ManifestError(
                f'构建戳不一致:{source} 的 commit={build["commit"]!r},'
                f'{commit_source} 的是 {commit!r}')
        if commit is None or len(build['commit']) > len(commit):
            commit, commit_source = build['commit'], source
        platform = build['platform']
        if platform in assets:
            raise ManifestError(f'平台 {platform} 重复({source})')
        assets[platform] = {'name': build['name'], 'size': build['size'],
                            'sha256': build['sha256']}
    missing = [p for p in REQUIRED_PLATFORMS if p not in assets]
    if missing and require_all:
        raise ManifestError(f'缺平台:{", ".join(missing)}')
    return {
        'version': first['version'],
        'commit': commit,
        'commit_time': first['commit_time'],
        'published_at': published_at,
        'assets': {p: assets[p] for p in sorted(assets)},
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description='汇总 .build.json 生成 manifest.json')
    parser.add_argument('builds', nargs='+', type=Path, help='各平台的 summon-<平台>.build.json')
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--published-at',
                        default=datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'),
                        help='发布时间,UTC,形如 2026-01-02T03:04:05Z(默认现在)')
    parser.add_argument('--allow-missing-platforms', action='store_true',
                        help='缺平台也生成(只给演练用,这样的 manifest 不可发布)')
    args = parser.parse_args(argv)
    try:
        builds = []
        for path in args.builds:
            try:
                builds.append((path.name, json.loads(path.read_text(encoding='utf-8'))))
            except (OSError, ValueError) as err:
                raise ManifestError(f'{path}:读不了({err})') from None
        manifest = build_manifest(builds, args.published_at,
                                  require_all=not args.allow_missing_platforms)
    except ManifestError as err:
        print(f'manifest: {err}', file=sys.stderr)
        return 1
    args.out.write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
    return 0


if __name__ == '__main__':
    sys.exit(main())
