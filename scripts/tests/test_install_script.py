"""scripts/install.sh 的测试:本地目录与本地 HTTP 作 Release 来源,临时 HOME,假 uname / codesign。

    python3 scripts/tests/test_install_script.py

包里的 summon 是一段 sh:`install-stamp print` 在 HOME 下记一笔再打环境变量 FAKE_STAMP,
`install-stamp record` 在 HOME 下留个记号。它与假 codesign 都先把 stdin 读空:安装脚本经管道交给 sh
时,哪个外部命令继承了 stdin,后面的脚本就被它吃掉、装不完。manifest 由 scripts/github-release/manifest.py 命令行生成,与发版同一条路径。
"""
import hashlib
import http.server
import io
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'scripts/install.sh'
MANIFEST_SCRIPT = ROOT / 'scripts/github-release/manifest.py'

STAMP = {'version': '0.7.0', 'commit': 'abcdef12', 'commit_time': 1700000000}

FAKE_SUMMON = '''#!/bin/sh
# team=TEAMA
cat > /dev/null
case "$1 ${2:-}" in
  "install-stamp print") echo run >> "$HOME/stamp-calls"; printf '%s\\n' "$FAKE_STAMP" ;;
  "install-stamp record") : > "$HOME/stamp-recorded" ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
'''

FAKE_UNAME = '''#!/bin/sh
case "$1" in
  -s) echo "$FAKE_UNAME_S" ;;
  -m) echo "$FAKE_UNAME_M" ;;
  *) echo "$FAKE_UNAME_S" ;;
esac
'''

# 按文件里 `# team=` 那一行报团队,有 `# unsigned` 的验签失败。
FAKE_CODESIGN = '''#!/bin/sh
cat > /dev/null
eval "path=\\${$#}"
team="$(sed -n 's/^# team=//p' "$path" | head -n 1)"
case "$1" in
  --verify)
    if grep -q '^# unsigned' "$path"; then echo "$path: not signed" >&2; exit 1; fi ;;
  -dv)
    if [ -n "$team" ]; then echo "TeamIdentifier=$team" >&2; else echo "TeamIdentifier=not set" >&2; fi ;;
esac
'''


def tarball(members):
    """members: [(名字, 内容字节, 类型)];类型是 tarfile 的 REGTYPE / SYMTYPE。"""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode='w:gz', format=tarfile.PAX_FORMAT) as tar:
        for name, body, kind in members:
            info = tarfile.TarInfo(name)
            info.type = kind
            info.mode = 0o755
            info.mtime = STAMP['commit_time']
            if kind == tarfile.SYMTYPE:
                info.linkname = '/bin/sh'
                tar.addfile(info)
            else:
                info.size = len(body)
                tar.addfile(info, io.BytesIO(body))
    return buf.getvalue()


def package(platform='linux-x64', extra=()):
    members = [('summon', FAKE_SUMMON.encode(), tarfile.REGTYPE),
               ('summon-sidecar', b'#!/bin/sh\n# team=TEAMA\n', tarfile.REGTYPE)]
    if platform.startswith('darwin'):
        members.append(('summon-menubar', b'#!/bin/sh\n# team=TEAMA\n', tarfile.REGTYPE))
    return tarball(members + list(extra))


def write_tool(path, text):
    path.write_text(text, encoding='utf-8')
    path.chmod(0o755)


