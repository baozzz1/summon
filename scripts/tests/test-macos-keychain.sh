#!/usr/bin/env bash
# scripts/github-release/macos-keychain.sh 的测试。用一次性的自签证书(不是 Developer ID)走一遍导入,
# 断言:密码与证书不进输出;中间证书摘要对不上就失败;没有 Developer ID 身份就失败;cleanup 删掉钥匙串、
# 恢复搜索列表,且可重复执行。
#
#   bash scripts/tests/test-macos-keychain.sh
#
# 会临时改动当前用户的钥匙串搜索列表(结束时恢复),所以只在 CI 里跑;本机要跑设 SUMMON_TEST_KEYCHAIN=1。
set -euo pipefail

cd "$(dirname "$0")/../.."
SCRIPT=scripts/github-release/macos-keychain.sh

fail() {
  printf 'FAIL %s\n' "$*" >&2
  exit 1
}

# 参数错误在任何平台都先于平台检查报出。
rc=0
bash "$SCRIPT" > /dev/null 2>&1 || rc=$?
[ "$rc" = 2 ] || fail "缺子命令应退出 2,实际 $rc"
rc=0
bash "$SCRIPT" setup > /dev/null 2>&1 || rc=$?
[ "$rc" = 2 ] || fail "缺 --state 应退出 2,实际 $rc"

if [ "$(uname -s)" != Darwin ]; then
  echo 'macos keychain tests: skipped (not macOS)'
  exit 0
fi
if [ "${GITHUB_ACTIONS:-}" != true ] && [ -z "${SUMMON_TEST_KEYCHAIN:-}" ]; then
  echo 'macos keychain tests: skipped (会改钥匙串搜索列表;本机设 SUMMON_TEST_KEYCHAIN=1 才跑)'
  exit 0
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/summon-keychain-test.XXXXXX")"
STATE="$WORK/state"
ORIGINAL_LIST="$(security list-keychains -d user)"
finish() {
  bash "$SCRIPT" cleanup --state "$STATE" > /dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap finish EXIT

# 系统自带的 LibreSSL 打出的 p12 用 security 认得的旧算法。
OPENSSL=/usr/bin/openssl
P12_PASSWORD="p12-$($OPENSSL rand -hex 12)"
"$OPENSSL" req -x509 -newkey rsa:2048 -nodes -days 1 -subj '/CN=summon keychain test' \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" > /dev/null 2>&1
"$OPENSSL" pkcs12 -export -inkey "$WORK/key.pem" -in "$WORK/cert.pem" \
  -passout "pass:$P12_PASSWORD" -out "$WORK/test.p12" > /dev/null 2>&1
P12_BASE64="$(base64 < "$WORK/test.p12" | tr -d '\n')"
printf 'not the apple certificate\n' > "$WORK/bogus.cer"

assert_no_secret() {
  case "$1" in
    *"$P12_PASSWORD"* | *"${P12_BASE64:0:40}"*) fail "$2:输出里出现了密码或证书" ;;
  esac
}

# ── 缺环境变量:不建任何东西 ──
rc=0
out="$(env -u SUMMON_MACOS_CERT_P12 -u SUMMON_MACOS_CERT_PASSWORD \
  bash "$SCRIPT" setup --state "$STATE" 2>&1)" || rc=$?
[ "$rc" = 1 ] || fail "缺证书应失败"
[ ! -e "$STATE" ] || fail "缺证书时不该建状态目录"

# ── 密码不对:导入失败,不泄露 ──
rc=0
out="$(SUMMON_MACOS_CERT_P12="$P12_BASE64" SUMMON_MACOS_CERT_PASSWORD="wrong-$P12_PASSWORD" \
  bash "$SCRIPT" setup --state "$STATE" 2>&1)" || rc=$?
[ "$rc" = 1 ] || fail "密码不对应失败"
case "$out" in *导入证书失败*) ;; *) fail "密码不对的提示:$out" ;; esac
assert_no_secret "$out" '密码不对'
[ ! -e "$STATE/cert.p12" ] || fail '导入失败后 p12 应已删除'
bash "$SCRIPT" cleanup --state "$STATE" > /dev/null 2>&1
[ ! -e "$STATE" ] || fail 'cleanup 应删掉状态目录'
[ "$(security list-keychains -d user)" = "$ORIGINAL_LIST" ] || fail '密码不对之后搜索列表没恢复'

# ── 导入成功但不是 Developer ID:去补中间证书,摘要对不上就失败 ──
rc=0
out="$(SUMMON_MACOS_CERT_P12="$P12_BASE64" SUMMON_MACOS_CERT_PASSWORD="$P12_PASSWORD" \
  SUMMON_DEVELOPER_ID_CA_URL="file://$WORK/bogus.cer" GITHUB_ENV="$WORK/github-env" \
  bash "$SCRIPT" setup --state "$STATE" 2>&1)" || rc=$?
[ "$rc" = 1 ] || fail "没有 Developer ID 身份应失败:$out"
case "$out" in *SHA-256*) ;; *) fail "中间证书摘要不符的提示:$out" ;; esac
assert_no_secret "$out" '自签证书'
[ ! -e "$STATE/cert.p12" ] || fail '导入后 p12 应已删除'
KEYCHAIN="$STATE/signing.keychain-db"
[ -f "$KEYCHAIN" ] || fail '应建了临时钥匙串'
security list-keychains -d user | grep -qF "$KEYCHAIN" || fail '临时钥匙串应在搜索列表里'
security find-certificate -c 'summon keychain test' "$KEYCHAIN" > /dev/null 2>&1 \
  || fail '证书应已导入临时钥匙串'
[ ! -s "$WORK/github-env" ] || fail '失败时不该写 GITHUB_ENV'

# ── cleanup:删钥匙串、恢复搜索列表,可重复执行 ──
bash "$SCRIPT" cleanup --state "$STATE" > /dev/null 2>&1
[ ! -e "$KEYCHAIN" ] || fail 'cleanup 应删掉钥匙串'
[ "$(security list-keychains -d user)" = "$ORIGINAL_LIST" ] || fail 'cleanup 应恢复搜索列表'
bash "$SCRIPT" cleanup --state "$STATE" > /dev/null 2>&1 || fail 'cleanup 应可重复执行'

echo 'macos keychain tests: ok'
