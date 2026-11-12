import { t as translate, rich } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import { live } from 'lit/directives/live.js';
import { errorMessage } from '../api/client';
import type { ScheduleView } from '../api/types';
import { hrefFor } from '../app/router';
import { store } from '../app/store';
import { segmented } from '../components/controls';
import { icon } from '../components/icon';
import { emptyState, errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { editScheduleDialog, scheduleRow, toggleSchedule, type ScheduleRuns } from '../components/schedule';

const PAGE_SIZE = 6;

export type GroupBy = 'agent' | 'chat' | 'kind';

const GROUP_BY: Array<{ value: GroupBy; label: string }> = [
  { value: 'agent', label: 'Agent' },
  { value: 'chat', get label() { return translate('d35b49e'); } },
  { value: 'kind', get label() { return translate('d5c428f'); } },
];
const GROUP_BY_KEY = 'summon-dash.schedules.group-by';

function loadGroupBy(): GroupBy {
  try {
    const v = localStorage.getItem(GROUP_BY_KEY);
    if (v === 'agent' || v === 'chat' || v === 'kind') return v;
  } catch {
    // 隐私窗口等读不到存储时用默认分组
  }
  return 'agent';
}

function saveGroupBy(v: GroupBy): void {
  try {
    localStorage.setItem(GROUP_BY_KEY, v);
  } catch {
    // 记不住只影响下次打开的默认分组
  }
}

export interface SubGroup {
  key: string;
  title: string;
  href?: string;
  items: ScheduleView[];
}

export interface Group {
  key: string;
  title: string;
  /** 标题旁的等宽小字(agent 别名) */
  aside?: string;
  avatar: string;
  subs: SubGroup[];
}

const KIND_GROUPS = [
  { key: 'once', get title() { return translate('d3de42d'); }, kinds: ['once'] },
  { key: 'repeat', get title() { return translate('dad3221'); }, kinds: ['interval', 'cron'] },
];

/** 两层分组:按 agent 时下一层是群,按群时下一层是 agent;按单次 / 循环时不再分层。 */
export function groupSchedules(list: ScheduleView[], by: GroupBy, display: (alias: string) => string): Group[] {
  const chatName = (s: ScheduleView) => s.chat_name ?? s.chat_id;
  const nest = (
    outer: (s: ScheduleView) => string,
    inner: (s: ScheduleView) => string,
    head: (s: ScheduleView) => Omit<Group, 'subs'>,
    sub: (s: ScheduleView) => Omit<SubGroup, 'items'>,
  ): Group[] => {
    const groups = new Map<string, Group>();
    for (const s of list) {
      const g = groups.get(outer(s)) ?? { ...head(s), subs: [] };
      groups.set(outer(s), g);
      let sg = g.subs.find((x) => x.key === inner(s));
      if (!sg) g.subs.push((sg = { ...sub(s), items: [] }));
      sg.items.push(s);
    }
    return [...groups.values()];
  };
  switch (by) {
    case 'agent':
      return nest(
        (s) => s.agent,
        (s) => s.chat_id,
        (s) => ({ key: s.agent, title: display(s.agent), aside: s.agent, avatar: display(s.agent) }),
        (s) => ({ key: s.chat_id, title: chatName(s), href: hrefFor({ page: 'chat', chatId: s.chat_id, agent: s.agent }) }),
      );
    case 'chat':
      return nest(
        (s) => s.chat_id,
        (s) => s.agent,
        (s) => ({ key: s.chat_id, title: chatName(s), avatar: chatName(s) }),
        (s) => ({ key: s.agent, title: display(s.agent), href: hrefFor({ page: 'chat', chatId: s.chat_id, agent: s.agent }) }),
      );
    case 'kind':
      return KIND_GROUPS.map((k) => ({
        key: k.key,
        title: k.title,
        avatar: k.title,
        subs: [{ key: k.key, title: '', items: list.filter((s) => k.kinds.includes(s.kind)) }],
      })).filter((g) => g.subs[0]!.items.length);
  }
}

class SchedulesPage extends PageBase {
  list: ScheduleView[] = [];
  query = '';
  page = 0;
  busy = new Set<string>();
  runs = new Map<string, ScheduleRuns>();
  groupBy: GroupBy = loadGroupBy();

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
  }

  protected override watchKeys(): string[] {
    return ['schedules'];
  }

  async refresh(): Promise<void> {
    try {
      this.list = await this.call('schedules.list', {});
      this.loadError = '';
    } catch (err) {
      this.loadError = errorMessage(err);
    }
    this.loading = false;
    this.requestUpdate();
  }

  private replace(v: ScheduleView) {
    this.list = this.list.map((x) => (x.id === v.id ? v : x));
    this.requestUpdate();
  }

  private async toggle(s: ScheduleView, on: boolean) {
    this.busy.add(s.id);
    this.requestUpdate();
    const v = await toggleSchedule(this.call, s, on);
    this.busy.delete(s.id);
    if (v) this.replace(v);
    else this.requestUpdate();
    await store.refreshStatus(true);
  }

  private async edit(s: ScheduleView) {
    const v = await editScheduleDialog(this.call, s);
    if (v) {
      this.replace(v);
      await store.refreshStatus(true);
    }
  }

  /** 展开「最近执行」时从台账取这条任务最近 10 轮;取过的留着,收起再展开不重取。 */
  private async toggleRuns(s: ScheduleView, open: boolean) {
    const cur = this.runs.get(s.id);
    const need = open && !cur?.turns;
    this.runs.set(s.id, { open, loading: need, ...(cur?.turns ? { turns: cur.turns } : {}) });
    this.requestUpdate();
    if (!need) return;
    const keep = () => this.runs.get(s.id)?.open ?? open;
    try {
      const page = await this.call('turns.list', { agent: s.agent, schedule: s.id, limit: 10 });
      this.runs.set(s.id, { open: keep(), loading: false, turns: page.turns });
    } catch (err) {
      this.runs.set(s.id, { open: keep(), loading: false, error: errorMessage(err) });
    }
    this.requestUpdate();
  }

  private display(alias: string): string {
    return store.state.status?.agents.find((a) => a.alias === alias)?.display || alias;
  }

  private rows(items: ScheduleView[], showChat = false, showAgent = false): TemplateResult {
    return html`${items.map((s) =>
      scheduleRow(s, {
        busy: this.busy.has(s.id),
        showChat,
        ...(this.runs.has(s.id) ? { runs: this.runs.get(s.id)! } : {}),
        onRuns: (x, open) => void this.toggleRuns(x, open),
        ...(showAgent ? { agentLabel: this.display(s.agent) } : {}),
        onToggle: (x, on) => this.toggle(x, on),
        onEdit: (x) => this.edit(x),
      }),
    )}`;
  }

  private active(): TemplateResult {
    const active = this.list.filter((s) => s.enabled);
    if (!active.length) {
      return html`<div class="card">${emptyState(translate('dc4869e'), translate('d40bcc0'))}</div>`;
    }
    const flat = this.groupBy === 'kind';
    return html`${groupSchedules(active, this.groupBy, (a) => this.display(a)).map(
      (g) => html`<section class="sched-group" aria-labelledby=${`sg-${g.key}`}>
        <header class="sched-group-head">
          <span class="avatar" aria-hidden="true">${[...g.avatar][0]}</span>
          <h2 id=${`sg-${g.key}`}>${g.title}${g.aside ? html` <span class="mono muted">${g.aside}</span>` : nothing}</h2>
          ${flat ? html`<span class="muted count">${rich('d376dc4', [g.subs[0]!.items.length])}</span>` : nothing}
        </header>
        ${g.subs.map((c) =>
          flat
            ? html`<div class="card">${this.rows(c.items, true, true)}</div>`
            : html`<div class="sched-chat">
                <div class="sched-chat-head">
                  <a class="link-btn quiet" href=${c.href ?? '#'}>${c.title}</a>
                  <span class="muted count">${rich('d376dc4', [c.items.length])}</span>
                </div>
                <div class="card">${this.rows(c.items)}</div>
              </div>`,
        )}
      </section>`,
    )}`;
  }

  private archived(): TemplateResult | typeof nothing {
    const all = this.list.filter((s) => !s.enabled);
    if (!all.length) return nothing;
    const q = this.query.trim().toLowerCase();
    const hits = q
      ? all.filter((s) => [s.id, s.prompt, s.human, s.chat_name ?? '', s.agent, this.display(s.agent)].join(' ').toLowerCase().includes(q))
      : all;
    const pages = Math.max(1, Math.ceil(hits.length / PAGE_SIZE));
    const page = Math.min(this.page, pages - 1);
    const shown = hits.slice(page * PAGE_SIZE, page * PAGE_SIZE + PAGE_SIZE);
    return html`<details class="archive">
      <summary>${rich('d672be4', [icon('chevronRight', 'icon-chev')])} <span class="muted count">${rich('d376dc4', [all.length])}</span></summary>
      <div class="archive-body">
        <p class="muted archive-note">${rich('dab8dbc')}</p>
        <div class="toolbar archive-tools">
          <label class="search">
            ${icon('search')}
            <input
              class="field-input"
              type="search"
              placeholder="${translate('d8b7e6e')}"
              aria-label="${translate('d89c67b')}"
              .value=${live(this.query)}
              @input=${(e: Event) => {
                this.query = (e.target as HTMLInputElement).value;
                this.page = 0;
                this.requestUpdate();
              }}
            />
          </label>
        </div>
        ${hits.length
          ? html`<div class="card">
              ${this.rows(shown, true)}
              ${pages > 1
                ? html`<div class="pager">
                    <span class="tnum">${rich('d4182cb', [page + 1, pages, hits.length])}</span>
                    <div class="pager-btns">
                      <button class="btn btn-sm" ?disabled=${page === 0} @click=${() => this.setPage(page - 1)} aria-label="${translate('dc9b9ae')}">
                        ${icon('chevronLeft', 'icon-sm')}
                      </button>
                      <button class="btn btn-sm" ?disabled=${page >= pages - 1} @click=${() => this.setPage(page + 1)} aria-label="${translate('d8a8542')}">
                        ${icon('chevronRight', 'icon-sm')}
                      </button>
                    </div>
                  </div>`
                : nothing}
            </div>`
          : html`<div class="card">${emptyState(translate('d7791c0'), translate('d5a7c1c'))}</div>`}
      </div>
    </details>`;
  }

  private setPage(p: number) {
    this.page = p;
    this.requestUpdate();
  }

  protected override render(): TemplateResult {
    return html`<div class="page">
      ${pageHead(
        translate('d3c39e9'),
        translate('d48250b'),
        segmented({
          label: translate('d72148c'),
          size: 'sm',
          value: this.groupBy,
          options: GROUP_BY,
          onChange: (v) => {
            this.groupBy = v as GroupBy;
            saveGroupBy(this.groupBy);
            this.requestUpdate();
          },
        }),
      )}
      ${this.loadError ? errorBanner(translate('d494c50'), this.loadError, () => this.refresh()) : nothing}
      ${this.loading ? skeletonCard(4) : html`${this.active()}${this.archived()}`}
    </div>`;
  }
}

customElements.define('sm-page-schedules', SchedulesPage);
