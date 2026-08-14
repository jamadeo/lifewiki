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

## Local inference

To run entirely offline, with nothing leaving the machine:

```sh
cargo install --path . --features metal   # or: local, cuda, vulkan
goose configure                           # download a local model
lw build --local
```

Off by default because it compiles llama.cpp from source (requires `cmake`).
Model downloads are handled by the goose CLI, not by lifewiki.

## Documentation

- [`docs/design.md`](docs/design.md) — architecture and principles
- [`docs/event-log.md`](docs/event-log.md) — the log format and behavioral contract
- [`docs/wiki-format.md`](docs/wiki-format.md) — phase 2: wiki pages as an OKF subset
- [`docs/pipeline.md`](docs/pipeline.md) — phase 2: what reconciliation must solve
- [`docs/decisions.md`](docs/decisions.md) — decision record and open questions
