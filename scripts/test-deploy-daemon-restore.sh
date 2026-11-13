#!/usr/bin/env bash
# deploy.sh 的不变量:**停机之后的任何一条退出路径,机器上都必须仍有 daemon 在跑**。
#
# `daemon stop` 一发出 SIGTERM 就不可撤销:CLI 等 10s 不见退出会报错返回,而那条 SIGTERM
# 已经在路上,daemon 终将退出。脚本此时若把它当成"可回滚的前置失败"直接中止,保住的只是
# 磁盘上的旧产物,保不住进程——结果是新产物没装上、旧 daemon 也没了。2026-09-03 本机实测
# 就这样裸奔了 1.5 分钟:tmux 里的 agent 恰好活着才被人工救回,codex/cursor 那种随 daemon
# 一起死的会话没有这个运气。
#
# 两个场景一正一反,缺一不可:
#   A 停机报错但 daemon 确实退了 → 停机其实成功了,部署必须照常走完。
#   B 停机报错且 daemon 还活着   → 必须中止,且绝不能起第二个(双实例比裸奔更糟)。
#   C 停机之后才失败             → 中止,但必须把 daemon 拉回来。
# 三条各锁一个机制:A 是「判据看 pid 不看退出码」,C 是「退出前兜底拉回」,B 是防止为了
# 前两条把双实例那道闸拆掉。少一条,另外两条就可能互相顶替着假绿。
#
# 同一套隔离环境顺带锁住 --install 与落点构建戳的契约:
#   D 本次构建比落点旧 → 提示但照装,且新 CLI 发布进落点之后、起 daemon 之前登记构建戳。
set -euo pipefail

cd "$(dirname "$0")/.."
REPO="$PWD"

WORK="$(mktemp -d)"
FAKE_PID=""
cleanup() {
  [ -z "$FAKE_PID" ] || kill "$FAKE_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { printf 'FAIL %s\n' "$1" >&2; exit 1; }

# deploy.sh 会通过 /proc 或 lsof 反查活 daemon 的可执行文件,拿它当停机控制程序。假 daemon
# 实际是 sleep,这两条路只会报出 sleep——在测试里没有意义。让反查返回空,脚本便退回
# probe_controller 链(按 `daemon status` 认代际),覆盖 Linux 无进程镜像时的同一条路径。
mkdir -p "$WORK/bin"
printf '#!/bin/sh\nexit 1\n' > "$WORK/bin/readlink"
chmod +x "$WORK/bin/readlink"
printf '#!/bin/sh\nexit 1\n' > "$WORK/bin/lsof"
chmod +x "$WORK/bin/lsof"
# deploy.sh 靠 `cargo metadata` 解析 target 目录(本机 .cargo/config.toml 可能改过它)。
# 隔离 HOME 下的 rustup 找不到 toolchain,真 cargo 跑不起来;这里只需要它报出测试的
# target 目录,好让隔离环境的 stub 产物被找到,真实 target/ 一个字节都不碰。
printf '#!/bin/sh\nprintf \047{"target_directory":"%%s"}\047 "$TEST_TARGET_DIR"\n' > "$WORK/bin/cargo"
chmod +x "$WORK/bin/cargo"

# 只实现 deploy.sh 真正调用的那几条子命令。状态与调用流水都落在隔离 HOME 里,
# 故 snapshot_controller 复制出的副本与本体行为一致。
make_stub() {
  cat > "$1" <<'STUB'
#!/usr/bin/env bash
set -u
STATE="$HOME/.summon/stub-state"
CALLS="$HOME/.summon/stub-calls"
PIDFILE="$HOME/.summon/run/daemon.pid"
running() { [ "$(cat "$STATE" 2>/dev/null)" = running ]; }
case "$*" in
  "--version") echo "summon 9.9.9 (stub)" ;;
  "daemon status")
    if running; then echo "running(pid $(cat "$PIDFILE" 2>/dev/null), version 9.9.9)"
    else echo "daemon 未运行"; fi ;;
  "daemon stop")
    echo "daemon stop" >> "$CALLS"
    running || { echo "daemon 未运行"; exit 0; }
    # 事故复现:SIGTERM 已送达(daemon 按 STUB_DAEMON_EXITS 决定退不退),但 CLI 等不到就报错返回。
    if [ "${STUB_DAEMON_EXITS:-1}" = 1 ]; then
      kill "$(cat "$PIDFILE" 2>/dev/null)" 2>/dev/null || true
      echo stopped > "$STATE"
    fi
    echo "daemon(pid $(cat "$PIDFILE" 2>/dev/null)) 10s 内未退出;为保护会话不自动强杀" >&2
    exit 1 ;;
  "daemon start")
    echo "daemon start" >> "$CALLS"
    # 镜像真 CLI 的单例保护:已有存活 daemon 就不起第二个。
    running && { echo "daemon 已在运行"; exit 0; }
    setsid sleep 300 >/dev/null 2>&1 &
    echo $! > "$PIDFILE"
    echo running > "$STATE"
    echo "daemon(summond,pid $(cat "$PIDFILE"), version 9.9.9)已后台启动" ;;
  "install-stamp check") echo "${STUB_INSTALL_RELATION:-}" ;;
  "install-stamp record") echo "install-stamp record $0" >> "$CALLS" ;;
  "agents ls")
    [ "${STUB_AGENTS_FAIL:-0}" = 1 ] || exit 0
    echo "registry 读不出来"; exit 1 ;;
  *) exit 0 ;;
