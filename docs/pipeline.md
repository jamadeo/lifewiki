# Phase 2 — the reconciliation pipeline

`lw build` folds new log events into the wiki. This is the hard part of the
project: phase 1 deliberately records raw intent and defers *all* interpretation
to here.

## The problem, stated precisely

Given an event like `"I actually prefer apples over oranges now"` and a wiki that
contains a page saying oranges are the favourite, produce a wiki that says apples
— without being told which page to look at, which claim is contradicted, or that
a contradiction exists at all.

Decomposed, that is five problems. Each is genuinely open; none is solved by our
format choices so far.

### P1+P2. Retrieval and reconciliation are one problem (S24)

The original plan separated these: retrieve candidate pages, then ask the model
to rewrite them. That was wrong, and the reason is worth keeping.

Retrieval has to decide what is relevant *before* the model has read anything.
But relevance is often only knowable after reading: a page titled
`preferences.md` may or may not mention fruit, and the only way to find out is to
open it. Worse, one read suggests the next — an event about Ada leads to her
page, which mentions a trip, whose page also needs updating. A one-shot retriever
must guess all of that up front; the failure is silent, and the wiki keeps
asserting a stale fact.

So reconciliation is an **agent loop**. The agent gets tools and decides how to
use them:

| tool | purpose |
|------|---------|
| `list_pages` | see what exists |
| `search_wiki` | case-insensitive literal search; the main way to find contradictions |
| `read_page` | read a page in full, including frontmatter |
| `write_page` | create or overwrite; validated before it touches disk |
| `delete_page` | rare; prefer rewriting |
| `finish` | done, with a summary |

The system prompt does the work that structure can't: it states that
reconciliation is the point, that a stale fact left behind is the worst outcome,
that searching several times and finding nothing is cheap, and that the
erase-vs-keep-history decision follows from the page `type`.

Safety properties (S25, S26):

- **Bounded**: default 24 turns, then the run fails loudly. An agent with write
  access that can loop forever burns money and corrupts pages.
- **Validated**: `write_page` runs OKF conformance first, so a mangled document
  becomes a tool error the agent can fix on the next turn, never a broken file.
- **Scoped**: tools resolve paths against `wiki/` and reject traversal. The log
  is not reachable at all — the agent is handed the events it must fold.
- **Errors are returned, not raised**: a bad path or invalid page comes back as
  tool output, because the agent can correct those itself.

Note what this does *not* solve: the agent can still fail to search for the right
thing. Bounding turns and validating writes protects the wiki's integrity, not
its accuracy. Accuracy is a prompt-quality problem, measured by S20's diffs.

### P3. Application — how do edits reach disk safely?
The model returns **whole-file content** per page (simpler and more reliable than
diffs at this scale). Requirements:

- Validate before writing: parseable frontmatter, non-empty `type` (OKF §11), and
  that footnote labels resolve to `sources` ids. A model that mangles YAML must
  produce a *failed run*, not a corrupt page.
- Write all pages for a batch, or none. A partial fold is worse than no fold,
  because the cursor can't describe it.
- Never touch `log/`.

### P4. Bookkeeping — what has been folded?
`state/cursor.json` holds the last folded event id. Required properties:

- **Idempotent**: no new events ⇒ no work, no commit.
- **Crash-safe**: interrupted run leaves a dirty tree; recovery is
  `git checkout -- wiki/` and retry, since the cursor advances only on success.
- **Atomic with the wiki**: cursor and wiki move in the same commit, so
  `git revert` of a build is complete and correct.

### P5. Observability — why did the wiki change?
Every run writes `state/runs/<timestamp>/` containing the batch, retrieved
candidates, prompts, raw model output, and the resulting file list. This is what
makes a weird edit explainable a month later, and what makes S20 (judging quality
by diffing pipeline versions) possible.

## Failure must be loud

The dangerous outcome is not a crash; it's a wiki that confidently states a stale
fact. So:

- Events the pipeline can't place go to `wiki/unresolved.md` with their id, and
  `lw status` reports the count. Better a visible backlog than a silent wrong
  answer.
