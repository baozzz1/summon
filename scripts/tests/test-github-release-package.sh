#!/usr/bin/env bash
# scripts/github-release/lib.sh 与 package.sh 参数解析的测试(不构建)。
#
#   bash scripts/tests/test-github-release-package.sh
set -euo pipefail

cd "$(dirname "$0")/../.."
source scripts/github-release/lib.sh

fail() {
  printf 'FAIL %s\n' "$*" >&2
  exit 1
}
assert_eq() {
  [ "$2" = "$1" ] || fail "$3: want=$(printf '%q' "$1") got=$(printf '%q' "$2")"
}

# ── 平台名 ──
assert_eq linux-x64 "$(summon_release_platform Linux x86_64)" 'linux x86_64'
assert_eq linux-x64 "$(summon_release_platform Linux amd64)" 'linux amd64'
assert_eq linux-arm64 "$(summon_release_platform Linux aarch64)" 'linux aarch64'
assert_eq linux-arm64 "$(summon_release_platform Linux arm64)" 'linux arm64'
assert_eq darwin-arm64 "$(summon_release_platform Darwin arm64)" 'darwin arm64'
for unsupported in 'Darwin x86_64' 'FreeBSD amd64' 'Linux riscv64' 'MINGW64_NT x86_64'; do
  # shellcheck disable=SC2086
  if summon_release_platform $unsupported > /dev/null; then
    fail "不发布的平台被接受了:$unsupported"
  fi
done
assert_eq $'summon\nsummon-sidecar' "$(summon_release_files linux-x64)" 'linux 包的文件'
assert_eq $'summon\nsummon-sidecar\nsummon-menubar' "$(summon_release_files darwin-arm64)" 'macOS 包的文件'
if summon_release_files darwin-x64 > /dev/null; then fail 'darwin-x64 不发'; fi

# ── glibc 版本提取 ──
OBJDUMP_SAMPLE='
0000000000000000      DF *UND*  0000000000000000 (GLIBC_2.3.2) pthread_cond_wait
0000000000000000      DF *UND*  0000000000000000 (GLIBC_2.34) __libc_start_main
0000000000000000      DF *UND*  0000000000000000 (GLIBC_2.17) clock_gettime
0000000000000000      DF *UND*  0000000000000000 (GLIBC_2.2.5) memcpy
0000000000000000      DF *UND*  0000000000000000 (OPENSSL_3.0.0) SSL_new'
assert_eq 2.34 "$(printf '%s\n' "$OBJDUMP_SAMPLE" | summon_glibc_max_version)" 'objdump 输出取最大版本'
READELF_SAMPLE='
Version needs section ".gnu.version_r" contains 2 entries:
 Addr: 0x0000000000000000  Offset: 0x000000  Link: 7 (.dynstr)
  000000: Version: 1  File: libc.so.6  Cnt: 3
  0x0010:   Name: GLIBC_2.36  Flags: none  Version: 5
  0x0020:   Name: GLIBC_2.4  Flags: none  Version: 4
  0x0030:   Name: GLIBC_2.35  Flags: none  Version: 3'
assert_eq 2.36 "$(printf '%s\n' "$READELF_SAMPLE" | summon_glibc_max_version)" 'readelf 输出取最大版本(2.36 > 2.4)'
assert_eq '' "$(printf 'no glibc here\n' | summon_glibc_max_version)" '没有 glibc 符号'

# ── 版本比较 ──
summon_version_le 2.35 2.35 || fail '2.35 ≤ 2.35'
summon_version_le 2.34 2.35 || fail '2.34 ≤ 2.35'
summon_version_le 2.3.2 2.35 || fail '2.3.2 ≤ 2.35'
summon_version_le 2.9 2.35 || fail '2.9 ≤ 2.35(按数字比,不按字符串)'
summon_version_le 2.35 2.35.0 || fail '2.35 ≤ 2.35.0'
if summon_version_le 2.36 2.35; then fail '2.36 > 2.35'; fi
if summon_version_le 2.35.1 2.35; then fail '2.35.1 > 2.35'; fi
if summon_version_le 3.0 2.35; then fail '3.0 > 2.35'; fi
rc=0
summon_version_le 2.x 2.35 || rc=$?
assert_eq 2 "$rc" '不是点分数字时返回 2'
assert_eq 2.35 "$SUMMON_RELEASE_GLIBC_MAX" '上限是 Ubuntu 22.04 的 glibc'

# ── glibc 检查(objdump 换成假的) ──
objdump() {
  case "$2" in
    */old) printf '%s\n' "$OBJDUMP_SAMPLE" ;;
    */new) printf '(GLIBC_2.34) a\n(GLIBC_2.38) b\n' ;;
    */static)
      printf 'objdump: %s: not a dynamic object\n' "$2"
      return 1
      ;;
    *)
      printf 'objdump: %s: file format not recognized\n' "$2"
      return 1
      ;;
  esac
}
summon_check_glibc 2.35 /bin/old /bin/static > /dev/null || fail '2.34 与静态二进制应通过'
err="$(summon_check_glibc 2.35 /bin/old /bin/new 2>&1 > /dev/null)" && fail '引用 2.38 的应失败'
case "$err" in
  */bin/new*GLIBC_2.38*2.35*) ;;
  *) fail "失败信息要报出二进制与版本:$err" ;;
esac
case "$err" in
  */bin/old*) fail "通过的二进制不该出现在失败信息里:$err" ;;
esac
if summon_check_glibc 2.35 /bin/garbage > /dev/null 2>&1; then
  fail 'objdump 读不了的文件不能当作通过'
fi
summon_check_glibc 2.34 /bin/old > /dev/null || fail '恰好等于上限应通过'
if summon_check_glibc 2.33 /bin/old > /dev/null 2>&1; then fail '调低上限后应失败'; fi
unset -f objdump

# ── package.sh 的参数 ──
rc=0
bash scripts/github-release/package.sh > /dev/null 2>&1 || rc=$?
assert_eq 2 "$rc" '缺 --out'
rc=0
bash scripts/github-release/package.sh --out /nonexistent/x --bogus > /dev/null 2>&1 || rc=$?
assert_eq 2 "$rc" '不认识的参数'
rc=0
bash scripts/github-release/package.sh --out /nonexistent/x --build-only --skip-build > /dev/null 2>&1 || rc=$?
assert_eq 2 "$rc" '--build-only 与 --skip-build 互斥'
help="$(bash scripts/github-release/package.sh --help)"
for flag in --build-only --skip-build --adhoc-sign SUMMON_ALLOW_DIRTY; do
  case "$help" in *"$flag"*) ;; *) fail "--help 要列出 $flag" ;; esac
done

echo 'github release package tests: ok'
