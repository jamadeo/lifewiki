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
   category, fill a field, learn a syntax, or link an event to an earlier one.
   Classification *and reconciliation* are the language model's job, downstream.
   A log event is three fields: id, timestamp, text.
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
├── wiki/                 # an OKF v0.2 bundle (subset); see wiki-format.md
│   ├── index.md          # progressive-disclosure directory listing
│   ├── people/ada.md     # markdown + YAML frontmatter, cites log events
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

- `log: 12 events (01K2A..01K2M)` — pending captures, committed by `build`/`sync`
- `wiki: fold 12 events (01K2A..01K2M)` — one commit per pipeline run

## Layer 1 — the event log

Full spec in [event-log.md](event-log.md). In short: monthly JSONL files,
ULID-based sortable ids, one object per line holding exactly `id`, `ts`, `text`.
No relations, no optional fields — an event is just something you said, when you
said it.

## Layer 2 — the pipeline

Pages are written as a subset of [OKF v0.2](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md)
— markdown with YAML frontmatter carrying provenance and trust metadata. Rationale
and exactly which subset in [wiki-format.md](wiki-format.md). Note that OKF
contributes nothing to reconciliation; it standardises how pages *record* what
they know and where it came from, not how contradictions get resolved.

`lw build` folds new events into the wiki. Sketch of the loop:

1. Read `state/cursor.json`, collect events after it.
2. Batch them (by count, or by "everything since last build").
3. For each batch, ask the model to plan: which wiki pages does this touch, and
   what needs to change? The model sees the batch, the current content of
   candidate pages, and a page index.
4. Apply edits page by page (model returns whole-file rewrites — simpler and
   more reliable than diffs at this scale). Pages are OKF-subset markdown, so
   each rewrite must also carry correct frontmatter: updated `generated`, and
   `sources` citing the events behind the claims (see wiki-format.md).
5. Commit the wiki changes, advance the cursor, write the transcript to
   `state/runs/<timestamp>/` so a weird edit can be explained after the fact.

Guarantees we want: idempotent (re-running with no new events is a no-op),
resumable (crash mid-run leaves a dirty tree we can reset), and auditable (every
change traceable to a run and an event).

### Reconciliation is the pipeline's core job

Because the log has no relations (S6), *all* of the work of noticing that a new
event contradicts an old fact happens here. "I actually prefer apples over
oranges" must find the page saying oranges and rewrite it. This is not a
side-feature; it is the main thing the pipeline does, and step 3 above has to be
built around it rather than treating it as an edge case.

Concretely, that shapes the design:

- **The plan step must retrieve by *topic*, not just by novelty.** Before writing,
  the model needs the existing pages that the event might contradict. Search the
  wiki for salient nouns from the event text; unlike a `revises` pointer, this
  works whether or not the user was explicit.
- **Page conventions must state a default for supersession.** For stable facts
  (favourite fruit, employer) the page shows the current value; for things whose
  history is the point (jobs, addresses) it keeps a dated history. Encoding this
  in the prompt and page templates, rather than per-event, is what makes the
  model's judgement consistent.
- **Failure must be visible.** If the model can't tell what an event refers to
  ("I've changed my mind about that"), it should leave pages alone and append to
  `wiki/unresolved.md` with the event id. Silent no-ops are the dangerous
  outcome — a wiki confidently showing a stale fact.
- **`lw rebuild` is the safety net.** Since the log is intact and complete, a
  prompt improvement plus a rebuild retroactively fixes every reconciliation the
  old prompt got wrong. This is what makes it acceptable to put this much
  judgement in the model: mistakes are in a derived artifact, never in the data.

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
lw show <id>           one event in full
lw search <query>      literal/regex search of the log
lw status              pending events, last build, dirty state, backup age
lw sync                commit pending events and push (no model, no build)
lw build               fold new events into the wiki
lw rebuild             rebuild the wiki from the whole log
lw ask <question>      query the wiki
lw wiki <page>         open/print a wiki page
lw doctor [--repair]   integrity checks (log torn lines; wiki OKF conformance)
lw redact <id>         rewrite an event's text away (see event-log.md)
```

Phase 1 is everything above except `build`, `rebuild`, `ask`, and `wiki`.

## Designing for pipeline iteration

Phase 1 records raw intent; the interesting work is the reconciliation algorithm,
which we expect to rewrite repeatedly. So the architecture should treat "compare
two versions of the pipeline over the same events" as a first-class operation,
not something bolted on later. Three consequences:

**A rebuild must be able to target an arbitrary directory.** `lw rebuild --into
<dir>` lets us materialise the same log under prompt A and prompt B and `diff -r`
the results. If the wiki path is hardcoded anywhere, this becomes painful, so it's
a parameter from the first commit — even in phase 1, where nothing builds yet.

**A rebuild must be able to stop at an event.** `lw rebuild --to <id>` bounds a
run so results are reproducible as the log keeps growing. Cheap to provide,
because ULIDs sort: it's a string comparison. This is the concrete reason phase 1
exposes `Log::since(id)` and `Log::upto(id)` rather than just "read everything."

**A test corpus is a real artifact.** Evaluating reconciliation needs events that
deliberately contradict each other, arrive out of order, or reference things
vaguely. That corpus should live in the repo (`tests/corpora/*.jsonl`) as plain
log files — which they are, since a log is just JSONL. Writing one by hand while
building phase 1 is also the best way to pressure-test the format.

Note what makes all of this possible: the log is complete and verbatim, so any
pipeline version can be run over any prefix of history at any time. Keeping phase
1 dumb is what buys the freedom to be clever later.

### Judging quality
Deferred to phase 2, but the shape is worth stating: the useful signal is not a
score, it's a **diff between successive pipeline versions over a fixed corpus**.
Human review of "what changed and did it get better?" beats an automated metric
here, because the thing being judged is whether a page reads correctly to the
person whose life it describes. Every run already writes its transcript to
`state/runs/`, which is what makes a bad edit explainable after the fact.

## Build order

1. **Event log + capture + read commands.** Useful on day one as a journal, with
   no model involved. Plus a hand-written test corpus.
2. **Pipeline.** First real milestone is retrieve-then-rewrite: search the wiki
   for topics in the new event, hand candidate pages to the model, rewrite in
   place. *Not* "append to a dated journal page" — that does no reconciliation, so
   it would build a wiki full of contradictions and teach us nothing about the
   hard part.
3. **Agent.**

## Non-goals (for now)

Multi-user, sync/conflict resolution, mobile capture, encryption at rest, a web
UI, attachments. Attachments (`blobs/` + a content hash in the event) are the
most likely of these to arrive early.
