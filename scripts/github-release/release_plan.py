#!/usr/bin/env python3
"""发布工作流对一个已有 Release 该怎么做:只读判定,不调 gh、不联网。只用标准库。

    python3 scripts/github-release/release_plan.py prepare --sha <提交> [--release-json <文件>] \\
        (--tag-commit <提交> | --tag-absent)
    python3 scripts/github-release/release_plan.py upload --sha <提交> --release-json <文件> \\
        --platform linux-x64 --platform linux-arm64
    python3 scripts/github-release/release_plan.py git-object --json <文件>

`--release-json` 是 `gh release view <tag> --json isDraft,targetCommitish,assets` 的输出。
`--tag-commit` 是同名 git tag 指向的提交(附注 tag 已解到提交),tag 不存在时给 `--tag-absent`。
`git-object` 从 GitHub `git/ref/...` 或 `git/tags/...` 接口的响应里取出 `<类型> <sha>`,供工作流
把附注 tag 一层层解到提交。

prepare 输出一个词:
    create    Release 不存在(没给 --release-json),建草稿
    skip      已发布,这一轮什么都不做
    continue  是草稿且目标就是这个提交(上一轮没做完),沿用它接着构建上传
目标提交不同的草稿不动它,失败。同名 tag 已存在却不指向这个提交时也失败:GitHub 发布时沿用已有的
tag、忽略草稿的目标提交,发出去的 Release 会指向旧提交,附件却来自新提交。已发布的不再查 tag。

upload 输出要上传的文件名(一行一个,可以为空)。按平台成对判断 `summon-<平台>.tar.gz` 与
`summon-<平台>.build.json`:两个都在就跳过这个平台;两个都不在就上传这一对;只有一个就失败并说该删
哪个 —— 重新构建的包字节不同,配不上旧的那一半,所以不覆盖、不补传。草稿里已有 manifest.json
(已经汇总过旧的包)、已不是草稿、目标提交不同,一律失败。
"""
import argparse
import json
import re
import sys

MANIFEST = 'manifest.json'


class PlanError(Exception):
    pass


def check_sha(sha):
    if not re.fullmatch(r'[0-9a-f]{40}', sha or ''):
        raise PlanError(f'不是完整的提交号:{sha!r}')


def check_target(release, sha):
    target = release.get('targetCommitish')
    if target != sha:
        raise PlanError(f'草稿的目标提交是 {target},本次是 {sha};不动它,'
                        '确认无用后删掉草稿再重跑')


def plan_prepare(release, sha, tag_commit):
    """release 为 None 表示不存在;tag_commit 为 None 表示同名 tag 不存在。"""
    check_sha(sha)
    if release is not None and release.get('isDraft') is not True:
        return 'skip'
    if tag_commit is not None and tag_commit != sha:
        raise PlanError(f'同名 tag 已存在,指向 {tag_commit},不是本次 {sha};GitHub 发布时会沿用它,'
                        '先删掉这个 tag 或换版本号')
    if release is None:
        return 'create'
    check_target(release, sha)
    return 'continue'


def git_object(response):
    """GitHub git/ref 或 git/tags 响应 → (类型, sha)。"""
    obj = response.get('object') if isinstance(response, dict) else None
    if not isinstance(obj, dict) or not isinstance(obj.get('type'), str) \
            or not re.fullmatch(r'[0-9a-f]{40}', obj.get('sha') or ''):
        raise PlanError(f'认不出的 git 对象响应:{response!r}')
    return obj['type'], obj['sha']


def asset_states(release):
    return {a['name']: a.get('state', 'uploaded') for a in release.get('assets') or []}


def plan_upload(release, sha, platforms):
    """返回 (要上传的文件名, 说明);不能上传时抛 PlanError。"""
    check_sha(sha)
    if release.get('isDraft') is not True:
        raise PlanError('Release 已不是草稿,不往上传')
    check_target(release, sha)
    states = asset_states(release)
    if MANIFEST in states:
        raise PlanError('草稿里已有 manifest.json(已汇总过旧的包),不再上传;'
                        '要重做就删掉草稿再重跑')
    unfinished = sorted(n for n, s in states.items() if s != 'uploaded')
    if unfinished:
        raise PlanError(f'草稿里有没上传完的附件:{", ".join(unfinished)};删掉它们再重跑')
    uploads, notes = [], []
    for platform in platforms:
        pair = [f'summon-{platform}.tar.gz', f'summon-{platform}.build.json']
        present = [n for n in pair if n in states]
        if len(present) == 2:
            notes.append(f'{platform}:两个附件都已在草稿里,跳过')
        elif not present:
            uploads.extend(pair)
        else:
            raise PlanError(f'{platform} 只有 {present[0]} 在草稿里,配不上新构建的另一半;'
                            f'删掉 {present[0]} 再重跑')
    return uploads, notes


def load_release(path):
    try:
        with open(path, encoding='utf-8') as f:
            release = json.load(f)
    except (OSError, ValueError) as err:
        raise PlanError(f'读不了 {path}:{err}') from None
    if not isinstance(release, dict):
        raise PlanError(f'{path} 不是 JSON 对象')
    return release


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest='cmd', required=True)
    prepare = sub.add_parser('prepare')
    prepare.add_argument('--sha', required=True)
    prepare.add_argument('--release-json')
    tag = prepare.add_mutually_exclusive_group(required=True)
    tag.add_argument('--tag-commit')
    tag.add_argument('--tag-absent', action='store_true')
    upload = sub.add_parser('upload')
    upload.add_argument('--sha', required=True)
    upload.add_argument('--release-json', required=True)
    upload.add_argument('--platform', action='append', required=True)
    obj = sub.add_parser('git-object')
    obj.add_argument('--json', required=True)
    args = parser.parse_args(argv)
    try:
        if args.cmd == 'prepare':
            release = load_release(args.release_json) if args.release_json else None
            if args.tag_commit is not None:
                check_sha(args.tag_commit)
            print(plan_prepare(release, args.sha, args.tag_commit))
        elif args.cmd == 'git-object':
            print(*git_object(load_release(args.json)))
        else:
            uploads, notes = plan_upload(load_release(args.release_json), args.sha, args.platform)
            for note in notes:
                print(note, file=sys.stderr)
            for name in uploads:
                print(name)
    except PlanError as err:
        print(f'release_plan: {err}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
