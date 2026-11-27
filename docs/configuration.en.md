# Configuring summon

[简体中文](configuration.md) | English

summon's behavior is defined by YAML files under `~/.summon/`. Fields, defaults and when each change takes
effect are documented in the Summon Guide. The Summon Guide is the manual shipped with the binary, so it
always matches the version installed on your machine.

**We recommend letting an agent configure summon for you.** Tell the agent in Feishu what you want; it
reads the manual, edits the configuration and verifies the result. You can edit the files yourself, but then
you have to check field names, override layers and activation timing on your own.

## Let an agent configure it (recommended)

The opening instructions summon gives every connected agent say: when the user asks about configuration,
switches or when a change takes effect, read `summon guide config` first; when something is broken, read
`summon guide troubleshooting` first. So you only need to describe the result you want, not the field
names. For example:

- "Don't show tool call details in this group."
- "Retire sessions after two idle hours."
- "Before each turn, make sure the local proxy on port 18765 is running; if it isn't, have launchd start it."
- "`summon doctor` reports a warning. Find out how to fix it."

An agent usually handles it in this order:

1. Read `summon guide config` to confirm the field, its allowed values and when it takes effect.
2. Read the current configuration and find where the value comes from: built-in default, global
   configuration, the agent's configuration, or a per-chat override.
3. Explain the planned change, and edit `config.yaml` or `agent.yaml` once you agree.
4. Verify it at the right moment (next turn, session reconnect or daemon restart), then run `summon doctor`.

What an agent cannot do: create and publish the app on the Feishu Open Platform. You do that step by
hand; see `summon guide manage`. Built-in conventions such as the message protocol cannot be overridden by
configuration either.

## Summon Guide

Run `summon guide` in a terminal for the index, and `summon guide <chapter>` for a chapter. The links below
point to the manual sources on the repository's `main` branch, which may be newer than the version you
have installed; your local `summon guide` output is authoritative.

| Chapter | Contents |
| --- | --- |
| [`summon guide`](../locales/assets/en/guide/index.md) | Index: mental model, chapter list, boundaries |
| [`summon guide config`](../locales/assets/en/guide/config.md) | Configuration fields, override layers, activation timing, and when to suggest which setting |
| [`summon guide commands`](../locales/assets/en/guide/commands.md) | Slash commands, buttons and interruption in Feishu |
| [`summon guide manage`](../locales/assets/en/guide/manage.md) | Terminal management: creating agents (onboard / agents add / bind) and daemon operations |
| [`summon guide troubleshooting`](../locales/assets/en/guide/troubleshooting.md) | Common symptoms, what to check, how to fix |
| [`summon guide protocol`](../locales/assets/en/guide/protocol.md) | Procedures agents follow: schedules, chat history, attachments, cross-chat delegation |
| [`summon guide handoff`](../locales/assets/en/guide/handoff.md) | `/handoff` file template and tags |
| [`summon guide mentionless`](../locales/assets/en/guide/mentionless.md) | Checks and permission requests for replying without @ in one-human/one-bot groups |
| [`summon guide quotes`](../locales/assets/en/guide/quotes.md) | Reading the original text of a quoted message by ID |
| [`summon guide reactions`](../locales/assets/en/guide/reactions.md) | Adding semantic reactions to messages |

## Configuration files

| File | Scope |
| --- | --- |
| `~/.summon/config.yaml` | Global, applies to all agents. Write only the fields you override; the rest keep their built-in defaults |
| `~/.summon/agents/<alias>/agent.yaml` | One agent: identity, names of the Feishu credential variables, runtime (CLI, working directory, environment, required services) and messaging settings |
| `~/.summon/agents/<alias>/.env` | That agent's credentials, such as the Feishu App ID and App Secret |
| `~/.summon/.env` | Global environment variables referenced as `${VAR}` from `config.yaml` |

Messaging switches are layered: built-in default → global → agent → current chat. Only some switches can be
overridden for the current chat with `/set` in Feishu. Fields take effect at different moments: some on the
next turn, others after a session reconnect or a daemon restart. See "Layers and activation" in
`summon guide config`.

Besides editing the YAML directly, the local Web dashboard and the Settings section of the macOS Menu Bar app
can change some of these fields; they write to the same files.

## Editing by hand

1. Run `summon guide config` to confirm the field name, allowed values and activation timing.
2. Edit `config.yaml` or the agent's `agent.yaml`, keeping the old value so you can roll back.
3. Wait for the next turn, reconnect the session or restart the daemon as required, then check the result.
4. Run `summon doctor` to check the configuration.

## Example: declaring a required service

Some agents depend on a long-running local service, such as a local proxy that connects the CLI to another
model. Declare such a service under `services` in the global `config.yaml`, then reference it from the
dependent agent's `agent.yaml` with `runtime.requires`. Before each turn summon probes the service: if it is
healthy, the turn starts; if not, summon runs the fallback command, and if the service is still not ready the
turn fails with an error instead of starting in a broken environment.

For a service kept alive by launchd:

```yaml
# ~/.summon/config.yaml
services:
  local-proxy:
    healthcheck:
      http: "http://127.0.0.1:18765/healthz"
    # On a failed probe, hand the restart to launchd; oneshot marks this as a one-shot handoff, so launchd keeps owning the process
    command: ["/bin/launchctl", "kickstart", "-k", "gui/501/com.example.local-proxy"]
    oneshot: true
```

```yaml
# ~/.summon/agents/<alias>/agent.yaml
runtime:
  requires:
    - local-proxy
```

Replace `501` in `gui/501` with your user ID (`id -u`). Then check the state with `summon services status`
and run a probe with `summon services check`. For all fields, including `tcp` and `command` probes,
`agent_env` injection and keeping processes across daemon restarts, see "Machine services and connections"
in `summon guide config`.
