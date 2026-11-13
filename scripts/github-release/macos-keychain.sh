#!/usr/bin/env bash
# 发布工作流的 macOS job 用的临时签名钥匙串。
#
#   bash scripts/github-release/macos-keychain.sh setup --state <目录>
#   bash scripts/github-release/macos-keychain.sh cleanup --state <目录>
#
# setup:建一个随机密码的临时钥匙串,把 SUMMON_MACOS_CERT_P12(base64 的 p12)以
# SUMMON_MACOS_CERT_PASSWORD 导入(私钥不可导出,只放行 codesign),放到用户钥匙串搜索列表最前面。
# 钥匙串里必须恰有一个有效的 Developer ID Application 身份;证书链缺中间证书时,按钉住的 SHA-256
# 下载 Apple 的 Developer ID 中间证书补进这个钥匙串。在 GitHub Actions 里把身份的 SHA-1 写进
# $GITHUB_ENV 的 SUMMON_CODESIGN_IDENTITY(scripts/codesign.sh 按它签名)。
# cleanup:恢复搜索列表、删除钥匙串与状态目录;什么都没建过也成功,可重复执行。
#
# 证书、私钥与两个密码都不进输出:不开 set -x,security 的输出一律丢弃,p12 只在导入那一刻落盘
# (0600,导入后立即删除)。
#
# 环境变量:
#   SUMMON_MACOS_CERT_P12、SUMMON_MACOS_CERT_PASSWORD  setup 必需
#   SUMMON_DEVELOPER_ID_CA_URL     中间证书的下载地址(测试用来换成本地文件)
set -euo pipefail
umask 077

# Apple「Developer ID Certification Authority」中间证书(DER)的 SHA-256。发布签名证书由它签发;
# 换成 G2 中间证书签发的证书时,地址与摘要一起改。
DEVELOPER_ID_CA_URL="${SUMMON_DEVELOPER_ID_CA_URL:-https://www.apple.com/certificateauthority/DeveloperIDCA.cer}"
DEVELOPER_ID_CA_SHA256="7afc9d01a62f03a2de9637936d4afe68090d2de18d03f29c88cfb0b1ba63587f"

say() { printf '[keychain] %s\n' "$*" >&2; }
die() {
  printf '[keychain] %s\n' "$*" >&2
  exit 1
}

usage() {
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

CMD="${1:-}"
[ $# -gt 0 ] && shift
STATE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --state)
      STATE="${2:?--state 要跟一个目录}"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "不认识的参数:$1" >&2
      exit 2
      ;;
  esac
done
case "$CMD" in
  setup | cleanup) ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    echo "用法:macos-keychain.sh setup|cleanup --state <目录>" >&2
    exit 2
    ;;
esac
[ -n "$STATE" ] || {
  echo "缺 --state <目录>" >&2
  exit 2
}
[ "$(uname -s)" = Darwin ] || die "只在 macOS 上用"

KEYCHAIN="$STATE/signing.keychain-db"

# 用户钥匙串搜索列表,一行一个路径。
search_list() {
  security list-keychains -d user | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//'
}

# 这个钥匙串里有效的 Developer ID Application 身份的 SHA-1,一行一个。
developer_identities() {
  security find-identity -v -p codesigning "$KEYCHAIN" 2> /dev/null \
    | sed -nE 's/^[[:space:]]*[0-9]+\)[[:space:]]+([0-9A-F]{40})[[:space:]]+"Developer ID Application:.*"$/\1/p' \
    | sort -u
}

file_sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

cleanup() {
  local saved=() path
  if [ -f "$STATE/search-list" ]; then
    while IFS= read -r path; do
      [ -n "$path" ] && saved+=("$path")
    done < "$STATE/search-list"
    if [ "${#saved[@]}" -gt 0 ]; then
      security list-keychains -d user -s "${saved[@]}" > /dev/null 2>&1 \
        || say "恢复钥匙串搜索列表失败"
    fi
  fi
  if [ -e "$KEYCHAIN" ]; then
    security delete-keychain "$KEYCHAIN" > /dev/null 2>&1 || rm -f "$KEYCHAIN"
    say "已删除临时钥匙串"
  fi
  rm -rf "$STATE"
}

