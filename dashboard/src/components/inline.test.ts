import { html, render } from 'lit';
import { describe, expect, test } from 'vitest';
import { inlineCode } from './inline';

describe('说明文字里的行内代码', () => {
  test('backticks_render_as_code_and_html_stays_text', () => {
    const host = document.createElement('div');
    render(html`${inlineCode('第 72 行 `quota_recovery`:已废弃 <b>x</b> `a`b')}`, host);
    const codes = [...host.querySelectorAll('code')].map((c) => c.textContent);
    expect(codes).toEqual(['quota_recovery', 'a']);
    expect(host.querySelector('b')).toBeNull();
    expect(host.textContent).toBe('第 72 行 quota_recovery:已废弃 <b>x</b> ab');
  });
});
