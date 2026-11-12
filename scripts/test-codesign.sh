#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/codesign.sh

assert_eq() {
  local want="$1"
  local got="$2"
  local label="$3"
  if [ "$got" != "$want" ]; then
    printf 'FAIL %s: want=%q got=%q\n' "$label" "$want" "$got" >&2
    exit 1
  fi
}

assert_eq \
  'developer-id|CERT_A' \
  "$(summon_select_codesign_identity auto '' 'CERT_A')" \
  'auto picks the only Developer ID identity'
assert_eq \
  'adhoc|' \
  "$(summon_select_codesign_identity auto '' '')" \
  'auto falls back to ad-hoc without Developer ID'
assert_eq \
  'ambiguous|' \
  "$(summon_select_codesign_identity auto '' $'CERT_A\nCERT_B')" \
  'auto refuses to guess between teams'
assert_eq \
  'developer-id|EXPLICIT_CERT' \
  "$(summon_select_codesign_identity auto 'EXPLICIT_CERT' $'CERT_A\nCERT_B')" \
  'explicit identity wins over discovery'
assert_eq \
  'error|' \
  "$(summon_select_codesign_identity developer-id '' '')" \
  'required Developer ID fails closed when absent'
assert_eq \
  'adhoc|' \
  "$(summon_select_codesign_identity adhoc 'EXPLICIT_CERT' 'CERT_A')" \
  'ad-hoc mode is an explicit local escape hatch'
assert_eq \
  'error|' \
  "$(summon_select_codesign_identity typo '' 'CERT_A')" \
  'unknown signing mode fails closed'
assert_eq \
  'best-effort' \
  "$(summon_select_timestamp_policy '' best-effort)" \
  'local deploy defaults to best-effort timestamping'
assert_eq \
  'required' \
  "$(summon_select_timestamp_policy required best-effort)" \
  'distribution can require timestamping'
assert_eq \
  'error' \
  "$(summon_select_timestamp_policy typo required)" \
  'unknown timestamp policy fails closed'

# 模拟 Apple timestamp 服务失败：本地 best-effort 必须只降级时间戳，不能降级 Team 身份。
SIGN_CALLS=""
CODESIGN_INFO_AUTHORITY='Developer ID Application: Test (TESTTEAM01)'
xattr() { :; }
codesign() {
  SIGN_CALLS="${SIGN_CALLS}|$*"
  case " $* " in
    *' --timestamp '*) return 1 ;;
    *' -dvvv '*)
      printf 'Identifier=com.summon.test\nAuthority=%s\nTeamIdentifier=TESTTEAM01\n' \
        "$CODESIGN_INFO_AUTHORITY"
      ;;
  esac
  return 0
}
SUMMON_CODESIGN_KIND="developer-id"
SUMMON_CODESIGN_CERT="CERT_A"
SUMMON_CODESIGN_TIMESTAMP_POLICY="best-effort"
summon_sign_path scripts/test-codesign.sh com.summon.test
case "$SIGN_CALLS" in
  *'--timestamp'*'--timestamp=none'*) ;;
  *)
    printf 'FAIL best-effort timestamp fallback was not exercised: %s\n' "$SIGN_CALLS" >&2
    exit 1
    ;;
esac
CODESIGN_INFO_AUTHORITY='Apple Development: Test (TESTTEAM01)'
if summon_sign_path scripts/test-codesign.sh com.summon.test 2>/dev/null; then
  echo 'FAIL Apple Development certificate was accepted as Developer ID' >&2
  exit 1
fi

echo 'codesign selection tests: ok'
