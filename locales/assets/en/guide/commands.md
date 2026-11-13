# Commands

Commands users send in Feishu. **Groups require @bot**, except one-human/one-bot chats
with mentionless enabled; DMs do not. Agents use this list to explain user controls,
not to issue these commands themselves.

Group commands that merely acknowledge an explicit action use a completion reaction
instead of text. Receipts with unknown/new state, inherited defaults, cross-chat
impact, quota notices, reset/restart consequences, or errors still send a topic reply.
DM commands retain text receipts.

## Session controls

- /help: full command list for the current agent.
- /session: control panel with model, quota, and exit controls.
- /session clean: exit, preserving context for next message.
- /session reset: close automatic resume; next message starts fresh.
- /quit or /exit: immediate /session clean, without confirmation.
- /reset, /archive, /arc: immediate archive; next message starts fresh, /resume can restore.
- /resume: restore this conversation's most recently ended session, archived or exited.
- /new <message>: start a new session with that text; empty equals /reset.
- /queue <message>: run separately after the active turn, without steering; ordinary
  message if no turn is running.
- /handoff [@agent] [notes]: write a handoff; another local agent in this chat receives
  it if mentioned. Otherwise Claude compacts in place; Codex/Cursor archive and start
  a new session from the file. See handoff guide.
- /clear: rotate native session with confirmation card.
- /cd <path>: change this session's working directory.
- /pwd: read-only configured startup directory plus actual runtime cwd if running,
  with drift reported. No agent start. Authorized users only; full host paths are
  sent privately, not shown in groups.

## Thinking and progress

- /cot on|off: show thinking/tools/spoken progress, or status/pause/final reply only.
- /cot thinking on|off: persist thinking visibility per chat across daemon restarts.
- /cot tools on|off: show tool calls; off retains thinking text only.

## Models and quota

- /model <id>: chat override. /model default <id>: bot default, affecting chats without
  overrides. /model inherit: clear chat override. Accepts catalog IDs; Claude also
  accepts built-in aliases (opus/sonnet/fable), configured model, and unique candidate
  fragments. Unknown input leaves settings and offers candidates. Bare /model shows
  usage/buttons; /session also has chat/default groups. For out-of-catalog defaults,
  use local summon agents model <agent> <id>.
- /effort <level>, /effort default <level>, /effort inherit: same layers. Cursor uses
  reasoning in the session panel's Cursor parameters instead of separate effort.
- When recommending a switch, provide a copyable complete /model command or ask the
  user to open /model candidates; do not invent a model name to type.
- /quota on|off: reply badge quota. Claude/Codex show 5h and 7d remaining/reset times;
  others quota N%. Bare /quota toggles.
- /set badge.cli|badge.model|badge.effort|badge.ctx on|off: show the CLI type, effective model, effort or context usage in the reply badge (all on by default; badge.ctx only affects display, context warnings still fire). The /session panel has the same toggles in its badge row. Bare /set lists all switches.
- /fast on|off: explicit Codex/Cursor chat Fast switch, affecting quota use. Claude
  uses native /fast, which writes user ~/.claude/settings.json and affects other
  Claude sessions sharing it, may change unsupported models, and has a higher rate.
  Bare Claude /fast opens a panel.
- /mode <id>: Cursor ACP session mode.

## Tasks and collaboration

