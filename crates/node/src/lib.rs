// Copyright (c) Telcoin, LLC
// SPDX-License-Identifier: Apache-2.0

use crate::{primary::PrimaryNode, worker::WorkerNode};
use engine::{ExecutionNode, TnBuilder};
use futures::{future::try_join_all, stream::FuturesUnordered, StreamExt};
use reth_db::{
    database::Database,
    database_metrics::{DatabaseMetadata, DatabaseMetrics},
};
use reth_provider::CanonStateSubscriptions;
use tn_config::{ConsensusConfig, KeyConfig, TelcoinDirs};
use tn_storage::open_db;
pub use tn_storage::NodeStorage;
use tracing::{info, instrument};

pub mod dirs;
pub mod engine;
mod error;
pub mod metrics;
pub mod primary;
pub mod worker;

/// Launch all components for the node.
///
/// Worker, Primary, and Execution.
#[instrument(level = "info", skip_all)]
pub async fn launch_node<DB, /* Evm, CE, */ P>(
    mut builder: TnBuilder<DB>,
    tn_datadir: P,
) -> eyre::Result<()>
where
    DB: Database + DatabaseMetadata + DatabaseMetrics + Clone + Unpin + 'static,
    P: TelcoinDirs + 'static,
{
    // config for validator keys
    let config = builder.tn_config.clone();
    // adjust rpc instance ports
    builder.node_config.adjust_instance_ports();
    let engine = ExecutionNode::new(builder)?; //XXXX, executor, evm_config)?;

    info!(target: "telcoin::node", "execution engine created");

    let narwhal_db_path = tn_datadir.narwhal_db_path();

    tracing::info!(target: "telcoin::cli", "opening node storage at {:?}", narwhal_db_path);

    // open storage for consensus
    // In case the DB dir does not yet exist.
    let _ = std::fs::create_dir_all(&narwhal_db_path);
    let db = open_db(&narwhal_db_path);
    let node_storage = NodeStorage::reopen(db);
    tracing::info!(target: "telcoin::cli", "node storage open");
    let key_config = KeyConfig::new(&tn_datadir)?;
    let consensus_config = ConsensusConfig::new(config, tn_datadir, node_storage, key_config)?;

    let (worker_id, _worker_info) = consensus_config.config().workers().first_worker()?;
    let worker = WorkerNode::new(*worker_id, consensus_config.clone());
    let primary = PrimaryNode::new(consensus_config.clone());

    let mut engine_state = engine.get_provider().await.canonical_state_stream();
    let eng_bus = primary.consensus_bus().await;

    // Spawn a task to update the consensus bus with new execution blocks as they are produced.
    tokio::spawn(async move {
        while let Some(latest) = engine_state.next().await {
            let latest_num_hash = latest.tip().block.num_hash();
            eng_bus.recent_blocks().send_modify(|blocks| blocks.push_latest(latest_num_hash));
        }
    });

    // used to retrieve the last executed certificate in case of restarts
    let last_executed_consensus_hash =
        engine.last_executed_output().await.expect("execution found HEAD");
    // start the primary
    primary.start(last_executed_consensus_hash).await?;

    // create receiving channel before spawning primary to ensure messages are not lost
    let consensus_output_rx = primary.consensus_bus().await.subscribe_consensus_output();
    // start engine XXXX
    engine.start_engine(consensus_output_rx).await?;

    let validator = engine.new_block_validator().await;
    // start the worker
    let block_provider = worker.start(validator).await?;

    // XXXX spawn batch maker for worker
    engine.start_block_builder(*worker_id, block_provider.blocks_tx()).await?;

    // TODO: use value from CLI
    let terminate_early = false;

    if terminate_early {
        Ok(())
    } else {
        // The pipeline has finished downloading blocks up to `--debug.tip` or
        // `--debug.max-block`. Keep other node components alive for further usage.
        futures::future::pending().await
    }
}
