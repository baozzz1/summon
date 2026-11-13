"""scripts/github-release/release_plan.py 的测试。

    python3 scripts/tests/test_github_release_plan.py
"""
import importlib.util
import json
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('release_plan',
                                              ROOT / 'scripts/github-release/release_plan.py')
plan = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(plan)

SHA = '0123456789abcdef0123456789abcdef01234567'
OTHER = 'f' * 40
LINUX = ['linux-x64', 'linux-arm64']


def release(draft=True, target=SHA, assets=(), states=None):
    states = states or {}
    return {'isDraft': draft, 'targetCommitish': target,
            'assets': [{'name': n, 'state': states.get(n, 'uploaded')} for n in assets]}


def pair(platform):
    return [f'summon-{platform}.tar.gz', f'summon-{platform}.build.json']


class PrepareTests(unittest.TestCase):
    def test_missing_release_is_created(self):
        self.assertEqual(plan.plan_prepare(None, SHA, None), 'create')
        self.assertEqual(plan.plan_prepare(None, SHA, SHA), 'create')

    def test_published_release_is_skipped(self):
        self.assertEqual(plan.plan_prepare(release(draft=False), SHA, None), 'skip')
        self.assertEqual(plan.plan_prepare(release(draft=False, target=OTHER), SHA, OTHER), 'skip')

    def test_draft_for_this_commit_continues(self):
        self.assertEqual(plan.plan_prepare(release(assets=pair('linux-x64')), SHA, None), 'continue')
        self.assertEqual(plan.plan_prepare(release(), SHA, SHA), 'continue')

    def test_draft_for_another_commit_fails_naming_both(self):
        with self.assertRaises(plan.PlanError) as ctx:
            plan.plan_prepare(release(target=OTHER), SHA, None)
        self.assertIn(OTHER, str(ctx.exception))
        self.assertIn(SHA, str(ctx.exception))

    def test_existing_tag_elsewhere_blocks_create_and_continue(self):
        # GitHub 发布时沿用已有 tag、忽略草稿的目标提交:tag 指向别处就不能建、不能沿用。
        for existing in (None, release()):
            with self.subTest(release=existing):
                with self.assertRaises(plan.PlanError) as ctx:
                    plan.plan_prepare(existing, SHA, OTHER)
                self.assertIn(OTHER, str(ctx.exception))
                self.assertIn('tag', str(ctx.exception))

    def test_needs_full_sha(self):
        with self.assertRaises(plan.PlanError):
            plan.plan_prepare(None, SHA[:7], None)


class GitObjectTests(unittest.TestCase):
    def test_reads_type_and_sha(self):
        self.assertEqual(plan.git_object({'ref': 'refs/tags/v1', 'object': {'type': 'tag', 'sha': OTHER}}),
                         ('tag', OTHER))
        self.assertEqual(plan.git_object({'object': {'type': 'commit', 'sha': SHA}}), ('commit', SHA))

    def test_rejects_unrecognized_response(self):
        for bad in ({}, {'object': {'type': 'commit'}}, {'object': {'type': 'commit', 'sha': 'x'}}, []):
            with self.subTest(bad=bad):
                with self.assertRaises(plan.PlanError):
                    plan.git_object(bad)


