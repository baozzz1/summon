#!/usr/bin/env bash
# 发布工作流里刚打出的包交给上传之前的冒烟检查:解包后能跑,自报的构建戳与 .build.json 一致;
# macOS 包里每个二进制都是同一个团队的 Developer ID 签名。
#
#   bash scripts/github-release/smoke.sh <package.sh 的 --out 目录> <平台>
set -euo pipefail

OUT="${1:?缺 package.sh 的 --out 目录}"
PLATFORM="${2:?缺平台}"
cd "$(dirname "$0")/../.."
source scripts/github-release/lib.sh

die() {
  printf '[smoke] %s\n' "$*" >&2
  exit 1
}

TARBALL="$OUT/summon-$PLATFORM.tar.gz"
BUILD_JSON="$OUT/summon-$PLATFORM.build.json"
[ -f "$TARBALL" ] || die "缺 $TARBALL"
[ -f "$BUILD_JSON" ] || die "缺 $BUILD_JSON"

SMOKE="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/summon-smoke.XXXXXX")"
trap 'rm -rf "$SMOKE"' EXIT
tar -xzf "$TARBALL" -C "$SMOKE"
ls -l "$SMOKE"
"$SMOKE/summon" --version
"$SMOKE/summon" install-stamp print > "$SMOKE/stamp.json"
python3 - "$SMOKE/stamp.json" "$BUILD_JSON" << 'PY'
import json, sys
stamp = json.load(open(sys.argv[1]))
build = json.load(open(sys.argv[2]))
for field in ('version', 'commit', 'commit_time'):
    if stamp[field] != build[field]:
        sys.exit(f'{field}: 二进制自报 {stamp[field]!r},.build.json 记的是 {build[field]!r}')
print('构建戳一致:', stamp)
PY

if [ "${PLATFORM%%-*}" = darwin ]; then
  TEAMS=""
  while IFS= read -r name; do
    codesign --verify --strict "$SMOKE/$name" || die "$name 签名校验失败"
    INFO="$(codesign -dvv "$SMOKE/$name" 2>&1)"
    case "$(printf '%s\n' "$INFO" | sed -n 's/^Authority=//p' | head -n 1)" in
      'Developer ID Application:'*) ;;
      *) die "$name 不是 Developer ID Application 签名" ;;
    esac
    printf '%s\n' "$INFO" | grep -q '^Timestamp=' || die "$name 缺安全时间戳"
    TEAMS="$TEAMS$(printf '%s\n' "$INFO" | sed -n 's/^TeamIdentifier=//p')"$'\n'
  done < <(summon_release_files "$PLATFORM")
  [ "$(printf '%s' "$TEAMS" | sort -u | awk 'NF' | wc -l | tr -d ' ')" = 1 ] \
    || die "包里的二进制不是同一个团队签的"
  echo "签名:Developer ID,团队 $(printf '%s' "$TEAMS" | awk 'NF' | head -n 1)"
fi
