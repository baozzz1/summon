// 由 Rust ignored 测试启动；NODE_PATH 指向装有 playwright 的测试环境。
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { chromium } = require('playwright');

const deadline = setTimeout(() => { console.error('browser acceptance timed out'); process.exit(1); }, 30000);
(async () => {
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    page.setDefaultTimeout(5000);
    const url = process.argv[2];
    await page.goto(url);
    const iframe = page.locator('main > iframe');
    assert.equal(await iframe.getAttribute('sandbox'), 'allow-scripts');
    const frame = await (await iframe.elementHandle()).contentFrame();
    await frame.waitForFunction(() => document.body.dataset.fetch === 'blocked');
    assert.deepEqual(await frame.evaluate(() => ({ ...document.body.dataset })), {
      ready: 'yes', parent: 'blocked', storage: 'blocked', cookie: 'blocked',
      top: 'blocked', popup: 'blocked', fetch: 'blocked',
    });
    assert.equal(await frame.locator('h1').innerText(), '单文件 HTML 预览');
    assert.equal(await frame.locator('h1').evaluate(el => getComputedStyle(el).color), 'rgb(24, 72, 120)');
    await frame.waitForFunction(() => document.querySelector('#embedded').naturalWidth === 40);
    await frame.locator('#counter').click();
    assert.equal(await frame.locator('#counter').innerText(), '1');
    assert.equal(page.url(), url);
    assert.equal(page.context().pages().length, 1);
    if (process.env.SUMMON_HTML_SCREENSHOT) {
      await page.screenshot({ path: process.env.SUMMON_HTML_SCREENSHOT, fullPage: true });
    }
    const [download] = await Promise.all([
      page.waitForEvent('download'),
      page.getByRole('link', { name: '下载原文件' }).click(),
    ]);
    assert.equal(await download.failure(), null);
    assert.deepEqual(fs.readFileSync(await download.path()), fs.readFileSync(path.join(__dirname, 'fixtures/html-preview.html')));
    // 导航也受父页面 frame-src 约束，不能通过切换文档访问其他预览。
    await frame.evaluate(() => { location.href = '/blocked/self-navigation'; });
    await page.waitForTimeout(250);
    assert.equal(page.url(), url);
    console.log('PASS: rendered HTML, inline CSS/JS/data image, sandbox isolation, raw download');
  } finally {
    await browser.close();
    clearTimeout(deadline);
  }
})().catch(error => { console.error(error); process.exitCode = 1; clearTimeout(deadline); });
