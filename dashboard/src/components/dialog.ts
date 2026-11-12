import { t as translate, rich } from '../i18n';
import { html, nothing, render, type TemplateResult } from 'lit';
import type { RawCheck } from '../api/types';
import { icon } from './icon';
import { inlineCode } from './inline';

export interface DialogHandle {
  update(): void;
  close(): void;
  el: HTMLDialogElement;
}

/**
 * 原生 <dialog>:showModal 自带焦点圈定与 Esc 关闭。内容用 Lit 模板渲染,需要刷新时调 update()。
 */
export function openDialog<T>(
  build: (done: (v: T) => void, h: DialogHandle) => TemplateResult,
  opts: { label: string; wide?: boolean } = { label: '' },
): Promise<T | undefined> {
  return new Promise((resolve) => {
    const el = document.createElement('dialog');
    el.className = `dlg${opts.wide ? ' wide' : ''}`;
    el.setAttribute('aria-label', opts.label);
    let result: T | undefined;
    const handle: DialogHandle = {
      el,
      update: () => render(build(done, handle), el),
      close: () => el.close(),
    };
    function done(v: T) {
      result = v;
      el.close();
    }
    el.addEventListener('close', () => {
      el.remove();
      resolve(result);
    });
    // 点遮罩关闭(点在对话框自身矩形之外)
    el.addEventListener('click', (ev) => {
      if (ev.target !== el) return;
      const r = el.getBoundingClientRect();
      const { clientX: x, clientY: y } = ev;
      if (x < r.left || x > r.right || y < r.top || y > r.bottom) el.close();
    });
    document.body.appendChild(el);
    handle.update();
    if (typeof el.showModal === 'function') el.showModal();
    else el.setAttribute('open', '');
  });
}

export async function confirmDialog(o: {
  title: string;
  body?: string | TemplateResult;
  confirm?: string;
  cancel?: string;
  danger?: boolean;
}): Promise<boolean> {
  const r = await openDialog<boolean>(
    (done) => html`<div class="dlg-head"><h2>${o.title}</h2></div>
      ${o.body ? html`<div class="dlg-body">${typeof o.body === 'string' ? html`<p>${o.body}</p>` : o.body}</div>` : nothing}
      <div class="dlg-foot">
        <button class="btn" @click=${() => done(false)}>${o.cancel ?? translate('d2cd0f3')}</button>
        <button class="btn ${o.danger ? 'btn-danger' : 'btn-primary'}" autofocus @click=${() => done(true)}>
          ${o.confirm ?? translate('dfac2a6')}
        </button>
      </div>`,
    { label: o.title },
  );
  return r === true;
}

export function diffView(diff: string): TemplateResult {
  const lines = diff.split('\n');
  return html`<pre class="diff" tabindex="0" aria-label="${translate('ddf979d')}">${lines.map((l) => {
    const cls = l.startsWith('@@') ? 'diff-hunk' : l.startsWith('+') && !l.startsWith('+++') ? 'diff-add' : l.startsWith('-') && !l.startsWith('---') ? 'diff-del' : '';
    return html`<span class="diff-line ${cls}">${l || ' '}</span>`;
  })}</pre>`;
}

/** 原文保存前的确认:列出警告,给出改动前后的差异。 */
export async function reviewRawDialog(check: RawCheck, label: string): Promise<boolean> {
  const r = await openDialog<boolean>(
    (done) => html`<div class="dlg-head">
        <h2>${rich('dd48497', [label])}</h2>
        <p>${rich('d9b2feb')}</p>
      </div>
      <div class="dlg-body">
        ${check.warnings.length
          ? html`<ul class="issues-list">
              ${check.warnings.map(
                (w) => html`<li class="warn">
                  ${icon('triangleAlert', 'icon-sm')}
                  <span
                    >${w.line ? html`<span class="tnum">${rich('d43cf63', [w.line])}</span> ` : nothing}${inlineCode(w.message)}${w.suggestion
                      ? html` ${rich('d4595de')} <code>${w.suggestion}</code>？`
                      : nothing}</span
                  >
                </li>`,
              )}
            </ul>`
          : nothing}
        ${diffView(check.diff)}
      </div>
      <div class="dlg-foot">
        <button class="btn" @click=${() => done(false)}>${rich('d3160fc')}</button>
        <button class="btn btn-primary" autofocus @click=${() => done(true)}>
          ${check.warnings.length ? translate('d35ffbf') : translate('da3030b')}
        </button>
      </div>`,
    { label: translate('dd48497', [label]), wide: true },
  );
  return r === true;
}
