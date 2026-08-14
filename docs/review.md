# Review: where the project got bigger than the idea

5,267 lines of Rust for "record notes, fold them into a wiki, ask questions."
That is too much, and this is an honest accounting of where it went.

The measurements first, because intuition about bloat is usually wrong:

| file | code | tests | verdict |
|------|-----:|------:|---------|
| `app.rs` | 759 | 64 | too big; doing four jobs |
| `reconcile.rs` | 623 | 143 | ~90% shared with `ask.rs` |
| `ask.rs` | 468 | 105 | ~90% shared with `reconcile.rs` |
| `model.rs` | 409 | 208 | config resolution is over-built |
| `build.rs` | 401 | 338 | earns its keep |
| `tools.rs` | 352 | 129 | earns its keep |
| `log.rs` | 224 | 0 | earns its keep |
| `okf.rs` | 221 | 85 | earns its keep |
| `progress.rs` | 192 | 40 | over-built |
| `index.rs` | 139 | 194 | borderline |
| `event.rs` | 81 | 0 | earns its keep |
| `git.rs` | 51 | 0 | earns its keep |

The core — log, event, tools, okf, build, git — is about 1,330 lines and is
roughly the right size. The bloat is concentrated in four places.

## The one real duplication: two agent loops

`ask.rs` and `reconcile.rs` share **12 identical top-level items**: `Session`,
`Runtime`, `Record`, `MaxTurnsOperation`, `EndTurnOperation`,
`ToolExecutionOperation`, `InferenceRunner`, and all four trait impls. Only three
things actually differ:

1. which tools are offered (`definitions()` vs `ask_definitions()`)
2. the system prompt
3. the terminal tool (`finish` vs `answer`) and what gets extracted from it

I wrote `ask.rs` by copying `reconcile.rs` and changing those three things. That
was the fastest way to get it working and the wrong thing to leave behind.

**Cut: one `agent.rs` parameterised by those three differences.** Roughly
1,090 lines of agent code becomes ~450 plus two ~40-line configs. Saves ~550
lines and, more importantly, means a fix to the loop happens once. The
`EndTurnOperation` subtlety — that `StateMachine::step` ignores
`Inference::applies` — is currently implemented twice and could drift.

## Over-built: progress reporting

192 lines with three verbosity levels, ANSI colour, `NO_COLOR` support, a mutex
for line atomicity, and per-tool argument summarisation.

The requirement was "print output for tool calls." Three levels is one more than
anyone needs: `Normal` and `Verbose` differ only in truncation, and `Quiet`
exists mostly because I added it reflexively. The mutex guards against
interleaving that cannot happen — the agent loop is strictly sequential.

**Cut to ~70 lines:** one `verbose: bool`, keep colour (it genuinely helps scan
a long run), drop the mutex, drop `Quiet` or keep it as a one-liner. Saves ~120
lines.

## Over-built: per-task model config

`ModelTable` with `Option` on every field, recursive `Box<ModelTable>`
sub-tables, inheritance rules, and 208 lines of tests for a config file.

The feature is right — building and answering genuinely want different models.
The implementation is heavier than the problem. The inheritance rule that a
`name` only carries across a matching provider is *correct* and subtle, and it
exists because inheritance exists. Without inheritance, the rule vanishes.

**Simplify:** two independent tables, no inheritance, each defaulting
independently.

```toml
[build]
provider = "anthropic"
name = "claude-opus-5"

[ask]
provider = "local"
```

Slight duplication when both are the same, and that is fine — it is two lines. It
deletes the inheritance logic, the provider-matching rule, and most of the tests.
Saves ~150 lines including tests. **Cost: breaks existing `config.toml`.** With
one user, a note in the changelog covers it.

## Three directory walkers

`app.rs:walk_markdown`, `build.rs:walkdir`, `index.rs:markdown_files` — all
recursive, all returning sorted `.md` paths, differing only in whether they skip
dotfiles. Two of the three skip dotfiles; the third doesn't, which is a latent
bug waiting for someone to open Obsidian at the wrong moment.

**Cut: one function in `okf.rs` or a small `fs.rs`.** Saves ~50 lines and
removes an inconsistency.

## `app.rs` is doing four jobs

