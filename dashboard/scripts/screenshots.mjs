// 用本机 Chrome 无头模式给示例环境的每一页截图:1440×900 与 390×844 × 浅色 / 深色,有预览的页面
// 另截 1024×768 与手机上滚过一屏的样子,再附几组飞书效果预览「拨开关前 / 后」的对照。产物不入库。
//
// 走 DevTools 协议而不是 --screenshot:桌面版 Chrome 的窗口有最小宽度,--window-size=390
// 实际按 500 左右排版,手机宽度会被截成「右边被切掉」的假象;设备尺寸覆盖才是真的 390。
//
// 用法:node scripts/screenshots.mjs [输出目录]
//   ONLY=正则   只截名字匹配的几张
//   FULL=1      截整页(自查用)
//   SCROLL=像素 截图前先滚到这个位置(自查用)
//   SIZE=宽x高  改用这一种尺寸(自查用)
//   LANGUAGE_MENU=1  打开语言菜单并检查顶栏入口与弹层边界
//   REPLAY=1    回放 Rust 生成的契约样例而不是示例环境(自查用)
//   CHROME=路径  改用这个 Chrome / Chromium(默认是 macOS 上的 Google Chrome)
import { execFileSync, spawn } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import net from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
// 默认写到仓库里本任务的记录目录(截图不入库,见 .gitignore 与任务记录约定)
const out = resolve(process.argv[2] ?? join(root, '..', 'tasks', '20261020-dashboard-console', 'screens'));
const chrome = process.env.CHROME ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const port = 5174;
const only = process.env.ONLY ? new RegExp(process.env.ONLY) : null;
const full = process.env.FULL === '1';

const DEV = 'oc_00000000000000000000000000000001';
const PAGES = [
  ['overview', '/'],
  ['global', '/global'],
  ['global-yaml', '/global?mode=yaml'],
  ['agents', '/agents'],
  ['usage', '/usage'],
  // 洞察的时间段记在 localStorage 里,同一个浏览器配置里各张图显式给定,不互相影响
  ['insights', '/insights', { 'summon-dash.insights.range': '30d' }],
  // 7 天的分析在示例环境里已过期、上次重新生成未成功;主 agent 配成 cursor 时不支持分析
  ['insights-stale', '/insights', { 'summon-dash.insights.range': '7d' }],
  ['insights-unsupported', '/insights?set=primary_agent=nimbus', { 'summon-dash.insights.range': '30d' }],
  ['global-insights', '/global/insights'],
  ['agent-overview', '/agents/starlight/overview'],
  ['agent-runtime', '/agents/starlight/runtime'],
  ['agent-permissions', '/agents/starlight/permissions'],
  ['agent-messaging', '/agents/starlight/messaging'],
  ['agent-chats', '/agents/starlight/chats'],
  ['agent-docs', '/agents/starlight/docs'],
  ['agent-broken-yaml', '/agents/saturn/yaml'],
  ['chats', '/chats'],
  ['chat-detail', `/chats/${DEV}`],
  ['chat-crowded', '/chats/oc_00000000000000000000000000000002'],
  ['schedules', '/schedules'],
  ['reopen', '/?unauth=1'],
];
// 手机上往下滚过一屏:预览要吸在顶栏下方
const MOBILE_SCROLLED = [
  ['global-scrolled', '/global'],
  ['usage-scrolled', '/usage'],
  ['chat-detail-scrolled', `/chats/${DEV}`],
];
// 放不下两栏的中间宽度:预览排在页头下面
const TABLET = [
  ['global', '/global'],
  ['agent-messaging', '/agents/starlight/messaging'],
  ['chat-detail', `/chats/${DEV}`],
];
const PAIRS = [
  ['cot-tools', 'messaging.cot_tool_details=true'],
  ['cot-hidden', 'messaging.cot_visible=false'],
  ['thinking-off', 'messaging.hide_thinking=false'],
  ['badge-effort-off', 'messaging.badge_effort=false'],
  ['feed-off', 'messaging.agent_feed=false'],
  ['reactions-off', 'messaging.agent_reactions=false'],
];

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
        if (Date.now() > until) bad(new Error('preview 服务没起来'));
        else setTimeout(tryOnce, 200);
      });
    };
    tryOnce();
  });
}

