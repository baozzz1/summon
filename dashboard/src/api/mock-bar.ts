// 示例环境的入口与左下角的小菜单(模拟别处改动、切换身份)。只在开发服务器与 --mode mock 构建里加载。
import { html, render } from 'lit';
import { store } from '../app/store';
import { toast } from '../components/toast';
import type { Transport } from './client';
import { createMockTransport, externalEdit, MockServer } from './mock';

export function startMock(): Transport {
  const q = new URLSearchParams(location.search);
  const srv = new MockServer({ identity: q.get('as') === 'owner' ? 'owner' : 'feishu' });
  // ?set=key=value,key=value:截图时直接改整机这一层,用来拍「拨开关前 / 后」
  for (const pair of (q.get('set') ?? '').split(',').filter(Boolean)) {
    const [k, v] = pair.split('=');
    if (k && v !== undefined) srv.global[k] = v === 'true' ? true : v === 'false' ? false : Number.isNaN(Number(v)) ? v : Number(v);
  }
  const theme = q.get('theme');
  if (theme === 'dark' || theme === 'light') document.documentElement.dataset.theme = theme;
  const base = createMockTransport(srv, q.has('shot') ? 0 : 60);
  const t: Transport = q.has('unauth')
    ? async () => ({ ok: false, error: { code: 'unauthenticated', message: '登录已失效' } })
    : q.has('replay')
      ? replayTransport()
      : base;
  (window as unknown as { summonMock: MockServer }).summonMock = srv;
  if (!q.has('shot')) mountBar(srv);
  return t;
}

/**
 * ?replay=1:每个方法原样回放 Rust 生成的契约样例(不看参数)。用真实服务端的响应形状把页面
 * 走一遍,看页面对真实数据画得对不对;写方法也只回样例,不改任何东西。
 */
function replayTransport(): Transport {
  const files = import.meta.glob<{ default: { data: unknown } }>('./fixtures/*.json', { eager: true });
  const byMethod = new Map(Object.entries(files).map(([p, m]) => [p.replace('./fixtures/', '').replace(/\.json$/, ''), m.default.data]));
  return async (method) => {
    const data = byMethod.get(method);
    return data === undefined
      ? { ok: false, error: { code: 'not_found', message: `没有 ${method} 的样例` } }
      : { ok: true, data: JSON.parse(JSON.stringify(data)) };
  };
}

function mountBar(srv: MockServer) {
  const host = document.createElement('details');
  host.className = 'mock-bar';
  const act = async (target: 'global' | 'starlight' | 'schedules') => {
    toast(externalEdit(srv, target), 'info');
    host.open = false;
    await store.refreshStatus();
  };
  render(
    html`<summary>示例数据</summary>
      <div class="mock-menu">
        <button @click=${() => act('global')}>模拟别处改了 config.yaml</button>
        <button @click=${() => act('starlight')}>模拟别处改了 starlight 的 agent.yaml</button>
        <button @click=${() => act('schedules')}>模拟 agent 改了一条定时任务</button>
        <button @click=${() => store.expire()}>模拟会话失效</button>
        <button @click=${() => {
          const q = new URLSearchParams(location.search);
          if (q.get('as') === 'owner') q.delete('as');
          else q.set('as', 'owner');
          location.search = q.toString();
        }}>
          切换身份(飞书 / 本机)
        </button>
      </div>`,
    host,
  );
  document.body.appendChild(host);
}
