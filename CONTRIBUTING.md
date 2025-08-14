<!-- omit in toc -->
# Contributing to SV2 Applications

First off, thanks for taking the time to contribute! ❤️

All types of contributions are encouraged and valued. See the [Table of Contents](#table-of-contents) for different ways to help and details about how this project handles them. Please make sure to read the relevant section before making your contribution. It will make it a lot easier for us maintainers and smooth out the experience for all involved. The community looks forward to your contributions. 🎉

> And if you like the project, but just don't have time to contribute, that's fine. There are other easy ways to support the project and show your appreciation, which we would also be very happy about:
> - Star the project
> - Tweet about it
> - Refer this project in your project's readme
> - Mention the project at local meetups and tell your friends/colleagues

<!-- omit in toc -->
## Table of Contents

- [I Have a Question](#i-have-a-question)
- [What Should I Know Before I Get Started](#what-should-i-know-before-i-get-started)
  - [Important Resources About SV2 Applications](#important-resources-about-sv2-applications)
  - [Project Communications](#project-communications)
- [I Want To Contribute](#i-want-to-contribute)
  - [Project Structure](#project-structure)
  - [Contribution workflow](#contribution-workflow)
  - [Your First Code Contribution](#your-first-code-contribution)
  

## I Have a Question

> If you want to ask a question, we assume that you have read the documentation available at [stratumprotocol.org/docs](https://stratumprotocol.org).

Best way to ask a question is to hop onto our community [Discord](https://discord.com/invite/fsEW23wFYs). Two most suitable places to post a question are:
- #newbies-qs and
- #dev, for technical questions, suitable for developers building on top of SRI, or contributing to it.

If you then still feel the need to ask a question and need clarification, we recommend the following:

- Open an [Issue](https://github.com/stratum-mining/sv2-apps/issues/new).
- Provide as much context as you can about what you're running into.
  
We will then take care of the issue as soon as possible.

## What Should I Know Before I Get Started

### Important Resources About SV2 Applications

In order to have a better overview about what SV2 Applications covers, have a look at the following resources before getting started with contributions.

  - Stratum V2 Protocol [Specifications](https://github.com/stratum-mining/sv2-spec). 
    - Studying SV2 specs can take some time and requires effort, but it's the best way to properly understand what SV2 is about and how it's composed.
  - SRI [Getting-started](https://stratumprotocol.org/getting-started/) guide.
    - This can be explored in the meantime of SV2 protocol study, so that it can help getting a general overview about SV2. Moreover, it's the best way to really understand how the broader SRI project is built on.
  - Main [SRI Repository](https://github.com/stratum-mining/stratum).
    - This is the core Stratum V2 Reference Implementation containing the protocol libraries and primitives that these applications depend on.
  - Stratum V2 [Master Degree Thesis](https://github.com/GitGab19/Stratum-V2-Master-Degree-Thesis) (by [@gitgab19](https://github.com/GitGab19/)).
    - This resource can be useful to get some knowledge about Bitcoin mining, pooled mining protocols history, and Stratum V2.  
  - Stratum V2 Explained - [Videos Playlist](https://www.youtube.com/playlist?list=PLZXAi8dsUIn0GmElOcmqUtgA5psfFIZoO) (by [@plebhash](https://github.com/plebhash)). 
    - This is a series of videos explaining Stratum V2 in depth, which cover the aforementioned topics.

### Project Communications

Most project communications happen in our [Discord](https://discord.gg/fsEW23wFYs) server. Communications related to general development typically happen under [dev](https://discord.com/channels/950687892169195530/958814900770205739) channel.

Discussion about specific codebase work happens in GitHub [issues](https://github.com/stratum-mining/sv2-apps/issues/) and on [pull requests](https://github.com/stratum-mining/sv2-apps/pulls/). For protocol-level discussions, see the main [SRI repository](https://github.com/stratum-mining/stratum).

Our dev calls are scheduled every Tuesday at 18.00 CET. You can see them in the sidebar under Events on Discord and subscribe to them to be notified.

## I Want To Contribute
> When contributing to this project, you must agree that you have authored 100% of the content, that you have the necessary rights to the content and that the content you contribute may be provided under the project license.

### Project Structure
This repository is part of the broader SRI ecosystem. You can contribute to different aspects of Stratum V2:
  - [SV2 Applications](https://github.com/stratum-mining/sv2-apps) (this repository)
    - This repo contains production-ready pool and miner applications built on top of SRI, organized in workspaces:
      - `pool-apps/` - Pool server and Job Declarator Server for pool operators
      - `miner-apps/` - Job Declarator Client, Translator Proxy, and test utilities for miners
      - `test/integration-tests/` - End-to-end testing suite
  - [Stratum V2 Reference Implementation](https://github.com/stratum-mining/stratum)
    - This repo contains the core SV2 protocol implementation, libraries, and primitives written in Rust.
  - [Stratum V2 Specifications](https://github.com/stratum-mining/sv2-specs)
    - This repo contains the entire SV2 protocol specifications.
  - [SRI website - stratumprotocol.org](https://github.com/stratum-mining/stratumprotocol.org)
    - This repo manages our website, containing docs, specs, and getting-started guides.

### Contribution workflow

The SRI project follows an open contributor model, where anyone is welcome to contribute through reviews, documentation, testing, and patches. Follow these steps to contribute:

1. **Fork the Repository**

2. **Create a Branch** 

3. **Commit Your Changes**
    
    **Note:** Commits should cover both the issue fixed and the solution's rationale. These [guidelines](https://chris.beams.io/posts/git-commit/) should be kept in mind.

4. **Run Tests, Clippy, and Formatter:** 

    For this repository, you can use the convenient scripts to run tests across all workspaces:

    `./scripts/build-all-workspaces.sh`: builds all workspaces (pool-apps, miner-apps, integration-tests)

    `./scripts/clippy-fmt-and-test.sh`: runs clippy, tests, and formatting across all workspaces

    Or run commands individually in each workspace:
    - `cargo test`: runs the project's test suite. Ensure that all tests pass without errors.
    - `cargo clippy`: linter tool for detecting common mistakes and style issues. Address any warnings or errors reported by Clippy.
    - `cargo fmt`: formats your code according to the project's style guidelines.

5. **Submit a Pull Request:** once you're satisfied with your changes, submit a pull request to this repository. Provide a clear and concise description of the changes you've made. If your pull request addresses an existing issue, reference the issue number in the description. Every PR must be opened against the `main` branch.

6. **Review and Iterate** 

7. **Merge and Close:** Once your pull request has been approved and all discussions have been resolved, a project maintainer will merge your changes into the `main` branch. Your contribution will then be officially part of the project. The pull request will be closed, marking the completion of your contribution.

### Your First Code Contribution
>In order to contribute, a basic learning about git and github is needed. If you're not familiar with them, have a look at https://docs.github.com/en/get-started/start-your-journey/git-and-github-learning-resources to dig into and learn how to use them.

Unsure where to begin contributing to SV2 Applications? You can start by looking through `good first issue` and `help wanted` issues:

* [Good first issue](https://github.com/stratum-mining/sv2-apps/issues?q=is%3Aopen+is%3Aissue+label%3A%22good+first+issue%22) are issues which should only require a few lines of code, and a test or two.
* [Help wanted](https://github.com/stratum-mining/sv2-apps/issues?q=is%3Aopen+is%3Aissue+label%3A%22help+wanted%22) - issues which should be a bit more involved than `good first issue` issues.

For protocol-level contributions, also check the main SRI repository:
* [SRI Good first issue](https://github.com/stratum-mining/stratum/issues?q=is%3Aopen+is%3Aissue+label%3A%22good+first+issue%22)
* [SRI Help wanted](https://github.com/stratum-mining/stratum/issues?q=is%3Aopen+is%3Aissue+label%3A%22help+wanted%22)

Another way to better understand where to focus your contribution is by looking at our roadmap: https://github.com/orgs/stratum-mining/projects/5
