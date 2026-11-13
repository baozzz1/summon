# 命令面

飞书会话里可用的命令。**群里用命令要先 @机器人**；私聊直接发。开启了免 @ 的一人一 Bot 群(见 `summon guide mentionless`)里命令也可以不 @。
你(agent)自己不需要执行这些命令 —— 它们是**用户**在飞书里用的。你需要这份清单,是为了
用户问「我能怎么操控你 / 这个会话」时答得上来。

群里只确认「照做了」的命令(如 `/quota on`、`/mentionless off`、`/model <完整 id>` 改本群)不回文字,在那条
命令上贴完成表情;回执里有用户推不出的信息(翻转后的状态、回到的默认值、对其他群的影响、额度消耗提示)、
要留意后果(会话被重置、要重启会话才生效)或出错时,照常回一条话题消息。私聊照常回文字。

## 会话控制

- `/help` —— 打印当前 agent 从飞书聊天框支持的完整命令清单
- `/session` —— 唤出会话控制面板(模型切换、额度显示、退出按钮都在里面)
- `/session clean` —— 退出当前会话，保留续聊入口,下次接着聊(不丢上下文)
- `/session reset` —— 关闭自动续聊入口，下次新建会话
- `/quit` 或 `/exit` —— 同 `/session clean` 的一步生效版(无确认)
- `/reset`、`/archive` 或 `/arc` —— 📦 归档（一步生效）：退出后下条消息新建会话；`/resume` 可恢复
- `/resume` —— 恢复本对话最近结束的会话，无论上次是归档还是退出
- `/new <内容>` —— 用这句话开一个新会话(不转发给 agent;不带内容等同 `/reset`)
- `/queue <内容>` —— 这条排在正在跑的那一轮之后单独跑,不插进那一轮(没有在跑的轮时就是一条普通消息)
- `/handoff [@agent] [备注]` —— 当前 agent 写交接文件:@ 本群另一个 agent 就交给它(当前会话随后退出);
  不 @ 时有交互式压缩命令的会话(目前是 claude)原地 `/compact`,其余(codex / cursor)归档当前会话、
  开新会话从交接文件接着做(`summon guide handoff`)
- `/clear` —— 轮换原生 session(带确认卡)
- `/cd <路径>` —— 切换本会话的工作目录
- `/pwd` —— 不启动 agent，显示配置解析出的启动目录；session 已运行时同时显示真实运行目录，二者不一致会标记漂移。仅授权用户可查；群聊的完整路径只私信操作者，群内不展示路径

## 思考与过程显示

- `/cot on|off` —— 开关本会话的 CoT(过程)显示。开时 CoT 里有思考(灰字)、工具和过程中说的话(正常字体);关时只保留运行状态与暂停按钮,只看终态回复
- `/cot thinking on|off` —— 开关思考块显示(per-chat 持久化,daemon 重启后恢复)
- `/cot tools on|off` —— 开关 CoT 里的工具调用(关时只显示思考正文)

## 模型与额度

- `/model <id>` —— 改**本群**模型;`/model default <id>` 改这个 bot 的**默认**模型(没有本群模型的群
  跟着换);`/model inherit` 清掉本群模型、回到默认。只认候选目录里的 id(claude 另认 `opus` `sonnet`
  `fable` 这类内置别名与当前配置的模型;输入恰是唯一一个候选的一截也认),对不上就不改、回几个候选
  按钮;裸 `/model` 回用法和候选按钮。按钮在 `/session` 面板里也有,分「本群」「设为默认」两组;
  目录之外的 id 用本机 `summon agents model <agent> <id>` 设(改的是默认)。
- `/effort <档位>` / `/effort default <档位>` / `/effort inherit` —— 思考档位,分层同 `/model`。
  Cursor 没有独立 effort,思考档位在 `/session` 面板 Cursor 参数段的 reasoning 一行。
- 提示别人切模型时,给能直接复制的完整命令(`/model <候选里的 id>`),或让对方发 `/model` 打开
  候选按钮;不要给需要手打、猜出来的模型名。
- `/quota on` / `/quota off` —— 回复卡角标的剩余额度显示开关(claude/codex 显示「5h N% ↻重置时刻 7d N% ↻重置日期时刻」,其余显示
  「quota N%」);裸 `/quota` = 翻转当前态
