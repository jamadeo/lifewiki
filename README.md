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
exactly as you typed them. An event is three fields — an id, a timestamp, and your
text. Nothing else: no tags, no categories, no links between entries.

Facts change, but the log never does. You just say the new thing:

```
$ lw add "oranges are my favorite fruit"
...weeks later...
$ lw add "I actually prefer apples over oranges now"
```

Both lines live in the log forever. The wiki is what changes — the pipeline finds
the page that says oranges and rewrites it to say apples. Capture is instant,
offline, and never touches a language model.

**2. The wiki** is a derived artifact: a directory of Markdown pages, mutable,
rewritten by the pipeline as new events arrive. Reconciling new facts against old
ones is its central job. Because it lives in a git repo, every rewrite is a
commit — you can diff, blame, browse, and revert. If the wiki is ever wrong, the
log is still right: improve the prompt, run `lw rebuild`, and the past is fixed.

**3. The agent** answers questions by searching the wiki (and, when needed, the
log behind it) and citing what it found.
