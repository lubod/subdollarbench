# Round NN: <theme>

Status: draft
Opened: <date>, base `<tag round-NN-open / commit>`
Branch: `roundNN`
Rules: `<current>` -> `<new, if grading/scoring/candidate-visible text changes>`

## Why

<!-- Reviewer: one paragraph that gives the evidence (cell, log line,
path:line) and the operator decision, if there was one. -->

## Items

<!-- Reviewer: at most about 5 items, one theme. Each item is one commit
by the implementer. Every acceptance box names the test or check that
proves it. -->

### <ID>. <title>

<what is wrong and where (path:line); what to change>

Acceptance:
- [ ] <behaviour> — test: `<test name>`
- [ ] <behaviour> — test: `<test name>`

## Out of scope

<!-- Reviewer: what this round must not touch, and which later round
takes it. -->

## Changes after open

<!-- Only with operator approval: date, what changed, why. -->

## Spec questions

<!-- Implementer: anything in the spec that looks wrong (a definition, a
line reference, an approach), asked before implementing it. Reviewer:
the answer, under each question. An answer that changes an item also
goes under "Changes after open". -->

## Implementer notes

<!-- Implementer, per item: the commit, the tests added, anything the
reviewer should look at, and anything deferred and why. For each new
test, cite the seen-failing record
`<git-common-dir>/sdb-gates/seen-<HEAD>-<test>.txt` instead of quoting
the failing line (AGENTS.md, "Tests"; scripts/seen-failing.sh). -->

Ready: `Status: ready <gated commit>`. After that commit, only this file
changes.

Gate: `<git-common-dir>/sdb-gates/<commit>.txt`, RESULT: <PASS/FAIL>

## Review

<!-- Reviewer: a verdict, then findings RNN-1, ..., each marked blocker
or next, with path:line. -->
