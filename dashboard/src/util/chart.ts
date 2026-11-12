// 控制台里自己画的 SVG 图表共用的几何:刻度与柱形。不引图表库。

/** y 轴刻度:从 0 起,步长取 1 / 2 / 5 × 10ⁿ 的整数,四段上下。 */
export function niceTicks(max: number): number[] {
  if (max <= 0) return [0, 1];
  const raw = max / 4;
  const pow = 10 ** Math.floor(Math.log10(raw));
  const step = Math.max(1, Math.round([1, 2, 5, 10].map((m) => m * pow).find((s) => s >= raw) ?? raw));
  const ticks: number[] = [];
  for (let v = 0; v < max + step; v += step) ticks.push(v);
  return ticks;
}

/** 顶端 4px 圆角、底端方角的柱段。 */
export function roundTop(x: number, y: number, w: number, h: number): string {
  const r = Math.min(4, w / 2, h);
  return `M${x},${y + h}V${y + r}Q${x},${y} ${x + r},${y}H${x + w - r}Q${x + w},${y} ${x + w},${y + r}V${y + h}Z`;
}
