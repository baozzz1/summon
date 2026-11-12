import { t as translate, rich } from '../i18n';
import { html, nothing, type TemplateResult } from 'lit';
import { live } from 'lit/directives/live.js';
import type { RawIssue } from '../api/types';
import { icon } from './icon';
import { inlineCode } from './inline';

/** YAML 原文:带行号的原生文本框。框随内容变高,长行只在框内横向滚动。 */
export function yamlEditor(o: {
  text: string;
  label: string;
  errors?: RawIssue[];
  readonly?: boolean;
  onInput: (v: string) => void;
}): TemplateResult {
  const n = o.text.split('\n').length;
  const bad = new Set((o.errors ?? []).map((e) => e.line).filter((x): x is number => typeof x === 'number'));
  const nums: TemplateResult[] = [];
  for (let i = 1; i <= n; i++) nums.push(html`<span class=${bad.has(i) ? 'err' : ''}>${i}</span>${i < n ? '\n' : ''}`);
  return html`<div class="yaml">
    <pre class="yaml-gutter" aria-hidden="true">${nums}</pre>
    <textarea
      class="yaml-text"
      wrap="off"
      spellcheck="false"
      autocapitalize="off"
      autocomplete="off"
      rows=${n + 1}
      aria-label=${o.label}
      ?readonly=${o.readonly}
      .value=${live(o.text)}
      @input=${(e: Event) => o.onInput((e.target as HTMLTextAreaElement).value)}
    ></textarea>
  </div>`;
}

export function issuesList(errors: RawIssue[], warnings: RawIssue[]): TemplateResult | typeof nothing {
  if (!errors.length && !warnings.length) return nothing;
  const row = (i: RawIssue, kind: 'err' | 'warn') => html`<li class=${kind}>
    ${icon(kind === 'err' ? 'circleAlert' : 'triangleAlert', 'icon-sm')}
    <span
      >${i.line ? html`<span class="tnum">${rich('dc4e3a9', [i.line, i.col ? translate('d3b067d', [i.col]) : ''])}</span>：` : nothing}${inlineCode(i.message)}${i.suggestion
        ? html` ${rich('dafcbfd')} <code>${i.suggestion}</code>。`
        : nothing}</span
    >
  </li>`;
  return html`<ul class="issues-list" role="alert">
    ${errors.map((e) => row(e, 'err'))} ${warnings.map((w) => row(w, 'warn'))}
  </ul>`;
}

/** 说明文件(CLAUDE.md / SOUL.md):普通文本框,随内容变高。 */
export function docEditor(o: {
  text: string;
  label: string;
  readonly?: boolean;
  placeholder?: string;
  onInput: (v: string) => void;
}): TemplateResult {
  const rows = Math.max(8, o.text.split('\n').length + 1);
  return html`<textarea
    class="field-input doc-text"
    rows=${rows}
    aria-label=${o.label}
    placeholder=${o.placeholder ?? ''}
    ?readonly=${o.readonly}
    .value=${live(o.text)}
    @input=${(e: Event) => o.onInput((e.target as HTMLTextAreaElement).value)}
  ></textarea>`;
}
