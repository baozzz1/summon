import { describe, expect, test } from 'vitest';
import type { Spend } from '../api/types';
import { costValue, nextSort, sortedRows } from './usage-sort';

describe('用量表排序', () => {
  test('numbers_use_raw_values_and_equal_values_stay_stable', () => {
    const rows = [{ id: 'a', n: 100 }, { id: 'b', n: 9 }, { id: 'c', n: 20 }, { id: 'd', n: 20 }];
    expect(sortedRows(rows, { key: 'n', direction: 'ascending' }, (r) => r.n).map((r) => r.id)).toEqual(['b', 'c', 'd', 'a']);
    expect(sortedRows(rows, { key: 'n', direction: 'descending' }, (r) => r.n).map((r) => r.id)).toEqual(['a', 'c', 'd', 'b']);
    expect(rows.map((r) => r.id)).toEqual(['a', 'b', 'c', 'd']);
  });

  test('missing_values_stay_last_in_both_directions_and_zero_is_a_value', () => {
    const rows = [{ id: 'missing' }, { id: 'zero', n: 0 }, { id: 'some', n: 0.01 }];
    for (const direction of ['ascending', 'descending'] as const) {
      const sorted = sortedRows(rows, { key: 'n', direction }, (r) => r.n);
      expect(sorted.at(-1)!.id).toBe('missing');
      expect(sorted[0]!.id).toBe(direction === 'ascending' ? 'zero' : 'some');
    }
  });

  test('dates_sort_across_year_boundaries_and_names_are_natural', () => {
    expect(sortedRows(['2026-12-31', '2027-01-01', '2026-01-01'], { key: 'name', direction: 'ascending' }, (r) => r)).toEqual(['2026-01-01', '2026-12-31', '2027-01-01']);
    expect(sortedRows(['Model 10', 'Model 2'], { key: 'name', direction: 'ascending' }, (r) => r)).toEqual(['Model 2', 'Model 10']);
    expect(nextSort(null, 'name', false).direction).toBe('ascending');
    expect(nextSort(null, 'cost', true).direction).toBe('descending');
  });

  test('costs_compare_known_amounts_and_keep_unpriced_amounts_missing', () => {
    const s: Spend = { tokens: { input: 10, output: 0, cache_read: 0, cache_write: 0 }, cost: { total: 0.1, input: 0.1, output: 0, cache_read: 0, cache_write: 0 } };
    expect(costValue(undefined)).toBeUndefined();
    expect(costValue(s)).toBe(0.1);
    expect(costValue(s, 'output')).toBe(0);
    expect(costValue({ ...s, unpriced: { tokens: 5, models: ['unknown'] } })).toBe(0.1);
    expect(costValue({ ...s, unpriced: { tokens: 10, models: ['unknown'] } })).toBeUndefined();
  });
});
