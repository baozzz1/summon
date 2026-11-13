# 配置面

配置按作用范围存放；生效时机取决于字段：

- 全局:`~/.summon/config.yaml` —— 影响所有 agent(只写要覆盖的字段,其余继承内置默认)
- 单 agent:`~/.summon/agents/<alias>/agent.yaml` —— 只影响这一个 agent

用户描述困扰时，先核对当前值和覆盖来源，再给出字段和值；获准修改后，按下面的生效时机验证。

## 显示语言

全局顶层 `language` 可选 `auto`（默认）、`zh-CN`、`en`，也可通过控制台右上角的“语言”菜单切换。设置影响本机 Dashboard、CLI、飞书内置文案和菜单栏，无需重启 daemon。
`auto` 优先读取 `lark-cli config show` 输出的 `.lang`，未设置、读取失败或不可用时取系统第一首选语言。
中文使用简体中文，其它首选语言使用英语；显式配置优先。用户消息、名字、自定义提示词和外部工具原文不翻译。

## 主 agent

全局顶层 `primary_agent` 指定控制台洞察页「分析」由哪个 agent 来跑(用它自己的 CLI、模型与账号)。
留空(默认)即自动：近 30 天 token 用量最多的 agent;还没有用量时取最早配置的(按 agent 目录的创建
时刻)。指定的 agent 不存在、已停用或不可路由时同样回落到自动。目前只有洞察分析使用它；主 agent 用
cursor 时不支持分析(一次性运行无法可靠关闭 hooks)。修改后立即生效。

## 覆盖层级与生效时机

`messaging` 的覆盖顺序为内置默认 → 全局 → agent → 本会话。只有下表列出的分级开关支持本会话覆盖，其他字段不能直接写成 `/set` key。

| 设置 | 生效时机 |
| --- | --- |
| 消息展示、分级开关 | 下一轮读取快照；当前轮保持开跑时的设置 |
| 模型、工作目录、环境、CLI 路径与权限策略 | 下次会话启动或重连；使用对应命令时以命令回执为准 |
| `agent.reactions`、`agent.feed`、开场约定 | 下次注入 PRIMER；Claude/Codex 启动或 resume，Cursor 需要新会话 |
| `codex_platform_tools` | 新建原生会话，`/quit` 后续聊不足以更新工具表 |
| `preview_server`、`web_terminal` 的监听配置 | daemon 重启 |
| `sentinel` | 下一次轮询读取配置 |

`/set` 查看有效值及来源；`/set <key> on|off|inherit` 设置本会话或恢复继承。群内先 @机器人。

| `/set` key | `messaging` YAML 字段 | 内置默认 |
| --- | --- | --- |
| `cot.visible` | `cot_visible` | `true` |
| `cot.thinking` | `hide_thinking`（语义相反） | 显示；`hide_thinking: false` |
| `cot.tools` | `cot_tool_details` | `false` |
| `timer.idle_retire_reset` | `timer_idle_retire_reset` | `false` |
| `badge.cli` | `badge_cli` | `true` |
| `badge.model` | `badge_model` | `true` |
| `badge.effort` | `badge_effort` | `true` |
| `badge.ctx` | `badge_ctx` | `true` |
| `agent.reactions` | `agent_reactions` | `true` |
| `agent.feed` | `agent_feed` | `true` |

例如，只在本会话隐藏工具详情用 `/set cot.tools off`；整个 agent 隐藏则在其 `agent.yaml` 的 `messaging:` 下写 `cot_tool_details: false`。

文档评论开关在设置列表中标为 `document_comments.enabled`，支持全局和单 agent 配置：在 `messaging:` 下写 `document_comments_enabled: true|false`，默认 `false`，读取配置后即时生效。它不支持本会话覆盖。关闭时收到的评论通知直接确认，不建任务。