- Interrupt using progress card Interrupt or /stop (native CoT may have no button;
  the client's stop key sends /stop).
- /schedules, /remind: schedule management; creation offers Progress and result or Result only.
- /introduce, /unintroduce: introduce/remove bots or colleagues in this chat. Introduced
  bots can @ you with tasks; humans can talk and count as internal requesters able to
  ask for writes/commands. Separate from operate permissions for session controls.
- Cross-chat delegation: Owner names target; summon delegate publishes summary and
  privately delivers full instructions to the target agent.
- /delegate auto on|off or bare: Owner controls/views this chat's autonomous delegation.
- /fork: select new/existing target from source card; also /fork new <name> or /fork to
  <full name>. Existing target sessions are never overwritten. Continue with next
  target message; see protocol.
- /fleet: local/remote fleet running/idle state.
- /bridge: advanced external Claude/Cursor/Codex session takeover.

## Terminal and dashboard

Both cards are **private to the requester**, never put links in groups, and require
chat operation permission.

- /term: Claude browser terminal card, view-only/interactive. Headless Codex/Cursor
  have no terminal and report that.
- /dash: dashboard for machine/agent/chat settings, schedules, and documents. Link
  single-use, lifetime preview_server.token_ttl_secs, then a 12-hour browser session.
  Groups receive no reply unless DM fails.
- summon dashboard open: local one-use link, 60 seconds; --print prints only. Menu Bar
  Dashboard uses the same command.
- summon usage: tokens/cost without browser. --from/--to inclusive local dates (today
  by default), --by day|agent|chat|cli, four token/cost components and unpriced usage,
  --json for structured output. Same definitions as dashboard, including out-of-turn usage.
- summon insights: the dashboard Insights statistics without browser. Gaps from a reply
  to the same person's next message in that chat (same day, 09:00-22:00), activity in other
  chats during long gaps, next-message types and daily input. --from/--to inclusive local
  dates; with neither, the last 30 days including today (the Insights page default). --agent
  limits to one agent's chats, --json prints the dashboard structure including finding
  keys and parameters. Reads run records only, never message content.

## Export troubleshooting material

- /summon-export: diagnostics without conversation content. Includes turn records,
  event types/timing, allowlisted ID/count/status logs, and native model/version/usage.
  Feishu users become user-1 etc. Sent as a private Feishu file for maintainers.
- /summon-export full: additionally includes conversation and native originals.
  Neither mode includes Cursor's unredactable SQLite session database. Full archives
  are not uploaded to Feishu; a private local-download card lets the user review before sharing.
- Both require chat operation permission and exclude credentials. Default latest
  three turns, counting the running one. Groups receive no reply unless DM fails.
- summon session export <TARGET>: local terminal, works without daemon. --full,
  --turns N / --since 2h, --max-mb, --json. In-agent current session uses --turn-id.
  Archives live under ~/.summon/run/exports/, retained up to 20 archives / 7 days.

## Other slash commands

Other slash commands such as /compact are handled by the backend CLI through the
configured safe command route. Unavailable/unsafe forms are rejected rather than guessed.

## Card buttons

- Interrupt stops the turn.
- Dismiss card on panels/reminders recalls only that card, not sessions/tasks.
  Content cards (replies, thinking, mirrors, context capsules) retain bodies.
- Refresh rereads state in place.
- CoT/thinking/tools switches share per-chat configuration with /cot.
- Exit / Archive match /session clean / reset.
- Recover / Ignore all: graceful restarts reattach live turns and their progress;
  hard crashes or unprovable liveness offer manual recovery instead.

## Terminal session management

- summon session / session ls: same status/TARGET source as Menu Bar; --json,
  session ls --watch every two seconds, Ctrl+C exits.
- session read <TARGET>: mark read. --reply <message_id> pins the previously seen
  reply and preserves concurrent newer replies.
- session exit / archive <TARGET>: preserve resume / start fresh. Running turns need --interrupt.
- summon comments list: comment task states, processes, and full task IDs.
- summon comments stop <id> --agent <alias>: stop/reap this draft; no automatic redraft.
- summon autostart on|off|status: shared with Menu Bar Launch at login; enabling
  preserves keep_alive unless explicit. enable/disable aliases supported.

Comment configuration, timeout, and lifecycle: config guide or dashboard Document comments.

Shell completions: zsh after compinit, source <(SUMMON_COMPLETE=zsh summon); bash,
source <(SUMMON_COMPLETE=bash summon); fish, SUMMON_COMPLETE=fish summon | source.
Subcommands/options derive from command definitions; TARGET is dynamic. --watch --json
emits one full list per line.

## Agent local commands

- summon delegate --turn-id <credential> --to <chat> --summary <public> --content <private>:
  target execution with public summary; --new-chat <name> instead of --to, --dry-run for preview.
- summon preview --turn-id <credential> <path>: confirm preview; include the absolute
  path in backticks in final reply for an open button. Authorized viewers receive links privately.
- summon schedules ls / rm <id>: list / soft-disable.
- summon schedules edit <id> [--when "daily 09:30"] [--prompt "task"]
  [--output progress|result] [--enabled true|false]: partial updates, omitted fields unchanged.

## Mentionless groups

@CurrentBot /mentionless on|off|permissions checks/enables, disables, or requests app
permissions for the current chat. Requires talk and chat operation permission. Enable
first proves exactly the requester and current bot are present, then checks app
permissions; failure does not write. Enabled groups accept messages, attachments,
and commands without @. See mentionless guide.
