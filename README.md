
<h1 align="center">
  <br>
  <a href="https://stratumprotocol.org"><img src="https://github.com/stratum-mining/stratumprotocol.org/blob/660ecc6ccd2eca82d0895cef939f4670adc6d1f4/src/.vuepress/public/assets/stratum-logo%402x.png" alt="SRI" width="200"></a>
  <br>
SV2 Applications
  <br>
</h1>

<h4 align="center">Stratum V2 pool and miner applications from the SRI project 🦀</h4>

<p align="center">
  <a href="https://codecov.io/gh/stratum-mining/stratum">
    <img src="https://codecov.io/gh/stratum-mining/stratum/branch/main/graph/badge.svg" alt="codecov">
  </a>
  <a href="https://twitter.com/intent/follow?screen_name=stratumv2">
    <img src="https://img.shields.io/twitter/follow/stratumv2?style=social" alt="X (formerly Twitter) Follow">
  </a>
</p>

> [!CAUTION]
> **🚧 REPOSITORY UNDER MIGRATION - NOT READY FOR USE 🚧**
> 
> This repository is part of an ongoing process to move applications outside of the main [SRI repository](https://github.com/stratum-mining/stratum). **This repository will be rebased on top of the main repository** and is subject to significant changes until the SRI 1.5.0 release.
>
> **⚠️ DO NOT USE THIS REPOSITORY YET** - it may contain outdated code, broken functionality, or be completely restructured without notice.
>
> **This repository will be ready for use only after the [SRI 1.5.0 release](https://github.com/stratum-mining/stratum) when the migration process is complete.**
>
> For stable, usable applications, please use the main [SRI repository](https://github.com/stratum-mining/stratum) instead.

## 💼 Table of Contents

<p align="center">
  <a href="#-introduction">Introduction</a> •
  <a href="#-applications">Applications</a> •
  <a href="#-contribute">Contribute</a> •
  <a href="#-support">Support</a> •
  <a href="#-donate">Donate</a> •
  <a href="#-supporters">Supporters</a> •
  <a href="#-license">License</a> 
  <a href="#-msrv">MSRV</a>
</p>

## 👋 Introduction

Welcome to the **SV2 Applications** repository, containing alpha-stage Stratum V2 implementations for both pool operators and miners from the [Stratum V2 Reference Implementation (SRI)](https://github.com/stratum-mining/stratum) project.

This repository provides complete application suite implementing the [Stratum V2 protocol](https://stratumprotocol.org), including pool server, job declaration server and client, translator proxy, and testing utilities - delivering enhanced efficiency, security, flexibility and decentralization for Bitcoin mining operations.

## 🏗️ Applications

This repository contains a complete suite of SV2 applications for pool operators, miners, and developers organized in three workspaces:

### 🏊 Pool Applications (`pool-apps/`)
- **Pool Server (`pool/`)** - SV2-compatible mining pool server that communicates with downstream roles and Template Providers
- **Job Declarator Server (`jd-server/`)** - Coordinates job declaration between miners and pools, maintains synchronized mempool

### ⛏️ Miner Applications (`miner-apps/`)
- **Job Declarator Client (`jd-client/`)** - Allows miners to declare custom block templates for decentralized mining
- **Translator Proxy (`translator/`)** - Bridges SV1 miners to SV2 pools, enabling protocol transition
- **Test Utilities (`test-utils/`)** - Mining device simulators for development and testing

### 🧪 Integration Tests (`test/`)
- **End-to-End Tests (`integration-tests/`)** - Comprehensive testing suite validating interoperability between all components

## ⚙️ Development & Building

### Prerequisites
- Rust 1.75.0 or later
- For coverage analysis: `cargo install cargo-tarpaulin`

### Build All Applications
```bash
# Builds pool-apps, miner-apps, and integration-tests workspaces
./scripts/build-all-workspaces.sh
```

### Run Tests and Linting
```bash
# Runs clippy, tests, and formatting across all workspaces
./scripts/clippy-fmt-and-test.sh
```

### Generate Coverage Reports
```bash
# Creates coverage reports for all workspaces
./scripts/coverage-apps.sh
```

### Publish to crates.io (Maintainers)
```bash
# Test publishing workflow
./scripts/publish-apps.sh --dry-run

# Publish all crates
./scripts/publish-apps.sh
```

For detailed development documentation, see [`scripts/README.md`](scripts/README.md).

## 🛣 Roadmap 

Our roadmap is publicly available as part of the broader SRI project, outlining current and future plans. Decisions are made through a consensus-driven approach via dev meetings, Discord, and GitHub.

[View the SRI Roadmap](https://github.com/orgs/stratum-mining/projects/5)

## 💻 Contribute 

We welcome contributions to improve these pool applications! Here's how you can help:

1. **Start small**: Check the [good first issue label](https://github.com/stratum-mining/stratum/labels/good%20first%20issue) in the main SRI repository
2. **Join the community**: Connect with us on [Discord](https://discord.gg/fsEW23wFYs) before starting larger contributions
3. **Open issues**: [Create GitHub issues](https://github.com/stratum-mining/stratum/issues) for bugs, feature requests, or questions
4. **Follow standards**: Ensure code follows Rust best practices and includes appropriate tests

## 🤝 Support

Join our Discord community for technical support, discussions, and collaboration:

[Join the Stratum V2 Discord Community](https://discord.gg/fsEW23wFYs)

For detailed documentation and guides, visit:
[Stratum V2 Documentation](https://stratumprotocol.org)

## 🎁 Donate

### 👤 Individual Donations 
Support the development of Stratum V2 and these pool applications through OpenSats:

[Donate through OpenSats](https://opensats.org/projects/stratumv2)

### 🏢 Corporate Donations
For corporate support and grants, contact us directly:

Email: stratumv2@gmail.com

## 🙏 Supporters

SRI contributors are independently supported by these organizations:

<p float="left">
  <a href="https://hrf.org"><img src="https://raw.githubusercontent.com/stratum-mining/stratumprotocol.org/refs/heads/main/public/assets/hrf-logo-boxed.svg" width="250" /></a>
  <a href="https://spiral.xyz"><img src="https://raw.githubusercontent.com/stratum-mining/stratumprotocol.org/refs/heads/main/public/assets/Spiral-logo-boxed.svg" width="250" /></a>
  <a href="https://opensats.org/"><img src="https://raw.githubusercontent.com/stratum-mining/stratumprotocol.org/refs/heads/main/public/assets/opensats-logo-boxed.svg" width="250" /></a>
  <a href="https://vinteum.org/"><img src="https://raw.githubusercontent.com/stratum-mining/stratumprotocol.org/refs/heads/main/public/assets/vinteum-logo-boxed.png" width="250" /></a>
</p>

## 📖 License
This software is licensed under Apache 2.0 or MIT, at your option.

## 🦀 MSRV
Minimum Supported Rust Version: 1.75.0

---

> Website [stratumprotocol.org](https://www.stratumprotocol.org) &nbsp;&middot;&nbsp;
> Discord [SV2 Discord](https://discord.gg/fsEW23wFYs) &nbsp;&middot;&nbsp;
> Twitter [@Stratumv2](https://twitter.com/StratumV2) &nbsp;&middot;&nbsp;
> Main Repository [stratum-mining/stratum](https://github.com/stratum-mining/stratum)
