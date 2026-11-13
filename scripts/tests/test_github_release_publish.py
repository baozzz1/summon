"""scripts/github-release/publish.py 的测试:gh 换成内存里的假 GitHub。

    python3 scripts/tests/test_github_release_publish.py
"""
import gzip
import hashlib
import importlib.util
import io
import json
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('publish', ROOT / 'scripts/github-release/publish.py')
publish = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publish)
plan = publish.plan

SHA = '0123456789abcdef0123456789abcdef01234567'
OTHER = 'f' * 40
VERSION = publish.info.read_version()
TAG = f'v{VERSION}'
TEST_TAG = f'test-{SHA[:7]}'
COMMIT_TIME = 1_700_000_000


class FakeGh:
    """只实现 publish.py 用到的那几个动作;附件内容存在内存里。"""

    def __init__(self, tag_commits=None, latest=None):
        self.releases = {}
        self.tag_commits = dict(tag_commits or {})
        self.latest = latest
        self.calls = []

    def add_release(self, tag, draft=True, target=SHA, assets=None, states=None):
        self.releases[tag] = {'isDraft': draft, 'targetCommitish': target, 'tagName': tag,
                              'assets': dict(assets or {}), 'states': dict(states or {}),
                              'prerelease': False}

    def assets(self, tag):
        return self.releases[tag]['assets']

    def view(self, tag):
        release = self.releases.get(tag)
        if release is None:
            return None
        return {'isDraft': release['isDraft'], 'targetCommitish': release['targetCommitish'],
                'tagName': tag,
                'assets': [{'name': n, 'state': release['states'].get(n, 'uploaded')}
                           for n in release['assets']]}

    def latest_tag(self):
        return self.latest

    def tag_commit(self, tag):
        return self.tag_commits.get(tag)

    def create_draft(self, tag, sha, title, notes_file, prerelease):
        self.calls.append(('create', tag, sha, title, prerelease))
        self.add_release(tag, target=sha)
        self.releases[tag]['prerelease'] = prerelease
        self.releases[tag]['notes'] = Path(notes_file).read_text(encoding='utf-8')

    def download(self, tag, name, directory):
        self.calls.append(('download', name))
        path = Path(directory) / name
        path.write_bytes(self.assets(tag)[name])
        return path

    def upload(self, tag, path):
        path = Path(path)
        self.calls.append(('upload', path.name))
        assert path.name not in self.assets(tag), f'重复上传 {path.name}'
        self.assets(tag)[path.name] = path.read_bytes()

    def delete_asset(self, tag, name):
        self.calls.append(('delete', name))
        del self.assets(tag)[name]

    def publish(self, tag, latest):
        self.calls.append(('publish', tag, latest))
        self.releases[tag]['isDraft'] = False

    def mutations(self):
        return [c for c in self.calls if c[0] != 'download']


def make_tarball(members, mode=0o755, mtime=COMMIT_TIME):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode='w', format=tarfile.PAX_FORMAT) as tar:
        for name in members:
            data = f'{name} {mtime}'.encode()
            entry = tarfile.TarInfo(name)
            entry.size = len(data)
            entry.mode = mode
            entry.mtime = mtime
            tar.addfile(entry, io.BytesIO(data))
    out = io.BytesIO()
    with gzip.GzipFile(filename='', mode='wb', fileobj=out, mtime=0) as gz:
        gz.write(buf.getvalue())
    return out.getvalue()


def build_record(platform, data, commit=SHA[:8], version=VERSION):
    return {'platform': platform, 'version': version, 'commit': commit,
            'commit_time': COMMIT_TIME, 'name': f'summon-{platform}.tar.gz', 'size': len(data),
            'sha256': hashlib.sha256(data).hexdigest()}


def make_pair(platform, salt=0, commit=SHA[:8], version=VERSION, members=None):
    """(tar.gz 字节, .build.json 字节);salt 不同的两份内容不同(模拟重新构建)。"""
    data = make_tarball(members or publish.MEMBERS[platform], mtime=COMMIT_TIME + salt)
    record = build_record(platform, data, commit, version)
    return data, (json.dumps(record, indent=2) + '\n').encode()


