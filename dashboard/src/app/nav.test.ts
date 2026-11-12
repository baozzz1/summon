import { describe, expect, test } from 'vitest';
import schemaFixture from '../api/fixtures/config.schema.json';
import type { Schema } from '../api/types';
import { GLOBAL_DOC, globalGroupOf, globalSections } from './nav';
import { hrefFor, parseRoute } from './router';

const schema = (schemaFixture as unknown as { data: Schema }).data;

describe('全局设置的二级菜单', () => {
  test('global_group_round_trips_through_the_address', () => {
    expect(parseRoute('/global/sentinel')).toEqual({ page: 'global', mode: 'form', group: 'sentinel' });
    expect(parseRoute('/global', '?key=messaging.cot_visible')).toEqual({ page: 'global', mode: 'form', key: 'messaging.cot_visible' });
    expect(hrefFor({ page: 'global', mode: 'form', group: 'cot', key: 'messaging.cot_visible' })).toBe('/global/cot?key=messaging.cot_visible');
    expect(hrefFor({ page: 'global', mode: 'yaml', group: 'cot' })).toBe('/global/cot?mode=yaml');
    expect(parseRoute('/global/a/b')).toEqual({ page: 'not_found', path: '/global/a/b' });
  });

  test('sections_are_the_machine_groups_then_the_global_doc', () => {
    const ids = globalSections(schema).map((s) => s.id);
    const machine = schema.groups.filter((g) => g.section === 'messaging' || g.section === 'system').map((g) => g.id);
    expect(ids).toEqual([...machine, 'agent_permissions', GLOBAL_DOC]);
    expect(ids).not.toContain('agent_identity');
    expect(ids).not.toContain('agent_runtime');
  });

  test('page_group_comes_from_the_address_then_the_key_then_the_first_group', () => {
    expect(globalGroupOf({ page: 'global', mode: 'form', group: GLOBAL_DOC }, schema)).toBe(GLOBAL_DOC);
    expect(globalGroupOf({ page: 'global', mode: 'form', key: 'sentinel.enabled' }, schema)).toBe('sentinel');
    expect(globalGroupOf({ page: 'global', mode: 'form', key: 'feishu.operators' }, schema)).toBe('agent_permissions');
    const first = globalSections(schema)[0]!.id;
    expect(globalGroupOf({ page: 'global', mode: 'form', group: 'no-such-group' }, schema)).toBe(first);
    // agent 层才有的键不在全局设置的任何一组里
    expect(globalGroupOf({ page: 'global', mode: 'form', key: 'runtime.cli' }, schema)).toBe(first);
  });
});
