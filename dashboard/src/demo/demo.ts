// README 首屏动图的演示页,只给 scripts/demo-gif.mjs 在开发服务器上截图用;不在任何构建入口里,不进 dist。
// 画面就是控制台的飞书效果预览(sm-preview),逐帧换上 timeline.ts 里的那一份消息流。
import '../styles/tokens.css';
import '../styles/base.css';
import '../styles/components.css';
import '../styles/preview.css';
import './demo.css';
import type { CotLive } from '../preview/cot';
import { PreviewController } from '../preview/controller';
import '../preview/panel';
import type { PreviewPanel } from '../preview/panel';
import { BOT_NAME, FRAMES } from './timeline';

// 面板自己的回放靠定时器推进,截图要逐帧确定的画面:关掉首次露出时的自动回放,
// 「进行中」由这里直接给出已出现的条目数。
interface PanelInternals {
  live: CotLive | null;
  played: boolean;
}

// 演示页不连服务端:scope 为空时控制器从不取数,画面只由下面直接放进 view 的消息决定。
const ctl = new PreviewController(() => Promise.reject(new Error('演示页不取数')), null, () => []);
const panel = document.createElement('sm-preview') as PreviewPanel;
const internals = panel as unknown as PanelInternals;
panel.ctl = ctl;
panel.botName = BOT_NAME;
internals.played = true;
document.getElementById('stage')?.append(panel);

async function show(index: number): Promise<void> {
  const frame = FRAMES[index];
  if (!frame) throw new Error(`没有第 ${index} 帧`);
  ctl.view = frame.view;
  const cot = frame.view.messages.find((m) => m.kind === 'cot');
  internals.live = frame.running && cot?.kind === 'cot' ? { shown: cot.items.length } : null;
  panel.requestUpdate();
  await panel.updateComplete;
  // 等排版与绘制落定再截
  await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
}

declare global {
  interface Window {
    demo: { durations: number[]; show: (index: number) => Promise<void> };
  }
}

window.demo = { durations: FRAMES.map((f) => f.ms), show };
