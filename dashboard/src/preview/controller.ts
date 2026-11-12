import { errorMessage } from '../api/client';
import type { ConfigChange, PreviewView, Scope } from '../api/types';
import type { Caller } from '../app/editable';
import { changedBlocks } from './diff';

export const PREVIEW_DEBOUNCE_MS = 150;
export const FLASH_MS = 600;

/**
 * 预览的取数与状态:草稿一变就去抖请求一次 preview.render,结果原地替换;与上一次相比
 * 变了的区块高亮 600ms。鼠标停在哪一行,就描出这一行管的区块(区块由服务端算好)。
 */
export class PreviewController extends EventTarget {
  view: PreviewView | null = null;
  error = '';
  changed = new Set<string>();
  focusKey: string | null = null;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private flash: ReturnType<typeof setTimeout> | undefined;
  private seq = 0;
  private quiet = false;

  constructor(
    private readonly call: Caller,
    public scope: Scope | null,
    private readonly draft: () => ConfigChange[],
    readonly delay = PREVIEW_DEBOUNCE_MS,
  ) {
    super();
  }

  private emit(): void {
    this.dispatchEvent(new Event('change'));
  }

  /** 草稿变了:去抖后请求。 */
  schedule(): void {
    if (this.timer) clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      this.timer = undefined;
      void this.request();
    }, this.delay);
  }

  async request(): Promise<void> {
    if (!this.scope) return;
    const seq = ++this.seq;
    try {
      const view = await this.call('preview.render', { scope: this.scope, draft: this.draft() });
      if (seq !== this.seq) return;
      const prev = this.view;
      this.view = view;
      this.error = '';
      if (prev && !this.quiet) this.highlight(changedBlocks(prev, view));
      this.quiet = false;
      // 区块表变了:设置行上的预览标记要跟着刷新(悬停这类高频变化只发 change)。
      this.dispatchEvent(new Event('view'));
    } catch (err) {
      if (seq !== this.seq) return;
      this.error = errorMessage(err);
    }
    this.emit();
  }

  private highlight(ids: string[]): void {
    if (this.flash) clearTimeout(this.flash);
    this.changed = new Set(ids);
    if (!ids.length) return;
    this.flash = setTimeout(() => {
      this.changed = new Set();
      this.emit();
    }, FLASH_MS);
  }

  setScope(scope: Scope): void {
    if (this.scope && JSON.stringify(this.scope) === JSON.stringify(scope)) return;
    this.scope = scope;
    this.quiet = true;
    void this.request();
  }

  setFocus(key: string | null): void {
    if (key === this.focusKey) return;
    this.focusKey = key;
    this.emit();
  }

  regions(key: string): string[] {
    return this.view?.regions[key] ?? [];
  }

  hasRegion(key: string): boolean {
    return this.regions(key).length > 0;
  }

  /** 给某个区块的类名:描边(悬停的那一行管它)与高亮(刚变过)。 */
  readonly mark = (id: string): string => {
    const cls: string[] = [];
    if (this.focusKey && this.regions(this.focusKey).includes(id)) cls.push('is-region');
    if (this.changed.has(id)) cls.push('is-changed');
    return cls.join(' ');
  };

  dispose(): void {
    if (this.timer) clearTimeout(this.timer);
    if (this.flash) clearTimeout(this.flash);
    this.seq++;
  }
}
