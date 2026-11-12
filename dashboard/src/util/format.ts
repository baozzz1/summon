import { t as translate } from '../i18n';
// 只做展示层的格式化:时间、时长、值的人话写法。不推断任何配置语义。

const pad = (n: number) => String(n).padStart(2, '0');

export function fmtDateTime(iso: string | undefined, now = new Date()): string {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  const sameYear = d.getFullYear() === now.getFullYear();
  const day = startOfDay(d);
  const today = startOfDay(now);
  const diffDays = Math.round((day - today) / 86_400_000);
  if (diffDays === 0) return translate('dd33b8d', [hm]);
  if (diffDays === 1) return translate('dd26688', [hm]);
  if (diffDays === -1) return translate('d61fa60', [hm]);
  const md = `${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  return sameYear ? `${md} ${hm}` : `${d.getFullYear()}-${md} ${hm}`;
}

function startOfDay(d: Date): number {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
}

export function fmtRelative(iso: string | undefined, now = new Date()): string {
  if (!iso) return '—';
  const t = new Date(iso).getTime();
  if (Number.isNaN(t)) return iso;
  const secs = Math.round((now.getTime() - t) / 1000);
  if (secs < 60) return translate('dde6785');
  if (secs < 3600) return translate('d66f93f', [Math.floor(secs / 60)]);
  if (secs < 86_400) return translate('d6504a9', [Math.floor(secs / 3600)]);
  if (secs < 86_400 * 2) return translate('d0c1848');
  if (secs < 86_400 * 30) return translate('d09f12d', [Math.floor(secs / 86_400)]);
  return fmtDateTime(iso, now);
}

/** 从某个时刻到现在过了多久:「42 秒」「3 分 12 秒」「1 小时 5 分」「2 天 3 小时」 */
export function fmtElapsed(sinceIso: string, now = new Date()): string {
  const secs = Math.max(0, Math.round((now.getTime() - new Date(sinceIso).getTime()) / 1000));
  if (secs < 60) return translate('d523fb6', [secs]);
  if (secs < 3600) return translate('df0bdd8', [Math.floor(secs / 60), secs % 60]);
  if (secs < 86_400) return translate('d5c8239', [Math.floor(secs / 3600), Math.floor((secs % 3600) / 60)]);
  return translate('d295f36', [Math.floor(secs / 86_400), Math.floor((secs % 86_400) / 3600)]);
}

/** 运行时长:「3 天 4 小时」「25 分钟」 */
export function fmtUptime(startedIso: string, now = new Date()): string {
  const secs = Math.max(0, Math.round((now.getTime() - new Date(startedIso).getTime()) / 1000));
  if (secs < 3600) return translate('dc65627', [Math.max(1, Math.floor(secs / 60))]);
  if (secs < 86_400) return translate('d2a1d95', [Math.floor(secs / 3600)]);
  const d = Math.floor(secs / 86_400);
  const h = Math.floor((secs % 86_400) / 3600);
  return h ? translate('d295f36', [d, h]) : translate('d84b058', [d]);
}

export function fmtUptimeShort(startedIso: string, now = new Date()): string {
  const secs = Math.max(0, Math.round((now.getTime() - new Date(startedIso).getTime()) / 1000));
  if (secs < 3600) return `${Math.max(1, Math.floor(secs / 60))}m`;
  if (secs < 86_400) return `${Math.floor(secs / 3600)}h`;
  return `${Math.floor(secs / 86_400)}d`;
}

export const DURATION_UNITS = [
  { secs: 86_400, get label() { return translate('d49da61'); } },
  { secs: 3600, get label() { return translate('d3b6fef'); } },
  { secs: 60, get label() { return translate('dbd957b'); } },
  { secs: 1, get label() { return translate('d9dcdc2'); } },
] as const;

/** 秒数的人话:「55 分钟」「1 天」「1 小时 30 分钟」;0 / 空写「关」。 */
export function fmtDuration(secs: unknown): string {
  if (secs === null || secs === undefined || secs === '') return translate('d5d0ae6');
  const n = Number(secs);
  if (!Number.isFinite(n)) return String(secs);
  if (n <= 0) return translate('d5d0ae6');
  const parts: string[] = [];
  let rest = Math.round(n);
  for (const u of DURATION_UNITS) {
    if (rest >= u.secs) {
      const q = Math.floor(rest / u.secs);
      rest -= q * u.secs;
      parts.push(`${q} ${u.label}`);
    }
    if (parts.length === 2) break;
  }
  return parts.join(' ');
}

/** 计数:1,000 以下照写,往上用 K / M / B,保留一位小数并省略 .0。 */
export function fmtCount(n: number): string {
  const short = (scale: number, unit: string) => {
    const value = Math.round(n / (scale / 10)) / 10;
    return `${value.toFixed(1).replace(/\.0$/, '')}${unit}`;
  };
  if (n < 1_000) return String(n);
  if (n < 1_000_000) return short(1_000, 'K');
  if (n < 1_000_000_000) return short(1_000_000, 'M');
  return short(1_000_000_000, 'B');
}

/** 美元金额:不小于 1 保留两位小数,小于 1 保留三位有效数字,0 写 $0。 */
export function fmtUsd(v: number): string {
  if (!v) return '$0';
  if (v >= 1) return `$${v.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
  return `$${v.toLocaleString('en-US', { maximumSignificantDigits: 3 })}`;
}

/** 把秒数拆成最大的整除单位,控件里用:3300 → 55 分钟 */
export function splitDuration(secs: number): { amount: number; unit: number } {
  if (secs <= 0) return { amount: 0, unit: 60 };
  for (const u of DURATION_UNITS) {
    if (secs % u.secs === 0) return { amount: secs / u.secs, unit: u.secs };
  }
  return { amount: secs, unit: 1 };
}

/** 设置值的简短文字形态(原值、继承值的提示里用)。 */
export function fmtValue(v: unknown, kind?: string, options?: Array<{ value: string; label: string }>): string {
  if (v === null || v === undefined) return kind === 'duration_secs' ? translate('d5d0ae6') : translate('d2f5f1d');
  if (kind === 'duration_secs') return fmtDuration(v);
  if (typeof v === 'boolean') return v ? translate('d39eae6') : translate('d5d0ae6');
  if (options) {
    const hit = options.find((o) => o.value === v);
    if (hit) return hit.label;
  }
  if (Array.isArray(v)) return v.length ? v.join('、') : translate('d2f8267');
  if (typeof v === 'object') {
    const n = Object.keys(v as object).length;
    return n ? translate('d8af892', [n]) : translate('d2f8267');
  }
  if (v === '') return translate('d2f8267');
  return String(v);
}

/** 稳定的深比较:对象按键排序后比 JSON。草稿与原值相等时就不算改动。 */
export function sameValue(a: unknown, b: unknown): boolean {
  return stableJson(a) === stableJson(b);
}

export function stableJson(v: unknown): string {
  return JSON.stringify(v, (_k, val: unknown) => {
    if (val && typeof val === 'object' && !Array.isArray(val)) {
      return Object.fromEntries(Object.entries(val as Record<string, unknown>).sort(([x], [y]) => (x < y ? -1 : x > y ? 1 : 0)));
    }
    return val;
  });
}

export const LAYER_LABEL: Record<string, string> = {
  get builtin() { return translate('d844b8c'); },
  get global() { return translate('d63d6b4'); },
  agent: 'agent',
  get chat() { return translate('d0d5f45'); },
};

export const EFFECT_LABEL: Record<string, string> = {
  get immediate() { return translate('d0d8877'); },
  get next_turn() { return translate('de960e7'); },
  get new_session() { return translate('dcf9de4'); },
  get next_poll() { return translate('d36b0e7'); },
  get restart() { return translate('db4c6f0'); },
};

export function shortId(id: string): string {
  return id.length > 14 ? `${id.slice(0, 7)}…${id.slice(-4)}` : id;
}