- Retrieval returning nothing for a non-trivial event is suspicious enough to
  flag, not to treat as "nothing to do".
- A failed page validation aborts the batch and reports which page and why.

## Model access

Provided by `goose-providers` 0.1.0-alpha.6 (S22), wrapped in a local `Model`
trait (`src/model.rs`):

```rust
pub trait Model: Send + Sync {
    fn actor(&self) -> String;              // OKF generated.by
    fn complete(&self, request: Request) -> Result<Response>;
}
```

The interface is deliberately small — a system prompt, one user turn, text back.
No tools and no multi-turn state, because reconciliation is a pure function of
(event, candidate pages). Three reasons for the wrapper:

1. **Testability.** `StubModel` lets every deterministic part of the pipeline be
   exercised with no API key and no network.
2. **Containment.** The dependency is `0.1.0-alpha`; churn hits one file.
3. **Provider freedom.** Anthropic, OpenAI, Google and Ollama are all reachable
   through the same crate, so "run this entirely locally" — which matters when the
   input is your whole life — is a config change, not a rewrite.

The loop itself is `goose_agent::machine::StateMachine` (S32). Its operations are
not yet public API, so we implement our own in `src/reconcile.rs`:

| step | role |
|------|------|
| `MaxTurnsOperation` | stop before an agent with write access runs away |
| `FinishOperation` | the model called `finish`; record the summary and yield |
| `EndTurnOperation` | the model replied with prose and no tool calls; stop |
| `ToolExecutionOperation` | run the requested wiki tools, feed results back |
| `InferenceRunner` | otherwise, ask the model what to do next |

The machine tries each step in order per iteration; the first that applies
produces effects and the loop repeats. `SessionLoader` reloads the conversation
at the top of each pass and `EffectHandler` applies the effects at the end, so
the transcript lives in one place rather than being threaded through the loop.

One sharp edge worth recording: `StateMachine::step` always reaches the
inference step — it does not consult `Inference::applies` — so a plain-prose
reply with no tool calls will loop forever unless an operation before inference
terminates the turn. Hence `EndTurnOperation`.

Configuration lives in `config.toml`:

```toml
[model]
provider = "anthropic"        # anthropic | local | none
name = "claude-sonnet-4-5"
timeout_seconds = 120
```

The Anthropic path reads `ANTHROPIC_API_KEY` from the environment. It is covered
by an `#[ignore]`d integration test (`cargo test -- --ignored`) rather than a
mock, because the thing worth testing is that the real request shape is accepted.

### Local inference (S33)

`provider = "local"`, or `lw build --local`, runs inference entirely on the
machine via `goose-local-inference` (llama.cpp). This matters more here than for
most tools: the input is the user's entire life, and this is the configuration
where none of it leaves the machine.

It is **behind a cargo feature**, off by default:

```sh
cargo install --path . --features local     # portable CPU build
cargo install --path . --features metal     # Apple silicon
cargo install --path . --features cuda      # NVIDIA
cargo install --path . --features vulkan
```

The gate is not incidental. `goose-local-inference` pulls in `llama-cpp-sys-2`,
which compiles llama.cpp from source and therefore requires `cmake` and minutes
of build time. Making every contributor pay that to work on the event log would
be wrong, so a default build has no `--local` flag at all: the clap argument, the
backend, and the dependency are all `#[cfg(feature = "local")]`. Asking for
`--local` without it gives a clear message naming the feature to rebuild with.

**We do not download models.** `goose-local-inference`'s own error points at
goose's desktop UI, which does not exist for a CLI; we rewrite it to point at
`goose configure`. Model management is goose's job, and duplicating it here would
mean owning download progress, cancellation and disk layout for no benefit.

Two caveats worth knowing before trusting a local build:

- **Tool-calling quality is the constraint.** Reconciliation depends on a model
  reliably emitting well-formed tool calls over many turns. Most of the featured
  GGUF models declare `native_tool_calling: false` and are driven through an
  emulation layer. The default is `unsloth/gemma-4-E4B-it-GGUF:Q4_K_M`, one of
  the few with native support.
