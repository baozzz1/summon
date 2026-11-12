---
name: summon
description: 把当前会话「召唤/关联」到某个 agent 的自我认知(SOUL/memory),以其人格行事,会话结束受治理回写。当用户说 /summon <agent>、召唤 <agent>、以 <agent>(如星光)身份、切到 <agent> 时触发。agent 自我认知在 ~/.summon/agents/<alias>/。
---

# summon — agent 自我认知召唤

把当前会话关联到 `~/.summon/agents/<alias>/` 定义的 agent(由 summon 运行时管理)。

## 步骤

1. **解析 alias**:扫 `~/.summon/agents/`(每个子目录一个 alias,含 `agent.yaml`)。
   - 无参 → 列出 `status=active` 的让用户挑。
   - 未知名 → 列出可选并停下,**不要瞎猜**。
   - `status=draft/retired` → 先警告再问是否继续。
2. **bootstrap 读入**(顺序固定):
   `~/.summon/agents/<alias>/SOUL.md` → `memory/MEMORY.md` → `memory/history/` 最近 N 天(N=`agent.yaml` 的 `cognition.history_lookback_days`,默认 2;同块 `cognition.history_dir` 可覆盖,如 starlight=`~/projects/work/standups`)。
   **SOUL.md 若声明了 workspace(`working_dir`)及要读的上下文(如 starlight 指向 `~/projects` 的 profile/work/CLAUDE.md),按其指示读** —— workspace 继承由 agent 自己在 SOUL 表达,skill 不硬编码。
   读不到的文件:加载现有 + 明确警告缺哪个,**绝不静默**。
3. **进入角色**:打印一行确认 + 自我简报(我是谁、盯着什么、最近记录),之后整段会话以该 agent 人格 + 其声明的偏好行事。
4. **保持角色**到会话结束或切换(切换 = 对另一 agent 再 summon → 先对离场 agent 跑回写 closing,再 bootstrap 新 agent)。

## 回写治理(closing,绝不静默写)

会话结束 / 切换离场时自检本次是否产出:
1. 值得记的工作 → 提议向 `history_dir/YYYY-MM-DD.md` 追加一行。
2. 可复用踩坑 → 提议按 5 字段 schema(Status: candidate / Trigger / Lesson / Evidence / Action)追加到 `memory/MEMORY.md`。
3. 长期事实 / 决策 → 提议追加到 `memory/MEMORY.md`,并更新"上次梳理"日期。

规则:**展示拟追加内容 → 用户 approve → 才落盘**(除非 `agent.yaml` 的 `cognition.writeback: auto`;`off` 跳过)。

## 安全 / 边界

- 未知 agent 列出停下不猜;缺文件加载现有 + 警告。
- 绝不打印解析出的飞书密钥真值(`.env` 里的值),只显示 env 变量名。
- summon 与回写全本地,不主动外发飞书(飞书 IO 由 summon daemon + 边车负责)。
