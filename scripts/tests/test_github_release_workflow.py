"""工作流文件的结构检查:语法、action 钉哈希、发布工作流的触发、权限与签名证书的边界。

    python3 scripts/tests/test_github_release_workflow.py

需要 PyYAML。
"""
import re
import sys
import unittest
from pathlib import Path

try:
    import yaml
except ImportError:  # 缺依赖要明确失败,不能当作检查通过
    sys.exit('需要 PyYAML(python3-yaml)才能检查工作流文件')

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / '.github/workflows'
PINNED = re.compile(r'^[A-Za-z0-9._-]+/[A-Za-z0-9._/-]+@[0-9a-f]{40}$')
WRITERS = {'prepare', 'publish'}
JOBS = {'prepare', 'build-linux', 'build-macos', 'publish'}


def load(path):
    data = yaml.safe_load(path.read_text(encoding='utf-8'))
    # YAML 1.1 把裸写的 on 读成布尔 True。
    if True in data and 'on' not in data:
        data['on'] = data.pop(True)
    return data


def steps(job):
    return job.get('steps', [])


def run_text(job):
    return '\n'.join(step.get('run', '') for step in steps(job))


def grants_write(permissions):
    if permissions in ('write-all',):
        return True
    if isinstance(permissions, dict):
        return any(v == 'write' for v in permissions.values())
    return False


class WorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.release_path = WORKFLOWS / 'release.yml'
        cls.release_text = cls.release_path.read_text(encoding='utf-8')
        cls.release = load(cls.release_path)
        cls.jobs = cls.release['jobs']
        cls.checks = load(WORKFLOWS / 'checks.yml')

    def test_every_workflow_parses_and_pins_actions_to_full_commit(self):
        files = sorted(WORKFLOWS.glob('*.yml'))
        self.assertIn(self.release_path, files)
        for path in files:
            data = load(path)
            self.assertIsInstance(data.get('jobs'), dict, path.name)
            for name, job in data['jobs'].items():
                for step in steps(job):
                    if 'uses' in step:
                        self.assertRegex(step['uses'], PINNED, f'{path.name}:{name}')
            # 解析之外再按文本扫一遍,防止 uses 藏在解析器认不出的位置。
            for line in path.read_text(encoding='utf-8').splitlines():
                match = re.match(r'^\s*(?:-\s*)?uses:\s*(\S+)', line)
                if match:
                    self.assertRegex(match.group(1), PINNED, f'{path.name}: {line.strip()}')

    def test_release_is_triggered_only_by_release_and_rehearsal_tags(self):
        on = self.release['on']
        self.assertEqual(set(on), {'push'})
        self.assertEqual(set(on['push']), {'tags'})
        self.assertEqual(set(on['push']['tags']), {'v*', 'test-*'})

    def test_release_tag_is_checked_against_version_and_main(self):
        prepare = run_text(self.jobs['prepare'])
        self.assertIn('release_info.py check-tag --tag "$TAG" --sha "$SHA"', prepare)
        self.assertIn('git merge-base --is-ancestor "$SHA" origin/main', prepare)
        self.assertIn('publish.py prepare', prepare)
        checkout = steps(self.jobs['prepare'])[0]
        self.assertEqual(checkout['with'].get('fetch-depth'), 0)

    def test_only_prepare_and_publish_can_write(self):
        self.assertEqual(self.release['permissions'], {'contents': 'read'})
        self.assertEqual(set(self.jobs), JOBS)
        for name, job in self.jobs.items():
            permissions = job.get('permissions', self.release['permissions'])
            if name in WRITERS:
                self.assertEqual(permissions, {'contents': 'write'}, name)
            else:
                self.assertFalse(grants_write(permissions), name)

    def test_signing_secrets_only_reach_the_import_step_of_the_macos_job(self):
        for name, job in self.jobs.items():
            if name != 'build-macos':
                self.assertNotIn('secrets.', str(job), name)
                self.assertNotIn('environment', job, name)
        macos = self.jobs['build-macos']
        self.assertEqual(macos['environment'], 'release')
        self.assertNotIn('secrets.', str(macos.get('env', {})))
        with_secrets = [s for s in steps(macos) if 'secrets.' in str(s)]
        self.assertEqual(len(with_secrets), 1)
        self.assertIn('macos-keychain.sh setup', with_secrets[0]['run'])
        self.assertEqual(set(with_secrets[0]['env']),
                         {'SUMMON_MACOS_CERT_P12', 'SUMMON_MACOS_CERT_PASSWORD'})
        self.assertNotIn('set -x', self.release_text)

    def test_macos_job_builds_before_import_and_always_removes_keychain(self):
        names = [s.get('name', s.get('uses', '')) for s in steps(self.jobs['build-macos'])]
        runs = [s.get('run', '') for s in steps(self.jobs['build-macos'])]
        build = next(i for i, r in enumerate(runs) if '--build-only' in r)
        setup = next(i for i, r in enumerate(runs) if 'macos-keychain.sh setup' in r)
        sign = next(i for i, r in enumerate(runs) if '--skip-build' in r)
        cleanup = next(i for i, r in enumerate(runs) if 'macos-keychain.sh cleanup' in r)
        self.assertLess(build, setup, names)
        self.assertLess(setup, sign, names)
        self.assertLess(sign, cleanup, names)
        self.assertEqual(steps(self.jobs['build-macos'])[cleanup].get('if'), 'always()')
        self.assertNotIn('--adhoc-sign', self.release_text)

    def test_publish_waits_for_every_build_and_uses_publish_py(self):
        publish = self.jobs['publish']
        self.assertEqual(sorted(publish['needs']), ['build-linux', 'build-macos', 'prepare'])
        self.assertIn('publish.py finalize', run_text(publish))
        used = {step['uses'].split('@')[0] for step in steps(publish) if 'uses' in step}
        self.assertEqual(used, {'actions/checkout', 'actions/download-artifact'})
        # 草稿转为发布只在 publish.py 里做,判定在 release_plan.py。
        self.assertNotIn('--draft=false', self.release_text)
        self.assertNotIn('release edit', self.release_text)

    def test_every_build_checks_out_the_prepared_commit(self):
        for name in ('build-linux', 'build-macos', 'publish'):
            checkout = steps(self.jobs[name])[0]
            self.assertTrue(checkout['uses'].startswith('actions/checkout@'), name)
            self.assertEqual(checkout['with']['ref'], '${{ needs.prepare.outputs.sha }}', name)
            self.assertIn('prepare', self.jobs[name]['needs'])

    def test_checkout_never_persists_credentials(self):
        for path in sorted(WORKFLOWS.glob('*.yml')):
            for name, job in load(path)['jobs'].items():
                for step in steps(job):
                    if step.get('uses', '').startswith('actions/checkout@'):
                        self.assertIs(step.get('with', {}).get('persist-credentials'), False,
                                      f'{path.name}:{name}')

    def test_concurrency_and_timeouts(self):
        concurrency = self.release['concurrency']
        self.assertIn('github.ref', concurrency['group'])
        self.assertIs(concurrency['cancel-in-progress'], False)
        for name, job in self.jobs.items():
            self.assertIsInstance(job.get('timeout-minutes'), int, name)

    def test_builds_linux_on_ubuntu_22_04_and_macos_on_arm64(self):
        include = self.jobs['build-linux']['strategy']['matrix']['include']
        self.assertEqual({(i['runner'], i['platform']) for i in include},
                         {('ubuntu-22.04', 'linux-x64'), ('ubuntu-22.04-arm', 'linux-arm64')})
        self.assertEqual(self.jobs['build-macos']['runs-on'], 'macos-15')

    def test_names_no_repository(self):
        self.assertNotRegex(self.release_text, r'github\.com[:/]')
        self.assertIn('${{ github.repository }}', self.release_text)

    def test_checks_run_on_linux_and_the_release_macos_runner(self):
        source = self.checks['jobs']['source']
        self.assertEqual(source['strategy']['matrix']['os'], ['ubuntu-latest', 'macos-15'])
        text = run_text(source)
        for script in sorted((ROOT / 'scripts/tests').glob('test*')):
            self.assertIn(f'scripts/tests/{script.name}', text, script.name)
        self.assertIn('swift test --package-path menubar', text)


if __name__ == '__main__':
    unittest.main()
