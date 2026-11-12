#!/usr/bin/env bash
# 一键部署 + 优雅重启 summon。
#
# 契约(与 daemon 内部的事件 outbox 配合 —— 脚本本身不碰事件,只触发优雅停机+启动):
#   1. 构建前保留旧控制 CLI,新 CLI/sidecar 只写暂存文件；**构建失败立即中止、绝不重启**。
#   2. 用旧控制 CLI 执行 `daemon stop`:SIGTERM → daemon 的 shutdown 钩子对在跑的无头子进程
#      发 SIGINT 打断,然后原子发布新产物。跨版本升级不得先覆盖旧 CLI,否则新协议可能无法停旧 daemon。
#      **打断前把「待发事件」(如「会话被重启打断」群通知)落盘到本地 outbox**(写盘快、不等网络),
#      2s 宽限后 SIGKILL 兜底。**绝不杀 tmux 会话(claude 持久层)**。
#   3. `daemon start` 启动时有个子任务**消费 outbox**:读待办事件→批量派发(发群消息/续跑)→ack→归档;
#      未能 ack 的事件保留 + 退避重试(防雪崩),不阻塞启动。
# 所以「无头 agent 被中断前在群里发消息」靠的是 outbox 的「写盘→重启后消费」,不是同步网络发送。
#
# 用法: scripts/deploy.sh [--debug|--release] [--no-sidecar] [--menubar|--no-menubar] [--skip-build] [--install]
#   --release    release 优化构建(默认,保留此参数只为兼容旧肌肉记忆)
#   --debug      debug 构建(opt-in;产物带 `summon dev` 调试命令,仅用于本地联调)
#   --no-sidecar 跳过 sidecar 构建
#   --menubar    首次安装 macOS 原生 Menu Bar；已安装时无需指定，deploy 会自动更新
#   --no-menubar 即使已经安装，也跳过 macOS 原生 Menu Bar 构建、发布与启动
#   --skip-build 跳过 cargo build(用已有产物)
#   --install    构建后安装到 ~/.summon/bin/summon(~/.local/bin/summon 建软链指过去),并登记构建戳;
#                装的比 ~/.summon/bin 里那份旧时提示,但照装(显式部署可以是回退)
# macOS 签名环境变量:
#   SUMMON_CODESIGN_MODE=auto|developer-id|adhoc（release 默认 auto；debug 默认 adhoc）
#   SUMMON_CODESIGN_IDENTITY=<证书 SHA-1 或完整名称>（多个 Developer ID 时必须显式指定）
#   SUMMON_CODESIGN_TIMESTAMP=required|best-effort|none（本地部署默认 best-effort）
if [ -z "${BASH_VERSION:-}" ]; then
  if command -v bash >/dev/null 2>&1; then
    exec bash "$0" "$@"
  fi
  echo "安装需要 bash，请先安装 bash 后重试" >&2
  exit 1
fi
set -euo pipefail

# 旧版用这个环境变量标记「已经脱离」。它会被新 daemon 继承,再污染之后的 agent 子进程,
# 导致下一次部署误判为已脱离、停机时把自己一起杀掉。新版改用不可继承的内部 argv;
# 这里同时清掉旧 daemon 可能留下的污染,确保本轮启动的 daemon 也是干净的。
unset SUMMON_DEPLOY_DETACHED

