# SV2 Applications Release Process

## Changelog

The changelog is a list of notable changes for each version of the SV2 Applications. 
It is a way to keep track of the project's progress and to communicate 
the changes to the users and the community.

The changelog is automatically generated on each release page, and extra contextual
information is added as needed, such as:
 - General release information.
 - Breaking changes.
 - Notable changes.
 - Dependencies updates from the main SRI repository.

## Release Process

Try to constrain each development cycle to a fixed time period, after which a
release is made. A release is not published straight from `main`: it is first
cut as a **release candidate**, which the SRI team and the community run on
mainnet during a test period. Only a candidate that gets through that period
without a blocking problem becomes the release.

Releases are coordinated with [`stratum`](https://github.com/stratum-mining/stratum)
and [`sv2-ui`](https://github.com/stratum-mining/sv2-ui), always in the order
`stratum`, `sv2-apps`, `sv2-ui`, because each one is built on the one before it:
the applications use the `stratum-core` crate, and SV2 UI runs the application
images. The order holds for candidates as well as for the final release.

Each release has a release branch named `release/vX.Y.Z`, created from `main`
when its first candidate is cut. `main` is never frozen: whatever merges there
after the branch was created belongs to the next release.

### Cutting a Release Candidate

A candidate is tagged `vX.Y.Z-rc.N`, starting at `rc.1`. The `stratum`
candidate it builds on has to be tagged first.

1. **Run Tests:** Execute `./scripts/clippy-fmt-and-test.sh` to ensure the workspace passes tests, linting, and formatting.
2. **Create Release Branch:** For the first candidate, create `release/vX.Y.Z` from the commit of `main` whose `Cargo.lock` pins the `stratum` commit that carries the `stratum` candidate tag.
3. **Pin `stratum-core`:** In `stratum-apps/Cargo.toml` and `bitcoin-core-sv2/Cargo.toml`, replace `branch = "main"` with the `stratum` candidate tag and the version `stratum-core` has at that tag, for example `tag = "v1.14.0-rc.1", version = "0.8.0"`. Run `cargo update --workspace` to bring `Cargo.lock` in line.
4. **Pin the Docker Images:** In `docker/docker-compose.yml`, point the images at the `vX.Y.Z-rc.N` tags.
5. **Push the Release Branch:** CI runs on pushes to `release/v*`. On top of what it runs for `main`, it runs `./scripts/check-published-crates.sh`, which fails when a crate carries a version that is already on crates.io while its contents differ from what was published. `cargo publish` would refuse that crate, so it needs a version bump before the candidate goes out, made like any other fix (see [Fixing a Blocking Problem](#fixing-a-blocking-problem)).
6. **Publish the Candidate:** Once CI passes on the release branch, create the `vX.Y.Z-rc.N` tag on it and publish it as a GitHub **pre-release**. The binaries are attached to it and the Docker images are pushed as `stratumv2/<app>:vX.Y.Z-rc.N`. Nothing is published to crates.io.
7. **Deploy and Announce:** Upgrade the hosted SRI Pool and JDS to the candidate, smoke-test the miner apps against them, and announce the candidate together with the `sv2-ui` candidate that runs its images.

### Test Period

The test period lasts 7 days, counted from the publication of the newest
candidate among `stratum`, `sv2-apps` and `sv2-ui`, since the three are released
together.

- The SRI team and community members run the candidate on mainnet, against as many different endpoints as possible. Where an endpoint can stay on the previous release, the candidate is also run against it, since pools and miners upgrade at different times.
- A problem is reported as an issue that names the candidate tag it was found on.
- Maintainers decide whether a reported problem blocks the release. A regression from the previous release blocks it, and so does anything that risks lost work or funds. Everything else moves to the next milestone.
- The period ends after 7 days without an open blocking problem.

### Fixing a Blocking Problem

1. **Fix on `main`:** Merge the fix on `main` first, through a regular PR. A candidate counts as a release for the version checks, so CI asks the PR to bump the crates it modifies above the candidate's versions (see [CONTRIBUTING.md](CONTRIBUTING.md)).
2. **Backport:** Open a PR against `release/vX.Y.Z` that cherry-picks the fix, together with its version bump if it has one. CI on a release branch compares each crate with crates.io, and fails if the fix still lacks a bump. If `main` has changed the same code since the branch was created, adapt the fix in this PR. A fix that only applies to the release branch still has to be checked against `main`, and ported there if `main` is affected.
3. **Cut the Next Candidate:** Repeat steps 4 to 7 of [Cutting a Release Candidate](#cutting-a-release-candidate) with the next `rc.N`. When the fix is in `stratum`, its next candidate is tagged first and step 3 is repeated with the new tag.
4. **Restart the Test Period:** A new candidate starts a new 7 days.

### Final Release

Once the test period ends, and the `stratum` release is on crates.io:

1. **Update Dependencies:** In `stratum-apps/Cargo.toml` and `bitcoin-core-sv2/Cargo.toml`, remove the `git` and `tag` keys of `stratum-core`, which leaves its version requirement, for example `stratum-core = { version = "0.8.0" }`. Run `cargo update --workspace` to bring `Cargo.lock` in line.
2. **Pin the Docker Images:** In `docker/docker-compose.yml`, point the images at the `vX.Y.Z` tags.
3. **Push the Release Branch:** These two commits are all that separates the release from the last candidate, and CI has to pass on them.
4. **Create GitHub Release:** Once CI passes on the release branch, create the `vX.Y.Z` tag on it and publish a GitHub release with changelog and release notes. Publishing it runs `./scripts/check-published-crates.sh` once more, publishes the crates to crates.io, attaches the binaries, and pushes the Docker images as `stratumv2/<app>:vX.Y.Z`.

### Patch Releases

A patch release follows the same process, with these differences:

- Its release branch is created from the tag of the release it patches, not from `main`.
- It only takes fixes. Each one is merged on `main` first and then backported, as in [Fixing a Blocking Problem](#fixing-a-blocking-problem).
- Its test period lasts at least 48 hours. It has to include a confirmation of the fix by whoever reported the problem, and a run against the previous release on the other end of every connection the fix touches.
- `stratum` only takes part when the fix is there. Otherwise the branch stays on the published `stratum-core`, and there is nothing to pin.

A fix for a vulnerability that has to stay private until it ships does not go
through a public candidate, which would disclose it. It is tested privately by
the maintainers and released directly.

## Versioning

This repository contains alpha-stage applications, all belonging to a single Cargo workspace rooted at the repository root:
- `pool-apps/` - Pool server and Job Declarator Server
- `miner-apps/` - Job Declarator Client, Translator Proxy, and test utilities
- `stratum-apps/` - Shared application-level library used by the roles
- `bitcoin-core-sv2/` - Bitcoin Core IPC bindings
- `integration-tests/` - End-to-end testing suite

### Application Versioning

Applications in this repository follow SemVer 2.0.0. The version number is stored in
the `Cargo.toml` file of each application. Applications are designed to be production-ready
and stable, so version changes should reflect this maturity level.

### Repository Versioning  

The global repository releases follow `X.Y.Z`, which is changed under the following criteria:
- If a release includes only bug fixes and minor improvements, then `Z` is bumped.
- If a release includes new features, application improvements, or dependency updates from the main SRI repository, then `Y` is bumped.
- If a release marks a major milestone (e.g., significant architectural changes, major new applications, or breaking changes), then `X` is bumped.

### Dependency Management

This repository depends on the main [SRI repository](https://github.com/stratum-mining/stratum) for protocol implementations. When SRI releases new versions, this repository should be updated to use the latest stable SRI dependencies and tested for compatibility.

## Tags and Branches

- Changes to `main` branch should be added through a merge commit.
- The `main` branch is the default branch and it is always active.
- The `main` branch is protected and requires a pull request to merge changes
  with at least 2 approvals.
- Each release is tagged with the version number of the release and a release
  branch is kept for future reference or fixes.
- Each release candidate is tagged `vX.Y.Z-rc.N` on the release branch of its
  release.
- Changes to a release branch are added through a pull request against it. The
  one exception is the pin commits this document lists for a release candidate
  and for the final release, which maintainers push directly.