非 owner 评论开关在设置列表中标为 `document_comments.non_owner`，YAML 字段为 `messaging.document_comments_non_owner_enabled`，默认 `false`。支持全局和单 bot 覆盖，控制台“文档评论”页可选择 bot 开启或恢复继承，不支持本会话覆盖。还须开启文档评论总开关；owner 评论遵循原有规则。开启后仅处理 bot 所属或由 bot 创建后转给 owner 的文档中的非 owner 评论，仍由 owner 审批、bot 发布；不会使用 owner 的用户权限读取其他文档。关闭时新评论直接结束处理，已有任务在读取、起草和审批发布时重新检查开关。

额外开关在设置列表中标为 `document_comments.owner_mentions`，YAML 字段 `messaging.document_comments_owner_mentions_enabled` 默认 `false`，支持全局和单 agent 覆盖，不支持本会话覆盖。控制台“文档评论”页显示为“允许 owner 在其他文档中 @机器人”。它受文档评论总开关控制；两项都开启后，owner 本人发出并明确 @当前 bot 的评论可以使用 owner 的文档权限读取上下文。

原有 bot 归属路径优先。需要用户权限时，使用与该 bot 同一应用的 lark-cli profile；该 profile 必须已登录配置 owner，并具有文档及评论读取授权。daemon 在每次读取前后核验应用和用户身份，只读取当前文档与评论串，按需正文仍只支持 docx。缺少登录、授权或权限时记录失败，不自动登录或给 bot 添加权限。

回复仍须 owner 在草稿卡上批准，随后由 bot 身份发布；bot 缺少评论权限时发布失败，不改用用户身份。关闭额外开关会取消使用用户权限的起草并阻止其发布；普通 bot 归属任务仍受总开关控制。已经取得上下文的任务保留冻结的读取通道，不因打开开关而改用用户权限。


起草超时使用 `messaging.document_comment_timeout_secs`，默认 `300` 秒，须为正整数。
本机 `config.yaml` 设置全局默认，单 agent 的 `agent.yaml` 可在 `messaging:` 下覆盖同名字段；下一次起草生效。
控制台“文档评论”页面可选择全局或单 agent，配置开关、起草超时及恢复继承。

起草初始材料包含文档链接、评论引用和评论串；需要原文时，通过绑定当前文档的只读工具分段读取 docx 正文，每次最多 2000 字符。

评论任务可在 Menu Bar 的 Document comments 区域、控制台“文档评论”独立页面查看并停止本次起草；Menu Bar 只列未完成的任务。
CLI 使用 `summon comments list` 查看完整任务 ID，再运行 `summon comments stop <id> --agent <别名>`。
停止会结束本次起草并回收进程，任务状态持久保存。完整草稿通过飞书审批卡由 owner 批准发布。

开启后，每条评论使用一次临时起草进程；完整草稿生成后发 owner 审批卡，批准后才发布回复。起草总时限默认 5 分钟，失败、超时、取消或重启中断后任务终止，不自动重试；失败、超时与重启中断会尽力给 owner 发一次失败通知卡，已核验访问授权的带文档标题、评论与原文档入口；手动停止、关闭开关取消的、还没确定 owner 的不发，不满足 bot 归属条件且没有通过 owner 提及授权的文档只记入任务状态。关闭开关会取消正在进行的起草，并阻止发布；尚未启动的任务和冻结草稿保留，关闭期间收到的新评论直接结束处理。

## 空闲与回收(最常被问)

- `messaging.idle_ttl_secs`(默认 86400 = 1 天)—— 会话空闲多久发「空闲提醒卡」。
  **用户被提醒卡打扰时建议调大**;走 API 计费的 agent 可直接配到一年(31536000)。
  注意:**写 0 不是关,是立刻发卡**(判定是 `空闲时长 > ttl`)。
- `messaging.idle_retire_secs`(默认关)—— 空闲多久自动退出会话(退出续聊,留 resume 记录,
  下次消息接回上下文)。**用户想省内存/嫌挂着的会话多时建议开**(如 86400 = 1 天)。
- `messaging.timer_idle_retire_secs`(默认 3300 = 55 分钟,**写 0 关**)—— 定时任务尾轮的
  静默退出:定时任务跑完后这么久没新消息,不发提醒卡直接退出。**用户觉得定时任务会话
  退场太吵/太安静时调它**。

