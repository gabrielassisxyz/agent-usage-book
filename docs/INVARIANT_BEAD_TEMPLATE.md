<!--
Invariant bead template. Start every bead whose work makes an invariant
enforced from this file: fill every `<...>` placeholder and keep every `##`
section below (a bead without an acceptance-criteria section is flagged by
`br lint`). This comment is author instruction, not bead content; delete it
when filing.

Consumer: whoever authors or reviews an invariant-enforcing bead. Gate: the
Phase 2 close refuses a bead still named as unenforced, and review checks the
row-update criterion is present and filled. Observed defect class: aub-p6ke,
where a landed suite closed its bead without repointing its row and the audit
went red on `main` in the next unrelated run. Retire when no row of
`docs/INVARIANTS.md` names a bead anymore, or when the tracker itself refuses
to close a named bead.
-->

## Outcome

<one sentence: which invariant becomes enforced, and by what mechanism>

## Context

<why this invariant needs a mechanical check, and what a plausible but wrong
implementation would do instead>

`docs/INVARIANTS.md` row `<N>` names open bead `<bead id>` today. The change
that makes the invariant enforced is the only actor that knows the row is now
stale: no gate running before the close can observe a status the close has
not yet produced, so the row update belongs in this change, in the same
commit series, not in a follow-up.

## Acceptance criteria

- [ ] `docs/INVARIANTS.md` row `<N>` no longer names `<bead id>`: its
  Enforcing path column says `<enforcing file path>` and its Test or
  constraint column says `<test name>`, in the same commit series as the
  enforcing change
- [ ] The enforcement-summary counts under the table match the table after
  the repoint
- [ ] Done when: <the observable behaviour that proves the invariant holds,
  and the planted negative that fails without it>

## Tests

- [ ] <the unit, integration or e2e cases this bead adds, each paired with
  the negative a naive wrong implementation would fail>

## Blast radius

<the modules, tests and docs this change touches; no change to the landing
flow, the tracker, or any tool outside this repository>

## Plan reference

<the PLAN.md sections this invariant comes from>
