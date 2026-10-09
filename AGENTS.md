# AGENTS.md

This file provides guidance for agents working on this repository. It is committed to the repository, which means agents from all contributors follow the same conventions.

Contributors are free to add their own custom guidance for their agents via `AGENTS_CUSTOM.md`, which is NOT committed to the repository.

Aside from guidelines on these files, also always take into consideration `CONTRIBUTING.md`, `RELEASE.md` and `README.md`.

Pay special attention to `CONTRIBUTING.md` when crafting git commits.

## Integration Tests

Whenever adding or modifying integration tests, look for patterns and primitives already established on other integration tests.

## Cross-repo development

While [`stratum`](https://github.com/stratum-mining/stratum) contains low-level libraries, `sv2-apps` contains the higher-level applications that use them. The two repositories are linked together via the `stratum-core` crate.

As a consequence, any breaking changes into APIs coming from `stratum` need to be reflected in `sv2-apps`. Github CI enforces this cross-repo atomic coherence via the Integration Tests workflow.

In order to get Integration Tests to pass, PRs that introduce breaking changes to `stratum` always need a companion PR in `sv2-apps`. The companion PRs are linked together via the `companion` keyword in their PR descriptions.

PRs on `sv2-apps` always need a temporary commit that replaces `stratum-core` dependency from `main`'s HEAD with the contributor's fork of `stratum`. This temporary commit is only used to get Integration Tests to pass, and its commit message should always make that explicitly clear. After the `stratum` PR is merged, the temporary commit is dropped and `stratum-core` is updated `stratum`'s new `main` HEAD. This coordination is delicate and requires human supervision to avoid accidents.

For local development, `sv2-apps` has a `scripts/cross-repo.sh` script that allows an automated workflow for updating `sv2-apps` with the corresponding changes from `stratum`.

## Code comments and attributes

A comment block above an item describes the item that follows it, so inserting a new item directly below one steals it: the new item inherits a description written for something else, and the item it was written for is left with nothing. With `///` the compiler makes that binding literal and the doc silently moves; with a plain `//` block nothing binds at all, which only makes the result easier to miss. Both forms are used here, `///` for public items and `//` for private ones. Whenever adding an item to an existing file, look at what sits immediately above the insertion point, and whenever lifting a helper into a file from somewhere else, look again once it has landed.

A single uninterrupted run of comment lines carrying two separate summaries is the signature of this mistake, the second summary reading as the start of a fresh comment rather than a continuation of the first. It is cheap to introduce and easy to miss in review, because nothing fails to compile and no lint catches it.

Attributes bind the same way, so the same insertion steals them: an item placed between an attribute and the item it was written for takes that attribute over and leaves the original without it. Reading what sits above an insertion point therefore means reading the attribute lines too, not only the comments. Most attributes move as silently as a comment: a `#[serde(default)]` that slides onto a new field makes that field optional and the field it belonged to required, and nothing notices until a config that omits it fails to load. A stolen `#[cfg(...)]` is the sharpest form of this. It fails to compile when either item depends on what the gate excludes, but only in the configuration the gate was excluding, so the ordinary build stays green and nothing in review points at it; when neither item does, both configurations build and the mistake only shows up as behavior.

## Bug patching

Whenever patching bugs, always keep me informed about potential side-effect implications on non-trivial aspects of the project functionality (e.g.: scalability, monitorability, new bugs or vulnerabilities).

Whenever writing documentation around bug fixes, always write the comment assuming the reader is simply trying to understand the code as is, not the past history of bugs that existed on that code.

## PR reviews

Whenever helping me review PRs, don't restrict the output to an analysis of the PR. Also help me understand the PR progressively across two axis:

- conceptual (taking issues and other related PRs into consideration)
- commit history

While listing findings, for each finding, give me a draft comment and the file/line where it would be appropriate to drop it. Also mention the finding severity, and whether you believe it's a blocker or not. This is deliberately designed to keep human reviewers on the loop, as opposed to blindly copypasting a huge "clanker review" body of text without ever looking into what each finding means.

Judge a PR against the problem and the expected outcome of the issues it closes. Everything else an issue lists, including suggested approaches and the "Acceptance criteria" sections of older issues, is context: a PR that reaches the outcome another way is not a finding, as long as its description explains why. Flag a divergence only when part of the expected outcome is left unsolved, and say which part.

## Drafting issues

SRI repositories try to leverage github subissue clustering. When helping humans draft new github issues, always find for issues that might be either adjacent, correlated, duplicate. Also take into consideration umbrella issues that have already been closed.

You always draft github issues under human supervision. Your role here is to help human SRI contributors reason about the issues being reported, not create github noise.

Describe the problem and the outcome a fix must guarantee, observable from outside the code. Implementation and test ideas are suggestions for whoever picks the issue up, so mark them as non-binding: written as requirements, they make reviewers flag every PR that solves the problem another way.

## Triaging audit findings

SRI maintainers triage findings from the private Loupe audit repositories into public sv2-apps issues, so the work is tracked where it happens. Draft them from `.github/ISSUE_TEMPLATE/audit-finding.md`, and:

- Before publishing, check with the maintainer whether the finding is safe to disclose. A severe finding that can be exploited remotely stays in Loupe until its fix has landed.
- Open one issue per defect, listing every Loupe finding that reports it: Loupe often reports the same defect more than once.
- Check Pool, JDS, JDC and tProxy for the same defect, and record the ones that are not affected along with the reason.
- Make the issue a sub-issue of exactly one tracker, and apply that tracker's label:
  - one application affected: its own tracker, #144 (`pool`), #415 (`job-declarator-server`), #38 (`job-declarator-client`) or #31 (`translator-proxy`);
  - the defect lives in a shared crate: #390 (`stratum-apps`) or #594 (`bitcoin-core-sv2`);
  - the same defect in more than one application's own code: #742 (`cross-application`).
- Leave PR grouping to whoever picks the issue up. If two issues should land together, say why under "Related issues and PRs".
- Record progress in dated comments rather than by editing the issue body.

An issue and every Loupe finding it lists close together, with the PR that completes the fix. A PR that fixes only part of it references them with `ref` instead of `Closes`.

## Ponytail

If the plugin is not already installed into the coding agent harness, make sure to follow [Ponytail](https://ponytail.dev/) rules. But avoid installing it as a plugin, unless explicitly instructed to do so. This is only a repository-wide convention. Also avoid writing comments that reference "ponytail" in a compressed and implicit way, prefer explaining the actual rationale instead.