- `messaging.timer_idle_retire_reset`（默认 `false`）—— 定时尾轮退出后是否关闭自动续聊入口；`true` 时下次触发新建会话，`/resume` 可恢复最近结束的会话。
- `messaging.idle_scan_secs`（默认 300 秒）—— 空闲扫描间隔。已退出或归档的会话不发送空闲提醒。

## 轮内行为

- `messaging.headless_stall_secs`（默认1800，即30分钟，正整数秒）—— Codex/Cursor无头轮距上次产出的真静默上限。全局或agent配置，下一轮生效；例如 `1800` 为30分钟。工具、子agent或新鲜引擎心跳存在时，上限至少30分钟，且不短于配置值。引擎心跳新鲜度独立为240秒，等待真人时暂停裁决。

- `messaging.steer_transcript`(默认开)/ `messaging.steer_rpc`(默认开)—— 你正在跑一轮时
  用户又发消息,是直接注入你的队列(true)还是排队等本轮结束(false)。开着时连发的消息
  可能被合成一轮回答,过程展示上会有一个「收到追加消息」的分隔;两条路机制不同,
  出问题可以只关一条。`messaging.steer_cli_session`（默认开）单独控制 Cursor CLI 的打断续跑式追加消息；Cursor ACP 不支持这条机制。
  Claude 主 Agent 已完成当前输出、仅等待后台任务时，排队的新消息会按独立轮次处理，使用新发起人的权限和凭证；后台任务继续运行，它的子 agent 仍按原发起人的权限执行，跑完后主 Agent 的后续输出并进当时正在进行的那一轮回复，没有轮在进行时由会话镜像转发。`messaging.session_mirror` 关闭时不提前处理，排队消息等后台任务完成。主 Agent 正在执行时，只有同一发起人的消息能追加进当前轮，其他消息排队。
- `messaging.hide_thinking`(默认关)—— 隐藏思考过程。**经网关接入、thinking 明文可见的模型建议开**,
  等同 claude 自家模型的加密思考体验。
- `messaging.cot_visible`(默认开)—— 是否外显 CoT 过程内容(思考、工具、过程中说的话)。关时
  只保留运行状态、呼吸效果与暂停按钮,只看终态回复。**用户觉得过程太吵时建议关**。
- `messaging.cot_tool_details`(默认关)—— CoT 是否展示工具调用(命令、参数、结果)。关时 CoT
  只有思考正文,工具的一句话说明照常作为思考正文显示。**用户想看工具在跑什么时建议开**。
  `hide_thinking` 开且 `cot_tool_details` 关时,CoT 里没有思考和工具内容,只剩运行状态、
  过程中说的话与分段/中断提示(「收到追加消息」「曾中断」等),CoT 照常开。
- `messaging.tool_trail`(默认关)—— 强制显示工具轨迹(三行状态栏),即使思考可见也渲染。
  **用户想同时看到思考和执行动作时建议开**。
- `messaging.background_continuation_max_secs`(默认 3600,**写 0 关**)—— 你起了后台任务
  (后台命令、后台子 agent)时,轮会等任务通知多久再强制收尾。**用户的后台任务经常超过 1 小时
  被截断时调大**。

## 模型

- 面板候选来自各 backend 自报的目录(codex/traex 的 `model/list`、cursor 的 `availableModels`),
  上游加了新模型不用改配置就能点到。飞书里 `/model <id>` 只认候选目录里的 id(claude 另认它内置的
  别名与当前配置的模型),对不上就不改、回几个最接近的候选按钮;目录之外的 id 用本机
  `summon agents model <agent> <id>` 设。
- `messaging.models_by_cli` —— 冷启兜底的候选,按 CLI 分桶。目录还没抓到时顶上;
  claude 没有列模型的接口,这张表是它候选的唯一来源。
- `messaging.default_model_by_cli` —— 钉选某个 CLI 的默认模型(onboard/bind 的 `--model` 缺省值)。
  **不想让主力随上游换代而变时钉这里**;不钉就跟随 backend 自报的默认。
