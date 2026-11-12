# Configuration

Configuration is scoped by file; activation depends on the field:

- Global: `~/.summon/config.yaml`, affecting all agents. Specify overrides only;
  omitted fields inherit built-in defaults.
- Agent: `~/.summon/agents/<alias>/agent.yaml`, affecting that agent only.

Before suggesting a setting, check its current value and override source. After an
authorized edit, verify according to the activation rules below.

## Language

Top-level `language` is machine-wide: `auto` (default), `zh-CN`, or `en`. Use the Language menu in the dashboard’s upper-right corner to change it.
It applies to Dashboard, CLI, built-in Feishu messages, and Menu Bar on this machine,
and hot-switches without daemon restart.
Auto reads `.lang` from `lark-cli config show`; if absent, unreadable, or unavailable,
it uses the operating system's first preferred language. Chinese selects Simplified
Chinese; other supported/unknown preferences fall back to English. Explicit language
wins. User messages, names, custom prompts, and external tool output are not translated.

## Main agent

Top-level `primary_agent` names the agent whose CLI, model and account run the analysis on the
dashboard's Insights page. Empty (default) means automatic: the agent with the most tokens in the
last 30 days, or, before there is any usage, the earliest configured agent (by when its agent
directory was created). A named agent that is missing, disabled or not routable also falls back to
automatic. Only the Insights analysis uses it; a main agent on cursor cannot run the analysis
(one-shot runs cannot reliably disable hooks). Changes take effect immediately.

## Layers and activation

Messaging precedence: built-in → global → agent → chat. Only the layered switches
listed below support chat overrides; other fields are not `/set` keys.

| Setting | When it applies |
|---|---|
| Language | Immediately on reload; no daemon restart |
| Message display / layered switches | Snapshot next turn; running turn keeps its settings |
| Models, directory, environment, CLI path, permission policy | Next startup/reconnect; dedicated command receipts take precedence |
| agent.reactions, agent.feed, opening primer | Next primer injection; Claude/Codex startup/resume, Cursor new session |
| codex_platform_tools | New native session; /quit and resume do not refresh tools |
| preview_server / web_terminal listener settings | Daemon restart |
| sentinel | Next poll |

`/set` shows effective values/sources. Use `/set <key> on|off|inherit` to set chat
preferences or restore inheritance. Groups require @ unless mentionless is enabled.

| /set key | messaging YAML field | Default |
|---|---|---|
| cot.visible | cot_visible | true |
| cot.thinking | hide_thinking (inverse) | shown; hide_thinking: false |
| cot.tools | cot_tool_details | false |
| timer.idle_retire_reset | timer_idle_retire_reset | false |
| badge.effort | badge_effort | true |
| agent.reactions | agent_reactions | true |
| agent.feed | agent_feed | true |

For example, `/set cot.tools off` hides tools only here; agent-wide uses
`messaging.cot_tool_details: false` in agent.yaml.

## Document comments

`messaging.document_comments_enabled` (default false), shown as
`document_comments.enabled` in setting lists, supports global/agent configuration,
not chat overrides. It applies when configuration is read. Disabled notifications
are acknowledged without creating tasks.

`messaging.document_comment_timeout_secs` defaults to 300 positive seconds. Agent
settings override global; changes apply to the next draft. The dashboard Document
comments page supports scope selection, enabling, timeout, and restoring inheritance.

Drafts start with document link, comment quote, and thread. A read-only tool bound to
the document fetches relevant docx sections, at most 2000 characters per call.

View/stop tasks in Menu Bar Document comments (unfinished only), the dashboard, or
`summon comments list` followed by `summon comments stop <full-id> --agent <alias>`.
Stopping reaps this draft process and persists its terminal state. Complete drafts
are published only after owner approval on a Feishu card.

Each comment uses a temporary process. Failure, timeout, cancellation, or restart
interruption terminates the task without automatic retry. Failure/timeout/restart
attempt one owner failure notice; verified documents include title, comment, and
original link. Manual stops, disabling cancellations, or undetermined owners produce
no notice. Out-of-scope documents only record task state: they must belong to the bot
or be provably bot-created and transferred to owner. Disabling cancels running drafts
and blocks publishing, retaining unstarted tasks/frozen drafts; new notifications
while disabled end immediately.

