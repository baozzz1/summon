# summon guide (for agents)

For quoted message bodies that need retrieval by ID, see `summon guide quotes`.

> This guide ships with summon and matches the installed binary. You are the user's
> agent (Claude Code / Codex / Cursor etc.); summon connects you to Feishu. Messages
> sent to the bot become your tasks, and your replies are sent directly to Feishu.

## Mental model

Summon is a local persistent service (daemon + sidecar). It delivers Feishu messages,
returns your replies, and manages session lifecycles (persistent tmux / headless RPC /
idle retirement). You do not call summon to receive or send ordinary replies; that is
automatic. Use summon when the user asks about its capabilities, configuration, or failures.

## Chapters

- `summon guide protocol`: procedures for schedules, chat history, attachments, and delegation.
- `summon guide commands`: Feishu slash commands, buttons, and interruption.
- `summon guide handoff`: handoff file template, tags, and continuation by another agent/new session.
- `summon guide mentionless`: enable replies without @ in one-human/one-bot groups and request permissions.
- `summon guide config`: configuration, layered switches, and when to suggest settings.
- `summon guide manage`: terminal CLI for agent creation, fleet, and daemon operations.
- `summon guide troubleshooting`: symptoms, checks, and fixes.

## Four common paths

1. To change behavior (idle reminders, model, steering), read `summon guide config`.
   Edit global `~/.summon/config.yaml` or per-agent `~/.summon/agents/<alias>/agent.yaml`.
   Check whether each field applies next turn, on reconnect, or after daemon restart.
2. For Feishu problems (no reply, stuck cards, permission errors), read
   `summon guide troubleshooting`, then run `summon doctor`.
3. For capabilities available to users, read `summon guide commands`.
4. Before creating schedules, reading this chat's history, or delegating to another
   chat, read `summon guide protocol`.

## Boundaries

- App creation and publication require manual Open Platform steps; summon cannot do them.
  `summon chats` exposes only this agent's sessions, not other agents; session export has no such gate.
- `system_prompt_core` is built-in and cannot be overridden. Feishu limits such as
  message recall and card edits follow platform capabilities.
