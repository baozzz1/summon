import { LocaleController } from '../i18n';
// 群 ID、open_id、union_id 的显示:查得到名字就照飞书文档里提及人的样子画成蓝色的「@名字」
// (群是群名),悬停看原 ID;查不到画成缩短的 ID。
import { html, LitElement, nothing, type TemplateResult } from 'lit';
import { nameOf, names } from '../app/names';
import { shortId } from '../util/format';
import { icon } from './icon';

export class Ident extends LitElement {
  readonly locale = new LocaleController(this);
  static override properties = {
    value: { type: String },
    name: { type: String },
    agent: { type: String },
  };

  declare value: string;
  /** 调用方已经知道的名字(如名单成员随数据带回来的) */
  declare name: string;
  /** ID 出自哪个 agent:open_id 按应用区分 */
  declare agent: string;

  constructor() {
    super();
    this.value = '';
    this.name = '';
    this.agent = '';
  }

  protected override createRenderRoot(): HTMLElement {
    return this;
  }

  private readonly onNames = () => this.requestUpdate();

  override connectedCallback(): void {
    super.connectedCallback();
    names.addEventListener('change', this.onNames);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    names.removeEventListener('change', this.onNames);
  }

  protected override render(): TemplateResult {
    const id = this.value;
    const name = this.name || nameOf(id, this.agent);
    const chat = id.startsWith('oc_');
    if (!name) return html`<span class="mention unknown" title=${id}>${shortId(id)}</span>`;
    return html`<span class="mention ${chat ? 'chat' : ''}" title=${id}
      >${chat ? icon('messagesSquare', 'icon-xs') : nothing}${chat ? name : `@${name}`}</span
    >`;
  }
}

customElements.define('sm-ident', Ident);

export function ident(id: string, opts: { name?: string; agent?: string } = {}): TemplateResult {
  return html`<sm-ident .value=${id} .name=${opts.name ?? ''} .agent=${opts.agent ?? ''}></sm-ident>`;
}
