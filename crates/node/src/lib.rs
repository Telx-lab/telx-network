// SPDX-License-Identifier: Apache-2.0
//! Library for managing all components used by a full-node in a single process.

use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use crate::{primary::PrimaryNode, worker::WorkerNode};
use consensus_metrics::start_prometheus_server;
use engine::{ExecutionNode, TnBuilder};
use futures::StreamExt;
use reth_db::{
    database_metrics::{DatabaseMetadata, DatabaseMetrics},
    Database,
};
use reth_provider::CanonStateSubscriptions;
use tn_config::{ConsensusConfig, KeyConfig, TelcoinDirs};
use tn_network_libp2p::{
    types::{IdentTopic, NetworkHandle},
    ConsensusNetwork, PeerId,
};
use tn_network_types::network_public_key_to_libp2p;
use tn_node_traits::TelcoinNode;
use tn_primary::{
    network::{PrimaryRequest, PrimaryResponse},
    ConsensusBus, NodeMode,
};
pub use tn_storage::NodeStorage;
use tn_storage::{open_db, tables::ConsensusBlocks, traits::Database as TNDatabase, DatabaseType};
use tn_types::{ConsensusHeader, Multiaddr, TaskManager, WorkerId};
use tn_worker::WorkerNetworkHandle;
use tokio::{runtime::Builder, sync::mpsc};
use tracing::{info, instrument};

pub mod dirs;
pub mod engine;
mod error;
pub mod primary;
pub mod worker;

/// Retrieve the worker's network address by id.
fn worker_address<DB: TNDatabase>(
    id: &WorkerId,
    consensus_config: &ConsensusConfig<DB>,
) -> Multiaddr {
    consensus_config
        .worker_cache()
        .worker(consensus_config.authority().protocol_key(), id)
        .expect("Our public key or worker id is not in the worker cache")
        .worker_address
}

