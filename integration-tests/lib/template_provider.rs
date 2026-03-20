use corepc_node::{types::GetBlockchainInfo, Conf, ConnectParams, Node};
use std::{
    env,
    fs::create_dir_all,
    path::PathBuf,
    process::{Child, Command, Stdio},
};
use stratum_apps::stratum_core::bitcoin::{Address, Amount, Txid};
use tracing::warn;

use crate::utils::{fs_utils, http, tarball};

const VERSION_SV2_TP: &str = "1.0.3";
const VERSION_BITCOIN_CORE: &str = "30.2";

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

fn get_bitcoin_core_filename(os: &str, arch: &str) -> String {
    match (os, arch) {
        ("macos", "aarch64") => {
            format!("bitcoin-{VERSION_BITCOIN_CORE}-arm64-apple-darwin.tar.gz")
        }
        ("macos", "x86_64") => {
            format!("bitcoin-{VERSION_BITCOIN_CORE}-x86_64-apple-darwin.tar.gz")
        }
        ("linux", "x86_64") => format!("bitcoin-{VERSION_BITCOIN_CORE}-x86_64-linux-gnu.tar.gz"),
        ("linux", "aarch64") => format!("bitcoin-{VERSION_BITCOIN_CORE}-aarch64-linux-gnu.tar.gz"),
        _ => format!("bitcoin-{VERSION_BITCOIN_CORE}-x86_64-apple-darwin.tar.gz"),
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
pub enum DifficultyLevel {
    Low,
    Mid,
    High,
}

/// Represents a Bitcoin Core v30.2+ node with IPC enabled.
#[derive(Debug)]
pub struct BitcoinCore {
    bitcoind: Node,
    data_dir: PathBuf,
    is_signet: bool,
}

impl BitcoinCore {
    /// Start a new [`BitcoinCore`] instance with IPC enabled.
    pub fn start(port: u16, difficulty_level: DifficultyLevel) -> Self {
        let current_dir: PathBuf = std::env::current_dir().expect("failed to read current dir");
        let bin_dir = current_dir.join("template-provider");
        // Use temp dir for Bitcoin datadir to avoid long Unix socket paths in CI
        let data_dir = std::env::temp_dir().join("sv2-integration-tests");

        let mut conf = Conf::default();
        conf.wallet = Some(port.to_string());

        let staticdir = format!(".bitcoin-{port}");
        conf.staticdir = Some(data_dir.join(staticdir.clone()));

        match difficulty_level {
            DifficultyLevel::Low => {
                // use default corepc-node settings, which means regtest mode
                // where every share is a block
            }
            DifficultyLevel::Mid => {
                // use signet mode with genesis difficulty
                // (signetchallenge=51, no signature needed on the coinbase)
                // most of the time, a CPU should find a block in a minute or less
                conf.args = vec!["-signet", "-fallbackfee=0.0001", "-signetchallenge=51"];
                conf.network = "signet";
            }
            DifficultyLevel::High => {
                // use signet mode with premined blocks raising difficulty to 77761.11
                // (signetchallenge=51, no signature needed on the coinbase)
                // most of the time, a CPU should take a REALLY long time to find a block
                conf.args = vec!["-signet", "-fallbackfee=0.0001", "-signetchallenge=51"];
                conf.network = "signet";

                // Create signet datadir
                let signet_datadir = data_dir.join(staticdir.clone()).join("signet");
                create_dir_all(signet_datadir.clone()).expect("Failed to create signet directory");

                // Copy high difficulty signet data into signet datadir
                let high_diff_chain_dir = current_dir.join("high_diff_chain");
                fs_utils::copy_dir_contents(&high_diff_chain_dir, &signet_datadir)
                    .expect("Failed to copy high difficulty chain data");
            }
        }

        // Download and setup Bitcoin Core v30.2 with IPC support
        let os = env::consts::OS;
        let arch = env::consts::ARCH;
        let bitcoin_filename = get_bitcoin_core_filename(os, arch);
        let bitcoin_home = bin_dir.join(format!("bitcoin-{VERSION_BITCOIN_CORE}"));
        let bitcoin_node_bin = bitcoin_home.join("libexec").join("bitcoin-node");
        let bitcoin_cli_bin = bitcoin_home.join("bin").join("bitcoin-cli");

        if !bitcoin_node_bin.exists() {
            let tarball_bytes = match env::var("BITCOIN_CORE_TARBALL_FILE") {
                Ok(path) => tarball::read_from_file(&path),
                Err(_) => {
                    warn!("Downloading Bitcoin Core {} for the testing session. This could take a while...", VERSION_BITCOIN_CORE);
                    let download_endpoint = env::var("BITCOIN_CORE_DOWNLOAD_ENDPOINT")
                        .unwrap_or_else(|_| {
                            "https://bitcoincore.org/bin/bitcoin-core-30.2".to_owned()
                        });
                    let url = format!("{download_endpoint}/{bitcoin_filename}");
                    http::make_get_request(&url, 5)
                }
            };

            if let Some(parent) = bitcoin_home.parent() {
                create_dir_all(parent).unwrap();
            }

            tarball::unpack(&tarball_bytes, &bin_dir);

            // Sign the binaries on macOS
            if os == "macos" {
                for bin in &[&bitcoin_node_bin, &bitcoin_cli_bin] {
                    std::process::Command::new("codesign")
                        .arg("--sign")
                        .arg("-")
                        .arg(bin)
                        .output()
                        .expect("Failed to sign Bitcoin Core binary");
                }
            }
        }

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

        // Wait for Bitcoin Core to fully start and create IPC socket
        std::thread::sleep(std::time::Duration::from_secs(2));

        let is_signet = conf.network == "signet";
        let data_dir = conf.staticdir.clone().expect("staticdir should be set");

        BitcoinCore {
            bitcoind,
            data_dir,
            is_signet,
        }
    }

    /// Mine `n` blocks.
    pub fn generate_blocks(&self, n: u64) {
        let mining_address = self
            .bitcoind
            .client
            .new_address()
            .expect("Failed to get mining address");
        self.bitcoind
            .client
            .generate_to_address(n as usize, &mining_address)
            .expect("Failed to generate blocks");
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

/// Represents a template provider using Bitcoin Core v30.2+ with IPC and standalone sv2-tp.
///
/// This implementation launches two separate processes:
/// 1. Bitcoin Core v30.2+ (bitcoin-node) with IPC enabled
/// 2. Standalone sv2-tp binary that connects to Bitcoin Core via IPC
#[derive(Debug)]
pub struct TemplateProvider {
    bitcoin_core: BitcoinCore,
    sv2_tp_process: Child,
    sv2_port: u16,
}

impl TemplateProvider {
    /// Start a new [`TemplateProvider`] instance with Bitcoin Core v30.2+ and standalone sv2-tp.
    pub fn start(port: u16, sv2_interval: u32, difficulty_level: DifficultyLevel) -> Self {
        let bitcoin_core = BitcoinCore::start(port, difficulty_level);

        let current_dir: PathBuf = std::env::current_dir().expect("failed to read current dir");
        let bin_dir = current_dir.join("template-provider");

        // Download and setup sv2-tp binary
        let os = env::consts::OS;
        let arch = env::consts::ARCH;
        let sv2_tp_filename = get_sv2_tp_filename(os, arch);
        let sv2_tp_home = bin_dir.join(format!("sv2-tp-{VERSION_SV2_TP}"));
        let sv2_tp_bin = sv2_tp_home.join("bin").join("sv2-tp");

        if !sv2_tp_bin.exists() {
            let tarball_bytes = match env::var("SV2TP_TARBALL_FILE") {
                Ok(path) => tarball::read_from_file(&path),
                Err(_) => {
                    warn!("Downloading sv2-tp for the testing session. This could take a while...");
                    let download_endpoint =
                        env::var("SV2TP_DOWNLOAD_ENDPOINT").unwrap_or_else(|_| {
                            "https://github.com/stratum-mining/sv2-tp/releases/download".to_owned()
                        });
                    let url = format!("{download_endpoint}/v{VERSION_SV2_TP}/{sv2_tp_filename}");
                    http::make_get_request(&url, 5)
                }
            };

            if let Some(parent) = sv2_tp_home.parent() {
                create_dir_all(parent).unwrap();
            }

            tarball::unpack(&tarball_bytes, &bin_dir);

            // Sign the binary on macOS
            if os == "macos" {
                std::process::Command::new("codesign")
                    .arg("--sign")
                    .arg("-")
                    .arg(&sv2_tp_bin)
                    .output()
                    .expect("Failed to sign sv2-tp binary");
            }
        }

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
            .arg(format!("-sv2port={}", port))
            .arg(format!("-sv2interval={}", sv2_interval))
            .arg("-sv2feedelta=0")
            .arg("-debug=sv2")
            .arg("-loglevel=sv2:trace")
            .stdout(Stdio::null()) // Suppress output in tests
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to start sv2-tp process");

        // Wait for sv2-tp to start and connect to Bitcoin Core
        std::thread::sleep(std::time::Duration::from_secs(3));

        TemplateProvider {
            bitcoin_core,
            sv2_tp_process,
            sv2_port: port,
        }
    }

    /// Mine `n` blocks.
    pub fn generate_blocks(&self, n: u64) {
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