/** 最小的 DevTools 协议客户端:一条 WebSocket,按 id 配对应答,按 sessionId 分发事件。 */
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
      const w = { method, session, ok };
      this.waiters.push(w);
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

async function shoot(cdp, url, file, w, h, scale, theme, scroll, storage) {
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId: s } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const mobile = w < 600;
  await cdp.send('Page.enable', {}, s);
  await cdp.send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: scale, mobile }, s);
  if (mobile) await cdp.send('Emulation.setTouchEmulationEnabled', { enabled: true, maxTouchPoints: 5 }, s);
  // 深浅色走系统偏好:同时验证「跟随系统」那一套媒体查询。
  // 减少动效:预览打开时那遍回放与高亮不跑,截到的总是终态。
  await cdp.send(
    'Emulation.setEmulatedMedia',
    { features: [{ name: 'prefers-color-scheme', value: theme }, { name: 'prefers-reduced-motion', value: 'reduce' }] },
    s,
  );
  const loaded = cdp.once('Page.loadEventFired', s);
  await cdp.send('Page.navigate', { url }, s);
  await loaded;
  // 页面记在 localStorage 里的选择(如洞察的时间段):写进去再重新加载一次
  if (storage) {
    const reloaded = cdp.once('Page.loadEventFired', s);
    await cdp.send('Runtime.evaluate', { expression: `${JSON.stringify(Object.entries(storage))}.forEach(([k, v]) => localStorage.setItem(k, v)); location.reload()` }, s);
    await reloaded;
  }
  await sleep(900);
  if (process.env.SUMMON_SHOT_LANGUAGE) {
    const { result } = await cdp.send('Runtime.evaluate', { expression: 'document.documentElement.lang', returnByValue: true }, s);
    if (result.value !== process.env.SUMMON_SHOT_LANGUAGE) throw new Error(`Unexpected language: ${result.value}`);
  }
  if (process.env.LANGUAGE_MENU === '1') {
    const { result } = await cdp.send('Runtime.evaluate', { expression: `(() => {
      const summary = document.querySelector('.topbar sm-language-menu summary');
      if (!summary) return false;
      const rect = summary.getBoundingClientRect();
      const visible = rect.width > 0 && rect.top >= 0 && rect.right <= innerWidth;
      summary.click();
      return visible;
    })()`, returnByValue: true }, s);
    if (!result.value) throw new Error('Language entry is not visible in the topbar');
    await sleep(300);
    const { result: panel } = await cdp.send('Runtime.evaluate', { expression: `(() => {
      const el = document.querySelector('sm-language-menu .language-panel');
      const rect = el.getBoundingClientRect();
      return rect.width > 0 && rect.left >= 0 && rect.right <= innerWidth && rect.bottom <= innerHeight
        && !document.querySelector('sm-page-global [data-key="language"]');
    })()`, returnByValue: true }, s);
    if (!panel.value) throw new Error('Language panel overflows or the web form repeats its field');
  }
  const y = process.env.SCROLL ? Number(process.env.SCROLL) : scroll;
  if (y) {
    await cdp.send('Runtime.evaluate', { expression: `window.scrollTo(0, ${y})` }, s);
    await sleep(300);
  }
  if (url.includes('/usage') && y) {
    const { result } = await cdp.send('Runtime.evaluate', { expression: `(() => {
      const picker = document.querySelector('sm-date-range');
      const topbar = document.querySelector('.topbar').getBoundingClientRect();
      const rect = picker.getBoundingClientRect();
      return rect.top >= topbar.bottom && rect.bottom < innerHeight && rect.top - topbar.bottom < 40;
    })()`, returnByValue: true }, s);
    if (!result.value) throw new Error('滚动后日期选择器未保持在顶栏下方');
  }
  const shot = full
    ? await (async () => {
        const { contentSize } = await cdp.send('Page.getLayoutMetrics', {}, s);
        return cdp.send(
          'Page.captureScreenshot',
          { format: 'png', captureBeyondViewport: true, clip: { x: 0, y: 0, width: w, height: Math.ceil(contentSize.height), scale: 1 } },
          s,
        );
      })()
    : await cdp.send('Page.captureScreenshot', { format: 'png' }, s);
  writeFileSync(file, Buffer.from(shot.data, 'base64'));
  // 防回归:整页不许被撑出横向滚动(长路径、长模型名、一群五六个 agent 都在示例数据里)
  const { result } = await cdp.send(
    'Runtime.evaluate',
    {
      expression: `(() => { const W = innerWidth; const sw = document.scrollingElement.scrollWidth; if (sw <= W) return null;
        const over = [...document.querySelectorAll('body *')].filter((e) => { const b = e.getBoundingClientRect(); return b.width > 0 && b.right > W + 1 && !e.closest('.yaml, .tabs, .pv-scroll, .diff, .drawer, .preview-col'); })
          .slice(0, 4).map((e) => e.tagName.toLowerCase() + '.' + String(e.className).trim().split(/\\s+/)[0]);
        return { W, sw, over }; })()`,
      returnByValue: true,
    },
    s,
  );
  await cdp.send('Target.closeTarget', { targetId });
  return result.value;
}

