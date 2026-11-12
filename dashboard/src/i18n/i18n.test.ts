import { afterEach, expect, test, vi } from 'vitest';
import { html, render } from 'lit';
import chinese from '../../../locales/dashboard/zh-CN.json';
import english from '../../../locales/dashboard/en.json';
import { language, languageEvents, rich, setLanguage, t } from './index';
import { DateRangePicker } from '../components/date-range';
import { NAV_GROUPS } from '../app/nav';
import { EFFECT_LABEL } from '../util/format';
import { ConfigDraft } from '../app/editable';
import { dayLabel } from '../pages/usage';

afterEach(async () => { await setLanguage('zh-CN'); document.body.replaceChildren(); });

test('catalogs_have_identical_keys_and_placeholders_and_complete_English', () => {
  expect(Object.keys(english).sort()).toEqual(Object.keys(chinese).sort());
  for (const [key, zh] of Object.entries(chinese)) {
    const en = english[key as keyof typeof english];
    const placeholders = (s: string) => [...s.matchAll(/\{\d+\}/g)].map((m) => m[0]).sort();
    expect(placeholders(en), key).toEqual(placeholders(zh));
    expect(en, key).not.toMatch(/[\u3400-\u9fff]/u);
  }
});

test('language_changes_rerender_the_existing_calendar_and_keep_its_selection', async () => {
  const element = new DateRangePicker();
  element.today = '2026-11-08';
  element.query = { range: 'custom', from: '2026-11-01', to: '2026-11-03' };
  document.body.append(element);
  await element.updateComplete;
  element.querySelector<HTMLButtonElement>('.date-range-trigger')!.click();
  await element.updateComplete;
  const input = element.querySelector<HTMLInputElement>('input[type=date]')!;
  expect(input.value).toBe('2026-11-01');
  await setLanguage('en');
  await element.updateComplete;
  expect(element.querySelector<HTMLInputElement>('input[type=date]')).toBe(input);
  expect(input.value).toBe('2026-11-01');
  expect(element.querySelector('[aria-label="Choose date range"]')).not.toBeNull();
  expect(element.textContent).toContain('Start date');
  expect(dayLabel('2026-11-08')).toBe('11-8 Sun');
  expect(NAV_GROUPS[0]!.label).toBe('Overview');
  expect(EFFECT_LABEL.next_turn).toBe('Effective next turn');
});

test('rich_messages_keep_untrusted_values_as_text_and_draft_changes_survive_language_switches', async () => {
  const draft = new ConfigDraft({ level: 'global' }, () => t('db77c1d', ['sample']), () => new Map(), async () => { throw new Error('no network'); }, () => {});
  draft.changes.set('messaging.cot_visible', false);
  const target = document.createElement('div');
  const malicious = '<img src=x onerror=alert(1)>';
  render(html`${rich('db77c1d', [malicious])}`, target);
  expect(target.querySelector('img')).toBeNull();
  await setLanguage('en');
  expect(language()).toBe('en');
  expect(draft.changes.get('messaging.cot_visible')).toBe(false);
  expect(draft.label).toBe("sample's agent.yaml");
  render(html`${rich('db77c1d', [malicious])}`, target);
  expect(target.querySelector('img')).toBeNull();
  expect(target.textContent).toContain(malicious);
});

test('rapid_language_switches_settle_on_the_last_requested_language', async () => {
  const changes: string[] = [];
  const onChange = () => changes.push(language());
  languageEvents.addEventListener('change', onChange);
  try {
    const first = setLanguage('en');
    const second = setLanguage('zh-CN');
    expect(await first).toBe(false);
    expect(await second).toBe(false);
    expect(language()).toBe('zh-CN');
    expect(changes).toEqual([]);
    await setLanguage('en');
    expect(t('d9d7407')).toBe(english.d9d7407);
    expect(changes).toEqual(['en']);
  } finally {
    languageEvents.removeEventListener('change', onChange);
  }
});

test('repeating_the_language_being_loaded_does_not_cancel_the_switch', async () => {
  // 手动刷新与定时轮询先后报同一种新语言:后一次不作废前一次,两者等到同一个结果,只切换一次。
  const changes: string[] = [];
  const onChange = () => changes.push(language());
  languageEvents.addEventListener('change', onChange);
  try {
    const manual = setLanguage('en');
    const poll = setLanguage('en');
    expect(await manual).toBe(true);
    expect(await poll).toBe(true);
    expect(language()).toBe('en');
    expect(changes).toEqual(['en']);
    // 生效之后再报同一种:不再切换
    expect(await setLanguage('en')).toBe(false);
    expect(changes).toEqual(['en']);
  } finally {
    languageEvents.removeEventListener('change', onChange);
  }
});

test('first_load_uses_the_page_language_and_loads_only_that_catalog', async () => {
  // 页面按语言给出 index.html / index.en.html:模块加载时读 <html lang>,首屏只取这一种语言包。
  const before = document.documentElement.lang;
  document.documentElement.lang = 'en';
  vi.resetModules();
  try {
    const fresh = await import('./index');
    expect(fresh.language()).toBe('en');
    expect(fresh.t('d9d7407')).toBe(english.d9d7407);
    expect(await fresh.setLanguage('zh-CN')).toBe(true);
    expect(fresh.t('d9d7407')).toBe(chinese.d9d7407);
    expect(document.documentElement.lang).toBe('zh-CN');
  } finally {
    document.documentElement.lang = before;
    vi.resetModules();
  }
});