## Idle and retirement

- `messaging.idle_ttl_secs`: 86400 (one day) before reminder card. Increase when
  reminders are intrusive; API-billed agents may use 31536000 (one year). **0 means
  immediate reminders, not disabled** (idle > ttl).
- `messaging.idle_retire_secs`: off by default. Exit idle sessions while preserving
  resume context for next message. Enable, for example 86400, to reduce memory use.
- `messaging.timer_idle_retire_secs`: 3300 (55 minutes); **0 disables**. Scheduled
  tail turns exit silently after this idle duration without reminder cards.
- `messaging.timer_idle_retire_reset`: false. If true, scheduled retirement closes
  automatic resume; next trigger starts fresh, with /resume restoring the last session.
- `messaging.idle_scan_secs`: 300 seconds. Exited/archived sessions receive no reminders.

## During turns

- `headless_stall_secs`: 1800 positive seconds for true silence in Codex/Cursor.
  Global/agent changes apply next turn. With tools, subagents, or fresh engine
  heartbeat, the limit is at least 30 minutes and never below the configured value.
  Heartbeat freshness is independently 240 seconds; waiting for humans pauses decisions.
- `steer_transcript` / `steer_rpc`: true. New messages steer the CLI instead of waiting
  for turn end, potentially combining answers. Progress marks actual consumption.
  Each route is independently disableable. `steer_cli_session` separately controls
  Cursor CLI interrupt/resume steering (true); Cursor ACP does not support it.
  While Claude's main agent is done and only background tasks remain, new queued
  messages run as independent turns with new requester permissions/credentials.
  Background subagents retain original permissions; later main output joins the then
  active reply or is mirrored if no turn runs. With session_mirror off, new work waits
  for backgrounds. While main is active, only the same requester may steer; others queue.
- `hide_thinking`: false. Hides readable reasoning, useful for models exposing thinking.
- `cot_visible`: true. When off, retain status/animation/pause and final reply only.
- `cot_tool_details`: false. Enable for commands/arguments/results. When off, only
  thinking remains; short tool descriptions still appear as thinking text. With
  hide_thinking on and tool details off, status, spoken progress, boundaries, and
  interruption notes remain; CoT itself stays enabled.
- `tool_trail`: false. Force the short tool trajectory even when thinking is visible.
- `background_continuation_max_secs`: 3600; **0 disables**. Maximum wait for background
  commands/subagents before forced settlement. Increase if tasks routinely exceed an hour.

All fields above belong under messaging.

## Models

Backend catalogs drive panel candidates: Codex/Traex model/list and Cursor
availableModels. New upstream entries appear without configuration edits. Feishu
/model accepts catalog IDs (Claude also accepts built-in aliases/current model);
unmatched input leaves values unchanged and offers nearest candidates. For IDs outside
the catalog use local `summon agents model <agent> <id>`.

- `messaging.models_by_cli`: per-CLI cold-start fallback. Claude has no catalog API,
  so this is its sole candidate source.
- `messaging.default_model_by_cli`: pin onboard/bind defaults to avoid upstream changes.
  Unpinned CLIs follow backend-reported defaults.
- Agent top-level `model` / `effort`: bot defaults. Per-chat `/model` and `/effort`
  override them in state/threads/<tag>/model and effort; inherit restores defaults.
  Cursor parameterized ACP defaults use state/cursor-parameters.json; chat files store
  only changed axes. Claude /model and /effort also update local user settings
  (~/.claude/settings.json model and modelSettings.<model>.effortLevel), affecting later
  unparameterized Claude launches. Summon explicitly passes configured values.
- `reasoning_summary`: Codex none / concise / auto / detailed.
- `messaging.efforts`: candidates intersected with backend support; models is a legacy
  flat fallback, with models_by_cli preferred.

## Working directory

Agent runtime.working_dir is default; runtime.chat_working_dirs binds directories per
chat. Suggest the latter when an agent repeatedly works in the wrong directory for one group.

/pwd is a read-only comparison: .cwd (/cd) → chat_working_dirs → working_dir determines
next startup. Running sessions also read the real Claude pane or RPC pool cwd. Drift
is reported without substituting configuration or starting/restarting a session.
Next cold start/reconnect converges. Group queries need operate permission; full
paths go privately to the operator, never to the group.

