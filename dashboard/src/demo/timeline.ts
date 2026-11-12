// README 首屏动图的剧本:群里 @ 机器人问一句,过程卡里思考与工具步骤逐条出现,最后落一张回复卡。
// 全部是合成内容(人、群、仓库、提交、链接都是编的),消息结构照 summon 真实发出的那几种:
// 工具行的标题是命令正文,下面是命令输出;回复卡是正文 + 分隔线 + 角标。
// 表情按默认配置:收到即贴 OnIt,完成后换成 DONE。
import type { CotItem, PreviewMessage, PreviewView } from '../api/types';

export const BOT_NAME = '小助';
const SENDER = '示例用户';
const TASK = '看下 order-service 最近三次发布的 diff,有没有动到超时配置';

type Step =
  | { kind: 'thinking'; text: string }
  | { kind: 'command'; command: string; output: string };

const STEPS: Step[] = [
  {
    kind: 'thinking',
    text: '先找出最近三次发布对应的 tag,再看这段区间里配置文件有没有改过,重点是超时相关的项。',
  },
  {
    kind: 'command',
    command: "git log --tags --simplify-by-decoration --format='%as %D' -n 4",
    output: '2026-08-25 tag: v2.14.0\n2026-08-18 tag: v2.13.0\n2026-08-11 tag: v2.12.0\n2026-08-04 tag: v2.11.0',
  },
  {
    kind: 'command',
    command: "git log --format='%h %as %s' v2.11.0..v2.14.0 -- config/",
    output: '5d07f3c 2026-08-14 下游调用超时收紧到 800ms (#482)',
  },
  {
    kind: 'thinking',
    text: '三次发布里只有 5d07f3c 改过配置,提交在 8-14,随 8-18 的 v2.13.0 发出。比对这一次的改动。',
  },
  {
    kind: 'command',
    command: 'git diff -U1 v2.12.0 v2.13.0 -- config/prod.yaml',
    output: '@@ -13,2 +13,2 @@ upstream:\n   inventory:\n-    client_timeout: 3s\n+    client_timeout: 800ms',
  },
];

const REPLY =
  '三次发布里只有 **8-18**(v2.13.0)那次动了超时配置:`config/prod.yaml` 里的 `client_timeout` ' +
  '从 `3s` 改成了 `800ms`,对应 [PR #482](https://git.example.com/acme/order-service/pull/482)。\n\n' +
  '8-11 和 8-25 两次发布没有改动配置文件。';

const REPLY_CARD = {
  schema: '2.0',
  config: { summary: { content: '只有 8-18 那次动了超时配置' } },
  body: {
    elements: [
      { tag: 'markdown', element_id: 'reply.0', content: REPLY },
      { tag: 'hr', element_id: 'reply.1' },
      {
        tag: 'markdown',
        element_id: 'reply.2',
        content: "<font color='grey'>claude · Opus 5 · 64k/200k ███░░░░░░░ 32% · 14:32</font>",
        text_size: 'notation',
      },
    ],
  },
};

/** 一帧:这一刻的消息流、过程还在不在跑、停留多久。 */
export interface DemoFrame {
  ms: number;
  view: PreviewView;
  running: boolean;
}

function toItem(step: Step, index: number, text?: string, done = true): CotItem {
  const id = `cot.${index}`;
  if (step.kind === 'thinking') return { id, type: 'thinking', text: text ?? step.text };
  return { id, type: 'tool', icon: 'bash', name: step.command, ...(done ? { result: step.output } : {}) };
}

function view(reaction: string | null, items: CotItem[] | null, reply: boolean): PreviewView {
  const messages: PreviewMessage[] = [
    { id: 'user', kind: 'user', sender: SENDER, text: `@${BOT_NAME} ${TASK}`, reactions: reaction ? [reaction] : [] },
  ];
  if (items) messages.push({ id: 'cot', kind: 'cot', title: TASK, status: 'done', items });
  if (reply) messages.push({ id: 'reply', kind: 'card', card: REPLY_CARD });
  return { title: '', messages, regions: {} };
}

/** 思考按流式增量出现:先露出前面一截,再补全。截断点落在标点后,不劈开词。 */
function partials(text: string): string[] {
  const cuts = [0.4, 0.75].map((r) => {
    const at = Math.floor(text.length * r);
    const punct = text.slice(0, at).search(/[,。、:][^,。、:]*$/);
    return text.slice(0, punct > 0 ? punct + 1 : at);
  });
  return [...new Set(cuts)].filter((s) => s && s !== text);
}

function build(): DemoFrame[] {
  const frames: DemoFrame[] = [{ ms: 1200, view: view(null, null, false), running: false }];
  const done: CotItem[] = [];
  const running = (ms: number, extra: CotItem[] = []) =>
    frames.push({ ms, view: view('OnIt', [...done, ...extra], false), running: true });
  running(600);
  STEPS.forEach((step, i) => {
    if (step.kind === 'thinking') {
      for (const part of partials(step.text)) running(200, [toItem(step, i, part)]);
      running(600, [toItem(step, i)]);
    } else {
      running(400, [toItem(step, i, undefined, false)]);
      running(800, [toItem(step, i)]);
    }
    done.push(toItem(step, i));
  });
  frames.push({ ms: 4500, view: view('DONE', done, true), running: false });
  return frames;
}

export const FRAMES: DemoFrame[] = build();
