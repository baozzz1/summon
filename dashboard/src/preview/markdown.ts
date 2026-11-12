import { t as translate } from '../i18n';
// 飞书卡片 markdown(lark_md)的受限子集:粗体、斜体、删除线、行内代码、代码块、列表、
// 链接文字、<font color>、<at>。全部用 DOM API 构造节点;服务端字符串只经 createTextNode
// 进入页面,认不出的标签原样当文字显示。链接只留文字、不可点。

const FONT_COLORS = new Set(['grey', 'gray', 'red', 'green', 'orange', 'blue']);

function span(doc: Document, cls: string): HTMLElement {
  const el = doc.createElement('span');
  el.className = cls;
  return el;
}

const RE_FONT = /^<font\s+color\s*=\s*['"]?([a-zA-Z-]+)['"]?\s*>/;
const RE_AT = /^<at\s+(?:id|user_id|open_id|email)\s*=\s*['"]?([^'">\s]*)['"]?\s*>([^<]*)<\/at>/;
const RE_LINK = /^\[([^\]\n]+)\]\(([^)\s]+)\)/;

/** 在 text[from..] 里找与开标记配对的闭标记;只在同一行里配,找不到返回 -1。 */
function closing(text: string, from: number, mark: string): number {
  const nl = text.indexOf('\n', from);
  const end = nl < 0 ? text.length : nl;
  const at = text.indexOf(mark, from);
  return at >= 0 && at < end ? at : -1;
}

export function renderInline(text: string, parent: Node, doc: Document = document): void {
  let buf = '';
  const flush = () => {
    if (buf) parent.appendChild(doc.createTextNode(buf));
    buf = '';
  };
  let i = 0;
  while (i < text.length) {
    const rest = text.slice(i);
    const c = text[i]!;
    if (c === '`') {
      const end = closing(text, i + 1, '`');
      if (end > i + 1) {
        flush();
        const code = span(doc, 'fs-code');
        code.textContent = text.slice(i + 1, end);
        parent.appendChild(code);
        i = end + 1;
        continue;
      }
    }
    if (rest.startsWith('**') || rest.startsWith('~~')) {
      const mark = rest.slice(0, 2);
      const end = closing(text, i + 2, mark);
      if (end > i + 2) {
        flush();
        const el = doc.createElement(mark === '**' ? 'strong' : 's');
        renderInline(text.slice(i + 2, end), el, doc);
        parent.appendChild(el);
        i = end + 2;
        continue;
      }
    }
    // 单个 * 从严认斜体:开标记前面不能是字母数字或 / . *,后面不能是空格;闭标记前面不能是
    // 空格,后面不能是字母数字或 *。crates/**/*.rs、2*3 这类正文因此原样保留(误删比漏删更伤)。
    if (c === '*' && !/[A-Za-z0-9_/.*\\]/.test(text[i - 1] ?? '') && text[i + 1] && !/[\s*]/.test(text[i + 1]!)) {
      const end = closing(text, i + 1, '*');
      if (end > i + 1 && !/\s/.test(text[end - 1]!) && !/[A-Za-z0-9_*]/.test(text[end + 1] ?? '')) {
        flush();
        const el = doc.createElement('em');
        renderInline(text.slice(i + 1, end), el, doc);
        parent.appendChild(el);
        i = end + 1;
        continue;
      }
    }
    if (c === '[') {
      const m = RE_LINK.exec(rest);
      if (m) {
        flush();
        const el = span(doc, 'fs-link');
        renderInline(m[1]!, el, doc);
        parent.appendChild(el);
        i += m[0].length;
        continue;
      }
    }
    if (c === '<') {
      const font = RE_FONT.exec(rest);
      if (font) {
        const close = text.indexOf('</font>', i + font[0].length);
        if (close >= 0) {
          flush();
          const color = font[1]!.toLowerCase();
          const el = span(doc, FONT_COLORS.has(color) ? `fs-c-${color === 'gray' ? 'grey' : color}` : '');
          renderInline(text.slice(i + font[0].length, close), el, doc);
          parent.appendChild(el);
          i = close + '</font>'.length;
          continue;
        }
      }
      const at = RE_AT.exec(rest);
      if (at) {
        flush();
        const el = span(doc, 'fs-at');
        const id = at[1] ?? '';
        const name = (at[2] ?? '').trim();
        el.textContent = `@${name || (id === 'all' ? translate('d06d9ae') : id || translate('d61edb9'))}`;
        parent.appendChild(el);
        i += at[0].length;
        continue;
      }
    }
    buf += c;
    i++;
  }
  flush();
}

/** 整段 markdown → 片段:段落、列表、代码块。 */
export function renderMarkdown(src: string, doc: Document = document): DocumentFragment {
  const frag = doc.createDocumentFragment();
  const lines = src.replace(/\r\n?/g, '\n').split('\n');
  let para: string[] = [];
  let list: { el: HTMLElement; ordered: boolean } | null = null;
  const flushPara = () => {
    if (!para.length) return;
    const p = doc.createElement('p');
    para.forEach((line, idx) => {
      if (idx) p.appendChild(doc.createElement('br'));
      renderInline(line, p, doc);
    });
    frag.appendChild(p);
    para = [];
  };
  const endList = () => {
    list = null;
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;
    if (line.trimStart().startsWith('```')) {
      flushPara();
      endList();
      const body: string[] = [];
      i++;
      while (i < lines.length && !lines[i]!.trimStart().startsWith('```')) body.push(lines[i++]!);
      const pre = doc.createElement('pre');
      pre.textContent = body.join('\n');
      frag.appendChild(pre);
      continue;
    }
    const bullet = /^\s*[-*+]\s+(.*)$/.exec(line);
    const ordered = /^\s*\d+[.)]\s+(.*)$/.exec(line);
    if (bullet || ordered) {
      flushPara();
      const isOrdered = !bullet;
      if (!list || list.ordered !== isOrdered) {
        list = { el: doc.createElement(isOrdered ? 'ol' : 'ul'), ordered: isOrdered };
        frag.appendChild(list.el);
      }
      const li = doc.createElement('li');
      renderInline((bullet ?? ordered)![1]!, li, doc);
      list.el.appendChild(li);
      continue;
    }
    if (!line.trim()) {
      flushPara();
      endList();
      continue;
    }
    endList();
    para.push(line);
  }
  flushPara();
  return frag;
}