- agent.yaml **顶层** `model` / `effort` —— 这个 bot 的**默认**模型与思考强度;某个群用别的,在那个群发
  `/model <id>` / `/effort <档位>`(落在 `state/threads/<tag>/model`、`effort`),`inherit` 回到默认。
  Cursor 参数化 ACP 的默认是 `state/cursor-parameters.json`,本群覆盖在 `state/threads/<tag>/` 下的同名
  文件,只记本群改过的轴。claude 会话里注入 `/model`、`/effort` 会顺带改写本机用户设置
  (`~/.claude/settings.json` 的 `model` 与 `modelSettings.<模型>.effortLevel`),影响之后不带参数新起的
  claude;summon 起的会话任一层有值就显式带参数,不受它影响。
  `reasoning_summary` 控制 Codex 思考摘要（`none` / `concise` / `auto` / `detailed`）。
- `messaging.efforts` —— 面板的档位候选，最终与 backend 支持范围求交；`models` 是扁平兼容候选，优先使用 `models_by_cli`。

## 工作目录

- agent.yaml 的 `runtime.working_dir` —— 该 agent 的默认干活目录
- agent.yaml 的 `runtime.chat_working_dirs` —— 按群绑定目录(进对应群自动切)
- **用户说「在某个群里你老在错的目录干活」时建议配 chat_working_dirs**
- `/pwd` 是只读对账入口:session 未运行时显示按 `.cwd` → `chat_working_dirs` →
  `working_dir` 解析出的下次启动目录；运行时再读取 Claude tmux pane 或 RPC 会话池钉死的 cwd。
  两者不一致时不会伪装一致，也不会为查询而启动/重启 session；下一次冷启或 RPC 重连会按配置收敛。
  群聊查询须有 operate 权限，完整路径只私信操作者；群内只确认私信结果，不回显宿主机路径。

## 按群配置

同一个 agent 在不同群里要不同的权限或约束时用这几个,只作用于对应的那个会话(键都是 chat_id,
在那个群里跑 `summon chat-id --turn-id <本轮凭证>` 拿到):

- agent.yaml 的 `runtime.chat_working_dirs` —— 按群绑定工作目录(见上一节)
- agent.yaml 的 `feishu.chat_reply_without_mention`：以精确 chat ID 为键，值为 `true` 时，允许该群一名真人与当前 Bot 的普通对话、附件和斜杠命令免 @。未配置或为 `false` 时关闭，下一条入站热重载；对话名单与命令的操作权限继续生效。示例：

  ```yaml
  feishu:
    chat_reply_without_mention:
      oc_example: true
  ```

  快速入口：在目标群发送 `@当前Bot /mentionless on`，缺权限时直接提供申请链接；关闭用 `/mentionless off`。完整步骤见 `summon guide mentionless`。

  应用需有 `im:message.group_msg`、群信息读取权限，以及 `im:chat.members:read` 或兼容的成员读取权限。每条消息实时核对群计数和唯一发信人，查询失败或验证期间配置变化时静默拒收。详情见[设计说明](../../../../docs/spec/2026-10-20-chat-mentionless-design.md)。

- agent.yaml 的 `feishu.chat_operators` —— 本群 operate 名单,与 `operators` 取并集,只在那个群里
  算数。改模型 / 思考档位、Cursor 共享参数与 Fast、Bridge 与召回这类效果跨群的操作不认它。
  **用户想「只给某个群里的人开操作权限」时建议配这个**,别加进 `operators`(那是整个 agent 的)。
  它只管能不能操作,不管能不能跟你说话(那是 `allowed_users`)。**它不是安全边界**:本群 operator
  在那个群里能用 `/stop`、斜杠透传、浏览器终端,也能让你写文件 —— 够得着本机上的一切,包括把自己
  写进 `operators`。只建议给信得过这台机器的人
- `~/.summon/agents/<alias>/chats/<chat_id>/CLAUDE.md` —— 本会话常驻要求,追加在全局与本 agent 的
  常驻要求之后,只注入这一个会话,下次冷启生效。**某条约束只该在一个群里生效时写这里**,别写进
  本 agent 的 CLAUDE.md 或 SOUL.md(那样每个群都会带上)

