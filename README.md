<h1 align="center">
  <br>
  <a href="https://stratumprotocol.org"><img src="https://github.com/stratum-mining/stratumprotocol.org/blob/660ecc6ccd2eca82d0895cef939f4670adc6d1f4/src/.vuepress/public/assets/stratum-logo%402x.png" alt="SRI" width="200"></a>
  <br>
SV2 Applications
  <br>
</h1>
<h4 align="center">Stratum V2 pool and miner applications from the SRI project 🦀</h4>
<p align="center">
  <a href="https://codecov.io/gh/stratum-mining/sv2-apps">
    <img src="https://codecov.io/gh/stratum-mining/sv2-apps/branch/main/graph/badge.svg" alt="codecov">
  </a>
  <a href="https://twitter.com/intent/follow?screen_name=stratumv2">
    <img src="https://img.shields.io/twitter/follow/stratumv2?style=social" alt="X (formerly Twitter) Follow">
  </a>
</p>

# SV2 Apps Repository

This repository contains the application-level crates, currently in **alpha** stage.
If you're looking for the low-level protocol libraries, check out the [`stratum` repository](https://github.com/stratum-mining/stratum). Those crates provide the foundational protocol implementations used as dependencies by these applications.

## Contents

All crates below belong to a single Cargo workspace rooted at the repository root.

- `bitcoin-core-sv2/` - Library crate that translates Bitcoin Core IPC into Sv2 Template Distribution Protocol
- `pool-apps/` - Pool operator applications
  - `pool/` - SV2-compatible mining pool server that communicates with downstream roles and Template Providers
  - `jd-server/` - Job Declarator Server coordinates job declaration between miners and pools, maintains synchronized mempool
- `miner-apps/` - Miner applications
  - `jd-client/` - Job Declarator Client allows miners to declare custom block templates for decentralized mining
  - `translator/` - Translator Proxy bridges SV1 miners to SV2 pools, enabling protocol transition
- `stratum-apps/` - Shared application utilities
  - Configuration helpers (TOML, coinbase outputs, logging)
  - Network connection utilities (Noise protocol, plain TCP, SV1 connections)
  - Payout identity parsing and coinbase output construction/verification helpers
  - Runtime fallback coordination helpers
  - RPC client implementation
  - Key management utilities
  - Custom synchronization primitives
- `integration-tests/` - End-to-end integration tests validating interoperability between all components


## ⛏️ Getting Started

To get started with the Stratum V2 Reference Implementation (SRI), please follow the detailed setup instructions available on the official website:

[Getting Started with Stratum V2](https://stratumprotocol.org/blog/getting-started/)

This guide provides all the necessary information on prerequisites, installation, and configuration to help you begin using, testing or contributing to SRI.

## 🛣 Roadmap 

Our roadmap is publicly available, outlining current and future plans. Decisions on the roadmap are made through a consensus-driven approach, through participation on dev meetings, Discord or GitHub.

[View the SRI Roadmap](https://github.com/orgs/stratum-mining/projects/15)

## 💻 Contribute 

We welcome contributions to improve our SV2 apps! Here's how you can help:

1. **Start small**: Check the [good first issue label](https://github.com/stratum-mining/sv2-apps/labels/good%20first%20issue) in the main SRI repository
2. **Join the community**: Connect with us on [Discord](https://discord.gg/fsEW23wFYs) before starting larger contributions
3. **Open issues**: [Create GitHub issues](https://github.com/stratum-mining/sv2-apps/issues) for bugs, feature requests, or questions
4. **Follow standards**: Ensure code follows Rust best practices and includes appropriate tests

## 🤝 Support

Join our Discord community for technical support, discussions, and collaboration:

[Join the Stratum V2 Discord Community](https://discord.gg/fsEW23wFYs)

For detailed documentation and guides, visit:
[Stratum V2 Documentation](https://stratumprotocol.org)

## 🎁 Donate

### 👤 Individual Donations 
If you wish to support the development and maintenance of the Stratum V2 Reference Implementation, individual donations are greatly appreciated. You can donate through OpenSats, a 501(c)(3) public charity dedicated to supporting open-source Bitcoin projects.

[Donate through OpenSats](https://opensats.org/projects/stratumv2)

### 🏢 Corporate Donations
For corporate entities interested in providing more substantial support, such as grants to SRI contributors, please get in touch with us directly. Your support can make a significant difference in accelerating development, research, and innovation.

## 🙏 Supporters

SRI contributors are independently, financially supported by following entities: 

<p float="left">
  <a href="https://btrust.tech"><img src="https://raw.githubusercontent.com/stratum-mining/stratumprotocol.org/refs/heads/main/public/assets/btrust-boxed.png" width="250" /></a>
  <a href="https://hrf.org"><img src="https://raw.githubusercontent.com/stratum-mining/stratumprotocol.org/refs/heads/main/public/assets/hrf-logo-boxed.svg" width="250" /></a>
  <a href="https://spiral.xyz"><img src="https://raw.githubusercontent.com/stratum-mining/stratumprotocol.org/refs/heads/main/public/assets/Spiral-logo-boxed.svg" width="250" /></a>
  <a href="https://opensats.org/"><img src="https://raw.githubusercontent.com/stratum-mining/stratumprotocol.org/refs/heads/main/public/assets/opensats-logo-boxed.svg" width="250" /></a>
  <a href="https://vinteum.org/"><img src="https://raw.githubusercontent.com/stratum-mining/stratumprotocol.org/refs/heads/main/public/assets/vinteum-logo-boxed.png" width="250" /></a>
</p>

## 📖 License
This software is licensed under Apache 2.0 or MIT, at your option.

## 🦀 MSRV
Minimum Supported Rust Version: 1.88.0

---

> Website [stratumprotocol.org](https://www.stratumprotocol.org) &nbsp;&middot;&nbsp;
> Discord [SV2 Discord](https://discord.gg/fsEW23wFYs) &nbsp;&middot;&nbsp;
> Twitter [@Stratumv2](https://twitter.com/StratumV2) &nbsp;&middot;&nbsp;