/// Spawn a task to dial a primary peer and to keep trying on failure.
fn dial_primary(
    handle: NetworkHandle<PrimaryRequest, PrimaryResponse>,
    peer_id: PeerId,
    peer_addr: tn_network_libp2p::Multiaddr,
    connected_count: Arc<AtomicU32>,
) {
    tokio::spawn(async move {
        let mut backoff = 1;
        while let Err(e) = handle.dial(peer_id, peer_addr.clone()).await {
            tracing::warn!(target: "telcoin::node", "failed to dial primary {peer_id} at {peer_addr}: {e}");
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            if backoff < 120 {
                backoff += backoff;
            }
        }
        connected_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
}

/// Spawn a task to dial a worker peer and to keep trying on failure.
fn dial_worker(
    handle: WorkerNetworkHandle,
    peer_id: PeerId,
    peer_addr: tn_network_libp2p::Multiaddr,
    connected_count: Arc<AtomicU32>,
) {
    tokio::spawn(async move {
        let mut backoff = 1;
        while let Err(e) = handle.dial(peer_id, peer_addr.clone()).await {
            tracing::warn!(target: "telcoin::node", "failed to dial worker {peer_id} at {peer_addr}: {e}");
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            if backoff < 120 {
                backoff += backoff;
            }
        }
        connected_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
}

/// Inner working of launch_node().
///
/// This will bring up a tokio runtime and start the app within it.
/// It also will shutdown this runtime, potentially violently, to make
/// sure any lefteover tasks are ended.  This allows it to be called more
/// than once per program execution to support changing modes of the
/// running node.
/// If it returns Ok(true) this indicates a mode change occurred and a restart
/// is required.
pub fn launch_node_inner<DB, P>(
    builder: &TnBuilder<DB>,
    tn_datadir: &P,
    db: DatabaseType,
) -> eyre::Result<bool>
where
    DB: Database + DatabaseMetrics + DatabaseMetadata + Clone + Unpin + 'static,
    P: TelcoinDirs + 'static,
{
    // Create a tokio runtime each time this is called.
    // This means we should be starting with a clean slate on a
    // relaunch (old tasks should be gone with relaunch).
    let runtime = Builder::new_multi_thread()
        .thread_name("telcoin-network")
        .enable_io()
        .enable_time()
        .build()
        .expect("failed to build a tokio runtime");

    let res = runtime.block_on(async move {
        if let Some(metrics_socket) = builder.consensus_metrics {
            start_prometheus_server(metrics_socket);
        }

        // config for validator keys
        let config = builder.tn_config.clone();
        let mut task_manager = TaskManager::new("Task Manager");
        let mut engine_task_manager = TaskManager::new("Engine Task Manager");
        let engine = ExecutionNode::<TelcoinNode<DB>>::new(builder, &engine_task_manager)?;

        info!(target: "telcoin::node", "execution engine created");

        let node_storage = NodeStorage::reopen(db.clone());
        tracing::info!(target: "telcoin::cli", "node storage open");
        let key_config = KeyConfig::new(tn_datadir)?;
        let consensus_config = ConsensusConfig::new(config, tn_datadir, node_storage, key_config)?;

        let (worker_id, _worker_info) = consensus_config.config().workers().first_worker()?;
        let worker = WorkerNode::new(*worker_id, consensus_config.clone());
        let (event_stream, rx_event_stream) = mpsc::channel(1000);
        let (worker_event_stream, rx_worker_event_stream) = mpsc::channel(1000);
        let consensus_bus =
            ConsensusBus::new_with_args(consensus_config.config().parameters.gc_depth);
        let consensus_network = ConsensusNetwork::new_for_primary(&consensus_config, event_stream)
            .expect("primry p2p network create failed!");
        let worker_network = ConsensusNetwork::new_for_worker(&consensus_config, worker_event_stream)
                    .expect("worker p2p network create failed!");
        let consensus_network_handle = consensus_network.network_handle();
        let worker_network_handle = worker_network.network_handle();
        let rx_shutdown = consensus_config.shutdown().subscribe();
        task_manager.spawn_task("primary network run loop", async move {
            tokio::select!(
                _ = &rx_shutdown => {
                    Ok(())
                }
                res = consensus_network.run() => {
                    res
                }
            )
        });
        let rx_shutdown = consensus_config.shutdown().subscribe();
        task_manager.spawn_task("worker network run loop", async move {
            tokio::select!(
                _ = &rx_shutdown => {
                    Ok(())
                }
                res = worker_network.run() => {
                   res
                }
            )
        });
        consensus_network_handle.subscribe(IdentTopic::new("tn-primary")).await?;
        let my_authority = consensus_config.authority();
        consensus_network_handle.start_listening(my_authority.primary_network_address().inner()).await?;
        let worker_address = worker_address(worker_id, &consensus_config);
        worker_network_handle.start_listening(worker_address.inner()).await?;
        let worker_network_handle = WorkerNetworkHandle::new(worker_network_handle);
        let peers_connected = Arc::new(AtomicU32::new(0));
        let workers_connected = Arc::new(AtomicU32::new(0));
        for (authority_id, addr, _) in consensus_config.committee().others_primaries_by_id(consensus_config.authority().id()) {
            let peer_id = consensus_config.peer_id_for_authority(&authority_id).expect("missing peer id!");
            dial_primary(consensus_network_handle.clone(), peer_id, addr.inner(), peers_connected.clone());
        }
        for (id, addr) in consensus_config.worker_cache().all_workers() {
            if addr != worker_address {
                let peer_id = network_public_key_to_libp2p(&id);
                dial_worker(worker_network_handle.clone(), peer_id, addr.inner(), workers_connected.clone());
            }
        }
        let quorum = ((consensus_config.committee().size() * 2) / 3) as u32;
        // Wait until we are connected to a quorum of peers (note this assumes we are a validator...).
        while peers_connected.load(Ordering::Relaxed) < quorum || workers_connected.load(Ordering::Relaxed) < quorum {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let primary = PrimaryNode::new(consensus_config.clone(), consensus_bus.clone(), consensus_network_handle, rx_event_stream);

        let mut engine_state = engine.get_provider().await.canonical_state_stream();

        // Prime the recent_blocks watch with latest executed blocks.
        let block_capacity = consensus_bus.recent_blocks().borrow().block_capacity();
        for recent_block in engine.last_executed_output_blocks(block_capacity).await? {
            consensus_bus
                .recent_blocks()
                .send_modify(|blocks| blocks.push_latest(recent_block));
        }

        // Prime the last consensus header from the DB.
        let (_, last_db_block) = db
            .last_record::<ConsensusBlocks>()
            .unwrap_or_else(|| (0, ConsensusHeader::default()));
        consensus_bus.last_consensus_header().send(last_db_block)?;

        if state_sync::can_cvv(
            consensus_bus.clone(),
            consensus_config.clone(),
            primary.network_handle().await,
        )
        .await
        {
            consensus_bus.node_mode().send_modify(|v| *v = NodeMode::CvvActive);
        } else {
            consensus_bus.node_mode().send_modify(|v| *v = NodeMode::CvvInactive);
        }

        // Spawn a task to update the consensus bus with new execution blocks as they are produced.
        let latest_block_shutdown = consensus_config.shutdown().subscribe();
        let consensus_bus_clone = consensus_bus.clone();
        task_manager.spawn_task("latest block", async move {
            loop {
                tokio::select!(
                    _ = &latest_block_shutdown => {
                        break;
                    }
                    latest = engine_state.next() => {
                        if let Some(latest) = latest {
                            consensus_bus_clone.recent_blocks().send_modify(|blocks| blocks.push_latest(latest.tip().block.header.clone()));
                        } else {
                            break;
                        }
                    }
                )
            }
        });


        // create receiving channel before spawning primary to ensure messages are not lost
        let consensus_output_rx = consensus_bus.subscribe_consensus_output();

        // start the primary
        let mut primary_task_manager = primary.start().await?;

        let validator = engine.new_batch_validator().await;
        // start the worker
        let (mut worker_task_manager, block_provider) = worker.start(validator, worker_network_handle, rx_worker_event_stream).await?;

        // start engine
        engine
            .start_engine(
                consensus_output_rx,
                &engine_task_manager,
                consensus_config.shutdown().subscribe(),
            )
            .await?;
        // spawn block maker for worker
        engine
            .start_batch_builder(
                *worker_id,
                block_provider.batches_tx(),
                &engine_task_manager,
                consensus_config.shutdown().subscribe(),
            )
            .await?;

        primary_task_manager.update_tasks();
        task_manager.add_task_manager(primary_task_manager);
        worker_task_manager.update_tasks();
        task_manager.add_task_manager(worker_task_manager);
        engine_task_manager.update_tasks();
        task_manager.add_task_manager(engine_task_manager);

        info!(target:"tn", tasks=?task_manager, "TASKS");

        task_manager.join_until_exit(consensus_config.shutdown().clone()).await;
        let running = consensus_bus.restart();
        consensus_bus.clear_restart();
        info!(target:"tn", "TASKS complete, restart: {running}");
        Ok(running)
    });
    // Kick over the runtime- don't let errant tasks block the Drop.
    runtime.shutdown_background();
    res
}

/// Launch all components for the node.
///
/// Worker, Primary, and Execution.
/// This will possibly "loop" to launch multiple times in response to
/// a nodes mode changes.  This ensures a clean state and fresh tasks
/// when switching modes.
#[instrument(level = "info", skip_all)]
pub fn launch_node<DB, P>(mut builder: TnBuilder<DB>, tn_datadir: P) -> eyre::Result<()>
where
    DB: Database + DatabaseMetadata + DatabaseMetrics + Clone + Unpin + 'static,
    P: TelcoinDirs + 'static,
{
    // adjust rpc instance ports
    builder.node_config.adjust_instance_ports();

    let consensus_db_path = tn_datadir.consensus_db_path();

    tracing::info!(target: "telcoin::cli", "opening node storage at {:?}", consensus_db_path);

    // open storage for consensus
    // In case the DB dir does not yet exist.
    let _ = std::fs::create_dir_all(&consensus_db_path);
    let db = open_db(&consensus_db_path);

    let mut running = true;
    while running {
        running = launch_node_inner(&builder, &tn_datadir, db.clone())?;
    }
    Ok(())
}
