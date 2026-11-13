#!/usr/bin/env bash
# GitHub Release 打包的纯逻辑:平台名映射与 glibc 版本检查。package.sh 与
# scripts/tests/test-github-release-package.sh 共用;这里只定义函数,source 时没有副作用。

# Linux 包里的二进制最多只能引用到这个版本的 glibc 符号。发布包在 Ubuntu 22.04(glibc 2.35)上
# 构建,最低支持的系统就是这一代;在更新的系统上构建会悄悄引用更新的符号,装到 22.04 / Debian 12
# 上直接起不来。改这个值等于改最低支持的系统,要连同构建环境一起改。
SUMMON_RELEASE_GLIBC_MAX="2.35"

# `uname -s` `uname -m` → 发布包的平台名。不在发布范围内的组合返回 1。
summon_release_platform() {
  case "$1/$2" in
    Linux/x86_64 | Linux/amd64) printf 'linux-x64\n' ;;
    Linux/aarch64 | Linux/arm64) printf 'linux-arm64\n' ;;
    Darwin/arm64 | Darwin/aarch64) printf 'darwin-arm64\n' ;;
    *) return 1 ;;
  esac
}

# 平台的包里有哪些文件(一行一个)。
summon_release_files() {
  case "$1" in
    linux-x64 | linux-arm64) printf 'summon\nsummon-sidecar\n' ;;
    darwin-arm64) printf 'summon\nsummon-sidecar\nsummon-menubar\n' ;;
    *) return 1 ;;
  esac
}

# 从 `objdump -T` / `readelf --version-info` 的输出(标准输入)里取最高的 glibc 符号版本;
# 没有任何 glibc 符号时输出空。
summon_glibc_max_version() {
  grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' \
    | sed 's/^GLIBC_//' \
    | sort -u -t. -k1,1n -k2,2n -k3,3n \
    | tail -n 1
}

# a ≤ b(点分数字版本)。任一边不是点分数字时返回 2。
summon_version_le() {
  local a="$1" b="$2" i x y
  [[ "$a" =~ ^[0-9]+(\.[0-9]+)*$ ]] || return 2
  [[ "$b" =~ ^[0-9]+(\.[0-9]+)*$ ]] || return 2
  local -a pa pb
  IFS=. read -r -a pa <<< "$a"
  IFS=. read -r -a pb <<< "$b"
  for ((i = 0; i < ${#pa[@]} || i < ${#pb[@]}; i++)); do
    x="${pa[i]:-0}"
    y="${pb[i]:-0}"
    ((10#$x < 10#$y)) && return 0
    ((10#$x > 10#$y)) && return 1
  done
  return 0
}

# 列出一个二进制的动态符号;优先 objdump,没有就用 readelf。静态链接的二进制没有动态符号表,
# objdump 对它报 "not a dynamic object" 并以非零退出,这种情形输出空、算成功。
summon_dynamic_symbols() {
  local out
  if command -v objdump > /dev/null 2>&1; then
    if out="$(objdump -T "$1" 2>&1)"; then
      printf '%s\n' "$out"
    else
      case "$out" in
        *'not a dynamic object'*) return 0 ;;
      esac
      printf '%s\n' "$out" >&2
      return 1
    fi
  elif command -v readelf > /dev/null 2>&1; then
    readelf --version-info --wide "$1"
  else
    printf '缺 objdump 与 readelf,查不了 glibc 符号版本\n' >&2
    return 1
  fi
}

# summon_check_glibc <上限> <二进制...>:任一二进制引用的 glibc 符号版本超过上限就报出是哪个、
# 哪个版本,返回 1。不引用 glibc 的(静态链接)算通过。
summon_check_glibc() {
  local limit="$1" bin symbols max failed=0
  shift
  for bin in "$@"; do
    symbols="$(summon_dynamic_symbols "$bin")" || return 1
    max="$(printf '%s\n' "$symbols" | summon_glibc_max_version)"
    if [ -z "$max" ]; then
      printf '  %s:不引用 glibc 符号\n' "$(basename "$bin")"
      continue
    fi
    if summon_version_le "$max" "$limit"; then
      printf '  %s:最高 GLIBC_%s(上限 %s)\n' "$(basename "$bin")" "$max" "$limit"
    else
      printf '%s 引用了 GLIBC_%s,超过上限 %s(最低支持的系统会被抬高)\n' \
        "$bin" "$max" "$limit" >&2
      failed=1
    fi
  done
  return "$failed"
}