esac
STUB
  chmod +x "$1"
}

# 每个场景一套干净的隔离 HOME + 假 daemon。
setup_scene() {
  rm -rf "$WORK/home" "$WORK/target"
  mkdir -p "$WORK/home/.summon/bin" "$WORK/home/.summon/run" "$WORK/target/release"
  make_stub "$WORK/home/.summon/bin/summon"
  make_stub "$WORK/target/release/summon"
  # 假 daemon 必须让 `ps -o command=` 认出来,否则 deploy.sh 判定「无运行中的 daemon」。
  bash -c 'exec -a "summond daemon run" sleep 300' >/dev/null 2>&1 &
  FAKE_PID=$!
  echo "$FAKE_PID" > "$WORK/home/.summon/run/daemon.pid"
  echo running > "$WORK/home/.summon/stub-state"
  : > "$WORK/home/.summon/stub-calls"
  sleep 0.3
}

run_deploy() {
  set +e
  env -i \
    PATH="$WORK/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
    HOME="$WORK/home" \
    TEST_TARGET_DIR="$WORK/target" \
    SUMMON_DEPLOY_NO_DETACH=1 \
    SUMMON_CODESIGN_MODE=adhoc \
    SUMMON_CODESIGN_TIMESTAMP=none \
    STUB_DAEMON_EXITS="$1" \
    STUB_AGENTS_FAIL="${2:-0}" \
    STUB_INSTALL_RELATION="${STUB_INSTALL_RELATION:-}" \
    bash "$REPO/scripts/deploy.sh" --skip-build --no-sidecar --no-menubar --install \
    > "$WORK/out" 2>&1
  DEPLOY_RC=$?
  set -e
}

daemon_alive() { [ "$(cat "$WORK/home/.summon/stub-state" 2>/dev/null)" = running ]; }
start_count() { grep -c '^daemon start$' "$WORK/home/.summon/stub-calls" 2>/dev/null || true; }
# C、D 都要真的起过 daemon,否则后面的断言是空转。
require_started() {
  [ "$(start_count)" -gt 0 ] || { dump; fail "$1: 没有起过 daemon,场景没摆成"; }
}
dump() {
  echo "--- deploy 输出 ---" >&2; cat "$WORK/out" >&2
  echo "--- stub 调用流水 ---" >&2; cat "$WORK/home/.summon/stub-calls" >&2
}
# 走到停机是这两个场景的共同前提。它不成立时后面的断言全是空转,必须当场判失败。
require_stop_attempted() {
  grep -q '^daemon stop$' "$WORK/home/.summon/stub-calls" 2>/dev/null || {
    dump; fail "$1: deploy 没走到停机段,场景没摆成"
  }
}

# ── 场景 A:停机报错,但 daemon 确实退了 ────────────────────────────────────
setup_scene
run_deploy 1
require_stop_attempted A
daemon_alive || { dump; fail "A: 停机后 daemon 没被起回来(裸奔);deploy 退出码 $DEPLOY_RC"; }
[ "$DEPLOY_RC" = 0 ] || { dump; fail "A: 停机其实成功了,部署却没走完(退出码 $DEPLOY_RC)"; }

# ── 场景 B:停机报错,daemon 还活着 → 必须中止且不起第二个 ──────────────────
setup_scene
run_deploy 0
require_stop_attempted B
[ "$DEPLOY_RC" -ne 0 ] || { dump; fail "B: daemon 未退出却让部署继续了"; }
[ "$(start_count)" = 0 ] || { dump; fail "B: 旧 daemon 还活着却又启动了一个(双实例)"; }

# ── 场景 C:停机之后才失败 → 必须拉回 daemon ───────────────────────────────
setup_scene
run_deploy 1 1
require_stop_attempted C
[ "$DEPLOY_RC" -ne 0 ] || { dump; fail "C: registry 不可读却报成功了"; }
daemon_alive || { dump; fail "C: 停机之后失败退出,daemon 没被拉回来(裸奔)"; }
require_started C

# ── 场景 D:装的比落点旧 → 提示但照装,并由落点那份登记构建戳 ─────────────────
setup_scene
STUB_INSTALL_RELATION="older 本次 0.6.8 (aaaaaaa) / 落点 0.6.9 (bbbbbbb)" run_deploy 1
[ "$DEPLOY_RC" = 0 ] || { dump; fail "D: 装旧版本应照装,却中止了(退出码 $DEPLOY_RC)"; }
grep -q '比 ~/.summon/bin 里那份旧:本次 0.6.8' "$WORK/out" \
  || { dump; fail "D: 装的比落点旧却没有提示"; }
# 登记必须由**落点里那份**做、在停机发布之后、起 daemon 之前 —— 早了描述的是旧文件,
# 晚了新 daemon 起来前那段时间读戳的一方认不出落点。(停机会连发两次 stop:真停 + 复验锁空闲。)
CALL_ORDER="$(grep -E '^(daemon stop|install-stamp record|daemon start)' "$WORK/home/.summon/stub-calls" \
  | uniq | tr '\n' '|')"
case "$CALL_ORDER" in
  "daemon stop|install-stamp record $WORK/home/.summon/bin/summon|daemon start|") ;;
  *) dump; fail "D: 构建戳登记的时机或执行者不对:$CALL_ORDER" ;;
esac
require_started D

echo "PASS deploy 停机后不留裸奔,且不为此制造双实例;装旧版本只提示、照装并登记构建戳"
