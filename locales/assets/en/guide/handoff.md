# Handoff (`/handoff`)

When the user sends `/handoff`, summon asks the current agent to write a handoff file
and passes work to a receiver. Groups require @ of the current agent. Permissions match `/new`.

- `/handoff [notes]` with interactive compaction (currently Claude): write the file,
  then `/compact` in place. The session stays and work does not restart automatically.
  Instructions ask you to reread the handoff on the next user message and continue
  from notes or Next steps. The summary may or may not retain that request; the file stays on disk.
- `/handoff [notes]` on other sessions (Codex / Cursor): archive the old session
  (`/resume` can restore), then start the same agent's new session with “Read handoff <path>, then <notes>”.
- `/handoff @agent [notes]`: hand over to another local summon agent whose bot is in
  this chat and whose conversation list permits the requester. Summon delivers the
  instruction using the initiator's identity internally, without bot-to-bot @; no
  mutual `/introduce` is required. After durable delivery, the source exits with resume access.
  Invalid targets are rejected before writing anything.

Without notes, the receiver follows Next steps. Handoff stays in this chat. For
Owner-authorized cross-chat delegation with public summaries, use `summon delegate`
(see protocol), not `/handoff`.

## When asked to write a handoff

1. Write for someone with no context. Save `tasks/<date-topic>/HANDOFF.md` in the
   working directory, using the existing task directory/file when available and updating it to current state.
2. End the reply with a separate `<summon-handoff>absolute path</summon-handoff>` line.
   Summon removes it; it may appear before or after the feed tag.
3. If writing fails, explain why and omit the tag.

Handoff requires normal turn completion and an absolute path to a file written this
turn; a named receiver must still be in the chat. Otherwise the session remains and
nothing is delivered. Messages arriving during handoff are queued: after in-place
compaction they run in the same session; after new-session handoff they run after
reading the file; for another agent they return to the source using its resume rules.
If handoff fails they run normally in the current session.

## Template

- Goal and completion criteria.
- Status: completed and in-progress work, with commit / branch / file locations.
- Next steps, ordered, with an immediately actionable first step.
- Constraints and decisions: exact user corrections/rulings and rejected directions.
- Key files, commands, and test entry points.
- Unverified paths, risks, and uncertainties.

## When receiving a handoff

The first message asks you to read the file, optionally prefixed with the sender's
name. Read it first, then continue from the notes or Next steps.
