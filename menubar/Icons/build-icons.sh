#!/bin/sh
# 从本目录的 SVG 生成 ../Sources/SummonMenuBar/IconData.swift:状态栏模板图的 1x/2x/3x PNG。
# SVG 是图标几何的唯一来源,改了就重跑;依赖 rsvg-convert(brew install librsvg)。
# 嵌成 PNG 而不是矢量:SVG 的 mask 转 PDF 后 AppKit 画不出来,而状态栏尺寸固定,多倍率位图足够清晰。
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out="$here/../Sources/SummonMenuBar/IconData.swift"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
{
  echo '// 由 menubar/Icons/build-icons.sh 从 menubar/Icons/*.svg 生成,勿手改;改 SVG 后重跑脚本。'
  echo 'import CoreGraphics'
  echo
  echo 'enum IconData {'
  echo '    struct Entry: Sendable {'
  echo '        let points: CGFloat'
  echo '        let pngs: [Int: String]'
  echo '    }'
  echo
  echo '    static let icons: [String: Entry] = ['
  for svg in "$here"/*.svg; do
    name=$(basename "$svg" .svg)
    case "$name" in
      summon-*) points=17 ;;
      *) points=16 ;;
    esac
    printf '        "%s": Entry(points: %s, pngs: [\n' "$name" "$points"
    for scale in 1 2 3; do
      px=$((points * scale))
      rsvg-convert -w "$px" -h "$px" -o "$tmp/$name-$scale.png" "$svg"
      printf '            %s: "%s",\n' "$scale" "$(base64 < "$tmp/$name-$scale.png" | tr -d '\n')"
    done
    echo '        ]),'
  done
  echo '    ]'
  echo '}'
} > "$out"
echo "wrote $out"
