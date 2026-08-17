# lifewiki

Tell it things about your life. It writes the wiki.

```sh
lw add "Ada is my sister. She lives in Lisbon and works as an architect."
lw add "Ada moved to Porto last month"
lw build
```

The second entry contradicts the first. Rather than storing both and leaving you
to reconcile them, `lw build` finds the page about Ada and rewrites it, so the
wiki says she lives in Porto — while the log still records that she once lived in
Lisbon, and when you learned otherwise.

## How it works

Three layers, each with a different relationship to truth.

**1. The log** is append-only and never edited. Entries are `{id, ts, text}` —
your words, verbatim, with no tags, types or categories to fill in. Capture
should cost nothing but typing, and any field you're asked to fill is a decision
made with less context than the model will have later.

Facts change, but the log doesn't. To correct something you just say so in a new
entry; there's no `revise` command and no link back. The pipeline works out what
changed from the prose.

**2. The wiki** is derived, mutable, and version-controlled. Because the log is
the source of truth, the wiki can be thrown away and rebuilt at any time — which
is what makes it safe for a language model to edit. Pages are markdown with YAML
frontmatter, following a subset of [OKF v0.2](docs/wiki-format.md), so every
claim cites the log entry behind it:

```markdown
---
type: Person
title: Ada
generated: { by: lifewiki/claude-opus-5, at: 2026-08-14T09:54:37-04:00 }
sources:
  - id: eNE004X79XW
    resource: /log/2026-08.jsonl#01KZYX8FMVNCK2XHNE004X79XW
    title: "Ada is my sister. She lives in Lisbon and works as an architect."
---

# Facts

Ada is the sister of [the owner](/people/me.md) and lives in Porto.[^eNE004X79XW]
```

**3. The agent** that maintains it explores the wiki with tools — search, read,
write — and decides for itself what to look at. Retrieval and reconciliation
aren't separable: whether a page matters is often only knowable after reading it,
and one read suggests the next. A fixed retrieve-then-rewrite pipeline has to
guess right in one shot, and fails silently when it doesn't.

## Install

```sh
cargo install --path .
lw init          # creates ~/.lifewiki, or $LIFEWIKI_HOME
```

