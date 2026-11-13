"""scripts/github-release/release_info.py 的测试。

    python3 scripts/tests/test_github_release_info.py
"""
import importlib.util
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('release_info',
                                              ROOT / 'scripts/github-release/release_info.py')
info = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(info)

SHA = '0123456789abcdef0123456789abcdef01234567'
NOTES = """# Release Notes

说明。

## 未发布

- 还没发。

## 1.2.3 · 2026-01-02

### 新增

- 新东西。

## 1.2.2 · 2026-01-01

- 旧东西。
"""


class ReleaseInfoTests(unittest.TestCase):
    def test_release_tag_must_equal_version(self):
        self.assertEqual(info.tag_kind('v1.2.3', SHA, '1.2.3'), 'release')
        with self.assertRaisesRegex(info.InfoError, '不符'):
            info.tag_kind('v1.2.4', SHA, '1.2.3')

    def test_release_tag_is_plain_x_y_z(self):
        for tag in ('v1.2.3-rc.1', 'v1.2', 'v01.2.3', 'v1.2.3+b', '1.2.3', 'V1.2.3', 'v1.2.3 '):
            with self.assertRaises(info.InfoError, msg=tag):
                info.tag_kind(tag, SHA, tag.strip().lstrip('vV'))

    def test_rehearsal_tag_is_a_prefix_of_the_commit(self):
        self.assertEqual(info.tag_kind(f'test-{SHA[:7]}', SHA, '1.2.3'), 'rehearsal')
        self.assertEqual(info.tag_kind(f'test-{SHA}', SHA, '1.2.3'), 'rehearsal')
        with self.assertRaisesRegex(info.InfoError, '前缀'):
            info.tag_kind('test-fffffff', SHA, '1.2.3')
        for tag in (f'test-{SHA[:6]}', f'test-{SHA[10:17].upper()}', 'test-main'):
            with self.assertRaises(info.InfoError, msg=tag):
                info.tag_kind(tag, SHA, '1.2.3')

    def test_tag_needs_full_sha(self):
        with self.assertRaises(info.InfoError):
            info.tag_kind('v1.2.3', SHA[:7], '1.2.3')

    def test_notes_take_only_that_version_section(self):
        notes = info.release_notes('1.2.3', NOTES)
        self.assertEqual(notes, '### 新增\n\n- 新东西。')

    def test_notes_do_not_match_version_prefix(self):
        self.assertIsNone(info.release_notes('1.2', NOTES))
        self.assertIsNone(info.release_notes('1.2.4', NOTES))

    def test_last_section_runs_to_end(self):
        self.assertEqual(info.release_notes('1.2.2', NOTES), '- 旧东西。')

    def test_missing_section_falls_back_to_generic_text(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'RELEASE_NOTES.md').write_text(NOTES, encoding='utf-8')
            self.assertIn('summon 9.9.9', info.notes_or_default('9.9.9', root))
            self.assertEqual(info.notes_or_default('1.2.3', root), '### 新增\n\n- 新东西。')

    def test_version_comes_from_package_section(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'crates/summon').mkdir(parents=True)
            (root / 'crates/summon/Cargo.toml').write_text(
                '[package]\nname = "summon"\nversion = "1.2.3"\n\n'
                '[dependencies]\nserde = { version = "9.9.9" }\n', encoding='utf-8')
            self.assertEqual(info.read_version(root), '1.2.3')
            (root / 'crates/summon/Cargo.toml').write_text(
                '[package]\nname = "summon"\nversion.workspace = true\n', encoding='utf-8')
            with self.assertRaises(info.InfoError):
                info.read_version(root)

    def test_reads_this_repository(self):
        self.assertRegex(info.read_version(), r'^\d+\.\d+\.\d+')


if __name__ == '__main__':
    unittest.main()
