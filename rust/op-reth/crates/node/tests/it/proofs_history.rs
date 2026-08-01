//! Black-box proofs-history restart tests.

use alloy_consensus::BlockHeader;
use alloy_genesis::Genesis;
use futures_util::FutureExt;
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};
use reth_chainspec::EthChainSpec;
use reth_db::{init_db, mdbx::DatabaseArguments};
use reth_e2e_test_utils::{
    node::NodeTestContext, transaction::TransactionTestContext, wallet::Wallet,
};
use reth_node_api::FullNodeComponents;
use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig, NodeHandle};
use reth_node_core::args::{DatadirArgs, RpcServerArgs};
use reth_optimism_chainspec::OpChainSpecBuilder;
use reth_optimism_exex::OpProofsExEx;
use reth_optimism_node::{OpNode, utils::optimism_payload_attributes};
use reth_optimism_rpc::debug::{DebugApiExt, DebugApiOverrideServer};
use reth_optimism_trie::{
    InitializationJob, MdbxProofsStorageV2, OpProofsStorage, RethTrieStorageLayout,
};
use reth_provider::{DatabaseProviderFactory, providers::BlockchainProvider};
use reth_rpc_server_types::{RethRpcModule, RpcModuleSelection};
use reth_tasks::Runtime;
use serde::Deserialize;
use std::{
    env, fs, future,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;

const TEST_NAME: &str = "proofs_history::proofs_history_recovers_durable_gap_on_restart";
const CHILD_MODE_ENV: &str = "OP_RETH_PROOFS_HISTORY_CHILD_MODE";
const CHILD_DATADIR_ENV: &str = "OP_RETH_PROOFS_HISTORY_DATADIR";
const CHILD_PROOFS_PATH_ENV: &str = "OP_RETH_PROOFS_HISTORY_STORAGE_PATH";
const CHILD_SIGNAL_PATH_ENV: &str = "OP_RETH_PROOFS_HISTORY_SIGNAL_PATH";
const PREPARE_MODE: &str = "prepare";
const QUERY_MODE: &str = "query";
const EXECUTION_TIP: u64 = 3;

#[derive(Debug, Deserialize)]
struct ProofsSyncStatus {
    latest: Option<u64>,
}

#[derive(Debug)]
struct ChildNode {
    child: Child,
    logs: Arc<Mutex<Vec<String>>>,
}

impl ChildNode {
    fn spawn(
        mode: &str,
        datadir: &Path,
        proofs_path: &Path,
        signal_path: &Path,
    ) -> eyre::Result<Self> {
        let mut child = Command::new(env::current_exe()?)
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_MODE_ENV, mode)
            .env(CHILD_DATADIR_ENV, datadir)
            .env(CHILD_PROOFS_PATH_ENV, proofs_path)
            .env(CHILD_SIGNAL_PATH_ENV, signal_path)
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr = child.stderr.take().ok_or_else(|| eyre::eyre!("child stderr not piped"))?;
        let logs = Arc::new(Mutex::new(Vec::new()));
        let logs_writer = Arc::clone(&logs);
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                logs_writer.lock().expect("logs lock poisoned").push(line);
            }
        });
        Ok(Self { child, logs })
    }

    fn wait_for_signal(&mut self, path: &Path) -> eyre::Result<String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(value) = fs::read_to_string(path) {
                return Ok(value);
            }
            if let Some(status) = self.child.try_wait()? {
                eyre::bail!(
                    "node exited before signaling readiness: {status}; logs:\n{}",
                    self.logs()
                );
            }
            if Instant::now() >= deadline {
                eyre::bail!("timed out waiting for node readiness signal; logs:\n{}", self.logs());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_success(&mut self) -> eyre::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait()? {
                eyre::ensure!(status.success(), "node failed: {status}; logs:\n{}", self.logs());
                return Ok(());
            }
            if Instant::now() >= deadline {
                eyre::bail!("timed out waiting for node exit; logs:\n{}", self.logs());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn kill(&mut self) -> eyre::Result<()> {
        self.child.kill()?;
        let status = self.child.wait()?;
        eyre::ensure!(!status.success(), "terminated node unexpectedly succeeded");
        Ok(())
    }

    fn logs(&self) -> String {
        self.logs.lock().expect("logs lock poisoned").join("\n")
    }
}

impl Drop for ChildNode {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn child_config() -> eyre::Result<Option<(String, PathBuf, PathBuf, PathBuf)>> {
    match (
        env::var(CHILD_MODE_ENV).ok(),
        env::var_os(CHILD_DATADIR_ENV),
        env::var_os(CHILD_PROOFS_PATH_ENV),
        env::var_os(CHILD_SIGNAL_PATH_ENV),
    ) {
        (Some(mode), Some(datadir), Some(proofs), Some(signal)) => {
            Ok(Some((mode, datadir.into(), proofs.into(), signal.into())))
        }
        (None, None, None, None) => Ok(None),
        _ => Err(eyre::eyre!("proofs-history child environment is incomplete")),
    }
}

fn test_config(
    chain_spec: Arc<reth_optimism_chainspec::OpChainSpec>,
    datadir: &Path,
) -> NodeConfig<reth_optimism_chainspec::OpChainSpec> {
    let rpc = RpcServerArgs::default()
        .with_http()
        .with_http_api(RpcModuleSelection::Standard.append(RethRpcModule::Debug));
    let mut config = NodeConfig::new(chain_spec)
        .with_unused_ports()
        .with_disabled_discovery()
        .with_rpc(rpc)
        .with_datadir_args(DatadirArgs {
            datadir: datadir.to_path_buf().into(),
            ..Default::default()
        });
    config.network.discovery.discv5_port = Some(0);
    config.network.discovery.discv5_port_ipv6 = Some(0);
    config
}

async fn run_child(
    mode: &str,
    datadir: &Path,
    proofs_path: &Path,
    signal_path: &Path,
) -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let genesis: Genesis = serde_json::from_str(include_str!("../assets/genesis.json"))?;
    let chain_spec = Arc::new(
        OpChainSpecBuilder::optimism_sepolia().genesis(genesis).ecotone_activated().build(),
    );
    let config = test_config(chain_spec.clone(), datadir);
    let db_path = config
        .datadir
        .datadir
        .unwrap_or_chain_default(config.chain.chain(), config.datadir.clone())
        .db();
    let db = Arc::new(init_db(db_path, DatabaseArguments::test())?);
    let runtime = Runtime::test();
    let op_node = OpNode::default();

    match mode {
        PREPARE_MODE => {
            let storage: OpProofsStorage<_> =
                Arc::new(MdbxProofsStorageV2::new(proofs_path)?).into();

            let NodeHandle { node, node_exit_future: _ } = NodeBuilder::new(config)
                .with_database(db)
                .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
                .with_components(op_node.components())
                .with_add_ons(op_node.add_ons())
                .launch_with_fn(|builder| {
                    let launcher = EngineNodeLauncher::new(
                        runtime.clone(),
                        builder.config.datadir(),
                        Default::default(),
                    );
                    builder.launch_with(launcher)
                })
                .await?;

            // Initialize proofs with the real genesis trie, then leave it there while execution
            // advances. This creates a valid persisted proofs base for startup catch-up.
            let provider = node.provider.database_provider_ro()?;
            InitializationJob::new(storage, provider.into_tx(), RethTrieStorageLayout::Packed)
                .run(0, chain_spec.genesis_hash())?;

            let wallet = Arc::new(AsyncMutex::new(
                Wallet::default().with_chain_id(chain_spec.chain().into()),
            ));
            let mut node = NodeTestContext::new(node, optimism_payload_attributes).await?;
            let payloads = node
                .advance(EXECUTION_TIP, |_| {
                    let wallet = Arc::clone(&wallet);
                    Box::pin(async move {
                        let mut wallet = wallet.lock().await;
                        let tx = TransactionTestContext::optimism_l1_block_info_tx(
                            wallet.chain_id,
                            wallet.inner.clone(),
                            wallet.inner_nonce,
                        );
                        wallet.inner_nonce += 1;
                        tx.await
                    })
                })
                .await?;
            eyre::ensure!(
                payloads.last().is_some_and(|payload| payload.block().number() == EXECUTION_TIP),
                "unexpected execution tip"
            );

            let done = node
                .inner
                .add_ons_handle
                .engine_shutdown
                .shutdown()
                .ok_or_else(|| eyre::eyre!("engine shutdown already requested"))?;
            done.await?;
            fs::write(signal_path, "ready")?;
        }
        QUERY_MODE => {
            let storage: OpProofsStorage<_> =
                Arc::new(MdbxProofsStorageV2::new(proofs_path)?).into();
            let storage_exec = storage.clone();
            let storage_rpc = storage.clone();
            let NodeHandle { node, node_exit_future: _ } = NodeBuilder::new(config)
                .with_database(db)
                .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
                .with_components(op_node.components())
                .with_add_ons(op_node.add_ons())
                .install_exex("proofs-history", async move |ctx| {
                    Ok(OpProofsExEx::new(ctx, storage_exec).run().boxed())
                })
                .extend_rpc_modules(move |ctx| {
                    let debug = DebugApiExt::new(
                        ctx.node().provider().clone(),
                        ctx.registry.eth_api().clone(),
                        storage_rpc,
                        ctx.node().task_executor().clone(),
                        ctx.node().evm_config().clone(),
                    );
                    ctx.modules.replace_configured(debug.into_rpc())?;
                    Ok(())
                })
                .launch_with_fn(|builder| {
                    let launcher = EngineNodeLauncher::new(
                        runtime.clone(),
                        builder.config.datadir(),
                        Default::default(),
                    );
                    builder.launch_with(launcher)
                })
                .await?;
            let url = node
                .rpc_server_handle()
                .http_url()
                .ok_or_else(|| eyre::eyre!("HTTP RPC server not running"))?;
            fs::write(signal_path, url)?;
            future::pending::<()>().await;
        }
        _ => return Err(eyre::eyre!("unknown child mode: {mode}")),
    }
    Ok(())
}

async fn wait_for_proofs_tip(url: &str, expected: u64) -> eyre::Result<Option<u64>> {
    let client = HttpClientBuilder::default().request_timeout(Duration::from_secs(2)).build(url)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut latest = None;
    let mut last_error = None;
    while Instant::now() < deadline {
        match client.request::<ProofsSyncStatus, _>("debug_proofsSyncStatus", rpc_params![]).await {
            Ok(status) => {
                latest = status.latest;
                if latest == Some(expected) {
                    break;
                }
            }
            Err(err) => last_error = Some(err),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if latest.is_none() &&
        let Some(err) = last_error
    {
        return Err(eyre::eyre!("debug_proofsSyncStatus never succeeded: {err}"));
    }
    Ok(latest)
}

#[tokio::test]
async fn proofs_history_recovers_durable_gap_on_restart() -> eyre::Result<()> {
    if let Some((mode, datadir, proofs, signal)) = child_config()? {
        return run_child(&mode, &datadir, &proofs, &signal).await;
    }

    let temp = TempDir::new()?;
    let datadir = temp.path().join("node");
    let proofs = temp.path().join("proofs");

    // Build a durable execution chain while leaving proofs initialized at genesis. This is the
    // on-disk state left when an unclean shutdown loses a below-threshold proof tail.
    let prepare_signal = temp.path().join("prepared");
    let mut prepare = ChildNode::spawn(PREPARE_MODE, &datadir, &proofs, &prepare_signal)?;
    prepare.wait_for_signal(&prepare_signal)?;
    prepare.wait_for_success()?;

    // Restart with proofs history enabled but do not produce another block. Startup reconciliation
    // must repair the gap without relying on a fresh canonical-chain notification.
    let rpc_signal = temp.path().join("rpc-url");
    let mut restarted = ChildNode::spawn(QUERY_MODE, &datadir, &proofs, &rpc_signal)?;
    let rpc_url = restarted.wait_for_signal(&rpc_signal)?;
    let latest = wait_for_proofs_tip(&rpc_url, EXECUTION_TIP).await?;
    restarted.kill()?;

    assert_eq!(latest, Some(EXECUTION_TIP), "restarted node logs:\n{}", restarted.logs());
    Ok(())
}
