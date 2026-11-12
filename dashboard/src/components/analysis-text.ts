// 模型写的分析文本:不可信。只认 Markdown 的一个小子集 —— 标题、无序 / 有序列表、粗体、行内代码、
// 段落 —— 节点全部由 Lit 模板生成,文本只经文本绑定进入页面。HTML 标签、链接与图片语法都原样显示成文字。
import { html, type TemplateResult } from 'lit';

export type AnalysisBlock =
  | { kind: 'heading'; level: 3 | 4; text: string }
  | { kind: 'list'; ordered: boolean; start: number; items: string[] }
  | { kind: 'paragraph'; text: string };

const HEADING = /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
const BULLET = /^\s*[-*+]\s+(.*)$/;
const ORDERED = /^\s*(\d{1,9})[.)]\s+(.*)$/;

/** 按行切成块。空行结束段落与列表;列表项后面缩进的行接在这一项里。 */
export function parseAnalysis(text: string): AnalysisBlock[] {
  const blocks: AnalysisBlock[] = [];
  let para: string[] = [];
  let list: Extract<AnalysisBlock, { kind: 'list' }> | null = null;
  const flush = () => {
    if (para.length) blocks.push({ kind: 'paragraph', text: para.join('\n') });
    para = [];
    list = null;
  };
  for (const line of text.replace(/\r\n?/g, '\n').split('\n')) {
    if (!line.trim()) {
      flush();
      continue;
    }
    const heading = HEADING.exec(line);
    if (heading) {
      flush();
      blocks.push({ kind: 'heading', level: heading[1]!.length <= 2 ? 3 : 4, text: heading[2]! });
      continue;
    }
    const bullet = BULLET.exec(line);
    const ordered = bullet ? null : ORDERED.exec(line);
    if (bullet || ordered) {
      const isOrdered = !!ordered;
      if (para.length) flush();
      if (!list || list.ordered !== isOrdered) {
        list = { kind: 'list', ordered: isOrdered, start: ordered ? Number(ordered[1]) : 1, items: [] };
        blocks.push(list);
      }
      list.items.push(bullet ? bullet[1]! : ordered![2]!);
      continue;
    }
    if (list && /^\s/.test(line)) {
      list.items[list.items.length - 1] += `\n${line.trim()}`;
      continue;
    }
    list = null;
    para.push(line);
  }
  flush();
  return blocks;
}

/** 行内:**粗体** 与 `代码`;配不上的标记照原样留在文字里。 */
export function renderInline(text: string): Array<string | TemplateResult> {
  const out: Array<string | TemplateResult> = [];
  let buf = '';
  let i = 0;
  const push = (node: TemplateResult) => {
    if (buf) out.push(buf);
    buf = '';
    out.push(node);
  };
  while (i < text.length) {
    if (text[i] === '`') {
      const end = text.indexOf('`', i + 1);
      if (end > i + 1) {
        push(html`<code>${text.slice(i + 1, end)}</code>`);
        i = end + 1;
        continue;
      }
    }
    if (text.startsWith('**', i)) {
      const end = text.indexOf('**', i + 2);
      if (end > i + 2) {
        push(html`<strong>${renderInline(text.slice(i + 2, end))}</strong>`);
        i = end + 2;
        continue;
      }
    }
    buf += text[i];
    i += 1;
  }
  if (buf) out.push(buf);
  return out;
}

function block(b: AnalysisBlock): TemplateResult {
  switch (b.kind) {
    case 'heading':
      return b.level === 3 ? html`<h3>${renderInline(b.text)}</h3>` : html`<h4>${renderInline(b.text)}</h4>`;
    case 'list':
      return b.ordered
        ? html`<ol start=${b.start}>${b.items.map((item) => html`<li>${renderInline(item)}</li>`)}</ol>`
        : html`<ul>${b.items.map((item) => html`<li>${renderInline(item)}</li>`)}</ul>`;
    case 'paragraph':
      return html`<p>${renderInline(b.text)}</p>`;
  }
}

export function renderAnalysis(text: string): TemplateResult {
  return html`<div class="analysis-text">${parseAnalysis(text).map(block)}</div>`;
}
