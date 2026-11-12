# Troubleshooting

Symptoms, checks, and fixes in common order. Start with actions users can run themselves.

## Bot does not reply in Feishu

1. App published? Open Platform → app → Version Management & Release must have a
   published version, or the bot cannot be found or receive messages.
2. Event subscriptions correct? Use **Long connection** and add `im.message.receive_v1`.
   This is a common omission.
3. Daemon running? `summon daemon status`, then `summon daemon start` if absent.
4. Access lists? agent.yaml allowed_users/operators may block the user. Group operations
   fail closed; with both empty, group-scoped operations separately allow the app owner.
   Add new users to the appropriate list.
5. Run `summon doctor`, then look for the chat's inbound records in `~/.summon/run/daemon.log`.

## Card stays in progress

A common cause is a background Bash task whose completion notification was not recognized.
Claude notification formats have changed; older summon versions may wait indefinitely.
Upgrade ({{upgrade_fix}}). Temporarily finish the turn explicitly or have the user
send `/stop`, then restart the request.

## “Only authorized users may operate”

The operation (exit, quota display, model etc.) needs operate permission.
Add the user's union_id (preferred across apps) or open_id to agent.yaml operators;
saving applies immediately. Onboard captures IDs during verification, or uses app owner when skipped.

For chat-only access use `feishu.chat_operators.<chat_id>`. Cross-chat effects such as
model defaults still require operators. Chat operators are not a sandbox boundary;
grant only to people trusted with this machine (see config).

## Configuration has no effect

- Agent overrides take priority over global config.yaml.
- Misspelled fields may be silently ignored; check `summon guide config`.
- system_prompt_core is built-in; only system_prompt is overridable.
- Language is machine-wide, with immediate hot switching; auto uses `lark-cli config show`
  `.lang`, then the OS's first preferred language when absent/unavailable.

## Session exited by itself

- idle_retire_secs (off by default): exit and resume next message, with a group receipt.
- timer_idle_retire_secs (on by default, 55 minutes): silent exit after a scheduled tail turn.
- Agent called summon session end: normal completion exits and adds a note/archive button to its reply.

Disable the relevant setting if undesired; timer_idle_retire_secs requires 0 to disable.
Claude and Codex/Cursor share these rules.

## Agent interrupted

/stop, daemon restart, or Interrupt stops the turn. Headless Codex/Cursor soft-interrupts
only the turn and preserves the session process/context. Claude tmux session remains.
Do not resend the previous reply after interruption; read the user's new message first.

## Task failed

Provider/execution failure sends a separate notice with its specific error text.
For `Selected model is at capacity. Please try a different model.`, choose another
available model with /model and restart. The notice does not switch or retry automatically.
Quota exhaustion and stream interruption use their own recovery cards. User interruptions
and graceful restart reattachments do not send generic failure cards. After a hard
crash, use recovery card choices; inspect daemon status and logs.

## Give material to a maintainer

Ask the user to send /summon-export in the affected session. The private diagnostics
archive has no conversation content and can be forwarded. If content is needed,
/summon-export full provides a local download card; the user reviews before sharing.
Agents may export current session with `summon session export --turn-id <current credential>`
and give the user the archive's absolute path.

## General health check

`summon doctor` checks tmux, CLI dependencies, credentials, and session state, and
suggests concrete fixes. Start there when uncertain.
