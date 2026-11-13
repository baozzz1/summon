"""scripts/github-release/release_plan.py 的测试。

    python3 scripts/tests/test_github_release_plan.py
"""
import importlib.util
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('release_plan',
                                              ROOT / 'scripts/github-release/release_plan.py')
plan = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(plan)

SHA = '0123456789abcdef0123456789abcdef01234567'
OTHER = 'f' * 40


def release(draft=True, target=SHA, assets=(), states=None):
    states = states or {}
    return {'isDraft': draft, 'targetCommitish': target,
            'assets': [{'name': n, 'state': states.get(n, 'uploaded')} for n in assets]}


class PrepareTests(unittest.TestCase):
    def test_missing_release_is_created(self):
        self.assertEqual(plan.plan_prepare(None, SHA, SHA), 'create')

    def test_published_release_is_skipped(self):
        self.assertEqual(plan.plan_prepare(release(draft=False), SHA, OTHER), 'skip')

    def test_draft_for_this_commit_continues(self):
        self.assertEqual(plan.plan_prepare(release(), SHA, SHA), 'continue')

    def test_draft_for_another_commit_fails_naming_both(self):
        with self.assertRaisesRegex(plan.PlanError, f'{OTHER}.*{SHA}'):
            plan.plan_prepare(release(target=OTHER), SHA, SHA)

    def test_tag_elsewhere_or_missing_blocks_create_and_continue(self):
        for tag_commit in (OTHER, None):
            for existing in (None, release()):
                with self.assertRaisesRegex(plan.PlanError, '同名 tag'):
                    plan.plan_prepare(existing, SHA, tag_commit)

    def test_needs_full_sha(self):
        with self.assertRaises(plan.PlanError):
            plan.plan_prepare(None, SHA[:7], SHA[:7])


class GitObjectTests(unittest.TestCase):
    def test_reads_type_and_sha(self):
        self.assertEqual(plan.git_object({'object': {'type': 'tag', 'sha': OTHER}}), ('tag', OTHER))

    def test_rejects_unrecognized_response(self):
        for response in (None, {}, {'object': {'type': 'commit', 'sha': 'abc'}}, []):
            with self.assertRaises(plan.PlanError):
                plan.git_object(response)


class DraftTests(unittest.TestCase):
    def test_draft_checks(self):
        plan.check_draft(release(), SHA)
        with self.assertRaisesRegex(plan.PlanError, '不是草稿'):
            plan.check_draft(release(draft=False), SHA)
        with self.assertRaisesRegex(plan.PlanError, OTHER):
            plan.check_draft(release(target=OTHER), SHA)
        with self.assertRaisesRegex(plan.PlanError, 'x.tar.gz'):
            plan.check_draft(release(assets=['x.tar.gz'], states={'x.tar.gz': 'starter'}), SHA)

    def test_platform_decisions(self):
        tarball, build_json = plan.pair('linux-x64')
        self.assertEqual(plan.plan_platform(release(), 'linux-x64'), ('upload', []))
        self.assertEqual(plan.plan_platform(release(assets=[tarball, build_json]), 'linux-x64'),
                         ('adopt', [tarball, build_json]))
        self.assertEqual(plan.plan_platform(release(assets=[build_json]), 'linux-x64'),
                         ('compare', [build_json]))
        # 别的平台的附件不影响这个平台。
        self.assertEqual(plan.plan_platform(release(assets=plan.pair('linux-arm64')), 'linux-x64'),
                         ('upload', []))

    def test_unfinished_upload_does_not_count_as_present(self):
        tarball, build_json = plan.pair('linux-x64')
        r = release(assets=[tarball, build_json], states={build_json: 'starter'})
        self.assertEqual(plan.uploaded_names(r), {tarball})

    def test_published_assets_are_exactly_three_packages_and_manifest(self):
        self.assertEqual(plan.published_assets(), [
            'manifest.json', 'summon-darwin-arm64.tar.gz', 'summon-linux-arm64.tar.gz',
            'summon-linux-x64.tar.gz'])
        self.assertTrue(plan.published_complete(release(assets=plan.published_assets())))
        self.assertFalse(plan.published_complete(
            release(assets=plan.published_assets() + ['summon-linux-x64.build.json'])))
        self.assertFalse(plan.published_complete(release(assets=plan.published_assets()[1:])))


class LatestTests(unittest.TestCase):
    def test_highest_release_compares_numerically(self):
        self.assertEqual(plan.highest_release(['v1.9.9', 'v1.10.0', 'v1.2.3']), 'v1.10.0')
        self.assertEqual(plan.highest_release(['v2.0.0', 'v1.99.99']), 'v2.0.0')

    def test_highest_release_ignores_other_tags(self):
        self.assertEqual(plan.highest_release(['nightly', 'test-0123456', 'v1.0.0']), 'v1.0.0')
        self.assertIsNone(plan.highest_release([]))
        self.assertIsNone(plan.highest_release(['1.0.0']))


if __name__ == '__main__':
    unittest.main()