## Per-chat configuration

Use exact chat IDs from `summon chat-id --turn-id <current credential>` in that chat.

- runtime.chat_working_dirs: directories as above.
- feishu.chat_reply_without_mention: boolean map, absent/false disables. true enables
  unmentioned ordinary messages, attachments, and commands in one-human/one-bot groups.
  Hot-reloads next inbound; conversation/command permissions remain. Example:

  ```yaml
  feishu:
    chat_reply_without_mention:
      oc_example: true
  ```

  Use @CurrentBot /mentionless on, off, or permissions; see mentionless guide.
  Requires im:message.group_msg, chat info, and im:chat.members:read or compatible scope.
  Every message checks counts and unique sender. Query failure or config changes
  during verification reject silently. [Design](../../../../docs/spec/2026-10-20-chat-mentionless-design.md).
- feishu.chat_operators: unioned with operators only in that chat. Cross-chat effects
  (model/effort, shared Cursor parameters/Fast, Bridge/recall) require agent operators.
  Use this for chat-local operation access rather than granting agent-wide access.
  It does not grant talk permission (allowed_users does). **It is not a sandbox**:
  chat operators can /stop, pass slash commands, use terminal, and request writes on
  the machine, potentially adding themselves to operators. Grant only to trusted users.
- ~/.summon/agents/<alias>/chats/<chat_id>/CLAUDE.md: chat-specific standing requirements
  after global/agent requirements, applied next cold start. Put chat-only constraints
  here rather than agent CLAUDE.md or SOUL.md, which affect every chat.

## Advanced

- messaging.system_prompt: custom base primer; built-in system_prompt_core is immutable.
- messaging.slash_blacklist: per-backend slash command blocklist.
- runtime.env: subprocess environment (endpoints/proxies etc.), passed unchanged.
  Configure credentials using the CLI's own authentication; never put secrets in examples/replies.
- claude.status_line_in_managed_sessions: false. Enable if your statusLine also reports
  to a statistics platform; otherwise it only duplicates badge information in the pane.
  Quota/context collection works independently in either mode.
- cursor.quota: personal quota badge. Cursor does not push quota; summon queries
  cursor.com at turn start only for sessions with quota display on. Same credentials
  share results and interval_secs (600). Default credentials: cursor-agent login
  (macOS keychain; Linux ~/.config/cursor/auth.json). token_command may supply an access
  token using only basic environment such as PATH/HOME. Agents with cli_cmd/env must
  provide runtime.cursor_quota_token_command in their own environment, otherwise account
  identity cannot be proven and no query runs. Invalid credentials hide the badge;
  enabled: false disables machine-wide.

## Other messaging settings

| Field | Default / purpose |
|---|---|
| completion_summary_card | true; separate completion summary |
| session_mirror | true; mirror Claude terminal replies to Feishu |
| mask_email | true; after moderation rejects email, retry once with username initial only (p***@example.com); untouched otherwise. Off sends explanation; hot reload |
| progress_max_steps | 3; 0 treated as 1 |
| badge_effort | true; show effort badge |
| agent_reactions | true; inject semantic reaction conventions, independent of system status reactions |
| agent_feed | true; inject feed summary; off previews start of body |
| document_comments_enabled | false; global/agent comments draft and approval |
| document_comment_timeout_secs | 300 positive seconds; global/agent |
| codex_platform_tools | false; native preview/react/send_image; new session required |
| reactions / reaction_meanings | safe set and meanings from installed version; reactions guide |
| default_reaction / working_reaction / done_reaction / interrupted_reaction | OK / OnIt / DONE / SILENT |
| prompt_template | localized source/text template with source, who, text, chat placeholders |
| session_exited | per-reason text overrides |

Session exit keys: manual_kill, manual_kill_reset, panel_clean, panel_reset, idle_retired,
agent_archived, group_disbanded, bot_removed, shutdown_interrupt. Override only needed
keys. agent_archived describes agent-initiated exit; next ordinary message resumes.

system_prompt_core and system_prompt_reactions are built-in. interrupt_enabled,
interrupt_transcript, and interrupt_prefix are obsolete; users interrupt with /stop or pause.

## Agent identity and environment

