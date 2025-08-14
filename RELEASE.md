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
release is made. After all release tasks are completed, initiate the release
process by creating a new release branch, named `x.y.z.` referring to the
version number, from the `main` branch.  The release branch is used as a
breaking point and future reference for the release and should include the
changelog entries for the release and any other release-specific tasks. Any bug
fixes or changes for the release should be done on the release branch. When the
release branch is ready, create a new tag and initiate any publishing tasks.

Usually the release process is as follows:

1. **Update Dependencies:** Ensure all SRI dependencies are updated to the latest stable versions from the main repository.
2. **Run Tests:** Execute `./scripts/clippy-fmt-and-test.sh` to ensure all workspaces pass tests, linting, and formatting.
3. **Create Release Branch:** Create a new release branch from the `main` branch.
4. **Update Versions:** Update version numbers in relevant `Cargo.toml` files.
5. **Create Tag:** Create a new tag for the release branch.
6. **Publish Release:** Use `./scripts/publish-apps.sh` to publish applications to crates.io (if applicable).
7. **Create GitHub Release:** Create a GitHub release with changelog and release notes.

## Versioning

This repository contains alpha-stage applications organized in workspaces:
- `pool-apps/` - Pool server and Job Declarator Server
- `miner-apps/` - Job Declarator Client, Translator Proxy, and test utilities  
- `test/integration-tests/` - End-to-end testing suite

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
