// 入库的 dist/ 必须与当前源码重新构建的结果逐字节一致(Rust 编译期嵌入的就是它)。
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readdirSync, readFileSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const dist = join(root, 'dist');
const tmp = mkdtempSync(join(tmpdir(), 'summon-dash-dist-'));

function walk(dir) {
  return readdirSync(dir).flatMap((name) => {
    const p = join(dir, name);
    return statSync(p).isDirectory() ? walk(p) : [p];
  });
}

try {
  execFileSync('npx', ['vite', 'build', '--outDir', tmp, '--emptyOutDir', '--logLevel', 'warn'], { cwd: root, stdio: 'inherit' });
  const want = walk(tmp).map((p) => relative(tmp, p)).sort();
  const have = walk(dist).map((p) => relative(dist, p)).filter((p) => p !== '.gitkeep').sort();
  const diff = [];
  for (const f of new Set([...want, ...have])) {
    if (!want.includes(f)) diff.push(`多余:${f}`);
    else if (!have.includes(f)) diff.push(`缺少:${f}`);
    else if (!readFileSync(join(tmp, f)).equals(readFileSync(join(dist, f)))) diff.push(`内容不同:${f}`);
  }
  if (diff.length) {
    console.error(`dist/ 与源码构建结果不一致,运行 npm run build 后提交:\n  ${diff.join('\n  ')}`);
    process.exit(1);
  }
  console.log(`dist/ 与源码一致(${want.length} 个文件)`);
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