cd "$(dirname "$0")/.."          # 仓库根
source scripts/build-version.sh
source scripts/codesign.sh
# 安装渠道钩子:`scripts/deploy.d/*.sh` 按文件名顺序 source,可以给 daemon start 补环境
# (DAEMON_START_ENV,若干个 KEY=VALUE)、换掉「本次比落点旧」的提示(OLDER_INSTALL_NOTE)。
# 没有钩子时两项保持默认,源码安装就是这样。
DAEMON_START_ENV=""
OLDER_INSTALL_NOTE="⚠ 照装。"
for hook in "${SUMMON_DEPLOY_HOOKS_DIR:-scripts/deploy.d}"/*.sh; do
  [ -f "$hook" ] || continue
  # shellcheck source=/dev/null
  source "$hook"
done
# 生产默认 release:daemon 是常驻进程,debug 构建的解析/序列化热点比 release 慢一个量级
# (实测同一份 234,888 B state.json:serde 解析 debug 4.96ms / release 0.67ms = 7.4×),
# 且 debug 产物会带上 `summon dev` 调试命令面。要装 debug 显式加 --debug。
PROFILE_DIR="release"
CARGO_FLAGS=("--release")
DO_SIDECAR=1
DO_BUILD=1
DO_INSTALL=0
MENUBAR_FORCE_INSTALL=0
MENUBAR_FORCE_SKIP=0
DETACHED_WORKER=0
for arg in "$@"; do
  case "$arg" in
    # 只由本脚本的 setsid 重进传入。daemon 会继承环境,但不会继承启动它的 argv。
    --_summon-detached-worker) DETACHED_WORKER=1 ;;
    # --release 已是默认,保留为幂等参数;与 --debug 同时出现时按「最后一个生效」。
    --release)     CARGO_FLAGS=("--release"); PROFILE_DIR="release" ;;
    --debug)       CARGO_FLAGS=(); PROFILE_DIR="debug" ;;
    --no-sidecar)  DO_SIDECAR=0 ;;
    --menubar)     MENUBAR_FORCE_INSTALL=1 ;;
    --no-menubar)  MENUBAR_FORCE_SKIP=1 ;;
    --skip-build)  DO_BUILD=0 ;;
    --install)     DO_INSTALL=1 ;;
    *) echo "未知参数: $arg" >&2; exit 2 ;;
  esac
done
if [ "$MENUBAR_FORCE_INSTALL" = 1 ] && [ "$MENUBAR_FORCE_SKIP" = 1 ]; then
  echo "参数冲突:--menubar 与 --no-menubar 不能同时使用" >&2
  exit 2
fi
if [ "$MENUBAR_FORCE_INSTALL" = 1 ] && [ "$DO_INSTALL" = 0 ]; then
  echo "--menubar 需要与 --install 一起使用" >&2
  exit 2
fi
# ── 0. 在 summon 管理的会话里跑时,先把部署本体脱出进程组 ───────────────────────
# **自指问题**:第 2 步 `daemon stop` 会触发 daemon 的 shutdown 钩子,对每个在跑的无头轮
# `killpg(-pgid, SIGINT)`。而 agent 执行本脚本时,脚本就在那一轮的进程组里 —— 停机把发起
# 停机的脚本自己打死,后面「启动新 daemon」永远跑不到,机器上只剩没有 daemon 的裸机。
#
# 2026-08-10 devbox 实测三次:20:24 / 21:52 侥幸活着(打断时已跑到 start 附近),22:04 那次
# codex app-server 软打断没在宽限内收敛、升级 SIGINT 整组 → deploy 陪葬 → **无 daemon 裸奔
# 17 分钟**,靠人工发现才救回。是竞态不是必现,所以不修一定复发。
#
# 做法:部署本体 setsid 进新进程组(killpg 按 pgid 打,打不到),原进程降级为**观察者**,只
# tail 日志。观察者被杀不影响部署跑完;正常情况下输出体验与同步执行无异。
#
# **无条件脱离,不看环境变量判据。** 第一版按 `SUMMON_MANAGED_SESSION` 判断"是否在托管会话
# 里",当天 23:53 就被打脸:codex 的 `command_execution` 子进程**不继承** summon 注入的环境
# 变量(remote-codex 自己得在命令行里手写 `SUMMON_AGENT=… SUMMON_CHAT_ID=…` 才能用),判据恒
# 为假 → 整段脱离被跳过 → 照样裸奔。我当时只在自己的 claude/tmux 环境里验了判据,而事故发生
# 在 codex 的无头环境 —— **判据必须在出事的那个环境里验,不是在手边的环境里验**。
# 判据本身就是熵:少一个条件就少一处会错的地方。ssh/终端直跑也脱离,但观察者 tail 跟随 +
# 透传退出码,输出体验无差别。要强制同步(调试)设 SUMMON_DEPLOY_NO_DETACH=1。
#
# 重进标记也不能用环境变量:2026-08-11 真实发生 `SUMMON_DEPLOY_DETACHED=1` 被新
# daemon 继承,下一次 deploy 全程跳过 setsid,停机后部署脚本自杀,机器用旧二进制空窗
# 18 分钟才被外部拉起。内部 `--_summon-detached-worker` 只活在子脚本 argv 里,不会传给 daemon。
SUMMON_DEPLOY_LOG="$HOME/.summon/run/deploy.log"
summon_detach_tool() {
  # setsid 是 Linux 常备、macOS 没有;perl 两边都自带。两条路互补,覆盖双机。
  if command -v setsid >/dev/null 2>&1; then
    printf 'setsid\n'
  elif command -v perl >/dev/null 2>&1; then
    printf 'perl\n'
  fi
}
if [ "$DETACHED_WORKER" = 0 ] && [ -z "${SUMMON_DEPLOY_NO_DETACH:-}" ]; then
  DETACH_TOOL="$(summon_detach_tool)"
  if [ -z "$DETACH_TOOL" ]; then
    # 没有脱离手段时不阻断部署:退回同步执行 = 现状,不比不改更差,但要让人看见风险。
    echo "[deploy] 警告:未找到 setsid/perl,无法脱离进程组;停机可能打断本脚本自身" >&2
  else
    SCRIPT_ABS="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
    mkdir -p "$(dirname "$SUMMON_DEPLOY_LOG")"
    : > "$SUMMON_DEPLOY_LOG"
    echo "[deploy] 部署本体已脱离进程组执行(停机 killpg 打不到它);本进程只跟随日志"
    echo "[deploy] 本轮若被打断,部署仍会跑完;完整日志:$SUMMON_DEPLOY_LOG"
    # 用**当前这个**解释器重新执行,不硬编码 sh:本脚本用了 bash 数组(`sign_args=(…)`),
    # 调用方若是 `bash scripts/deploy.sh`(remote-codex 就是),换成 dash 重跑会当场语法错。
    if [ -n "${BASH_VERSION:-}" ]; then
      DEPLOY_SHELL=bash
    else
      DEPLOY_SHELL=sh
    fi
    if [ "$DETACH_TOOL" = setsid ]; then
      setsid "$DEPLOY_SHELL" "$SCRIPT_ABS" --_summon-detached-worker "$@" >"$SUMMON_DEPLOY_LOG" 2>&1 &
    else
      perl -MPOSIX -e 'POSIX::setsid() or die "setsid 失败: $!\n"; exec @ARGV' \
        -- "$DEPLOY_SHELL" "$SCRIPT_ABS" --_summon-detached-worker "$@" >"$SUMMON_DEPLOY_LOG" 2>&1 &
    fi
    DEPLOY_WORKER=$!
    tail -f "$SUMMON_DEPLOY_LOG" &
    DEPLOY_TAILER=$!
    DEPLOY_RC=0
    wait "$DEPLOY_WORKER" || DEPLOY_RC=$?
    sleep 0.3   # 让 tail 把最后几行吐完再收,否则结尾的「部署完成」会丢
    kill "$DEPLOY_TAILER" 2>/dev/null || true
    exit "$DEPLOY_RC"
  fi
fi

HOST_OS="$(uname -s)"
MENUBAR_PLIST="$HOME/Library/LaunchAgents/com.summon.menubar.plist"
MENUBAR_WAS_INSTALLED=0
[ -f "$MENUBAR_PLIST" ] && MENUBAR_WAS_INSTALLED=1
DO_MENUBAR=0
if [ "$HOST_OS" = "Darwin" ]; then
  if [ "$MENUBAR_FORCE_SKIP" = 0 ] \
    && { [ "$MENUBAR_FORCE_INSTALL" = 1 ] || [ "$MENUBAR_WAS_INSTALLED" = 1 ]; }; then
    DO_MENUBAR=1
  fi
elif [ "$MENUBAR_FORCE_INSTALL" = 1 ]; then
  echo "--menubar 仅支持 macOS" >&2
  exit 2
fi
# target 目录可能被本机 .cargo/config.toml 重定向,不能写死 ./target;
# 用 cargo metadata 解析,拿不到(如 --skip-build 且无 cargo)退回默认。
TARGET_DIR="target"
if command -v cargo >/dev/null 2>&1; then
  RESOLVED_TARGET="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
  [ -z "$RESOLVED_TARGET" ] || TARGET_DIR="$RESOLVED_TARGET"
fi
BIN="$TARGET_DIR/${PROFILE_DIR}/summon"
# 三个产物的**真身统一放 ~/.summon/bin**;~/.local/bin 只留一个指过去的软链。
#
# why(owner 2026-08-10):此前 CLI 装 ~/.local/bin、menubar 与 sidecar 装 ~/.summon/bin,
# 同一套东西散在两处,排查时得先想"这个组件归哪边"。统一后 summon 自己的目录就是唯一事实源,
# 而 ~/.local/bin 那个软链只承担一件事:让它落在用户 PATH 里。
#
# 软链方向是「local → summon」而不是反过来:~/.summon 是本工具自有的目录(卸载即整体删除),
# ~/.local/bin 是用户共享目录,不该由 summon 持有实体。两平台同构,Linux 无需分支。
BIN_DIR="$HOME/.summon/bin"
INSTALL_BIN="$BIN_DIR/summon"
LINK_DIR="$HOME/.local/bin"
LINK_BIN="$LINK_DIR/summon"
SIDECAR_OUT="$BIN_DIR/summon-sidecar"
MENUBAR_OUT="$BIN_DIR/summon-menubar"
RUN_DIR="$HOME/.summon/run"
STAGED_CLI=""
STAGED_SIDECAR=""
STAGED_MENUBAR=""
OLD_CONTROL_EXACT=""
OLD_CONTROL_INSTALLED=""
OLD_CONTROL_TARGET=""
SNAPSHOT_RESULT=""
SIDECAR_BACKUP=""
MENUBAR_BACKUP=""
CLI_BACKUP=""
SIDECAR_PUBLISHED=0
MENUBAR_PUBLISHED=0
CLI_PUBLISHED=0
PUBLISH_COMPLETE=0
DAEMON_RESTORE_PENDING=0
MENUBAR_READY_ATTEMPTS=25

log() { printf '\033[36m[deploy]\033[0m %s\n' "$*"; }
die() { printf '\033[31m[deploy] 中止: %s\033[0m\n' "$*" >&2; exit 1; }

# release 默认自动使用唯一的 Developer ID；debug 保持便宜的 ad-hoc，除非调用方显式要求。
if [ "$PROFILE_DIR" = "debug" ] && [ -z "${SUMMON_CODESIGN_MODE+x}" ]; then
  SUMMON_CODESIGN_MODE="adhoc"
fi
SUMMON_CODESIGN_TIMESTAMP="${SUMMON_CODESIGN_TIMESTAMP:-best-effort}"
summon_prepare_codesign || die "macOS 代码签名身份选择失败"

if [ "$DO_BUILD" = 1 ] && ! command -v cargo >/dev/null 2>&1; then
  die "未找到 cargo；请先安装 Rust stable 并确保 cargo 在 PATH"
fi
if [ "$DO_SIDECAR" = 1 ] && ! command -v go >/dev/null 2>&1; then
  die "未找到 go；飞书 sidecar 需要 Go 1.24.4+ 并且 go 在 PATH"
fi

wait_for_menubar() {
  local attempt
  MENUBAR_STATUS=""
  # bootstrap/kickstart 返回时进程未必已进入 running；最多等待约 5 秒，避免启动竞态误报。
  for attempt in $(seq 1 "$MENUBAR_READY_ATTEMPTS"); do
    MENUBAR_STATUS="$("$BIN" menubar status 2>/dev/null || true)"
    case "$MENUBAR_STATUS" in
      *"binary:存在"*"plist:已安装"*"launchd:运行中"*) return 0 ;;
    esac
    [ "$attempt" -ge "$MENUBAR_READY_ATTEMPTS" ] || sleep 0.2
  done
  return 1
}

cleanup_staged() {
  # 两份产物未全部发布时恢复原集合；不能留下新 sidecar + 旧 CLI（或反向）的混代状态。
  if [ "$PUBLISH_COMPLETE" = 0 ]; then
    if [ -n "$CLI_BACKUP" ] && [ -e "$CLI_BACKUP" ]; then
      mv -f "$CLI_BACKUP" "$INSTALL_BIN" 2>/dev/null || true
      CLI_BACKUP=""
    elif [ "$CLI_PUBLISHED" = 1 ]; then
      rm -f "$INSTALL_BIN"
    fi
    if [ -n "$SIDECAR_BACKUP" ] && [ -e "$SIDECAR_BACKUP" ]; then
      mv -f "$SIDECAR_BACKUP" "$SIDECAR_OUT" 2>/dev/null || true
      SIDECAR_BACKUP=""
    elif [ "$SIDECAR_PUBLISHED" = 1 ]; then
      rm -f "$SIDECAR_OUT"
    fi
    if [ -n "$MENUBAR_BACKUP" ] && [ -e "$MENUBAR_BACKUP" ]; then
      mv -f "$MENUBAR_BACKUP" "$MENUBAR_OUT" 2>/dev/null || true
      MENUBAR_BACKUP=""
    elif [ "$MENUBAR_PUBLISHED" = 1 ]; then
      rm -f "$MENUBAR_OUT"
    fi
  fi
  [ -z "$STAGED_CLI" ] || rm -f "$STAGED_CLI"
  [ -z "$STAGED_SIDECAR" ] || rm -f "$STAGED_SIDECAR"
  [ -z "$STAGED_MENUBAR" ] || rm -f "$STAGED_MENUBAR"
  [ -z "$OLD_CONTROL_EXACT" ] || rm -f "$OLD_CONTROL_EXACT"
  [ -z "$OLD_CONTROL_INSTALLED" ] || rm -f "$OLD_CONTROL_INSTALLED"
  [ -z "$OLD_CONTROL_TARGET" ] || rm -f "$OLD_CONTROL_TARGET"
  [ -z "$SNAPSHOT_RESULT" ] || rm -f "$SNAPSHOT_RESULT"
  [ -z "$SIDECAR_BACKUP" ] || rm -f "$SIDECAR_BACKUP"
  [ -z "$MENUBAR_BACKUP" ] || rm -f "$MENUBAR_BACKUP"
  [ -z "$CLI_BACKUP" ] || rm -f "$CLI_BACKUP"
}

# 停机不可撤销:`daemon stop` 一旦发出 SIGTERM,那个 daemon 就一定会退——CLI 等不到它退而
# 报错返回,不代表停机没发生。所以停机之后的每一条退出路径都得把 daemon 起回来,否则机器
# 停在没有 daemon 的裸机状态:tmux 里的 claude 会话还活着尚能人工补救,codex/cursor 那种
# 随 daemon 一起死的会话谁也救不回来。
restore_daemon() {
  [ "$DAEMON_RESTORE_PENDING" = 1 ] || return 0
  if [ ! -x "$INSTALL_BIN" ]; then
    printf '\033[31m[deploy] 停机后无法拉回 daemon:%s 不可执行,机器当前没有 daemon\033[0m\n' \
      "$INSTALL_BIN" >&2
    return 0
  fi
  # 旧 daemon 还活着(如停机被拒)就不该再起一个;真正裸奔时才动手。
  case "$("$INSTALL_BIN" daemon status 2>/dev/null || true)" in
    running*) return 0 ;;
  esac
  log "部署中断在停机之后,拉回 daemon 以免裸奔…"
  # 用安装态那份而不是 target 产物:cleanup_staged 刚把线上产物恢复成自洽的一代,
  # 起回来的 daemon 必须与机器上的 CLI 同代。daemon start 自带单例保护,不会起出双实例。
  # 与第 3 步一样带上渠道钩子给的环境。
  # shellcheck disable=SC2086 # 钩子给的是若干个 KEY=VALUE,按空白拆开交给 env
  env $DAEMON_START_ENV "$INSTALL_BIN" daemon start \
    || printf '\033[31m[deploy] 停机后拉回 daemon 失败;机器当前没有 daemon,需手工处理\033[0m\n' >&2
}

on_exit() {
  cleanup_staged
  restore_daemon
}
trap on_exit EXIT

# cargo build 也可能替换 target 下的旧 CLI。若 daemon 正在运行，构建前先保存它所属代际的
# 控制程序；临时副本只用于 status/stop，不参与新 daemon 启动。
snapshot_controller() {
  local source="$1"
  SNAPSHOT_RESULT=""
  [ -x "$source" ] || return 0
  SNAPSHOT_RESULT="$(mktemp "${TMPDIR:-/tmp}/summon-control.XXXXXX")"
  cp "$source" "$SNAPSHOT_RESULT" || die "无法暂存旧控制 CLI:$source"
  chmod +x "$SNAPSHOT_RESULT"
  if command -v codesign >/dev/null 2>&1; then
    xattr -c "$SNAPSHOT_RESULT" 2>/dev/null || true
    codesign --force --deep --identifier summon -s - "$SNAPSHOT_RESULT" 2>/dev/null || true
  fi
}

# `KeepAlive=true`(连正常退出也拉起)会在 stop 后立即复活旧代，与“停机后发布”的事务边界冲突。
# 先 fail-closed，避免脚本把“新 CLI 发现已有旧 daemon”误报为部署成功；用户关闭后可立即重跑，
# 不改动任何线上产物。默认单元(launchd `KeepAlive.SuccessfulExit=false` / systemd `Restart=on-failure`)
# 只在异常退出时拉起，`daemon stop` 的优雅退出码是 0，不受此闸约束；开了自启时第 3 步的
# `daemon start` 经自启单元起新 daemon。
AUTOSTART_STATE="$RUN_DIR/autostart.json"
AUTOSTART_PLIST="$HOME/Library/LaunchAgents/com.summon.daemon.plist"
KEEP_ALIVE_ENABLED=0
if grep -Eq '"keep_alive"[[:space:]]*:[[:space:]]*true' "$AUTOSTART_STATE" 2>/dev/null; then
  KEEP_ALIVE_ENABLED=1
elif [ -f "$AUTOSTART_PLIST" ]; then
  if command -v plutil >/dev/null 2>&1 \
    && [ "$(plutil -extract KeepAlive raw -o - "$AUTOSTART_PLIST" 2>/dev/null || true)" = "true" ]; then
    KEEP_ALIVE_ENABLED=1
  elif awk '
    /<key>KeepAlive<\/key>/ { after_key = 1; next }
    after_key && /<true\/>/ { found = 1 }
    after_key { after_key = 0 }
    END { exit !found }
  ' "$AUTOSTART_PLIST"; then
    KEEP_ALIVE_ENABLED=1
  fi
fi
# Linux(systemd --user)侧同一道闸:会在正常退出后也拉起的 `Restart=`(always / on-success)与
# launchd 的 `KeepAlive=true` 是同一个隐患 —— stop 之后被立刻复活,发布就没有事务边界了。
# 状态文件那条判据两平台通用(上面已查),这里补的是「有人绕过 summon 手改了单元文件」的情形,
# 与 macOS 读 plist 的兜底对称。
SYSTEMD_UNIT="$HOME/.config/systemd/user/summon-daemon.service"
if [ "$KEEP_ALIVE_ENABLED" = 0 ] && [ -f "$SYSTEMD_UNIT" ] \
  && grep -Eq '^[[:space:]]*Restart[[:space:]]*=[[:space:]]*(always|on-success)[[:space:]]*$' "$SYSTEMD_UNIT"; then
  KEEP_ALIVE_ENABLED=1
fi
if [ "$KEEP_ALIVE_ENABLED" = 1 ]; then
  die "检测到 autostart 自动重启(launchd KeepAlive / systemd Restart=)；先执行 summon autostart disable，再重新部署"
fi

OLD_PID="$(sed -n '1p' "$RUN_DIR/daemon.pid" 2>/dev/null || true)"
case "$OLD_PID" in
  ''|*[!0-9]*) OLD_PID="" ;;
esac
if [ -n "$OLD_PID" ]; then
  OLD_COMMAND="$(ps -p "$OLD_PID" -o command= 2>/dev/null || true)"
  case "$OLD_COMMAND" in
    summond\ daemon\ run*|summon\ daemon\ run*|*/summon\ daemon\ run*)
      DAEMON_EXE=""
      if [ -e "/proc/$OLD_PID/exe" ]; then
        DAEMON_EXE="$(readlink "/proc/$OLD_PID/exe" 2>/dev/null || true)"
      elif command -v lsof >/dev/null 2>&1; then
        DAEMON_EXE="$(lsof -a -p "$OLD_PID" -d txt -Fn 2>/dev/null \
          | sed -n 's/^n//p' | head -n 1 || true)"
      fi
      snapshot_controller "$DAEMON_EXE"
      OLD_CONTROL_EXACT="$SNAPSHOT_RESULT"
      snapshot_controller "$INSTALL_BIN"
      OLD_CONTROL_INSTALLED="$SNAPSHOT_RESULT"
      snapshot_controller "$BIN"
      OLD_CONTROL_TARGET="$SNAPSHOT_RESULT"
      OLD_DAEMON_VERIFIED=1
      ;;
  esac
