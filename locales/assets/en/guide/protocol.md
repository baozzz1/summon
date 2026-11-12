# Procedures for agents

Read this chapter **before acting** on these capabilities. Standing conventions
contain only their entry points and strongest prohibitions; details are here.

## Schedules and reminders

Create a schedule for the **current chat**; summon delivers its content as a new task:

```shell
summon remind --turn-id <current credential> --when <frequency> --content <task> [--output progress|result] [--context resume|fresh]
```

- `--when`: `in 30m`, `at 2026-08-22T09:00`, `every 2h`, `daily 09:00`,
  `weekday 09:30`, `weekly mon,fri 10:00`, or `cron 0 9 * * 1-5`.
  Weekdays accept mon..sun / Monday / Chinese names / 0..7.
- Convert natural-language times yourself; the command does not parse natural language.
- `progress` (default) sends execution progress and final result. `result` runs quietly
  and sends only a terminal result, including explicit failure. Prefer it for recurring
  reports and inspections where only conclusions matter.
- `resume` (default) reuses context. `fresh` resets before every run using the same
  teardown as /reset, suitable when state is entirely in files. It avoids paying to
  reread irrelevant accumulated history. If a turn, queued message, or active subagent
  exists when due, that occurrence uses existing context rather than disrupting work.
- For long/multiline content use `--content -` and stdin to avoid quoting issues.
- Scheduled turns may create follow-ups. After three consecutive scheduled turns in
  the same chat without a human reply, remind refuses further self-continuation.
- Read back the created ID, time, and output mode to the user to confirm the result.
- List, disable, reassign: `summon schedules ls`, `rm <id>`, `mv <id> --agent <alias>`;
  these need no turn credential. Partially edit with `summon schedules edit <id> …`,
  including `--context fresh|resume`.

Use `mv` to change the executor while preserving chat, frequency, content, and ID.
Preserving ID keeps self-removal commands in task content correct. Targets absent
from registry are rejected: scheduler cannot route to an unknown identity and would
otherwise silently skip triggers.

**Never edit ~/.summon/schedules.yaml manually.** All agents share it; one invalid
field breaks every schedule, and bypassing ID allocation locks risks concurrent overwrite.

## End the current session

```shell
summon session end --turn-id <current credential>
```

Registers exit after normal completion, preserving context for the next user message.
No separate exit receipt is sent; the final reply gets an exit note. Queued messages
or active subagents prevent exit. Failure/interruption invalidates the registration.

## Read current Feishu chat history

Model context may not contain all chat history. Read when the user asks for a recap,
review, feasibility analysis, or prior decisions, or refers to earlier discussion that
current context cannot explain. Do not mechanically fetch history for self-contained tasks.

Obtain chat ID **only** this way:

```shell
CHAT=$(summon chat-id --turn-id <current credential>)
```

Never read `$SUMMON_CHAT_ID`: fixed at process spawn, it points to the **previous chat**
after migration. Do not search by chat name, guess, or substitute another chat ID.

Read the latest 50 messages:

```shell
LARKSUITE_CLI_NO_UPDATE_NOTIFIER=1 LARKSUITE_CLI_NO_SKILLS_NOTIFIER=1 \
  lark-cli im +chat-messages-list --as bot --chat-id "$CHAT" \
  --order desc --page-size 50 --no-reactions --format json
```

- If `has_more=true` and content is insufficient, continue the same query using its
  `--page-token`, or narrow with `--start` / `--end`. Fetch only what the task needs.
- Results are descending; restore chronological order before analysis. Distinguish
  sender, body, and thread_id; do not merge different senders' views.
- If a relevant message has thread_id and replies affect the conclusion, read it:

  ```shell
  LARKSUITE_CLI_NO_UPDATE_NOTIFIER=1 LARKSUITE_CLI_NO_SKILLS_NOTIFIER=1 \
    lark-cli im +threads-messages-list --as bot --thread "<thread_id>" \
    --order desc --page-size 50 --no-reactions --format json
  ```

This is **read-only**: do not auth login, send messages, or query other chats for it.
On permission/API/visibility failure or unread `has_more`, state actual coverage and
gaps. Never describe partial context as a complete account.

## Attachments

Summon downloads user images/files/audio/video locally before delivering the message.
Paths appear in its tail. Open them with Read or another appropriate tool; no extra
command is needed.

## Cross-chat delegation

`summon delegate` publishes a public summary to the target chat and delivers private
instructions internally to the same agent there. Use only for explicit Owner requests
or when this turn confirms autonomous delegation is authorized. Ordinary assignments
are not cross-chat authorization.

```sh
summon delegate --turn-id <current credential> --to "full existing chat name" --summary "public summary" --content <private instructions|->
summon delegate --turn-id <current credential> --new-chat "new task chat" --summary "public summary" --content <private instructions|->
```

Choose `--to` or `--new-chat`. `--to` also accepts a summon chats index. Candidate chat
IDs are for internal disambiguation; **do not ask users to find/fill chat IDs**. Names
match exactly. For duplicates, let Owner choose after checking description/membership;
never take the first. A missing existing chat does not authorize creating a new one.

- Owner enables `/delegate auto on`, disables with `off`, and views with `/delegate auto`
  in the control chat. Permission is bound to source chat, Owner, and bot identity; later
  human task turns state the current status.
- With autonomous delegation, reuse a suitable chat for independent ongoing topics;
  create only if none fits. Verify membership and keep summary content public to that group.
- New chats contain only Owner and current bot, with Owner as owner. Daemon persists
  creation; retries with the same turn credential and parameters reuse it. If outcome
  is unknown, stop retrying and ask Owner to verify.
- `--dry-run` resolves targets/actions without creation, invitation, cards, delivery, or
  consuming this turn's one-delegation allowance.
- Private content: maximum 16000 bytes; stdin for multiline. Include goal, necessary
  background, constraints, progress, and acceptance criteria. It is hidden from cards/dry-run output.
- Public summary: maximum 2000 bytes. Omission uses a fixed notice, never the private
  instructions. Cards contain only public text and random detail identifiers. Details
  are returned privately only after server verification of original requester, current
  Owner/bot identity, and card chat. Talk/operation permission does not substitute for Owner.
- Human Owner turns only: schedules, bot turns, and delegated turns cannot delegate.
  At most one task per turn; identical retries deliver once. Existing-chat invitations
  use a Feishu CLI user matching Owner identity.
- Summary arrives first; full instructions follow internally. Busy targets queue work
  and recover in that chat after restart. Progress, errors, results, questions, and new
  schedules belong to the target; source replies state target and delivery status.
- Hidden instructions are not a separate agent-context isolation boundary and do not
  guarantee model output secrecy. Carry only needed context; do not ask the agent to repeat instructions.
- CLI and daemon must both support separated summaries/private instructions; version
  mismatch rejects instead of falling back to publishing private instructions.

## Fork context from the source chat

Owner or a user with operation permission at both ends sends `/fork` in the source,
then selects new/existing target by full name. Direct forms: `/fork new <name>` or
`/fork to <full name>`. Only Owner can create chats.

Names match exactly; duplicate cards offer choices and links for verification. Users
need no chat IDs. Cards are initiator-only, valid ten minutes, and recheck source and
both permissions on submit. Cards and commands share a request record. Retry failed
requests through the original card; do not repeat creation commands after uncertain outcomes.

Source remains unchanged; target has an independent session ID. Existing target
sessions cannot be overwritten even when idle. After preparation, the next target
message creates the copy; parent history is not flooded as messages into the target.
Previously issued `/fork #<code>` invitations remain redeemable in the target chat.
