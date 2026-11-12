import { render } from 'lit';
import { describe, expect, test } from 'vitest';
import schemaFixture from '../api/fixtures/config.schema.json';
import type { ConfigView, FieldDesc, Schema } from '../api/types';
import { ConfigDraft } from '../app/editable';
import { fieldsIn, groupSection } from './field-row';
import { RERENDER_EVENT } from './events';
import { store } from '../app/store';
import { primaryAgentOptions } from '../pages/global';

const schema = (schemaFixture as unknown as { data: Schema }).data;

function sentinelDraft(enabled: boolean | null, dryRun: boolean | null) {
  const fields = new Map(schema.fields.map((f): [string, FieldDesc] => [f.key, f]));
  const view: ConfigView = {
    scope: { level: 'global' },
    revision: 'r1',
    values: fieldsIn(schema, 'sentinel').map((f) => ({
      key: f.key,
      value: f.key === 'sentinel.enabled' ? enabled : f.key === 'sentinel.dry_run' ? dryRun : null,
      effective: f.key === 'sentinel.enabled' ? (enabled ?? false) : f.key === 'sentinel.dry_run' ? (dryRun ?? false) : null,
      source: 'builtin',
      overridden_by: [],
      editable: true,
    })),
  };
  const host = document.createElement('div');
  const d: ConfigDraft = new ConfigDraft({ level: 'global' }, 'config.yaml', () => fields, (async () => ({})) as never, () =>
    render(groupSection(schema.groups.find((g) => g.id === 'sentinel')!, fieldsIn(schema, 'sentinel'), { draft: d, level: 'global' }), host),
  );
  d.setView(view);
  const pick = (label: string) => {
    const opt = [...host.querySelectorAll('.row--composite .seg-opt')].find((o) => o.textContent?.trim() === label);
    const input = opt!.querySelector('input')!;
    input.checked = true;
    input.dispatchEvent(new Event('change'));
  };
  return { d, host, pick };
}

describe('Sentinel 的「关 / 演练 / 开」', () => {
  test('sentinel_modes_map_to_both_keys_in_one_patch', () => {
    const { d, host, pick } = sentinelDraft(false, null);
    // 两个布尔合成一行
    expect(host.querySelectorAll('.row--composite')).toHaveLength(1);
    expect(host.querySelector('[data-key="sentinel.enabled"]')).toBeNull();
    expect(host.querySelector('[data-key="sentinel.dry_run"]')).toBeNull();
    pick('演练');
    expect(d.changeList()).toEqual([
      { key: 'sentinel.enabled', value: true },
      { key: 'sentinel.dry_run', value: true },
    ]);
    pick('开');
    expect(d.changeList()).toEqual([
      { key: 'sentinel.enabled', value: true },
      { key: 'sentinel.dry_run', value: false },
    ]);
    // 关:只写 enabled:false,dry_run 回到原值不提交
    pick('关');
    expect(d.changeList()).toEqual([]);
  });

  test('sentinel_off_from_dry_run_writes_enabled_false_only', () => {
    const { d, pick } = sentinelDraft(true, true);
    pick('关');
    expect(d.changeList()).toEqual([{ key: 'sentinel.enabled', value: false }]);
  });
});

describe('取反项', () => {
  test('inverted_field_shows_hint_by_flag_not_key', () => {
    const fields = new Map(schema.fields.map((f): [string, FieldDesc] => [f.key, f]));
    const cot = schema.groups.find((g) => g.id === 'cot')!;
    const values = fieldsIn(schema, 'cot').map((f) => ({
      key: f.key,
      value: null,
      effective: f.kind === 'bool' ? true : 3,
      source: 'builtin' as const,
      overridden_by: [],
      editable: true,
    }));
    const host = document.createElement('div');
    const d: ConfigDraft = new ConfigDraft({ level: 'global' }, 'config.yaml', () => fields, (async () => ({})) as never, () =>
      render(groupSection(cot, fieldsIn(schema, 'cot'), { draft: d, level: 'global' }), host),
    );
    d.setView({ scope: { level: 'global' }, revision: 'r', values });
    const inverted = schema.fields.filter((f) => f.inverted).map((f) => f.key);
    expect(inverted.length).toBeGreaterThan(0);
    const tagged = [...host.querySelectorAll('.inv-tag')].map((t) => t.closest('.row')!.getAttribute('data-key'));
    expect(tagged.sort()).toEqual(inverted.filter((k) => fieldsIn(schema, 'cot').some((f) => f.key === k)).sort());
    expect(host.querySelector('.inv-tag')!.getAttribute('title')).toMatch(/开关打开对应 \w+: false/);
  });
});

