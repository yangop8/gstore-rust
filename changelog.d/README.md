# changelog.d — change fragments

**Status: folded.** The review-fix campaign's fragments — one per work
package of its five rounds (37 packages) plus each round's integration
record (`r1-integration.md` … `r5-integration.md`), 42 in all — were folded
by the campaign's closing step and removed:

- `CHANGELOG.md`: the "Review-fix campaign (after Phase 6)" section at the
  top of "Unreleased" (upgrade notes, behaviour changes, fixes, new tests
  and tools, measured numbers), and the corrections the fragments asked for
  in the Phase 6, Phase 5 and Phase 3 sections and the CI notes;
- `docs/DEPLOY.md`: the "Secure defaults and upstream-compat switches"
  section (one table row per compat switch: key, secure default, upstream
  value, risk, whether `scripts/conformance.sh` enables it; the limits),
  the new "Operations" and "Upgrade notes" sections, and the deployment
  notes of the other sections;
- `README.md`: the user-facing notes, including the new "SPARQL semantics
  and known deviations", "HTTP behaviour and secure defaults" and
  "Upgrading from an earlier version" sections;
- `docs/STORAGE_BACKEND.md`: the "Review-fix campaign" section (commit
  protocol, crash recovery, the format registry's "no format change"
  table) and the in-place corrections;
- `docs/GAP_PLAN.md`: the campaign's status lines (§10) and the updated
  status paragraphs and rows;
- `docs/REFACTOR_BACKLOG.md`: part J — the plan's deferred ids and every
  partial or deferred id and remaining refactor the fragments reported,
  each with its review ids and reason, and the refuted findings;
- `docs/CPP_PARITY.md`, `docs/DESIGN.md` §11 and `docs/PFN.md`: the notes
  addressed to them.

Before that, the Phase 6 fragments — one per work package of Rounds 1–4
plus their merge and review records, and the CI toolchain-pin note — were
folded by the closing package (R4-CLOSURE) into `CHANGELOG.md` (the
"Phase 6" section of "Unreleased", and its "CI notes"), `README.md`,
`docs/BENCHMARKS.md`, `docs/GAP_PLAN.md` §9 and the other `docs/*.md`, and
removed. The full text of both sets of fragments stays in the git history
of this directory. The directory is empty until work runs in parallel
again; the convention below is kept for that.

Phase 6 work packages (docs/PHASE6_PLAN.md) ran in parallel worktrees, so
none of them edited the shared documents (`README.md`, `CHANGELOG.md`,
`docs/*.md`). Each package instead wrote **one fragment**,
`changelog.d/<phase>-<ID>.md` (for example `phase6-R1-BENCH.md`), and the
closing package folded every fragment into those documents and removed the
fragments. The review-fix campaign's packages did the same, naming each
fragment after the package key (`changelog.d/r1-http-surface.md`); a round's
integration wrote `changelog.d/r<N>-integration.md`.

A fragment has exactly these sections, in this order (contract C10):

```markdown
# <ID> — <one-line title>

## Changelog
Bullets in CHANGELOG.md's style, for its "Unreleased" section: what changed
for users and developers, behaviour changes first, new files and tools after.

## GAP_PLAN status line
One line for the phase's row of docs/GAP_PLAN.md §8 / §9 (or "none").

## Docs notes
What README.md / docs/*.md must say once this lands: new commands, options,
configuration keys, formats, caveats — with the target file for each.

## Measured numbers
The benchmark figures behind the package's acceptance items: commands,
machine state (say so when other jobs were running), base vs branch measured
in the same session, and which items were left to a later, quiet
measurement.

## Requests/Not yet
Changes the package needed outside its file ownership (for the integrator or
a later round), and what it did not finish, with the reason.
```

Write fragments in English, keep them factual, and give numbers with their
units and the command that produced them.
