# lifewiki — design

## Principles

1. **The log is sacred.** It is append-only and stores the user's text verbatim.
   Everything else in the system is derived and can be thrown away and rebuilt.
2. **Capture must be instant and offline.** `lw add` does a file append and
   exits. No network, no model, no waiting. Anything that could fail slowly is
   deferred to `lw build`.
3. **Plain files first.** The whole system is files on disk you can read with
   `cat` and edit with vim if you must. No database unless something forces us
   to have one, and nothing so far does.
4. **Structure is derived, not demanded.** The user should never have to pick a
   category, fill a field, or learn a syntax. Classification is the language
   model's job, downstream.
5. **Git for the mutable part.** The wiki changes constantly; git is already the
   best tool we have for "mutable but with an inspectable, revertible history".
6. **Nondeterminism is quarantined.** Models only ever write to the wiki, never
   to the log. A bad model run is a bad commit: reviewable and revertible.

## Layout on disk

One directory, one git repo, default `~/.lifewiki` (override with
`$LIFEWIKI_HOME`).

```
~/.lifewiki/
├── .git/
├── config.toml           # user settings (model, editor, timezone...)
├── log/
│   ├── 2026-07.jsonl     # append-only, one JSON object per line
│   └── 2026-08.jsonl
├── wiki/
│   ├── index.md
│   ├── people/ada.md
│   ├── projects/lifewiki.md
│   └── journal/2026-08-11.md
└── state/
    ├── cursor.json       # last event folded into the wiki
    └── runs/             # pipeline run logs, prompts, model output
```

**One repo, and the log is versioned in it** (S7). The deciding reason is backup:
the log is the only thing in the system that can't be regenerated, and sharing a
repo means one `git push` protects it. Secondarily, git makes unintended mutation
of the log detectable, which matters because the log is immutable only *by
convention* — nothing at the filesystem level enforces it, so a bug in our own
append path is the likeliest threat.

Two things this deliberately does *not* claim:

- **Git is not how we know the log's past.** The log is append-only, so its state
  at any past moment is a *prefix* of its current state; since ULIDs sort, "what
  did the log look like at the last build?" is `id <= cursor` against today's
  files. Nothing in the pipeline should ever read the log's git history — it
  reads the log and the cursor. Versioning is for backup and tamper-detection,
  full stop.
- **The "wiki reflects log through X" commit isn't meaningful because of git.**
  That guarantee rests entirely on the cursor.

The accepted cost is redaction: `lw redact` exists precisely because some bytes
need to be gone, and git keeps them in history. We chose backup over erasure, and
the command says so in its output.

### Commit timing and the backup gap

`lw add` does not commit (S12) — it appends and `fsync`s, staying instant. The
log is committed by the next `lw build`, alongside the wiki changes it produced.

That leaves a real gap: events written since the last build live in exactly one
place, the working tree. `fsync` protects them from a crash, not from losing the
machine — which is the thing we said versioning was *for*. So:

- `lw status` shows pending event count and the age of the oldest, warning past a
  configurable threshold (default 24h).
- `lw sync` commits pending events and pushes, with no model and no build. Cheap
  enough for a cron job or a shell hook.

Commits are made by `lw`, not by the user:

- `log: add 01K2...` — one commit per capture (cheap, and gives you off-site
  backup for free if you push somewhere)
- `wiki: fold 12 events (01K2A..01K2M)` — one commit per pipeline run

## Layer 1 — the event log

Full spec in [event-log.md](event-log.md). In short: monthly JSONL files,
ULID-based sortable IDs, one object per line holding `id`, `ts`, `text`, and
optionally a typed relation to an earlier event.

## Layer 2 — the pipeline

`lw build` folds new events into the wiki. Sketch of the loop:

1. Read `state/cursor.json`, collect events after it.
2. Batch them (by count, or by "everything since last build").
3. For each batch, ask the model to plan: which wiki pages does this touch, and
   what needs to change? The model sees the batch, the current content of
   candidate pages, and a page index.
4. Apply edits page by page (model returns whole-file rewrites — simpler and
   more reliable than diffs at this scale).
5. Commit the wiki changes, advance the cursor, write the transcript to
   `state/runs/<timestamp>/` so a weird edit can be explained after the fact.

Guarantees we want: idempotent (re-running with no new events is a no-op),
resumable (crash mid-run leaves a dirty tree we can reset), and auditable (every
change traceable to a run and an event). An event with `revises` is handed to the
model along with the event it supersedes, so the link never has to be guessed —
though the model still has to read the prose to decide whether the old value
should be erased or kept as history (see S6 in [decisions.md](decisions.md)).

Rebuilding from scratch (`lw rebuild`) replays the whole log into an empty wiki.
Expensive, but it's the escape hatch that lets us change prompts and page
conventions later without being stuck with old decisions.

## Layer 3 — the agent

`lw ask "when did I start running again?"` — a tool-using loop with read-only
access: grep the wiki, read pages, read the log, read git history. It answers
with citations (page + event id), and refuses to guess when the wiki is silent.
Search starts as ripgrep over Markdown; embeddings only if that proves
insufficient.

## CLI surface (target)

```
lw init                create ~/.lifewiki and its git repo
lw add <text>          append an event (also: piped stdin, or $EDITOR if no args)
lw log [-n 20]         recent events
lw show <id>           one event, its revision chain, and anything revising it
lw search <query>      literal/regex search of the log
lw revise <id> <text>  append an event superseding an earlier one
lw undo                revise away the event you just added
lw status              pending events, last build, dirty state, backup age
lw sync                commit pending events and push (no model, no build)
lw build               fold new events into the wiki
lw rebuild             rebuild the wiki from the whole log
lw ask <question>      query the wiki
lw wiki <page>         open/print a wiki page
lw doctor [--repair]   integrity checks (torn lines, dangling revises)
lw redact <id>         rewrite an event's text away (see event-log.md)
```

Phase 1 is everything above except `build`, `rebuild`, `ask`, and `wiki`.

## Build order

1. Event log + capture + read commands. Useful on day one as a journal, with no
   model involved.
2. Pipeline. Start dumb: append every event to a dated journal page. Then add
   the model-driven page routing.
3. Agent.

## Non-goals (for now)

Multi-user, sync/conflict resolution, mobile capture, encryption at rest, a web
UI, attachments. Attachments (`blobs/` + a content hash in the event) are the
most likely of these to arrive early.
