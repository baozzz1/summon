"""scripts/github-release/manifest.py 的测试。

    python3 scripts/tests/test_github_release_manifest.py
"""
import importlib.util
import json
import tempfile
import unittest
from contextlib import redirect_stderr
from io import StringIO
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('manifest', ROOT / 'scripts/github-release/manifest.py')
manifest = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(manifest)

PUBLISHED_AT = '2026-01-02T03:04:05Z'


def build(platform, /, **override):
    record = {
        'platform': platform,
        'version': '1.2.3',
        'commit': 'abcdef12',
        'commit_time': 1700000000,
        'name': f'summon-{platform}.tar.gz',
        'size': 1024,
        'sha256': (platform.encode().hex() * 8)[:64].ljust(64, '0'),
    }
    record.update(override)
    return record


def all_builds(**override_for_last):
    platforms = list(manifest.REQUIRED_PLATFORMS)
    builds = [(f'{p}.build.json', build(p)) for p in platforms[:-1]]
    last = platforms[-1]
    builds.append((f'{last}.build.json', build(last, **override_for_last)))
    return builds


class BuildManifestTests(unittest.TestCase):
    def test_consistent_builds_produce_complete_manifest(self):
        result = manifest.build_manifest(all_builds(), PUBLISHED_AT)
        self.assertEqual(list(result), ['version', 'commit', 'commit_time', 'published_at', 'assets'])
        self.assertEqual(result['version'], '1.2.3')
        self.assertEqual(result['commit'], 'abcdef12')
        self.assertEqual(result['commit_time'], 1700000000)
        self.assertEqual(result['published_at'], PUBLISHED_AT)
        self.assertEqual(set(result['assets']), set(manifest.REQUIRED_PLATFORMS))
        for platform, asset in result['assets'].items():
            self.assertEqual(set(asset), {'name', 'size', 'sha256'})
            self.assertEqual(asset['name'], f'summon-{platform}.tar.gz')
            self.assertEqual(asset['size'], 1024)
            self.assertEqual(asset['sha256'], build(platform)['sha256'])

    def test_any_stamp_field_mismatch_fails(self):
        for field, value in (('version', '1.2.4'), ('commit', 'abcdef13'), ('commit_time', 1700000001)):
            with self.subTest(field=field):
                with self.assertRaisesRegex(manifest.ManifestError, f'构建戳不一致.*{field}'):
                    manifest.build_manifest(all_builds(**{field: value}), PUBLISHED_AT)

    def test_short_commits_of_different_length_are_the_same_commit(self):
        # CI 的浅克隆打 7 位、完整克隆打 8 位;manifest 取最长的那个。
        builds = all_builds(commit='abcdef1')
        self.assertEqual(manifest.build_manifest(builds, PUBLISHED_AT)['commit'], 'abcdef12')
        builds = [(s, {**b, 'commit': 'abcdef1'}) for s, b in all_builds()]
        builds[1] = (builds[1][0], {**builds[1][1], 'commit': 'abcdef1234'})
        self.assertEqual(manifest.build_manifest(builds, PUBLISHED_AT)['commit'], 'abcdef1234')
        with self.assertRaisesRegex(manifest.ManifestError, '构建戳不一致.*commit'):
            manifest.build_manifest(all_builds(commit='abcdef13'), PUBLISHED_AT)

    def test_commit_consistency_does_not_depend_on_input_order(self):
        # 7 位与两个 8 位都前缀相同,两个 8 位之间却不同:无论哪个顺序都要失败。
        commits = ('1234567', '12345678', '12345679')
        orders = [commits, commits[::-1], (commits[1], commits[0], commits[2])]
        for order in orders:
            with self.subTest(order=order):
                builds = [(f'{p}.build.json', build(p, commit=c))
                          for p, c in zip(manifest.REQUIRED_PLATFORMS, order)]
                with self.assertRaisesRegex(manifest.ManifestError, '构建戳不一致.*commit'):
                    manifest.build_manifest(builds, PUBLISHED_AT)

    def test_missing_platform_fails_unless_allowed(self):
        builds = all_builds()[:-1]
        with self.assertRaisesRegex(manifest.ManifestError, '缺平台:darwin-arm64'):
            manifest.build_manifest(builds, PUBLISHED_AT)
        result = manifest.build_manifest(builds, PUBLISHED_AT, require_all=False)
        self.assertEqual(set(result['assets']), {'linux-x64', 'linux-arm64'})

    def test_duplicate_platform_fails(self):
        builds = all_builds() + [('again.build.json', build('linux-x64'))]
        with self.assertRaisesRegex(manifest.ManifestError, '平台 linux-x64 重复'):
            manifest.build_manifest(builds, PUBLISHED_AT)

    def test_malformed_build_fails(self):
        cases = {
            'platform': 'windows-x64',
            'name': 'summon-other.tar.gz',
            'size': 0,
            'sha256': 'XYZ',
            'commit': 'not-a-sha',
            'commit_time': '1700000000',
            'version': 'latest',
        }
        for field, value in cases.items():
            with self.subTest(field=field):
                with self.assertRaises(manifest.ManifestError):
                    manifest.build_manifest([('x', build('linux-x64', **{field: value}))],
                                            PUBLISHED_AT, require_all=False)

    def test_published_at_must_be_utc_seconds(self):
        for value in ('2026-01-02 03:04:05', '2026-01-02T03:04:05+08:00', ''):
            with self.subTest(value=value):
                with self.assertRaisesRegex(manifest.ManifestError, 'published_at'):
                    manifest.build_manifest(all_builds(), value)

    def test_no_input_fails(self):
        with self.assertRaises(manifest.ManifestError):
            manifest.build_manifest([], PUBLISHED_AT)


class CommandLineTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name)

    def write_builds(self, builds):
        paths = []
        for _, record in builds:
            path = self.dir / f'summon-{record["platform"]}-{len(paths)}.build.json'
            path.write_text(json.dumps(record), encoding='utf-8')
            paths.append(str(path))
        return paths

    def run_main(self, *args):
        stderr = StringIO()
        with redirect_stderr(stderr):
            rc = manifest.main(list(args))
        return rc, stderr.getvalue()

    def test_writes_manifest_file(self):
        out = self.dir / 'manifest.json'
        rc, _ = self.run_main('--published-at', PUBLISHED_AT, '--out', str(out),
                              *self.write_builds(all_builds()))
        self.assertEqual(rc, 0)
        data = json.loads(out.read_text(encoding='utf-8'))
        self.assertEqual(data, manifest.build_manifest(all_builds(), PUBLISHED_AT))

    def test_failure_writes_nothing_and_explains(self):
        out = self.dir / 'manifest.json'
        rc, err = self.run_main('--published-at', PUBLISHED_AT, '--out', str(out),
                                *self.write_builds(all_builds(commit='0123456789')))
        self.assertEqual(rc, 1)
        self.assertIn('构建戳不一致', err)
        self.assertFalse(out.exists())

    def test_allow_missing_platforms_flag(self):
        out = self.dir / 'manifest.json'
        paths = self.write_builds(all_builds()[:1])
        rc, err = self.run_main('--published-at', PUBLISHED_AT, '--out', str(out), *paths)
        self.assertEqual(rc, 1)
        self.assertIn('缺平台', err)
        rc, _ = self.run_main('--published-at', PUBLISHED_AT, '--out', str(out),
                              '--allow-missing-platforms', *paths)
        self.assertEqual(rc, 0)

    def test_unreadable_input_fails(self):
        bad = self.dir / 'bad.build.json'
        bad.write_text('{', encoding='utf-8')
        rc, err = self.run_main('--out', str(self.dir / 'm.json'), str(bad))
        self.assertEqual(rc, 1)
        self.assertIn('读不了', err)


if __name__ == '__main__':
    unittest.main()
