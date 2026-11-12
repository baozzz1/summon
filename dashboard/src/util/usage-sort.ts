import { language } from '../i18n';
import type { Spend } from '../api/types';
import { tokenTotal } from './spend';

export interface SortState<K extends string> {
  key: K;
  direction: 'ascending' | 'descending';
}

export function nextSort<K extends string>(current: SortState<K> | null, key: K, numeric: boolean): SortState<K> {
  return { key, direction: current?.key === key ? (current.direction === 'ascending' ? 'descending' : 'ascending') : numeric ? 'descending' : 'ascending' };
}

/** 缺失读数始终排在末尾,同值保留原来的顺序。 */
export function sortedRows<T, K extends string>(rows: T[], sort: SortState<K> | null, value: (row: T, key: K) => string | number | undefined): T[] {
  if (!sort) return rows;
  return [...rows].sort((a, b) => {
    const av = value(a, sort.key);
    const bv = value(b, sort.key);
    if (av === undefined) return bv === undefined ? 0 : 1;
    if (bv === undefined) return -1;
    const cmp = typeof av === 'number' && typeof bv === 'number' ? av - bv : String(av).localeCompare(String(bv), language(), { numeric: true });
    return sort.direction === 'ascending' ? cmp : -cmp;
  });
}

/** 完全未计价时没有可比较的金额;部分计价沿用已知金额。 */
export function costValue(spend: Spend | undefined, key: keyof Spend['cost'] = 'total'): number | undefined {
  if (!spend || (spend.unpriced && spend.unpriced.tokens >= tokenTotal(spend.tokens))) return undefined;
  return spend.cost[key];
}
