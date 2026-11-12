export type ThemePref = 'system' | 'light' | 'dark';

const KEY = 'summon-dash-theme';

// 偏好只是本浏览器的便利设置:读写失败(隐私模式、禁用存储)时按跟随系统处理,不影响页面。
export function loadTheme(): ThemePref {
  try {
    const v = localStorage.getItem(KEY);
    if (v === 'light' || v === 'dark' || v === 'system') return v;
  } catch {
    /* 存储不可用时跟随系统 */
  }
  return 'system';
}

export function applyTheme(pref: ThemePref): void {
  const root = document.documentElement;
  if (pref === 'system') delete root.dataset.theme;
  else root.dataset.theme = pref;
}

export function saveTheme(pref: ThemePref): void {
  applyTheme(pref);
  try {
    localStorage.setItem(KEY, pref);
  } catch {
    /* 存不下也照样生效到本页 */
  }
}