fi
OLD_DAEMON_VERIFIED="${OLD_DAEMON_VERIFIED:-0}"

# ── 1. 构建(fail-fast:失败则旧 daemon 不动) ──────────────────────────────
if [ "$DO_BUILD" = 1 ]; then
  BUILD_VERSION="$(summon_build_version)" || die "无法读取 summon 版本"
  log "cargo build ${CARGO_FLAGS[*]:-} (version $BUILD_VERSION)…(失败将中止,不重启)"
  # 版本不再靠环境变量注入:二进制自己从 CARGO_PKG_VERSION 取,和这里读到的是同一个源。
  # 空数组展开:用 `${arr[@]+"${arr[@]}"}` 而非裸 `"${arr[@]}"`——后者在 macOS `/bin/sh`(bash 3.2)
  # 的 `set -u` 下会把空数组当未绑定变量报错(`sh scripts/deploy.sh` 即触发);此写法 3.2~5.x 通用。
  cargo build ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} \
    || die "Rust 构建失败,保留运行中的旧 daemon"
else
  log "跳过 Rust 构建(--skip-build)"
fi
[ -x "$BIN" ] || die "未找到可执行 $BIN(先构建)"
# 构建后立即对产物清扩展属性 + 固定 identifier 签名:
#   - 清 xattr:去掉 cargo 产出的 com.apple.provenance,避免 Gatekeeper 在新路径 SIGKILL(exit 137)
#   - release 有唯一 Developer ID 时使用稳定 Team ID + identifier；无证书才兼容回退 ad-hoc。
# 签在构建产物上,后续 cp/mv/暂存/安装都自带签名,无需每步重复签。
summon_sign_path "$BIN" "com.summon.cli" \
  || die "summon CLI 签名或签名验证失败"