759 lines holding: the clap definition (~230), command dispatch, seven command
implementations, and shared helpers. `ask()` alone is 75 lines of resolution and
error mapping that belongs next to `Asker`.

**Cut: move each command's body to its own module,** leaving `app.rs` as the CLI
definition and dispatch. Doesn't delete lines but stops one file being where
everything lands by default.

## Features worth questioning, not just code

These are judgement calls for you rather than mechanical cuts.

**`lw sync` (~20 lines).** Exists because commits happen at build time, so
entries between builds live only in the working tree. But it duplicates
`git add log && git commit && git push`, and its whole purpose is compensating
for a decision we made (S28: commit only on build). **Alternative:** commit on
every `lw add`. That deletes `sync`, deletes the "uncommitted events" logic in
`status`, and makes the backup story trivially correct. The cost is noisier
history — but the log is append-only, so nobody reads its history anyway (we
established that when deciding to version it at all). I think **this is the
best single simplification available** and I argued the wrong way originally.

**`--into` and `--to` (~40 lines).** Justified by prompt iteration (S17). I have
never used them, including while iterating on the prompt — I used
`rebuild --force` and read the diff. They are cheap, but they are speculative.
**Suggest:** keep `--into`, drop `--to`. `--to` needs a ULID typed by hand and
solves a problem (log growing mid-comparison) that takes seconds to occur.

**`--dry-run` (~15 lines).** Writes pages, skips the commit. `--into /tmp/x` does
the same thing more clearly. **Suggest: drop it.**

**`lw doctor --repair` (~30 lines).** Quarantines torn lines. A torn line
requires a crash during a single `write(2)` of a few hundred bytes. It has never
happened. **Suggest:** keep the check, drop `--repair` — reporting is the
valuable half and repair is the risky half, on the one file that must not be
damaged.

**`lw redact` (~20 lines plus complexity in `Event`).** It is why `Event.text` is
`Option<String>`, which every reader must handle. It genuinely earned its place
once — the bug where a redacted log became unreadable proves the code path is
real. But it cannot do what its name promises, since git keeps the text.
**Suggest: keep.** It is the only tool for a real problem, and the `Option` is
four match arms.

**`state/runs/` transcripts.** Gitignored, 88KB so far, and I have read them
zero times — the live progress output made them redundant. **Suggest:** keep
writing them only on failure. Saves nothing in code but stops accumulating
files nobody reads.

**`lw status` model reporting.** Three lines that prevent a whole class of silent
config error. Keep.

## What should not be cut

- **The log format and its guarantees.** `event.rs` + `log.rs` are 305 lines with
  locked appends, torn-line detection, and prefix resolution. This is the file
  that cannot be regenerated.
- **`okf.rs` validation.** 221 lines standing between a confused model and a
  corrupt wiki.
- **`build.rs`'s transactional properties.** 401 lines of code to 338 of tests is
  the right ratio for something that must be atomic and crash-safe.
- **`tools.rs` path handling.** The traversal and symlink checks are
  security-critical.

## Recommended order

| # | change | saves | risk |
|---|--------|------:|------|
| 1 | Merge the two agent loops into `agent.rs` | ~550 | low, well tested |
| 2 | Commit on `add`; delete `sync` and pending-tracking | ~60 | low |
| 3 | Flatten model config to `[build]` / `[ask]` | ~150 | breaks config |
| 4 | Trim `progress.rs` to one bool | ~120 | low |
| 5 | Drop `--to`, `--dry-run`, `--repair` | ~85 | low |
| 6 | One directory walker | ~50 | low, fixes a bug |
| 7 | Split `app.rs` per command | 0 | low |

Total: **~1,000 lines removed**, about 19% of the project, and the CLI drops from
12 commands and 20 flags to 11 and 15.

## The honest summary

Roughly 30% of what I built was not asked for. The pattern is consistent: each
addition was locally defensible and I did not weigh it against the whole. The
worst offenders were **copying `reconcile.rs` to make `ask.rs`** (a shortcut that
should not have survived the commit), **three verbosity levels** where one bool
was requested, and **`lw sync`** — a command that exists only to compensate for
an earlier decision, which is the clearest sign a design has gone slightly wrong.
