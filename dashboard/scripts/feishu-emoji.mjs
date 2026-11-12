// 从飞书开放平台的「表情文案说明」生成表情键 → 飞书官方表情图地址的对照表(src/preview/feishu-emoji.json)。
// daemon 按它去飞书的 CDN 取图,页面按它判断哪些表情有图。飞书增改表情时重跑:node scripts/feishu-emoji.mjs
import { writeFileSync } from 'node:fs';

const DOC = 'https://open.feishu.cn/api/tools/document/detail?fullPath=/server-docs/im-v1/message-reaction/emojis-introduce';

const res = await fetch(DOC);
if (!res.ok) throw new Error(`取文档失败:HTTP ${res.status}`);
const content = (await res.json())?.data?.document?.content ?? '';
const cells = [...content.matchAll(/<md-td>\s*!\[/g)].length;
const map = new Map();
for (const m of content.matchAll(/<md-td>\s*!\[[^\]]*\]\(([^)\s]+)\)\s*<\/md-td>\s*<md-td>\s*([^<\s]+)\s*<\/md-td>/g)) {
  const url = new URL(m[1].startsWith('//') ? `https:${m[1]}` : m[1]);
  url.protocol = 'https:';
  url.search = '';
  map.set(m[2], url.toString());
}
// 每个带图的格子都要认出一条:文档改了版式时宁可失败,也不写出一份残缺的表。
if (map.size === 0 || map.size !== cells) throw new Error(`文档里有 ${cells} 张表情图,只认出 ${map.size} 条`);
const sorted = Object.fromEntries([...map.keys()].sort((a, b) => a.localeCompare(b, 'en')).map((k) => [k, map.get(k)]));
writeFileSync(new URL('../src/preview/feishu-emoji.json', import.meta.url), `${JSON.stringify(sorted, null, 2)}\n`);
console.log(`${map.size} 个表情`);
