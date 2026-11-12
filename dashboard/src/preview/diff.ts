// 预览区块的比对。区块 = 消息、CoT 条目、带 element_id 的卡片元素;每块只比「自己的内容」,
// 带 id 的子块各自比。只在一边出现的块归到另一边仍在的最近父块。
import type { PreviewView } from '../api/types';

interface Block {
  own: string;
  parent: string | null;
}

type El = Record<string, unknown>;

function collect(view: PreviewView): Map<string, Block> {
  const out = new Map<string, Block>();
  // 把元素拆成「自己」与「带 id 的子元素」:子元素单独登记,自己的内容里不含它们。
  const own = (el: El, parent: string): El => {
    const id = typeof el.element_id === 'string' ? el.element_id : null;
    const next = id ?? parent;
    const copy: El = {};
    for (const [k, v] of Object.entries(el)) {
      if ((k === 'elements' || k === 'columns') && Array.isArray(v)) {
        copy[k] = (v as El[]).flatMap((child) => {
          const kept = own(child, next);
          return typeof child.element_id === 'string' ? [] : [kept];
        });
      } else copy[k] = v;
    }
    if (id) out.set(id, { own: JSON.stringify(copy), parent });
    return copy;
  };
  for (const m of view.messages) {
    if (m.kind === 'cot') {
      out.set(m.id, { own: JSON.stringify([m.kind, m.title, m.status]), parent: null });
      for (const it of m.items) out.set(it.id, { own: JSON.stringify(it), parent: m.id });
    } else if (m.kind === 'card') {
      const card = (m.card ?? {}) as { body?: { elements?: El[] }; elements?: El[] };
      const els = card.body?.elements ?? card.elements ?? [];
      const anon = els.flatMap((e) => {
        const kept = own(e, m.id);
        return typeof e.element_id === 'string' ? [] : [kept];
      });
      out.set(m.id, { own: JSON.stringify([m.kind, anon]), parent: null });
    } else {
      out.set(m.id, { own: JSON.stringify(m), parent: null });
    }
  }
  return out;
}

/** base 里与 alt 不同的区块(alt 独有的块归到它在 base 里最近的父块);父块已在内时不列子块。 */
export function regionDiff(base: PreviewView, alt: PreviewView): string[] {
  const a = collect(base);
  const b = collect(alt);
  const hit = new Set<string>();
  for (const [id, n] of a) {
    if (b.get(id)?.own !== n.own) hit.add(id);
  }
  for (const [id, n] of b) {
    if (a.has(id)) continue;
    let p = n.parent;
    while (p && !a.has(p)) p = b.get(p)?.parent ?? null;
    if (p) hit.add(p);
  }
  const underHit = (id: string) => {
    let p = a.get(id)?.parent ?? null;
    while (p) {
      if (hit.has(p)) return true;
      p = a.get(p)?.parent ?? null;
    }
    return false;
  };
  return [...hit].filter((id) => !underHit(id));
}

/** 新一次渲染相对上一次变了的区块(高亮用)。 */
export function changedBlocks(prev: PreviewView, next: PreviewView): string[] {
  return regionDiff(next, prev);
}
