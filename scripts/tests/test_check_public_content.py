"""scripts/check-public-content.py 的规则测试:每条规则都要能拦下,合成写法都要放行,输出里不出现命中值。

    python3 scripts/tests/test_check_public_content.py
"""
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / 'check-public-content.py'


class CheckPublicContentTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'tree'
        self.root.mkdir()

    def write(self, name, content, mode='w'):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if mode == 'wb':
            path.write_bytes(content)
        else:
            path.write_text(content, encoding='utf-8')
        return path

    def scan(self, private=None):
        args = [sys.executable, str(SCRIPT), '--root', str(self.root)]
        if private is not None:
            terms = Path(self.temp.name) / 'private-terms.json'
            terms.write_text(json.dumps(private, ensure_ascii=False), encoding='utf-8')
            args += ['--private-terms', str(terms)]
        return subprocess.run(args, capture_output=True, text=True)

    def assert_blocked(self, kind, secret, private=None):
        result = self.scan(private)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(kind, result.stdout)
        self.assertNotIn(secret, result.stdout + result.stderr)

    def assert_clean(self, private=None):
        result = self.scan(private)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_synthetic_content_passes(self):
        self.write('src/a.rs', '\n'.join([
            'let id = "ou_00000000000000000000000000000001";',
            'let chat = "oc_0123456789abcdef";',
            'let user = "ou_1a2b3c4d5e6f7a8b";',
            'let pair = "ou_aabbccddeeff0011";',
            'let home = "/Users/me/project"; let other = "/home/runner/work";',
            'let mail = "someone@example.com"; let local = "a@b.cn";',
            'let url = "https://oauth2:token@git.example.org/x"; let pkg = "pkg@1.2.3";',
            'let unit = "user@1001.service";',
        ]))
        self.assert_clean()

    def test_realistic_feishu_id_is_blocked(self):
        value = 'ou_' + '7f3a9c2e5b18d4f6a0c3e7b9d2f5a8c1'
        self.write('tests/fixtures/x.json', json.dumps({'sender': value}))
        self.assert_blocked('realistic-feishu-id', value)

    def test_credentials_are_blocked(self):
        token = 'ghp_' + 'Ab3dEf6hIj9kLm2nOp5qRs8tUv1wXy4zA7bC'
        self.write('a.sh', f'export TOKEN={token}\n')
        self.assert_blocked('credential-pattern', token)

    def test_literal_secret_value_is_blocked_but_placeholders_pass(self):
        value = 'q8Zr4Lm2Vx9Tp1Ks'
        self.write('config.yaml', 'app_secret: "${FEISHU_APP_SECRET}"\n')
        self.assert_clean()
        self.write('config.yaml', f'app_secret: "{value}"\n')
        self.assert_blocked('literal-secret-value', value)

    def test_local_user_path_is_blocked(self):
        self.write('notes.md', 'cwd: /Users/' + 'privacy-canary/work\n')
        self.assert_blocked('local-user-path', 'privacy-canary')

    def test_mailbox_outside_reserved_domains_is_blocked(self):
        address = 'canary.person@' + 'mail-provider.com'
        self.write('a.txt', f'Email: {address}\n')
        self.assert_blocked('email-address', address)

    def test_private_runtime_files_and_external_symlinks_are_blocked(self):
        self.write('.env', 'KEY=value\n')
        result = self.scan()
        self.assertIn('private-or-runtime-file', result.stdout)
        self.assertNotIn('value', result.stdout)
        (self.root / '.env').unlink()
        os.symlink('../outside', self.root / 'link')
        self.assert_blocked('external-or-broken-symlink', 'outside')

    def test_unreviewed_binary_is_blocked(self):
        self.write('img.png', b'\x89PNG\r\n\x1a\n' + b'\0' * 16, mode='wb')
        result = self.scan()
        self.assertEqual(result.returncode, 1)
        self.assertIn('img.png:0: binary-needs-review', result.stdout)

    def test_private_terms_block_names_ids_and_prefixes(self):
        private = {
            'terms': [{'kind': 'agent-alias', 'value': 'night-owl'},
                      {'kind': 'project', 'value': 'Orca'},
                      {'kind': 'chat-or-sender', 'value': '周末读书分享会'}],
            'ids': ['on_' + '5d1c7e9a3b6f2048c1e7a9d3b5f26c8e'],
        }
        cases = [
            ('agent-alias', 'Night_Owl'),
            ('agent-alias', '/summonNightOwl'),  # 嵌在驼峰标识符里
            ('project', 'orca'),
            ('chat-or-sender', '周末读书分享会'),
            ('private-id', 'on_5d1c7e9a'),
        ]
        for kind, value in cases:
            with self.subTest(value=value):
                self.write('src/x.rs', f'// 现场:{value} 那一轮\n')
                self.assert_blocked(kind, value, private)
        # 短词只在词边界上算数;claude-<名> 形态的模型 id 不是 agent 名。
        self.write('src/x.rs', 'let a = "orcas"; let m = "claude-night-owl";\n')
        self.assert_clean(private)

    def test_private_terms_in_paths_and_link_targets_are_blocked(self):
        private = {'terms': [{'kind': 'agent-alias', 'value': 'night-owl'}], 'ids': []}
        self.write('tests/fixtures/agents/night-owl/agent.yaml', 'name: demo\n')
        self.assert_blocked('private-agent-alias', 'night-owl', private)
        (self.root / 'tests/fixtures/agents/night-owl/agent.yaml').unlink()
        (self.root / 'tests/fixtures/agents/night-owl').rmdir()
        self.write('tests/fixtures/agents/demo/agent.yaml', 'name: demo\n')
        os.symlink('demo/agent.yaml', self.root / 'tests/fixtures/agents/NightOwl.yaml')
        self.assert_blocked('private-agent-alias', 'NightOwl', private)
        (self.root / 'tests/fixtures/agents/NightOwl.yaml').unlink()
        # 路径里没有模型 id 要放行:claude-<名> 形态的目录名同样拦。
        self.write('tests/claude-night-owl/x.txt', 'x\n')
        self.assert_blocked('private-agent-alias', 'night-owl', private)

    def test_spaced_and_truncated_variants_of_non_ascii_terms_are_blocked(self):
        # 折叠后 18 个字符:前缀 / 后缀片段各 11 个字符。
        long_name = '示例Alpha支持Beta的试点功能'
        private = {'terms': [{'kind': 'chat-or-sender', 'value': long_name},
                             {'kind': 'chat-or-sender', 'value': '周末读书分享会'}],
                   'ids': []}
        cases = [
            '周末 读书 分享会',  # 加空格
            '周末读书　分享会',  # 全角空格
            '示例alpha支持beta的试点功能',  # 去掉空格、换了大小写
            '示例 Alpha 支持 Beta 的试点',  # 加空格又截掉结尾两个字符
            '示例 Alpha 支持 Beta',  # 截掉更多结尾
            'Alpha 支持 Beta 的试点功能',  # 截掉开头
        ]
        for value in cases:
            with self.subTest(value=value):
                self.write('src/x.rs', f'fn a() {{}}\n\n    // 现场:{value} 那一轮\n')
                result = self.scan(private)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn('src/x.rs:3: private-chat-or-sender', result.stdout)
                self.assertNotIn(value, result.stdout + result.stderr)
        # 路径里加了空格的名字同样拦,报出来的路径整段打码。
        self.write('src/x.rs', 'fn a() {}\n')
        self.write('tests/示例 Alpha 支持 Beta/x.txt', 'x\n')
        self.assert_blocked('private-chat-or-sender', '示例 Alpha 支持 Beta', private)
        self.assertIn('tests/***/x.txt:0: private-chat-or-sender', self.scan(private).stdout)

    def test_short_fragments_of_non_ascii_terms_pass(self):
        private = {'terms': [{'kind': 'chat-or-sender', 'value': '示例Alpha支持Beta的试点功能'},
                             {'kind': 'chat-or-sender', 'value': '周末读书分享会'},
                             {'kind': 'chat-or-sender', 'value': 'Weekly Sync 小组'}],
                   'ids': []}
        self.write('src/x.rs', '\n'.join([
            '// 支持 Beta 的试点,试点功能,示例 Alpha',  # 长名字中间与两头的短片段
            '// 周末读书分享',  # 不够长的名字只认整词
            '周末读书',
            '分享会',  # 换行不折叠:上一行结尾与这一行开头拼不成名字
            '// Weekly Sync',  # 只剩 ASCII 的片段不拦
        ]) + '\n')
        self.assert_clean(private)

    def test_build_directories_are_scanned_in_an_export_tree(self):
        private = {'terms': [{'kind': 'agent-alias', 'value': 'night-owl'}], 'ids': []}
        self.write('crates/x/tests/fixtures/target/data.txt', 'night-owl\n')
        self.assert_blocked('private-agent-alias', 'night-owl', private)


if __name__ == '__main__':
    unittest.main()
