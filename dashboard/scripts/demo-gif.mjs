// 生成 README 首屏的演示动图:起开发服务器打开演示页(src/demo/,画面是控制台的飞书效果预览),
// 按 src/demo/timeline.ts 的时间线逐帧截图,再用 ffmpeg(palettegen + paletteuse)合成循环播放的 GIF。
// 演示页不在任何构建入口里,dist/ 不受影响。浅色主题、减少动效(转圈与扫光停住,每帧都是确定的画面)。
//
// 用法:node scripts/demo-gif.mjs [输出 GIF](默认 ../assets/demo.gif)
//   CHROME=路径    Chrome / Chromium(默认是 macOS 上的 Google Chrome)
//   FFMPEG=路径    ffmpeg(默认 PATH 上的 ffmpeg),要有 gif 编码器与 palettegen / paletteuse 滤镜
//   FRAMES=目录    帧写到这个目录并保留(默认写进临时目录,合成后删掉)
//   FRAMES_ONLY=1  只出帧:写 PNG 帧与带每帧时长的 frames.ffconcat,打印合成命令,不调 ffmpeg
import { execFileSync, spawn } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, rmSync, statSync, writeFileSync } from 'node:fs';
import net from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const output = resolve(process.argv[2] ?? join(root, '..', 'assets', 'demo.gif'));
const chrome = process.env.CHROME ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const ffmpeg = process.env.FFMPEG ?? 'ffmpeg';
const framesOnly = process.env.FRAMES_ONLY === '1';
const port = 5176;
// 演示页按 720px 排版、2 倍像素比截图,合成时缩到 900px:正文约合 16px,缩放后的字边比 1 倍直出平滑
const PAGE_WIDTH = 720;
const PIXEL_RATIO = 2;
const GIF_WIDTH = 900;
const FILTER =
  `scale=${GIF_WIDTH}:-1:flags=lanczos,split[a][b];` +
  '[a]palettegen=stats_mode=full[p];[b][p]paletteuse=dither=none:diff_mode=rectangle';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function waitPort(p, ms = 30000) {
  const until = Date.now() + ms;
  return new Promise((ok, bad) => {
    const tryOnce = () => {
      const s = net.connect(p, '127.0.0.1');
      s.on('connect', () => {
        s.destroy();
        ok();
      });
      s.on('error', () => {
        s.destroy();
        if (Date.now() > until) bad(new Error('开发服务器没起来'));
        else setTimeout(tryOnce, 200);
      });
    };
    tryOnce();
  });
}

/** 最小的 DevTools 协议客户端(同 screenshots.mjs):按 id 配对应答,按 sessionId 分发事件。 */
class Cdp {
  constructor(url) {
    this.ws = new WebSocket(url);
    this.seq = 0;
    this.pending = new Map();
    this.waiters = [];
    this.ready = new Promise((ok, bad) => {
      this.ws.addEventListener('open', ok);
      this.ws.addEventListener('error', bad);
    });
    this.ws.addEventListener('message', (ev) => {
      const msg = JSON.parse(String(ev.data));
      if (msg.id && this.pending.has(msg.id)) {
        const { ok, bad } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) bad(new Error(`${msg.error.message}`));
        else ok(msg.result);
      } else if (msg.method) {
        this.waiters = this.waiters.filter((w) => !(w.method === msg.method && w.session === msg.sessionId && (w.ok(msg.params), true)));
      }
    });
  }
  send(method, params = {}, sessionId) {
    const id = ++this.seq;
    this.ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    return new Promise((ok, bad) => this.pending.set(id, { ok, bad }));
  }
  once(method, session, ms = 15000) {
    return new Promise((ok, bad) => {
      this.waiters.push({ method, session, ok });
      setTimeout(() => bad(new Error(`等 ${method} 超时`)), ms);
    });
  }
}