describe('表情盘', () => {
  const REACTION_DEFAULTS: Record<string, string> = {
    'messaging.working_reaction': 'OnIt',
    'messaging.done_reaction': 'DONE',
    'messaging.interrupted_reaction': 'SILENT',
    'messaging.default_reaction': 'OK',
  };

  function behaviorDraft(withCandidates: boolean) {
    const fields = new Map(schema.fields.map((f): [string, FieldDesc] => [f.key, f]));
    const group = schema.groups.find((g) => g.id === 'behavior')!;
    const view: ConfigView = {
      scope: { level: 'global' },
      revision: 'r1',
      values: fieldsIn(schema, 'behavior')
        .filter((f) => f.layers.includes('global'))
        .map((f) => ({
          key: f.key,
          value: null,
          effective: REACTION_DEFAULTS[f.key] ?? null,
          source: 'builtin',
          overridden_by: [],
          editable: true,
        })),
    };
    if (withCandidates) {
      view.reactions = [
        { value: 'OK', label: '好的' },
        { value: 'DONE', label: '完成' },
        { value: 'THUMBSUP', label: '点赞' },
        { value: 'MeMeMe', label: '举手 / 我来' },
        { value: 'OnIt', label: '在做了' },
        { value: 'SILENT', label: '安静' },
        { value: 'LGTM', label: '看起来不错' },
        { value: 'MadeUpKey', label: '自造' },
      ];
    }
    const host = document.createElement('div');
    const draw = () => render(groupSection(group, fieldsIn(schema, 'behavior'), { draft: d, level: 'global' }), host);
    const d: ConfigDraft = new ConfigDraft({ level: 'global' }, 'config.yaml', () => fields, (async () => ({})) as never, draw);
    host.addEventListener(RERENDER_EVENT, draw);
    d.setView(view);
    const tile = (key: string) => [...host.querySelectorAll<HTMLButtonElement>('.rx-tile')].find((b) => b.title.startsWith(`${key} `) || b.title === key)!;
    const slot = (key: string) => host.querySelector<HTMLButtonElement>(`[id="f-${key}"] .rx-pick`)!;
    return { d, host, tile, slot };
  }

  test('reaction_keys_merge_into_one_block_and_tiles_fill_the_active_slot', () => {
    const { d, host, tile, slot } = behaviorDraft(true);
    expect(host.querySelectorAll('.row--reactions')).toHaveLength(1);
    expect(host.querySelectorAll('.rx-slot')).toHaveLength(4);
    expect(host.querySelector('.row[data-key="messaging.done_reaction"]')).toBeNull();
    slot('messaging.done_reaction').click();
    expect(slot('messaging.done_reaction').getAttribute('aria-pressed')).toBe('true');
    tile('THUMBSUP').click();
    expect(d.changeList()).toEqual([{ key: 'messaging.done_reaction', value: 'THUMBSUP' }]);
    expect(host.querySelector('[id="f-messaging.done_reaction"] .rx-key')!.textContent).toBe('THUMBSUP');
    // 选回原来那个就不算改动
    tile('DONE').click();
    expect(d.changeList()).toEqual([]);
    // 有飞书原图的画原图;既没原图也没近似 emoji 的写含义
    expect(tile('LGTM').querySelector('img')).not.toBeNull();
    expect(tile('MadeUpKey').querySelector('img')).toBeNull();
    expect(tile('MadeUpKey').textContent!.trim()).toBe('自造');
  });

  test('reaction_search_matches_key_or_meaning', () => {
    const { host } = behaviorDraft(true);
    const input = host.querySelector<HTMLInputElement>('.rx-search input')!;
    input.value = '点赞';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    expect([...host.querySelectorAll('.rx-tile')].map((b) => (b as HTMLElement).title)).toEqual(['THUMBSUP · 点赞']);
    input.value = 'oni';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    expect([...host.querySelectorAll('.rx-tile')].map((b) => (b as HTMLElement).title)).toEqual(['OnIt · 在做了']);
    input.value = '';
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });

  test('raise_hand_can_be_found_by_chinese_name_and_selected', () => {
    const { d, host, tile, slot } = behaviorDraft(true);
    slot('messaging.default_reaction').click();
    const input = host.querySelector<HTMLInputElement>('[aria-label="搜索表情"]')!;
    input.value = '举手';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    expect(host.querySelectorAll('.rx-tile')).toHaveLength(1);
    expect(tile('MeMeMe').querySelector('img')?.getAttribute('src')).toBe('/emoji/MeMeMe');
    tile('MeMeMe').click();
    expect(d.changeList()).toEqual([{ key: 'messaging.default_reaction', value: 'MeMeMe' }]);
    input.value = '';
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });

  test('without_candidates_reaction_keys_stay_plain_rows', () => {
    const { host } = behaviorDraft(false);
    expect(host.querySelector('.row--reactions')).toBeNull();
    expect(host.querySelector('.row[data-key="messaging.done_reaction"]')).not.toBeNull();
  });
});

