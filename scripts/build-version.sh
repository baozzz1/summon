#!/usr/bin/env bash

# summon 的版本号 —— **单一事实源是 `crates/summon/Cargo.toml` 的 `[package].version`**。
# 安装与发版脚本共用这两个函数。
#
# why 问 cargo 而不是自己 grep Cargo.toml:cargo 才认识 workspace 继承(`version.workspace = true`)
# 这类规则。手写解析在有人改成继承写法的那天会静默给出一个过时的答案 —— 而版本号一旦错,
# 装出来的东西就和它自报的版本对不上。
#
# 「包上写的版本」和「二进制里编进去的版本」同源于 Cargo.toml,没有第二条链 —— 也就没有
# 「两边对不上而无人报错」的可能。所以这里不提供任何覆盖版本号的入口。

summon_validate_build_version() {
  [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]
}

summon_build_version() {
  local repo pkgid version
  # 绑到脚本自身位置,不靠调用方的 cwd —— 各调用方的 cwd 约定不同。
  repo="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
  # `--offline`:取版本号不该联网,更不该因为网络抖动让部署失败。
  pkgid="$(cd "$repo" && cargo pkgid -p summon --offline 2>/dev/null)" || pkgid=""
  # `path+file:///…#0.2.0`,包目录名与包名不同时是 `…#summon@0.2.0` —— 两种形态都剥掉。
  version="${pkgid##*#}"
  version="${version##*@}"
  if ! summon_validate_build_version "$version"; then
    printf '解析不出 summon 的版本(cargo pkgid 给的是:%s);检查 crates/summon/Cargo.toml\n' \
      "${pkgid:-空}" >&2
    return 1
  fi
  printf '%s\n' "$version"
}
