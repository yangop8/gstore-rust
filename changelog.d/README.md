# changelog.d — change fragments

**Status: folded.** The Phase 6 fragments — one per work package of Rounds
1–4 plus their merge and review records, and the CI toolchain-pin note —
were folded by the closing package (R4-CLOSURE) into `CHANGELOG.md` (the
"Phase 6" section of "Unreleased", and its "CI notes"), `README.md`,
`docs/BENCHMARKS.md`, `docs/GAP_PLAN.md` §9 and the other `docs/*.md`, and
removed. Their full text stays in the git history of this directory. The
directory is empty until work runs in parallel again; the convention below
is kept for that.

Phase 6 work packages (docs/PHASE6_PLAN.md) ran in parallel worktrees, so
none of them edited the shared documents (`README.md`, `CHANGELOG.md`,
`docs/*.md`). Each package instead wrote **one fragment**,
`changelog.d/<phase>-<ID>.md` (for example `phase6-R1-BENCH.md`), and the
closing package folded every fragment into those documents and removed the
fragments.

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