- `/set badge.cli|badge.model|badge.effort|badge.ctx on|off` —— 回复卡角标里 CLI 类型 / 生效模型 / 思考档位 / 上下文用量各自的显示开关(都默认开;`badge.ctx` 只管显示，上下文过重提醒照常);`/session` 面板角标那一行也能点;裸 `/set` 列出本会话全部开关
- `/fast on|off` —— Codex / Cursor 的本群 Fast 开关(改变额度消耗,只接受显式 on/off)。Claude agent 上交给
  Claude Code 原生 `/fast`:它写进用户级 `~/.claude/settings.json`,本机共用这份配置的 Claude 会话都会
  切到 Fast(更高费率),当前模型不支持时会顺带切模型;裸 `/fast` 弹选择面板
- `/mode <id>` —— Cursor ACP 会话模式切换

## 任务与协作

- 打断正在跑的一轮 —— turn 进度卡上的「⏸ 打断」按钮,或直接发 `/stop`(原生 CoT 形态下卡片没有按钮,客户端的停止键发的就是这条文本)
- `/schedules`、`/remind` —— 定时任务管理；创建卡可选“过程与结果”或“仅结果（减少推送）”
- `/introduce` / `/unintroduce` —— 在本群认识 / 解除认识 bot 或同事。被认识的 bot 能直接 @ 你派活;
  被认识的同事(真人)能在这个群里跟你说话,而且算内部人 —— 能让你写文件、跑命令。这和 operate
  权限(`chat_operators`,管会话控制)是两回事
- 跨群委派：Owner 指定目标群；`summon delegate` 在群卡发布摘要，将完整指令交给该群 agent 执行。
- `/delegate auto on|off`：Owner 开启或关闭本群自主分流授权；`/delegate auto` 查看状态。
- `/fork`：在源群用卡片选择新建群或已有群；也可用 `/fork new 新群名`、`/fork to 已有群完整名称`。
  目标已有会话时拒绝覆盖，完成后到目标群发下一条消息继续。详见 `summon guide protocol`。
- `/fleet` —— 查看本机/跨机 fleet 状态(谁在跑、谁空闲)
- `/bridge` —— 外部会话接管(高级场景:把已有的 Claude/Cursor/Codex 会话绑进飞书群)

## 终端与控制台

两张卡都**只私聊**发给发命令的人,链接不进群;都要本群的操作权限。

- `/term` —— 私聊你一张浏览器终端卡(只读查看 / 可操作),只对 claude 会话有效;codex / cursor 这类无头 agent 会回一句没有终端
- `/dash` —— 私聊你一张 summon 控制台卡:点「打开控制台」在网页里改整机 / agent / 按群配置、定时任务和说明文件。链接只能用一次,有效期与文件预览链接相同(`preview_server.token_ttl_secs`);进入后 12 小时内有效;群里发的不在群里应答,私聊发不出去时才在群里说一句原因
- `summon dashboard open` —— 在本机直接打开控制台(一次性链接,60 秒内有效);`--print` 只打印链接。Menu Bar 的「Dashboard…」走的也是它
- `summon usage` —— 不开网页看用量与费用:`--from` / `--to` 指定日期区间(本机日历,含两端,默认今天),`--by day|agent|chat|cli` 选汇总维度,给四项 token、四项费用与未计价部分;`--json` 输出机器可读结构。口径与控制台用量页一致(含轮外用量)
- `summon insights` —— 不开网页看控制台「洞察」页的统计:回复后到同一会话里同一个人下一条消息的间隔分布(当日 9–22 点口径)、长间隔期间其它会话的活动、下一条消息的类型与每日输入量。`--from` / `--to` 指定日期区间(本机日历,含两端),都不给时默认近 30 天(含今天,与洞察页的默认时间段相同),`--agent` 只看一个 agent 的会话,`--json` 输出与控制台洞察页相同的结构(含规则结论的 key 与参数)。只读运行记录,不读消息内容

## 导出排查材料

会话出了问题、要交给维护者看时,把这个会话最近几轮的材料打成一个包。两档:

