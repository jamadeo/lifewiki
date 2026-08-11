# Decision record

All phase-1 decisions are settled. Newer decisions are appended as they're made.

## Settled

| # | Decision | Rationale | Alternative rejected |
|---|----------|-----------|----------------------|
| S1 | Log format is JSONL, one object per line | atomic single-write appends, greppable, verbatim text via JSON escaping, clean git diffs | bespoke delimited text (escaping traps), TOML/YAML per entry (not append-friendly), sqlite (nothing needs it yet) |
| S2 | One file per month under `log/` | human-sized files, guessable names, bounded reverse reads | one big file (unbounded), one file per event (inode churn) |
| S3 | ULID ids, prefix-matchable like git | sortable, offline, no counter, safe to retype | UUIDv4 (unsortable), sequence numbers (needs shared state), content hashes (not sortable, dupes collide) |
| S4 | Timestamps are RFC 3339 with local offset | "2am" and "in Tokyo" are content, not presentation; still totally ordered | UTC only (destroys information), epoch ints (unreadable) |
| S5 | Event has only `id`, `ts`, `text`, optional `revises` | no capture-time friction; the model has more context later than the user does now | required type/tags/category fields |
| S6 | **One** relation, `revises`, as a bare id — no `kind` field | D4: prose already says whether something is a correction, an update, or a withdrawal, and the model reads the prose. A `kind` field with one value is dead weight | `correct`/`update`/`retract` as separate kinds (S6 v1, superseded); `{"kind":...,"target":...}` |
| S7 | Log and wiki share one git repo, and the log **is** versioned | D1 + D9: one `push` backs up the one thing that can't be regenerated. Accepted cost: `lw redact` can't fully erase | two repos; unversioned log + `state/digests` integrity check |
| S8 | Shell out to the `git` binary | no build friction, trivially inspectable, behind a trait if needed later | `libgit2`/`gix` |
| S9 | No index, no cache, full parse on read | a year of heavy use is a few MB; an index is a rebuildable optimisation, not architecture | sqlite index, in-memory daemon |
| S10 | `lw redact` is an explicit, documented exception to immutability | you will eventually paste a password in; better a sanctioned tool than a lie | pretending it can't happen; encrypting everything |
| S11 | `lw add` never calls a model | capture must be instant and work on a plane | classify on capture |
| S12 | Log is committed by `lw build`, not by `lw add` | D2: fewer, more meaningful commits; `fsync` makes the entry durable regardless of commit state. See "backup latency" below | commit every capture; time-based batching |
| S13 | No retro-dating; `ts` is only when you wrote it | D3: "last Tuesday I saw Ada" is prose, and the pipeline resolves it relative to `ts`. One less field, one less thing to teach | optional `at` field / `--at` flag |
| S14 | To-dos get no special treatment | D5: they're events like any other; the wiki grows a to-do page because the pipeline decides to, not because the log knows what a to-do is | `done` relation kind; deterministic to-do projection |
| S15 | One `lw add` invocation is always exactly one event | D6: blank lines in `$EDITOR`/stdin input are just content. Predictable | splitting input on blank lines |
| S16 | Binary is `lw` | D8 | `lifewiki` with an alias |

## Deferred

| # | Topic | Note |
|---|-------|------|
| D7 | Model provider | Deferred entirely; you have a crate in mind. Phase 1 touches no model, so this blocks nothing. `config.toml` gains a `[model]` table when we get there |

## Consequences worth remembering

### Collapsing relations to a single `revises` (S6)
The record loses a level of nesting. Instead of:

```json
"rel": {"kind": "correct", "target": "01JZ8..."}
```

just:

```json
"revises": "01JZ8..."
```

`revises` means only "this event supersedes that one; read my text to find out
how." Withdrawal is `lw revise <id> "forget this, it never happened"` — no
separate `retract`. This goes slightly further than D4 asked (you dropped
`correct`/`update` but didn't mention `retract`), on the grounds that the same
argument kills it: the pipeline was always going to read the text anyway.

The cost: the pipeline can no longer branch on relation type without reading
prose, so "should the wiki keep a history of the old value?" becomes a model
judgement rather than a user declaration. Given that you expect `revise` to be
rare, that seems like the right place to spend the uncertainty. If the pipeline
turns out to systematically get "erase" vs. "keep history" wrong, the fix is to
reintroduce `kind` as an *optional* field — additive, so no migration.

### Backup latency (S12)
You version the log primarily for backup, but the log is only committed at
`lw build`. So events captured since the last build exist in exactly one place:
the working tree. `fsync` protects them against crashes, not against losing the
machine.

This is a real gap between the stated goal and the mechanism, so:

- `lw status` reports uncommitted event count and the age of the oldest one, and
  warns above a threshold (default 24h, configurable).
- `lw sync` = commit any pending log events + push, without running a build.
  Cheap, no model, safe to put in a cron job or shell hook.

That keeps `lw add` instant while making "back everything up now" a one-word
command that doesn't require a build.