class Release:
    """一个临时 HOME 加一个本地目录形式的 Release。"""

    def __init__(self, test):
        self._tmp = tempfile.TemporaryDirectory()
        test.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name).resolve()
        self.home = self.root / 'home'
        self.home.mkdir()
        self.dir = self.root / 'release'
        self.dir.mkdir()
        self.tools = self.root / 'tools'
        self.tools.mkdir()
        write_tool(self.tools / 'uname', FAKE_UNAME)
        write_tool(self.tools / 'codesign', FAKE_CODESIGN)
        self.uname = ('Linux', 'x86_64')
        self.stamp = dict(STAMP)
        self.publish({'linux-x64': package()})

    def publish(self, packages, sizes=None, shas=None):
        """把各平台的包放进 Release,manifest 照实记(sizes / shas 可改写某个平台的记录)。

        manifest 由真的 scripts/github-release/manifest.py 生成(与发版同一条命令行):它的输出
        格式一变,安装脚本按格式取字段的那一段就会在这里红。"""
        builds = []
        for platform, data in packages.items():
            name = f'summon-{platform}.tar.gz'
            (self.dir / name).write_bytes(data)
            build = self.root / f'summon-{platform}.build.json'
            build.write_text(json.dumps({
                'platform': platform, **STAMP, 'name': name,
                'size': (sizes or {}).get(platform, len(data)),
                'sha256': (shas or {}).get(platform, hashlib.sha256(data).hexdigest()),
            }), encoding='utf-8')
            builds.append(str(build))
        subprocess.run(
            ['python3', str(MANIFEST_SCRIPT), '--allow-missing-platforms',
             '--published-at', '2026-01-02T03:04:05Z', '--out', str(self.dir / 'manifest.json'),
             *builds],
            check=True, timeout=60)

    def run(self, source=None, piped=None, **env):
        """跑安装脚本;`piped` 给了 shell 名时像 `curl … | sh` 那样把脚本经 stdin 交给它。"""
        full = {
            key: value for key, value in os.environ.items()
            if not key.lower().endswith('_proxy')
        }
        full.update({
            'HOME': str(self.home),
            'PATH': f'{self.tools}:/usr/bin:/bin:/usr/sbin:/sbin',
            'LC_ALL': 'C',
            'SUMMON_INSTALL_FROM': source or str(self.dir),
            'FAKE_STAMP': json.dumps(self.stamp, separators=(',', ':')),
            'FAKE_UNAME_S': self.uname[0],
            'FAKE_UNAME_M': self.uname[1],
            'SUMMON_INSTALL_STAMP_TIMEOUT': '20',
        })
        full.update(env)
        if piped:
            result = subprocess.run([piped], input=SCRIPT.read_bytes(), env=full,
                                    capture_output=True, timeout=120)
        else:
            result = subprocess.run(['sh', str(SCRIPT)], env=full, stdin=subprocess.DEVNULL,
                                    capture_output=True, timeout=120)
        return (result.returncode, result.stdout.decode('utf-8', 'strict'),
                result.stderr.decode('utf-8', 'strict'))

    @property
    def bin(self):
        return self.home / '.summon/bin'

    def installed(self):
        return sorted(p.name for p in self.bin.iterdir()) if self.bin.exists() else []

    def stages(self):
        run = self.home / '.summon/run'
        return sorted(p.name for p in run.iterdir()) if run.exists() else []


def serve(directory):
    """本地 HTTP 服务 directory;返回地址与关停函数。"""
    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, directory=str(directory), **kwargs)

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    def stop():
        server.shutdown()
        server.server_close()
    return f'http://127.0.0.1:{server.server_address[1]}', stop


