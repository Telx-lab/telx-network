//! Inner-execution node components for both Worker and Primary execution.

use consensus_metrics::metered_channel::Sender;
use futures::{stream_select, StreamExt};
use jsonrpsee::http_client::HttpClient;
use reth::rpc::builder::{config::RethRpcServerConfig, RpcModuleBuilder, RpcServerHandle};
use reth_auto_seal_consensus::AutoSealConsensus;
use reth_beacon_consensus::{
    hooks::{EngineHooks, StaticFileHook},
    BeaconConsensusEngine, EthBeaconConsensus, MIN_BLOCKS_FOR_PIPELINE_RUN,
};
use reth_blockchain_tree::{
    BlockchainTree, BlockchainTreeConfig, BlockchainTreeViewer, ShareableBlockchainTree,
    TreeExternals,
};
use reth_db::{
    database::Database,
    database_metrics::{DatabaseMetadata, DatabaseMetrics},
};
use reth_db_common::init::init_genesis;
use reth_evm::execute::BlockExecutorProvider;
use reth_exex::ExExManagerHandle;
use reth_network::NetworkEvents;
use reth_node_builder::{
    common::WithConfigs,
    components::{NetworkBuilder as _, PayloadServiceBuilder as _, PoolBuilder},
    setup::build_networked_pipeline,
    BuilderContext, NodeConfig,
};
use reth_node_ethereum::{
    node::{EthereumNetworkBuilder, EthereumPayloadBuilder, EthereumPoolBuilder},
    EthEvmConfig,
};
use reth_primitives::{Address, Head};
use reth_provider::{
    providers::{BlockchainProvider, StaticFileProvider},
    BlockIdReader, CanonChainTracker, CanonStateNotificationSender, DatabaseProviderFactory,
    FinalizedBlockReader, HeaderProvider, ProviderFactory, StaticFileProviderFactory as _,
};
use reth_prune::PruneModes;
use reth_rpc_types::engine::ForkchoiceState;
use reth_static_file::StaticFileProducer;
use reth_tasks::TaskExecutor;
use reth_transaction_pool::{noop::NoopTransactionPool, TransactionPool};
use std::{collections::HashMap, sync::Arc};
use tn_batch_maker::{BatchMakerBuilder, MiningMode};
use tn_batch_validator::BatchValidator;
use tn_executor::Executor;
use tn_faucet::{FaucetArgs, FaucetRpcExtApiServer as _};
use tn_types::{Consensus, ConsensusOutput, NewBatch, WorkerId};
use tokio::sync::{broadcast, mpsc::unbounded_channel};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error, info};

use super::{PrimaryNode, TnBuilder};
use crate::{
    engine::{WorkerNetwork, WorkerNode},
    error::ExecutionError,
};

/// Inner type for holding execution layer types.
pub(super) struct ExecutionNodeInner<DB, Evm>
where
    DB: Database + Clone + Unpin + 'static,
    Evm: BlockExecutorProvider + 'static,
{
    /// The [Address] for the authority used as the suggested beneficiary.
    ///
    /// The address refers to the execution layer's address
    /// based on the authority's secp256k1 public key.
    address: Address,
    /// The type that holds all information needed to launch the node's engine.
    ///
    /// The [NodeConfig] is reth-specific and holds many helper functions that
    /// help TN stay in-sync with the Ethereum community.
    node_config: NodeConfig,
    /// Type that fetches data from the database.
    blockchain_db: BlockchainProvider<DB>,
    /// Provider factory is held by the blockchain db, but there isn't a publicly
    /// available way to get a cloned copy.
    /// TODO: add a method to `BlockchainProvider` in upstream reth
    provider_factory: ProviderFactory<DB>,
    /// The Evm configuration type.
    evm: Evm,
    /// Broadcasting channel for canonical state changes.
    canon_state_notification_sender: CanonStateNotificationSender,
    /// The task executor is responsible for executing
    /// and spawning tasks to the runtime.
    ///
    /// This type is owned by the current runtime and facilitates
    /// a convenient way to spawn tasks that shutdown with the runtime.
    task_executor: TaskExecutor,
    /// TODO: temporary solution until upstream reth supports public rpc hooks
    opt_faucet_args: Option<FaucetArgs>,
    /// Collection of execution components by worker.
    workers: HashMap<WorkerId, RpcServerHandle>,
    // TODO: add Pool to self.workers for direct access (tests)
}