BUILT_VERSION="$("$BIN" --version 2>/dev/null | awk '{print $2}')"
if [ "$DO_BUILD" = 1 ]; then
  [ "$BUILT_VERSION" = "$BUILD_VERSION" ] \
    || die "构建产物版本不匹配(want $BUILD_VERSION, got ${BUILT_VERSION:-empty})"
elif ! summon_validate_build_version "$BUILT_VERSION"; then
  die "--skip-build 的存量产物给不出合法 semver 版本(got ${BUILT_VERSION:-empty});请先正常构建"
fi

# sidecar 和待安装 CLI 都写到目标目录内的暂存文件。停机成功后用同文件系统 mv 原子发布，
# 构建失败或旧 daemon 停机失败时，线上产物保持不变。
if [ "$DO_SIDECAR" = 1 ]; then
  mkdir -p "$(dirname "$SIDECAR_OUT")"
  STAGED_SIDECAR="$(mktemp "$(dirname "$SIDECAR_OUT")/.summon-sidecar.new.XXXXXX")"
  log "go build sidecar → 暂存文件"
  ( cd sidecar/feishu-connector && go build -o "$STAGED_SIDECAR" . ) \
    || die "sidecar go build 失败(群事件需要它;保留旧 daemon)"
  chmod +x "$STAGED_SIDECAR"
  summon_sign_path "$STAGED_SIDECAR" "com.summon.sidecar" \
    || die "sidecar 签名或签名验证失败"
