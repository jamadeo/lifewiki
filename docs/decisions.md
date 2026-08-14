# Decision record

All phase-1 decisions are settled. Newer decisions are appended as they're made.

## Settled

| # | Decision | Rationale | Alternative rejected |
|---|----------|-----------|----------------------|
| S1 | Log format is JSONL, one object per line | atomic single-write appends, greppable, verbatim text via JSON escaping, clean git diffs | bespoke delimited text (escaping traps), TOML/YAML per entry (not append-friendly), sqlite (nothing needs it yet) |
| S2 | One file per month under `log/` | human-sized files, guessable names, bounded reverse reads | one big file (unbounded), one file per event (inode churn) |
| S3 | ULID ids, prefix-matchable like git | sortable, offline, no counter, safe to retype | UUIDv4 (unsortable), sequence numbers (needs shared state), content hashes (not sortable, dupes collide) |
| S4 | Timestamps are RFC 3339 with local offset | "2am" and "in Tokyo" are content, not presentation; still totally ordered | UTC only (destroys information), epoch ints (unreadable) |
| S5 | Event is exactly `id`, `ts`, `text` — no optional fields at all | no capture-time friction; the model has more context later than the user does now; one valid shape for a log line | required type/tags/category fields |
| S6 | **No relations in the log.** Revision is expressed as an ordinary new event; the pipeline reconciles it against the wiki | D4 taken to its conclusion: if prose can describe a revision, a link field is redundant. The pipeline had to handle unlinked revisions anyway, so the field bought nothing it didn't already need | `correct`/`update`/`retract` kinds (v1); a single bare `revises` id (v2); both superseded |
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
| ~~D7~~ | Model provider | **Resolved**: `goose-agent` + `goose-providers` `0.1.0-alpha.6`. See S22 |