class FinalizeTests(unittest.TestCase):
    def setUp(self):
        self.temp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.temp)
        self.built = self.temp / 'built'
        self.built.mkdir()
        self.work = self.temp / 'work'
        for platform in plan.PLATFORMS:
            self.write_built(platform)
        self.stderr = io.StringIO()
        patcher = redirect_stderr(self.stderr)
        patcher.__enter__()
        self.addCleanup(patcher.__exit__, None, None, None)

    def write_built(self, platform, **kwargs):
        data, record = make_pair(platform, **kwargs)
        tarball, build_json = plan.pair(platform)
        (self.built / tarball).write_bytes(data)
        (self.built / build_json).write_bytes(record)

    def built_bytes(self, platform):
        return {n: (self.built / n).read_bytes() for n in plan.pair(platform)}

    def gh(self, tag=TAG, latest=None, **release):
        gh = FakeGh(tag_commits={tag: SHA}, latest=latest)
        gh.add_release(tag, **release)
        return gh

    def finalize(self, gh, tag=TAG):
        return publish.finalize(gh, tag, SHA, self.built, self.work,
                                published_at='2026-01-02T03:04:05Z', recheck_interval=0)

    def manifest(self, gh, tag=TAG):
        return json.loads(gh.assets(tag)['manifest.json'])

    def test_empty_draft_uploads_everything_then_publishes(self):
        gh = self.gh()
        self.assertEqual(self.finalize(gh), 'published')
        self.assertEqual(sorted(gh.assets(TAG)), plan.published_assets())
        self.assertFalse(gh.releases[TAG]['isDraft'])
        uploads = [c[1] for c in gh.calls if c[0] == 'upload']
        self.assertEqual(uploads[-1], 'manifest.json')
        self.assertEqual(len(uploads), 7)
        self.assertEqual(sorted(c[1] for c in gh.calls if c[0] == 'delete'),
                         sorted(plan.pair(p)[1] for p in plan.PLATFORMS))
        self.assertEqual(gh.calls[-1], ('publish', TAG, True))
        manifest = self.manifest(gh)
        self.assertEqual(manifest['version'], VERSION)
        self.assertEqual(manifest['published_at'], '2026-01-02T03:04:05Z')
        for platform in plan.PLATFORMS:
            data = gh.assets(TAG)[plan.pair(platform)[0]]
            self.assertEqual(manifest['assets'][platform]['sha256'],
                             hashlib.sha256(data).hexdigest())

    def test_rehearsal_tag_stops_at_draft(self):
        gh = self.gh(tag=TEST_TAG)
        self.assertEqual(self.finalize(gh, TEST_TAG), 'draft-ready')
        self.assertTrue(gh.releases[TEST_TAG]['isDraft'])
        self.assertEqual(sorted(gh.assets(TEST_TAG)), plan.published_assets())
        self.assertNotIn('publish', [c[0] for c in gh.calls])

    def test_complete_pair_in_draft_is_adopted_not_overwritten(self):
        old_data, old_record = make_pair('darwin-arm64', salt=7)
        gh = self.gh(assets={'summon-darwin-arm64.tar.gz': old_data,
                             'summon-darwin-arm64.build.json': old_record})
        self.finalize(gh)
        uploads = [c[1] for c in gh.calls if c[0] == 'upload']
        self.assertNotIn('summon-darwin-arm64.tar.gz', uploads)
        self.assertEqual(gh.assets(TAG)['summon-darwin-arm64.tar.gz'], old_data)
        self.assertEqual(self.manifest(gh)['assets']['darwin-arm64']['sha256'],
                         hashlib.sha256(old_data).hexdigest())

    def test_half_pair_with_same_bytes_gets_the_other_half(self):
        built = self.built_bytes('linux-x64')
        gh = self.gh(assets={'summon-linux-x64.tar.gz': built['summon-linux-x64.tar.gz']})
        self.assertEqual(self.finalize(gh), 'published')
        uploads = [c[1] for c in gh.calls if c[0] == 'upload']
        self.assertIn('summon-linux-x64.build.json', uploads)
        self.assertNotIn('summon-linux-x64.tar.gz', uploads)

    def test_half_pair_with_different_bytes_fails_without_overwriting(self):
        other, _ = make_pair('linux-x64', salt=3)
        gh = self.gh(assets={'summon-linux-x64.tar.gz': other})
        with self.assertRaisesRegex(publish.PublishError, 'summon-linux-x64.tar.gz.*不同'):
            self.finalize(gh)
        self.assertEqual(gh.assets(TAG)['summon-linux-x64.tar.gz'], other)
        self.assertNotIn('manifest.json', gh.assets(TAG))
        self.assertTrue(gh.releases[TAG]['isDraft'])

    def test_manifest_in_draft_is_verified_and_reused(self):
        first = self.gh(tag=TEST_TAG)
        self.finalize(first, TEST_TAG)
        assets = dict(first.assets(TEST_TAG))
        # 上一轮传完 manifest 后中断,中间文件还在。
        for platform in plan.PLATFORMS:
            name = plan.pair(platform)[1]
            assets[name] = self.built_bytes(platform)[name]
        gh = self.gh(tag=TEST_TAG, assets=assets)
        self.assertEqual(self.finalize(gh, TEST_TAG), 'draft-ready')
        self.assertNotIn('upload', [c[0] for c in gh.calls])
        self.assertEqual(sorted(gh.assets(TEST_TAG)), plan.published_assets())

    def test_manifest_in_draft_that_does_not_match_packages_fails(self):
        first = self.gh(tag=TEST_TAG)
        self.finalize(first, TEST_TAG)
        assets = dict(first.assets(TEST_TAG))
        assets['summon-linux-arm64.tar.gz'], _ = make_pair('linux-arm64', salt=9)
        gh = self.gh(tag=TEST_TAG, assets=assets)
        with self.assertRaisesRegex(publish.PublishError, 'summon-linux-arm64.tar.gz'):
            self.finalize(gh, TEST_TAG)
        self.assertEqual(gh.mutations(), [])

    def test_published_and_complete_does_nothing(self):
        first = self.gh()
        self.finalize(first)
        gh = self.gh(draft=False, assets=first.assets(TAG))
        self.assertEqual(self.finalize(gh), 'already-published')
        self.assertEqual(gh.calls, [])

    def test_published_but_incomplete_is_not_touched(self):
        gh = self.gh(draft=False, assets={'manifest.json': b'{}'})
        with self.assertRaisesRegex(publish.PublishError, '不改写'):
            self.finalize(gh)
        self.assertEqual(gh.calls, [])

    def test_missing_release_fails(self):
        gh = FakeGh(tag_commits={TAG: SHA})
        with self.assertRaisesRegex(publish.PublishError, '不存在'):
            self.finalize(gh)

    def test_draft_for_another_commit_fails(self):
        gh = self.gh(target=OTHER)
        with self.assertRaisesRegex(publish.PublishError, OTHER):
            self.finalize(gh)
        self.assertEqual(gh.calls, [])

    def test_unfinished_asset_fails(self):
        gh = self.gh(assets={'summon-linux-x64.tar.gz': b'x'},
                     states={'summon-linux-x64.tar.gz': 'starter'})
        with self.assertRaisesRegex(publish.PublishError, '没上传完'):
            self.finalize(gh)
        self.assertEqual(gh.calls, [])

    def test_build_from_another_commit_is_rejected_before_upload(self):
        self.write_built('linux-arm64', commit=OTHER[:8])
        gh = self.gh()
        with self.assertRaisesRegex(publish.PublishError, '不是目标提交'):
            self.finalize(gh)
        self.assertNotIn('summon-linux-arm64.tar.gz', gh.assets(TAG))
        self.assertNotIn('manifest.json', gh.assets(TAG))

    def test_build_json_that_does_not_match_tarball_is_rejected(self):
        tarball, build_json = plan.pair('linux-x64')
        (self.built / tarball).write_bytes(make_pair('linux-x64', salt=5)[0])
        gh = self.gh()
        with self.assertRaisesRegex(publish.PublishError, 'linux-x64'):
            self.finalize(gh)
        self.assertNotIn(tarball, gh.assets(TAG))

    def test_wrong_members_are_rejected(self):
        self.write_built('darwin-arm64', members=('summon', 'summon-sidecar'))
        gh = self.gh()
        with self.assertRaisesRegex(publish.PublishError, '成员'):
            self.finalize(gh)

    def test_manifest_version_must_equal_release_tag(self):
        for platform in plan.PLATFORMS:
            self.write_built(platform, version='9.9.9')
        gh = self.gh()
        with self.assertRaisesRegex(publish.PublishError, '9.9.9'):
            self.finalize(gh)
        self.assertNotIn('manifest.json', gh.assets(TAG))

    def test_moved_tag_blocks_publish(self):
        gh = self.gh()
        gh.tag_commits[TAG] = OTHER
        with self.assertRaisesRegex(publish.PublishError, OTHER):
            self.finalize(gh)
        self.assertTrue(gh.releases[TAG]['isDraft'])
        self.assertNotIn('publish', [c[0] for c in gh.calls])

    def test_higher_latest_is_not_taken_over(self):
        gh = self.gh(latest='v999.0.0')
        self.finalize(gh)
        self.assertEqual(gh.calls[-1], ('publish', TAG, False))

    def test_lower_latest_is_taken_over(self):
        gh = self.gh(latest='v0.0.1')
        self.finalize(gh)
        self.assertEqual(gh.calls[-1], ('publish', TAG, True))

    def test_unrecognized_tag_fails_before_any_call(self):
        gh = self.gh(tag='nightly')
        with self.assertRaisesRegex(publish.PublishError, 'nightly'):
            self.finalize(gh, 'nightly')
        self.assertEqual(gh.calls, [])


