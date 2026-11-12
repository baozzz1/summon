import { icon } from './icon';
import { html, render } from 'lit';

export type ToastKind = 'info' | 'ok' | 'warn' | 'error';

let host: HTMLElement | null = null;

/** 轻提示:屏幕下方一条,几秒后自己消失。屏幕阅读器经 aria-live 读到。 */
export function toast(message: string, kind: ToastKind = 'info', ms = 3200): void {
  if (typeof document === 'undefined') return;
  if (!host || !host.isConnected) {
    host = document.createElement('div');
    host.className = 'toasts';
    host.setAttribute('role', 'status');
    host.setAttribute('aria-live', 'polite');
    document.body.appendChild(host);
  }
  const el = document.createElement('div');
  el.className = `toast toast-${kind}`;
  const ic = kind === 'ok' ? 'check' : kind === 'error' ? 'circleAlert' : kind === 'warn' ? 'triangleAlert' : 'info';
  render(html`${icon(ic, 'icon-sm')}<span>${message}</span>`, el);
  host.appendChild(el);
  setTimeout(() => el.remove(), kind === 'error' ? ms * 1.6 : ms);
}
