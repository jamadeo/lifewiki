# Recording the demo

`demo.sh` runs a full walkthrough: capture three notes, build the wiki, ask two
questions. About **70 seconds** at default speed.

```sh
cargo build --features local        # or plain `cargo build` for a hosted model
./demo/demo.sh                      # watch it once before recording
```

It runs against `/tmp/lifewiki-demo` and never touches `~/.lifewiki`.

## Recording

No recorder is installed here. [asciinema](https://asciinema.org) gives the
smallest artifact and copy-pasteable text:

```sh
brew install asciinema
asciinema rec -c ./demo/demo.sh lifewiki.cast
asciinema upload lifewiki.cast
```

For a GIF, [vhs](https://github.com/charmbracelet/vhs) or `agg`:

```sh
brew install agg && agg lifewiki.cast lifewiki.gif
```

Use an 80–100 column terminal. Wiki pages and the agent's indented tool output
wrap badly below 80, and above ~110 the text drifts away from the left edge.

## Knobs

| variable | default | why you'd change it |
|---|---|---|
| `DEMO_SPEED` | `0.035` | seconds per typed character; `0` disables the animation |
| `PAUSE` | `1.2` | beat after each command |
| `PROVIDER` | `local` | `anthropic` to record against a hosted model |
| `LW` | `target/debug/lw` | path to the binary |

## What it shows, and why in that order

1. **Two `lw add`s** — capture is plain prose with nothing to fill in.
2. **`lw log`** — entries are committed as they arrive.
3. **A third entry contradicting the first** ("Ada moved to Porto").
4. **`lw build`** — the agent searches, finds the Lisbon page, and rewrites it.
   This is the point of the whole project, so it gets the most screen time.
5. **`cat wiki/people/ada.md`** — the result, with footnotes citing the log
   entries behind each claim.
6. **`lw ask`** — a question answered from the wiki, with sources.
7. **`lw ask` for something absent** — it says it doesn't know rather than
   guessing. Worth showing: for a record of your own life, a confident wrong
   answer is the failure that matters.
8. **`git log`** — one commit per entry, one per build.

## Warm-up

With `PROVIDER=local` the script loads the model into the daemon *before*
clearing the screen, then resets the repo. Otherwise the first command sits for
about a minute loading 17 GB, which says nothing about the tool.

The recording therefore shows warm timings. That is honest for a returning user
but not for a first run — worth a caption if you publish it.

## If a take goes wrong

The agent is non-deterministic, so takes differ. Things that have gone wrong:

- **The model narrates instead of answering.** Re-run; if it persists, a hosted
  provider (`PROVIDER=anthropic`) is far more reliable for a recording.
- **A page lands somewhere unexpected** (`/family/ada.md` rather than
  `/people/ada.md`), which breaks the `cat` line. Check the path before the
  take, or replace that step with `lw ask`.
- **It splits Ada across two pages** instead of reconciling. That is the real
  failure mode this project is about, and it is rarer with a stronger model.

Record two or three takes and keep the clearest.
