use corepc_node::{Conf, ConnectParams, Node, types::GetBlockchainInfo};
use std::{
    env,
    fs::create_dir_all,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};
use stratum_apps::{
    bitcoin_core_sv2::runtime_api::BitcoinCoreVersion,
    stratum_core::bitcoin::{Address, Amount, Txid},
};
use tracing::warn;

use crate::utils::{
    PROCESS_READY_TIMEOUT, fs_utils, http, tarball, wait_for_listener, wait_for_path,
    with_exclusive_lock,
};

const VERSION_SV2_TP: &str = "1.1.0";
const BITCOIN_CORE_V30X: &str = "30.2";
const BITCOIN_CORE_V31X: &str = "31.0";
/// Allow static signet fixtures to leave IBD without freezing Bitcoin Core's
/// clock, so mined blocks still use wall-clock timestamps.
///
/// Since Bitcoin Core v31, IPC `createNewBlock` waits while IBD is active, so
/// `bitcoin_core_sv2` does not send templates before IBD is over.
/// See https://github.com/bitcoin/bitcoin/issues/33994.
///
/// 100 years is intentionally far above the fixture age so these static chains
/// remain usable without periodic timestamp refreshes. This only relaxes the
/// stale-tip IBD threshold; it does not change Bitcoin Core's clock.
const SIGNET_FIXTURE_MAX_TIP_AGE_SECS: u64 = 100 * 365 * 24 * 60 * 60;

fn get_sv2_tp_filename(os: &str, arch: &str) -> String {
    match (os, arch) {
        ("macos", "aarch64") => {
            format!("sv2-tp-{VERSION_SV2_TP}-arm64-apple-darwin.tar.gz")
        }
        ("macos", "x86_64") => {
            format!("sv2-tp-{VERSION_SV2_TP}-x86_64-apple-darwin.tar.gz")
        }
        ("linux", "x86_64") => format!("sv2-tp-{VERSION_SV2_TP}-x86_64-linux-gnu.tar.gz"),
        ("linux", "aarch64") => format!("sv2-tp-{VERSION_SV2_TP}-aarch64-linux-gnu.tar.gz"),
        _ => format!("sv2-tp-{VERSION_SV2_TP}-x86_64-apple-darwin.tar.gz"),
    }
}

fn get_bitcoin_core_filename(os: &str, arch: &str, bitcoin_core_version: &str) -> String {
    match (os, arch) {
        ("macos", "aarch64") => {
            format!("bitcoin-{bitcoin_core_version}-arm64-apple-darwin.tar.gz")
        }
        ("macos", "x86_64") => {
            format!("bitcoin-{bitcoin_core_version}-x86_64-apple-darwin.tar.gz")
        }
        ("linux", "x86_64") => format!("bitcoin-{bitcoin_core_version}-x86_64-linux-gnu.tar.gz"),
        ("linux", "aarch64") => {
            format!("bitcoin-{bitcoin_core_version}-aarch64-linux-gnu.tar.gz")
        }
        _ => format!("bitcoin-{bitcoin_core_version}-x86_64-apple-darwin.tar.gz"),
    }
}

pub const BITCOIN_CORE_LATEST: BitcoinCoreVersion = BitcoinCoreVersion::V31X;

fn release_version(version: BitcoinCoreVersion) -> &'static str {
    match version {
        BitcoinCoreVersion::V30X => BITCOIN_CORE_V30X,
        BitcoinCoreVersion::V31X => BITCOIN_CORE_V31X,
    }
}

/// Represents the consensus difficulty level of the network.
///
/// Low: regtest mode (every share is a block)
///
/// Mid: signet mode with genesis difficulty
/// (most of the time, a CPU should find a block in a minute or less)
///
/// High: signet mode with premined blocks raising difficulty to 77761.11
/// (most of the time, a CPU should take a REALLY long time to find a block)
///
/// Note: signet mode has signetchallenge=51, which means no signature is needed on the coinbase.
#[derive(PartialEq, Clone)]
pub enum DifficultyLevel {
    Low,
    Mid,
    High,
}

/// Represents a Bitcoin Core node with IPC enabled.
#[derive(Debug)]
pub struct BitcoinCore {
    bitcoind: Node,
    data_dir: PathBuf,
    is_signet: bool,
}