class PrepareTests(unittest.TestCase):
    def run_prepare(self, gh, tag=TAG):
        with tempfile.TemporaryDirectory() as work, redirect_stderr(io.StringIO()):
            return publish.prepare(gh, tag, SHA, work)

    def test_creates_draft_with_notes(self):
        gh = FakeGh(tag_commits={TAG: SHA})
        self.assertEqual(self.run_prepare(gh), 'create')
        self.assertEqual(gh.calls, [('create', TAG, SHA, f'summon {VERSION}', False)])
        self.assertTrue(gh.releases[TAG]['isDraft'])
        self.assertTrue(gh.releases[TAG]['notes'].strip())

    def test_draft_whose_recorded_target_differs_fails_before_builds(self):
        gh = FakeGh(tag_commits={TAG: SHA})
        create = gh.create_draft

        def create_with_branch_target(*args, **kwargs):
            create(*args, **kwargs)
            gh.releases[TAG]['targetCommitish'] = 'main'
        gh.create_draft = create_with_branch_target
        with self.assertRaisesRegex(publish.PublishError, '新建的草稿不对.*main'):
            self.run_prepare(gh)

    def test_rehearsal_draft_is_prerelease(self):
        gh = FakeGh(tag_commits={TEST_TAG: SHA})
        self.assertEqual(self.run_prepare(gh, TEST_TAG), 'create')
        self.assertIs(gh.calls[0][-1], True)
        self.assertIn('演练', gh.calls[0][3])

    def test_existing_draft_continues_and_published_skips(self):
        gh = FakeGh(tag_commits={TAG: SHA})
        gh.add_release(TAG)
        self.assertEqual(self.run_prepare(gh), 'continue')
        gh.releases[TAG]['isDraft'] = False
        self.assertEqual(self.run_prepare(gh), 'skip')
        self.assertEqual(gh.calls, [])

    def test_tag_elsewhere_or_missing_fails(self):
        for tag_commit in (OTHER, None):
            gh = FakeGh(tag_commits={TAG: tag_commit} if tag_commit else {})
            with self.assertRaisesRegex(publish.PublishError, '同名 tag'):
                self.run_prepare(gh)
            self.assertEqual(gh.calls, [])

    def test_tag_that_does_not_match_version_fails(self):
        gh = FakeGh(tag_commits={'v0.0.0': SHA})
        with self.assertRaisesRegex(publish.PublishError, '不符'):
            self.run_prepare(gh, 'v0.0.0')
        self.assertEqual(gh.calls, [])