Building the wiki needs a model. Either export `ANTHROPIC_API_KEY`, or run
[entirely offline](#local-inference).

## Commands

Capture and read the log — fast, offline, no model:

| | |
|---|---|
| `lw add [text]` | Record an entry. Also reads stdin, or opens `$EDITOR`. |
| `lw log [-n N]` | Recent entries, newest first. |
| `lw show <id>` | One entry in full. Ids abbreviate like git hashes. |
| `lw search <regex>` | Search what you wrote. |
| `lw status` | How far the wiki lags, and which models are configured. |
| `lw doctor` | Check the log and wiki for damage. |
| `lw redact <id> --reason <why>` | Erase one entry's text. See below. |

Build and query the wiki — needs a model, costs money, takes time:

| | |
|---|---|
| `lw build` | Fold new entries into the wiki. |
| `lw rebuild --force` | Discard the wiki and derive it again from the whole log. |
| `lw ask <question>` | Answer a question from the wiki. Read-only. |

Entries are committed to git as soon as they are added, so nothing waits for a
build to be backed up.

`lw <command> --help` explains any of them. `add`, `log`, `search`, `build`,
`rebuild` and `ask` have short aliases (`a`, `ls`, `s`, `b`, `rb`, `q`).

### Asking

```
$ lw ask "who are my children and when were they born?"
Your children are Rian, born 4 May 2021, and Avi, born 26 August 2023.
(Sources: /people/avi.md, /people/rian.md)
```

Answers come only from the wiki, and it is told to say so when something isn't
there rather than guess — this is a record of your own life, so a confident wrong
answer is worse than none.

A question can never modify the wiki: the workspace is opened read-only *and*
the write tools aren't offered to the model.

### Watching a build

Builds print each tool call as it happens, since they're slow, spend money, and
rewrite files you care about:

```
folding 2 events with lifewiki/claude-opus-5

[1] thinking…
  → search_wiki "Ada"
    /people/ada.md: Ada is the owner's sister. She lives in Lisbon…
[2] thinking…
  → write_page /people/ada.md (612 bytes)
    Wrote /people/ada.md.
[3] thinking…
  ✓ Updated Ada's page: she has moved from Lisbon to Porto.
```

Every command that runs an agent narrates itself this way. `-q` silences the
narration and prints only the result, which is what you want when piping:

```sh
lw ask -q "who are my children?" > children.txt
```

### Iterating on the prompt

Because the wiki is derived, improving the pipeline means re-deriving the past.
`--into` builds elsewhere so a run can't touch the real wiki:

```sh
lw rebuild --into /tmp/wiki-v2
diff -r wiki /tmp/wiki-v2
```

Quality here is judged by reading diffs, not by a score. Whether a page reads
correctly to the person whose life it describes isn't something a metric knows.

## Local inference

To run with nothing leaving the machine:

```sh
cargo install --path . --features metal   # or: local, cuda, vulkan
goose configure                           # download a local model
lw build --local
```

Off by default because it compiles llama.cpp from source, which needs `cmake` and
several minutes — too much to impose on someone working on the log. Model
downloads are the [goose](https://github.com/aaif-goose/goose) CLI's job, not
ours.

The first local command starts a daemon that holds the loaded model, so later
commands skip the ~60s load:

```sh
lw daemon status     # what it is holding
lw daemon stop       # free the memory (a 26B model is ~17 GB resident)
lw ask --no-daemon   # bypass it
```

Reconciliation asks a lot of a small model: many turns, well-formed tool calls
throughout. Pages record which model wrote them (`generated.by:
lifewiki/local-…`), so mixed-provenance wikis stay auditable.

## Configuring models

Each task has its own model, because they want different things: reconciliation
rewrites files over a long run and rewards a stronger model, while answering is
short, frequent and read-only.

```toml
[build]
provider = "anthropic"        # anthropic | local | none
name = "claude-opus-5"

[ask]
provider = "local"            # questions never leave the machine
```

Omit `name` for the provider's own default. `lw status` prints what each task
resolved to. `--local` on either command overrides the config for that run.

## What's stored

```
~/.lifewiki/
  config.toml          model provider and settings
  log/2026-08.jsonl    one file per month, one entry per line, append-only
  wiki/                the generated wiki (an OKF bundle)
  state/cursor.json    how far the wiki has been folded
```

One git repository holds both log and wiki. Adding an entry commits it
immediately; each build is one commit covering wiki and cursor together, so
`git revert` of that commit undoes a build completely.

Plain files throughout, no database. A heavy year of entries is a few megabytes,
so the log is parsed on every read.

## Privacy and deletion

The log is the most sensitive file on your machine, and it's a plain file: no
encryption, no signing. Treat `~/.lifewiki` accordingly, and think before
pushing it to a host you don't control.

`lw redact` is the deliberate exception to append-only, for when a password gets
pasted in. It replaces the text with a tombstone recording your reason, keeping
the id and timestamp so nothing dangles. **Git still has the old text** — erasing
it there needs `git filter-repo`. See [docs/event-log.md](docs/event-log.md).

## Status

All three phases work end to end: capture, a wiki that reconciles itself, and
prose questions answered from it — including fully offline with a local model.

Known gaps:

- **The agent can fail to search for the right thing.** Bounded turns and
  validated writes protect the wiki's integrity, not its accuracy. A missed
  contradiction leaves a stale fact behind with nothing marking it. This is the
  central risk of the design and no amount of validation fixes it.
- **Dangling links are reported, not prevented.** A per-write check would reject
  the agent's legitimate habit of linking a page it is about to create, so
  `lw build` and `lw doctor` warn instead.
- **Small local models struggle with `ask`.** Reconciliation and question
  answering both need many well-formed tool calls; a small model will sometimes
  keep searching rather than concluding a fact is absent, and exhaust its
  context.
- **No `index.md` in subdirectories.** Only the bundle root gets one, though OKF
  permits them at any level.
- **No transcripts.** Runs narrate themselves as they go, but nothing is kept
  afterwards; a build's commit message is the durable record.

## Documentation

- [`docs/design.md`](docs/design.md) — architecture and principles
- [`docs/event-log.md`](docs/event-log.md) — the log format and its contract
- [`docs/wiki-format.md`](docs/wiki-format.md) — wiki pages as an OKF subset
- [`docs/pipeline.md`](docs/pipeline.md) — what reconciliation has to solve
- [`docs/daemon.md`](docs/daemon.md) — proposed: keeping the local model loaded
  between commands
- [`docs/review.md`](docs/review.md) — where the project outgrew the idea, and
  what to cut
- [`docs/decisions.md`](docs/decisions.md) — 40 decisions, with rejected
  alternatives and what's still open
