// 体积预算:打开任意一个页面时实际加载的 JS —— 入口、该页面的分块及它们静态引入的分块,加上一份
// 语言包 —— gzip 后 ≤ 80 KB;首屏 CSS ≤ 20 KB。页面与语言包都按需加载:没打开的页面不算进来,
// 语言包每次只加载正在用的一种,按较重的那一种计。
import { readdirSync, readFileSync, existsSync } from 'node:fs';
import { gzipSync } from 'node:zlib';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const dist = resolve(dirname(fileURLToPath(import.meta.url)), '..', 'dist');
const JS_BUDGET = 80 * 1024;
const CSS_BUDGET = 20 * 1024;

if (!existsSync(join(dist, 'index.html'))) {
  console.error('dist/ 不存在,先运行 npm run build');
  process.exit(1);
}
const gz = (file) => gzipSync(readFileSync(file), { level: 9 }).length;
const assets = join(dist, 'assets');
const files = readdirSync(assets);
const html = readFileSync(join(dist, 'index.html'), 'utf8');
const css = [...html.matchAll(/href="\/assets\/([^"]+\.css)"/g)].map((m) => m[1]);
const entry = [...html.matchAll(/src="\/assets\/([^"]+\.js)"/g)].map((m) => m[1]);

const source = (f) => readFileSync(join(assets, f), 'utf8');
// 压缩后的静态引入形如 `import{a}from"./x.js"`、`import"./x.js"`;按需加载的 `import(`./x.js`)` 带括号,不算。
const staticDeps = (f) => [...source(f).matchAll(/(?:\bfrom|\bimport)\s*["'`]\.\/([^"'`]+\.js)["'`]/g)].map((m) => m[1]);
const closure = (roots) => {
  const seen = new Set();
  const walk = (f) => {
    if (seen.has(f)) return;
    seen.add(f);
    staticDeps(f).forEach(walk);
  };
  roots.forEach(walk);
  return seen;
};
const dynamicDeps = (f) => [...source(f).matchAll(/\bimport\(\s*["'`]\.\/([^"'`]+\.js)["'`]\s*\)/g)].map((m) => m[1]);
// 语言包是 locales/dashboard/<语言>.json 各自的分块,由 i18n 按需加载;其余按需加载的分块都是页面。
const isLocale = (f) => /^(zh-CN|en)-[\w-]+\.js$/.test(f);
const startup = closure(entry);
const dynamic = [...new Set([...startup].flatMap(dynamicDeps))];
const localeChunks = dynamic.filter(isLocale);
const pages = dynamic.filter((f) => !isLocale(f));

const kb = (n) => `${(n / 1024).toFixed(1)} KB`;
let ok = true;
if (!pages.length) {
  console.error('入口里认不出按需加载的页面分块:构建产物的形状变了,先更新这个脚本');
  ok = false;
}
if (localeChunks.length !== 2) {
  console.error(`认出的语言包分块不是两个(${localeChunks.join(', ') || '无'}):构建产物的形状变了,先更新这个脚本`);
  ok = false;
}
const total = (set) => [...set].reduce((n, f) => n + gz(join(assets, f)), 0);
/** 根分块加上最重的一种语言包。 */
const loadedSize = (roots) => Math.max(0, ...(localeChunks.length ? localeChunks : [null]).map((locale) => total(closure(locale ? [...roots, locale] : roots))));
const sizes = pages.map((page) => ({ page, size: loadedSize([...entry, page]) })).sort((a, b) => b.size - a.size);
const cssTotal = css.reduce((n, f) => n + gz(join(assets, f)), 0);
const entryTotal = loadedSize(entry);
console.log(`JS  入口 ${kb(entryTotal)} · 最重的页面 ${sizes[0] ? `${sizes[0].page.replace(/-[\w]+\.js$/, '')} ${kb(sizes[0].size)}` : '—'}(每页预算 ${kb(JS_BUDGET)})`);
console.log(`CSS 首屏 ${kb(cssTotal)}(预算 ${kb(CSS_BUDGET)})`);
for (const { page, size } of sizes.filter((s) => s.size > JS_BUDGET)) {
  console.error(`JS 超出预算:${page} ${kb(size)}`);
  ok = false;
}
if (cssTotal > CSS_BUDGET) {
  console.error('CSS 超出预算');
  ok = false;
}
if (files.some((f) => /mock/i.test(f))) {
  console.error('产物里混进了示例环境(mock)');
  ok = false;
}
process.exit(ok ? 0 : 1);
