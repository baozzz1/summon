#!/usr/bin/env python3
"""发布工作流里所有改动 GitHub Release 的步骤:建草稿、收尾并转为发布。只用标准库,经 gh 访问 GitHub。

    python3 scripts/github-release/publish.py prepare --repo <owner/name> --tag <tag> --sha <提交>
    python3 scripts/github-release/publish.py finalize --repo <owner/name> --tag <tag> --sha <提交> \\
        --built <目录> --work <目录>

prepare 确认同名 tag 指向这个提交,再按 release_plan.plan_prepare 建草稿、沿用草稿或跳过(已发布;
跳过时把 latest 收敛到已发布的最高版本,修好转为发布之后才失败的那一轮),
标准输出只有一个词 `create` / `continue` / `skip`。演练 tag(test-…)的草稿标成 prerelease。

finalize 在三个平台都构建完之后跑。--built 里是本次构建的 `summon-<平台>.tar.gz` 与
`summon-<平台>.build.json`。步骤:
    ① 前提:Release 是草稿、目标是这个提交、没有上传到一半的附件;已发布且附件齐全时什么都不做
    ② 每个平台按 release_plan.plan_platform 沿用草稿里的一对、上传本次构建的一对,或逐字节比对后补传
       另一半;上传前先核对本次构建
    ③ 草稿里每个包与它的 .build.json 对得上(字节数、SHA-256),提交就是这个提交,包里恰是该平台的
       那几个文件、普通文件、0755
    ④ manifest.py 汇总出 manifest.json(构建戳不一致即失败);发版 tag 要求版本正是 tag 去掉 v;上传
    ⑤ 删掉草稿里的 .build.json 中间文件,附件只剩三个包与 manifest.json
    ⑥ 演练 tag 到此为止,停在草稿;发版 tag 确认 tag 仍指向这个提交,转为发布(不在这一步标 latest),
       再确认 tag 指向这个提交
    ⑦ 把 latest 收敛到已发布版本里最高的那个:两个版本同时收尾时,后完成的一方会看到双方都已发布,
       latest 不会停在较低的版本上
草稿里已有 manifest.json 时它是权威:只核对它与三个包,接着 ⑤ ⑥。任何一步失败都停在草稿;已发布的
Release 不改写。
"""
import argparse
import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tarfile
import time
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent


def _load(name):
    spec = importlib.util.spec_from_file_location(name, HERE / f'{name}.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


plan = _load('release_plan')
info = _load('release_info')
manifest_lib = _load('manifest')
MEMBERS = _load('pack').MEMBERS


class PublishError(Exception):
    pass


def say(message):
    # 进度写 stderr:prepare 的标准输出只有一个决定,工作流拿它当输出。
    print(f'[publish] {message}', file=sys.stderr, flush=True)


# ── gh ──

class Gh:
    """对 gh 的最小封装;测试换成内存里的假实现。"""

    def __init__(self, repo):
        self.repo = repo

    def _run(self, *args, check=True):
        proc = subprocess.run(['gh', *args], capture_output=True, text=True)
        if check and proc.returncode != 0:
            sys.stderr.write(proc.stderr)
            raise PublishError(f'gh {args[0]} {args[1] if len(args) > 1 else ""} 失败')
        return proc

    def view(self, tag):
        proc = self._run('release', 'view', tag, '--repo', self.repo, '--json',
                         'isDraft,tagName,targetCommitish,assets', check=False)
        if proc.returncode == 0:
            return json.loads(proc.stdout)
        if 'release not found' in proc.stderr:
            return None
        sys.stderr.write(proc.stderr)
        raise PublishError(f'查不出 Release {tag}')

    def latest_tag(self):
        proc = self._run('release', 'view', '--repo', self.repo, '--json', 'tagName', check=False)
        if proc.returncode == 0:
            return json.loads(proc.stdout)['tagName']
        if 'release not found' in proc.stderr:
            return None
        sys.stderr.write(proc.stderr)
        raise PublishError('查不出仓库当前的 latest Release')

    def _api(self, path):
        proc = self._run('api', path, check=False)
        if proc.returncode == 0:
            return json.loads(proc.stdout)
        if 'HTTP 404' in proc.stderr:
            return None
        sys.stderr.write(proc.stderr)
        raise PublishError(f'查询 {path} 失败')

    def tag_commit(self, tag):
        """远端 tag 指向的提交(附注 tag 一层层解到提交);tag 不存在返回 None。"""
        response = self._api(f'repos/{self.repo}/git/ref/tags/{tag}')
        if response is None:
            return None
        for _ in range(5):
            kind, sha = plan.git_object(response)
            if kind == 'commit':
                return sha
            if kind != 'tag':
                raise PublishError(f'tag {tag} 指向 {kind} {sha},不是提交')
            response = self._api(f'repos/{self.repo}/git/tags/{sha}')
            if response is None:
                raise PublishError(f'tag {tag} 的附注对象 {sha} 查不到')
        raise PublishError(f'tag {tag} 嵌套过深')

    def create_draft(self, tag, sha, title, notes_file, prerelease):
        args = ['release', 'create', tag, '--repo', self.repo, '--draft', '--verify-tag',
                '--target', sha, '--title', title, '--notes-file', str(notes_file)]
        if prerelease:
            args.append('--prerelease')
        self._run(*args)

    def download(self, tag, name, directory):
        target = Path(directory) / name
        if target.exists():
            target.unlink()
        self._run('release', 'download', tag, '--repo', self.repo, '--pattern', name,
                  '--dir', str(directory))
        if not target.is_file():
            raise PublishError(f'下载 {name} 后找不到文件')
        return target

    def upload(self, tag, path):
        self._run('release', 'upload', tag, str(path), '--repo', self.repo)

    def delete_asset(self, tag, name):
        self._run('release', 'delete-asset', tag, name, '--repo', self.repo, '--yes')

    def publish(self, tag):
        self._run('release', 'edit', tag, '--repo', self.repo, '--draft=false', '--latest=false')

    def published_tags(self):
        proc = self._run('release', 'list', '--repo', self.repo, '--exclude-drafts',
                         '--exclude-pre-releases', '--limit', '1000', '--json', 'tagName')
        return [r['tagName'] for r in json.loads(proc.stdout)]

    def set_latest(self, tag):
        self._run('release', 'edit', tag, '--repo', self.repo, '--latest')


# ── 核对 ──

def sha256(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            digest.update(chunk)
    return digest.hexdigest()


def check_file(path, name, size, digest):
    path = Path(path)
    if path.name != name:
        raise PublishError(f'{path}:文件名应为 {name}')
    actual = path.stat().st_size
    if actual != size:
        raise PublishError(f'{name}:字节数 {actual},记录的是 {size}')
    actual = sha256(path)
    if actual != digest:
        raise PublishError(f'{name}:SHA-256 {actual},记录的是 {digest}')


def check_members(path, platform):
    """包里恰是该平台的那几个文件:平铺、普通文件、0755。"""
    name = Path(path).name
    try:
        with tarfile.open(path, 'r:gz') as tar:
            members = tar.getmembers()
    except (OSError, tarfile.TarError) as err:
        raise PublishError(f'{name}:打不开({err})') from None
    names = [m.name for m in members]
    for m in members:
        if '/' in m.name or '\\' in m.name:
            raise PublishError(f'{name}:成员 {m.name} 带路径分隔符')
        if not m.isreg():
            raise PublishError(f'{name}:成员 {m.name} 不是普通文件')
        if m.mode & 0o7777 != 0o755:
            raise PublishError(f'{name}:成员 {m.name} 的模式是 {m.mode & 0o7777:o},应为 755')
    if len(names) != len(set(names)) or sorted(names) != sorted(MEMBERS[platform]):
        raise PublishError(f'{name}:成员是 {sorted(names)},应为 {sorted(MEMBERS[platform])}')


def check_commit(source, commit, sha):
    if not isinstance(commit, str) or len(commit) < 7 or not sha.startswith(commit):
        raise PublishError(f'{source}:提交 {commit} 不是目标提交 {sha[:12]}')


def load_json(path):
    try:
        with open(path, encoding='utf-8') as f:
            data = json.load(f)
    except (OSError, ValueError) as err:
        raise PublishError(f'读不了 {path}:{err}') from None
    if not isinstance(data, dict):
        raise PublishError(f'{path} 不是 JSON 对象')
    return data


def verify_build(directory, platform, sha):
    """目录里这个平台的包与 .build.json 对得上、提交是 sha;返回 .build.json 的内容。"""
    tarball, build_json = (Path(directory) / n for n in plan.pair(platform))
    for path in (tarball, build_json):
        if not path.is_file():
            raise PublishError(f'缺 {path}')
    build = load_json(build_json)
    if build.get('platform') != platform:
        raise PublishError(f'{build_json.name}:平台是 {build.get("platform")},应为 {platform}')
    check_commit(build_json.name, build.get('commit'), sha)
    check_file(tarball, build.get('name'), build.get('size'), build.get('sha256'))
    check_members(tarball, platform)
    return build


def verify_manifest(manifest, directory, sha):
    check_commit('manifest.json', manifest.get('commit'), sha)
    assets = manifest.get('assets') or {}
    if sorted(assets) != sorted(plan.PLATFORMS):
        raise PublishError(f'manifest.json 列的平台是 {sorted(assets)},应为 {sorted(plan.PLATFORMS)}')
    for platform in plan.PLATFORMS:
        asset = assets[platform]
        if not isinstance(asset, dict):
            raise PublishError(f'manifest.json 的 {platform} 不是对象')
        path = Path(directory) / plan.pair(platform)[0]
        check_file(path, asset.get('name'), asset.get('size'), asset.get('sha256'))
        check_members(path, platform)


def check_manifest_version(manifest, tag, kind):
    """发版 tag 发出去就是「这一版」:manifest 的版本必须正是 tag 去掉 v。演练 tag 不查。"""
    if kind == 'release' and manifest.get('version') != tag[1:]:
        raise PublishError(f'manifest.json 的版本是 {manifest.get("version")},与 tag {tag} 不符')


def same_bytes(a, b):
    return Path(a).stat().st_size == Path(b).stat().st_size and sha256(a) == sha256(b)


# ── 步骤 ──

def tag_kind(tag, sha):
    try:
        return info.tag_kind(tag, sha, info.read_version())
    except info.InfoError as err:
        raise PublishError(str(err)) from None


def prepare(gh, tag, sha, work):
    kind = tag_kind(tag, sha)
    tag_commit = gh.tag_commit(tag)
    try:
        decision = plan.plan_prepare(gh.view(tag), sha, tag_commit)
    except plan.PlanError as err:
        raise PublishError(str(err)) from None
    if decision == 'skip':
        say(f'Release {tag} 已发布,本次不构建、不上传')
        # 转为发布之后才失败的那一轮留下的 latest 在这里补齐,重跑整个工作流就能修好。
        if kind == 'release':
            say(f'latest 是 {ensure_latest(gh)}')
    elif decision == 'continue':
        say(f'沿用已有的草稿 {tag}(目标提交相同)')
    else:
        version = info.read_version()
        notes = Path(work) / 'notes.md'
        notes.parent.mkdir(parents=True, exist_ok=True)
        notes.write_text(info.notes_or_default(version) + '\n', encoding='utf-8')
        rehearsal = kind == 'rehearsal'
        title = f'summon {version}' + (f'(演练 {sha[:7]})' if rehearsal else '')
        gh.create_draft(tag, sha, title, notes, prerelease=rehearsal)
        # 收尾按草稿的目标提交认草稿;在构建之前就确认 GitHub 记下的正是这个提交。
        try:
            plan.check_draft(refetch(gh, tag), sha)
        except plan.PlanError as err:
            raise PublishError(f'新建的草稿不对:{err}') from None
        say(f'已建草稿 {tag}' + (',标为 prerelease(演练)' if rehearsal else ''))
    return decision


def refetch(gh, tag):
    release = gh.view(tag)
    if release is None:
        raise PublishError(f'Release {tag} 不见了')
    return release


def place_platform(gh, tag, release, platform, built, check, sha):
    """把这个平台的一对附件落到草稿里,并把草稿里的那一对放进 check 目录。"""
    action, present = plan.plan_platform(release, platform)
    names = plan.pair(platform)
    if action == 'adopt':
        say(f'{platform}:草稿里已有这一对,沿用(本次构建不上传)')
        for name in names:
            gh.download(tag, name, check)
        return
    # 要上传本次构建的,先核对它本身。
    verify_build(built, platform, sha)
    if action == 'compare':
        probe = Path(check) / '.probe'
        probe.mkdir(parents=True, exist_ok=True)
        for name in present:
            gh.download(tag, name, probe)
            if not same_bytes(probe / name, Path(built) / name):
                raise PublishError(f'草稿里已有 {name} 且与本次构建不同,不覆盖;确认无用后'
                                   f' gh release delete-asset {tag} {name} 再重跑')
            say(f'{name} 已在草稿里且内容相同')
        shutil.rmtree(probe)
    for name in names:
        if name not in present:
            gh.upload(tag, Path(built) / name)
            say(f'已上传 {name}')
        shutil.copyfile(Path(built) / name, Path(check) / name)


def finalize(gh, tag, sha, built, work, published_at=None, recheck_retries=5, recheck_interval=3):
    kind = tag_kind(tag, sha)
    release = gh.view(tag)
    if release is None:
        raise PublishError(f'Release {tag} 不存在(prepare 没建成?)')
    if release.get('isDraft') is not True:
        if plan.published_complete(release):
            say(f'{tag} 已发布且附件齐全,无事可做')
            return 'already-published'
        raise PublishError(f'{tag} 已发布但附件不齐;已发布的 Release 不改写,去 GitHub 上看')
    try:
        plan.check_draft(release, sha)
    except plan.PlanError as err:
        raise PublishError(str(err)) from None

    check = Path(work) / 'check'
    if check.exists():
        shutil.rmtree(check)
    check.mkdir(parents=True)

    if plan.MANIFEST in plan.uploaded_names(release):
        say('manifest.json 已在草稿里,核对它与三个包')
        manifest = load_json(gh.download(tag, plan.MANIFEST, check))
        for platform in plan.PLATFORMS:
            name = plan.pair(platform)[0]
            if name not in plan.uploaded_names(release):
                raise PublishError(f'manifest.json 已上传,却缺 {name}')
            gh.download(tag, name, check)
        verify_manifest(manifest, check, sha)
        check_manifest_version(manifest, tag, kind)
    else:
        for platform in plan.PLATFORMS:
            place_platform(gh, tag, release, platform, built, check, sha)
        builds = [(plan.pair(p)[1], verify_build(check, p, sha)) for p in plan.PLATFORMS]
        say('三个包的字节数、SHA-256、提交与成员都核对过')
        try:
            manifest = manifest_lib.build_manifest(
                builds, published_at or datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'))
        except manifest_lib.ManifestError as err:
            raise PublishError(f'生成 manifest.json 失败:{err}') from None
        verify_manifest(manifest, check, sha)
        check_manifest_version(manifest, tag, kind)
        path = check / plan.MANIFEST
        path.write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
        gh.upload(tag, path)
        say('已上传 manifest.json')

    # ⑤ 删掉中间文件,发布出去的只有三个包与 manifest.json
    release = refetch(gh, tag)
    for name in sorted(plan.uploaded_names(release)):
        if name.endswith('.build.json'):
            gh.delete_asset(tag, name)
            say(f'已删除中间文件 {name}')
    release = refetch(gh, tag)
    plan.check_draft(release, sha)
    if not plan.published_complete(release):
        raise PublishError(f'草稿的附件应只有 {", ".join(plan.published_assets())},'
                           f'实际:{", ".join(sorted(plan.uploaded_names(release)))}')

    if kind == 'rehearsal':
        say(f'演练 tag {tag}:草稿已就绪,不转为发布')
        return 'draft-ready'

    # ⑥ 转为发布。tag 被挪走后 GitHub 发布会沿用新的位置,附件却来自这个提交 —— 不能发。
    try:
        plan.check_tag_commit(gh.tag_commit(tag), sha)
    except plan.PlanError as err:
        raise PublishError(str(err)) from None
    gh.publish(tag)
    release = refetch(gh, tag)
    if release.get('isDraft') is not False:
        raise PublishError('转为发布后它仍是草稿')
    # 已经发布了:再出问题要人工处理。查询失败可能只是还没传开,重试;指向别处不会自己变好。
    for attempt in range(recheck_retries + 1):
        try:
            tag_commit = gh.tag_commit(tag)
        except PublishError:
            tag_commit = None
        if tag_commit is not None:
            break
        if attempt < recheck_retries:
            time.sleep(recheck_interval)
    if tag_commit != sha:
        raise PublishError(f'{tag} 已发布,但 tag 指向 {tag_commit or "(查不到)"} 而不是 {sha};'
                           '需要人工处理这个 Release 与 tag')
    latest = ensure_latest(gh)
    say(f'已发布 {tag};latest 是 {latest}')
    return 'published'


def ensure_latest(gh):
    """latest 指向已发布版本里最高的那个;返回它。"""
    try:
        highest = plan.highest_release(gh.published_tags())
    except plan.PlanError as err:
        raise PublishError(str(err)) from None
    if highest is None:
        raise PublishError('没有找到已发布的版本,latest 无从设置')
    if gh.latest_tag() != highest:
        gh.set_latest(highest)
        if gh.latest_tag() != highest:
            raise PublishError(f'已发布,但 latest 没能指向最高版本 {highest};需要人工处理')
    return highest


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest='cmd', required=True)
    for name in ('prepare', 'finalize'):
        cmd = sub.add_parser(name)
        cmd.add_argument('--repo', required=True)
        cmd.add_argument('--tag', required=True)
        cmd.add_argument('--sha', required=True)
        cmd.add_argument('--work', required=True, type=Path)
        if name == 'finalize':
            cmd.add_argument('--built', required=True, type=Path)
    args = parser.parse_args(argv)
    gh = Gh(args.repo)
    try:
        if args.cmd == 'prepare':
            print(prepare(gh, args.tag, args.sha, args.work))
        else:
            finalize(gh, args.tag, args.sha, args.built, args.work)
    except (PublishError, plan.PlanError) as err:
        print(f'::error::{err}', file=sys.stderr)
        if args.cmd == 'finalize':
            print(f'[publish] Release {args.tag} 停在原状态(草稿或已发布的不改写);'
                  '修好后重跑失败的 job', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
