import { render } from 'lit';
import { afterEach, describe, expect, test, vi } from 'vitest';
import type { ConfigChange, PreviewView } from '../api/types';
import type { Caller } from '../app/editable';
import { PreviewController } from './controller';
import { renderCot } from './cot';
import { changedBlocks, regionDiff } from './diff';
import { hasRenderer, renderCard, SUPPORTED_TAGS } from './feishu-card';
import { renderMarkdown } from './markdown';

const mount = (tpl: ReturnType<typeof renderCard>) => {
  const host = document.createElement('div');
  render(tpl, host);
  return host;
};

const SAMPLE: Record<string, Record<string, unknown>> = {
  markdown: { tag: 'markdown', element_id: 'm', content: '**粗** 与 `code`' },
  hr: { tag: 'hr', element_id: 'h' },
  img: { tag: 'img', element_id: 'i', img_key: 'img_v3_x', alt: { tag: 'plain_text', content: '截图' } },
  column_set: {
    tag: 'column_set',
    element_id: 'cs',
    flex_mode: 'flow',
    horizontal_spacing: '8px',
    columns: [{ tag: 'column', width: 'auto', elements: [{ tag: 'button', text: { tag: 'plain_text', content: '打开' }, type: 'primary' }] }],
  },
  column: { tag: 'column', element_id: 'c', width: 'weighted', weight: 1, elements: [] },
  button: { tag: 'button', element_id: 'b', text: { tag: 'plain_text', content: '确定' }, type: 'default' },
  collapsible_panel: {
    tag: 'collapsible_panel',
    element_id: 'p',
    expanded: false,
    header: { title: { tag: 'markdown', content: '完整回复' } },
    elements: [{ tag: 'markdown', content: '正文' }],
  },
};

describe('卡片渲染', () => {
  test('supported_tags_all_render', () => {
    expect(SUPPORTED_TAGS.length).toBeGreaterThan(0);
    // Rust 侧列举的元素必须全部画得出;另外多认的两种也一起验。
    for (const tag of [...SUPPORTED_TAGS, 'img', 'collapsible_panel']) {
      expect(hasRenderer(tag), tag).toBe(true);
      const el = SAMPLE[tag];
      expect(el, `缺少 ${tag} 的样例`).toBeTruthy();
      const host = mount(renderCard({ schema: '2.0', body: { elements: [el] } }));
      expect(host.querySelector('.fsc-unknown'), tag).toBeNull();
      expect(host.querySelector(`[data-eid="${el!.element_id}"]`), tag).toBeTruthy();
    }
  });

  test('unknown_card_element_renders_placeholder', () => {
    const host = mount(renderCard({ schema: '2.0', body: { elements: [{ tag: 'chart', element_id: 'x' }, { tag: 'markdown', content: 'ok' }] } }));
    const ph = host.querySelector('.fsc-unknown');
    expect(ph).toBeTruthy();
    expect(ph!.textContent).toContain('不支持的卡片元素');
    expect(ph!.textContent).toContain('<chart>');
    expect(host.querySelector('.fsc-md')!.textContent).toBe('ok');
  });

  test('buttons_are_not_interactive', () => {
    const host = mount(renderCard({ schema: '2.0', body: { elements: [SAMPLE.button!] } }));
    expect(host.querySelector('button, a, [tabindex]')).toBeNull();
  });
});

describe('受限 markdown', () => {
  test('markdown_never_injects_html', () => {
    const evil = '<img src=x onerror=alert(1)><script>alert(2)</script> **ok** <a href="javascript:1">x</a> [链接](javascript:alert(3))';
    const frag = renderMarkdown(evil);
    const host = document.createElement('div');
    host.appendChild(frag);
    expect(host.querySelector('img, script, a, iframe')).toBeNull();
    const tags = [...host.querySelectorAll('*')].map((e) => e.tagName.toLowerCase());
    for (const t of tags) expect(['p', 'br', 'strong', 'em', 's', 'span', 'ul', 'ol', 'li', 'pre']).toContain(t);
    expect(host.textContent).toContain('<script>alert(2)</script>');
    expect(host.querySelector('.fs-link')!.textContent).toBe('链接');
  });

  test('supported_subset_renders', () => {
    const host = document.createElement('div');
    host.appendChild(renderMarkdown("<font color='grey'>灰字</font> <at id=ou_x>示例用户</at> *斜* ~~删~~ `c`\n- 一\n- 二\n\n```\nfn main() {}\n```"));
    expect(host.querySelector('.fs-c-grey')!.textContent).toBe('灰字');
    expect(host.querySelector('.fs-at')!.textContent).toBe('@示例用户');
    expect(host.querySelector('em')!.textContent).toBe('斜');
    expect(host.querySelector('s')!.textContent).toBe('删');
    expect(host.querySelectorAll('li')).toHaveLength(2);
    expect(host.querySelector('pre')!.textContent).toBe('fn main() {}');
  });

  test('globs_and_snake_case_stay_literal', () => {
    const host = document.createElement('div');
    host.appendChild(renderMarkdown('crates/**/*.rs 与 snake_case 与 2*3'));
    expect(host.querySelector('strong, em')).toBeNull();
    expect(host.textContent).toBe('crates/**/*.rs 与 snake_case 与 2*3');
  });
});