class UploadTests(unittest.TestCase):
    def test_empty_draft_uploads_both_pairs(self):
        uploads, notes = plan.plan_upload(release(), SHA, LINUX)
        self.assertEqual(uploads, pair('linux-x64') + pair('linux-arm64'))
        self.assertEqual(notes, [])

    def test_platform_already_uploaded_is_skipped(self):
        uploads, notes = plan.plan_upload(release(assets=pair('linux-x64')), SHA, LINUX)
        self.assertEqual(uploads, pair('linux-arm64'))
        self.assertEqual(len(notes), 1)
        self.assertIn('linux-x64', notes[0])

    def test_everything_uploaded_uploads_nothing(self):
        assets = pair('linux-x64') + pair('linux-arm64') + pair('darwin-arm64')
        uploads, notes = plan.plan_upload(release(assets=assets), SHA, LINUX)
        self.assertEqual(uploads, [])
        self.assertEqual(len(notes), 2)

    def test_half_pair_fails_naming_the_asset_to_delete(self):
        for present in pair('linux-arm64'):
            with self.subTest(present=present):
                with self.assertRaises(plan.PlanError) as ctx:
                    plan.plan_upload(release(assets=pair('linux-x64') + [present]), SHA, LINUX)
                self.assertIn(f'删掉 {present}', str(ctx.exception))

    def test_manifest_in_draft_blocks_upload(self):
        with self.assertRaisesRegex(plan.PlanError, 'manifest.json'):
            plan.plan_upload(release(assets=['manifest.json']), SHA, LINUX)

    def test_unfinished_upload_blocks_upload(self):
        assets = pair('linux-x64')
        with self.assertRaisesRegex(plan.PlanError, '没上传完.*summon-linux-x64.tar.gz'):
            plan.plan_upload(release(assets=assets, states={assets[0]: 'starter'}), SHA, LINUX)

    def test_published_release_is_not_uploaded_to(self):
        with self.assertRaisesRegex(plan.PlanError, '不是草稿'):
            plan.plan_upload(release(draft=False), SHA, LINUX)

    def test_draft_for_another_commit_is_not_uploaded_to(self):
        with self.assertRaisesRegex(plan.PlanError, '目标提交'):
            plan.plan_upload(release(target=OTHER), SHA, LINUX)


class CommandLineTests(unittest.TestCase):
    def run_main(self, *args):
        out, err = StringIO(), StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            rc = plan.main(list(args))
        return rc, out.getvalue(), err.getvalue()

    def write(self, data):
        f = tempfile.NamedTemporaryFile('w', suffix='.json', delete=False, encoding='utf-8')
        self.addCleanup(Path(f.name).unlink)
        with f:
            json.dump(data, f)
        return f.name

    def test_prepare_without_release_prints_create(self):
        self.assertEqual(self.run_main('prepare', '--sha', SHA, '--tag-absent'), (0, 'create\n', ''))

    def test_prepare_continue_and_failure(self):
        self.assertEqual(self.run_main('prepare', '--sha', SHA, '--tag-absent', '--release-json',
                                       self.write(release()))[1], 'continue\n')
        rc, out, err = self.run_main('prepare', '--sha', SHA, '--tag-absent', '--release-json',
                                     self.write(release(target=OTHER)))
        self.assertEqual((rc, out), (1, ''))
        self.assertIn(OTHER, err)

    def test_prepare_with_tag_elsewhere_fails(self):
        rc, out, err = self.run_main('prepare', '--sha', SHA, '--tag-commit', OTHER)
        self.assertEqual((rc, out), (1, ''))
        self.assertIn(OTHER, err)
        self.assertEqual(self.run_main('prepare', '--sha', SHA, '--tag-commit', SHA)[1], 'create\n')

    def test_prepare_requires_tag_state(self):
        with redirect_stderr(StringIO()), self.assertRaises(SystemExit):
            plan.main(['prepare', '--sha', SHA])

    def test_git_object_prints_type_and_sha(self):
        path = self.write({'object': {'type': 'tag', 'sha': OTHER}})
        self.assertEqual(self.run_main('git-object', '--json', path), (0, f'tag {OTHER}\n', ''))

    def test_upload_prints_files_and_notes(self):
        path = self.write(release(assets=pair('linux-x64')))
        rc, out, err = self.run_main('upload', '--sha', SHA, '--release-json', path,
                                     '--platform', 'linux-x64', '--platform', 'linux-arm64')
        self.assertEqual(rc, 0)
        self.assertEqual(out.split(), pair('linux-arm64'))
        self.assertIn('linux-x64', err)

    def test_unreadable_release_json_fails(self):
        rc, _, err = self.run_main('upload', '--sha', SHA, '--release-json', '/nonexistent/x',
                                   '--platform', 'linux-x64')
        self.assertEqual(rc, 1)
        self.assertIn('读不了', err)


if __name__ == '__main__':
    unittest.main()
