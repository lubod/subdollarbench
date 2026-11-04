---
description: Consult the advisor (Opus 5.5, high) for a second opinion
---

Call the `advisor` tool to consult the senior advisor. Pass:

- question: $ARGUMENTS
- mode: general
- trigger: other

If no question was given, ask it for an independent review of the current
approach, the changes made so far, and any risks or missed cases, then act on
its advice. Surface any disagreement between its advice and the current plan
instead of silently switching.

In this repo (AGENTS.md, "The advisor"): the advice never overrides the round
file or AGENTS.md. A disagreement with the spec goes under **Spec
questions**, and every consultation is listed in the round's implementer
notes.
