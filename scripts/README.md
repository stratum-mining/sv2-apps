# Scripts Directory

This directory contains utility scripts for building, testing, and publishing the SV2 Applications (both Pool and Miner applications).

## Available Scripts

### 🚀 Publishing Scripts

#### `publish-apps.sh`
**Main publishing script for this repository**
- Publishes Pool Apps: `pool_sv2` and `jd_server` crates to crates.io
- Publishes Miner Apps: `jd_client` and `translator_sv2` crates to crates.io
- Includes safety checks and confirmations
- Interactive confirmation before publishing

**Usage:**
```bash
# Dry run (recommended first)
./scripts/publish-apps.sh --dry-run

# Actual publish (requires crates.io login)
./scripts/publish-apps.sh
```

### 🔧 Development Scripts

#### `clippy-fmt-and-test.sh`
**Complete code quality check**
- Runs clippy linting on all three workspaces
- Runs all tests across all applications
- Formats code with rustfmt
- Works on Pool Apps workspace (`pool`, `jd-server`), Miner Apps workspace (`jd-client`, `translator`, `test-utils`), and Integration Tests workspace

**Usage:**
```bash
./scripts/clippy-fmt-and-test.sh
```

#### `build-all-workspaces.sh`
**Build and format all three workspaces**
- Builds Pool Apps workspace, Miner Apps workspace, and Integration Tests workspace
- Formats code across all workspaces
- Good for CI/CD pipelines

**Usage:**
```bash
./scripts/build-all-workspaces.sh
```



### 📊 Testing & Coverage Scripts

#### `coverage-apps.sh`
**Generate test coverage reports**
- Uses cargo-tarpaulin for coverage analysis
- Generates XML reports for all three workspaces
- Covers Pool Apps workspace, Miner Apps workspace, and Integration Tests workspace

**Prerequisites:**
```bash
cargo install cargo-tarpaulin
```

**Usage:**
```bash
./scripts/coverage-apps.sh
```

## Prerequisites

### For Publishing
1. **crates.io account and login token**
   ```bash
   cargo login <your-token>
   ```

2. **Repository access** - You need to be a maintainer of the published crates

### For Development
1. **Rust toolchain** (1.75.0 or later)
   ```bash
   rustup install 1.75.0
   rustup install nightly  # for formatting
   ```

2. **For coverage** (optional)
   ```bash
   cargo install cargo-tarpaulin
   ```

## Current Repository Structure

This repository contains multiple SV2 applications organized in **three main workspaces**:

**Pool Applications Workspace (`pool-apps/`):**
- **`pool/`** - SV2 Pool implementation (`pool_sv2` crate)
- **`jd-server/`** - Job Declarator Server implementation (`jd_server` crate)

**Miner Applications Workspace (`miner-apps/`):**
- **`jd-client/`** - Job Declarator Client implementation (`jd_client` crate)
- **`translator/`** - SV1 to SV2 Translator implementation (`translator_sv2` crate)
- **`test-utils/`** - Mining device test utilities:
  - **`mining-device/`** - Mining device simulator (`mining_device` crate)
  - **`mining-device-sv1/`** - SV1 mining device simulator (`mining_device_sv1` crate)

**Integration Tests Workspace (`test/`):**
- **`integration-tests/`** - End-to-end integration tests

All crates depend on external SV2 protocol libraries that should be available on crates.io.

## Publishing Workflow

1. **Prepare for release:**
   ```bash
   # Check everything builds and tests pass
   ./scripts/clippy-fmt-and-test.sh
   ```

2. **Test publishing (dry run):**
   ```bash
   ./scripts/publish-apps.sh --dry-run
   ```

3. **Actual publishing:**
   ```bash
   # Make sure you're logged in
   cargo login <your-token>
   
   # Publish
   ./scripts/publish-apps.sh
   ```

## Notes

- All scripts are designed to work from the project root directory
- Scripts will automatically navigate to the correct directories
- Publishing scripts include safety checks for "already published" crates
- Development scripts use specific Rust toolchain versions for consistency 