impl BitcoinCore {
    /// Start a new [`BitcoinCore`] instance with IPC enabled for a specific Core major line.
    pub fn start(
        port: u16,
        difficulty_level: DifficultyLevel,
        node_version: BitcoinCoreVersion,
    ) -> Self {
        let current_dir: PathBuf = std::env::current_dir().expect("failed to read current dir");
        let bin_dir = current_dir.join("template-provider");
        if !bin_dir.exists() {
            create_dir_all(&bin_dir).expect("Failed to create bin directory");
        }
        let high_diff_chain_dir = bin_dir.join("high_diff_chain");

        // Use temp dir for Bitcoin datadir to avoid long Unix socket paths in CI
        let data_dir = std::env::temp_dir().join("sv2-integration-tests");

        let mut conf = Conf::default();
        conf.wallet = Some(port.to_string());

        let staticdir = format!(".bitcoin-{port}");
        conf.staticdir = Some(data_dir.join(staticdir.clone()));

        // Remove any datadir left from an earlier test that drew the same port. Ports come from
        // the OS, so reuse is rare but possible, and `BitcoinCore`'s cleanup does not run if a
        // previous run was killed. Without this a stale `node.sock` can satisfy the readiness
        // check below while belonging to a node that is long gone.
        if let Err(e) = std::fs::remove_dir_all(conf.staticdir.as_ref().unwrap())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!("failed to clear stale datadir for port {port}: {e}");
        }

        let max_tip_age_arg = format!("-maxtipage={SIGNET_FIXTURE_MAX_TIP_AGE_SECS}");
        match difficulty_level {
            DifficultyLevel::Low => {
                // use default corepc-node settings, which means regtest mode
                // where every share is a block
            }
            DifficultyLevel::Mid => {
                // use signet mode with genesis difficulty
                // (signetchallenge=51, no signature needed on the coinbase)
                // most of the time, a CPU should find a block in a minute or less
                conf.args = vec![
                    "-signet",
                    "-fallbackfee=0.0001",
                    "-signetchallenge=51",
                    max_tip_age_arg.as_str(),
                ];
                conf.network = "signet";
            }
            DifficultyLevel::High => {
                // use signet mode with premined blocks raising difficulty to 77761.11
                // (signetchallenge=51, no signature needed on the coinbase)
                // most of the time, a CPU should take a REALLY long time to find a block
                conf.args = vec![
                    "-signet",
                    "-fallbackfee=0.0001",
                    "-signetchallenge=51",
                    max_tip_age_arg.as_str(),
                ];
                conf.network = "signet";

                // Create signet datadir
                let signet_datadir = data_dir.join(staticdir.clone()).join("signet");
                create_dir_all(signet_datadir.clone()).expect("Failed to create signet directory");

                // Download and cache high difficulty chain if not exists.
                // Guarded with a flock so concurrent test processes don't race
                // on the unpack.
                if !high_diff_chain_dir.exists() {
                    with_exclusive_lock(
                        &bin_dir.join(".locks").join("high_diff_chain.lock"),
                        || {
                            if !high_diff_chain_dir.exists() {
                                let local_tarball =
                                    current_dir.join("resources").join("high_diff_chain.tar.gz");
                                let tarball_bytes = if local_tarball.exists() {
                                    warn!("Using local high_diff_chain.tar.gz");
                                    tarball::read_from_file(local_tarball.to_str().unwrap())
                                } else {
                                    warn!("Downloading high_diff_chain for the testing session...");
                                    let url = "https://raw.githubusercontent.com/stratum-mining/sv2-apps/eb41b790626fb51ce55e74be8fa0b4f07d4029bf/integration-tests/resources/high_diff_chain.tar.gz";
                                    http::make_get_request(url, 5)
                                };

                                tarball::unpack_path_atomically(
                                    &tarball_bytes,
                                    &bin_dir,
                                    Path::new("high_diff_chain"),
                                    |_| {},
                                );
                            }
                        },
                    );
                }

                // Copy high difficulty signet data into signet datadir
                fs_utils::copy_dir_contents(&high_diff_chain_dir, &signet_datadir)
                    .expect("Failed to copy high difficulty chain data");
            }
        }

