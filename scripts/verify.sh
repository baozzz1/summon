#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "[1/8] script tests"
bash scripts/test-codesign.sh
bash scripts/test-deploy-daemon-restore.sh
python3 scripts/tests/test_check_public_content.py
bash scripts/tests/test-github-release-package.sh
python3 scripts/tests/test_github_release_manifest.py
python3 scripts/tests/test_github_release_info.py
python3 scripts/tests/test_github_release_plan.py
python3 scripts/tests/test_github_release_workflow.py
python3 scripts/tests/test_install_script.py
# 安装渠道自带的检查:scripts/verify.d/*.sh 按文件名顺序跑,源码安装没有。
for check in scripts/verify.d/*.sh; do
  [ -f "$check" ] || continue
  bash "$check"
done

echo "[2/8] rustfmt"
cargo fmt --all -- --check

echo "[3/8] clippy"
cargo clippy --workspace --all-targets -- -D warnings

echo "[4/8] Rust tests"
cargo test --workspace --all-targets

echo "[5/8] 原生 macOS Menu Bar"
if [ "$(uname -s)" = "Darwin" ]; then
  command -v swift >/dev/null 2>&1 || {
    echo "错误：macOS 主体门禁需要 Swift toolchain。" >&2
    exit 1
  }
  swift test --package-path menubar
  swift build -c release --package-path menubar
else
  echo "非 macOS，跳过 AppKit Menu Bar 构建。"
fi

echo "[6/8] 无凭据的 ignored 安全测试"
if ! command -v tmux >/dev/null 2>&1; then
  echo "错误：主体门禁需要 tmux；未找到 tmux，不能把本次验证宣称为通过。" >&2
  exit 1
fi
cargo test -p summon spawn_scrubs_inherited_feishu_and_injects_own -- --ignored --test-threads=1
cargo test -p summon session::tmux::tests:: -- --ignored --test-threads=1
cargo test -p summon ensure_summon_repo_inits_and_respects_existing_gitignore -- --ignored --test-threads=1

echo "[7/8] Feishu sidecar"
(
  cd sidecar/feishu-connector
  go test ./...
  go test -race ./...
  go vet ./...
)

echo "[8/8] build"
cargo build --workspace
# 渠道检查里要拿刚构建的真 summon 跑的那一段(给了 SUMMON_TEST_BIN 才跑)。
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
for check in scripts/verify.d/*.sh; do
  [ -f "$check" ] || continue
  SUMMON_TEST_BIN="${TARGET_DIR:-target}/debug/summon" bash "$check"
done

cat <<'EOF'

主体门禁通过。

仍需真实 CLI/登录态的人工批量验证：
  cargo test -p summon -- --ignored real_claude
  cargo test -p summon -- --ignored real_cursor
  cargo test -p summon -- --ignored real_codex
  cargo test -p summon -- --ignored claude_transient_worker_runs_and_tears_down_session
EOF
