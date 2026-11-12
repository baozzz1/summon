# Management commands (terminal CLI)

These commands run in a terminal. Feishu slash commands are in `summon guide commands`.
Use these for creating agents, connecting new bots, or inspecting the fleet.

## Creating agents

- `summon onboard`: interactive path for new users, from scaffold through permissions,
  credentials, live verification, and daemon startup.
- `summon agents add <alias>`: noninteractive creation. Credentials come from
  `FEISHU_<ALIAS>_APP_ID` / `FEISHU_<ALIAS>_APP_SECRET`; absent credentials create draft.
  Set them and rerun to activate. Options: `--display / --role / --cli / --model /
  --effort / --workspace / --force`.
- `summon agents add --file <agents.yaml>`: batch fleet-as-code. Map keys are aliases;
  values are optional:

  ```yaml
  agents:
    saturn:
      display: Saturn
      role: Code review
      cli: claude
    nimbus:
      display: Nimbus
      cli: codex
      effort: high
  ```

  Use environment variable names only (defaults as above, or `id_env`/`secret_env`).
  Never put secret values in the manifest. Existing aliases are skipped unless
  `--force`. Agents evolve independently after creation; the manifest is not runtime configuration.
- `summon bind <alias> --app-id <id> --app-secret <secret>`: add literal credentials
  to an existing directory. Omit `--app-secret` to read stdin without shell history.

## Daily operations

- `summon daemon start|stop|restart|status`: restart leaves persistent tmux sessions
  alive. Restart detaches from its initiating turn and completes even if that headless turn is interrupted.
- {{upgrade}}
- `summon agents status`: fleet running/idle state.
- `summon agents disable <alias>`: reject future tasks while allowing the current turn
  to finish. Disabled state survives restart; rejected messages/schedules are reported.
- `summon agents enable <alias>`: resume task receipt; tasks rejected while disabled stay skipped.
- `summon agents reload`: hot-load manually created agent.yaml files without restart; additions only.
- `summon doctor`: check tmux / CLI / credentials / configuration; start here for failures.
- `summon bridge ...`: advanced external Claude/Cursor/Codex takeover; not needed for normal setup.
