#!/bin/sh
# 从 GitHub Releases 安装 summon 的预编译包到 ~/.summon/bin/。
#
#   curl -fsSL https://raw.githubusercontent.com/baozzz1/summon/main/scripts/install.sh | sh
#   sh scripts/install.sh            # 在源码目录里
#
# 下载 manifest.json 与本平台的包,核对字节数、SHA-256 与包成员;macOS 先核代码签名与团队,
# 通过之后才运行解出的 `summon install-stamp print` 核对构建戳;再把三件套装进 ~/.summon/bin/
# (暂存 + rename)。落点已有 summon 时不覆盖,升级用 `summon upgrade`。依赖只有 curl、tar、
# sha256sum 或 shasum。
#
# 经 `curl … | sh` 运行时脚本本身就在 stdin 上,外部命令一律从 /dev/null 读,免得吃掉后面的脚本。
#
# 环境变量:
#   SUMMON_INSTALL_VERSION  要装的版本(x.y.z);默认最新版
#   SUMMON_INSTALL_FROM     Release 的来源:URL 前缀或本地目录,其下直接有 manifest.json 与各平台的包;
#                           默认 https://github.com/baozzz1/summon/releases/latest/download
#                           (给了 SUMMON_INSTALL_VERSION 时是 releases/download/v<版本>)。
#                           明文 http:// 只接受本机地址(127.0.0.1、localhost、[::1])
set -eu

REPOSITORY="https://github.com/baozzz1/summon"
# 运行解出的 summon 的时限(秒):刚解出的二进制在 macOS 首次执行要过一遍系统评估。
STAMP_TIMEOUT="${SUMMON_INSTALL_STAMP_TIMEOUT:-30}"

say() { printf '[install] %s\n' "$*"; }
die() {
  printf '[install] %s\n' "$*" >&2
  exit 1
}

VERSION="${SUMMON_INSTALL_VERSION:-}"
if [ -n "$VERSION" ]; then
  printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
    || die "SUMMON_INSTALL_VERSION 只接受 x.y.z,收到 ${VERSION}"
fi
if [ -n "${SUMMON_INSTALL_FROM:-}" ]; then
  FROM="$SUMMON_INSTALL_FROM"
elif [ -n "$VERSION" ]; then
  FROM="$REPOSITORY/releases/download/v$VERSION"
else
  FROM="$REPOSITORY/releases/latest/download"
fi

# 明文 http 只许本机:manifest 与包同源,明文链路上两者能被一起换掉,核对就没有意义。
# 主机后面只许跟纯数字端口,所以 `127.0.0.1@别处`、`127.0.0.1:80@别处` 这类带用户信息的都混不过去。
local_http() {
  authority="${1#http://}"
  authority="${authority%%/*}"
  for host in 127.0.0.1 localhost '[::1]'; do
    case "$authority" in
      "$host") return 0 ;;
      "$host":*)
        port="${authority#"$host":}"
        [ -n "$port" ] && [ -z "$(printf '%s' "$port" | tr -d 0-9)" ] && return 0
        ;;
    esac
  done
  return 1
}
case "$FROM" in
  http://*)
    local_http "$FROM" \
      || die "SUMMON_INSTALL_FROM 的明文 http:// 只接受本机地址(127.0.0.1、localhost、[::1]),收到 ${FROM};远程来源用 https:// 或本地目录"
    ;;
esac

case "$(uname -s < /dev/null)/$(uname -m < /dev/null)" in
  Linux/x86_64 | Linux/amd64) PLATFORM=linux-x64 ;;
  Linux/aarch64 | Linux/arm64) PLATFORM=linux-arm64 ;;
  Darwin/arm64 | Darwin/aarch64) PLATFORM=darwin-arm64 ;;
  *) die "这个平台($(uname -s < /dev/null) $(uname -m < /dev/null))没有预编译包,请从源码安装:见 $REPOSITORY 的 README「从源码构建」" ;;
esac
case "$PLATFORM" in
  darwin-*) FILES="summon summon-sidecar summon-menubar" ;;
  *) FILES="summon summon-sidecar" ;;
esac

[ -n "${HOME:-}" ] || die "HOME 没有设置"
BIN="$HOME/.summon/bin"
if [ -e "$BIN/summon" ]; then
  say "$BIN/summon 已存在,不覆盖。升级用 \`summon upgrade\`(或 \`$BIN/summon upgrade\`)。"
  exit 0
fi

for tool in tar; do
  command -v "$tool" > /dev/null 2>&1 || die "缺 $tool"
done
if command -v sha256sum > /dev/null 2>&1; then
  sha256_of() { sha256sum "$1" < /dev/null | awk '{print $1}'; }
elif command -v shasum > /dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" < /dev/null | awk '{print $1}'; }
else
  die "缺 sha256sum 或 shasum"
fi