else
  log "跳过 sidecar(--no-sidecar)"
fi

# 原生 Menu Bar 只随全局安装发布，并保持部署前的安装拓扑；开发态 cargo build 不应修改
# 登录项。Swift 构建也在停旧 daemon 之前完成，任何编译失败都不会影响当前运行版本。
if [ "$DO_INSTALL" = 1 ] && [ "$DO_MENUBAR" = 0 ] \
  && [ "$HOST_OS" = "Darwin" ] && [ "$MENUBAR_FORCE_SKIP" = 0 ]; then
  log "Menu Bar 未安装,保持未安装(首次安装加 --menubar)"
fi
if [ "$DO_INSTALL" = 1 ] && [ "$DO_MENUBAR" = 1 ]; then
  command -v swift >/dev/null 2>&1 || die "未找到 Swift toolchain，无法构建 macOS Menu Bar"
  log "swift build --release → summon 原生 Menu Bar"
  # 失败先清一次构建缓存再重试。`.build` 里的 clang module cache 会被 SDK / 工具链更新悄悄作废,
  # 而 SwiftPM 的增量判据只按 swift 版本指纹走(失败与成功两次构建写的是同一个
  # `swift-version-<hash>.txt`),察觉不到这件事 —— 2026-09-09 真机:源码一个字没动,
  # `FileManager.default` 报「type 'FileManager' has no member 'default'」,`rm -rf .build` 后
  # 立刻通过;同一段代码用 swiftc 单独编译(含 `-swift-version 6`、各档部署目标)从不复现。
  #
  # 不去做「指纹里再加 SDK 版本」那套:那要枚举哪些东西算指纹,漏一个就还会撞;而重试的代价
  # 是有界的(一次全量重建),且只在**已经失败**的路径上付。真的代码错误在第二次照样中止。
  if ! swift build -c release --package-path menubar; then
    log "Menu Bar 构建失败,清掉 menubar/.build 重试一次"
    rm -rf menubar/.build
    swift build -c release --package-path menubar \
      || die "Menu Bar Swift 构建失败(已清缓存重试一次),保留运行中的旧 daemon"
  fi
  MENUBAR_BUILT="menubar/.build/release/summon-menubar"
  [ -x "$MENUBAR_BUILT" ] || die "未找到 Menu Bar 构建产物:$MENUBAR_BUILT"
  mkdir -p "$(dirname "$MENUBAR_OUT")"
  STAGED_MENUBAR="$(mktemp "$(dirname "$MENUBAR_OUT")/.summon-menubar.new.XXXXXX")"
  cp "$MENUBAR_BUILT" "$STAGED_MENUBAR"
  chmod +x "$STAGED_MENUBAR"
  # 暂存文件带随机后缀；显式 identifier 保证 mv 到最终路径后身份不漂移。
  summon_sign_path "$STAGED_MENUBAR" "com.summon.menubar" \
    || die "Menu Bar 签名或签名验证失败"