if (!existsSync(chrome)) {
  console.error(`找不到 Chrome:${chrome}`);
  process.exit(1);
}
mkdirSync(out, { recursive: true });
execFileSync('npx', ['vite', 'build', '--mode', 'mock', '--outDir', '.mock-dist', '--logLevel', 'warn'], { cwd: root, stdio: 'inherit' });
const server = spawn('npx', ['vite', 'preview', '--outDir', '.mock-dist', '--host', '127.0.0.1', '--port', String(port), '--strictPort'], {
  cwd: root,
  stdio: 'ignore',
});
const profile = mkdtempSync(join(tmpdir(), 'summon-shots-'));
let browser;
try {
  await waitPort(port);
  browser = await startChrome(profile);
  const cdp = new Cdp(browser.ws);
  await cdp.ready;
  const base = `http://127.0.0.1:${port}`;
  const overflows = [];
  const jobs = [];
  for (const theme of ['light', 'dark']) {
    for (const [name, path, storage] of PAGES) {
      jobs.push([`${name}-desktop-${theme}`, path, theme, 1440, 900, 1, 0, storage]);
      jobs.push([`${name}-mobile-${theme}`, path, theme, 390, 844, 2, 0, storage]);
    }
    for (const [name, path] of MOBILE_SCROLLED) jobs.push([`${name}-mobile-${theme}`, path, theme, 390, 844, 2, 700]);
    for (const [name, path] of TABLET) jobs.push([`${name}-tablet-${theme}`, path, theme, 1024, 768, 1]);
  }
  for (const [name, set] of PAIRS) {
    jobs.push([`preview-${name}-before`, '/global', 'light', 1440, 900, 1]);
    jobs.push([`preview-${name}-after`, `/global?set=${encodeURIComponent(set)}`, 'light', 1440, 900, 1]);
  }
  // SIZE=宽x高:改用这一种尺寸(自查中间断点用)
  const size = process.env.SIZE ? process.env.SIZE.split('x').map(Number) : null;
  for (const [name, path, theme, w0, h0, scale, scroll = 0, storage] of jobs) {
    if (only && !only.test(name)) continue;
    const [w, h] = size ?? [w0, h0];
    const sep = path.includes('?') ? '&' : '?';
    // REPLAY=1:页面吃 Rust 生成的契约样例(自查用)
    const replay = process.env.REPLAY === '1' ? '&replay=1' : '';
    const url = new URL(`${base}${path}${sep}shot=1${replay}`);
    if (process.env.SUMMON_SHOT_LANGUAGE) {
      const set = url.searchParams.get('set');
      url.searchParams.set('set', [set, `language=${process.env.SUMMON_SHOT_LANGUAGE}`].filter(Boolean).join(','));
    }
    const overflow = await shoot(cdp, url.href, resolve(out, `${name}.png`), w, h, scale, theme, scroll, storage);
    if (overflow) {
      overflows.push(`${name}: 页面宽 ${overflow.sw} > ${overflow.W}(${overflow.over.join(', ')})`);
      console.log(`✗ ${name} 横向溢出`);
    } else console.log(`✓ ${name}`);
  }
  cdp.ws.close();
  if (overflows.length) {
    console.error(`有页面被撑出横向滚动:\n  ${overflows.join('\n  ')}`);
    process.exitCode = 1;
  }
} finally {
  if (browser) {
    const exited = new Promise((r) => browser.proc.once('exit', r));
    browser.proc.kill();
    await Promise.race([exited, sleep(5000)]);
  }
  server.kill();
  rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
}
