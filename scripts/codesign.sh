#!/usr/bin/env bash
# macOS 发布签名的单一事实源。调用方先 `summon_prepare_codesign`，再逐个 `summon_sign_path`。

SUMMON_CODESIGN_KIND="none"
SUMMON_CODESIGN_CERT=""
SUMMON_CODESIGN_TIMESTAMP_POLICY="required"

summon_select_codesign_identity() {
  local mode="${1:-auto}"
  local explicit="${2:-}"
  local discovered="${3:-}"
  local unique count

  case "$mode" in
    adhoc)
      printf 'adhoc|\n'
      return 0
      ;;
    auto|developer-id) ;;
    *)
      printf 'error|\n'
      return 0
      ;;
  esac
  if [ -n "$explicit" ]; then
    printf 'developer-id|%s\n' "$explicit"
    return 0
  fi

  unique="$(printf '%s\n' "$discovered" | awk 'NF' | sort -u)"
  count="$(printf '%s\n' "$unique" | awk 'NF { n++ } END { print n + 0 }')"
  case "$count" in
    0)
      if [ "$mode" = "developer-id" ]; then
        printf 'error|\n'
      else
        printf 'adhoc|\n'
      fi
      ;;
    1) printf 'developer-id|%s\n' "$unique" ;;
    *) printf 'ambiguous|\n' ;;
  esac
}

summon_discover_developer_identities() {
  security find-identity -v -p codesigning 2>/dev/null \
    | sed -nE 's/^[[:space:]]*[0-9]+\)[[:space:]]+([0-9A-F]+)[[:space:]]+"Developer ID Application:.*"$/\1/p' \
    | sort -u
}

summon_select_timestamp_policy() {
  local requested="${1:-}"
  local default_policy="${2:-required}"
  local selected="${requested:-$default_policy}"
  case "$selected" in
    required|best-effort|none) printf '%s\n' "$selected" ;;
    *) printf 'error\n' ;;
  esac
}

summon_prepare_codesign() {
  local mode="${SUMMON_CODESIGN_MODE:-auto}"
  local explicit="${SUMMON_CODESIGN_IDENTITY:-}"
  local discovered selection

  SUMMON_CODESIGN_KIND="none"
  SUMMON_CODESIGN_CERT=""
  SUMMON_CODESIGN_TIMESTAMP_POLICY="$(
    summon_select_timestamp_policy "${SUMMON_CODESIGN_TIMESTAMP:-}" required
  )"
  if [ "$SUMMON_CODESIGN_TIMESTAMP_POLICY" = "error" ]; then
    printf '[codesign] SUMMON_CODESIGN_TIMESTAMP 只接受 required|best-effort|none\n' >&2
    return 1
  fi
  if [ "$(uname -s)" != "Darwin" ] || ! command -v codesign >/dev/null 2>&1; then
    return 0
  fi
  discovered="$(summon_discover_developer_identities)"
  selection="$(summon_select_codesign_identity "$mode" "$explicit" "$discovered")"
  SUMMON_CODESIGN_KIND="${selection%%|*}"
  SUMMON_CODESIGN_CERT="${selection#*|}"
  case "$SUMMON_CODESIGN_KIND" in
    developer-id)
      printf '[codesign] 使用 Developer ID Application（稳定 Team ID）\n'
      ;;
    adhoc)
      printf '[codesign] 未发现 Developer ID，回退 ad-hoc；TCC 授权不会跨构建稳定\n' >&2
      ;;
    ambiguous)
      printf '[codesign] 发现多个 Developer ID；请设置 SUMMON_CODESIGN_IDENTITY 指定证书 SHA-1 或名称\n' >&2
      return 1
      ;;
    *)
      printf '[codesign] SUMMON_CODESIGN_MODE=%s 要求 Developer ID，但没有可用身份\n' "$mode" >&2
      return 1
      ;;
  esac
}

# 执行一次 codesign,按 `SUMMON_CODESIGN_TIMESTAMP_POLICY` 决定时间戳。
# 抽出来是因为签名现在要对**每个嵌套组件**各调一次(见 `summon_sign_path` 的 bundle 分支)。
summon_codesign_exec() {
  local target="$1"
  shift
  if [ "$SUMMON_CODESIGN_KIND" != "developer-id" ]; then
    codesign "$@" "$target" || return 1
    return 0
  fi
  case "$SUMMON_CODESIGN_TIMESTAMP_POLICY" in
    required)
      codesign "$@" --timestamp "$target" || return 1
      ;;
    none)
      codesign "$@" --timestamp=none "$target" || return 1
      ;;
    best-effort)
      if ! codesign "$@" --timestamp "$target"; then
        printf '[codesign] 安全时间戳服务不可用，本地部署退回无时间戳 Developer ID 签名\n' >&2
        codesign "$@" --timestamp=none "$target" || return 1
      fi
      ;;
  esac
}

# Hardened Runtime 下 Electron 必需的 entitlements:allow-jit / allow-unsigned-executable-memory /
# disable-library-validation(Electron 官方的最小集合,不含任何沙箱或隐私权限)。
#
# 2026-08-10 真机:签名带了 `--options runtime` 却没给 entitlements,Hardened Runtime 默认禁 JIT
# 与未签名可执行内存,而 V8 两者都要 —— 启动即
# `Fatal process out of memory: Failed to reserve virtual memory for CodeRange`,崩溃报告是
# EXC_BREAKPOINT/SIGTRAP,用户看到的是「Summon quit unexpectedly」。
#
# **那个 plist 里不能写 XML 注释**:codesign 的 AMFI 解析器会 `syntax error` 拒绝整份文件
# (同日踩过),所以 why 只能写在这里。
SUMMON_ELECTRON_ENTITLEMENTS="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)/electron.entitlements.plist"