## 高级

- `messaging.system_prompt` —— 自定义开场白(可整个改写;但消息协议在不可覆盖的
  `system_prompt_core` 里,改不掉)
- `messaging.slash_blacklist` —— 斜杠命令黑名单(分 backend),**用户想禁掉某些透传命令时用**
- agent.yaml 的 `runtime.env` —— 注入你进程的环境变量(API endpoint、代理等);
  值原样交给子进程；凭据按 CLI 自身认证方式配置，不把密钥写进示例或回复
- `claude.status_line_in_managed_sessions` —— summon 起的 claude 会话里是否照常执行你自己那条
  statusLine 命令(**默认关**)。关着是因为同样的信息已经在飞书角标上,pane 里再画一遍占地方;
  但如果你的 statusLine 同时在往统计平台透传数据,不跑就断了 —— 那就把它打开。
  额度与上下文的采集与这个开关无关,两边都照常
- `cursor.quota` —— Cursor 个人额度(角标「quota N%」)。Cursor 不推额度,summon 只在打开了
  「额度显示」的会话里、每轮开跑时去 cursor.com 查;同一份凭据最多每 `interval_secs`
  (默认 600)查一次,用它的几个 agent 共享结果。凭据默认读 cursor-agent 自己的登录态(macOS
  钥匙串,Linux `~/.config/cursor/auth.json`);放在别处时写 `token_command`,它的 stdout 输出
  access token,运行时只有 PATH、HOME 这类基础环境。配了 `runtime.cli_cmd` / `runtime.env` 的
  agent 要在 agent.yaml 写 `runtime.cursor_quota_token_command`(在该 agent 的环境里跑),否则
  不查(证明不了是同一个账号)。凭据失效或接口不认时角标不再显示额度。`enabled: false` 整机关掉

## 其余消息配置

以下字段同样放在全局或 agent 的 `messaging:` 下。

| 字段 | 默认 / 用途 |
| --- | --- |
| `completion_summary_card` | `true`；完成时发送独立总结卡 |
| `session_mirror` | `true`；Claude Transcript 会话镜像，把终端里产生的回复投回飞书 |
| `mask_email` | `true`；消息被飞书审核以含邮箱整条拒发时，邮箱只留用户名首字母（`p***@example.com`）后重发一次，没被拒的原样送达；关掉则被拒时改发一句说明；改了即生效 |
| `progress_max_steps` | `3`；工具轨迹最多保留几步，0 按 1 处理 |
| `badge_cli` | `true`；回复角标显示 CLI 类型 |
| `badge_model` | `true`；回复角标显示生效模型 |
| `badge_effort` | `true`；回复角标显示思考档位 |
| `badge_ctx` | `true`；回复角标显示上下文用量，只管显示，上下文过重提醒照常 |
| `agent_reactions` | `true`；向 agent 注入语义表情约定，系统工程表情独立运行 |
| `agent_feed` | `true`；向 agent 注入回复末尾 feed 总结约定，关掉后会话列表预览取正文开头 |
| `document_comments_enabled` | `false`；文档评论起草与审批发布，支持全局和单 agent 配置 |
| `document_comments_non_owner_enabled` | `false`；响应非 owner 文档评论，支持全局和单 bot 配置 |
| `document_comments_owner_mentions_enabled` | `false`；owner 明确 @当前 bot 时允许使用用户文档权限，支持全局和单 agent 配置 |
| `document_comment_timeout_secs` | `300`；单次评论起草总时限，正整数秒，支持全局和单 agent 配置 |
| `codex_platform_tools` | `false`；Codex/Traex 原生注册 preview、react、send_image，需新建会话 |
| `reactions` / `reaction_meanings` | 表情安全集及含义，按安装版本内置表；查 `summon guide reactions` |
| `default_reaction` / `working_reaction` / `done_reaction` / `interrupted_reaction` | `OK` / `OnIt` / `DONE` / `SILENT` |
| `prompt_template` | `{source}:「{text}」`；每轮消息模板，可用 `{source}`、`{who}`、`{text}`、`{chat}` |
| `session_exited` | 按退出原因覆盖说明文案，字段见下文 |

