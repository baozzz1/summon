"""镜像仓工作流文件的结构检查:语法、action 钉哈希、发布工作流的权限边界。

    python3 scripts/tests/test_github_release_workflow.py

工作流文件在源码仓与发布出去的仓里位置不同,两处都认。需要 PyYAML。
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
# 源码仓与发布出去的仓里工作流文件的位置不同,两处都认。
CANDIDATES = (ROOT / '.github/workflows', ROOT / 'scripts/public/overlay/.github/workflows')
WORKFLOWS = next((d for d in CANDIDATES if (d / 'release.yml').is_file()), None)
PINNED = re.compile(r'^[A-Za-z0-9._-]+/[A-Za-z0-9._/-]+@[0-9a-f]{40}$')
WRITERS = {'prepare', 'upload'}


def load(path):
    data = yaml.safe_load(path.read_text(encoding='utf-8'))
    # YAML 1.1 把裸写的 on 读成布尔 True。
    if True in data and 'on' not in data:
        data['on'] = data.pop(True)
    return data


def steps(job):
    return job.get('steps', [])


def grants_write(permissions):
    if permissions in ('write-all',):
        return True
    if isinstance(permissions, dict):
        return any(v == 'write' for v in permissions.values())
    return False


class WorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if WORKFLOWS is None:
            raise RuntimeError(f'没找到 release.yml(找过 {", ".join(map(str, CANDIDATES))})')
        cls.release_path = WORKFLOWS / 'release.yml'
        cls.release_text = cls.release_path.read_text(encoding='utf-8')
        cls.release = load(cls.release_path)

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

    def test_triggers(self):
        on = self.release['on']
        self.assertEqual(set(on['push']['branches']), {'main', 'release-test/**'})
        self.assertIn('workflow_dispatch', on)
        self.assertEqual(set(on), {'push', 'workflow_dispatch'})

    def test_only_prepare_and_upload_can_write(self):
        self.assertEqual(self.release['permissions'], {'contents': 'read'})
        for name, job in self.release['jobs'].items():
            permissions = job.get('permissions', self.release['permissions'])
            if name in WRITERS:
                self.assertEqual(permissions, {'contents': 'write'}, name)
            else:
                self.assertFalse(grants_write(permissions), name)
        self.assertEqual(set(self.release['jobs']), {'prepare', 'build', 'upload'})

    def test_upload_uses_no_third_party_action(self):
        # 有写权限的 job 只用 GitHub 自己的 checkout(取判定脚本)与 download-artifact,其余是 gh。
        upload = self.release['jobs']['upload']
        self.assertEqual(sorted(upload['needs']), ['build', 'prepare'])
        used = {step['uses'].split('@')[0] for step in steps(upload) if 'uses' in step}
        self.assertEqual(used, {'actions/checkout', 'actions/download-artifact'})

    def test_existing_release_decisions_come_from_release_plan(self):
        # 整轮重跑能接着做:prepare 与 upload 都交给 release_plan.py 判定,不在 YAML 里另写一套。
        def run_text(job):
            return '\n'.join(step.get('run', '') for step in steps(self.release['jobs'][job]))
        # 已有 Release 时把它交给判定脚本,没有时才是 create;两处都带上同名 tag 的状态。
        prepare = run_text('prepare')
        self.assertIn('release_plan.py prepare --sha "$SHA" "${TAG_STATE[@]}" --release-json', prepare)
        self.assertIn('release_plan.py prepare --sha "$SHA" "${TAG_STATE[@]}")', prepare)
        self.assertIn('targetCommitish', prepare)
        # 同名 tag:按精确 ref 查,附注 tag 解到提交,只有 404 算不存在。
        self.assertIn('gh api "repos/$REPO/git/ref/tags/$TAG"', prepare)
        self.assertIn('gh api "repos/$REPO/git/tags/$OBJ_SHA"', prepare)
        self.assertIn('TAG_STATE=(--tag-commit "$OBJ_SHA")', prepare)
        self.assertIn("grep -q 'HTTP 404'", prepare)
        self.assertIn('release_plan.py upload', run_text('upload'))
        self.assertNotIn('<< \'PY\'', run_text('upload'))

    def test_checkout_never_persists_credentials(self):
        for name, job in self.release['jobs'].items():
            for step in steps(job):
                if step.get('uses', '').startswith('actions/checkout@'):
                    self.assertIs(step.get('with', {}).get('persist-credentials'), False, name)

    def test_concurrency_and_timeouts(self):
        concurrency = self.release['concurrency']
        self.assertIn('github.ref', concurrency['group'])
        self.assertIs(concurrency['cancel-in-progress'], False)
        for name, job in self.release['jobs'].items():
            self.assertIsInstance(job.get('timeout-minutes'), int, name)

    def test_builds_both_linux_platforms_on_ubuntu_22_04(self):
        include = self.release['jobs']['build']['strategy']['matrix']['include']
        self.assertEqual({(i['runner'], i['platform']) for i in include},
                         {('ubuntu-22.04', 'linux-x64'), ('ubuntu-22.04-arm', 'linux-arm64')})

    def test_never_publishes_and_names_no_repository(self):
        # 草稿转为发布在发版的 Mac 上做;仓库一律取 github.repository。
        self.assertNotIn('--draft=false', self.release_text)
        self.assertNotIn('release edit', self.release_text)
        self.assertNotRegex(self.release_text, r'github\.com[:/]')
        self.assertIn('${{ github.repository }}', self.release_text)


if __name__ == '__main__':
    unittest.main()
