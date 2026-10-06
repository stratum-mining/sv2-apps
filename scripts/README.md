# Scripts Directory

This directory contains utility scripts for building, testing, and publishing the SV2 Applications (both Pool and Miner applications).

## Available Scripts

### 🚀 Publishing Scripts

#### `publish.sh`
**Main publishing script for this repository**
- Publishes Pool Apps: `pool_sv2` and `jd_server_sv2` crates to crates.io
- Publishes Miner Apps: `jd_client_sv2` and `translator_sv2` crates to crates.io
- Includes safety checks and confirmations
- Interactive confirmation before publishing

**Usage:**
```bash
# Dry run (recommended first)
./scripts/publish.sh --dry-run

# Actual publish (requires crates.io login)
./scripts/publish.sh
```

### 🔧 Development Scripts

#### `clippy-fmt-and-test.sh`
**Complete code quality check**
- Runs clippy linting on each crate, with the features that crate enables itself
- Runs all tests across every crate, including the integration tests
- Formats code with rustfmt

**Usage:**
```bash
./scripts/clippy-fmt-and-test.sh
```

#### `build-all.sh`
**Build and format the workspace**
- Builds every crate in the workspace
- Formats code with rustfmt
- Good for CI/CD pipelines

**Usage:**
```bash
./scripts/build-all.sh
```

#### `cross-repo.sh`
**Local development against a stratum checkout**
- Must be *sourced*, not executed, since it defines shell functions
- `patch-cargo-toml` points `stratum-core` at a local `stratum` checkout, via `[patch]` in the workspace root manifest
- `restore-cargo-toml` drops the patch section and refreshes the lockfile
- `get-stratum-core-path` resolves the checkout and keeps the `stratum` symlink at the repository root pointed at it

**Usage:**
```bash
source scripts/cross-repo.sh
patch-cargo-toml            # or: patch-cargo-toml /path/to/stratum
restore-cargo-toml
```



### 📊 Testing & Coverage Scripts

#### Script regression tests
Run offline with Python 3, Bash, and jq installed. Cargo publishing and crates.io
requests are stubbed out:
```bash
python3 -B -m unittest discover -s scripts/tests -v
```

These tests also run in CI and `clippy-fmt-and-test.sh`.

#### `coverage.sh`
**Generate test coverage reports**
- Uses cargo-tarpaulin for coverage analysis
- Generates one XML report per crate group, so each keeps its own codecov flag
- Writes everything under `target/tarpaulin-reports/`

**Prerequisites:**
```bash
cargo install cargo-tarpaulin
```

**Usage:**
```bash
./scripts/coverage.sh
```

## Prerequisites

### For Publishing
1. **crates.io account and login token**
   ```bash
   cargo login <your-token>
   ```

2. **Repository access** - You need to be a maintainer of the published crates

### For Development
1. **Rust toolchain** (1.88.0 or later)
   ```bash
   rustup install 1.88.0
   rustup install nightly  # for formatting
   ```

2. **For coverage** (optional)
   ```bash
   cargo install cargo-tarpaulin
   ```

## Current Repository Structure

This repository is a **single Cargo workspace** rooted at the repository root, holding
every SV2 application:

**Pool Applications (`pool-apps/`):**
- **`pool/`** - SV2 Pool implementation (`pool_sv2` crate)
- **`jd-server/`** - Job Declarator Server implementation (`jd_server_sv2` crate)

**Miner Applications (`miner-apps/`):**
- **`jd-client/`** - Job Declarator Client implementation (`jd_client_sv2` crate)
- **`translator/`** - SV1 to SV2 Translator implementation (`translator_sv2` crate)

**Shared libraries:**
- **`stratum-apps/`** - Application-level helpers shared by the roles (`stratum-apps` crate)
- **`bitcoin-core-sv2/`** - Bitcoin Core IPC bindings (`bitcoin_core_sv2` crate)

**Integration Tests (`integration-tests/`):**
- End-to-end integration tests, kept out of `default-members` because they drive
  real `bitcoind` and template provider binaries. Run them with
  `cargo test -p integration_tests_sv2`.

All crates depend on external SV2 protocol libraries that should be available on crates.io.

## Publishing Workflow

1. **Prepare for release:**
   ```bash
   # Check everything builds and tests pass
   ./scripts/clippy-fmt-and-test.sh
   ```

2. **Test publishing (dry run):**
   ```bash
   ./scripts/publish.sh --dry-run
   ```

3. **Actual publishing:**
   ```bash
   # Make sure you're logged in
   cargo login <your-token>
   
   # Publish
   ./scripts/publish.sh
   ```

## Notes

- All scripts are designed to work from the project root directory
- Scripts will automatically navigate to the correct directories
- Publishing scripts include safety checks for "already published" crates
- Development scripts use specific Rust toolchain versions for consistency 