        // Download and setup Bitcoin Core with IPC support
        let bitcoin_core_version = release_version(node_version);
        let os = env::consts::OS;
        let arch = env::consts::ARCH;
        let bitcoin_filename = get_bitcoin_core_filename(os, arch, bitcoin_core_version);
        let bitcoin_dirname = format!("bitcoin-{bitcoin_core_version}");
        let bitcoin_home = bin_dir.join(&bitcoin_dirname);
        let bitcoin_node_bin = bitcoin_home.join("libexec").join("bitcoin-node");

        if !bitcoin_node_bin.exists() {
            with_exclusive_lock(
                &bin_dir
                    .join(".locks")
                    .join(format!("bitcoin-{bitcoin_core_version}.lock")),
                || {
                    if !bitcoin_node_bin.exists() {
                        let tarball_bytes = match env::var("BITCOIN_CORE_TARBALL_FILE") {
                            Ok(path) => tarball::read_from_file(&path),
                            Err(_) => {
                                warn!(
                                    "Downloading Bitcoin Core {} for the testing session. This could take a while...",
                                    bitcoin_core_version
                                );
                                let download_endpoint = env::var("BITCOIN_CORE_DOWNLOAD_ENDPOINT")
                        .unwrap_or_else(|_| {
                            format!(
                                "https://bitcoincore.org/bin/bitcoin-core-{bitcoin_core_version}"
                            )
                        });
                                let url = format!("{download_endpoint}/{bitcoin_filename}");
                                http::make_get_request(&url, 5)
                            }
                        };

                        tarball::unpack_path_atomically(
                            &tarball_bytes,
                            &bin_dir,
                            Path::new(&bitcoin_dirname),
                            |staged_home| {
                                let staged_node_bin =
                                    staged_home.join("libexec").join("bitcoin-node");
                                let staged_cli_bin = staged_home.join("bin").join("bitcoin-cli");
                                assert!(
                                    staged_node_bin.exists(),
                                    "Bitcoin Core node binary not found after unpack in {}",
                                    staged_home.display()
                                );

                                // Preserve valid upstream signatures; otherwise sign before
                                // publication so readers only see ready binaries.
                                if os == "macos" {
                                    for bin in [&staged_node_bin, &staged_cli_bin] {
                                        let signature_is_valid = std::process::Command::new(
                                            "codesign",
                                        )
                                        .arg("--verify")
                                        .arg(bin)
                                        .output()
                                        .expect("Failed to verify Bitcoin Core binary signature")
                                        .status
                                        .success();
                                        if signature_is_valid {
                                            continue;
                                        }

                                        let output = std::process::Command::new("codesign")
                                            .arg("--force")
                                            .arg("--sign")
                                            .arg("-")
                                            .arg(bin)
                                            .output()
                                            .expect("Failed to sign Bitcoin Core binary");
                                        assert!(
                                            output.status.success(),
                                            "Failed to sign Bitcoin Core binary {}: {}",
                                            bin.display(),
                                            String::from_utf8_lossy(&output.stderr)
                                        );
                                    }
                                }
                            },
                        );
                    } // inner re-check: bin still missing?
                }, // closure
            ); // with_exclusive_lock
        } // outer exists() guard

        // Add IPC and basic args
        conf.args.extend(vec![
            "-txindex=1",
            "-ipcbind=unix", // Enable IPC for sv2-tp to connect
            "-debug=rpc",
            "-logtimemicros=1",
        ]);

        // Launch bitcoin-node using corepc-node (which will manage the process for us)
        let timeout = std::time::Duration::from_secs(10);
        let current_time = std::time::Instant::now();
        let bitcoind = loop {
            match Node::with_conf(&bitcoin_node_bin, &conf) {
                Ok(bitcoind) => {
                    break bitcoind;
                }
                Err(e) => {
                    if current_time.elapsed() > timeout {
                        panic!("Failed to start bitcoin-node: {e}");
                    }
                    println!("Failed to start bitcoin-node due to {e}");
                }
            }
        };

        let is_signet = conf.network == "signet";
        let data_dir = conf.staticdir.clone().expect("staticdir should be set");

        let core = BitcoinCore {
            bitcoind,
            data_dir,
            is_signet,
        };

