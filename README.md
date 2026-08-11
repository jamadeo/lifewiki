# lifewiki

A local-first tool for turning the messy stream of your life into a readable wiki.

This repository currently implements **phase 1 only**: the durable, append-only
JSONL event log and its capture/read/maintenance CLI. The wiki pipeline (`build`,
`rebuild`, `ask`, and `wiki`) is intentionally not implemented yet.

## Install and run

```sh
cargo install --path .
lw init
lw add "Started running again, 3km around the park."
lw log
```

The default repository is `~/.lifewiki`; set `LIFEWIKI_HOME` to use another
directory.

## Available commands

- `lw init`
- `lw add [text]` (also accepts stdin, or opens `$EDITOR` with no argument)
- `lw log [-n N]`
- `lw show <id-prefix>`
- `lw search <regex>`
- `lw status`
- `lw sync`
- `lw doctor [--repair]`
- `lw redact <id-prefix> --reason <reason>`

See [`docs/event-log.md`](docs/event-log.md) for the format and behavioral
contract.
