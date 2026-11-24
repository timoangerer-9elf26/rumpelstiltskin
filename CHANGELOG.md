# Changelog

Config files stay backward compatible: a config that works with one release works
with every later one.

## 0.1.0 — 2026-11-24

First public release, previously an internal tool named `slack-bot`.

- `rumpel run`: Socket Mode listener that starts or continues Paseo agents per
  thread, channel or message; triggers by message, @mention or emoji reaction.
- `rumpel post | read | upload | api` for agents.
- `rumpel init` creates an agent home repo from the built-in template.
- `rumpel manifest --name <Name>` prints the Slack app manifest.
- `rumpel setup` writes tokens and config, creates the home repo and installs a
  systemd user unit or launchd agent.
- Agent prompts in English.
- Moving from `slack-bot`: rename `~/.config/slack-bot` to `~/.config/rumpel` and
  `~/.local/state/slack-bot` to `~/.local/state/rumpel`; the config format is unchanged.