`session_exited` 子键为 `manual_kill`、`manual_kill_reset`、`panel_clean`、`panel_reset`、
`idle_retired`、`agent_archived`、`group_disbanded`、`bot_removed`、`shutdown_interrupt`。
只写要覆盖的子键；`agent_archived` 对应 agent 自行退出的说明，下一条普通消息续聊。

`system_prompt_core`、`system_prompt_reactions` 属于内置协议，不可覆盖。
`interrupt_enabled`、`interrupt_transcript`、`interrupt_prefix` 为无效旧字段；用户打断用 `/stop` 或暂停按钮。

## Agent 身份与运行环境

`agent.yaml` 的顶层字段：`name`（稳定 alias）、`display`（显示名）、`status`（默认 `active`）、
`role`（`agent` 或 `steward`）、`model`、`effort`、`reasoning_summary`、`aliases`（用户 ID 到昵称）、
`feishu`、`runtime`、`messaging`。接入步骤查 `summon guide manage`。

`feishu` 配置：`app_id_env`、`app_secret_env` 为凭据环境变量名；`scope` 是应用存储身份；
`allowed_users` 控制谁能发起对话（空为允许），`operators` 控制操作权限（空时回退 talk 名单），
`chat_operators` 追加本群权限。群里两级操作名单都为空时拒绝操作，只有本群范围的操作另行放行飞书应用所有者（daemon 查到所有者之后）；任一名单非空时所有者不另行放行。

| `runtime` 字段 | 用途 / 默认 |
| --- | --- |
| `cli` / `cli_cmd` | backend 种类 / 自定义可执行命令；优先用 onboard 配置 |
| `capture` | 仅 Cursor 可用 `cli`（或 `exec`）选择 CLI Session，不写时走 ACP；onboard / bind / `agents add` 新建的 Cursor agent 会写上 `cli`。两种通道的 SID 不互通 |
| `working_dir` / `chat_working_dirs` | 默认目录 / 按 chat 绑定目录；`/cd` 保存的目录优先 |
| `resume` | 继承全局 `resume_sessions`；显式布尔值覆盖 |
| `model_context_window` | Codex/Traex 上下文窗口 token 数；不配时跟随 CLI |
| `codex_sandbox` / `codex_approval_policy` | 覆盖全局 Codex 权限策略，见下文 |
| `codex_skip_git_repo_check` | 默认 `false`；Codex exec 的非 Git 目录检查例外 |
| `env` | 子进程环境变量键值 |
| `cursor_quota_token_command` | 此 agent 的 Cursor 额度凭据命令 |
| `execution.target` / `execution.working_dir` | 远程工具执行目标 / 目标机器上的绝对工作目录 |
| `tmux_name` / `seed` | 自定义 tmux 会话名 / CLI 初始输入，通常留空 |
| `rpc_model` | Cursor ACP 参数迁移字段，使用会话模型控制入口维护当前参数 |
| `headless` | 兼容字段；Claude 固定 Transcript，Codex/Traex 固定 RPC，不用它选择捕获通道 |

全局 `codex.sandbox` 默认 `danger-full-access`，可选 `read-only`、`workspace-write`；
`codex.approval_policy` 默认 `never`，可选 `untrusted`、`on-request`。交互策略下 provider 发出的
审批请求经飞书操作权限验证后交给用户处理；改配置后在下一次会话启动或 resume 时下发。

## 整机服务与连接

这些放在全局 `~/.summon/config.yaml`，不写进 `messaging`。