        // Wait for Bitcoin Core to create the IPC socket that sv2-tp connects to.
        //
        // This waits for the socket to *appear* and deliberately does not connect to it. Core's
        // libmultiprocess layer sets TCP_NODELAY on every accepted connection; probing with a
        // connect that is immediately dropped makes that setsockopt run against a socket which is
        // already gone, which returns EINVAL on macOS and kills Core's IPC listener with an
        // "Uncaught exception in daemonized task". sv2-tp can then never connect. A capnp RPC
        // endpoint ascribes meaning to a bare connection, so it must not be used as a liveness
        // probe.
        //
        // Waiting on the path alone is sound here because any datadir left over from an earlier
        // test using this port is removed above, so the socket that appears can only be the one
        // this node just created.
        wait_for_path(
            &core.ipc_socket_path(),
            PROCESS_READY_TIMEOUT,
            "Bitcoin Core IPC socket",
        );

        core
    }

    /// Mine `n` blocks.
    pub fn generate_blocks(&self, n: usize) {
        let mining_address = self
            .bitcoind
            .client
            .new_address()
            .expect("Failed to get mining address");
        let generated_blocks = self
            .bitcoind
            .client
            .generate_to_address(n, &mining_address)
            .expect("Failed to generate blocks");
        // Bitcoin Core's generatetoaddress returns Ok(block_hashes) with an array of hashes of the
        // generated blocks.
        assert_eq!(
            generated_blocks.0.len(),
            n,
            "Bitcoin Core generated {} of {n} requested blocks",
            generated_blocks.0.len()
        );
    }

    /// Return the node's RPC info.
    pub fn rpc_info(&self) -> &ConnectParams {
        &self.bitcoind.params
    }

    /// Return the result of `getblockchaininfo` RPC call.
    pub fn get_blockchain_info(&self) -> Result<GetBlockchainInfo, corepc_node::Error> {
        let client = &self.bitcoind.client;
        let blockchain_info = client.get_blockchain_info()?;
        Ok(blockchain_info)
    }

    /// Create and broadcast a transaction to the mempool.
    ///
    /// It is recommended to use [`BitcoinCore::fund_wallet`] before calling this method to
    /// ensure the wallet has enough funds.
    pub fn create_mempool_transaction(&self) -> Result<(Address, Txid), corepc_node::Error> {
        let client = &self.bitcoind.client;
        const MILLION_SATS: Amount = Amount::from_sat(1_000_000);
        let address = client.new_address()?;
        let txid = client
            .send_to_address(&address, MILLION_SATS)?
            .txid()
            .expect("Unexpected behavior: txid is None");
        Ok((address, txid))
    }

    /// Fund the node's wallet.
    ///
    /// This can be useful before using [`BitcoinCore::create_mempool_transaction`].
    pub fn fund_wallet(&self) -> Result<(), corepc_node::Error> {
        let client = &self.bitcoind.client;
        let address = client.new_address()?;
        client.generate_to_address(101, &address)?;
        Ok(())
    }

    /// Return the hash of the most recent block.
    pub fn get_best_block_hash(&self) -> Result<String, corepc_node::Error> {
        let client = &self.bitcoind.client;
        let block_hash = client.get_best_block_hash()?.0;
        Ok(block_hash)
    }

    /// Return the IPC socket path for connecting to this node.
    pub fn ipc_socket_path(&self) -> PathBuf {
        let network_dir = if self.is_signet { "signet" } else { "regtest" };
        self.data_dir.join(network_dir).join("node.sock")
    }

    /// Return the data directory (without network subdirectory).
    pub fn data_dir(&self) -> &PathBuf {
        &self.data_dir
    }

    /// Return whether this node is running on signet.
    pub fn is_signet(&self) -> bool {
        self.is_signet
    }
}

/// Set to keep each node's datadir after the test finishes, for post-mortem inspection of
/// `debug.log`, chainstate and wallet.
///
/// Retained datadirs are not cleaned up by anything else, and a full suite run creates one per
/// node — roughly 1.5 GB — so this is opt-in rather than the default. Left on, successive runs
/// accumulate until the filesystem fills, which on a tmpfs-backed `/tmp` means RAM and on a CI
/// runner means a disk with only a few GB spare.
const KEEP_DATADIR_ENV: &str = "SV2_KEEP_TEST_DATADIR";