setup() {
  [ -n "${SUMMON_MACOS_CERT_P12:-}" ] || die "缺 SUMMON_MACOS_CERT_P12"
  [ -n "${SUMMON_MACOS_CERT_PASSWORD:-}" ] || die "缺 SUMMON_MACOS_CERT_PASSWORD"
  [ ! -e "$STATE" ] || die "$STATE 已存在;先 cleanup"
  mkdir -p "$STATE"

  local keychain_password path identities count
  keychain_password="$(openssl rand -hex 32)"
  [ "${#keychain_password}" = 64 ] || die "生成钥匙串密码失败"
  if [ "${GITHUB_ACTIONS:-}" = true ]; then
    echo "::add-mask::$keychain_password"
  fi

  # 先记下原来的搜索列表,cleanup 照它恢复。
  search_list > "$STATE/search-list"

  security create-keychain -p "$keychain_password" "$KEYCHAIN" > /dev/null 2>&1 \
    || die "建临时钥匙串失败"
  security set-keychain-settings -lut 7200 "$KEYCHAIN" > /dev/null 2>&1 \
    || die "设置临时钥匙串失败"
  security unlock-keychain -p "$keychain_password" "$KEYCHAIN" > /dev/null 2>&1 \
    || die "解锁临时钥匙串失败"

  printf '%s' "$SUMMON_MACOS_CERT_P12" | base64 --decode > "$STATE/cert.p12" 2> /dev/null || {
    rm -f "$STATE/cert.p12"
    die "SUMMON_MACOS_CERT_P12 不是有效的 base64"
  }
  # -x:私钥不可导出;-T:只放行 codesign。
  if ! security import "$STATE/cert.p12" -k "$KEYCHAIN" -f pkcs12 \
    -P "$SUMMON_MACOS_CERT_PASSWORD" -x -T /usr/bin/codesign > /dev/null 2>&1; then
    rm -f "$STATE/cert.p12"
    die "导入证书失败(p12 或密码不对)"
  fi
  rm -f "$STATE/cert.p12"
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
    -k "$keychain_password" "$KEYCHAIN" > /dev/null 2>&1 \
    || die "设置私钥访问控制失败"

  local list=("$KEYCHAIN")
  while IFS= read -r path; do
    [ -n "$path" ] && [ "$path" != "$KEYCHAIN" ] && list+=("$path")
  done < "$STATE/search-list"
  security list-keychains -d user -s "${list[@]}" > /dev/null 2>&1 \
    || die "把临时钥匙串加进搜索列表失败"

  identities="$(developer_identities)"
  if [ -z "$identities" ]; then
    say "没有有效的 Developer ID 身份,补 Apple 的 Developer ID 中间证书再查"
    curl -fsSL --retry 3 -o "$STATE/developer-id-ca.cer" "$DEVELOPER_ID_CA_URL" \
      || die "下载 Developer ID 中间证书失败"
    [ "$(file_sha256 "$STATE/developer-id-ca.cer")" = "$DEVELOPER_ID_CA_SHA256" ] \
      || die "Developer ID 中间证书的 SHA-256 与钉住的不符"
    security import "$STATE/developer-id-ca.cer" -k "$KEYCHAIN" > /dev/null 2>&1 \
      || die "导入 Developer ID 中间证书失败"
    identities="$(developer_identities)"
  fi
  count="$(printf '%s\n' "$identities" | awk 'NF { n++ } END { print n + 0 }')"
  [ "$count" = 1 ] || die "临时钥匙串里应恰有一个有效的 Developer ID Application 身份,实际 $count 个"
  if [ -n "${GITHUB_ENV:-}" ]; then
    printf 'SUMMON_CODESIGN_IDENTITY=%s\n' "$identities" >> "$GITHUB_ENV"
  fi
  say "临时钥匙串就绪,Developer ID 身份 $identities"
}

"$CMD"
