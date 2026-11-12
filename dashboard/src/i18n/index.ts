import type { ReactiveController, ReactiveControllerHost } from 'lit';
import type chinese from '../../../locales/dashboard/zh-CN.json';

export type Language = 'zh-CN' | 'en';
type MessageKey = keyof typeof chinese;
type Catalog = Readonly<Record<string, string>>;

// 每种语言一个按需加载的分块:页面只下载正在用的那一份,切换语言时才取另一份。两份语言包的 key 由
// i18n.test.ts 钉成一致。
const LOADERS: Record<Language, () => Promise<{ default: Catalog }>> = {
  'zh-CN': () => import('../../../locales/dashboard/zh-CN.json'),
  en: () => import('../../../locales/dashboard/en.json'),
};
const loaded = new Map<Language, Catalog>();

async function load(next: Language): Promise<Catalog> {
  const cached = loaded.get(next);
  if (cached) return cached;
  const catalog = (await LOADERS[next]()).default;
  loaded.set(next, catalog);
  return catalog;
}

let current: Language = typeof document !== 'undefined' && document.documentElement.lang === 'en' ? 'en' : 'zh-CN';
// 顶层 await:引用本模块的代码在首屏语言就位之后才执行,t() 与 rich() 因此保持同步。
let messages: Catalog = await load(current);
let switching = 0;
/** 正在加载、尚未生效的那次切换 */
let pending: { next: Language; result: Promise<boolean> } | null = null;
export const languageEvents = new EventTarget();

export function language(): Language { return current; }

/**
 * 换语言;另一份语言包取不到时保持现状、返回 false,下一次状态轮询会再试。
 * 切到正在加载的那一种(手动刷新与定时轮询先后报同一种语言)时一起等它,不作废它;切回当前语言时
 * 作废在途的切换。返回 true 表示语言随之变了;一起等的调用拿到同一个结果。
 */
export function setLanguage(next: Language): Promise<boolean> {
  if (pending?.next === next) return pending.result;
  if (next === current) {
    if (pending) {
      switching += 1;
      pending = null;
    }
    return Promise.resolve(false);
  }
  const ticket = ++switching;
  const result = (async () => {
    let catalog: Catalog;
    try {
      catalog = await load(next);
    } catch {
      return false;
    }
    if (ticket !== switching) return false;
    messages = catalog;
    current = next;
    if (typeof document !== 'undefined') document.documentElement.lang = next;
    languageEvents.dispatchEvent(new Event('change'));
    return true;
  })();
  const entry = { next, result };
  pending = entry;
  void result.finally(() => {
    if (pending === entry) pending = null;
  });
  return result;
}

function message(key: MessageKey): string {
  return messages[key] ?? `[${key}]`;
}

export function t(key: MessageKey, args: readonly unknown[] = []): string {
  return message(key).replace(/\{(\d+)\}/g, (_, index: string) => String(args[Number(index)]));
}

/** Preserve Lit values and user text as values; translated text never becomes markup. */
export function rich(key: MessageKey, args: readonly unknown[] = []): unknown[] {
  const source = message(key);
  const parts: unknown[] = [];
  let cursor = 0;
  for (const match of source.matchAll(/\{(\d+)\}/g)) {
    parts.push(source.slice(cursor, match.index), args[Number(match[1])]);
    cursor = match.index! + match[0].length;
  }
  parts.push(source.slice(cursor));
  return parts;
}

/** Language changes rerender an existing element, retaining its drafts and focus. */
export class LocaleController implements ReactiveController {
  constructor(private readonly host: ReactiveControllerHost) { host.addController(this); }
  private readonly changed = () => this.host.requestUpdate();
  hostConnected(): void { languageEvents.addEventListener('change', this.changed); }
  hostDisconnected(): void { languageEvents.removeEventListener('change', this.changed); }
}