const view = (over: Partial<Record<string, unknown>> = {}): PreviewView => ({
  title: 't',
  regions: {},
  messages: [
    { id: 'user', kind: 'user', sender: 'u', text: 'hi', reactions: (over.reactions as string[]) ?? ['DONE'] },
    { id: 'cot', kind: 'cot', title: 'q', status: 'done', items: (over.items as never) ?? [{ id: 't1', type: 'thinking', text: 'a' }] },
    {
      id: 'reply',
      kind: 'card',
      card: { schema: '2.0', body: { elements: [{ tag: 'markdown', element_id: 'body', content: 'x' }, { tag: 'markdown', element_id: 'badge', content: (over.badge as string) ?? 'high' }] } },
    },
  ],
});

describe('区块比对', () => {
  test('changed_leaf_is_reported_alone', () => {
    expect(changedBlocks(view(), view({ badge: 'low' }))).toEqual(['badge']);
  });

  test('block_only_in_contrast_maps_to_parent', () => {
    // 基准里没有条目、对照里有 → 归到 CoT 这条消息
    expect(regionDiff(view({ items: [] }), view())).toEqual(['cot']);
    // 基准里有、对照里没有 → 就是它自己
    expect(regionDiff(view(), view({ items: [] }))).toEqual(['t1']);
  });

  test('unchanged_view_has_no_region', () => {
    expect(regionDiff(view(), view())).toEqual([]);
  });
});

describe('预览取数', () => {
  afterEach(() => vi.useRealTimers());

  test('preview_debounces_draft_changes', async () => {
    vi.useFakeTimers();
    let draft: ConfigChange[] = [];
    const calls: ConfigChange[][] = [];
    const call = (async (_m: string, p: { draft: ConfigChange[] }) => {
      calls.push(p.draft);
      return view();
    }) as unknown as Caller;
    const ctl = new PreviewController(call, { level: 'global' }, () => draft);
    draft = [{ key: 'a', value: true }];
    ctl.schedule();
    await vi.advanceTimersByTimeAsync(100);
    draft = [{ key: 'a', value: false }];
    ctl.schedule();
    await vi.advanceTimersByTimeAsync(100);
    draft = [{ key: 'a', value: true }, { key: 'b', value: 1 }];
    ctl.schedule();
    await vi.advanceTimersByTimeAsync(149);
    expect(calls).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(1);
    expect(calls).toEqual([[{ key: 'a', value: true }, { key: 'b', value: 1 }]]);
  });

  test('changed_blocks_flash_then_clear', async () => {
    vi.useFakeTimers();
    let n = 0;
    const call = (async () => (n++ === 0 ? view() : view({ badge: 'low' }))) as unknown as Caller;
    const ctl = new PreviewController(call, { level: 'global' }, () => []);
    await ctl.request();
    expect(ctl.changed.size).toBe(0);
    await ctl.request();
    expect([...ctl.changed]).toEqual(['badge']);
    expect(ctl.mark('badge')).toContain('is-changed');
    await vi.advanceTimersByTimeAsync(600);
    expect(ctl.changed.size).toBe(0);
  });

  test('focus_outlines_the_rows_region', async () => {
    const v = { ...view(), regions: { 'messaging.badge_effort': ['badge'] } };
    const ctl = new PreviewController((async () => v) as unknown as Caller, { level: 'global' }, () => []);
    await ctl.request();
    expect(ctl.hasRegion('messaging.badge_effort')).toBe(true);
    expect(ctl.hasRegion('messaging.mask_email')).toBe(false);
    ctl.setFocus('messaging.badge_effort');
    expect(ctl.mark('badge')).toContain('is-region');
    expect(ctl.mark('body')).toBe('');
  });
});

describe('过程块', () => {
  test('cot_folds_and_unfolds_from_the_title_row', () => {
    const cot = {
      id: 'cot',
      kind: 'cot' as const,
      title: '帮我看下 CI',
      status: 'done' as const,
      items: [{ id: 'cot.thinking-1', type: 'thinking' as const, text: '先看日志' }],
    };
    const fold = { collapsed: false, toggle: () => (fold.collapsed = !fold.collapsed) };
    const host = document.createElement('div');
    const draw = () => render(renderCot(cot, () => '', fold), host);
    draw();
    const head = host.querySelector<HTMLButtonElement>('.fs-cot-head')!;
    expect(head.getAttribute('aria-expanded')).toBe('true');
    expect(host.querySelector('.fs-cot-body')).toBeTruthy();
    head.click();
    draw();
    expect(head.getAttribute('aria-expanded')).toBe('false');
    expect(host.querySelector('.fs-cot-body')).toBeNull();
    expect(host.querySelector('.fs-cot-title')!.textContent).toBe('帮我看下 CI');
  });

  test('cot_replay_shows_items_so_far_with_a_shimmering_title', () => {
    const cot = {
      id: 'cot',
      kind: 'cot' as const,
      title: '帮我看下 CI',
      status: 'done' as const,
      items: [
        { id: 'cot.thinking-1', type: 'thinking' as const, text: '先看日志' },
        { id: 'cot.thinking-2', type: 'thinking' as const, text: '再跑测试' },
      ],
    };
    const host = document.createElement('div');
    render(renderCot(cot, () => '', { collapsed: false, toggle: () => {} }, { shown: 1 }), host);
    expect(host.querySelector('.fs-cot-title')!.classList.contains('fs-shimmer')).toBe(true);
    expect(host.querySelector('.fs-cot-steps')!.textContent).toBe('进行中 · 1 步');
    expect(host.querySelectorAll('.fs-cot-item').length).toBe(1);
    render(renderCot(cot, () => '', { collapsed: false, toggle: () => {} }), host);
    expect(host.querySelector('.fs-cot-title')!.classList.contains('fs-shimmer')).toBe(false);
    expect(host.querySelector('.fs-cot-steps')!.textContent).toBe('已完成 · 2 步');
  });
});
