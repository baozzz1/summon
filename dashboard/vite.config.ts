import { readdirSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import type { Plugin } from 'vite';
import { defineConfig } from 'vitest/config';

// 产物里打进了第三方代码与图标(Lit、Lucide),它们的许可证要求随产物附上版权与许可声明。
// LICENSES/ 下每个文件是一份上游声明,构建时合成一个文件放进 dist,随 dist 编进 summon 二进制。
function thirdPartyLicenses(): Plugin {
  const dir = resolve(import.meta.dirname, 'LICENSES');
  return {
    name: 'summon-third-party-licenses',
    apply: 'build',
    generateBundle() {
      const parts = readdirSync(dir)
        .filter((name) => name.endsWith('.txt'))
        .sort()
        .map((name) => `==== ${name} ====\n\n${readFileSync(resolve(dir, name), 'utf8').trimEnd()}\n`);
      this.emitFile({
        type: 'asset',
        fileName: 'assets/third-party-licenses.txt',
        source: `Third-party notices for the summon dashboard bundle.\n\n${parts.join('\n')}`,
      });
    },
  };
}

function localizedHtml(): Plugin {
  const catalogs = Object.fromEntries(['zh-CN', 'en'].map((language) => [
    language, JSON.parse(readFileSync(resolve(import.meta.dirname, `../locales/dashboard/${language}.json`), 'utf8')) as Record<string, string>,
  ]));
  const escape = (text: string) => text.replace(/[&<>"']/g, (character) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[character]!);
  const translate = (html: string, language: string) => html.replaceAll('__SUMMON_LANGUAGE__', language)
    .replace(/__SUMMON_TEXT_([a-z.]+)__/g, (_, key: string) => {
      const value = catalogs[language]?.[key];
      if (value === undefined) throw new Error(`Missing HTML locale key ${language}:${key}`);
      return escape(value);
    });
  return {
    name: 'summon-localized-html',
    transformIndexHtml(html, context) { return context.server ? translate(html, 'zh-CN') : html; },
    generateBundle: {
      order: 'post',
      handler(_options, bundle) {
        for (const [name, item] of Object.entries(bundle)) {
          if (item.type !== 'asset' || !name.endsWith('.html')) continue;
          const html = typeof item.source === 'string' ? item.source : Buffer.from(item.source).toString('utf8');
          item.source = translate(html, 'zh-CN');
          this.emitFile({ type: 'asset', fileName: name.replace(/\.html$/, '.en.html'), source: translate(html, 'en') });
        }
      },
    },
  };
}

// 三个入口:控制台本体、远程链接的落地页(waiting.html)、本机码落地页。CSP 不允许内联脚本与内联样式,
// 所以产物一律是带摘要的外部文件,不让 Vite 把小资源内联成 data URI。
// 示例环境(src/api/mock.ts)只在开发服务器与截图构建(--mode mock)里打开;生产构建里这个常量
// 是 false,整段连同 mock 模块一起被摇掉。
export default defineConfig(({ mode }) => ({
  define: { __SUMMON_MOCK__: JSON.stringify(mode === 'development' || mode === 'mock') },
  base: '/',
  plugins: [thirdPartyLicenses(), localizedHtml()],
  // JSON 只按 default 导入。具名导出会让按需加载的语言包里每个 key 多一个变量与一条导出,体积翻倍。
  json: { namedExports: false },
  // summon 图标的几何在 menubar/Icons(Menu Bar 与控制台共用一份),开发服务器与测试要能读到它。
  server: { fs: { allow: [import.meta.dirname, resolve(import.meta.dirname, '../menubar/Icons')] } },
  build: {
    target: 'es2022',
    outDir: 'dist',
    emptyOutDir: true,
    assetsInlineLimit: 0,
    modulePreload: { polyfill: false },
    reportCompressedSize: false,
    rolldownOptions: {
      input: {
        index: resolve(import.meta.dirname, 'index.html'),
        waiting: resolve(import.meta.dirname, 'waiting.html'),
        local: resolve(import.meta.dirname, 'local.html'),
      },
    },
  },
  test: {
    environment: 'happy-dom',
    include: ['src/**/*.test.ts'],
  },
}));
