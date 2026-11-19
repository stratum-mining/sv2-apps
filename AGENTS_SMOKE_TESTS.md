# AGENTS_SMOKE_TESTS.md

Before a release we smoke-test the miner apps against the hosted `pool_sv2` deployment, whose pool and JDS addresses are the ones the hosted config examples below point at. This is the Smoke Tests step of the release process in `RELEASE.md`. Sv1 miners use [`minerd`](https://github.com/stratum-mining/cpuminer) (`minerd -a sha256d -t 1`, one process per miner); Sv2 miners use [`sv2-cpu-miner`](https://github.com/plebhash/sv2-cpu-miner), whose binary is `cpu_miner_sv2` ([#873](https://github.com/stratum-mining/sv2-apps/issues/873) tracks moving it into this repository; once that lands, build it from here). Build `translator_sv2` and `jd_client_sv2` from the commit being released.

Only do this when explicitly instructed to — it is not a regular development workflow. A session touches the system in exactly two ways: one throwaway workspace directory holding every config, log, and helper file, and the user-space test processes themselves. Both are gone after Teardown. The one exception is the local Bitcoin Core node of the JDC scenarios: the human provides it, it lives outside the workspace, and Teardown leaves it running.

Two things come from the human, never from the agent. Every `user_identity` is theirs to provide, one per `[[upstreams]]` entry a run can reach, including the backup the failover case lands on: it is an account on that pool, so ask for it instead of inventing one or keeping the placeholder. And if `minerd`, `socat` or another tool is missing from the host, report it instead of installing it.

## Core scenarios

10 miners each, with configs copied from `miner-apps/*/config-examples/mainnet/` and given the `user_identity` values the human provided:

- Translator (`tproxy-config-hosted-pool-example.toml`) + 10 Sv1 miners, run once with `aggregate_channels = true` (expect one upstream channel) and once `false` (expect one upstream channel per miner).
- JDC + 10 Sv2 miners pointed at JDC's `listening_address`. The miner's shipped `config.toml` opens 2 extended and 2 standard channels per process, so expect 40 downstream channels, 4 under each client. Run once per template source:
  - `jdc-config-bitcoin-core-ipc-hosted-infra-example.toml`: templates from a synced local Bitcoin Core node over IPC. This is the setup most miners are expected to run.
  - `jdc-config-hosted-infra-example.toml`: templates from the hosted Sv2 Template Provider, no local node needed.
- JDC + a translator pointed at that JDC, fed by 10 Sv1 miners. Start the translator from `tproxy-config-local-jdc-example.toml`, whose single `[[upstreams]]` is already the local JDC, and set `enable_vardiff = false` so JDC owns difficulty. Keep that example's `aggregate_channels = false`: each miner then has its own channel on JDC and gets its own target, whereas an aggregated translator shows JDC a single channel, so all 10 miners would share one target. Either JDC config works here; prefer the local Bitcoin Core one.

## Running a fleet of `sv2-cpu-miner`

It takes one process per miner and reads a single TOML file, but every field can be overridden per process with a `CPU_MINER__<FIELD>` environment variable. A fleet is therefore one shared config plus per-process overrides of whatever must differ:

```sh
for i in $(seq 1 10); do
  CPU_MINER__USER_IDENTITY="release-test-$i" CPU_MINER__DEVICE_ID="cpu-miner-$i" \
    cpu_miner_sv2 -c config.toml -f "miner-$i.log" &
  echo $! >> pids
done
```

`-f`/`--log-file` mirrors stdout into a file (overriding `log_file` in the config), which is what keeps a ten-process run readable afterwards. Run the loop from the workspace. Append the PID of every process the session starts (miners, proxies, `socat`) to that `pids` file: an agent harness runs each command in a fresh shell, so the job table of the launching shell is gone by the time Teardown runs. Keep `user_identity` distinct per process: that is the string the monitoring endpoints report per channel, so it is how the miners are told apart.

## Sv2 miner variations

`sv2-cpu-miner` is a generic Sv2 Mining Protocol client, not only a mining device, so its knobs reach server-side paths that ten plain miners never touch. Both JDC and the hosted pool accept standard and extended channels, so these are worth running against each:

- **Many channels on one connection** (`n_extended_channels`, `n_standard_channels`; at least one must be greater than 0): emulates a proxy. Exercises per-channel bookkeeping and group-channel job routing on the server from a single connection. In `/api/v1/clients/{id}/channels` a single client should show every channel.
- **`requires_standard_jobs = true`** (the miner rejects the config unless `n_extended_channels = 0`, and its shipped `config.toml` sets 2): sets the `REQUIRES_STANDARD_JOBS` flag on `SetupConnection`, so the server must send a per-channel `NewMiningJob` instead of a group-channel `NewExtendedMiningJob`. The integration tests already set this flag on both Pool and JDC through a mock downstream; what this adds is a client that actually mines on those jobs.
- **A standard channel against JDC**: JDC opens an *extended* channel upstream even when its first downstream channel is standard, forwarding that downstream's nominal hashrate and max target. Both sides are visible on JDC: expect `/api/v1/server/channels` to list the upstream channel under `extended_channels` while `/api/v1/clients/{id}/channels` lists the downstream one under `standard_channels`.
- **`nominal_hashrate_multiplier`**: the miner measures its real hashrate once at startup, scales it by this factor and splits the result across its channels. Over- and under-advertising tests the server's initial target derivation and how fast vardiff converges from a wrong seed. The miner never sends `UpdateChannel`, so it only reacts to `SetTarget`; the advertised figure sets only the starting point, since steady-state difficulty is driven by the server's configured `shares_per_minute`.
- **`single_submit = true`**: each channel stops hashing after submitting its first share. Use it as a fast "is a share accepted at all" check before committing to a long run.
- **`cpu_usage_percent`**: throttle so ten processes don't peg the machine. The startup hashrate measurement runs at this usage, so the advertised hashrate scales with it too.

The miner also handles `SetGroupChannel` regrouping, but no sv2-apps server ever sends that message, so that path cannot be covered here.

## Edge cases

Beyond the happy path, walk the failure modes a real user hits. Roughly in order of how likely they are to burn a release:

- **Upstream failover**: sever the primary pool connection mid-run by interposing a local relay: `socat TCP-LISTEN:13333,reuseaddr TCP:<pool address>:<pool port>` (the primary `[[upstreams]]` of the config), point that `[[upstreams]]` at `127.0.0.1:13333`, and kill the `socat` process when it's time to cut the cord. The relay serves exactly one connection and exits when it ends, so start it right before the proxy, once per run, and check it is still alive before the kill: a proxy already on its next upstream has not been tested (or, for the startup-time variant, just configure the primary at a dead port). The translator should reconnect to the next `[[upstreams]]` entry; it disconnects its Sv1 miners while doing so, they re-attach by themselves, and `/api/v1/sv1/clients` should list all 10 again after the `minerd` retry pause described under Proxy restart. JDC should walk its own upstream list and, once exhausted, keep mining solo on `coinbase_reward_script`. JDC also disconnects its downstreams on every fallback, and `sv2-cpu-miner` exits when the server closes the connection, so the Sv2 fleet must be relaunched afterwards. Only when every option is gone may the process exit, with a clear error.
- **Proxy restart under load**: send the translator/JDC a `SIGINT` (`kill -INT <pid>`, the scripted Ctrl+C) while all 10 miners are attached — expect a graceful shutdown, not a hang or panic. On restart, Sv1 miners re-attach by themselves: `minerd` retries forever, pausing 30 s between attempts by default (`-R` changes it), so allow that long plus one monitoring cache refresh before `/api/v1/sv1/clients` lists all 10 again; `sv2-cpu-miner` exits when the server closes the connection, so the Sv2 fleet must be relaunched — that is expected, not a bug.
- **Miner churn**: kill a few miners mid-run and start a few new ones. Dead miners' channels should disappear from the monitoring endpoints (allow the cache refresh), late joiners should get a job and submit within seconds, and the survivors should be unaffected.
- **Bad config UX**: point at a wrong port, a wrong `authority_pubkey`, and a malformed TOML file (one run each). In every run the log must name the actual problem, and a silent hang is a blocker. What happens next differs:
  - Malformed TOML: the app fails fast.
  - Wrong port or wrong `authority_pubkey`, translator: this is an upstream that cannot be set up, so it does not fail fast. The translator retries it a few times, logging each failed attempt with the upstream's address, then moves to the next `[[upstreams]]` entry. It exits with a clear error only once every entry has failed, so run this with a single `[[upstreams]]` to see the exit.
  - Wrong port or wrong `authority_pubkey`, JDC: it logs that upstream initialization failed and falls back to solo mining.

  A proxy that fell back still opens channels and gets shares accepted, so a healthy-looking run is not a pass here: confirm in the log which upstream it ended up on.
- **Bitcoin Core outage (JDC)**: stop the local node mid-run and try starting JDC with the node down. Look for a comprehensible error and sane recovery once the node is back, not a crash loop. The node belongs to the human: stop and restart it only with the commands they gave for it, skip this case if they gave none, bring it back up before moving on, and never add it to `pids`, or Teardown would kill it.
- **New-block boundary**: keep a run alive across at least one real mainnet block. A couple of stale rejects right at the prev-hash change are acceptable; shares that stop flowing after it are not.
- **Soak**: leave the full setup running for a few hours. Monitoring stays responsive, memory stays flat, and `shares_rejected_by_reason` shows no reason other than `stale-share`.

## `min_individual_miner_hashrate` gotcha

The example configs assume ASIC hashrate (e.g. `10 TH/s`), and the translator derives each miner's starting difficulty from it. A CPU miner (~1 MH/s) is about seven orders of magnitude slower, so it submits nothing until vardiff has brought the difficulty down, and shares look stuck. For CPU testing, lower `min_individual_miner_hashrate` to ~`1_000_000.0` so shares flow within seconds. Sv2 miners self-measure and advertise their hashrate, so they need no such change.

## Verification

Both apps serve the same monitoring routes on their configured `monitoring_address` (`stratum-apps/src/monitoring/routes.rs` is the authoritative list): `/api/v1/server/channels` for upstream channel count and `shares_submitted`/`shares_rejected`, and `/api/v1/clients/{id}/channels` for per-miner shares, plus `/api/v1/sv1/clients` on the translator, which returns 404 on JDC since it has no Sv1 downstreams. Snapshots are cached for the configured `monitoring_cache_refresh_secs`, so allow that long for a change to show up. For runs straight against the hosted pool there is no local monitoring: read the miner logs, where each share shows up as `SubmitShares.Success` or `SubmitShares.Error`. Healthy run = channels open, shares accepted, and no rejects other than `stale-share` in `shares_rejected_by_reason`. Stale rejects are expected around a new block and must stay a small fraction of `shares_submitted`; a count that keeps growing between blocks is a failure.

## Teardown

From the workspace, stop everything the session started with `kill $(cat pids)`, confirm nothing lingers — `ps -p "$(paste -sd, pids)"` lists no process (background jobs survive a closed shell) — and delete the workspace with one `rm -r`.
