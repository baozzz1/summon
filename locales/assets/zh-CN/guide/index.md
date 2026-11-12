# summon 使用指南(agent 视角)

引用消息的正文需要按 ID 读取时，查 `summon guide quotes`。

> 本指南随 summon 二进制分发,与你机器上装的版本一致。你是用户的 agent(Claude Code /
> Codex / Cursor 等),summon 把你接到了飞书:用户在飞书里给机器人发的消息会变成你的任务,
> 你的回复会被直接发回飞书。

## 心智模型(一段话)

summon 是本地常驻服务(daemon + sidecar),它做三件事:把飞书消息投递给你、把你的回复
发回飞书、管理你的会话生命周期(tmux 持久会话 / 无头 RPC / 空闲回收)。你不需要主动调用
summon 来「收发消息」——那是自动的。你需要 summon 的时刻是:**用户问你 summon 能做什么、
想改配置、或出了问题**。

## 分节

- `summon guide protocol` —— 你自己要执行的操作规程:建定时器、读会话前文、收附件、把任务委派到另一个群
- `summon guide commands` —— 飞书里可用的全部命令(斜杠命令、按钮、打断)
- `summon guide handoff` —— `/handoff` 交接:写交接文件的模板与标签、交给别的 agent 或新会话
- `summon guide mentionless` —— 一人一 Bot 群的免 @ 开启检查与权限申请入口
- `summon guide config` —— 配置字段与分级开关,含「什么时候该建议用户配什么」
- `summon guide manage` —— 终端管理 CLI:新建 agent(onboard / agents add / bind)、daemon 运维
- `summon guide troubleshooting` —— 常见症状 → 查什么 → 怎么修

## 最常用的四条

1. 用户想改 summon 行为(空闲提醒、模型、插话……)→ 先查 `summon guide config`,
   改动落在 `~/.summon/config.yaml`(全局)或 `~/.summon/agents/<alias>/agent.yaml`(单 agent),
   生效时机按字段核对：下一轮、会话重连或 daemon 重启。
2. 飞书里出问题(不回消息、卡片卡住、权限报错)→ 先查 `summon guide troubleshooting`,
   再跑 `summon doctor` 做体检。
3. 用户想知道你能干什么 → 查 `summon guide commands`。
4. 你自己要建定时器、读这个会话的历史前文、把任务交到另一个群去做 → 动手前查 `summon guide protocol`。

## 边界(诚实声明)

- 你看不到的:飞书 app 的创建与发布(开放平台手工步骤,summon 无法代办)。`summon chats`
  只给本 agent 的会话,跨 agent 不可读;`summon session export` 不设这道闸。
- 改不了的:`system_prompt_core`(消息协议,内置不可覆盖)、飞书平台限制(消息撤回、
  已发卡片编辑等以平台能力为准)。
