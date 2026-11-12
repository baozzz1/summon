import { describe, expect, test } from 'vitest';
import { checkResult, METHOD_GUARDS, METHODS } from './guards';
import { createMockTransport, MockServer } from './mock';
import type { Method } from './types';

const files = import.meta.glob<{ default: { params: unknown; data: unknown } }>('./fixtures/*.json', { eager: true });
const fixtures = new Map(Object.entries(files).map(([path, mod]) => [path.replace('./fixtures/', '').replace(/\.json$/, ''), mod.default]));

describe('契约样例', () => {
  test('fixtures_match_types', () => {
    for (const m of METHODS) {
      const fx = fixtures.get(m);
      expect(fx, `缺少 fixtures/${m}.json`).toBeTruthy();
      expect(METHOD_GUARDS[m].params(fx!.params, `${m}.params`)).toBeNull();
      expect(METHOD_GUARDS[m].data(fx!.data, `${m}.data`)).toBeNull();
    }
  });

  test('comment_tasks_validate_every_field_without_accepting_private_content', () => {
    const data = fixtures.get('sessions.list')!.data as Record<string, unknown>;
    const task = { id: 'task', agent: 'writer', title: 'Document', stage: 'approval_failed',
      status: '审批复验失败', status_en: 'Approval failed', busy: false,
      received_ms: 100, updated_ms: 200, pid: null, error: '归属无法确认' };
    const g = METHOD_GUARDS['sessions.list'].data;
    expect(g({ ...data, comment_tasks: [task] }, 'x')).toBeNull();
    expect(g({ ...data, comment_tasks: [{ ...task, pid: '4242' }] }, 'x')).toMatch(/pid/);
    expect(g({ ...data, comment_tasks: [{ ...task, draft: 'private' }] }, 'x')).toMatch(/draft/);
  });

  test('every_fixture_names_a_known_method', () => {
    for (const name of fixtures.keys()) expect(METHODS).toContain(name);
  });

  test('guards_reject_renamed_missing_and_extra_fields', () => {
    const fx = fixtures.get('status.get')!.data as Record<string, unknown>;
    const g = METHOD_GUARDS['status.get'].data;
    const { hostname, ...rest } = fx;
    expect(g({ ...rest, host_name: hostname }, 'x')).toMatch(/hostname|host_name/);
    expect(g(rest, 'x')).toMatch(/缺少字段 hostname/);
    expect(g({ ...fx, extra: 1 }, 'x')).toMatch(/多出未知字段 extra/);
    const daemon = { ...(fx.daemon as object), autostart: 'maybe' };
    expect(g({ ...fx, daemon }, 'x')).toMatch(/autostart/);
  });

  test('guards_check_tagged_unions', () => {
    const g = METHOD_GUARDS['preview.render'].data;
    const fx = fixtures.get('preview.render')!.data as { messages: Array<Record<string, unknown>> };
    const bad = { ...fx, messages: [{ ...fx.messages[0], kind: 'banner' }] };
    expect(g(bad, 'x')).toMatch(/kind/);
    expect(checkResult('names.resolve', { ok: false, error: { code: 'nope', message: '' } })).toMatch(/code/);
    expect(checkResult('names.resolve', { ok: true, data: {} })).toBeNull();
  });
});

function currentRevision(srv: MockServer, m: Method, p: Record<string, unknown>): string | null {
  if (m === 'chats.set_operators' || m === 'chats.set_work_dir') return srv.revision(`agent:${String(p.agent)}`);
  if (m === 'docs.put') {
    const doc = p.doc as { level: string; agent?: string; chat_id?: string; file?: string };
    const path =
      doc.level === 'global'
        ? '~/.summon/CLAUDE.md'
        : doc.level === 'agent'
          ? `~/.summon/agents/${doc.agent}/${doc.file}`
          : `~/.summon/agents/${doc.agent}/chats/${doc.chat_id}/CLAUDE.md`;
    return srv.docs.get(path)?.exists ? srv.revision(`doc:${path}`) : null;
  }
  const scope = (p.scope ?? (p.file === 'agent' ? { level: 'agent', agent: p.agent } : { level: 'global' })) as { level: string; agent?: string };
  return srv.revision(scope.level === 'global' ? 'global' : `agent:${scope.agent}`);
}

describe('示例环境', () => {
  test('mock_conforms_to_contract_for_every_method', async () => {
    for (const m of METHODS as Method[]) {
      // 每个方法用一份新的示例数据:写方法会改状态,版本号也随之变化。
      const srv = new MockServer({ now: new Date('2026-10-20T14:32:00+08:00') });
      const t = createMockTransport(srv);
      const params = structuredClone(fixtures.get(m)!.params) as Record<string, unknown>;
      // 样例是 Rust 侧真实环境里的一次调用,参数原样用;只有版本号与核验任务号这种
      // 「上一次读取才拿得到」的值,换成示例环境此刻的(版本校验本身另有测试钉着)。
      if ('revision' in params) params.revision = currentRevision(srv, m, params);
      if (m === 'chats.mentionless_job') {
        const job = await t('chats.set_mentionless', fixtures.get('chats.set_mentionless')!.params, {});
        params.job = (job as { ok: true; data: { job: string } }).data.job;
      }
      const res = await t(m, params, {});
      if (m.startsWith('agents.cleanup.')) {
        expect(res.ok, `${m}: demo must not run destructive actions`).toBe(false);
      } else {
        expect(res.ok, `${m}: ${JSON.stringify(res)}`).toBe(true);
      }
      expect(checkResult(m, res), m).toBeNull();
    }
  });
});