| # | Decision | Rationale |
|---|----------|-----------|
| S27 | `lw build` batches **all** events since the cursor, uncapped (resolves D12) | events written in one sitting usually concern one topic, and the agent reconciles them better together than in isolation. A cap would split related events across runs arbitrarily. Revisit if a long gap produces an unmanageable batch |
| S28 | One commit covers `wiki/`, `state/cursor.json` and `log/` together | the repo must never record a wiki claiming to reflect events it hasn't seen. Also means `git revert` of a build undoes it completely, and folded events get committed (and so backed up) as a side effect |
| S29 | The cursor advances only after a successful agent run | a failed or interrupted build leaves a dirty tree that `git checkout -- wiki/` discards, and the next build retries the same events. Crash-safety without a journal |
| S30 | `lw build` refuses to run on a dirty `wiki/` | otherwise a build would mix hand-edits into its own commit, or destroy them. Forces the D11 question to be answered deliberately rather than by accident |
| S31 | Run logs are written even when the agent fails | a failed run is exactly when the transcript is most useful |
| S48 | `build` and `ask` are configured separately, via `[model.build]` / `[model.ask]` sub-tables that override `[model]` | the tasks differ: reconciliation rewrites files over a long run and rewards a stronger model, while answering is short, frequent and read-only. Inheritance keeps the common case one table | separate top-level tables (duplicated settings); a flag only (not persistent) |
| S49 | A `name` from the base table is inherited **only** when the task uses the same provider | a model name means something only to the provider it was written for. Otherwise `[model] name = "claude-opus-5"` plus `[model.ask] provider = "local"` would ask the local provider for claude-opus-5 — the S47 bug, reachable through config instead of a flag |
| S50 | `[model]` rejects unknown keys, and `lw status` prints the resolved model per task | a mistyped sub-table (`[model.asks]`) would otherwise silently do nothing, which is the worst failure mode for configuration. `deny_unknown_fields` catches the typo; `status` makes the result visible without spending a model call |
| S41 | `wiki/index.md` is generated deterministically after each build, not by the model | it is a mechanical listing of titles and descriptions that already exist in frontmatter, and OKF explicitly permits producers to generate it (SPEC §8). A model call would cost money and introduce drift. Also fixes `rebuild` deleting it |
| S42 | Dangling internal links are **reported**, not rejected | the agent legitimately links to a page it is about to create, so a per-write check would reject correct work. Checked across the whole wiki after a run, and by `lw doctor` |
| S43 | `lw doctor` validates the wiki as well as the log | pages are validated when written, but a hand-edit or a deleted page breaks them afterwards. The same OKF gate is reused |
| S44 | `lw ask` is read-only twice over: the workspace is constructed non-writable **and** the write tools are not offered | a question must never change the wiki. One mechanism could be defeated by a bug; two independent ones is cheap |
| S45 | `ask` gets a `read_event` tool that the build agent does not | when answering, the owner's exact words are often the real answer, and pages only paraphrase. During a build the events are supplied directly, so log browsing would only invite folding facts nobody asked about |
| S46 | `ask`'s prompt optimises for honesty over helpfulness | this is someone's memory of their own life, so a confident wrong answer is far worse than "the wiki does not say". It is told not to infer from adjacent facts, not to add outside knowledge, and to report ambiguity rather than resolve it |
| S47 | `--local` always clears the configured model name | a name is meaningful only to the provider it was written for. Carrying it over meant `--local` with `name = "claude-opus-5"` asked the local provider for a model by that name |
| S38 | `lw init` writes a `.gitignore` covering `.lock`, `state/runs/`, `.obsidian/`, `.DS_Store` | without it the tree is permanently dirty, which blocks every build via S30. Run logs in particular are regenerated each run and would bloat history for no benefit — the wiki commit already records what changed |
| S39 | A failed `rebuild` restores `wiki/` from the last commit | rebuild clears pages *before* the agent runs, so any failure left an empty wiki. Recoverable from git, but only if the user knew to look; the tool that broke it should fix it |
| S40 | The dirty-wiki error lists the offending files and gives both the keep and discard commands | "uncommitted changes" with no list forces the user to run `git status` themselves. It also detects `.obsidian/` and suggests the gitignore fix, since an editor is the likeliest cause |
| S36 | Footnote labels are `e` + the **last** 10 chars of the event id, and duplicate source ids are a validation error | ULIDs begin with a millisecond timestamp, so events from one sitting share a prefix. Labels taken from the front collided, silently merging distinct citations — provenance looked right and was wrong. Found by inspecting a real build's output, not by a test | first 8 chars; a per-page counter (unstable across rebuilds) |
| S37 | The agent links first mentions of entities that have their own page | makes the wiki navigable in Obsidian and any graph viewer, which was otherwise sparse because pages only cited log events. Also helps the agent's own retrieval on later runs: reading Ada's page surfaces the trip page | leaving pages unlinked; Obsidian `[[wikilinks]]`, which are not portable |
| S35 | `lw build` prints every tool call and result live, to **stderr**, on by default (`-q` to silence, `-v` for prose and full output) | a build makes many slow, opaque model calls and edits files the user cares about; silence then a summary is the wrong default. stderr keeps stdout clean for the final report, so piping still works. Tool errors always print in full, since watching the agent self-correct is the most useful signal when a build misbehaves | quiet by default with a `--verbose` opt-in; a spinner; printing to stdout |
| S33 | Local inference via `goose-local-inference`, behind a non-default cargo feature (`local`, plus `metal`/`cuda`/`vulkan`) | the input is the user's whole life, so a fully offline option is worth real effort. Gated because it compiles llama.cpp from source and needs `cmake`: contributors working on the log shouldn't pay that. The `--local` flag, the backend and the dependency are all cfg'd out by default | on by default; a separate binary; runtime-loaded provider |
| S34 | lifewiki does **not** download models; `lw build --local` tells the user to run `goose configure` | model management is goose's job. Owning download progress, cancellation and disk layout here would duplicate it for no benefit. We only rewrite the upstream error, which points at goose's desktop UI that a CLI user doesn't have | implementing `lw model pull` (built, then removed) |
| S32 | The agent loop is `goose_agent::machine::StateMachine`, with operations we implement ourselves | goose's own operations are not public API yet, but the machine is. Using it means the loop's shape (ordered steps, effects, cancellation, session reload per pass) is shared with goose rather than reinvented, and our operations become contributable upstream as they stabilise | a hand-rolled loop (previous implementation); waiting for goose to publish its operations |

## Open (phase 2, raised by S21)

### D10. How many `sources` does a page keep?
OKF assumes a handful of sources per concept. Ours accrete indefinitely — a page
about a person could reach hundreds of event citations over years, which bloats
frontmatter past usefulness. Options: (a) cite only events that support
*currently stated* claims, dropping citations when a claim is rewritten (keeps
frontmatter proportional to content; loses the audit trail, though git and the log
both still have it); (b) keep everything and accept bloat; (c) cap at N most
recent. I lean **(a)** — the page cites what it currently says, and history lives
in git and the log where it belongs.

### D11. Does `verified` make pages partly hand-editable?
OKF's trust tiers give us a machine-readable "a human confirmed this" marker,
which suggests letting you edit a page directly and mark it `verified: human:jack`
so the pipeline treats it as more authoritative and won't casually overwrite it.

Tempting, but it puts a crack in "the wiki is purely derived": a hand-edit is
content that exists nowhere in the log, so `lw rebuild` destroys it. Options:
(a) wiki stays strictly derived, `verified` only ever set by a `lw review`
command that records approval without changing text; (b) hand-edits allowed, and
`lw rebuild` preserves human-verified sections; (c) hand-edits allowed, and the
diff of your edit is appended to the log as an event so it survives rebuilds.

(c) is the only one that keeps the log as complete truth, and it's appealing — but
it means the log gains events the user didn't type, which cuts against "verbatim
what you said." Needs a decision before `verified` is written by anything.

## Consequences worth remembering

### Removing relations entirely (S6)
The log went through three shapes before landing: `{kind, target}`, then a bare
`revises` id, then nothing. The final step was the right one — once prose is
accepted as sufficient to *describe* a revision, a field pointing at the revised
event is redundant, because the pipeline must cope with unlinked revisions anyway
(users will just `lw add`, and should be able to). A field used sometimes, whose
absence must be handled regardless, is pure cost.