summon_sign_path() {
  local target="$1"
  local identifier="$2"
  local shape="${3:-binary}"
  local sign_args verify_args info actual_identifier team_identifier authority nested nested_list

  [ "$SUMMON_CODESIGN_KIND" != "none" ] || return 0
  [ -e "$target" ] || {
    printf '[codesign] 待签名产物不存在:%s\n' "$target" >&2
    return 1
  }
  if [ "$shape" = "bundle" ]; then
    xattr -cr "$target" 2>/dev/null || true
    verify_args=(--verify --deep --strict --verbose=2)
  else
    xattr -c "$target" 2>/dev/null || true
    verify_args=(--verify --strict --verbose=2)
  fi
  sign_args=(--force)
  if [ "$SUMMON_CODESIGN_KIND" = "developer-id" ]; then
    sign_args+=(--options runtime --sign "$SUMMON_CODESIGN_CERT")
  else
    sign_args+=(--sign -)
  fi
  # **Hardened Runtime 必须配 entitlements,否则 Electron 的 V8 分配不到可执行内存**
  # (`--options runtime` 默认禁 JIT)。只对 bundle 加:CLI/sidecar/menubar 是原生二进制,
  # 不需要 JIT,给多余权限反而扩大攻击面。
  if [ "$shape" = "bundle" ] && [ -f "$SUMMON_ELECTRON_ENTITLEMENTS" ]; then
    sign_args+=(--entitlements "$SUMMON_ELECTRON_ENTITLEMENTS")
  fi

  if [ "$shape" = "bundle" ]; then
    # **从内到外逐个签,不用 `--deep`。**
    #
    # 2026-08-10 真机:`--deep` 漏签了
    # `Electron Framework.framework/Versions/A/Libraries/libffmpeg.dylib`,它保持 Electron 自带的
    # ad-hoc 签名(TeamIdentifier=not set),而外层是 Developer ID 签名。dyld 加载时因
    # 「mapping process and mapped file have different Team IDs」直接拒绝,**控制台根本起不来**——
    # 而 macOS 弹的是「Check with the developer...may need to reinstall」这种看着像兼容性问题的话。
    #
    # 更糟的是 `--verify --deep --strict` **也没查出来**,所以部署一路绿灯。Apple 文档明确说过
    # `--deep` 不能替代逐个签署嵌套代码;这里按深度优先(`find -depth`)从最内层往外签。
    # 内层**不加** `--identifier`:每个嵌套组件有自己的 identifier,强行统一会破坏它们的身份。
    # **不用进程替换 `< <(...)`**:本脚本被 `sh scripts/deploy.sh` 以 POSIX 模式 source,
    # macOS 的 /bin/sh 会禁用它(2026-08-10 踩过:`bash -n` 查得过、`sh` 一跑就 syntax error)。
    # 改为先落一份清单再逐行读;路径含空格(`Summon Helper (GPU).app`)靠 `IFS=` 保住。
    nested_list="$(mktemp "${TMPDIR:-/tmp}/summon-codesign-nested.XXXXXX")" || return 1
    find "$target" -depth \
      \( -name '*.dylib' -o -name '*.so' -o -name '*.framework' -o -name '*.app' -o -path '*/Helpers/*' \) \
      ! -path "$target" -print > "$nested_list" 2>/dev/null
    while IFS= read -r nested; do
      [ -n "$nested" ] || continue
      summon_codesign_exec "$nested" "${sign_args[@]}" || {
        printf '[codesign] 嵌套组件签名失败:%s\n' "$nested" >&2
        rm -f "$nested_list"
        return 1
      }
    done < "$nested_list"
    rm -f "$nested_list"
  fi
  summon_codesign_exec "$target" "${sign_args[@]}" --identifier "$identifier" || return 1
  codesign "${verify_args[@]}" "$target" || return 1
  info="$(codesign -dvvv "$target" 2>&1)" || return 1
  actual_identifier="$(printf '%s\n' "$info" | sed -n 's/^Identifier=//p' | head -n 1)"
  [ "$actual_identifier" = "$identifier" ] || {
    printf '[codesign] identifier 校验失败:%s(want %s)\n' "$actual_identifier" "$identifier" >&2
    return 1
  }
  if [ "$SUMMON_CODESIGN_KIND" = "developer-id" ]; then
    authority="$(printf '%s\n' "$info" | sed -n 's/^Authority=//p' | head -n 1)"
    case "$authority" in
      'Developer ID Application:'*) ;;
      *)
        printf '[codesign] 签名证书不是 Developer ID Application:%s\n' "$authority" >&2
        return 1
        ;;
    esac
    team_identifier="$(printf '%s\n' "$info" | sed -n 's/^TeamIdentifier=//p' | head -n 1)"
    case "$team_identifier" in
      ''|'not set')
        printf '[codesign] Developer ID 签名缺少 TeamIdentifier:%s\n' "$target" >&2
        return 1
        ;;
    esac
  fi
}