impl<DB, Evm> ExecutionNodeInner<DB, Evm>
where
    DB: Database + DatabaseMetadata + DatabaseMetrics + Clone + Unpin + 'static,
    Evm: BlockExecutorProvider + 'static,
{
    /// Create a new instance of `Self`.
    pub(super) fn new(tn_builder: TnBuilder<DB>, evm: Evm) -> eyre::Result<Self> {
        // deconstruct the builder
        let TnBuilder { database, node_config, task_executor, tn_config, opt_faucet_args } =
            tn_builder;

        // resolve the node's datadir
        let datadir = node_config.datadir();

        // Raise the fd limit of the process.
        // Does not do anything on windows.
        let _ = fdlimit::raise_fd_limit();

        let provider_factory = ProviderFactory::new(
            database.clone(),
            Arc::clone(&node_config.chain),
            StaticFileProvider::read_write(datadir.static_files())?,
        )
        .with_static_files_metrics();

        debug!(target: "tn::execution", chain=%node_config.chain.chain, genesis=?node_config.chain.genesis_hash(), "Initializing genesis");

        let genesis_hash = init_genesis(provider_factory.clone())?;

        info!(target: "tn::execution",  ?genesis_hash);
        info!(target: "tn::execution", "\n{}", node_config.chain.display_hardforks());

        let auto_consensus: Arc<dyn Consensus> =
            Arc::new(AutoSealConsensus::new(Arc::clone(&node_config.chain)));

        debug!(target: "tn::cli", "Spawning stages metrics listener task");
        let (sync_metrics_tx, sync_metrics_rx) = unbounded_channel();
        let sync_metrics_listener = reth_stages::MetricsListener::new(sync_metrics_rx);
        task_executor.spawn_critical("stages metrics listener task", sync_metrics_listener);

        // get config from file
        let prune_config = node_config.prune_config(); //.or(reth_config.prune.clone());
        let tree_config = BlockchainTreeConfig::default();
        let tree_externals =
            TreeExternals::new(provider_factory.clone(), auto_consensus.clone(), evm.clone());
        let tree = BlockchainTree::new(
            tree_externals,
            tree_config,
            prune_config.map(|config| config.segments.clone()),
        )?
        .with_sync_metrics_tx(sync_metrics_tx.clone());

        let canon_state_notification_sender = tree.canon_state_notification_sender();
        let blockchain_tree = Arc::new(ShareableBlockchainTree::new(tree));
        debug!(target: "tn::execution", "configured blockchain tree");

        // setup the blockchain provider
        let blockchain_db = BlockchainProvider::new(provider_factory.clone(), blockchain_tree)?;
        let address = *tn_config.execution_address();

        Ok(Self {
            address,
            node_config,
            blockchain_db,
            provider_factory,
            evm,
            canon_state_notification_sender,
            task_executor,
            opt_faucet_args,
            workers: HashMap::default(),
        })
    }

    /// Spawn tasks associated with executing output from consensus.
    ///
    /// The method is consumed by [PrimaryNodeInner::start].
    /// All tasks are spawned with the [ExecutionNodeInner]'s [TaskManager].
    pub(super) async fn start_engine(
        &self,
        from_consensus: broadcast::Receiver<ConsensusOutput>,
    ) -> eyre::Result<()> {
        // TODO: start metrics endpoint - need to update Generics
        //
        // // start metrics endpoint -
        let prometheus_handle = self.node_config.install_prometheus_recorder()?;
        self.node_config
            .start_metrics_endpoint(
                prometheus_handle,
                self.provider_factory.db_ref().clone(),
                self.provider_factory.static_file_provider(),
                self.task_executor.clone(),
            )
            .await?;

        // TODO: both start_engine and start_batch_maker lookup head
        let head = self.node_config.lookup_head(self.provider_factory.clone())?;

        let ctx = BuilderContext::<PrimaryNode<_, _>>::new(
            head,
            self.blockchain_db.clone(),
            self.task_executor.clone(),
            WithConfigs {
                config: self.node_config.clone(),
                toml_config: reth_config::Config::default(),
            },
        );

        // let components_builder = PrimaryNode::<DB, _>::components();
        // let NodeComponents { network, payload_builder, .. } =
        //     components_builder.build_components(&ctx).await?;
        // let pool = EthereumPoolBuilder::default().build_pool(&ctx).await?;
        let pool = NoopTransactionPool::default();
        let network = EthereumNetworkBuilder::default().build_network(&ctx, pool.clone()).await?;
        let payload_builder =
            EthereumPayloadBuilder::default().spawn_payload_service(&ctx, pool.clone()).await?;

        // TODO: call hooks?

        // let network_client = network.fetch_client().await?;

        // TODO: support tip? only max_block should work with NoopNetwork
        // - tip results in an infinite loop
        // let max_block = self.node_config.max_block(&network_client,
        // self.provider_factory.clone()).await?;
        let max_block = self.node_config.debug.max_block;

        // engine channel
        // let (to_engine, from_engine) = unbounded_channel();
        // let beacon_engine_stream = UnboundedReceiverStream::from(from_engine);

        // // build executor
        // let (_, client, mut task) = Executor::new(
        //     Arc::clone(&self.node_config.chain),
        //     self.blockchain_db.clone(),
        //     from_consensus,
        //     to_engine.clone(),
        //     self.canon_state_notification_sender.clone(),
        //     self.evm.clone(),
        // )
        // .build();

        // let reth_config = reth_config::Config::default();
        // let (sync_metrics_tx, _sync_metrics_rx) = unbounded_channel();

        // let auto_consensus: Arc<dyn Consensus> =
        //     Arc::new(AutoSealConsensus::new(self.node_config.chain.clone()));
        // let mut hooks = EngineHooks::new();

        // let static_file_producer =
        //     StaticFileProducer::new(self.provider_factory.clone(), PruneModes::default());

        // // let static_file_producer_events = static_file_producer.lock().events();

        // hooks.add(StaticFileHook::new(
        //     static_file_producer.clone(),
        //     Box::new(self.task_executor.clone()),
        // ));

        // // capture static file events before passing ownership
        // let static_file_producer_events = static_file_producer.lock().events();

        // let pipeline = build_networked_pipeline(
        //     &reth_config.stages,
        //     client.clone(),
        //     Arc::clone(&auto_consensus),
        //     self.provider_factory.clone(),
        //     &self.task_executor,
        //     sync_metrics_tx,
        //     None, // prune.node_config.clone(),
        //     max_block,
        //     static_file_producer,
        //     self.evm.clone(),
        //     ExExManagerHandle::empty(), // TODO: evaluate use for exex manager
        // )
        // .await?;

        // let pipeline_events_for_task = pipeline.events();
        // task.set_pipeline_events(pipeline_events_for_task);

        // // capture pipeline events for events handler
        // // TODO: EventStream<_> doesn't impl Clone yet
        // let pipeline_events_for_events_handler = pipeline.events();

        // let (beacon_consensus_engine, beacon_engine_handle) = BeaconConsensusEngine::with_channel(
        //     client.clone(),
        //     pipeline,
        //     self.blockchain_db.clone(),
        //     Box::new(self.task_executor.clone()),
        //     Box::new(network.clone()),
        //     None, // max block
        //     payload_builder,
        //     None, // initial_target
        //     MIN_BLOCKS_FOR_PIPELINE_RUN,
        //     to_engine,
        //     Box::pin(beacon_engine_stream), // unbounded stream
        //     hooks,
        // )?;

        // // spawn task to execute consensus output
        // self.task_executor.spawn_critical("Execution Engine Task", Box::pin(task));

        // debug!("awaiting beacon engine task...");

        // // spawn beacon engine
        // self.task_executor.spawn_critical_blocking("consensus engine", async move {
        //     let res = beacon_consensus_engine.await;
        //     tracing::error!("beacon consensus engine: {res:?}");
        //     // TODO: return oneshot channel here?
        // });

        // let events = stream_select!(
        //     network.event_listener().map(Into::into),
        //     beacon_engine_handle.event_listener().map(Into::into),
        //     pipeline_events_for_events_handler.map(Into::into),
        //     // pruner_events.map(Into::into),
        //     static_file_producer_events.map(Into::into),
        // );
        // ctx.task_executor().spawn_critical(
        //     "events task",
        //     reth_node_events::node::handle_events(
        //         Some(network),
        //         Some(head.number),
        //         events,
        //         self.provider_factory.db_ref().clone(),
        //     ),
        // );

        // // wait for engine to spawn
        // tokio::task::yield_now().await;

        // // finalize genesis
        // let genesis_hash = self.node_config.chain.genesis_hash();
        // let genesis_state = ForkchoiceState {
        //     head_block_hash: genesis_hash,
        //     finalized_block_hash: genesis_hash,
        //     safe_block_hash: genesis_hash,
        // };

        // debug!("sending forkchoice update");

        // // send forkchoice for genesis to finalize
        // let res = beacon_engine_handle.fork_choice_updated(genesis_state, None).await?;

        // debug!("genesis finalized: {res:?}");

        Ok(())
    }

    pub(super) async fn start_batch_maker(
        &mut self,
        to_worker: Sender<NewBatch>,
        worker_id: WorkerId,
    ) -> eyre::Result<()> {
        // TODO: both start_engine and start_batch_maker lookup head
        let head = self.node_config.lookup_head(self.provider_factory.clone())?;

        let ctx = BuilderContext::<WorkerNode<DB, Evm>>::new(
            head,
            self.blockchain_db.clone(),
            self.task_executor.clone(),
            WithConfigs {
                config: self.node_config.clone(),
                toml_config: reth_config::Config::default(), /* mostly peer / staging configs */
            },
        );

        // default tx pool
        let pool_builder = EthereumPoolBuilder::default();

        // taken from components_builder.build_components();
        let transaction_pool = pool_builder.build_pool(&ctx).await?;
        // TODO: this is basically noop and missing some functionality
        let network = WorkerNetwork::default();

        // TODO: call hooks?

        // let network_client = network.fetch_client().await?;

        // TODO: support tip? only max_block should work with NoopNetwork
        // - tip results in an infinite loop
        // let max_block = self.node_config.max_block(&network_client,
        // self.provider_factory.clone()).await?;

        // let max_block = self.node_config.debug.max_block;

        // build batch maker
        let max_transactions = 10;
        let mining_mode =
            MiningMode::instant(max_transactions, transaction_pool.pending_transactions_listener());
        let task = BatchMakerBuilder::new(
            Arc::clone(&self.node_config.chain),
            self.blockchain_db.clone(),
            transaction_pool.clone(),
            to_worker,
            mining_mode,
            self.address,
            self.evm.clone(),
        )
        .build();

        // spawn batch maker mining task
        self.task_executor.spawn_critical("batch maker", task);

        // let mut hooks = EngineHooks::new();

        // let static_file_producer = StaticFileProducer::new(
        //     provider_factory.clone(),
        //     provider_factory.static_file_provider(),
        //     prune.node_config.clone().unwrap_or_default().segments,
        // );
        // let static_file_producer_events = static_file_producer.lock().events();
        // hooks.add(StaticFileHook::new(static_file_producer.clone(), Box::new(executor.clone())));
        // info!(target: "tn::batch_maker", "StaticFileProducer initialized");

        // TODO: adjust instance ports?
        //
        //.node_config.adjust_instance_ports();
        //

        // spawn RPC
        let rpc_builder = RpcModuleBuilder::default()
            .with_provider(self.blockchain_db.clone())
            .with_pool(transaction_pool.clone())
            .with_network(network)
            .with_executor(self.task_executor.clone())
            .with_evm_config(EthEvmConfig::default()) // TODO: this should come from self
            .with_events(self.blockchain_db.clone());

        //.node_configure namespaces
        let modules_config = self.node_config.rpc.transport_rpc_module_config();
        let mut server = rpc_builder.build(modules_config);

        // TODO: rpc hook here
        // server.merge.node_configured(rpc_ext)?;

        if let Some(faucet_args) = self.opt_faucet_args.take() {
            // create extension from CLI args
            let faucet_ext = faucet_args
                .create_rpc_extension(self.blockchain_db.clone(), transaction_pool.clone())?;

            // add faucet module
            if let Err(e) = server.merge_configured(faucet_ext.into_rpc()) {
                error!(target: "faucet", "Error merging faucet rpc module: {e:?}");
            }

            info!(target: "tn::execution", "faucet rpc extension successfully merged");
        }

        // start the server
        let server_config = self.node_config.rpc.rpc_server_config();
        let rpc_handle = server_config.start(server).await?;

        self.workers.insert(worker_id, rpc_handle);
        Ok(())
    }

    /// Create a new batch validator.
    pub(super) fn new_batch_validator(&self) -> BatchValidator<DB, Evm> {
        // validate batches using beaacon consensus
        // to ensure inner-chain compatibility
        let consensus: Arc<dyn Consensus> =
            Arc::new(EthBeaconConsensus::new(self.node_config.chain.clone()));

        // batch validator
        BatchValidator::<DB, Evm>::new(consensus, self.blockchain_db.clone(), self.evm.clone())
    }

    /// Fetch the last executed stated from the database.
    ///
    /// This method is called when the primary spawns to retrieve
    /// the last committed sub dag from it's database in the case
    /// of the node restarting.
    ///
    /// TODO: there is some consideration about executing batches as blocks
    /// for scalability, but for now all output is one block,
    /// so just use the head's number.
    ///
    /// The primary adds +1 to this value for recovering output
    /// since the execution layer is confirming the last executing block.
    pub(super) async fn last_executed_output(&self) -> eyre::Result<u64> {
        // TODO: this needs to confirm the `ConsensusOutput` was fully executed
        // scenario: output contains 3 blocks, only one block executed before crash
        // outcome: re-execute the entire consensus output?
        // - blockchain tree could keep the last block as part of the canonical tip which would
        //   prevent redundant rewrite to db
        //      - blockchain tree loads last canonical hashes + finalized block number
        //      - consensus output restores last finalized block's nonce (subdag index)
        //      - finalize block is the last call after executing consensus output
        //      - any blocks that are re-executed will already be in the tree and prevent db
        //        rewrites
        //
        // recover finalized block's nonce: this is the last subdag index from consensus
        let last = match self.blockchain_db.finalized_block_number()? {
            Some(num) => {
                self.blockchain_db.header_by_number(num)?.map(|opt| opt.nonce).unwrap_or(0)
            }
            None => 0, // genesis
        };

        Ok(last)
    }

    /// Return an database provider.
    pub(super) fn get_provider(&self) -> BlockchainProvider<DB> {
        self.blockchain_db.clone()
    }

    /// Return a worker's HttpClient if the RpcServer exists.
    pub(super) fn worker_http_client(
        &self,
        worker_id: &WorkerId,
    ) -> eyre::Result<Option<HttpClient>> {
        let handle = self
            .workers
            .get(worker_id)
            .ok_or(ExecutionError::WorkerNotFound(worker_id.to_owned()))?
            .http_client();
        Ok(handle)
    }
}
