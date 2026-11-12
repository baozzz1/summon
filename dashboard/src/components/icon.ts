import { html, type TemplateResult } from 'lit';
import { ICONS, type IconName } from './icons';

export function icon(name: IconName, cls = ''): TemplateResult {
  return html`<svg class="icon ${cls}" viewBox="0 0 24 24" aria-hidden="true" focusable="false">${ICONS[name]}</svg>`;
}