fi

# ── 1b. 暂存待安装 CLI(可选；此时绝不覆盖旧控制程序) ──────────────────────
if [ "$DO_INSTALL" = 1 ]; then
  mkdir -p "$BIN_DIR"
  STAGED_CLI="$(mktemp "$BIN_DIR/.summon.new.XXXXXX")"
  log "暂存 summon → $STAGED_CLI"
  # 构建产物已在 cargo build 后签过名,cp 保留 Mach-O 内嵌签名,
  # 暂存/移动后无需重签。
  cp "$BIN" "$STAGED_CLI"
  chmod +x "$STAGED_CLI"
  "$STAGED_CLI" --version >/dev/null \
    || die "暂存 CLI 无法执行；保留旧 daemon 与旧产物"
  # 与落点里那份比新旧(判据见 summon_core::install_stamp,daemon start 用的是同一份)。
  # 旧了只提示不拒绝:这是 owner 的显式安装,回退到旧构建是正当用途;而同号的两份按提交时间
  # 排先后,并行分支上只是近似,拿它挡部署会误伤。只读,在停机之前做,不影响任何线上产物。
  INSTALL_RELATION="$("$STAGED_CLI" install-stamp check 2>/dev/null || true)"
  case "$INSTALL_RELATION" in
    older\ *)
      log "⚠ 本次构建比 ~/.summon/bin 里那份旧:${INSTALL_RELATION#older }"
      log "$OLDER_INSTALL_NOTE"
      ;;
    unknown\ *)
      log "提示:判不出本次构建与 ~/.summon/bin 里那份谁新(${INSTALL_RELATION#unknown }),照装"
      ;;
    newer\ * | same\ * | absent\ *) ;;
    *) log "提示:本次构建比不了新旧(早于构建戳的版本?),照装;落点不会登记构建戳" ;;
  esac
