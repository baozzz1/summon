import { t as translate, rich } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import { errorMessage } from '../api/client';
import type { OutsideUsage, SessionAction, SessionDetail, TurnRecord } from '../api/types';
import { hrefFor, type Route } from '../app/router';
import { store } from '../app/store';
import { ident } from '../components/ident';
import { icon } from '../components/icon';
import { emptyState, errorBanner, pageHead, PageBase, skeletonCard } from '../components/page';
import { outcomeText, runSessionAction, sessionActionButtons } from '../components/session-actions';
import { toast } from '../components/toast';
import { END_LABEL, ORIGIN_LABEL, turnSeconds } from '../components/turn';
import { fmtDateTime, fmtDuration } from '../util/format';
import { spendText, spendTitle } from '../util/spend';
import { sessionMeta, STATE_LABEL } from './sessions';

const POLL_MS = 5000;

type SessionRoute = Extract<Route, { page: 'session' }>;

function iso(ms: number): string {
  return new Date(ms).toISOString();
}

const outsideKey = (o: OutsideUsage) => `${o.from_ms}-${o.to_ms}`;

/** 合并两份轮外用量:同一段只留一份,新的在前。 */
export function mergeOutside(a: OutsideUsage[], b: OutsideUsage[]): OutsideUsage[] {
  const seen = new Map<string, OutsideUsage>();
  for (const o of [...a, ...b]) if (!seen.has(outsideKey(o))) seen.set(outsideKey(o), o);
  return [...seen.values()].sort((x, y) => y.to_ms - x.to_ms);
}

type Entry = { at: number; turn: TurnRecord } | { at: number; outside: OutsideUsage };

/** 时间线:轮按结束时刻、轮外用量按最后一次请求的时刻,新的在前。 */
export function timeline(turns: TurnRecord[], outside: OutsideUsage[]): Entry[] {
  const entries: Entry[] = [
    ...turns.map((turn) => ({ at: turn.ended_at_ms, turn })),
    ...outside.map((o) => ({ at: o.to_ms, outside: o })),
  ];
  return entries.sort((a, b) => b.at - a.at);
}

class SessionPage extends PageBase {
  detail: SessionDetail | null = null;
  turns: TurnRecord[] = [];
  outside: OutsideUsage[] = [];
  more = false;
  loadingMore = false;
  acting: SessionAction | undefined;
  private timer: ReturnType<typeof setInterval> | undefined;

