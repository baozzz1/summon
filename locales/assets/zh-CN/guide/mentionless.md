# 群内免 @ 对话

只有你和一个 Bot 的群，可以按群开启免 @ 对话：普通消息、附件和斜杠命令都不用 @ 当前 Bot。设置属于当前 agent，默认关闭；群里加入其他人或 Bot 后，免 @ 消息会被拒收，需要显式 @。命令的操作权限不因免 @ 改变。

## 快速开启

在目标群发送 `@当前Bot /mentionless on`。Summon 会检查：

- 发起者是真人，有当前 agent 的对话权限及本群操作权限。
- 群内恰好一名真人和一个 Bot，真人就是发起者，当前 Bot 在群中。群信息读得到时先查这一条，不满足就直接说明，不会让你去申请权限。
- 当前应用已获得接收群消息、读取群信息、读取群成员的应用权限。

全部通过才把当前 chat ID 写入 `~/.summon/agents/<alias>/agent.yaml`，下一条消息生效。检查失败时保持配置当前值并说明原因；配置检查期间有变化时，请重新执行。

发送 `@当前Bot /mentionless off` 关闭(已开启的群里可以不 @)。本操作需要相同的用户权限，不依赖应用权限查询。

这些是 Summon 原生命令，不启动模型会话。命令只操作当前群与当前 agent，不接受其它 chat ID。

## 一键进入权限申请

在目标群发送 `@当前Bot /mentionless permissions`，或直接执行 `/mentionless on`。缺少权限时，回复里会出现“申请所需应用权限”链接，指向当前应用的飞书或 Lark 权限管理页，并预选尚缺能力对应的权限。

点击链接，在权限页申请开通，按平台提示创建并发布版本、等待管理员审批。申请不代表已经授权；完成后回到群里重新发送 `@当前Bot /mentionless on`。Summon 会重新查询当前租户的授权状态。

所需能力与推荐权限：

- 接收群内未 @ 消息：`im:message.group_msg`。
- 读取群信息：`im:chat:read`。
- 读取群成员：`im:chat.members:read`。

已有兼容权限也会计入满足条件。查询失败时会给出所需权限页入口，但不会把未知状态说成已授权。成员不符合条件时，申请权限不能解决人数问题。

应用还需开启机器人能力，订阅 `im.message.receive_v1`。事件订阅与版本发布请在开放平台确认。

## 手工配置

```yaml
feishu:
  chat_reply_without_mention:
    oc_example: true
```

键是精确 chat ID，值为布尔值；不配置或设为 `false` 即关闭。特殊 YAML 格式无法安全自动改写时，快捷命令会拒写，可按此格式手动编辑。每条免 @ 消息与命令仍会实时检查成员及发送者权限，查询失败时请显式 @ Bot。

官方说明：[权限申请与审核](https://open.feishu.cn/document/uAjLw4CM/ugTN1YjL4UTN24CO1UjN/trouble-shooting/how-to-fix-the-99991672-error)、[租户授权状态](https://open.feishu.cn/document/application-v6/scope/list)。
