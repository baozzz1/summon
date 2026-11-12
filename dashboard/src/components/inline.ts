import { html, type TemplateResult } from 'lit';

/**
 * 服务端说明文字里的受限 markdown:只认反引号包起来的行内代码,其余原样当文字。
 * 走 Lit 的文本绑定,不把服务端字符串当 HTML。
 */
export function inlineCode(text: string): Array<string | TemplateResult> {
  return text
    .split(/(`[^`\n]+`)/)
    .filter((part) => part !== '')
    .map((part) => (part.length > 2 && part.startsWith('`') && part.endsWith('`') ? html`<code>${part.slice(1, -1)}</code>` : part));
}
