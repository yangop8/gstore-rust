# changelog.d — change fragments

Phase 6 work packages (docs/PHASE6_PLAN.md) run in parallel worktrees, so
none of them edits the shared documents (`README.md`, `CHANGELOG.md`,
`docs/*.md`). Each package instead writes **one fragment**,
`changelog.d/phase6-<ID>.md` (for example `phase6-R1-BENCH.md`), and the
closing package (R4-CLOSURE) folds every fragment into those documents and
removes the fragments.

A fragment has exactly these sections, in this order (contract C10):

```markdown
# <ID> — <one-line title>

## Changelog
Bullets in CHANGELOG.md's style, for its "Unreleased" section: what changed
for users and developers, behaviour changes first, new files and tools after.

## GAP_PLAN status line
One line for the Phase 6 row of docs/GAP_PLAN.md §8 (or "none").

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