  private get target(): SessionRoute {
    return this.route as SessionRoute;
  }

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
    this.timer = setInterval(() => {
      if (document.visibilityState === 'visible') void this.poll();
    }, POLL_MS);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    clearInterval(this.timer);
  }

  async refresh(): Promise<void> {
    try {
      const d = await this.call('sessions.get', { agent: this.target.agent, chat_id: this.target.chatId });
      this.detail = d;
      this.turns = d.turns;
      this.outside = d.outside ?? [];
      this.more = d.more;
      this.loadError = '';
    } catch (err) {
      this.loadError = errorMessage(err);
    }
    this.loading = false;
    this.requestUpdate();
  }

  /** 定时刷新:头部换成最新状态,新跑完的轮接到最前面,翻出来的更早的轮留着。 */
  private async poll(): Promise<void> {
    try {
      const d = await this.call('sessions.get', { agent: this.target.agent, chat_id: this.target.chatId });
      const newest = this.turns[0]?.ended_at_ms ?? 0;
      this.turns = [...d.turns.filter((t) => t.ended_at_ms > newest), ...this.turns];
      this.outside = mergeOutside(d.outside ?? [], this.outside);
      this.detail = d;
      this.requestUpdate();
    } catch {
      // 下一拍再试;手动刷新会把错误显示出来
    }
  }

  private async loadMore(): Promise<void> {
    const last = this.turns[this.turns.length - 1];
    if (!last) return;
    this.loadingMore = true;
    this.requestUpdate();
    try {
      const page = await this.call('turns.list', {
        agent: this.target.agent,
        chat_id: this.target.chatId,
        before: last.ended_at_ms,
      });
      this.turns = [...this.turns, ...page.turns];
      this.outside = mergeOutside(this.outside, page.outside ?? []);
      this.more = page.more;
    } catch (err) {
      toast(translate('d7f8446', [errorMessage(err)]), 'error');
    }
    this.loadingMore = false;
    this.requestUpdate();
  }

  private async act(action: SessionAction) {
    const row = this.detail?.session;
    if (!row) return;
    this.acting = action;
    this.requestUpdate();
    const outcome = await runSessionAction(this.call, row, action);
    if (outcome) {
      const { text, kind } = outcomeText(action, outcome);
      toast(text, kind);
    }
    this.acting = undefined;
    await this.refresh();
  }

  private origin(t: TurnRecord): TemplateResult {
    if (t.origin === 'schedule' && t.schedule) {
      return html`<a href=${hrefFor({ page: 'schedules' })}>${ORIGIN_LABEL.schedule} <span class="mono">${t.schedule}</span></a>`;
    }
    if ((t.origin === 'human' || t.origin === 'bot') && t.requester) {
      return ident(t.requester, { agent: t.agent });
    }
    return html`${ORIGIN_LABEL[t.origin]}`;
  }

  private turn(t: TurnRecord): TemplateResult {
    const end = END_LABEL[t.end];
    const secs = turnSeconds(t);
    return html`<div class="turn-row" data-end=${t.end}>
      <div class="turn-when">
        <span>${fmtDateTime(iso(t.ended_at_ms))}</span>
        <span class="muted">${secs ? fmtDuration(secs) : translate('d33a1e1')}</span>
      </div>
      <div class="turn-what">
        <div class="turn-line">
          <span class="turn-origin">${this.origin(t)}</span>
          ${t.prompt_head ? html`<span class="turn-prompt">${t.prompt_head}</span>` : nothing}
        </div>
        ${t.feed ? html`<div class="turn-feed">${icon('check', 'icon-xs')}${t.feed}</div>` : nothing}
        <div class="turn-facts">
          <span class="chip ${end.tone ? `chip-${end.tone}` : 'chip-quiet'}">${end.text}${t.error ? ` · ${t.error}` : ''}</span>
          ${t.tool_steps ? html`<span>${rich('d309b29', [t.tool_steps])}</span>` : nothing}
          ${t.model ? html`<span class="mono">${t.model}</span>` : nothing}
          ${t.effort ? html`<span>${t.effort}</span>` : nothing}
          ${t.ctx?.pct !== undefined ? html`<span>${rich('dc00e83', [t.ctx.pct])}</span>` : nothing}
          ${t.spend ? html`<span title=${spendTitle(t.spend)}>${spendText(t.spend)}</span>` : nothing}
          ${t.reattached ? html`<span title="${translate('de35785')}">${rich('d342a95')}</span>` : nothing}
          ${t.end === 'failed' && store.state.whoami?.machine
            ? html`<a
                class="link-btn"
                href=${hrefFor({ page: 'logs', agent: t.agent, since: iso(t.started_at_ms - 60_000), until: iso(t.ended_at_ms + 60_000) })}
                >${rich('d151662', [icon('chevronRight', 'icon-sm')])}</a
              >`
            : nothing}
        </div>
      </div>
    </div>`;
  }

  private outsideRow(o: OutsideUsage): TemplateResult {
    const span = o.to_ms > o.from_ms ? translate('d62f86d', [fmtDateTime(iso(o.from_ms))]) : '';
    return html`<div class="turn-row turn-outside">
      <div class="turn-when">
        <span>${fmtDateTime(iso(o.to_ms))}</span>
        ${span ? html`<span class="muted">${span}</span>` : nothing}
      </div>
      <div class="turn-what">
        <div class="turn-line">
          <span class="turn-origin">${rich('dc3043a')}</span>
          <span class="turn-prompt">${rich('d63714a')}</span>
        </div>
        <div class="turn-facts">
          ${o.spend ? html`<span title=${spendTitle(o.spend)}>${spendText(o.spend)}</span>` : nothing}
        </div>
      </div>
    </div>`;
  }

  private header(d: SessionDetail): TemplateResult {
    const s = d.session;
    const last = this.turns[0];
    return html`<div class="card card-pad sess-head">
      <div class="sess-head-line">
        ${s
          ? html`<span class="chip">${STATE_LABEL[s.state]}</span><span>${sessionMeta(s)}</span>`
          : html`<span class="chip chip-quiet">${rich('d494cf1')}</span>`}
        ${s?.cli ? html`<span class="muted">${s.cli}</span>` : nothing}
        ${last?.ctx?.pct !== undefined ? html`<span class="muted">${rich('d0d04f6', [last.ctx.pct])}</span>` : nothing}
        ${last?.quota?.five_hour_left !== undefined
          ? html`<span class="muted">${rich('da8f633', [last.quota.five_hour_left])}</span>`
          : nothing}
        ${last?.quota?.long_left !== undefined ? html`<span class="muted">${rich('d2c6b8d', [last.quota.long_left])}</span>` : nothing}
      </div>
      ${s?.deny_reason && !Object.values(s.actions).some(Boolean)
        ? html`<p class="row-note">${icon('lock', 'icon-sm')}<span>${s.deny_reason}</span></p>`
        : nothing}
      <div class="sess-head-links">
        <a class="btn btn-sm btn-ghost" href=${hrefFor({ page: 'chat', chatId: d.chat_id, agent: d.agent })}
          >${rich('d54c924', [icon('slidersHorizontal', 'icon-sm')])}</a
        >
        ${s?.link ? html`<a class="btn btn-sm btn-ghost" href=${s.link}>${rich('dceb559', [icon('externalLink', 'icon-sm')])}</a>` : nothing}
      </div>
    </div>`;
  }

  protected override render(): TemplateResult {
    const d = this.detail;
    const title = d?.name || (d ? ident(d.chat_id, { agent: d.agent }) : '');
    const sub = d ? `${d.session?.agent_display ?? d.agent} · ${d.chat_id}` : '';
    const buttons = d?.session ? sessionActionButtons(d.session, this.acting, (a) => void this.act(a)) : nothing;
    const head = pageHead(title || translate('da63280'), sub, buttons === nothing ? undefined : buttons, {
      href: hrefFor({ page: 'sessions' }),
      label: translate('da63280'),
    });
    if (this.loading) return html`<div class="page">${head}${skeletonCard(4)}</div>`;
    if (this.loadError) {
      return html`<div class="page">${head}${errorBanner(translate('dc11775'), this.loadError, () => this.refresh())}</div>`;
    }
    if (!d) return html`<div class="page">${head}</div>`;
    return html`<div class="page">
      ${head} ${this.header(d)}
      <section class="group" aria-labelledby="turns-head">
        <header class="group-head">
          <div>
            <h2 id="turns-head">${rich('d6e55e0')}</h2>
            <p>${rich('d0f01f5')}</p>
          </div>
        </header>
        ${this.turns.length || this.outside.length
          ? html`<div class="card">
                ${timeline(this.turns, this.outside).map((e) => ('turn' in e ? this.turn(e.turn) : this.outsideRow(e.outside)))}
              </div>
              ${this.more
                ? html`<div class="more-row">
                    <button class="btn btn-sm" ?disabled=${this.loadingMore} @click=${() => this.loadMore()}>
                      ${this.loadingMore ? translate('d4927a5') : translate('d5164b0')}
                    </button>
                  </div>`
                : nothing}`
          : html`<div class="card">${emptyState(translate('d6c3df5'), translate('d5bafc7'))}</div>`}
      </section>
    </div>`;
  }
}

customElements.define('sm-page-session', SessionPage);
