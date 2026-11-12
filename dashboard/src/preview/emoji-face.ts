// 页面上画一个飞书表情:有飞书官方图就用它(由控制台同源提供,见 daemon 的 `dashboard::emoji`),图到之前
// 和取不到时显示近似的系统 emoji,再没有就写文字。
import { html, type TemplateResult } from 'lit';
import { keyed } from 'lit/directives/keyed.js';
import { emojiFor } from './emoji';
import table from './feishu-emoji.json';

const WITH_IMAGE = new Set(Object.keys(table));

/** 这个表情画得出图形(飞书原图或近似 emoji),否则只能写文字。 */
export function hasEmojiFace(name: string): boolean {
  return WITH_IMAGE.has(name) || emojiFor(name) !== null;
}

/** `text` 是没有图形时写的字(通常是含义)。外层自己标 aria,这里只管样子。 */
export function emojiFace(name: string, text: string): TemplateResult {
  const glyph = emojiFor(name);
  const fallback = glyph
    ? html`<span class="emo-glyph">${glyph}</span>`
    : html`<span class="emo-word">${text || name}</span>`;
  if (!WITH_IMAGE.has(name)) return fallback;
  // 换了表情就换一个新节点:上一张图的加载状态不能带到下一张上。
  return html`${keyed(
    name,
    html`<span class="emo-face"
      ><img
        class="emo-img"
        src=${`/emoji/${encodeURIComponent(name)}`}
        alt=""
        draggable="false"
        loading="lazy"
        decoding="async"
        @load=${(e: Event) => (e.currentTarget as HTMLElement).parentElement?.setAttribute('data-loaded', '')}
      />${fallback}</span
    >`,
  )}`;
}
