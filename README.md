# lifewiki

A local-first tool for turning the messy stream of your life into a wiki you can actually read.

You type facts, events, to-dos and opinions in plain language. They go into an
append-only log, verbatim, forever. A background pipeline uses language models to
fold each new entry into a human-readable wiki, kept in git so you can browse and
revert its history. Finally, an agent lets you ask questions of the wiki in plain
English.

```
  capture            distill             ask
  ───────            ───────             ───
  lw add        →    lw build       →    lw ask
  event log          wiki (git)          agent
  immutable          mutable, versioned  read-only
```

## Status

Design phase. Nothing is implemented yet.

- [docs/design.md](docs/design.md) — architecture and principles
- [docs/event-log.md](docs/event-log.md) — phase 1: the event log (detailed spec)
- [docs/decisions.md](docs/decisions.md) — decision record and open questions

## The three layers

**1. The log** is the source of truth. It is append-only and stores your words
exactly as you typed them. Facts change, but the log never does: a change is a new
event that points at an older one. Capture is instant, offline, and never
touches a language model.

**2. The wiki** is a derived artifact: a directory of Markdown pages, mutable,
rewritten by the pipeline as new events arrive. Because it lives in a git repo,
every rewrite is a commit — you can diff, blame, browse, and revert. If the wiki
is ever lost or corrupted it can be rebuilt from the log.

**3. The agent** answers questions by searching the wiki (and, when needed, the
log behind it) and citing what it found.
