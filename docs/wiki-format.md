# Wiki format: OKF assessment

Evaluated [Open Knowledge Format v0.2](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md)
as the format for the `wiki/` layer.

**Verdict: adopt a deliberate subset.** OKF's substrate is already what we chose
independently, and it standardises the provenance and trust conventions we had
listed as to-be-designed. It contributes nothing to reconciliation, which remains
the hard part and stays entirely ours.

## Why it fits

OKF's core is "a directory of markdown files with YAML frontmatter, distributed as
a git repo, no required tooling, readable with `cat`." That is a description of
what `design.md` already specified. Agreement on substrate means adoption is
mostly *naming* things we would otherwise name ourselves.

Beyond that, three parts earn their place:

**`sources` + footnote attribution (§5.1)** — the important one. We had committed
to citing provenance ("this wiki line came from event 01K2C5…") without deciding a
representation. OKF specifies one: list sources in frontmatter with stable `id`s,
then attribute individual claims with markdown footnotes keyed to those ids.

```markdown
Birthday is 14 March.[^e01K2C57T]

[^e01K2C57T]: "Ada's birthday is the 14th of March, not the 4th."
```

Because our log lives in the same repo, an event is addressable as a
bundle-relative path (`/log/2026-08.jsonl#01K2C57T…`), so citations resolve to
real files. The spec's reasoning for keyed rather than positional labels is
exactly our situation: *"agents constantly rewrite these documents; a positional
index misattributes silently the moment the list is reordered."*

**`generated: {by, at}` (§5.2)** — records which actor wrote the current content,
e.g. `lifewiki/claude-sonnet-4.5`. This directly serves S20 (judging quality by
diffing pipeline versions): every page states which pipeline version produced it,
so a rebuild diff is attributable without cross-referencing `state/runs/`.

**`verified` and trust tiers (§5.3)** — distinguishes model-written from
human-confirmed content. More useful for us than for the enterprise case it was
designed for: it gives the pipeline a machine-readable signal for "a human
touched this, don't casually overwrite it." See the open question below.

Two smaller wins: `index.md` for progressive disclosure (which the phase-3 agent
wants anyway, to see what exists before opening files), and a consumer contract
that is aggressively forgiving — unknown `type`s, unknown keys, missing optional
fields, and **broken links** must all be tolerated. That last one matters when a
model writes pages: linking to a not-yet-written page is explicitly legal, so the
pipeline can reference `/people/ada.md` before it exists.

## What we skip, and why

| Part | Decision |
|------|----------|
| **Attested computations (§10)** — executors, attesters, receipts | Skip entirely. ~180 lines of the spec, aimed at verifying that a financial metric was computed by sanctioned code. No analogue in a life wiki. Fully optional |
| **`log.md` (§9)** — per-directory changelog | Skip. Git gives us this, and we already have an "event log"; two things called a log in one repo is a naming collision waiting to confuse |
| **`resource`** — canonical URI of the underlying asset | Mostly absent. Most life concepts are abstract; §4.4 explicitly covers this |
| **`stale_after`** — absolute staleness date | Skip initially. Might earn a place later ("recheck whether I still work here") but it's speculative |
| **Cross-org exchange**, the spec's main motivation | Irrelevant to a single-user local tool. Costs nothing to ignore |

## What OKF does not do for us

Worth stating plainly, because the format's polish makes it tempting to think it
solves more than it does.

**No supersession semantics.** OKF has no way to say "this fact replaced that
one." Links are explicitly *untyped* — §6.1 says the relationship kind "is
conveyed by the surrounding prose, not by the link itself." `status: deprecated`
is page-level, not claim-level. So the entire problem from S6 — noticing that "I
prefer apples" contradicts a page saying oranges, and deciding whether to erase
or keep a dated history — is untouched. That is fine, since we concluded it's a
model judgement, but adopting OKF must not create the impression the hard part is
handled.

**A frontmatter burden on every rewrite.** The pipeline returns whole-file
rewrites, so the model must now reproduce and update frontmatter correctly every
time. That is more tokens and a new class of failure (mangled YAML, dropped
sources). Two mitigations: conformance is machine-checkable, so `lw doctor` can
validate every page (non-empty `type`, parseable frontmatter) and turn model
sloppiness into a caught error rather than silent corruption; and we can consider
having the pipeline emit frontmatter fields as structured output that *we* render,
rather than trusting the model to hand-write YAML.

**A scaling concern the spec wasn't designed for.** OKF concepts derive from a
handful of sources. Ours accrete: a page about a person could accumulate hundreds
of event citations over years, bloating frontmatter past the point of usefulness.
Needs a policy — see open questions.

**Stability risk.** v0.2 of a young spec, with breaking changes already between
v0.1 and v0.2 (§13). Mitigated by the fact that our subset is small and the
substrate — markdown plus frontmatter in git — survives regardless. If OKF
changes or dies, we keep the conventions and drop the name. We should record
`okf_version: "0.2"` in the root `index.md` so future us knows what was targeted.

## Proposed shape

```markdown
---
type: Person
title: Ada
description: Sister; lives in Lisbon.
tags: [family]
generated: { by: lifewiki/claude-sonnet-4.5, at: 2026-08-11T13:02:44-04:00 }
verified: { by: human:jack, at: 2026-08-12T08:10:00-04:00 }
sources:
  - id: e01K2C57T
    resource: /log/2026-08.jsonl#01K2C57TDA6P1YQ8W3ZR4H2J9C
    title: "Ada's birthday is the 14th of March, not the 4th."
    last_modified: 2026-08-11
---

# Facts

Birthday is 14 March.[^e01K2C57T]

[^e01K2C57T]: "Ada's birthday is the 14th of March, not the 4th."
```

Types are producer-defined and unregistered, so ours are just: `Person`,
`Project`, `Place`, `Opinion`, `Habit`, `Journal`, `Reference`. Links use the
recommended bundle-absolute form (`/people/ada.md`).

## Impact on phase 1

**None.** The event log is unaffected — this is a phase 2 decision, recorded now
because it changes what the pipeline emits and gives `lw doctor` a second job
(validating wiki conformance alongside log integrity). One incidental
confirmation: citing events as `/log/YYYY-MM.jsonl#<id>` only works because log
and wiki share a repo (S7), so that choice pays off in an unplanned way.
