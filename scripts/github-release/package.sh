#!/usr/bin/env bash
# 把当前源码树构建成 GitHub Release 的一个平台包。镜像仓的发布工作流(Linux)与发版的 Mac(macOS)
# 共用这一份。
#
#   bash scripts/github-release/package.sh --out <目录> [--skip-build] [--adhoc-sign]
#
# 产物(都落在 --out):
#   summon-<平台>.tar.gz     summon、summon-sidecar(macOS 另有 summon-menubar),平铺、0755
#   summon-<平台>.build.json 包里 summon 自报的构建戳(版本、提交、提交时间)+ 平台、文件名、字节数、SHA-256
# 平台名:linux-x64、linux-arm64、darwin-arm64。三个平台的 .build.json 交给 manifest.py 汇总成
# manifest.json。
#
# --skip-build  不构建,直接用已有的构建产物打包;产物自报的提交必须仍是当前 HEAD
# --adhoc-sign  macOS 上用 ad-hoc 签名(只给测试用);默认要求 Developer ID 签名并带安全时间戳
#
# 环境变量:
#   CARGO_TARGET_DIR          cargo 的构建目录(cargo 自己认;也认 .cargo/config.toml 的重定向)
#   SUMMON_SWIFT_SCRATCH_PATH Menu Bar 的构建目录(默认 menubar/.build)
#   SUMMON_CODESIGN_IDENTITY  有多个 Developer ID 证书时指定用哪个(scripts/codesign.sh)
#   SUMMON_ALLOW_DIRTY=1      工作区有未提交改动也打包(那样的包不可发布:构建戳里的提交对不上源码)
SUMMON_NEEDS_BASH=""
[ -n "${BASH_VERSION:-}" ] || SUMMON_NEEDS_BASH=1
case ":${SHELLOPTS:-}:" in
  *:posix:*) SUMMON_NEEDS_BASH=1 ;;
esac
if [ -n "$SUMMON_NEEDS_BASH" ]; then
  if command -v bash > /dev/null 2>&1; then
    exec bash "$0" "$@"
  fi
  echo "本脚本需要 bash(且不能是 posix 模式)" >&2
  exit 1
fi
set -euo pipefail

usage() {
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

OUT=""
SKIP_BUILD=0
ADHOC_SIGN=0
while [ $# -gt 0 ]; do
  case "$1" in
    --out)
      OUT="${2:?--out 要跟一个目录}"
      shift 2
      ;;
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    --adhoc-sign)
      ADHOC_SIGN=1
      shift
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
[ -n "$OUT" ] || {
  echo "缺 --out <目录>" >&2
  exit 2
}
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

cd "$(dirname "$0")/../.."
ROOT="$PWD"
source scripts/github-release/lib.sh
source scripts/build-version.sh
source scripts/codesign.sh

say() { printf '\033[1m[package]\033[0m %s\n' "$*"; }
die() {
  printf '\033[31m[package] %s\033[0m\n' "$*" >&2
  exit 1
}

PLATFORM="$(summon_release_platform "$(uname -s)" "$(uname -m)")" \
  || die "不发布这个平台的包:$(uname -s) $(uname -m)"
FILES=()
while IFS= read -r name; do FILES+=("$name"); done < <(summon_release_files "$PLATFORM")
IS_MAC=0
[ "$(uname -s)" = Darwin ] && IS_MAC=1

for tool in git python3 cargo go; do
  command -v "$tool" > /dev/null 2>&1 || die "缺 $tool"
done

# 对外的包默认要求 Developer ID 签名并带安全时间戳,不静默降级成 ad-hoc。
if [ "$IS_MAC" = 1 ]; then
  command -v codesign > /dev/null 2>&1 || die "缺 codesign,macOS 包必须签名"
  if [ "$ADHOC_SIGN" = 1 ]; then
    SUMMON_CODESIGN_MODE=adhoc
  else
    SUMMON_CODESIGN_MODE=developer-id
    SUMMON_CODESIGN_TIMESTAMP=required
  fi
  export SUMMON_CODESIGN_MODE SUMMON_CODESIGN_TIMESTAMP
  summon_prepare_codesign || die "签名不可用"
  case "$SUMMON_CODESIGN_KIND" in
    developer-id) ;;
    adhoc) [ "$ADHOC_SIGN" = 1 ] || die "没有 Developer ID 签名身份" ;;
    *) die "签名不可用(SUMMON_CODESIGN_KIND=$SUMMON_CODESIGN_KIND)" ;;
  esac
fi