class InstallScriptTests(unittest.TestCase):
    def assert_refused(self, release, want, **run):
        code, out, err = release.run(**run)
        self.assertNotEqual(code, 0, out + err)
        self.assertIn(want, err, out + err)
        self.assertEqual(release.installed(), [], '校验没过就不该装任何东西')
        self.assertEqual(release.stages(), [], '暂存没清')

    def test_fresh_install_from_a_local_directory(self):
        release = Release(self)
        code, out, err = release.run()
        self.assertEqual(code, 0, out + err)
        self.assertEqual(release.installed(), ['summon', 'summon-sidecar'])
        self.assertEqual((release.bin / 'summon').read_bytes(), FAKE_SUMMON.encode())
        for name in release.installed():
            self.assertEqual((release.bin / name).stat().st_mode & 0o777, 0o755, name)
        self.assertTrue((release.home / 'stamp-recorded').exists(), '要登记构建戳')
        self.assertEqual(release.stages(), [])
        self.assertIn('export PATH="$HOME/.summon/bin:$PATH"', out)
        self.assertIn('summon onboard', out)

    def test_fresh_install_over_http(self):
        release = Release(self)
        base, stop = serve(release.dir)
        self.addCleanup(stop)
        code, out, err = release.run(source=base)
        self.assertEqual(code, 0, out + err)
        self.assertEqual(release.installed(), ['summon', 'summon-sidecar'])
        self.assertIn(f'{base}/manifest.json', out)

    def test_a_local_http_source_bypasses_the_proxy(self):
        """环境里配着代理(指向一个没人听的端口)也照样从本机来源装上:本机地址不经代理。"""
        release = Release(self)
        base, stop = serve(release.dir)
        self.addCleanup(stop)
        dead = 'http://127.0.0.1:9'
        code, out, err = release.run(source=base, http_proxy=dead, HTTPS_PROXY=dead,
                                     all_proxy=dead, ALL_PROXY=dead)
        self.assertEqual(code, 0, out + err)
        self.assertEqual(release.installed(), ['summon', 'summon-sidecar'])

    def test_a_missing_http_package_is_refused(self):
        release = Release(self)
        (release.dir / 'summon-linux-x64.tar.gz').unlink()
        base, stop = serve(release.dir)
        self.addCleanup(stop)
        self.assert_refused(release, '下载', source=base)

    def test_an_existing_install_is_left_alone(self):
        release = Release(self)
        release.bin.mkdir(parents=True)
        (release.bin / 'summon').write_bytes(b'old summon')
        code, out, err = release.run()
        self.assertEqual(code, 0, out + err)
        self.assertIn('summon upgrade', out)
        self.assertEqual((release.bin / 'summon').read_bytes(), b'old summon')
        self.assertEqual(release.installed(), ['summon'])

    def test_a_hash_mismatch_installs_nothing(self):
        release = Release(self)
        release.publish({'linux-x64': package()}, shas={'linux-x64': '0' * 64})
        self.assert_refused(release, 'SHA-256')

    def test_a_size_mismatch_installs_nothing(self):
        release = Release(self)
        data = package()
        release.publish({'linux-x64': data}, sizes={'linux-x64': len(data) + 1})
        self.assert_refused(release, '字节数')

    def test_a_stamp_mismatch_installs_nothing(self):
        for field, value in (('version', '0.7.1'), ('commit', 'abcdef13'),
                             ('commit_time', 1700000001)):
            with self.subTest(field=field):
                release = Release(self)
                release.stamp[field] = value
                self.assert_refused(release, '构建戳')

    def test_a_shorter_commit_of_the_same_build_is_accepted(self):
        release = Release(self)
        release.stamp['commit'] = 'abcdef1'
        code, out, err = release.run()
        self.assertEqual(code, 0, out + err)

    def test_packages_with_the_wrong_members_install_nothing(self):
        """解包之前先按清单拒绝(成员集合、成员类型),解包之后再逐个确认;这里钉的是清单那一层的
        报法,解包之后那一层另有兜底。"""
        cases = {
            '多成员': (package(extra=[('README', b'x', tarfile.REGTYPE)]), '的成员不对'),
            '少成员': (tarball([('summon', FAKE_SUMMON.encode(), tarfile.REGTYPE)]), '的成员不对'),
            '带路径': (tarball([('bin/summon', FAKE_SUMMON.encode(), tarfile.REGTYPE),
                              ('summon-sidecar', b'x', tarfile.REGTYPE)]), '的成员不对'),
            '链接': (tarball([('summon', FAKE_SUMMON.encode(), tarfile.REGTYPE),
                            ('summon-sidecar', b'', tarfile.SYMTYPE)]), '里有不是普通文件的成员'),
        }
        for what, (data, want) in cases.items():
            with self.subTest(what=what):
                release = Release(self)
                release.publish({'linux-x64': data})
                self.assert_refused(release, f'summon-linux-x64.tar.gz {want}')

    def test_a_release_without_this_platform_installs_nothing(self):
        release = Release(self)
        (release.dir / 'summon-linux-x64.tar.gz').unlink()
        release.publish({'linux-arm64': package('linux-arm64')})
        self.assert_refused(release, '没有本平台(linux-x64)的包')

    def test_an_unsupported_platform_is_pointed_at_source_builds(self):
        release = Release(self)
        release.uname = ('Linux', 'riscv64')
        self.assert_refused(release, '从源码安装')
        release.uname = ('Darwin', 'x86_64')
        self.assert_refused(release, '从源码安装')

    def test_plaintext_sources_other_than_this_machine_are_refused(self):
        """192.0.2.1 是文档专用、不可路由的地址:真去连只会等到超时,立刻拒绝说明没发请求。"""
        for source in ('http://192.0.2.1/release', 'http://example.invalid/release',
                       'http://localhost@192.0.2.1/release', 'http://127.0.0.1:80@192.0.2.1/x',
                       'http://127.0.0.1.example.invalid/release', 'http://[::1]x/release',
                       'http://localhost:1x/release'):
            with self.subTest(source=source):
                release = Release(self)
                started = time.monotonic()
                self.assert_refused(release, '只接受本机地址', source=source)
                self.assertLess(time.monotonic() - started, 10)

    def test_a_requested_version_must_match(self):
        release = Release(self)
        self.assert_refused(release, '不是要求的 0.7.1', SUMMON_INSTALL_VERSION='0.7.1')
        self.assert_refused(release, 'x.y.z', SUMMON_INSTALL_VERSION='latest')

    def test_darwin_packages_need_valid_signatures_from_one_team(self):
        release = Release(self)
        release.uname = ('Darwin', 'arm64')
        release.publish({'darwin-arm64': package('darwin-arm64')})
        code, out, err = release.run()
        self.assertEqual(code, 0, out + err)
        self.assertEqual(release.installed(), ['summon', 'summon-menubar', 'summon-sidecar'])
        self.assertIn('签名团队 TEAMA', out)

        for body, want in ((b'# team=TEAMB\n', 'TEAMB'), (b'# unsigned\n', 'codesign'),
                           (b'no team\n', '没有签名团队')):
            with self.subTest(want=want):
                release = Release(self)
                release.uname = ('Darwin', 'arm64')
                members = [('summon', FAKE_SUMMON.encode(), tarfile.REGTYPE),
                           ('summon-sidecar', b'# team=TEAMA\n', tarfile.REGTYPE),
                           ('summon-menubar', body, tarfile.REGTYPE)]
                release.publish({'darwin-arm64': tarball(members)})
                self.assert_refused(release, want)
                self.assertFalse((release.home / 'stamp-calls').exists(),
                                 '签名没过就执行了解出的 summon')

    def test_signatures_are_checked_before_anything_unpacked_runs(self):
        release = Release(self)
        release.uname = ('Darwin', 'arm64')
        release.publish({'darwin-arm64': package('darwin-arm64')})
        code, out, err = release.run()
        self.assertEqual(code, 0, out + err)
        self.assertEqual((release.home / 'stamp-calls').read_text(), 'run\n')
        self.assertLess(out.index('签名团队 TEAMA'), out.index('已安装'), out)

    def test_installs_completely_when_the_script_comes_through_stdin(self):
        """dash 会把管道里的整份脚本先读进缓冲,子进程偷读 stdin 也看不出来;bash(macOS 的 /bin/sh)
        从管道逐字节读脚本,哪个外部命令继承了 stdin,后面的脚本就没了。两个都跑。"""
        shells = ['sh'] + (['bash'] if shutil.which('bash') else [])
        cases = [(shell, platform, uname) for shell in shells
                 for platform, uname in (('linux-x64', ('Linux', 'x86_64')),
                                         ('darwin-arm64', ('Darwin', 'arm64')))]
        for shell, platform, uname in cases:
            with self.subTest(shell=shell, platform=platform):
                release = Release(self)
                release.uname = uname
                release.publish({platform: package(platform)})
                code, out, err = release.run(piped=shell)
                self.assertEqual(code, 0, out + err)
                self.assertIn('summon onboard', out, '脚本没有读完')
                self.assertTrue((release.home / 'stamp-recorded').exists(), out + err)
                self.assertIn('summon', release.installed())

    def test_an_oversized_package_record_installs_nothing(self):
        release = Release(self)
        data = package()
        release.publish({'linux-x64': data}, sizes={'linux-x64': 1024 * 1024 * 1024 + 1})
        self.assert_refused(release, '超过 1 GiB')

    def test_a_failed_install_leaves_no_temporary_files_in_bin(self):
        """换进落点那一步失败(这里用一个总失败的 mv 造出来):落点里不留 `.<名字>.new.<pid>`。"""
        release = Release(self)
        write_tool(release.tools / 'mv', '#!/bin/sh\necho "mv 坏了" >&2\nexit 1\n')
        code, out, err = release.run()
        self.assertNotEqual(code, 0, out + err)
        self.assertEqual(release.installed(), [], '临时文件留在了落点里')
        self.assertEqual(release.stages(), [])

    def test_the_default_source_is_the_repository_in_cargo_toml(self):
        cargo = (ROOT / 'crates/summon/Cargo.toml').read_text(encoding='utf-8')
        repository = re.search(r'^repository = "([^"]+)"', cargo, re.M).group(1)
        script = SCRIPT.read_text(encoding='utf-8')
        self.assertIn(f'REPOSITORY="{repository}"', script)
        self.assertIn('/releases/latest/download', script)
        self.assertNotIn('api.github.com', script)


if __name__ == '__main__':
    unittest.main()