mkdir -p "$HOME/.summon/run"
STAGE="$(mktemp -d "$HOME/.summon/run/install-stage.XXXXXX")"
# 退出时(含失败、被打断)清掉暂存与落点里没换进去的 `.<名字>.new.<pid>`。
cleanup() {
  rm -rf "$STAGE"
  for name in $FILES; do rm -f "$BIN/.$name.new.$$"; done
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

# fetch <附件名> <目标文件> <字节上限>
fetch() {
  case "$FROM" in
    https://*)
      command -v curl > /dev/null 2>&1 || die "缺 curl"
      # 只走 https,跳转也不降到明文。
      curl -fL --proto '=https' --proto-redir '=https' --silent --show-error --retry 2 \
        --connect-timeout 15 --max-time 900 --max-filesize "$3" -o "$2" "${FROM%/}/$1" \
        < /dev/null || die "下载 ${FROM%/}/$1 失败"
      ;;
    http://*)
      command -v curl > /dev/null 2>&1 || die "缺 curl"
      # 本机的明文来源(上面已核过):不经代理,不跟随跳转,免得被引到别处的明文地址。
      curl -f --proto '=http' --noproxy '*' --silent --show-error --connect-timeout 15 \
        --max-time 900 --max-filesize "$3" -o "$2" "${FROM%/}/$1" < /dev/null \
        || die "下载 ${FROM%/}/$1 失败"
      ;;
    *)
      [ -f "$FROM/$1" ] || die "$FROM/$1 不存在"
      cp "$FROM/$1" "$2"
      ;;
  esac
}

say "取 manifest:${FROM%/}/manifest.json"
fetch manifest.json "$STAGE/manifest.json" 262144
[ "$(wc -c < "$STAGE/manifest.json" | tr -d ' ')" -le 262144 ] || die "manifest 超过大小上限"
# manifest 由 scripts/github-release/manifest.py 生成;去掉空白后按字段取值,再逐个校验形状。
FLAT="$(tr -d ' \t\r\n' < "$STAGE/manifest.json")"
field() { printf '%s\n' "$1" | sed -n "s/.*\"$2\":\"\{0,1\}\([^\",}]*\)\"\{0,1\}[,}].*/\1/p" | head -n 1; }
M_VERSION="$(field "$FLAT" version)"
M_COMMIT="$(field "$FLAT" commit)"
M_TIME="$(field "$FLAT" commit_time)"
ENTRY="$(printf '%s\n' "$FLAT" | sed -n "s/.*\"$PLATFORM\":{\([^}]*\)}.*/\1/p")"
A_NAME="$(field "{$ENTRY}" name)"
A_SIZE="$(field "{$ENTRY}" size)"
A_SHA="$(field "{$ENTRY}" sha256)"

printf '%s\n' "$M_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "manifest 的版本不合法:${M_VERSION}"
printf '%s\n' "$M_COMMIT" | grep -Eq '^[0-9a-f]{7,40}$' || die "manifest 的提交不合法:${M_COMMIT}"
printf '%s\n' "$M_TIME" | grep -Eq '^[1-9][0-9]*$' || die "manifest 的提交时间不合法:${M_TIME}"
if [ -n "$VERSION" ] && [ "$M_VERSION" != "$VERSION" ]; then
  die "Release 的版本是 ${M_VERSION},不是要求的 ${VERSION}"
fi
[ -n "$ENTRY" ] || die "Release 的这一版 $M_VERSION 没有本平台(${PLATFORM})的包"
[ "$A_NAME" = "summon-$PLATFORM.tar.gz" ] || die "manifest 里 $PLATFORM 的文件名不对:${A_NAME}"
# 包的大小上限 1 GiB(与 summon upgrade 相同;现在的包约 20 MB),超过的记录不可信。
printf '%s\n' "$A_SIZE" | grep -Eq '^[1-9][0-9]{0,9}$' && [ "$A_SIZE" -le 1073741824 ] \
  || die "manifest 里 $PLATFORM 的字节数不合法或超过 1 GiB:${A_SIZE}"
printf '%s\n' "$A_SHA" | grep -Eq '^[0-9a-f]{64}$' || die "manifest 里 $PLATFORM 的 SHA-256 不合法:${A_SHA}"

say "下载 $A_NAME($A_SIZE 字节)"
fetch "$A_NAME" "$STAGE/$A_NAME" "$A_SIZE"
GOT_SIZE="$(wc -c < "$STAGE/$A_NAME" | tr -d ' ')"
[ "$GOT_SIZE" = "$A_SIZE" ] || die "$A_NAME 的字节数是 ${GOT_SIZE},manifest 记的是 ${A_SIZE}"
GOT_SHA="$(sha256_of "$STAGE/$A_NAME")"
[ "$GOT_SHA" = "$A_SHA" ] || die "$A_NAME 的 SHA-256 是 ${GOT_SHA},manifest 记的是 ${A_SHA}"

# 成员必须恰为本平台应有的文件、平铺的普通文件:先看清单,解开后再逐个确认。
LISTED="$(tar -tzf "$STAGE/$A_NAME" < /dev/null)" || die "$A_NAME 解不开"
WANT="$(printf '%s\n' $FILES | sort)"
[ "$(printf '%s\n' "$LISTED" | sort)" = "$WANT" ] \
  || die "$A_NAME 的成员不对:$(printf '%s ' $LISTED)(应为 ${FILES})"