function startChrome(profile) {
  return new Promise((ok, bad) => {
    const proc = spawn(chrome, [
      '--headless=new',
      '--disable-gpu',
      '--hide-scrollbars',
      '--no-first-run',
      '--no-default-browser-check',
      '--remote-debugging-port=0',
      `--user-data-dir=${profile}`,
      'about:blank',
    ]);
    let buf = '';
    proc.stderr.on('data', (d) => {
      buf += d;
      const m = /DevTools listening on (ws:\/\/\S+)/.exec(buf);
      if (m) ok({ proc, ws: m[1] });
    });
    proc.once('exit', () => bad(new Error(`Chrome 退出了:${buf.slice(-400)}`)));
  });
}

/** ffmpeg 要能编 GIF、有调色板滤镜;不行就早点说,免得白截一遍。 */
function checkFfmpeg() {
  const probe = (arg) => {
    try {
      return execFileSync(ffmpeg, ['-hide_banner', arg], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
    } catch {
      return '';
    }
  };
  const missing = [];
  if (!/^\s*V\S*\s+gif\s/m.test(probe('-encoders'))) missing.push('gif 编码器');
  const filters = probe('-filters');
  for (const f of ['palettegen', 'paletteuse', 'scale']) if (!new RegExp(`\\s${f}\\s`).test(filters)) missing.push(`${f} 滤镜`);
  if (missing.length) {
    console.error(`ffmpeg(${ffmpeg})不可用:缺 ${missing.join('、')}。用 FFMPEG=路径 换一个,或 FRAMES_ONLY=1 只出帧`);
    process.exit(1);
  }
}

// 页面里跑:聊天区多高,有没有横向溢出或被省略号截掉的文字
const INSPECT = `(() => {
  const fs = document.querySelector('.fs');
  const box = fs.getBoundingClientRect();
  const name = (el) => el.tagName.toLowerCase() + (el.classList[0] ? '.' + el.classList[0] : '');
  const bad = [];
  for (const el of fs.querySelectorAll('*')) {
    const r = el.getBoundingClientRect();
    if (r.width && (r.right > box.right + 0.5 || r.left < box.left - 0.5)) bad.push('溢出 ' + name(el));
    if (getComputedStyle(el).textOverflow === 'ellipsis' && el.scrollWidth > el.clientWidth) bad.push('截断 ' + name(el));
  }
  return { x: box.x, y: box.y, width: box.width, height: box.height, bad };
})()`;

if (!existsSync(chrome)) {
  console.error(`找不到 Chrome:${chrome}`);
  process.exit(1);
}
if (!framesOnly) checkFfmpeg();
const keep = Boolean(process.env.FRAMES);
const frames = keep ? resolve(process.env.FRAMES) : mkdtempSync(join(tmpdir(), 'summon-demo-frames-'));
mkdirSync(frames, { recursive: true });
// 直接起 vite 而不经 npx:kill 掉 npx 时 vite 不跟着退,端口会一直占着
const server = spawn(process.execPath, [join(root, 'node_modules', 'vite', 'bin', 'vite.js'), '--host', '127.0.0.1', '--port', String(port), '--strictPort', '--logLevel', 'warn'], {
  cwd: root,
  stdio: 'ignore',
});
const profile = mkdtempSync(join(tmpdir(), 'summon-demo-chrome-'));
let browser;
try {
  await waitPort(port);
  browser = await startChrome(profile);
  const cdp = new Cdp(browser.ws);
  await cdp.ready;
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId: s } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const evaluate = async (expression) => {
    const { result, exceptionDetails } = await cdp.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true }, s);
    if (exceptionDetails) throw new Error(`页面出错:${exceptionDetails.exception?.description ?? exceptionDetails.text}`);
    return result.value;
  };
  await cdp.send('Page.enable', {}, s);
  // 视口高度给足,截图区域不会落到视口外
  await cdp.send('Emulation.setDeviceMetricsOverride', { width: PAGE_WIDTH, height: 2000, deviceScaleFactor: PIXEL_RATIO, mobile: false }, s);
  await cdp.send(
    'Emulation.setEmulatedMedia',
    { features: [{ name: 'prefers-color-scheme', value: 'light' }, { name: 'prefers-reduced-motion', value: 'reduce' }] },
    s,
  );
  const loaded = cdp.once('Page.loadEventFired', s, 60000);
  await cdp.send('Page.navigate', { url: `http://127.0.0.1:${port}/src/demo/demo.html` }, s);
  await loaded;
  // 开发服务器第一次打开要现转译模块,演示页的脚本跑完才有 window.demo
  for (let i = 0; !(await evaluate('typeof window.demo === "object"')); i++) {
    if (i > 300) throw new Error('演示页没就绪');
    await sleep(100);
  }
  await evaluate('document.fonts.ready.then(() => true)');

  const durations = await evaluate('window.demo.durations');
  const total = durations.reduce((a, b) => a + b, 0);
  const last = durations.at(-1);
  if (total < 10000 || total > 15000 || last < 3000) throw new Error(`时间线不合规格:总长 ${total}ms(要 10–15s),末帧 ${last}ms(要 ≥3s)`);

  // 聊天区自上而下长,末帧最高:按末帧定截图区域,每帧都截同一块
  await evaluate(`window.demo.show(${durations.length - 1})`);
  const box = await evaluate(INSPECT);
  const clip = { x: Math.floor(box.x), y: Math.floor(box.y), width: Math.round(box.width), height: Math.ceil(box.height), scale: 1 };
  const problems = [];
  const list = ['ffconcat version 1.0'];
  for (let i = 0; i < durations.length; i++) {
    await evaluate(`window.demo.show(${i})`);
    const now = await evaluate(INSPECT);
    if (now.height > clip.height + 0.5) problems.push(`第 ${i} 帧比末帧高(${now.height} > ${clip.height})`);
    for (const b of new Set(now.bad)) problems.push(`第 ${i} 帧 ${b}`);
    const shot = await cdp.send('Page.captureScreenshot', { format: 'png', clip }, s);
    const file = `frame-${String(i).padStart(3, '0')}.png`;
    writeFileSync(join(frames, file), Buffer.from(shot.data, 'base64'));
    list.push(`file '${file}'`, `duration ${(durations[i] / 1000).toFixed(2)}`);
  }
  await cdp.send('Target.closeTarget', { targetId });
  cdp.ws.close();
  if (problems.length) throw new Error(`画面有问题:\n  ${problems.join('\n  ')}`);
  // concat 分离器不计最后一条的时长,末帧再列一次它才停够
  list.push(list.at(-2));
  writeFileSync(join(frames, 'frames.ffconcat'), `${list.join('\n')}\n`);
  console.log(`${durations.length} 帧,${(total / 1000).toFixed(1)}s,截图 ${clip.width * PIXEL_RATIO}×${clip.height * PIXEL_RATIO}`);

  const args = (input, out) => ['-y', '-v', 'error', '-f', 'concat', '-i', input, '-vf', FILTER, '-loop', '0', out];
  if (framesOnly) {
    console.log(`帧在 ${frames}`);
    console.log(`在帧目录里合成:\n  ffmpeg ${args('frames.ffconcat', 'demo.gif').map((a) => (/[\s;[\]'$]/.test(a) ? `"${a}"` : a)).join(' ')}`);
  } else {
    mkdirSync(dirname(output), { recursive: true });
    execFileSync(ffmpeg, args(join(frames, 'frames.ffconcat'), output), { stdio: 'inherit' });
    console.log(`${output}(${(statSync(output).size / 1024).toFixed(0)} KB)`);
  }
} finally {
  if (browser) {
    const exited = new Promise((r) => browser.proc.once('exit', r));
    browser.proc.kill();
    await Promise.race([exited, sleep(5000)]);
  }
  server.kill();
  rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  if (!keep && !framesOnly) rmSync(frames, { recursive: true, force: true });
}