class GhTests(unittest.TestCase):
    """Gh 拼出的命令与对 gh 输出的解读。"""

    def fake_run(self, responses):
        calls = []

        def run(args, capture_output, text):
            calls.append(args)
            for prefix, (rc, out, err) in responses:
                if args[:len(prefix)] == prefix:
                    return subprocess.CompletedProcess(args, rc, out, err)
            raise AssertionError(f'没料到的命令 {args}')
        return calls, mock.patch.object(publish.subprocess, 'run', run)

    def test_view_missing_release_is_none_other_errors_raise(self):
        gh = publish.Gh('o/r')
        calls, patch = self.fake_run([(['gh', 'release', 'view'], (1, '', 'release not found\n'))])
        with patch:
            self.assertIsNone(gh.view(TAG))
        self.assertIn('--json', calls[0])
        calls, patch = self.fake_run([(['gh', 'release', 'view'], (1, '', 'HTTP 502\n'))])
        with patch, redirect_stderr(io.StringIO()), self.assertRaises(publish.PublishError):
            gh.view(TAG)

    def test_tag_commit_peels_annotated_tags(self):
        gh = publish.Gh('o/r')
        ref = json.dumps({'object': {'type': 'tag', 'sha': OTHER}})
        obj = json.dumps({'object': {'type': 'commit', 'sha': SHA}})
        calls, patch = self.fake_run([
            (['gh', 'api', f'repos/o/r/git/ref/tags/{TAG}'], (0, ref, '')),
            (['gh', 'api', f'repos/o/r/git/tags/{OTHER}'], (0, obj, '')),
        ])
        with patch:
            self.assertEqual(gh.tag_commit(TAG), SHA)
        calls, patch = self.fake_run([(['gh', 'api'], (1, '', 'gh: Not Found (HTTP 404)\n'))])
        with patch:
            self.assertIsNone(gh.tag_commit(TAG))

    def test_publish_and_create_arguments(self):
        gh = publish.Gh('o/r')
        calls, patch = self.fake_run([(['gh', 'release'], (0, '', ''))])
        with patch:
            gh.publish(TAG, latest=False)
            gh.create_draft(TEST_TAG, SHA, 't', '/n.md', prerelease=True)
        self.assertEqual(calls[0], ['gh', 'release', 'edit', TAG, '--repo', 'o/r',
                                    '--draft=false', '--latest=false'])
        for flag in ('--draft', '--verify-tag', '--prerelease'):
            self.assertIn(flag, calls[1])
        self.assertEqual(calls[1][calls[1].index('--target') + 1], SHA)

    def test_latest_tag(self):
        gh = publish.Gh('o/r')
        calls, patch = self.fake_run([(['gh', 'release', 'view'], (0, '{"tagName": "v1.0.0"}', ''))])
        with patch:
            self.assertEqual(gh.latest_tag(), 'v1.0.0')
        self.assertNotIn(TAG, calls[0])


