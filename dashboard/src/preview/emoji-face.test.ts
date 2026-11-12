import { render } from 'lit';
import { describe, expect, test } from 'vitest';
import { emojiFace } from './emoji-face';
import table from './feishu-emoji.json';

describe('飞书表情', () => {
  test('emoji_table_points_at_https_images', () => {
    const urls = Object.values(table as Record<string, string>);
    expect(urls.length).toBeGreaterThan(150);
    for (const u of urls) expect(new URL(u).protocol).toBe('https:');
  });

  test('face_shows_the_fallback_until_the_same_origin_image_loads', () => {
    const host = document.createElement('div');
    render(emojiFace('OK', '好的'), host);
    const img = host.querySelector('img')!;
    expect(img.getAttribute('src')).toBe('/emoji/OK');
    const face = host.querySelector('.emo-face')!;
    expect(face.hasAttribute('data-loaded')).toBe(false);
    expect(host.querySelector('.emo-glyph')!.textContent).toBe('👌');
    img.dispatchEvent(new Event('load'));
    expect(face.hasAttribute('data-loaded')).toBe(true);

    // 换成另一个表情:新节点,不带上一张图的加载状态
    render(emojiFace('DONE', '完成'), host);
    expect(host.querySelector('img')!.getAttribute('src')).toBe('/emoji/DONE');
    expect(host.querySelector('.emo-face')!.hasAttribute('data-loaded')).toBe(false);

    render(emojiFace('NO_SUCH_KEY', '某个含义'), host);
    expect(host.querySelector('img')).toBeNull();
    expect(host.textContent!.trim()).toBe('某个含义');
  });
});
