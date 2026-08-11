# Phase 1 — the event log

The log is the only part of lifewiki that holds original data. Everything else is
recoverable from it. So the design goals here are, in order: **durability**,
**verbatim fidelity**, **inspectability by hand**, and **speed of capture**.
Cleverness is not on the list.

## Format: monthly JSONL

One JSON object per line, one file per month, under `log/`:

```
log/2026-08.jsonl
```

```json
{"id":"01K2C4V8YQ3XKQ2S9F1N7B0M4E","ts":"2026-08-11T12:47:03-04:00","text":"Started running again, 3km around the park."}
{"id":"01K2C57TDA6P1YQ8W3ZR4H2J9C","ts":"2026-08-11T13:02:44-04:00","text":"Ada's birthday is the 14th of March, not the 4th."}
```

Why JSONL:

- **Appending is one `write(2)`.** No parsing, rewriting, or locking of prior
  content. The failure mode of a crash mid-append is a torn final line, which is
  detectable and recoverable.
- **Readable and greppable.** `tail`, `grep`, `jq` all just work.
- **Line-oriented diffs.** New events show up in git as pure additions.
- **Text stays verbatim.** JSON string escaping handles newlines, emoji, quotes
  and CJK without us inventing an escape scheme (the trap with a bespoke
  delimited format).

Why monthly files rather than one big file or one file per event: a single file
grows unbounded and makes any future compaction awkward; a file per event means
tens of thousands of inodes and slow directory scans. Monthly gives us
human-sized files (a heavy user writes maybe 30 entries/day ≈ 900 lines/month ≈
100 KB), natural bounds for "read the last N events", and a filename you can
guess. Rollover is by the local date of the event's timestamp.

Files are written **without** a trailing incomplete line: every line ends `\n`.

## The event record

Exactly four possible top-level keys, written in this fixed order so diffs stay
stable:

Exactly three keys, no optional fields, written in this fixed order so diffs stay
stable:

| key    | type   | meaning |
|--------|--------|---------|
| `id`   | string | ULID, uppercase Crockford base32, 26 chars |
| `ts`   | string | RFC 3339 with local UTC offset |
| `text` | string | exactly what the user typed, unmodified |

That's it. No tags, no categories, no title, no type field, and no relations.
Structure is the pipeline's job — anything we ask the user to fill in is friction
at the moment we most need there to be none, and a decision made with less
context than the model will have later.

### `id` — ULID

26 characters, lexicographically sortable, timestamp-prefixed, generated
offline. Sortable IDs mean "events after X" is a string comparison and the log's
natural file order is chronological, with no counter to keep or collisions to
worry about. Uppercase Crockford base32 is case-insensitive and unambiguous when
read aloud or retyped.

With `lw revise` gone, ids are rarely typed by a human — they're mostly for the
pipeline to cite provenance ("this wiki line came from event 01K2C5…") and for
`lw show`/`lw redact`. Commands still accept an unambiguous **prefix** (like git),
minimum 4 characters, since the times you *do* type one you shouldn't have to
type 26 characters.

### `ts` — local time, with offset

`2026-08-11T12:47:03-04:00`, second precision (the ULID carries milliseconds if
we ever need them).

Storing the local offset rather than UTC is deliberate: for a life log, "I wrote
this at 2am" and "I wrote this while in Tokyo" are content, not presentation.
Discarding the offset destroys information we can't get back. It's still a total
order, since the offset is present.

`ts` is **when the event was recorded**, not when the thing happened. Retro-dated
entries ("last Tuesday I…") are left as natural language for the pipeline to
resolve; the log doesn't try to parse dates.

### `text` — verbatim

Stored byte-for-byte as received, with two exceptions: trailing whitespace on the
whole entry is trimmed, and an entry that is empty or whitespace-only is
rejected. Internal newlines are preserved (multi-line entries are normal, e.g.
from `$EDITOR`).

No markup is interpreted. If the user writes `#running` or `@ada` it's just
text — though the pipeline is free to treat it as a hint.

## How facts change: they don't, here

The log has **no relations** and no way to express "this supersedes that." You
just write the new thing in your own words:

```
lw add "oranges are my favorite fruit"
...weeks later...
lw add "I actually prefer apples over oranges now"
```

Both events stay in the log forever, exactly as typed. The *wiki* is where
reconciliation happens: the pipeline sees the new event, finds the page that says
oranges, and rewrites it to say apples. The log is the history; the wiki is the
current truth.

This is the design's central bet, so it's worth stating what it rests on:

- **Prose already carries the intent.** "I actually prefer apples" says it's a
  change, "I've left Acme" says the old fact expired, "forget that, it never
  happened" says withdraw it. A structured field would be asking you to declare
  something your sentence already said — and which the model must read anyway.
- **The pipeline has more context than you do at capture time.** It can see every
  page and every prior event; at 7am typing one line, you can't remember which
  event id said what about fruit. Making you supply a link is asking for the one
  input you're worst positioned to give.
- **Any relation field is optional in practice.** Even with `lw revise`, the
  common case is a plain `lw add` and the pipeline coping anyway. So the pipeline
  needs to handle unlinked revisions *regardless* — which makes the field
  redundant rather than merely inconvenient.

### What this costs

Recorded plainly, because it's a real transfer of risk from the log to the model:

- **Stale facts are now a pipeline bug, not a data property.** If the model fails
  to connect "I prefer apples" to the oranges page, the wiki is silently wrong
  and nothing in the log marks the contradiction. The mitigation is that the log
  is intact, so a better prompt plus `lw rebuild` fixes it retroactively — the
  data is never the thing that was wrong.