tar -tzvf "$STAGE/$A_NAME" < /dev/null | while IFS= read -r line; do
  case "$line" in
    -*) ;;
    *) die "${A_NAME} 里有不是普通文件的成员:${line}" ;;
  esac
done || exit 1
PKG="$STAGE/pkg"
mkdir "$PKG"
tar -xzf "$STAGE/$A_NAME" -C "$PKG" --no-same-owner < /dev/null || die "$A_NAME 解不开"
[ "$(ls -A "$PKG" | wc -l | tr -d ' ')" = "$(printf '%s\n' $FILES | wc -l | tr -d ' ')" ] \
  || die "$A_NAME 解出了多余的文件"
for name in $FILES; do
  [ -f "$PKG/$name" ] && [ ! -L "$PKG/$name" ] || die "$A_NAME 里的 $name 不是普通文件"
  chmod 0755 "$PKG/$name"
done
rm -f "$STAGE/$A_NAME"

# 签名先于执行:解出的文件通过签名核对之前,一个都不运行。
if [ "$PLATFORM" = darwin-arm64 ]; then
  command -v codesign > /dev/null 2>&1 || die "缺 codesign,核对不了代码签名"
  TEAM=""
  for name in $FILES; do
    codesign --verify --strict "$PKG/$name" < /dev/null 2> "$STAGE/codesign.err" \
      || die "$name 的代码签名没通过 codesign --verify --strict:$(head -c 400 "$STAGE/codesign.err")"
    team="$(codesign -dv "$PKG/$name" < /dev/null 2>&1 | sed -n 's/^TeamIdentifier=//p' | head -n 1)"
    case "$team" in
      "" | "not set") die "$name 没有签名团队" ;;
    esac
    [ -z "$TEAM" ] || [ "$TEAM" = "$team" ] || die "签名团队不一致:$name 是 ${team},其余是 ${TEAM}"
    TEAM="$team"
  done
  say "代码签名有效,签名团队 $TEAM"
fi

# 解出的 summon 自报的构建戳必须就是 manifest 记的(提交按前缀认,短提交号长度随仓库变)。
"$PKG/summon" install-stamp print < /dev/null > "$STAGE/stamp.out" 2> "$STAGE/stamp.err" &
STAMP_PID=$!
(
  sleep "$STAMP_TIMEOUT"
  kill "$STAMP_PID" 2> /dev/null
) > /dev/null 2>&1 &
WATCH_PID=$!
STAMP_RC=0
wait "$STAMP_PID" || STAMP_RC=$?
kill "$WATCH_PID" 2> /dev/null || true
[ "$STAMP_RC" = 0 ] || die "解出的 summon install-stamp print 失败(退出码 ${STAMP_RC}):$(head -c 400 "$STAGE/stamp.err")"
STAMP="$(head -n 1 "$STAGE/stamp.out" | tr -d ' \t\r')"
S_VERSION="$(field "$STAMP" version)"
S_COMMIT="$(field "$STAMP" commit)"
S_TIME="$(field "$STAMP" commit_time)"
# 同一个提交:都是 7–40 位十六进制,一个是另一个的前缀。
same_commit() {
  printf '%s\n' "$1" | grep -Eq '^[0-9a-f]{7,40}$' || return 1
  case "$1" in "$2"*) return 0 ;; esac
  case "$2" in "$1"*) return 0 ;; esac
  return 1
}
if [ "$S_VERSION" != "$M_VERSION" ] || ! same_commit "$S_COMMIT" "$M_COMMIT" \
  || [ "$S_TIME" != "$M_TIME" ]; then
  die "解出的 summon 自报的构建戳(${S_VERSION} ${S_COMMIT} ${S_TIME})与 manifest(${M_VERSION} ${M_COMMIT} ${M_TIME})不一致"
fi

mkdir -p "$BIN"
[ -e "$BIN/summon" ] && die "$BIN/summon 刚刚出现了,不覆盖"
for name in $FILES; do
  cp "$PKG/$name" "$BIN/.$name.new.$$"
  chmod 0755 "$BIN/.$name.new.$$"
done
# summon 最后换进去:前面任何一个失败,落点里都还没有 summon,下次照样能重装。
for name in $FILES; do
  [ "$name" = summon ] || mv -f "$BIN/.$name.new.$$" "$BIN/$name"
done
mv -f "$BIN/.summon.new.$$" "$BIN/summon"
# 登记构建戳,之后 `summon upgrade` 据此比新旧;写不成只是那时多问一次 --version。
"$BIN/summon" install-stamp record < /dev/null > /dev/null 2>&1 || say "构建戳没写成,不影响使用"

say "已安装 summon $M_VERSION 到 $BIN"
case ":${PATH:-}:" in
  *":$BIN:"*) ;;
  *)
    say "把 $BIN 加进 PATH(写进 shell 的启动文件):"
    say "  export PATH=\"\$HOME/.summon/bin:\$PATH\""
    ;;
esac
say "下一步:summon onboard(创建机器人配置并连接本地 CLI),之后用 summon doctor 体检。"
