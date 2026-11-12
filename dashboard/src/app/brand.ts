// 控制台的标识用 Menu Bar 当前选的那个 summon 图标:左上角与浏览器标签页图标都跟着
// `menubar.icon` 走。几何唯一来源是 menubar/Icons/*.svg,构建时整份打进来,不另画一份。
import { html, type TemplateResult } from 'lit';
import { unsafeSVG } from 'lit/directives/unsafe-svg.js';

/** 与 Menu Bar 的 `StatusIcons.defaultSummon` 一致:没配 `menubar.icon` 时用它。 */
export const DEFAULT_ICON = 'xyz';

const RAW = import.meta.glob('../../../menubar/Icons/summon-*.svg', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

// 源文件按模板图标画成纯黑;换成 currentColor 才能跟着深浅色走。
const ICONS = new Map(
  Object.entries(RAW).map(([path, svg]) => [
    path.replace(/^.*\/summon-/, '').replace(/\.svg$/, ''),
    svg.replaceAll('#000', 'currentColor'),
  ]),
);

export function iconSvg(name: string | null | undefined): string {
  return ICONS.get(name ?? '') ?? ICONS.get(DEFAULT_ICON) ?? '';
}

export function hasIcon(name: string): boolean {
  return ICONS.has(name);
}

/** 内容是构建期打进来的仓内 SVG,不是服务端字符串。 */
export function brandIcon(name: string | null | undefined, cls = 'brand-mark'): TemplateResult {
  return html`<span class=${cls} aria-hidden="true">${unsafeSVG(iconSvg(name))}</span>`;
}

// 标签页图标不随页面主题变,只能在 SVG 里按系统深浅色上色。
const FAVICON_STYLE = '<style>svg{color:#1c1c21}@media (prefers-color-scheme:dark){svg{color:#ececf1}}</style>';

let appliedFavicon = '';

export function applyFavicon(name: string | null | undefined): void {
  if (typeof document === 'undefined') return;
  const svg = iconSvg(name);
  if (!svg || svg === appliedFavicon) return;
  appliedFavicon = svg;
  const themed = svg.replace(/<svg([^>]*)>/, `<svg$1>${FAVICON_STYLE}`);
  let link = document.querySelector<HTMLLinkElement>('link[rel="icon"]');
  if (!link) {
    link = document.createElement('link');
    link.rel = 'icon';
    document.head.append(link);
  }
  link.type = 'image/svg+xml';
  link.href = `data:image/svg+xml,${encodeURIComponent(themed)}`;
}