Top-level agent.yaml: name (stable alias), display, status (active), role (agent/steward),
model, effort, reasoning_summary, aliases (user ID → nickname), feishu, runtime, messaging.
See manage for setup.

Feishu app_id_env/app_secret_env name credential variables; scope identifies app storage.
allowed_users governs talk (empty allows); operators governs operations (empty falls
back to talk); chat_operators adds local permissions. With both operation lists empty,
group operations are rejected except chat-scoped operations allowed to the discovered
app owner. A nonempty list disables this owner fallback.

| runtime field | Purpose / default |
|---|---|
| cli / cli_cmd | backend / executable override; prefer onboard |
| capture | Cursor cli/exec selects CLI Session, otherwise ACP; SID namespaces differ |
| working_dir / chat_working_dirs | default/chat directory; /cd overrides |
| resume | inherits global resume_sessions unless explicitly set |
| model_context_window | Codex/Traex token window; otherwise CLI default |
| codex_sandbox / codex_approval_policy | override global Codex policy |
| codex_skip_git_repo_check | false; Codex exec non-Git exception |
| env | subprocess environment |
| cursor_quota_token_command | agent quota credential command |
| execution.target / execution.working_dir | remote tools target / absolute remote directory |
| tmux_name / seed | session name / initial input; usually omit |
| rpc_model | Cursor ACP parameter migration; maintain through session controls |
| headless | compatibility only; Claude fixed Transcript, Codex/Traex fixed RPC |

Global codex.sandbox defaults to danger-full-access, also read-only/workspace-write.
approval_policy defaults to never, also untrusted/on-request. Interactive requests go
to authorized Feishu operators. Policy applies on next startup/resume.

## Machine services and connections

Top-level global config.yaml, outside messaging:

| Field | Default / purpose |
|---|---|
| language | auto; zh-CN/en override; hot switch |
| agents_dir | ~/.summon/agents; startup definitions |
| resume_sessions | true; runtime.resume may override |
| poll_ms / debounce_ms | 400 / 1200 ms |
| default_domain | feishu.cn API domain |
| sidecar_bin | sidecar executable |
| tools | absolute external paths, ~/ supported; empty uses daemon PATH |
| menubar.terminal | command argv for agents open; automatic by default |
| menubar.show_counts | true; working/waiting counts beside icon |
| remotes | empty; fleet machines with name, ssh_host, summon_bin |
| execution_targets | empty; remote targets with provider: claude-code |

Tools: tmux, lark_cli, claude, codex, traex, cursor_agent, git, ssh, npx, pkill, open,
xdg_open, launchctl, systemctl, loginctl, pbcopy, osascript. tools takes precedence over
legacy tmux_bin and sentinel.lark_cli_path.

preview_server: enabled true, bind 127.0.0.1, port 7765, public_base_url empty,
token_ttl_secs 3600, max_file_mb 10. Set a browser-accessible public URL for remote
hosts; empty probes local routable addresses, and failure issues no links.
web_terminal: enabled true, port 7766, shares preview_server.bind. These services read
configuration at daemon startup.

Sentinel is opt-in: enabled false. Read summon sentinel --help first. Other defaults:
dry_run false, interval_secs 60, fast_interval_secs 5, fast_top_k 20, slow_batch_size 2,
top_k 50, max_age_secs 300, per_hour 6. Default join/create triggers are the existing
Chinese phrases; invite_verbs is /invite. Configure trigger strings in join_verbs /
create_verbs. blocklist excludes chats; aliases adds names; owner_open_id binds requester;
lark_profile selects login identity. Deployment/user authorization still bounds capabilities.

## Edit safely

1. Confirm the field in this guide.
2. Edit global config.yaml or the relevant agent.yaml.
3. Wait for next turn/reconnect or restart only as required, then verify actual behavior.
4. Run summon doctor. Preserve prior values and restore affected fields on failure.
   If ~/.summon is a Git repository, inspect git -C ~/.summon diff.

Menu Bar Settings writes the same machine fields through summon menubar set and
validates readback: sentinel, cot_visible/hide_thinking/cot_tool_details/badge_effort,
idle_ttl_secs/idle_retire_secs, show_counts, and autostart. summon menubar settings lists
values and agent overrides.
