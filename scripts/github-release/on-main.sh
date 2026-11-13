#!/usr/bin/env bash
# 提交是否在 main 的第一父链上,即某次 PR 合入之后的 main 本身。
#
#   bash scripts/github-release/on-main.sh <完整提交号> <main 的 ref>
#
# 只认第一父链、不认「是 main 的祖先」:PR 里的中间提交在 merge commit 或 rebase 合入后也会成为
# main 的祖先,而只有合入后的那棵树才是作为整体 review 过的。仓库只允许 squash 合并,第一父链上
# 每个提交都是一次合入后的 main,这条判据才成立。调用方要有 main 的完整历史(fetch-depth: 0)。
set -euo pipefail

SHA="${1:-}"
REF="${2:-}"
if ! [[ "$SHA" =~ ^[0-9a-f]{40}$ ]] || [ -z "$REF" ]; then
  echo "用法:on-main.sh <完整提交号> <main 的 ref>" >&2
  exit 2
fi
# 先落进变量再比:grep -q 提前退出会让 rev-list 收到 SIGPIPE,在 pipefail 下误判。
CHAIN="$(git rev-list --first-parent "$REF")"
if grep -qxF -- "$SHA" <<< "$CHAIN"; then
  exit 0
fi
echo "$SHA 不在 $REF 的第一父链上(不是某次 PR 合入后的 main)" >&2
exit 1
