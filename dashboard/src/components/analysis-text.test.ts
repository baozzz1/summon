import { render } from 'lit';
import { describe, expect, test } from 'vitest';
import { parseAnalysis, renderAnalysis } from './analysis-text';

function draw(text: string): HTMLElement {
  const host = document.createElement('div');
  render(renderAnalysis(text), host);
  return host.querySelector('.analysis-text')!;
}

describe('分析文本的渲染', () => {
  test('headings_lists_bold_code_and_paragraphs', () => {
    const el = draw('# 结论\n\n## 观察\n1. 长间隔占 **74%** 的时长\n2. 用 `summon remind` 设提醒\n   接着上一项\n\n- 甲\n* 乙\n\n### 局限\n第一行\n第二行');
    expect([...el.children].map((c) => c.tagName)).toEqual(['H3', 'H3', 'OL', 'UL', 'H4', 'P']);
    const ol = el.querySelector('ol')!;
    expect(ol.getAttribute('start')).toBe('1');
    expect(ol.querySelector('strong')!.textContent).toBe('74%');
    expect(ol.querySelector('code')!.textContent).toBe('summon remind');
    expect(ol.children[1]!.textContent).toBe('用 summon remind 设提醒\n接着上一项');
    expect([...el.querySelectorAll('ul li')].map((li) => li.textContent)).toEqual(['甲', '乙']);
    expect(el.querySelector('p')!.textContent).toBe('第一行\n第二行');
  });

  test('ordered_lists_keep_their_start_and_unmatched_marks_stay_text', () => {
    expect(parseAnalysis('3. 三\n4. 四')).toEqual([{ kind: 'list', ordered: true, start: 3, items: ['三', '四'] }]);
    const el = draw('**没有闭合 与 `也没有 与 2*3');
    expect(el.querySelector('strong')).toBeNull();
    expect(el.querySelector('code')).toBeNull();
    expect(el.textContent).toBe('**没有闭合 与 `也没有 与 2*3');
  });

  test('html_scripts_links_and_images_render_as_plain_text', () => {
    const hostile = [
      '<script>alert(1)</script>',
      '<img src=x onerror=alert(1)>',
      '<a href="javascript:alert(1)">点我</a>',
      '[链接](https://example.com) 与 ![图](https://example.com/x.png)',
      '**<b>粗</b>** `<i>代码</i>`',
      '# <h1>标题</h1>',
      '- <iframe src="https://example.com"></iframe>',
    ].join('\n\n');
    const el = draw(hostile);
    for (const tag of ['script', 'img', 'a', 'b', 'i', 'iframe', 'h1']) expect(el.querySelector(tag), tag).toBeNull();
    expect([...el.querySelectorAll('*')].every((n) => n.attributes.length === (n.tagName === 'OL' ? 1 : 0))).toBe(true);
    const text = el.textContent!;
    expect(text).toContain('<script>alert(1)</script>');
    expect(text).toContain('<img src=x onerror=alert(1)>');
    expect(text).toContain('[链接](https://example.com) 与 ![图](https://example.com/x.png)');
    expect(el.querySelector('strong')!.textContent).toBe('<b>粗</b>');
    expect(el.querySelector('code')!.textContent).toBe('<i>代码</i>');
    expect(el.querySelector('h3')!.textContent).toBe('<h1>标题</h1>');
  });

  test('empty_text_renders_no_blocks', () => {
    expect(parseAnalysis('\n  \n')).toEqual([]);
    expect(draw('').children).toHaveLength(0);
  });
});