VERSION="$(summon_build_version)" || die "读不出版本号"
HEAD_SHA="$(git rev-parse HEAD)"
HEAD_TIME="$(git log -1 --format=%ct HEAD)"

# 构建戳只记提交,不记工作区:脏树上打出的包和干净的同号同提交、内容却不同。
DIRTY_TREE="$(git status --porcelain)"
if [ -n "$DIRTY_TREE" ]; then
  if [ -z "${SUMMON_ALLOW_DIRTY:-}" ]; then
    printf '%s\n' "$DIRTY_TREE" >&2
    die "工作区有未提交改动,拒绝打包(本地试打设 SUMMON_ALLOW_DIRTY=1,那样的包不可发布)"
  fi
  say "⚠ SUMMON_ALLOW_DIRTY=1:工作区脏,本次产物不可发布"
fi

# target 目录可能被 CARGO_TARGET_DIR 或 .cargo/config.toml 重定向,不能写死 target/。
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps 2> /dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
TARGET_DIR="${TARGET_DIR:-$ROOT/target}"
SWIFT_SCRATCH="${SUMMON_SWIFT_SCRATCH_PATH:-$ROOT/menubar/.build}"
SIDECAR_BIN="$TARGET_DIR/release/summon-sidecar"

# 包是公开分发的:rustc 与 swiftc 默认把源码、依赖源码和构建目录的绝对路径编进二进制(panic 信息、
# #file),会带出构建机的用户目录。这几处前缀各映射成固定的占位路径;RUSTFLAGS 按空白切分参数,
# 路径里不能有空格。
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
case "$CARGO_HOME_DIR$ROOT" in
  *[[:space:]]*) die "CARGO_HOME 或源码路径含空白,无法传给 --remap-path-prefix:$CARGO_HOME_DIR / $ROOT" ;;
esac
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$CARGO_HOME_DIR=/cargo --remap-path-prefix=$ROOT=/summon"

if [ "$SKIP_BUILD" = 0 ]; then
  say "构建 summon $VERSION($PLATFORM)"
  cargo build --release --locked -p summon
  (cd sidecar/feishu-connector && go build -trimpath -o "$SIDECAR_BIN" .)
  if [ "$IS_MAC" = 1 ]; then
    swift build -c release --package-path menubar --scratch-path "$SWIFT_SCRATCH" \
      -Xswiftc -file-prefix-map -Xswiftc "$ROOT=/summon" \
      -Xswiftc -file-prefix-map -Xswiftc "$SWIFT_SCRATCH=/build"
  fi
fi

source_of() {
  case "$1" in
    summon) printf '%s\n' "$TARGET_DIR/release/summon" ;;
    summon-sidecar) printf '%s\n' "$SIDECAR_BIN" ;;
    summon-menubar) printf '%s\n' "$SWIFT_SCRATCH/release/summon-menubar" ;;
  esac
}

STAGE="$OUT/.stage-$PLATFORM"
rm -rf "$STAGE"
mkdir -p "$STAGE"
trap 'rm -rf "$STAGE"' EXIT
for name in "${FILES[@]}"; do
  src="$(source_of "$name")"
  [ -f "$src" ] || die "缺构建产物 $src"
  cp "$src" "$STAGE/$name"
  chmod 0755 "$STAGE/$name"
done

# --skip-build 拿到的可能是没带重映射的旧产物,所以按包里的文件查,不按这次有没有构建判断。
# 要带 -a:macOS 的 grep 不把 Mach-O 当文本,不加就静默查不到。
for name in "${FILES[@]}"; do
  for prefix in "$HOME/" "$CARGO_HOME_DIR/" "$ROOT/"; do
    if LC_ALL=C grep -a -q -F -- "$prefix" "$STAGE/$name"; then
      die "$name 里含构建机路径 $prefix(没走重映射的旧产物?去掉 --skip-build 重新构建)"
    fi
  done
done

if [ "$IS_MAC" = 1 ]; then
  say "签名($SUMMON_CODESIGN_KIND)"
  summon_sign_path "$STAGE/summon" com.summon.cli || die "summon 签名失败"
  summon_sign_path "$STAGE/summon-sidecar" com.summon.sidecar || die "summon-sidecar 签名失败"
  summon_sign_path "$STAGE/summon-menubar" com.summon.menubar || die "summon-menubar 签名失败"
fi