- **Erase vs. keep-history is entirely a model judgement.** Whether the wiki
  should show "prefers apples" or "preferred oranges until Aug 2026, now apples"
  is decided by the pipeline reading your prose. Page conventions in the prompt
  will have to set the default (see the pipeline design).
- **Ambiguous references may resolve wrongly.** "I've changed my mind about that"
  with no antecedent is genuinely unresolvable. The pipeline should prefer
  leaving the wiki unchanged and flagging the event over guessing — a
  `wiki/unresolved.md` for events it couldn't place, so failures are visible
  rather than silent.

If this bet fails, the recovery path is additive and cheap: add an **optional**
`revises` field later and teach `lw add --revises <id>`. Old events remain valid,
no migration. Nothing about starting simple forecloses it.

## Writing safely

```
open(path, O_WRONLY|O_APPEND|O_CREAT)   # never O_TRUNC
flock(LOCK_EX)                          # ~/.lifewiki/.lock, guards all writes
write(one buffer, ending in \n)         # single syscall
fsync()                                 # then unlock, close
```

`O_APPEND` plus a single `write` is atomic against other appenders on local
filesystems; the advisory lock additionally serialises us against concurrent `lw`
processes (two shells, or a build running while you capture) and covers
network filesystems where `O_APPEND` is weaker. `fsync` before exit means a
successful `lw add` really is on disk — worth ~5ms for the source of truth.

**Torn lines.** If a crash lands mid-write, the last line of a file may be
invalid JSON. Readers detect this (last line only, not parseable), warn, and
`lw doctor --repair` moves it to `state/quarantine/` so the log parses again.
An invalid line anywhere but the end is a hard error, never silently skipped.

## Reading

A full parse of a year of heavy use is a few MB — fast enough to read from disk
every time, so there is no index and no cache. Newest-first reads (`lw log`) walk
month files in reverse. If this ever becomes slow, the fix is a derived index in
`state/`, rebuildable and deletable, and still not a database.

## Redaction — the one exception

Sometimes you write something you genuinely need gone: a password, someone
else's secret. `lw redact <id>` is the only operation that rewrites history. It
replaces the event's `text` with `null`, adds `"redacted":"<reason>"`, rewrites
the month file atomically (write temp + rename), and rebuilds the wiki. The event
stays as a tombstone so ids and relations stay valid. This is documented as a
deliberate break in the append-only contract rather than pretended away.

Because the log **is** version-controlled (S7), the original text survives in git
history, so this command cannot fully deliver on its promise. It must say so
plainly in its output and point at `git filter-repo`. This is the accepted cost of
choosing backup over erasure; see S7 in [decisions.md](decisions.md).

## Non-goals here

Encryption, signing/hash-chaining, attachments, and sync. Hash-chaining
(`prev` hash per event) is tempting for tamper-evidence, but git already gives us
that for free and the threat model for a local single-user notebook doesn't
justify the complexity.

## Rust implementation notes

Crates, kept deliberately few:

| need | crate |
|------|-------|
| CLI | `clap` (derive) |
| serialisation | `serde`, `serde_json` |
| ids | `ulid` |
| time | `jiff` (or `chrono`) for local offsets |
| locking | `fs2` |
| errors | `anyhow` (+ `thiserror` in the core) |
| git | shelling out to `git`, at least at first |

Module sketch:

```
src/main.rs        clap dispatch
src/paths.rs       LIFEWIKI_HOME resolution, init
src/event.rs       Event { id, ts, text }, serde, id/prefix handling
src/log/mod.rs     Log: append, iterate, iterate_rev, resolve_prefix, since, upto
src/log/writer.rs  locked append + fsync
src/log/reader.rs  month-file discovery, torn-line handling
src/cmd/*.rs       add, log, show, search, status, sync, doctor, redact
```

`Event` is a three-field struct with no `Option`s, which keeps serde trivial and
means there is exactly one valid shape for a log line.

`Log` is the only thing allowed to touch `log/`, and it exposes no mutating
operation other than `append`. Shelling out to `git` avoids `libgit2` build
friction and makes what we do trivially inspectable; swappable later behind a
`Vcs` trait if it becomes a bottleneck.

`since(id)` and `upto(id)` are both string comparisons on sorted ULIDs. They exist
in phase 1 even though nothing consumes them yet, because the pipeline needs to
replay bounded ranges of history to be reproducible while the log keeps growing
(see "Designing for pipeline iteration" in [design.md](design.md)).

`Log::open` takes the log directory as a parameter rather than reading a global —
so tests, and later `lw rebuild --into`, can point at any directory.

### Phase 1 acceptance

- `lw init` creates the home dir and the git repo.
- `lw add "text"`, piped stdin, and `$EDITOR` with no args all produce one line.
- Multi-line, emoji, quotes and 100 KB entries all round-trip byte-identically.
- `lw log`, `lw show`, `lw search` read it back; ids resolve by prefix, and an
  ambiguous prefix is an error listing the candidates.
- `lw sync` commits pending events and pushes; `lw status` warns when events have
  been uncommitted for over 24h.
- Month rollover works; a hand-truncated file is detected, not crashed on.
- `since(id)` / `upto(id)` return the right half-open ranges, including at month
  boundaries and for ids not present in the log.
- A hand-written test corpus exists in `tests/corpora/` containing contradicting
  facts, vague references, and out-of-order narration, ready for phase 2.
- 10k appends stay well under a second each and the log parses in well under a
  second.
