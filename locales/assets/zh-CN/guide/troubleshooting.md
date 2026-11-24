# 排错

症状 → 查什么 → 怎么修。按出现频率排序。每条都先给**用户能自查的动作**,需要你(agent)
动手的会标明。

## 机器人在飞书里不回消息

按顺序查:

1. **应用发布了吗**:飞书开放平台 → 你的应用 →「版本管理与发布」,必须有已发布版本,
   否则飞书里搜不到机器人、消息也进不来。
2. **事件订阅对吗**:开放平台 →「事件订阅」→ 接收方式必须是「**长连接**」,且已添加
   `im.message.receive_v1`(接收消息)事件。这是最常见的漏配。
3. **daemon 在跑吗**:终端跑 `summon daemon status`,不在跑就 `summon daemon start`。
4. **白名单**:agent.yaml 的 `allowed_users` / `operators` 不含该用户时,消息会被
   operate-gate 挡下(群里 fail-closed;两份名单都为空时,本群范围的操作另行放行飞书应用所有者)。
   新用户要先加进白名单。
5. 以上都对还不回:跑 `summon doctor`,再看 `~/.summon/run/daemon.log` 有没有该 chat 的
   入站记录。

## 卡片状态一直「⏳ 进行中」不结束

最常见原因:**你起了后台任务(Bash run_in_background),任务通知没被识别**。Claude Code
新版的任务通知格式变过,summon 旧版本会因此一直等。

- 修法:升级 summon 到最新版({{upgrade_fix}})。
- 临时绕过:你在会话里主动结束那一轮(回复用户),或用户发 `/stop` 打断后重新发起。

## 「🔒 仅授权用户可操作」

用户触发了需要 operate 权限的动作(退出会话、改额度显示、改模型等),但不在该 agent 的
`operators` 名单里。

- 修法:把用户的 union_id(优先,跨 app 稳定)或 open_id 加进 agent.yaml 的
  `operators`,保存即生效。onboard 联调时会自动抓过用户的 id,跳过联调时写入飞书应用所有者。
- 只该在某个群里给权限时,加进 `feishu.chat_operators.<chat_id>`。改模型这类效果跨群的
  操作只认 `operators`,本群名单里的人点了照样是这条提示。本群名单不是安全边界(见
  `summon guide config`「按群配置」),只给信得过这台机器的人。

## 改了配置没效果

- 改的是 `~/.summon/config.yaml` 还是 agent.yaml?全局配置只影响**没在 agent.yaml 里
  覆盖**的字段;agent 级覆盖优先。
- 字段名拼错会被静默忽略(yaml 不报错)——对照 `summon guide config` 核对字段名。
- `system_prompt_core` 是内置的,改不掉;能改的是 `system_prompt`。

## macOS 隐私授权弹窗

summon 起的 agent CLI 及其工具（脚本、编译出的小程序）申请语音识别、麦克风、桌面 / 文稿 / 外接盘等
隐私权限时，macOS 按 agent 的 CLI 认人：弹窗写的是 `claude`、`codex`、`cursor-agent` 这类可执行文件名，
授权记在「系统设置 → 隐私与安全性」里对应的条目上，不记在 summon 上（`macos.agent_privacy_identity`，
默认开）。

- 以前授给 summon 的权限（例如外接盘访问）不会转给各 CLI。agent 第一次用到时会再弹窗，按 CLI 逐个授权。
- CLI 升级后安装路径或签名变了，macOS 可能把它当成新程序再问一次。
- CLI 由 node 等解释器运行时，弹窗显示的是实际运行的可执行文件（例如 `node`）。
- 想让所有 agent 继续共用 summon 的授权，在 `~/.summon/config.yaml` 写
  `macos:` 下 `agent_privacy_identity: false`。下次起 agent 生效，已在跑的会话退出后重开才生效。

## 会话自己退出了

以下入口可以自动退出会话：

- `idle_retire_secs` 到点(默认关)—— 退出续聊,下条消息接回上下文,群里有退出回执;
- `timer_idle_retire_secs` 到点(默认开,55 分钟)—— 定时任务尾轮静默退场,不发回执。

- agent 本轮调用 `summon session end`，正常完成后退出，原回复末尾显示退出状态与归档按钮。

不想空闲自动退就把对应字段关掉(注意 `timer_idle_retire_secs: 0` 才是关)。两者都不分捕获模式,
claude 与 codex/cursor 同一条通路。

## 你(agent)被 summon 打断了

`/stop`、daemon 重启、用户点「⏸ 打断」都会打断当前轮。打断后:

- 无头(codex/cursor):软打断优先,只停这一轮,会话进程留着,下条消息接回上下文;
- tmux(claude):会话本身不受影响,重开一轮即可。
- 不要在被打断后试图「补发」上一轮的回复 —— 先看用户的新消息说什么。

## Task failed

Provider 报错或执行失败时会发独立通知卡，卡片保留具体错误原文。
例如 `Selected model is at capacity. Please try a different model.` 表示所选模型当前容量不足，
可通过 `/model` 选择其他可用模型后重新发起。通知本身不会切换模型或自动重试。
额度超限和流中断使用各自的恢复卡；用户主动打断与优雅重启接管不发通用失败卡。
Claude 的 API 报错不论落在本轮收尾之前还是之后都会发到群里：流中断（`API Error: The response stopped arriving…`
等）发带「继续」按钮的流中断卡，已经发出的半截回复保留；其余报错发失败卡并附原文。本轮收尾后才出现的报错由会话镜像
（`messaging.session_mirror`）发出，关闭镜像时只有本轮之内读到的报错会发。
子 agent 的 API 报错另发一张「子 agent 报 API 错误」提示卡，写明是哪个子 agent、错误类别与原文；它不结束主 agent 的本轮，
不带「继续」按钮，主轮进行中、收尾之后或终端里直接发起的轮都会发，与会话镜像开关无关。daemon 起来之前已经写下的报错不补发。
Daemon 硬崩后，未完成任务按恢复卡选择继续；排错时结合 `summon daemon status` 与日志。

## 要把问题交给维护者

让用户在出问题的会话里发 `/summon-export`,私聊收到的诊断包不含对话内容,可以直接转给维护者。
维护者还要看对话内容时再发 `/summon-export full`,完整包只给本机下载链接,由用户自己看过再决定交不交。
你(agent)也可以在本轮里跑 `summon session export --turn-id <本轮凭证>` 导出当前会话,把回复里
包的绝对路径交给用户。

## 通用体检

`summon doctor` —— 查 tmux / CLI 依赖 / 凭据 / 会话状态,给出可执行的修复建议。
拿不准的时候先跑它。
