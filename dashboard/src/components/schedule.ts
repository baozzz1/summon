import { t as translate, rich } from '../i18n';
// 定时任务的一行与编辑对话框。「定时任务」页与群详情里「本群定时任务」共用。
import { html, nothing, type TemplateResult } from 'lit';
import { live } from 'lit/directives/live.js';
import { ApiCallError, errorMessage } from '../api/client';
import type { ScheduleEdit, ScheduleView, TurnRecord, WhenCheck } from '../api/types';
import type { Caller } from '../app/editable';
import { fmtDateTime } from '../util/format';
import { segmented, switchCtl } from './controls';
import { openDialog } from './dialog';
import { icon } from './icon';
import { ident } from './ident';
import { toast } from './toast';
import { turnBrief } from './turn';

export const OUTPUT_OPTIONS = [
  { value: 'progress', get label() { return translate('d2ee451'); } },
  { value: 'result', get label() { return translate('d36b1b0'); } },
];
export const CONTEXT_OPTIONS = [
  { value: 'resume', get label() { return translate('d2abac2'); } },
  { value: 'fresh', get label() { return translate('d964777'); } },
];

const label = (opts: Array<{ value: string; label: string }>, v: string) => opts.find((o) => o.value === v)?.label ?? v;

/** 把存储里的 kind + spec 还原成一条合法的节律写法,作为编辑框的初值。 */
export function whenFromSpec(s: Pick<ScheduleView, 'kind' | 'spec'>): string {
  if (s.kind === 'cron') return `cron ${s.spec}`;
  if (s.kind === 'interval') {
    const n = Number(s.spec);
    if (n % 86400 === 0) return `every ${n / 86400}d`;
    if (n % 3600 === 0) return `every ${n / 3600}h`;
    if (n % 60 === 0) return `every ${n / 60}m`;
    return `every ${n}s`;
  }
  if (s.kind === 'once') return `at ${s.spec.slice(0, 16)}`;
  return s.spec;
}

/** 一条任务的「最近执行」:展开时才去台账取。 */
export interface ScheduleRuns {
  open: boolean;
  loading: boolean;
  turns?: TurnRecord[];
  error?: string;
}

export interface ScheduleRowOpts {
  busy?: boolean;
  /** 给了 onRuns 才有「最近执行」这一块 */
  runs?: ScheduleRuns;
  onRuns?: (s: ScheduleView, open: boolean) => void;
  showChat?: boolean;
  /** 列表没按 agent 分组时,在事实行里写出是哪个 agent 的 */
  agentLabel?: string;
  onToggle: (s: ScheduleView, enabled: boolean) => void;
  onEdit: (s: ScheduleView) => void;
}

export function scheduleRow(s: ScheduleView, o: ScheduleRowOpts): TemplateResult {
  const facts: Array<string | TemplateResult> = [];
  if (s.enabled) facts.push(html`${rich('d6b88c5')} <span class="tnum">${s.next_fire ? fmtDateTime(s.next_fire) : '—'}</span>`);
  facts.push(html`${rich('d2b9bbc')} <span class="tnum">${s.last_fired ? fmtDateTime(s.last_fired) : translate('da81b68')}</span>`);
  facts.push(label(OUTPUT_OPTIONS, s.output));
  facts.push(label(CONTEXT_OPTIONS, s.context));
  if (o.agentLabel) facts.push(o.agentLabel);
  if (o.showChat && s.chat_name) facts.push(s.chat_name);
  return html`<div class="sched ${s.enabled ? '' : 'is-off'}" id=${`s-${s.id}`}>
    <div class="sched-switch">
      ${switchCtl({
        checked: s.enabled,
        label: `${s.enabled ? translate('d4e6fd0') : translate('ddced8a')} ${s.human}`,
        disabled: !s.editable || o.busy,
        onChange: (v) => o.onToggle(s, v),
      })}
    </div>
    <div class="sched-main">
      <div class="sched-title">
        <span class="sched-human">${s.human}</span>
        <span class="mono muted">#${s.id}</span>
        ${s.enabled ? nothing : html`<span class="chip chip-quiet">${rich('d82395d')}</span>`}
      </div>
      <p class="sched-prompt" title=${s.prompt}>${s.prompt}</p>
      <div class="sched-facts">${facts.map((f, i) => html`${i ? html`<span class="meta-sep" aria-hidden="true"></span>` : nothing}<span>${f}</span>`)}</div>
      ${o.onRuns ? runsBlock(s, o.runs, o.onRuns) : nothing}
    </div>
    <div class="sched-actions">
      <button class="btn btn-sm" aria-label=${translate('d4ff722', [s.id])} ?disabled=${!s.editable || o.busy} @click=${() => o.onEdit(s)}>
        ${icon('pencil', 'icon-sm')}<span class="btn-label">${rich('d051836')}</span>
      </button>
    </div>
  </div>`;
}

function runsBlock(s: ScheduleView, r: ScheduleRuns | undefined, onRuns: (s: ScheduleView, open: boolean) => void): TemplateResult {
  let body: TemplateResult | typeof nothing = nothing;
  if (r?.open) {
    if (r.loading) body = html`<p class="muted">${rich('d4927a5')}</p>`;
    else if (r.error) body = html`<p class="muted">${rich('de86014', [r.error])}</p>`;
    else if (r.turns?.length) body = html`${r.turns.map((t) => turnBrief(t))}`;
    else if (r.turns) body = html`<p class="muted">${rich('de12d88')}</p>`;
  }
  return html`<details
    class="sched-runs"
    .open=${live(!!r?.open)}
    @toggle=${(e: Event) => {
      const open = (e.target as HTMLDetailsElement).open;
      if (open !== !!r?.open) onRuns(s, open);
    }}
  >
    <summary>${rich('d230c85', [icon('chevronRight', 'icon-chev')])}</summary>
    <div class="sched-runs-body">${body}</div>
  </details>`;
}

