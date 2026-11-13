#!/usr/bin/env bash
# scripts/github-release/on-main.sh 的测试:在临时仓库里造 squash 合入与 merge commit 合入。
#
#   bash scripts/tests/test-github-release-on-main.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SCRIPT="$ROOT/scripts/github-release/on-main.sh"
fail() {
  printf 'FAIL %s\n' "$*" >&2
  exit 1
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/summon-on-main.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"
git init -q
git checkout -q -b main
git config user.name test
git config user.email test@example.com
commit() {
  git commit -q --allow-empty -m "$1"
  git rev-parse HEAD
}

BASE="$(commit base)"
git checkout -q -b feature
MIDDLE="$(commit 'PR 的中间提交')"
TIP="$(commit 'PR 的最后一个提交')"
git checkout -q main
SQUASHED="$(commit 'squash 合入')"
git merge -q --no-ff -m 'merge commit 合入' feature
MERGE="$(git rev-parse HEAD)"

bash "$SCRIPT" "$BASE" main || fail '初始提交在第一父链上'
bash "$SCRIPT" "$SQUASHED" main || fail 'squash 合入的提交在第一父链上'
bash "$SCRIPT" "$MERGE" main || fail '合入后的 main 在第一父链上'
git merge-base --is-ancestor "$MIDDLE" main || fail '前提:中间提交是 main 的祖先'
for sha in "$MIDDLE" "$TIP"; do
  if bash "$SCRIPT" "$sha" main 2> /dev/null; then fail "PR 的中间提交不该算:$sha"; fi
done
rc=0
bash "$SCRIPT" "${BASE:0:7}" main 2> /dev/null || rc=$?
[ "$rc" = 2 ] || fail "短提交号应退出 2,实际 $rc"
rc=0
bash "$SCRIPT" "$BASE" 2> /dev/null || rc=$?
[ "$rc" = 2 ] || fail "缺 ref 应退出 2,实际 $rc"

echo 'github release on-main tests: ok'