impl Drop for BitcoinCore {
    fn drop(&mut self) {
        if env::var_os(KEEP_DATADIR_ENV).is_some() {
            warn!(
                "{KEEP_DATADIR_ENV} set, keeping test datadir {}",
                self.data_dir.display()
            );
            return;
        }

        // The node must be stopped before its files are removed. `Drop` for this struct runs
        // *before* its fields are dropped, so `bitcoind` is still live here; `Node::drop` would
        // otherwise try a graceful RPC shutdown against a datadir that no longer exists.
        let _ = self.bitcoind.stop();

        match std::fs::remove_dir_all(&self.data_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(
                "failed to remove test datadir {}: {e}",
                self.data_dir.display()
            ),
        }
    }
}

/// Represents a template provider using Bitcoin Core with IPC and standalone sv2-tp.
///
/// This implementation launches two separate processes:
/// 1. Bitcoin Core (bitcoin-node) with IPC enabled
/// 2. Standalone sv2-tp binary that connects to Bitcoin Core via IPC
#[derive(Debug)]
pub struct TemplateProvider {
    bitcoin_core: BitcoinCore,
    sv2_tp_process: Child,
    sv2_port: u16,
}

impl TemplateProvider {
    /// Start a new [`TemplateProvider`] instance with Bitcoin Core and standalone sv2-tp.
    pub fn start(port: u16, sv2_interval: u32, difficulty_level: DifficultyLevel) -> Self {
        let bitcoin_core = BitcoinCore::start(port, difficulty_level, BITCOIN_CORE_LATEST);

        let current_dir: PathBuf = std::env::current_dir().expect("failed to read current dir");
        let bin_dir = current_dir.join("template-provider");

        // Download and setup sv2-tp binary
        let os = env::consts::OS;
        let arch = env::consts::ARCH;
        let sv2_tp_filename = get_sv2_tp_filename(os, arch);
        let sv2_tp_dirname = format!("sv2-tp-{VERSION_SV2_TP}");
        let sv2_tp_home = bin_dir.join(&sv2_tp_dirname);
        let sv2_tp_bin = sv2_tp_home.join("bin").join("sv2-tp");

        if !sv2_tp_bin.exists() {
            with_exclusive_lock(
                &bin_dir
                    .join(".locks")
                    .join(format!("sv2-tp-{VERSION_SV2_TP}.lock")),
                || {
                    if !sv2_tp_bin.exists() {
                        let tarball_bytes = match env::var("SV2TP_TARBALL_FILE") {
                            Ok(path) => tarball::read_from_file(&path),
                            Err(_) => {
                                warn!(
                                    "Downloading sv2-tp for the testing session. This could take a while..."
                                );
                                let download_endpoint = env::var("SV2TP_DOWNLOAD_ENDPOINT")
                                    .unwrap_or_else(|_| {
                                        "https://github.com/stratum-mining/sv2-tp/releases/download"
                                            .to_owned()
                                    });
                                let url = format!(
                                    "{download_endpoint}/v{VERSION_SV2_TP}/{sv2_tp_filename}"
                                );
                                http::make_get_request(&url, 5)
                            }
                        };

                        tarball::unpack_path_atomically(
                            &tarball_bytes,
                            &bin_dir,
                            Path::new(&sv2_tp_dirname),
                            |staged_home| {
                                let staged_binary = staged_home.join("bin").join("sv2-tp");
                                assert!(
                                    staged_binary.exists(),
                                    "sv2-tp binary not found after unpack in {}",
                                    staged_home.display()
                                );

                                // Preserve a valid upstream signature; otherwise sign before
                                // publication so readers only see a ready binary.
                                if os == "macos" {
                                    let signature_is_valid = std::process::Command::new("codesign")
                                        .arg("--verify")
                                        .arg(&staged_binary)
                                        .output()
                                        .expect("Failed to verify sv2-tp binary signature")
                                        .status
                                        .success();
                                    if !signature_is_valid {
                                        let output = std::process::Command::new("codesign")
                                            .arg("--force")
                                            .arg("--sign")
                                            .arg("-")
                                            .arg(&staged_binary)
                                            .output()
                                            .expect("Failed to sign sv2-tp binary");
                                        assert!(
                                            output.status.success(),
                                            "Failed to sign sv2-tp binary: {}",
                                            String::from_utf8_lossy(&output.stderr)
                                        );
                                    }
                                }
                            },
                        );
                    } // inner re-check: bin still missing?
                }, // closure
            ); // with_exclusive_lock
        } // outer exists() guard

        // Launch sv2-tp process
        let datadir = bitcoin_core.data_dir();
        let network = if bitcoin_core.is_signet() {
            "-signet"
        } else {
            "-regtest"
        };

        let sv2_tp_process = Command::new(&sv2_tp_bin)
            .arg(network)
            .arg(format!("-datadir={}", datadir.display()))
            .arg(format!("-sv2port={port}"))
            .arg(format!("-templateinterval={sv2_interval}"))
            .arg("-sv2feedelta=0")
            .arg("-debug=sv2")
            .arg("-loglevel=sv2:trace")
            .stdout(Stdio::null()) // Suppress output in tests
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to start sv2-tp process");

        // Wait for sv2-tp to start and accept connections on its Sv2 port. A successful connect is
        // the direct readiness signal, so this returns as soon as sv2-tp is actually serving.
        wait_for_listener(
            SocketAddr::from(([127, 0, 0, 1], port)),
            PROCESS_READY_TIMEOUT,
            "sv2-tp",
        );

        TemplateProvider {
            bitcoin_core,
            sv2_tp_process,
            sv2_port: port,
        }
    }

