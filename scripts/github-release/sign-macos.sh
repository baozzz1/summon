#!/usr/bin/env bash
# 发布工作流的签名 job 用:给构建 job 交来的未签名 macOS 二进制做 Developer ID 签名(带安全时间戳)
# 并打包。只运行 codesign、security 与 pack.py,不执行被签名的二进制,也不构建。
#
#   bash scripts/github-release/sign-macos.sh --unsigned <目录> --sha <提交> --out <目录>
#
# <目录> 是 `package.sh --unsigned` 产出的 summon-darwin-arm64.unsigned/:恰是包里的三个二进制加
# stamp.json(构建 job 在签名之前读出的构建戳),构建戳的提交必须是 --sha 的前缀。签名身份按
# scripts/codesign.sh 的规则选(SUMMON_CODESIGN_IDENTITY 由 macos-keychain.sh 写进环境)。
# 产物:<out>/summon-darwin-arm64.tar.gz 与 .build.json。
set -euo pipefail

UNSIGNED=""
SHA=""
OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --unsigned)
      UNSIGNED="${2:?--unsigned 要跟一个目录}"
      shift 2
      ;;
    --sha)
      SHA="${2:?--sha 要跟一个提交号}"
      shift 2
      ;;
    --out)
      OUT="${2:?--out 要跟一个目录}"
      shift 2
      ;;
    *)
      echo "不认识的参数:$1" >&2
      exit 2
      ;;
  esac
done
if [ -z "$UNSIGNED" ] || [ -z "$SHA" ] || [ -z "$OUT" ]; then
  echo "用法:sign-macos.sh --unsigned <目录> --sha <提交> --out <目录>" >&2
  exit 2
fi

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLATFORM=darwin-arm64
die() {
  printf '[sign] %s\n' "$*" >&2
  exit 1
}

[ "$(uname -s)" = Darwin ] || die "只在 macOS 上签名"
python3 "$HERE/pack.py" check --stage "$UNSIGNED" --platform "$PLATFORM" \
  --stamp "$UNSIGNED/stamp.json" --sha "$SHA" || die "未签名产物核对失败"

# shellcheck source=scripts/codesign.sh
source "$HERE/../codesign.sh"
export SUMMON_CODESIGN_MODE=developer-id SUMMON_CODESIGN_TIMESTAMP=required
summon_prepare_codesign || die "签名不可用"
[ "$SUMMON_CODESIGN_KIND" = developer-id ] || die "没有 Developer ID 签名身份"

mkdir -p "$OUT"
STAGE="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/summon-sign.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
cp "$UNSIGNED/summon" "$UNSIGNED/summon-sidecar" "$UNSIGNED/summon-menubar" "$UNSIGNED/stamp.json" "$STAGE/"
chmod 0755 "$STAGE/summon" "$STAGE/summon-sidecar" "$STAGE/summon-menubar"
summon_sign_path "$STAGE/summon" com.summon.cli || die "summon 签名失败"
summon_sign_path "$STAGE/summon-sidecar" com.summon.sidecar || die "summon-sidecar 签名失败"
summon_sign_path "$STAGE/summon-menubar" com.summon.menubar || die "summon-menubar 签名失败"
python3 "$HERE/pack.py" pack --stage "$STAGE" --platform "$PLATFORM" --stamp "$STAGE/stamp.json" \
  --sha "$SHA" --out "$OUT" || die "打包失败"
