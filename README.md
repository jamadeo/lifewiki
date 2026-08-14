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
| `lw status` | How far the wiki lags, and what isn't backed up yet. |
| `lw sync` | Commit and push the log without building. Cron-safe. |
| `lw doctor` | Check the log for damage. |
| `lw redact <id> --reason <why>` | Erase one entry's text. See below. |

Build the wiki — needs a model, costs money, takes time:

| | |
|---|---|
| `lw build` | Fold new entries into the wiki. |
| `lw rebuild --force` | Discard the wiki and derive it again from the whole log. |

`lw <command> --help` explains any of them. `add`, `log`, `search`, `build` and
`rebuild` have short aliases (`a`, `ls`, `s`, `b`, `rb`).

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

`-v` adds the model's reasoning and untruncated output; `-q` prints only the
summary. Full transcripts land in `state/runs/`.

### Iterating on the prompt

Because the wiki is derived, improving the pipeline means re-deriving the past.
`--into` builds elsewhere so a run can't touch the real wiki, and `--to` bounds
it at an entry so results stay comparable as the log grows:

```sh
lw rebuild --into /tmp/wiki-v2 --to 01KZYX8F
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

Reconciliation asks a lot of a small model: many turns, well-formed tool calls
throughout. Pages record which model wrote them (`generated.by:
lifewiki/local-…`), so mixed-provenance wikis stay auditable.

## What's stored

```
~/.lifewiki/
  config.toml          model provider and settings
  log/2026-08.jsonl    one file per month, one entry per line, append-only
  wiki/                the generated wiki (an OKF bundle)
  state/cursor.json    how far the wiki has been folded
  state/runs/          build transcripts (gitignored)
```

One git repository holds both log and wiki. A single `push` backs up the one
thing that can't be regenerated, and each build is one commit covering wiki,
cursor and log together — so `git revert` of that commit undoes a build
completely.

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

Phases 1 and 2 work end to end: capture, and a wiki that reconciles itself.
Phase 3 — `lw ask`, querying the wiki in prose — is designed but not built.

Known gaps:

- **`wiki/index.md` is not maintained.** `lw init` creates it and `lw rebuild`
  deletes it, and nothing regenerates it. OKF specifies an index of titles and
  descriptions; it would also give the agent a cheap way to route over pages it
  hasn't read.
- **Cross-links depend on the model remembering to add them.** Nothing validates
  that a link resolves to a real page.
- **`lw doctor` only checks that log lines parse.** It doesn't validate wiki
  conformance, though the same OKF checks that gate every write could be reused.
- **The agent can fail to search for the right thing.** Bounded turns and
  validated writes protect the wiki's integrity, not its accuracy. A missed
  contradiction leaves a stale fact with nothing marking it.

## Documentation

- [`docs/design.md`](docs/design.md) — architecture and principles
- [`docs/event-log.md`](docs/event-log.md) — the log format and its contract
- [`docs/wiki-format.md`](docs/wiki-format.md) — wiki pages as an OKF subset
- [`docs/pipeline.md`](docs/pipeline.md) — what reconciliation has to solve
- [`docs/decisions.md`](docs/decisions.md) — 40 decisions, with rejected
  alternatives and what's still open
