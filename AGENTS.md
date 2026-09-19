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

## Bug patching

Whenever patching bugs, always keep me informed about potential side-effect implications on non-trivial aspects of the project functionality (e.g.: scalability, monitorability, new bugs or vulnerabilities).

Whenever writing documentation around bug fixes, always write the comment assuming the reader is simply trying to understand the code as is, not the past history of bugs that existed on that code.

## PR reviews

Whenever helping me review PRs, don't restrict the output to an analysis of the PR. Also help me understand the PR progressively across two axis:

- conceptual (taking issues and other related PRs into consideration)
- commit history

While listing findings, for each finding, give me a draft comment and the file/line where it would be appropriate to drop it. Also mention the finding severity, and whether you believe it's a blocker or not. This is deliberately designed to keep human reviewers on the loop, as opposed to blindly copypasting a huge "clanker review" body of text without ever looking into what each finding means.

## Drafting issues

SRI repositories try to leverage github subissue clustering. When helping humans draft new github issues, always find for issues that might be either adjacent, correlated, duplicate. Also take into consideration umbrella issues that have already been closed.

You always draft github issues under human supervision. Your role here is to help human SRI contributors reason about the issues being reported, not create github noise.

## Ponytail

If the plugin is not already installed into the coding agent harness, make sure to follow [Ponytail](https://ponytail.dev/) rules. But avoid installing it as a plugin, unless explicitly instructed to do so. This is only a repository-wide convention. Also avoid writing comments that reference "ponytail" in a compressed and implicit way, prefer explaining the actual rationale instead.

## clanker-driven smoke tests

Before a release we smoke-test the miner apps against the hosted `pool_sv2` deployment (`75.119.150.111`, pool `:3333`, JDS `:3334`). Sv1 miners use [`minerd`](https://github.com/stratum-mining/cpuminer) (`minerd -a sha256d -t 1`, one process per miner); Sv2 miners use [`sv2-cpu-miner`](https://github.com/plebhash/sv2-cpu-miner). Build `translator_sv2` and `jd_client_sv2` from the release commit; JDC scenarios need a synced local Bitcoin Core node with IPC enabled.

Only do this when explicitly instructed to — it is not a regular development workflow. A session touches the system in exactly two ways: one throwaway workspace directory holding every config, log, and helper file, and the user-space test processes themselves. Both are gone after Teardown.

### Core scenarios

10 miners each, all using `miner-apps/*/config-examples/mainnet/*-hosted-*` configs with a real `user_identity`:

- Translator + 10 Sv1 miners, run once with `aggregate_channels = true` (expect one upstream channel) and once `false` (expect one upstream channel per miner).
- JDC + 10 Sv2 miners pointed at JDC's `listening_address`.
- JDC + a translator whose single `[[upstreams]]` is that JDC (`address`/`port` = JDC `listening_address`, `authority_pubkey` = JDC `authority_public_key`), fed by 10 Sv1 miners. Set the translator's `enable_vardiff = false` here so JDC owns difficulty.

### Running a fleet of `sv2-cpu-miner`

It takes one process per miner and reads a single TOML file, but every field can be overridden per process with a `CPU_MINER__<FIELD>` environment variable. A fleet is therefore one shared config plus per-process overrides of whatever must differ:

```sh
for i in $(seq 1 10); do
  CPU_MINER__USER_IDENTITY="release-test-$i" CPU_MINER__DEVICE_ID="cpu-miner-$i" \
    cpu_miner_sv2 -c config.toml -f "miner-$i.log" &
done
```

`-f`/`--log-file` mirrors stdout into a file (overriding `log_file` in the config), which is what keeps a ten-process run readable afterwards. Run the loop from the workspace. Keep `user_identity` distinct per process: that is the string the monitoring endpoints report per channel, so it is how the miners are told apart.

### Sv2 miner variations

`sv2-cpu-miner` is a generic Sv2 Mining Protocol client, not only a mining device, so its knobs reach server-side paths that ten plain miners never touch. Both JDC and the hosted pool accept standard and extended channels, so these are worth running against each:

- **Many channels on one connection** (`n_extended_channels`, `n_standard_channels`; at least one must be greater than 0): emulates a proxy. Exercises per-channel bookkeeping and group-channel job routing on the server from a single connection. In `/api/v1/clients/{id}/channels` a single client should show every channel.
- **`requires_standard_jobs = true`** (which forces `n_extended_channels = 0`): sets the `REQUIRES_STANDARD_JOBS` flag on `SetupConnection`, so the server must send a per-channel `NewMiningJob` instead of a group-channel `NewExtendedMiningJob`. Nothing else in our testing toolkit reaches that branch.
- **A standard channel against JDC**: JDC opens an *extended* channel upstream even when its first downstream channel is standard, forwarding that downstream's nominal hashrate and max target. Expect the pool to see an extended channel while JDC reports a standard one downstream.
- **`nominal_hashrate_multiplier`**: the miner measures its real hashrate once at startup, scales it by this factor and splits the result across its channels. Over- and under-advertising tests the server's initial target derivation and how fast vardiff converges from a wrong seed. The miner never sends `UpdateChannel`, so it only reacts to `SetTarget`; the advertised figure sets only the starting point, since steady-state difficulty is driven by the server's configured `shares_per_minute`.
- **`single_submit = true`**: each channel stops hashing after submitting its first share. Use it as a fast "is a share accepted at all" check before committing to a long run.
- **`cpu_usage_percent`**: throttle so ten processes don't peg the machine. The startup hashrate measurement runs at this usage, so the advertised hashrate scales with it too.

The miner also handles `SetGroupChannel` regrouping, but no sv2-apps server ever sends that message, so that path cannot be covered here.

### Edge cases

Beyond the happy path, walk the failure modes a real user hits. Roughly in order of how likely they are to burn a release:

- **Upstream failover**: sever the primary pool connection mid-run by interposing a local relay: `socat TCP-LISTEN:13333,fork,reuseaddr TCP:75.119.150.111:3333`, point the primary `[[upstreams]]` at `127.0.0.1:13333`, and kill the `socat` process when it's time to cut the cord (or, for the startup-time variant, just configure the primary at a dead port). The translator should fall back to the next `[[upstreams]]` entry without dropping its Sv1 miners; JDC should walk its own upstream list and, once exhausted, keep mining solo on `coinbase_reward_script`. Only when every option is gone may the process exit, with a clear error.
- **Proxy restart under load**: Ctrl+C the translator/JDC while all 10 miners are attached — expect a graceful shutdown, not a hang or panic. On restart, Sv1 miners re-attach by themselves (`minerd` retries forever); `sv2-cpu-miner` exits when the server closes the connection, so the Sv2 fleet must be relaunched — that is expected, not a bug.
- **Miner churn**: kill a few miners mid-run and start a few new ones. Dead miners' channels should disappear from the monitoring endpoints (allow the cache refresh), late joiners should get a job and submit within seconds, and the survivors should be unaffected.
- **Bad config UX**: point at a wrong port, a wrong `authority_pubkey`, and a malformed TOML file (one run each). Each should fail fast with an error message that names the actual problem — a silent hang on a bad `authority_pubkey` is a blocker.
- **Bitcoin Core outage (JDC)**: stop the local node mid-run and try starting JDC with the node down. Look for a comprehensible error and sane recovery once the node is back, not a crash loop.
- **New-block boundary**: keep a run alive across at least one real mainnet block. A couple of stale rejects right at the prev-hash change are acceptable; shares that stop flowing after it are not.
- **Soak**: leave the full setup running for a few hours. Monitoring stays responsive, memory stays flat, `shares_rejected` stays at zero.

### `min_individual_miner_hashrate` gotcha

The example configs assume ASIC hashrate (e.g. `10 TH/s`), which seeds an initial difficulty near ~8000. CPU miners (~1 MH/s) then submit nothing for ~10 min while vardiff ramps down, so shares look stuck. For CPU testing, lower `min_individual_miner_hashrate` to ~`1_000_000.0` so shares flow within seconds. Sv2 miners self-measure and advertise their hashrate, so they need no such change.

### Verification

Both apps serve the same monitoring routes (translator `:9092`, JDC `:9091`): `/api/v1/server/channels` for upstream channel count and `shares_submitted`/`shares_rejected`, and `/api/v1/clients/{id}/channels` for per-miner shares, plus `/api/v1/sv1/clients` on the translator, which returns 404 on JDC since it has no Sv1 downstreams. Snapshots are cached for `monitoring_cache_refresh_secs` (15 in the examples), so allow that long for a change to show up. Healthy run = channels open, shares accepted, zero rejected.

### Teardown

Stop the fleets with `kill $(jobs -p)` in the shells that launched them, stop any helper processes (`socat`), confirm nothing lingers — `pgrep -f 'minerd|cpu_miner_sv2|socat'` comes back empty (background jobs survive a closed shell) — and delete the workspace with one `rm -r`.
