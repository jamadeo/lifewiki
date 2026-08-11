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
{"id":"01K2C57TDA6P1YQ8W3ZR4H2J9C","ts":"2026-08-11T13:02:44-04:00","text":"Ada's birthday is the 14th of March, not the 4th.","revises":"01JZ8QF2M6TB0X4WKD7YV3N1RS"}
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

| key       | type   | required | meaning |
|-----------|--------|----------|---------|
| `id`      | string | yes      | ULID, uppercase Crockford base32, 26 chars |
| `ts`      | string | yes      | RFC 3339 with local UTC offset |
| `text`    | string | yes      | exactly what the user typed, unmodified |
| `revises` | string | no       | id of an earlier event this one supersedes |

That's it. No tags, no categories, no title, no type field. Structure is the
pipeline's job — anything we ask the user to fill in is friction at the moment we
most need there to be none, and a decision made with less context than the model
will have later.

### `id` — ULID

26 characters, lexicographically sortable, timestamp-prefixed, generated
offline. Sortable IDs mean "events after X" is a string comparison and the log's
natural file order is chronological, with no counter to keep or collisions to
worry about. Uppercase Crockford base32 is case-insensitive and unambiguous when
read aloud or retyped, which matters because the user will be typing these into
`lw revise <id>`.

Commands accept an unambiguous **prefix** of an id (like git), minimum 4
characters.

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

### `revises` — how facts change

The log is immutable, so a change is a new event pointing at an old one. There is
exactly **one** relation, and it carries no type:

```json
{"id":"01K2C5...","ts":"...","text":"I've left Acme.","revises":"01JZ8Q..."}
```

`revises` means *"this event supersedes that one; read my text to find out how."*
Corrections, updates and withdrawals are all the same shape:

| you write | effect |
|-----------|--------|
| `lw revise 01JZ8Q "Ada's birthday is March 14th, not the 4th"` | correction — the old value was never true |
| `lw revise 01JZ8Q "I've left Acme"` | update — the old value was true then |
| `lw revise 01JZ8Q "forget this, it never happened"` | withdrawal |

**Why no `kind` field.** An earlier draft had `correct`/`update`/`retract`, on the
theory that the wiki should erase a corrected value but keep history for an
updated one, and only the user can tell those apart. But the user's prose already
says which it is, and the pipeline reads the prose regardless — so the field was
asking for a declaration the model was going to re-derive anyway. One relation,
no enum, no nesting.

The cost, recorded honestly: "erase the old value" vs. "keep it as history" is
now a model judgement rather than a user declaration. Acceptable because revision
is expected to be rare. If the pipeline gets it systematically wrong, we
reintroduce `kind` as an *optional* field — purely additive, no migration.

Rules: the target must exist and must be an earlier id. Revisions may chain (C
revises B revises A) and the pipeline resolves to the newest live version. A
non-chaining "reply/annotation" relation is deliberately omitted — plain new
events are usually the right answer.

Note that revision is a *hint to the pipeline*, not a requirement. Writing "oh,
Ada's birthday is actually March 14th" as a plain event is perfectly valid and
the pipeline should cope; `revises` just makes the link explicit and unambiguous
so it doesn't have to guess which earlier fact you meant.

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
src/event.rs       Event, serde, id/prefix handling
src/log/mod.rs     Log: append, iterate, iterate_rev, resolve_prefix, since(id)
src/log/writer.rs  locked append + fsync
src/log/reader.rs  month-file discovery, torn-line handling
src/cmd/*.rs       add, log, show, search, revise, undo, status, sync
```

`Log` is the only thing allowed to touch `log/`, and it exposes no mutating
operation other than `append`. Shelling out to `git` avoids `libgit2` build
friction and makes what we do trivially inspectable; swappable later behind a
`Vcs` trait if it becomes a bottleneck.

### Phase 1 acceptance

- `lw init` creates the home dir and the git repo.
- `lw add "text"`, piped stdin, and `$EDITOR` with no args all produce one line.
- Multi-line, emoji, quotes and 100 KB entries all round-trip byte-identically.
- `lw log`, `lw show`, `lw search` read it back; ids resolve by prefix.
- `lw revise <id> "..."` writes a valid `revises`; a nonexistent, ambiguous, or
  not-earlier target is rejected with a useful message.
- `lw show <id>` lists events that revise it, and the chain it revises.
- `lw sync` commits pending events and pushes; `lw status` warns when events have
  been uncommitted for over 24h.
- Month rollover works; a hand-truncated file is detected, not crashed on.
- 10k appends stay well under a second each and the log parses in well under a
  second.