    /// Mine `n` blocks.
    pub fn generate_blocks(&self, n: usize) {
        self.bitcoin_core.generate_blocks(n);
    }

    /// Return the node's RPC info.
    pub fn rpc_info(&self) -> &ConnectParams {
        self.bitcoin_core.rpc_info()
    }

    /// Return the result of `getblockchaininfo` RPC call.
    pub fn get_blockchain_info(&self) -> Result<GetBlockchainInfo, corepc_node::Error> {
        self.bitcoin_core.get_blockchain_info()
    }

    /// Create and broadcast a transaction to the mempool.
    ///
    /// It is recommended to use [`TemplateProvider::fund_wallet`] before calling this method to
    /// ensure the wallet has enough funds.
    pub fn create_mempool_transaction(&self) -> Result<(Address, Txid), corepc_node::Error> {
        self.bitcoin_core.create_mempool_transaction()
    }

    /// Fund the node's wallet.
    ///
    /// This can be useful before using [`TemplateProvider::create_mempool_transaction`].
    pub fn fund_wallet(&self) -> Result<(), corepc_node::Error> {
        self.bitcoin_core.fund_wallet()
    }

    /// Return the hash of the most recent block.
    pub fn get_best_block_hash(&self) -> Result<String, corepc_node::Error> {
        self.bitcoin_core.get_best_block_hash()
    }

    /// Return the sv2 port that sv2-tp is listening on.
    pub fn sv2_port(&self) -> u16 {
        self.sv2_port
    }

    /// Return the IPC socket path for connecting to the Bitcoin Core node.
    pub fn ipc_socket_path(&self) -> PathBuf {
        self.bitcoin_core.ipc_socket_path()
    }

    /// Return the data directory (without network subdirectory).
    pub fn data_dir(&self) -> &PathBuf {
        self.bitcoin_core.data_dir()
    }

    /// Return whether this node is running on signet.
    pub fn is_signet(&self) -> bool {
        self.bitcoin_core.is_signet()
    }

    /// Return a reference to the inner [`BitcoinCore`] instance.
    pub fn bitcoin_core(&self) -> &BitcoinCore {
        &self.bitcoin_core
    }
}

impl Drop for TemplateProvider {
    fn drop(&mut self) {
        // Kill sv2-tp process first
        let _ = self.sv2_tp_process.kill();
        let _ = self.sv2_tp_process.wait();
        // bitcoin-node is managed by corepc-node::Node and will be cleaned up automatically
    }
}

#[cfg(test)]
mod tests {
    use super::{DifficultyLevel, TemplateProvider};
    use crate::utils::get_available_address;

    #[tokio::test]
    async fn test_create_mempool_transaction() {
        let address = get_available_address();
        let port = address.port();
        let tp = TemplateProvider::start(port, 1, DifficultyLevel::Low);
        assert!(tp.fund_wallet().is_ok());
        assert!(tp.create_mempool_transaction().is_ok());
    }
}