describe('全局设置里的主 agent', () => {
  function primaryDraft(value: string | null) {
    const fields = new Map(schema.fields.map((f): [string, FieldDesc] => [f.key, f]));
    const field = fields.get('primary_agent')!;
    const view: ConfigView = {
      scope: { level: 'global' },
      revision: 'r1',
      values: [{ key: 'primary_agent', value, effective: value, source: value ? 'global' : 'builtin', overridden_by: [], editable: true }],
    };
    const host = document.createElement('div');
    const options = { primary_agent: [{ value: '', label: '自动' }, { value: 'alpha', label: 'alpha' }, { value: 'beta', label: '示例 · beta' }] };
    const d: ConfigDraft = new ConfigDraft({ level: 'global' }, 'config.yaml', () => fields, (async () => ({})) as never, () =>
      render(groupSection(schema.groups.find((g) => g.id === field.group)!, [field], { draft: d, level: 'global', options }), host),
    );
    d.setView(view);
    const select = () => host.querySelector<HTMLSelectElement>('[data-key="primary_agent"] select')!;
    const choose = (v: string) => {
      select().value = v;
      select().dispatchEvent(new Event('change'));
    };
    return { d, select, choose };
  }

  test('primary_agent_is_a_dropdown_with_automatic_first', () => {
    const { d, select, choose } = primaryDraft(null);
    expect([...select().options].map((o) => o.textContent)).toEqual(['自动', 'alpha', '示例 · beta']);
    expect(select().value).toBe('');
    choose('beta');
    expect(d.changeList()).toEqual([{ key: 'primary_agent', value: 'beta' }]);
    // 选回「自动」:没有改动,不写空字符串
    choose('');
    expect(d.changeList()).toEqual([]);
  });

  test('choosing_automatic_clears_a_configured_primary_agent', () => {
    const { d, select, choose } = primaryDraft('alpha');
    expect(select().value).toBe('alpha');
    choose('');
    expect(d.changeList()).toEqual([{ key: 'primary_agent', value: null }]);
  });

  test('candidates_are_automatic_plus_the_usable_agents', () => {
    const before = store.state;
    const agent = (alias: string, display: string, over: Record<string, unknown> = {}) => ({ alias, name: alias, display, cli: 'claude', enabled: true, routable: true, credentials: 'ready', chats: 0, ...over });
    store.state = { ...store.state, status: { agents: [agent('alpha', ''), agent('beta', '示例'), agent('gamma', '', { enabled: false }), agent('delta', '', { routable: false }), agent('eps', '', { load_error: 'x' })] } as never };
    try {
      expect(primaryAgentOptions()).toEqual([{ value: '', label: '自动' }, { value: 'alpha', label: 'alpha' }, { value: 'beta', label: '示例 · beta' }]);
    } finally {
      store.state = before;
    }
  });
});