class CommandLineTests(unittest.TestCase):
    def test_failure_is_reported_as_error_annotation(self):
        err = io.StringIO()
        with mock.patch.object(publish, 'finalize', side_effect=publish.PublishError('坏了')), \
                redirect_stderr(err), redirect_stdout(io.StringIO()):
            rc = publish.main(['finalize', '--repo', 'o/r', '--tag', TAG, '--sha', SHA,
                               '--built', '/b', '--work', '/w'])
        self.assertEqual(rc, 1)
        self.assertIn('::error::坏了', err.getvalue())

    def test_prepare_prints_only_the_decision(self):
        out = io.StringIO()
        with mock.patch.object(publish, 'prepare', return_value='create'), \
                redirect_stdout(out), redirect_stderr(io.StringIO()):
            rc = publish.main(['prepare', '--repo', 'o/r', '--tag', TAG, '--sha', SHA,
                               '--work', '/w'])
        self.assertEqual((rc, out.getvalue()), (0, 'create\n'))

    def test_members_match_lib_sh(self):
        for platform, members in publish.MEMBERS.items():
            out = subprocess.run(
                ['bash', '-c', 'source scripts/github-release/lib.sh && summon_release_files "$1"',
                 'x', platform], cwd=ROOT, capture_output=True, text=True, check=True).stdout
            self.assertEqual(tuple(out.split()), members, platform)
        self.assertEqual(tuple(publish.MEMBERS), plan.PLATFORMS)


if __name__ == '__main__':
    unittest.main()