An event is now three required fields with no `Option`s, so there is exactly one
valid shape for a log line. `lw revise` and `lw undo` are both gone from the CLI.

**This moves real risk into the pipeline**, and that should be tracked as the
project's main technical risk:

- A missed reconciliation means the wiki *silently* shows a stale fact. Nothing
  in the log marks the contradiction; only the model noticing it does.
- "Erase the old value" vs. "keep a dated history" is decided entirely by the
  model reading prose. Page conventions in the prompt have to set the default,
  or the wiki will be inconsistent about it.
- Ambiguous events ("I've changed my mind about that") are genuinely
  unresolvable. The pipeline must prefer flagging to guessing —
  `wiki/unresolved.md` exists so failures are visible.

What makes this acceptable: the log is complete and verbatim, so every
reconciliation error is a defect in a *derived* artifact. Improving the prompt and
running `lw rebuild` fixes the past. The data is never what was wrong.

Escape hatch if the bet fails: add an **optional** `revises` field and
`lw add --revises <id>`. Purely additive, no migration, no old event invalidated.

### Phase 1 gets simpler, phase 2 gets harder
Phase 1 is now a very small program: append lines, read lines back, commit, push.
Almost no domain logic. The corollary is that phase 2 can't be the naive version
I first sketched ("append each event to a dated journal page") — that does no
reconciliation at all, so the wiki would accumulate contradictions and teach us
nothing about the hard part. The first milestone for phase 2 is
retrieve-then-rewrite: find topically related pages, hand them to the model with
the new event, rewrite in place.

| # | Decision | Rationale |
|---|----------|-----------|
| S17 | `lw rebuild` takes `--into <dir>` and `--to <id>` | the pipeline is the part we expect to rewrite repeatedly, so materialising the same log under two prompt versions and `diff -r`-ing them must be trivial. Bounded replay keeps runs reproducible as the log grows |
| S18 | `Log::open` takes a directory; nothing reads a global path | makes S17 and tests possible without special cases |
| S19 | A hand-written test corpus lands in phase 1, at `tests/corpora/*.jsonl` | evaluating reconciliation needs deliberately contradictory and vague events; a corpus is just a log file, so this costs nothing. Writing one also pressure-tests the format while it's still cheap to change |
| S20 | Quality is judged by diffing pipeline versions over a fixed corpus, not by a score | what's being judged is whether a page reads correctly to the person whose life it describes — human review of "what changed" is the honest signal |
| S22 | Model access via `goose-providers` 0.1.0-alpha.6, behind our own `Model` trait in `src/model.rs` | resolves D7. One dependency covers Anthropic, OpenAI, Google, Ollama and others, so switching providers (including to fully local) is config, not code. The wrapper trait keeps an alpha-versioned dependency contained to one file and lets the pipeline be tested with a `StubModel` and no API key | calling HTTP APIs directly; committing to a single vendor SDK |
| ~~S23~~ | ~~Reconciliation is a single completion, not an agent~~ | **Superseded by S24.** Recorded because the reasoning was wrong in an instructive way: it assumed reconciliation was a pure function of (event, pages), which presumes we can identify the relevant pages *before* the model runs. Deciding what is relevant is itself the hard part | |
| S24 | Reconciliation is an **agent loop with filesystem tools** over the wiki: `list_pages`, `search_wiki`, `read_page`, `write_page`, `delete_page`, `finish` | retrieval and reconciliation are not separable. Whether a page is relevant is often only knowable after reading it, and one read suggests the next ("Ada" → her page mentions a trip → the trip page also needs updating). A fixed retrieve-then-rewrite pipeline has to guess its retrieval right in one shot; an agent can look, and look again | fixed retrieve-then-rewrite; whole-wiki context stuffing |
| S25 | The loop is bounded (default 24 turns), every write is OKF-validated before it touches disk, and tool errors are returned to the model rather than raised | an agent with write access that can loop indefinitely burns money and corrupts pages. Validation-before-write turns model sloppiness into a retryable tool error instead of a broken page | unbounded loop; validating after the fact |
| S26 | Tools are scoped to `wiki/`; the log is not reachable through them | the agent is handed the events it must fold. Letting it browse the log invites rediscovering facts we did not ask it to reconcile, and there is no reason it should ever write there | exposing the log read-only as a tool |
| S21 | Wiki pages follow a **subset of OKF v0.2**: `type`/`title`/`description`/`tags`, `generated`, `verified`, `sources` + footnote attribution, `index.md`. Skip attested computations, `log.md`, `stale_after` | the substrate (markdown + YAML frontmatter in git) is what we'd already chosen, and OKF standardises the provenance/trust conventions we'd left as TBD. Full assessment in [wiki-format.md](wiki-format.md) | inventing our own frontmatter conventions; adopting OKF wholesale; plain markdown with no frontmatter |

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
