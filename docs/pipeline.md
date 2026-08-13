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

### P1. Retrieval — which pages might this event touch?
The model can't see the whole wiki (context limits, and it grows without bound),
so something must select candidate pages *before* generation. This is the problem
that most determines output quality: **a page that isn't retrieved cannot be
reconciled**, and the failure is silent — the wiki keeps quietly asserting the old
fact.

Hard because the event's wording need not share vocabulary with the page
("apples" vs. a page titled `preferences.md`), and because the *absence* of a
relevant page is a legitimate answer (create a new one).

Approaches, cheapest first:

1. **Literal search** over page text and titles for salient nouns from the event.
   Free, deterministic, no model. Misses paraphrase.
2. **Index-based routing**: feed the model `index.md` (titles + descriptions
   only — cheap) and let it name candidate pages. Handles paraphrase, costs one
   model call, and is why OKF's `index.md` is worth generating (S21).
3. **Embeddings** over pages/sections. Best recall, but adds a model dependency,
   a vector store, and a rebuild cost. Explicitly deferred until 1+2 are shown
   insufficient.

Start with 1+2 combined: literal search proposes, the model's view of the index
adds what literal search missed.

### P2. Reconciliation — what should change, and how?
Given the event plus candidate pages, decide per claim: leave alone, amend,
delete, or add. Includes the judgement S6 assigned to the model — whether the old
value should be **erased** ("that was never true") or **kept as dated history**
("was true, isn't now").

That judgement can't be made per-event in isolation; it depends on the *kind of
fact*. So it belongs in page conventions, expressed via `type`:

| `type` | supersession policy |
|--------|--------------------|
| `Person`, `Place`, `Project` | current state; corrections erase |
| `Opinion`, `Preference` | keep dated history — how views changed is the point |
| `Habit` | keep dated history |
| `Journal` | append-only; never rewritten (it's a record of a day) |
| `Todo` | current state; completion moves the item |

Encoding this per-type rather than per-event is what makes the model's behaviour
consistent enough to be predictable.

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
3. **Model reconciliation.** Single-page case first: one event, one candidate
   page, whole-file rewrite, strict validation.
4. **Multi-page batches**, `index.md` regeneration, `unresolved.md`.
5. **`lw rebuild`** over the test corpus; start diffing prompt versions (S20).

Step 1 is the one that de-risks the rest: everything except the model call gets
exercised and tested before nondeterminism enters.

## Open questions

- **D12. Batch granularity.** One model call per event (simple, expensive, misses
  that two events in one sitting often concern one topic) or per batch (cheaper,
  more context, but a single bad response can corrupt several pages)? Leaning
  per-batch with a small cap.
- **D13. Journal pages at all?** A per-day append-only page is cheap and gives
  narrative texture, but it duplicates the log. Is a journal page a *view* of the
  log (regenerable, deterministic, no model) rather than something the model
  writes? Leaning: deterministic view.
- **D14. Does `lw build` run automatically?** A hook or timer keeps the wiki warm
  but spends money in the background and makes `lw add` non-local in effect.
  Leaning: explicit only, with `lw status` nudging.
- **D10 / D11** from [decisions.md](decisions.md) now become live: how many
  `sources` a page keeps, and whether hand-edits are permitted.