| 字段 | 默认 / 用途 |
| --- | --- |
| `agents_dir` | `~/.summon/agents`；agent 定义目录，daemon 启动读取 |
| `resume_sessions` | `true`；允许原生会话续聊，可被 `runtime.resume` 覆盖 |
| `poll_ms` / `debounce_ms` | `400` / `1200` 毫秒；轮询与消息合并节拍 |
| `default_domain` | `feishu.cn`；飞书 API 域 |
| `sidecar_bin` | sidecar 可执行文件路径 |
| `tools` | 外部命令绝对路径（支持 `~/`），留空用 daemon 的 PATH |
| `menubar.terminal` | `summon agents open` 在终端里打开会话时用的命令参数列表；默认自动选择 |
| `menubar.show_counts` | `true`；Menu Bar 状态栏在图标旁显示 working / waiting 计数，关掉只剩图标 |
| `remotes` | 跨机 fleet 列表，每项 `name`、`ssh_host`、`summon_bin`；默认空 |
| `execution_targets` | 远程工具目标表，每项 `provider: claude-code`；默认空 |

`tools` 可配 `tmux`、`lark_cli`、`claude`、`codex`、`traex`、`cursor_agent`、`git`、`ssh`、`npx`、
`pkill`、`open`、`xdg_open`、`launchctl`、`systemctl`、`loginctl`、`pbcopy`、`osascript`。
`tmux_bin` 与 `sentinel.lark_cli_path` 是兼容字段，优先使用 `tools`。

文件预览 `preview_server`：`enabled: true`、`bind: 127.0.0.1`、`port: 7765`、
`public_base_url: ""`、`token_ttl_secs: 3600`(预览链接有效期，秒)、`max_file_mb: 10`。
远端机器的 `public_base_url` 填用户浏览器可达的地址；留空时探测本机可路由地址，探测失败不生成链接。
浏览器终端 `web_terminal`：`enabled: true`、`port: 7766`，监听地址沿用 `preview_server.bind`。
这些服务在 daemon 启动时读取配置。

`sentinel` 用于显式召唤 agent，默认 `enabled: false`。要使用时先看 `summon sentinel --help`。
其余默认：`dry_run: false`、`interval_secs: 60`、`fast_interval_secs: 5`、`fast_top_k: 20`、
`slow_batch_size: 2`、`top_k: 50`、`max_age_secs: 300`、`per_hour: 6`；
触发词 `join_verbs: [进来回答]`、`create_verbs: [建群]`、`invite_verbs: [/invite]`。
`blocklist` 排除 chat；`aliases` 设称呼；`owner_open_id` 绑定发起者，`lark_profile` 选择身份配置。
这份指南只说明配置；身份能力仍受部署环境和用户授权约束。

自动升级 `upgrade`:`auto: false`(默认关)、`check_interval_hours: 6`、`min_age_hours: 2`(新版发布满这么久
才采用)、`idle_minutes: 10`(整机连续空闲这么久才换代)、`range: minor`(`patch` 只跟同一 x.y 的补丁版本;
主版本变化一律不自动跟)。`channel` 选新版从哪个渠道取:`github` 是 GitHub Releases,不写用本构建的默认
渠道。`from` 是演练用的 Release 来源,只收绝对路径的本地目录或本机回环地址上的明文 `http://`,其下直接有
`manifest.json` 与各平台的包。换代后观察 10 分钟,新版起不来或很快崩了就放回换代前的落点、不再自动采用
它;过程写 `~/.summon/run/upgrade.log`,状态看 `summon doctor`。

## 改配置的正确姿势

1. 先 `summon guide config` 确认字段(你正在读的这份);
2. 改 `~/.summon/config.yaml`(全局)或对应 agent.yaml;
3. 按“生效时机”表等待下一轮、重连或重启，并实际验证；
4. 用 `summon doctor` 检查配置；修改前保留原值，发现异常时恢复该字段。若 `~/.summon` 已纳入 Git，可用 `git -C ~/.summon diff` 核对改动。

Menu Bar 的 Settings 区改的是同一批整机字段（sentinel、`messaging` 的 `cot_visible` / `hide_thinking` / `cot_tool_details` / `badge_effort` / `idle_ttl_secs` / `idle_retire_secs`、`menubar.show_counts`、开机自启），经 `summon menubar set <key> <value>` 按行改写 config.yaml 并校验读回值；`summon menubar settings` 列出当前值及在 agent.yaml 里覆盖了同一项的 agent。