fi

# ── 2. 优雅停机(触发 shutdown 钩子:打断无头+写 outbox+2s 宽限;不碰 tmux) ──
STOP_BIN=""
WAS_RUNNING=0
probe_controller() {
  local candidate="$1"
  local status_output
  [ -x "$candidate" ] || return 1
  status_output="$("$candidate" daemon status 2>/dev/null || true)"
  if [ -n "$OLD_PID" ]; then
    case "$status_output" in
      "running(pid $OLD_PID)"|"running(pid $OLD_PID, version "*)
        STOP_BIN="$candidate"
        WAS_RUNNING=1
        return 0
        ;;
    esac
  fi
  return 1
}

# 精确进程镜像优先，其次是构建前保存的全局/target CLI；都不认识当前代际时再让新 CLI
# 以 fail-closed 规则处理陈旧 pidfile/lock。精确镜像来自已验证的活 `summond daemon run`，
# 不依赖旧版本的人类可读 status 文案；fallback status 始终返回 0，不能用退出码判断运行态。
if [ "$OLD_DAEMON_VERIFIED" = 1 ] && [ -x "$OLD_CONTROL_EXACT" ]; then
  STOP_BIN="$OLD_CONTROL_EXACT"
  WAS_RUNNING=1
else
  probe_controller "$OLD_CONTROL_INSTALLED" \
    || probe_controller "$OLD_CONTROL_TARGET" \
    || probe_controller "$BIN" \
    || STOP_BIN="$BIN"
fi

if [ "$WAS_RUNNING" = 1 ]; then
  log "优雅停机运行中的 daemon(pid ${OLD_PID:-?})…"
  # SIGTERM 从下一行起就可能已经送达。窗口在新 daemon 就绪时关闭,期间任何退出都要拉回。
  DAEMON_RESTORE_PENDING=1
else
  log "确认 daemon 已停并清理陈旧身份…"
fi
STOP_ERROR=""
"$STOP_BIN" daemon stop || STOP_ERROR="daemon stop 返回失败(SIGTERM 可能已送达)"

if [ "$WAS_RUNNING" = 1 ]; then
  # 能不能继续,判据是「那个 pid 还在不在」这个客观事实,不是 stop 的退出码:stop 超时报错
  # 只说明它没在自己的 10s 里等到,而 SIGTERM 已经发出去了,那一代终将退出。等待上界放宽到
  # 30s 给收尾慢的一代留余量,仍然有界;pid 到点还在才是真的停不下来。
  for _ in $(seq 1 150); do
    [ -n "${OLD_PID:-}" ] && kill -0 "$OLD_PID" 2>/dev/null || break
    sleep 0.2
  done
  if [ -n "${OLD_PID:-}" ] && kill -0 "$OLD_PID" 2>/dev/null; then
    die "旧 daemon(pid $OLD_PID)未退出,拒绝 start(避免双实例)${STOP_ERROR:+;$STOP_ERROR}"
  fi
  [ -z "$STOP_ERROR" ] || log "$STOP_ERROR,但旧 daemon 已退出,继续部署"
  log "旧 daemon 已退出"
else
  [ -z "$STOP_ERROR" ] || log "$STOP_ERROR;改由新代身份协议复验单例锁"
  log "无运行中的 daemon(跳过 stop)"
fi

# 发布前再用新代身份协议确认锁确实空闲。旧 KeepAlive/外部监督器若已抢先复活，会在覆盖产物前
# fail-closed；正常无进程时此命令只清理陈旧 pidfile/ready。
"$BIN" daemon stop >/dev/null \
  || die "停机后 daemon 单例锁再次被占；保留旧产物，请先关闭外部监督器"

# ── 2b. 停机成功后原子发布新产物 ─────────────────────────────────────────
if [ "$DO_SIDECAR" = 1 ]; then
  if [ -e "$SIDECAR_OUT" ]; then
    SIDECAR_BACKUP="$(mktemp "$(dirname "$SIDECAR_OUT")/.summon-sidecar.old.XXXXXX")"
    if ! cp -p "$SIDECAR_OUT" "$SIDECAR_BACKUP"; then
      rm -f "$SIDECAR_BACKUP"
      SIDECAR_BACKUP=""
      die "备份旧 sidecar 失败；未发布新产物"
    fi
  fi
  mv -f "$STAGED_SIDECAR" "$SIDECAR_OUT"
  STAGED_SIDECAR=""
  SIDECAR_PUBLISHED=1
  log "已发布 sidecar → $SIDECAR_OUT"
