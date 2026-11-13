"""发布工作流对已有 Release 的判定:纯函数,不调 gh、不联网。只用标准库。

publish.py 先用 gh 把 Release、tag 的现状读出来,再按这里的判定决定做什么。

`release` 参数是 `gh release view <tag> --json isDraft,targetCommitish,assets` 的输出(字典),
不存在时为 None。

prepare(建草稿之前):
    create    Release 不存在,建草稿
    skip      已发布,这一轮什么都不做
    continue  是草稿且目标就是这个提交(上一轮没做完),沿用它
目标提交不同的草稿不动它,失败。同名 tag 不指向这个提交时也失败:GitHub 发布时沿用已有的 tag、
忽略草稿的目标提交,发出去的 Release 会指向别的提交,附件却来自这个提交。

收尾时每个平台的一对附件(`summon-<平台>.tar.gz` 与 `summon-<平台>.build.json`):
    adopt     一对都在草稿里:用草稿里的(重新构建的包字节不同,不覆盖已有的)
    upload    一对都不在:上传本次构建的
    compare   只有一半:本次构建与草稿里那一半逐字节相同才补传另一半(只重跑失败 job 时产物相同),
              不同就失败,不覆盖
草稿里有没上传完的附件、已不是草稿、目标提交不同,一律失败。
"""
import re

MANIFEST = 'manifest.json'
PLATFORMS = ('linux-x64', 'linux-arm64', 'darwin-arm64')
FULL_SHA = re.compile(r'[0-9a-f]{40}')
RELEASE_VERSION = re.compile(r'v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)')


class PlanError(Exception):
    pass


def check_sha(sha):
    if not FULL_SHA.fullmatch(sha or ''):
        raise PlanError(f'不是完整的提交号:{sha!r}')


def check_target(release, sha):
    target = release.get('targetCommitish')
    if target != sha:
        raise PlanError(f'草稿的目标提交是 {target},本次是 {sha};不动它,'
                        '确认无用后删掉草稿再重跑')


def check_tag_commit(tag_commit, sha):
    if tag_commit != sha:
        raise PlanError(f'同名 tag 指向 {tag_commit or "(不存在)"},不是本次 {sha};'
                        'GitHub 发布时会沿用 tag,先查清这个 tag')


def plan_prepare(release, sha, tag_commit):
    """release 为 None 表示不存在;tag_commit 是同名 tag 现在指向的提交(不存在为 None)。"""
    check_sha(sha)
    if release is not None and release.get('isDraft') is not True:
        return 'skip'
    check_tag_commit(tag_commit, sha)
    if release is None:
        return 'create'
    check_target(release, sha)
    return 'continue'


def git_object(response):
    """GitHub git/ref 或 git/tags 响应 → (类型, sha)。"""
    obj = response.get('object') if isinstance(response, dict) else None
    if not isinstance(obj, dict) or not isinstance(obj.get('type'), str) \
            or not FULL_SHA.fullmatch(obj.get('sha') or ''):
        raise PlanError(f'认不出的 git 对象响应:{response!r}')
    return obj['type'], obj['sha']


def asset_states(release):
    return {a['name']: a.get('state', 'uploaded') for a in release.get('assets') or []}


def uploaded_names(release):
    """上传完成的附件名;上传中断留下的不算。"""
    return {n for n, s in asset_states(release).items() if s == 'uploaded'}


def pair(platform):
    return f'summon-{platform}.tar.gz', f'summon-{platform}.build.json'


def published_assets():
    """发布出去的 Release 恰好有这些附件。"""
    return sorted([MANIFEST] + [pair(p)[0] for p in PLATFORMS])


def published_complete(release):
    return sorted(uploaded_names(release)) == published_assets()


def check_draft(release, sha):
    """收尾的前提:是草稿、目标是这个提交、没有上传到一半的附件。"""
    check_sha(sha)
    if release.get('isDraft') is not True:
        raise PlanError('Release 已不是草稿,不改动它')
    check_target(release, sha)
    unfinished = sorted(n for n, s in asset_states(release).items() if s != 'uploaded')
    if unfinished:
        raise PlanError(f'草稿里有没上传完的附件:{", ".join(unfinished)};删掉它们再重跑')


def plan_platform(release, platform):
    """('adopt' | 'upload' | 'compare', 草稿里已有的那几个文件名)。"""
    names = uploaded_names(release)
    present = [n for n in pair(platform) if n in names]
    if len(present) == 2:
        return 'adopt', present
    if not present:
        return 'upload', present
    return 'compare', present


def version_key(text):
    match = RELEASE_VERSION.fullmatch(text or '')
    if not match:
        raise PlanError(f'认不出的版本号:{text!r}')
    return tuple(int(part) for part in match.groups())


def highest_release(tags):
    """已发布(非草稿、非预发布)的 tag 里版本最高的那个;认不出版本号的 tag 不参与。没有就是 None。"""
    best = None
    for tag in tags:
        if not RELEASE_VERSION.fullmatch(tag or '') or not tag.startswith('v'):
            continue
        if best is None or version_key(tag) > version_key(best):
            best = tag
    return best