- `/summon-export` —— **诊断包**,不含对话内容:轮记录、事件的类型与时长、日志里只带 id / 计数 / 状态的那些行、CLI 原生记录的模型 / 版本 / 用量,飞书用户换成 `user-1` 这样的编号。作为飞书文件私聊发给你,可以直接转给维护者
- `/summon-export full` —— **完整包**,另附对话内容与原生记录原件(cursor 的会话库没法打码,两档都不带)。不上传飞书,私聊你一张本机下载卡;自己看过再决定交给谁
- 两档都要本群的操作权限,都不收凭据;默认取最近 3 轮(在跑的那一轮算一轮)。群里发的不在群里应答,私聊发不出去时才在群里说一句原因
- `summon session export <TARGET>` —— 在本机终端直接导出,daemon 不在也能用;`--full` 出完整包,`--turns N` / `--since 2h` 调时间窗,`--max-mb` 调体积上限,`--json` 输出包路径与清单。agent 在会话里用 `summon session export --turn-id <本轮凭证>` 导出当前会话。包在 `~/.summon/run/exports/`,只留最近 20 个、7 天内的

## 斜杠透传

以上是 summon 自有命令。**其余斜杠命令**(如 `/compact`)会透传给你背后的 CLI
(claude / codex / cursor)按它们自己的语义执行;不认识的命令 summon 会安全拒绝,不会瞎执行。

## 卡片上的按钮

- 「⏸ 打断」—— 中止当前轮
- 操作面板和提醒卡的「关闭卡片」—— 撤回当前卡，不影响会话或任务；回复、思考、镜像和上下文胶囊等内容卡保留正文
- Session 卡「刷新」—— 原地重读当前状态
- Session 卡的 CoT / 思考正文 / 工具详情开关 —— 与 `/cot` 三组命令共用 per-chat 配置
- 「退出」/「📦 归档」—— 同 `/session clean` / `/session reset`
- 「恢复」/「全部忽略」—— 优雅重启会自动接管在飞的轮(过程也接着刷同一条);硬崩、或证不出那一轮还在跑时才给恢复选项

## 终端会话管理

- `summon session` / `summon session ls` —— 查看与 Menu Bar 同源的状态和 TARGET；`--json` 提供结构化输出，`session ls --watch` 每 2 秒刷新，Ctrl+C 退出
- `summon session read <TARGET>` —— 标已读；`--reply <message_id>` 可固定先前看到的回复，并发到达的新回复保留未读
- `summon session exit <TARGET>` / `summon session archive <TARGET>` —— 退出保留续聊 / 归档后新开；运行中须显式加 `--interrupt`
- `summon comments list` —— 查看文档评论任务的状态、起草进程和完整任务 ID
- `summon comments stop <id> --agent <别名>` —— 停止指定 agent 的本次评论起草并回收进程，任务结束后不会自动重新起草；`<id>` 使用 `comments list` 中的完整任务 ID
- `summon autostart on` / `off` / `status` —— 与 Menu Bar 的 Launch at login 共用设置；开启时默认保留当前 `keep_alive`，兼容 `enable` / `disable`

文档评论的开关、起草超时与生命周期见 `summon guide config`；也可通过 `/dash` 打开控制台的“文档评论”页面查看、配置和停止任务。

Shell 补全：zsh 初始化 compinit 后用 `source <(SUMMON_COMPLETE=zsh summon)`，bash 用 `source <(SUMMON_COMPLETE=bash summon)`，fish 用 `SUMMON_COMPLETE=fish summon | source`。子命令和选项来自命令定义，TARGET 随会话动态更新。`--watch --json` 每行输出一份完整列表。

## Agent 本机命令

- `summon delegate --turn-id <本轮凭证> --to <群> --summary <公开摘要> --content <私有正文>` —— 群卡发摘要，任务在该群执行；`--new-chat <名称>` 可替代 `--to` 创建任务群，`--dry-run` 只预演；用法见 `summon guide protocol`
- `summon preview --turn-id <本轮凭证> <路径>` —— 确认文件能否预览；把路径用反引号写进最终回复，summon 在回复末尾生成打开按钮，能和你对话或有操作权限的人点了会私聊收到打开链接
- `summon schedules ls` —— 列出全部定时器；`summon schedules rm <id>` —— 软禁用
- `summon schedules edit <id> [--when "daily 09:30"] [--prompt "正文"] [--output progress|result] [--enabled true|false]` —— 非交互部分更新，未给字段保持不变

## 群内免 @ 配置

`@当前Bot /mentionless on|off|permissions`：检查并开启本群、关闭本群、查询应用权限并取得申请链接。需要对话与本群操作权限。开启时先核对群内只有发起者和当前 Bot，再核对应用权限，不满足不会写配置。开启后本群的普通消息、附件与命令都可以不 @。详见 `summon guide mentionless`。