fi
if [ "$DO_INSTALL" = 1 ]; then
  if [ -e "$INSTALL_BIN" ]; then
    CLI_BACKUP="$(mktemp "$BIN_DIR/.summon.old.XXXXXX")"
    if ! cp -p "$INSTALL_BIN" "$CLI_BACKUP"; then
      rm -f "$CLI_BACKUP"
      CLI_BACKUP=""
      die "备份旧 CLI 失败；将恢复本次已发布产物"
    fi
  fi
  mv -f "$STAGED_CLI" "$INSTALL_BIN"
  STAGED_CLI=""
  CLI_PUBLISHED=1
  BIN="$INSTALL_BIN"
  log "已发布全局 summon($("$BIN" --version 2>/dev/null || echo 'version-n/a'))"
  # PATH 入口:~/.local/bin/summon 指向真身。
  #
  # **`ln -sfn` 是原子替换**(rename 语义),中途不会出现"链接不存在"的窗口。但它只对
  # 「目标已是软链」成立 —— 旧版本在这里放的是**实体文件**,`ln -s` 会拒绝覆盖,所以先删。
  # 删之前确认它不是我们刚发布的真身本体(路径不同即安全),避免把 CLI 自己删掉。
  mkdir -p "$LINK_DIR"
  if [ "$LINK_BIN" != "$INSTALL_BIN" ]; then
    if [ -e "$LINK_BIN" ] && [ ! -L "$LINK_BIN" ]; then
      log "迁移:$LINK_BIN 原为实体文件,替换为指向 $INSTALL_BIN 的软链"
      rm -f "$LINK_BIN"
    fi
    if ln -sfn "$INSTALL_BIN" "$LINK_BIN"; then
      log "已链接 $LINK_BIN → $INSTALL_BIN"
    else
      log "警告:创建 $LINK_BIN 软链失败;直接用 $INSTALL_BIN 或自行加软链"
    fi
  fi
  case ":$PATH:" in
    *":$LINK_DIR:"*) ;;
    *) log "提示:确保 $LINK_DIR 在 PATH 里(export PATH=\"$LINK_DIR:\$PATH\")" ;;
  esac
fi
if [ -n "$STAGED_MENUBAR" ]; then
  if [ -e "$MENUBAR_OUT" ]; then
    MENUBAR_BACKUP="$(mktemp "$(dirname "$MENUBAR_OUT")/.summon-menubar.old.XXXXXX")"
    if ! cp -p "$MENUBAR_OUT" "$MENUBAR_BACKUP"; then
      rm -f "$MENUBAR_BACKUP"
      MENUBAR_BACKUP=""
      die "备份旧 Menu Bar 失败；将恢复本次已发布产物"
    fi
  fi
  mv -f "$STAGED_MENUBAR" "$MENUBAR_OUT"
  STAGED_MENUBAR=""
  MENUBAR_PUBLISHED=1
  log "已发布原生 Menu Bar → $MENUBAR_OUT"
fi
PUBLISH_COMPLETE=1
# 落点那份登记自己的构建戳:运行期自发布凭它拒绝拿旧的覆盖新的。写不成不回滚 —— 产物已经
# 自洽,缺戳只让下一次比较退回「判不出新旧」的保守路径。
if [ "$DO_INSTALL" = 1 ]; then
  "$INSTALL_BIN" install-stamp record \
    || log "警告:落点构建戳没写成;daemon start 会把落点当作判不出新旧"
fi
[ -z "$SIDECAR_BACKUP" ] || rm -f "$SIDECAR_BACKUP"
[ -z "$CLI_BACKUP" ] || rm -f "$CLI_BACKUP"
[ -z "$MENUBAR_BACKUP" ] || rm -f "$MENUBAR_BACKUP"
SIDECAR_BACKUP=""
CLI_BACKUP=""
MENUBAR_BACKUP=""

# ── 3. 启动(启动时子任务消费 outbox:批量派发待办→ack→归档,防雪崩) ──────
if ! AGENT_LIST="$("$BIN" agents ls 2>&1)"; then
  case "$AGENT_LIST" in
    *"agents dir not found:"*)
      log "未发现已配置 agent；CLI 与 sidecar 已安装，跳过 daemon 启动"
      printf '%s\n' "下一步: summon onboard <alias> 配置第一个飞书 bot agent。"
      exit 0
      ;;
    *) die "agent registry 不可读；请先修复后再启动 daemon: $AGENT_LIST" ;;
  esac
fi
log "启动新 daemon …"
# 起 daemon 时带上渠道钩子给的环境(见文件开头)。
# shellcheck disable=SC2086 # 钩子给的是若干个 KEY=VALUE,按空白拆开交给 env
env $DAEMON_START_ENV "$BIN" daemon start || die "daemon start 失败"
DAEMON_RESTORE_PENDING=0
sleep 1
NEW_PID="$(sed -n '1p' "$HOME/.summon/run/daemon.pid" 2>/dev/null || true)"
case "$NEW_PID" in
  ''|*[!0-9]*) die "daemon 启动后没有有效 PID" ;;
esac
DAEMON_STATUS="$("$BIN" daemon status 2>/dev/null || true)"
EXPECTED_STATUS="running(pid $NEW_PID, version $BUILT_VERSION)"
[ "$DAEMON_STATUS" = "$EXPECTED_STATUS" ] \
  || die "daemon 版本未生效(want '$EXPECTED_STATUS', got '${DAEMON_STATUS:-empty}')"
printf '%s\n' "$DAEMON_STATUS"
if [ "$DO_INSTALL" = 1 ] && [ "$DO_MENUBAR" = 1 ]; then
  "$BIN" menubar install || die "Menu Bar LaunchAgent 安装失败（daemon 与 CLI 已发布）"
  wait_for_menubar \
    || die "核心部署已完成,但 Menu Bar 未进入运行态(got '${MENUBAR_STATUS:-empty}')"
  printf '%s\n' "$MENUBAR_STATUS"
fi
log "部署完成。新 daemon pid=$NEW_PID,version=$BUILT_VERSION;tmux(claude)会话未受影响,无头会话靠 outbox 续跑/补发通知。"