/** 停用 / 重新启用:都经存储层,停用即移进归档。 */
export async function toggleSchedule(call: Caller, s: ScheduleView, enabled: boolean): Promise<ScheduleView | null> {
  try {
    const v = await call('schedules.set_enabled', { id: s.id, enabled });
    toast(enabled ? translate('d47b29d', [s.id]) : translate('dbe6c8c', [s.id]), 'ok');
    return v;
  } catch (err) {
    toast(translate('d860480', [errorMessage(err)]), 'error');
    return null;
  }
}

export function editScheduleDialog(call: Caller, s: ScheduleView): Promise<ScheduleView | undefined> {
  const initialWhen = whenFromSpec(s);
  const state = {
    when: initialWhen,
    prompt: s.prompt,
    output: s.output,
    context: s.context,
    check: null as WhenCheck | null,
    checking: false,
    saving: false,
    error: '',
    errorKey: '',
  };
  let timer: ReturnType<typeof setTimeout> | undefined;
  let seq = 0;
  return openDialog<ScheduleView>(
    (done, h) => {
      const runCheck = () => {
        if (timer) clearTimeout(timer);
        timer = setTimeout(async () => {
          const mine = ++seq;
          state.checking = true;
          h.update();
          try {
            const r = await call('schedules.check_when', { when: state.when });
            if (mine === seq) state.check = r;
          } catch (err) {
            if (mine === seq) state.check = { ok: false, next: [], error: errorMessage(err) };
          }
          if (mine === seq) state.checking = false;
          h.update();
        }, 250);
      };
      if (!state.check && !state.checking && !timer) runCheck();
      const save = async () => {
        const edit: ScheduleEdit = {};
        if (state.when.trim() !== initialWhen) edit.when = state.when.trim();
        if (state.prompt !== s.prompt) edit.prompt = state.prompt;
        if (state.output !== s.output) edit.output = state.output;
        if (state.context !== s.context) edit.context = state.context;
        if (!Object.keys(edit).length) return done(s);
        state.saving = true;
        state.error = '';
        h.update();
        try {
          const v = await call('schedules.edit', { id: s.id, edit });
          toast(translate('deda078', [s.id]), 'ok');
          done(v);
        } catch (err) {
          state.error = errorMessage(err);
          state.errorKey = err instanceof ApiCallError ? (err.key ?? '') : '';
          state.saving = false;
          h.update();
        }
      };
      const c = state.check;
      const whenBad = c && !c.ok;
      return html`<div class="dlg-head">
          <h2>${rich('d6d9314')} <span class="mono muted">#${s.id}</span></h2>
          <p>${rich('d98b6b0', [s.chat_name ?? s.chat_id, ident(s.created_by, { agent: s.agent })])}</p>
        </div>
        <div class="dlg-body">
          <div class="form-grid">
            <label class="form-field">
              <span class="form-label">${rich('d42b1af')}</span>
              <input
                class="field-input mono"
                type="text"
                spellcheck="false"
                autocomplete="off"
                .value=${live(state.when)}
                @input=${(e: Event) => {
                  state.when = (e.target as HTMLInputElement).value;
                  runCheck();
                }}
              />
              ${state.checking && !c
                ? html`<span class="form-hint">${rich('d84717e')}</span>`
                : c?.ok
                  ? html`<span class="form-hint">${c.human}${c.next.length ? translate('dfc732b') : translate('dc967bb')}</span>
                      ${c.next.length
                        ? html`<span class="next-fires">${c.next.map((n) => html`<span class="chip tnum">${fmtDateTime(n)}</span>`)}</span>`
                        : nothing}`
                  : whenBad
                    ? html`<span class="form-hint err">${c.error}</span>${c.forms ? html`<span class="form-hint mono">${c.forms}</span>` : nothing}`
                    : nothing}
            </label>
            <label class="form-field">
              <span class="form-label">${rich('d4b47db')}</span>
              <textarea
                class="field-input"
                rows="4"
                .value=${live(state.prompt)}
                @input=${(e: Event) => {
                  state.prompt = (e.target as HTMLTextAreaElement).value;
                  h.update();
                }}
              ></textarea>
            </label>
            <div class="form-field">
              <span class="form-label">${rich('de32e26')}</span>
              ${segmented({
                label: translate('de32e26'),
                value: state.output,
                options: OUTPUT_OPTIONS,
                onChange: (v) => {
                  state.output = v;
                  h.update();
                },
              })}
            </div>
            <div class="form-field">
              <span class="form-label">${rich('d6fc38e')}</span>
              ${segmented({
                label: translate('d6fc38e'),
                value: state.context,
                options: CONTEXT_OPTIONS,
                onChange: (v) => {
                  state.context = v;
                  h.update();
                },
              })}
              <span class="form-hint">${rich('d31b731')}</span>
            </div>
            ${state.error ? html`<p class="row-error" role="alert">${icon('circleAlert', 'icon-sm')}<span>${state.error}</span></p>` : nothing}
          </div>
        </div>
        <div class="dlg-foot">
          <button class="btn" @click=${() => h.close()}>${rich('d2cd0f3')}</button>
          <button class="btn btn-primary" ?disabled=${state.saving || !!whenBad || !state.prompt.trim()} @click=${save}>
            ${state.saving ? translate('dff509c') : translate('da3030b')}
          </button>
        </div>`;
    },
    { label: translate('dd6902b', [s.id]) },
  );
}
