# Group conversations without @

A group containing only you and one bot may enable replies without @ for ordinary
messages, attachments, and slash commands. This per-agent/per-chat setting is off by
default. If another human or bot joins, unmentioned messages are rejected; explicitly
@ the bot. Command permissions remain unchanged.

## Enable quickly

Send `@CurrentBot /mentionless on` in the target group. Summon verifies:

- A human requester with conversation and chat operation permissions.
- Exactly one human and one bot; the human is the requester and the current bot is
  present. When readable, membership is checked first; failures do not prompt unnecessary permission requests.
- App permissions to receive group messages and read chat info/members.

Only after all pass is the chat ID written to agent.yaml, effective next message.
Failures preserve configuration and explain why. If configuration changes during
verification, run the command again.

Use `@CurrentBot /mentionless off` to disable (omit @ if already enabled). Same user
permissions apply, without querying app permissions. These native commands start no
model session and target only the current agent/chat, accepting no other chat IDs.

## Request app permissions

Send `@CurrentBot /mentionless permissions` or `/mentionless on`. Missing permissions
produce a targeted link to this app's Feishu/Lark permissions page, preselecting unmet scopes.

Request them, publish a version as prompted, and await administrator approval. A
request is not a grant. Return and rerun `/mentionless on`; summon rechecks tenant authorization.

Recommended permissions:

- Unmentioned group messages: `im:message.group_msg`.
- Chat info: `im:chat:read`.
- Members: `im:chat.members:read`.

Compatible existing scopes count. Query failures link to the requirements without
claiming authorization. Permissions cannot fix incorrect membership. Also enable Bot
capability and subscribe to `im.message.receive_v1` in Open Platform.

## Manual configuration

```yaml
feishu:
  chat_reply_without_mention:
    oc_example: true
```

Keys are exact chat IDs; values are booleans. Missing/false disables. Unusual YAML
that cannot be safely rewritten is rejected; edit manually in that case. Each
unmentioned message/command still verifies current members and sender permissions.
If queries fail, explicitly @ the bot.

Official references: [permission requests and approval](https://open.feishu.cn/document/uAjLw4CM/ugTN1YjL4UTN24CO1UjN/trouble-shooting/how-to-fix-the-99991672-error), [tenant authorization state](https://open.feishu.cn/document/application-v6/scope/list).
