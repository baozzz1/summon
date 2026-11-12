// 页面上的群 ID、open_id、union_id 反查名字。同一轮渲染里要查的 ID 攒成一批问服务端,结果缓存在
// 页面里;查不到的也记下,不反复问。
import { store } from './store';

const cache = new Map<string, string | null>();
const inflight = new Set<string>();
/** 按 agent 分批:open_id 按应用区分,服务端先用这个 agent 的应用查。 */
const queued = new Map<string, Set<string>>();
let timer: ReturnType<typeof setTimeout> | undefined;

export const names = new EventTarget();
const BATCH = 200;

export function isLookupId(id: string): boolean {
  return /^(oc|ou|on)_[0-9a-z]+$/i.test(id);
}

/** 已知的名字;还没查过就排进下一批,查到后发 `change`。 */
export function nameOf(id: string, agent = ''): string | undefined {
  if (cache.has(id)) return cache.get(id) ?? undefined;
  if (!isLookupId(id) || inflight.has(id)) return undefined;
  const set = queued.get(agent) ?? new Set<string>();
  queued.set(agent, set);
  set.add(id);
  timer ??= setTimeout(flush, 30);
  return undefined;
}

/** 服务端随数据一起给了名字(如名单成员)时先记下,免得再问一次。 */
export function rememberName(id: string, name: string | undefined): void {
  if (name) cache.set(id, name);
}

async function flush(): Promise<void> {
  timer = undefined;
  const batches = [...queued];
  queued.clear();
  for (const [agent, set] of batches) {
    const ids = [...set].filter((id) => !cache.has(id));
    for (let i = 0; i < ids.length; i += BATCH) {
      const chunk = ids.slice(i, i + BATCH);
      chunk.forEach((id) => inflight.add(id));
      try {
        const found = await store.call('names.resolve', agent ? { agent, ids: chunk } : { ids: chunk });
        for (const id of chunk) cache.set(id, found[id] ?? null);
      } catch {
        // 这一批没问成:不记成「查不到」,下次渲染再问
      } finally {
        chunk.forEach((id) => inflight.delete(id));
      }
    }
  }
  names.dispatchEvent(new Event('change'));
}