# 构建戳取包里那份二进制自报的,与它运行期比新旧用的是同一份编译期常量。缺提交或提交时间的包
# 发出去,同号的两份就永远判不出新旧,所以缺一项就拒绝打包;提交对不上 HEAD 说明用了旧产物。
STAMP="$("$STAGE/summon" install-stamp print)" || die "summon install-stamp print 失败"
REPORTED_VERSION="$("$STAGE/summon" --version 2> /dev/null | awk '{print $2}')"
[ "$REPORTED_VERSION" = "$VERSION" ] \
  || die "summon --version 报 ${REPORTED_VERSION:-空},应为 $VERSION"
python3 - "$STAMP" "$VERSION" "$HEAD_SHA" "$HEAD_TIME" << 'PY' || {
import json, re, sys
raw, version, head, head_time = sys.argv[1:]
stamp = json.loads(raw)
problems = []
if stamp.get('version') != version:
    problems.append(f'version={stamp.get("version")},应为 {version}')
commit = stamp.get('commit')
if not isinstance(commit, str) or not re.fullmatch(r'[0-9a-f]{7,40}', commit):
    problems.append(f'commit={commit},不是提交号')
elif not head.startswith(commit):
    problems.append(f'commit={commit},不是当前 HEAD {head[:12]}(用了旧的构建产物?)')
commit_time = stamp.get('commit_time')
if type(commit_time) is not int or commit_time <= 0:
    problems.append(f'commit_time={commit_time},缺提交时间')
elif commit_time != int(head_time):
    problems.append(f'commit_time={commit_time},HEAD 的是 {head_time}')
if problems:
    sys.exit('构建戳不对:' + ';'.join(problems))
PY
  # build.rs 拿不到 git 时静默给空值,原因只能在这里补问:常见的是 safe.directory、浅克隆缺对象。
  git log -1 --format='%h %ct' HEAD 2>&1 | sed 's/^/  git: /' >&2 || true
  die "构建戳校验失败"
}

if [ "$IS_MAC" = 0 ]; then
  say "glibc 符号版本检查(上限 $SUMMON_RELEASE_GLIBC_MAX)"
  STAGED=()
  for name in "${FILES[@]}"; do STAGED+=("$STAGE/$name"); done
  summon_check_glibc "$SUMMON_RELEASE_GLIBC_MAX" "${STAGED[@]}" || die "glibc 检查没过"
fi

TARBALL="summon-$PLATFORM.tar.gz"
BUILD_JSON="summon-$PLATFORM.build.json"
rm -f "$OUT/$TARBALL" "$OUT/$BUILD_JSON"
say "打包 $TARBALL"
# 用 Python 打包:GNU 与 BSD tar 的参数不同,macOS 的 tar 还会夹带 ._* 扩展属性文件。
# 属主、时间统一,同一批二进制打出的包逐字节相同。
python3 - "$STAGE" "$OUT/$TARBALL" "$OUT/$BUILD_JSON" "$PLATFORM" "$STAMP" "${FILES[@]}" << 'PY'
import gzip, hashlib, io, json, os, sys, tarfile
stage, tarball, build_json, platform, raw_stamp, *files = sys.argv[1:]
stamp = json.loads(raw_stamp)
buf = io.BytesIO()
with tarfile.open(fileobj=buf, mode='w', format=tarfile.PAX_FORMAT) as tar:
    for name in files:
        path = os.path.join(stage, name)
        info = tarfile.TarInfo(name)
        info.size = os.path.getsize(path)
        info.mode = 0o755
        info.mtime = stamp['commit_time']
        info.uid = info.gid = 0
        info.uname = info.gname = ''
        with open(path, 'rb') as f:
            tar.addfile(info, f)
with open(tarball, 'wb') as raw:
    with gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=0) as gz:
        gz.write(buf.getvalue())
# 读回核对:文件集合与权限。
with tarfile.open(tarball, 'r:gz') as tar:
    members = tar.getmembers()
    names = [m.name for m in members]
    if sorted(names) != sorted(files):
        sys.exit(f'包里的文件不对:{names}')
    for m in members:
        if not m.isfile() or m.mode != 0o755:
            sys.exit(f'{m.name} 不是 0755 的普通文件')
data = open(tarball, 'rb').read()
record = {
    'platform': platform,
    'version': stamp['version'],
    'commit': stamp['commit'],
    'commit_time': stamp['commit_time'],
    'name': os.path.basename(tarball),
    'size': len(data),
    'sha256': hashlib.sha256(data).hexdigest(),
}
with open(build_json, 'w', encoding='utf-8') as f:
    json.dump(record, f, indent=2)
    f.write('\n')
PY

say "完成:$OUT/$TARBALL"
say "      $OUT/$BUILD_JSON"
cat "$OUT/$BUILD_JSON"