- **Provenance records it.** A local run writes `generated.by:
  lifewiki/local-gemma-4-E4B-it-GGUF-Q4_K_M`, so it is always visible which
  pages were written by a small local model rather than a frontier one — which
  matters when reviewing a wiki built by mixed providers.

## Prompt and page conventions live in the repo

Prompts are versioned files (`prompts/*.md`), not string literals, because S20
requires diffing pipeline versions. The actor written to `generated.by` includes
the model id, and run directories record the prompt hash, so any page can be
traced to the exact pipeline that wrote it.

## Build order within phase 2

1. **Deterministic skeleton, no model.** `lw build` with a stub "reconciler" that
   appends each event to `wiki/journal/<date>.md` with correct OKF frontmatter and
   citations. Proves cursor, atomic commit, validation, run logging, and
   `--into`/`--to` end to end. No API key needed, and the result is a real
   (if dumb) wiki.
2. **Retrieval, still no model.** Literal search over the wiki returning candidate
   pages, with a `lw retrieve <text>` debug command to inspect it directly.
3. **The agent loop.** ✅ Implemented in `src/reconcile.rs`, with tools in
   `src/tools.rs` and the OKF write gate in `src/okf.rs`. Tested against a
   scripted model, so the loop, the turn limit, and the
   invalid-write-is-refused path are all covered without an API key.
4. **`lw build`**: batching, cursor, atomic commit, run logs. ✅ Implemented in
   `src/build.rs`.
5. **`lw rebuild`** over the test corpus; start diffing prompt versions (S20).

Steps 1 and 2 are subsumed: with tools, the agent does its own retrieval, and the
deterministic skeleton is now the `lw build` command around the loop rather than
a stand-in for it.

## Open questions

- ~~**D12. Batch granularity.**~~ Resolved (S27): all events since the cursor,
  uncapped. Events from one sitting usually concern one topic and reconcile
  better together.
- **D13. Journal pages at all?** A per-day append-only page is cheap and gives
  narrative texture, but it duplicates the log. Is a journal page a *view* of the
  log (regenerable, deterministic, no model) rather than something the model
  writes? Leaning: deterministic view.
- **D14. Does `lw build` run automatically?** A hook or timer keeps the wiki warm
  but spends money in the background and makes `lw add` non-local in effect.
  Leaning: explicit only, with `lw status` nudging.
- **D10 / D11** from [decisions.md](decisions.md) now become live: how many
  `sources` a page keeps, and whether hand-edits are permitted.


## Turn budgets

Each role has a turn limit, and is told when it is running out:

| role | limit | warns at | on exhaustion |
|------|------:|---------:|---------------|
| build | 16 | 4 remaining | fail; the cursor does not advance |
| ask | 8 | 2 remaining | report that nothing was found |

The numbers come from measurement, not intuition. An answerable question takes
about 3 turns and a build 5–6; a small local model exhausts its 36k context
around turn 8, so a limit above that would never be reached — the run would die
of context first, which is a worse failure because it produces nothing.

The warning is appended to the conversation as a user message and counts down,
so a model that ignores "4 left" sees "3 left". It is deliberately *not* sent
once as a system instruction: a model that has already started casting around
needs the pressure to increase.

Exhaustion means different things for the two roles. A build that runs out has
written pages without knowing whether they are consistent, so it fails and
leaves the cursor alone for a later build to retry. A question that runs out
*has* an answer — the wiki does not appear to say — and reporting that is more
useful than an error.

Two rules keep this from misfiring, both found by watching real runs:

- **Never warn a model that has stopped calling tools.** The warning is a new
  user message, so warning after a prose reply restarts a finished conversation;
  the model then spends its whole budget explaining that it already answered.
- **Warn late.** An earlier version warned with 5 of 8 turns remaining, which
  fired on the third turn of a *healthy* question and derailed it — the model
  replied to the warning ("I understand, I will use my remaining searches
  carefully") instead of answering. A warning is an interruption, so it must
  only arrive once a run is clearly overrunning.
- **Never let exhaustion overwrite an answer.** A model that answers and then
  keeps talking has still answered.
