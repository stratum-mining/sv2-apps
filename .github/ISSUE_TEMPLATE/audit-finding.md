---
name: Audit finding (maintainers)
about: Publish a defect triaged from the private Loupe audit repositories
title: "<application or crate>: <what the fix guarantees>"
labels: ""
assignees: ""
---

<!--
For SRI maintainers. The triage rules (placement, labels, closure) are in AGENTS.md, under
"Triaging audit findings". Delete these comments before submitting.
-->

## Affected applications and crates

<!--
Every application (Pool, JDS, JDC, tProxy) and shared crate checked for this defect.
List the unaffected ones too, each with one line on why.
-->

## Problem

<!-- What goes wrong, where, and under which conditions. -->

## Impact

<!-- Who is affected, what it costs them, and what a peer or attacker needs to trigger it. -->

## Expected outcome

<!--
The guarantees that must hold once this is fixed, observable from outside the code.
A PR is reviewed against these.
-->

## Possible approach (non-binding)

<!-- Implementation and test ideas. A PR may solve the problem another way. -->

## Loupe findings

<!-- Every Loupe finding reporting this defect. They close together with this issue. -->

- [ ] project-loupe/audit-sv2-apps#N: one-line summary

## Related issues and PRs

<!-- Facts only, e.g. "touches the same token lifecycle as #751". -->
