# 管理命令(终端 CLI)

这些是**用户在终端**跑的命令(不是飞书里的斜杠命令,飞书命令见 `summon guide commands`)。
用户让你「再建一个 agent」「把新机器人接进来」「看看 fleet 状态」时,用它们。

## 新建 agent(三条路,按场景选)

- `summon onboard` —— 交互向导,**新手默认路径**:脚手架 → 权限 → 凭据 → 联调 → 起 daemon,
  一条龙。用户第一次建 agent 时带他走这个。
- `summon agents add <alias>` —— 非交互单条创建。凭据从环境变量读
  (`FEISHU_<ALIAS>_APP_ID` / `FEISHU_<ALIAS>_APP_SECRET`);没设就落 draft,设好后重跑即翻
  active。可选 `--display / --role / --cli / --model / --effort / --workspace / --force`。
- `summon agents add --file <agents.yaml>` —— 批量创建(fleet-as-code)。清单是 YAML,
  map 键为 alias,值全可选:

  ```yaml
  agents:
    saturn:
      display: 土星
      role: 代码审查
      cli: claude
    nimbus:
      display: 雨云
      cli: codex
      effort: high
  ```

  凭据只写 env 名(缺省 `FEISHU_<ALIAS>_APP_ID/SECRET`,可用 `id_env`/`secret_env` 自定义),
  **真值从环境变量读,清单里不落 secret**。已存在的 alias 跳过,`--force` 覆盖。
  创建后各 agent 独立演化,清单不参与运行时路由(它不是配置源)。
- `summon bind <alias> --app-id <id> --app-secret <secret>` —— 给**已存在**的目录绑/补
  字面凭据(onboard 的非交互孪生;省略 `--app-secret` 走 stdin,不进 shell history)。

## 日常运维

- `summon daemon start|stop|restart|status` —— daemon 生命周期。重启不杀 tmux 持久会话。
  重启段脱离当前会话执行:codex / cursor agent 在自己的一轮里跑,这一轮会随停机被打断,重启照样跑完。
- {{upgrade}}
- `summon agents status` —— fleet 状态(谁在跑一轮、谁空闲)。
- `summon agents disable <alias>` —— 禁用后续任务，允许当前轮完成。daemon 对消息回复「agent 已禁用」，对定时触发通知任务标识及未启动原因；状态跨重启保留。
- `summon agents enable <alias>` —— 恢复接收后续任务。禁用期间被拒绝的任务保持跳过。
- `summon agents reload` —— 手工建完 agent.yaml 后热加载上线(免重启;只新增,不删改)。
- `summon doctor` —— 体检:tmux / CLI / 凭据 / 配置。出问题先跑它。
- `summon bridge ...` —— 【高级场景】外部 Claude/Cursor/Codex 会话接管进飞书,默认路径用不到。
