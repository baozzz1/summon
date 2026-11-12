import { describe, expect, test } from 'vitest';
import favicon from '../favicon.svg?raw';
import landing from '../../waiting.html?raw';
import local from '../../local.html?raw';
import { DEFAULT_ICON, iconSvg } from './brand';

/** 图形元素(path / circle 等)按出现顺序,空白归一。 */
function shapes(svg: string): string[] {
  return [...svg.matchAll(/<(path|circle|rect|ellipse|line|polyline|polygon)\b[^>]*\/>/g)].map((m) => m[0].replace(/\s+/g, ' '));
}

describe('标识', () => {
  // 登录前的两张落地页与默认标签页图标读不到配置,画的是 Menu Bar 的默认图标,几何须与 menubar/Icons 一致。
  test('pre_login_pages_draw_the_default_menubar_icon', () => {
    const want = shapes(iconSvg(DEFAULT_ICON));
    expect(want.length).toBeGreaterThan(0);
    for (const [name, text] of [
      ['favicon.svg', favicon],
      ['waiting.html', landing],
      ['local.html', local],
    ] as const) {
      expect(shapes(text), name).toEqual(want);
    }
  });
});
