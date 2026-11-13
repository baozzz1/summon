"""scripts/github-release/pack.py 与 sign-macos.sh 参数的测试。

    python3 scripts/tests/test_github_release_pack.py
"""
import importlib.util
import io
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / f'scripts/github-release/{name}.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


pack = load('pack')
publish = load('publish')

SHA = '0123456789abcdef0123456789abcdef01234567'


class PackTests(unittest.TestCase):
    def setUp(self):
        self.temp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.temp)
        self.stage = self.temp / 'stage'
        self.stage.mkdir()
        for name in pack.MEMBERS['darwin-arm64']:
            (self.stage / name).write_bytes(f'binary {name}'.encode())
        self.stamp = self.stage / 'stamp.json'
        self.write_stamp()

    def write_stamp(self, **override):
        stamp = {'version': '1.2.3', 'commit': SHA[:8], 'commit_time': 1_700_000_000}
        stamp.update(override)
        self.stamp.write_text(json.dumps(stamp), encoding='utf-8')

    def run_pack(self, out='out', sha=SHA):
        return pack.pack(self.stage, 'darwin-arm64', self.stamp, self.temp / out, sha)

    def test_pack_output_passes_publish_verification(self):
        record = self.run_pack()
        self.assertEqual(record['commit'], SHA[:8])
        build = publish.verify_build(self.temp / 'out', 'darwin-arm64', SHA)
        self.assertEqual(build['sha256'], record['sha256'])

    def test_same_binaries_give_identical_packages(self):
        a, b = self.run_pack('a'), self.run_pack('b')
        self.assertEqual(a['sha256'], b['sha256'])
        self.assertEqual((self.temp / 'a/summon-darwin-arm64.tar.gz').read_bytes(),
                         (self.temp / 'b/summon-darwin-arm64.tar.gz').read_bytes())

    def test_stage_must_hold_exactly_the_platform_files(self):
        (self.stage / 'extra').write_bytes(b'x')
        with self.assertRaisesRegex(pack.PackError, 'extra'):
            pack.check(self.stage, 'darwin-arm64', self.stamp)
        (self.stage / 'extra').unlink()
        (self.stage / 'summon-menubar').unlink()
        with self.assertRaisesRegex(pack.PackError, '应为'):
            pack.check(self.stage, 'darwin-arm64', self.stamp)

    def test_symlink_is_rejected(self):
        (self.stage / 'summon-menubar').unlink()
        os.symlink(self.stage / 'summon', self.stage / 'summon-menubar')
        with self.assertRaisesRegex(pack.PackError, '普通文件'):
            pack.check(self.stage, 'darwin-arm64', self.stamp)

    def test_stamp_must_be_complete_and_from_the_commit(self):
        for override in ({'commit': ''}, {'commit_time': 0}, {'version': '1.2.3-rc.1'},
                         {'commit_time': '1'}):
            self.write_stamp(**override)
            with self.assertRaises(pack.PackError, msg=override):
                pack.check(self.stage, 'darwin-arm64', self.stamp)
        self.write_stamp(commit='fedcba98')
        with self.assertRaisesRegex(pack.PackError, 'fedcba98'):
            pack.check(self.stage, 'darwin-arm64', self.stamp, SHA)
        pack.check(self.stage, 'darwin-arm64', self.stamp)  # 不给 sha 时不比提交

    def test_command_line(self):
        with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()) as err:
            self.assertEqual(pack.main(['check', '--stage', str(self.stage), '--platform',
                                        'darwin-arm64', '--stamp', str(self.stamp), '--sha', SHA]), 0)
            self.assertEqual(pack.main(['check', '--stage', str(self.stage), '--platform',
                                        'linux-x64', '--stamp', str(self.stamp)]), 1)
            self.assertEqual(pack.main(['check', '--stage', str(self.stage), '--platform',
                                        'darwin-arm64', '--stamp', str(self.stamp), '--sha', 'abc']), 2)
        self.assertIn('pack:', err.getvalue())


class SignScriptTests(unittest.TestCase):
    def test_missing_arguments_exit_2(self):
        for args in ([], ['--unsigned', '/x'], ['--unsigned', '/x', '--sha', SHA], ['--bogus']):
            rc = subprocess.run(['bash', 'scripts/github-release/sign-macos.sh', *args], cwd=ROOT,
                                capture_output=True).returncode
            self.assertEqual(rc, 2, args)


if __name__ == '__main__':
    unittest.main()
