//! Inner-execution node components for both Worker and Primary execution.
//!
//! This module contains the logic for execution.

use super::{WorkerComponents, WorkerTxPool};
use crate::{engine::WorkerNetwork, error::ExecutionError};
use eyre::eyre;
use jsonrpsee::http_client::HttpClient;
use reth::{
    primitives::EthPrimitives,
    rpc::{
        builder::{config::RethRpcServerConfig, RpcModuleBuilder, RpcServerHandle},
        eth::EthApi,
    },
};
use reth_chainspec::ChainSpec;
use reth_db::{
    database_metrics::{DatabaseMetadata, DatabaseMetrics},
    Database,
};
use reth_node_builder::{NodeConfig, RethTransactionPoolConfig};
use reth_provider::{
    providers::BlockchainProvider, BlockIdReader, BlockNumReader, BlockReader,
    CanonStateSubscriptions as _, ChainSpecProvider, ChainStateBlockReader,
    DatabaseProviderFactory, EthStorage, HeaderProvider, ProviderFactory, TransactionVariant,
};
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, TransactionPool, TransactionValidationTaskExecutor,
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tn_batch_builder::BatchBuilder;
use tn_batch_validator::BatchValidator;
use tn_config::Config;
use tn_engine::ExecutorEngine;
use tn_faucet::{FaucetArgs, FaucetRpcExtApiServer as _};
use tn_node_traits::{TNExecution, TelcoinNodeTypes};
use tn_rpc::{TelcoinNetworkRpcExt, TelcoinNetworkRpcExtApiServer};
use tn_types::{
    Address, BatchSender, BatchValidation, BlockBody, ConsensusOutput, EnvKzgSettings, ExecHeader,
    LastCanonicalUpdate, Noticer, SealedBlock, SealedBlockWithSenders, SealedHeader, TaskManager,
    WorkerId, B256, MIN_PROTOCOL_BASE_FEE,
};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tracing::{error, info};

/// Inner type for holding execution layer types.
pub(super) struct ExecutionNodeInner<N>
where
    N: TelcoinNodeTypes,
    N::DB: Database + DatabaseMetrics + DatabaseMetadata + Clone + Unpin + 'static,
{
    /// The [Address] for the authority used as the suggested beneficiary.
    ///
    /// The address refers to the execution layer's address
    /// based on the authority's secp256k1 public key.
    pub(super) address: Address,
    /// The validator node config.
    pub(super) tn_config: Config,
    /// The type that holds all information needed to launch the node's engine.
    ///
    /// The [NodeConfig] is reth-specific and holds many helper functions that
    /// help TN stay in-sync with the Ethereum community.
    pub(super) node_config: NodeConfig<N::ChainSpec>,
    /// Type that fetches data from the database.
    pub(super) blockchain_db: BlockchainProvider<N>,
    /// Provider factory is held by the blockchain db, but there isn't a publicly
    /// available way to get a cloned copy.
    /// TODO: add a method to `BlockchainProvider` in upstream reth
    pub(super) provider_factory: ProviderFactory<N>,
    /// The Evm configuration type.
    pub(super) evm_executor: N::Executor,
    /// The type to configure the EVM for execution.
    pub(super) evm_config: N::EvmConfig,
    /// TODO: temporary solution until upstream reth supports public rpc hooks
    pub(super) opt_faucet_args: Option<FaucetArgs>,
    /// Collection of execution components by worker.
    pub(super) workers: HashMap<WorkerId, WorkerComponents<N>>,
    // TODO: add Pool to self.workers for direct access (tests)
}

impl<N> ExecutionNodeInner<N>
where
    N: TelcoinNodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives, Storage = EthStorage>,
    N::DB: Database + DatabaseMetrics + DatabaseMetadata + Clone + Unpin + 'static,
{
    /// Spawn tasks associated with executing output from consensus.
    ///
    /// The method is consumed by [PrimaryNodeInner::start].
    /// All tasks are spawned with the [ExecutionNodeInner]'s [TaskManager].
    pub(super) async fn start_engine(
        &self,
        from_consensus: broadcast::Receiver<ConsensusOutput>,
        task_manager: &TaskManager,
        rx_shutdown: Noticer,
    ) -> eyre::Result<()> {
        let head = self.node_config.lookup_head(&self.provider_factory)?;

        // TODO: call hooks?

        let parent_header = self.blockchain_db.sealed_header(head.number)?.expect("Failed to retrieve sealed header from head's block number while starting executor engine");

        // spawn execution engine to extend canonical tip
        let tn_engine = ExecutorEngine::new(
            self.blockchain_db.clone(),
            self.evm_config.clone(),
            self.node_config.debug.max_block,
            BroadcastStream::new(from_consensus),
            parent_header,
            rx_shutdown,
        );

        // spawn tn engine
        task_manager.spawn_task("consensus engine", async move {
            let res = tn_engine.await;
            match res {
                Ok(_) => info!(target: "engine", "TN Engine exited gracefully"),
                Err(e) => error!(target: "engine", ?e, "TN Engine error"),
            }
        });

        Ok(())
    }

    /// The worker's RPC, TX pool, and block builder
    pub(super) async fn start_batch_builder(
        &mut self,
        worker_id: WorkerId,
        block_provider_sender: BatchSender,
        task_manager: &TaskManager,
        rx_shutdown: Noticer,
    ) -> eyre::Result<()> {
        let head = self.node_config.lookup_head(&self.provider_factory)?;

        // inspired by reth's default eth tx pool:
        // - `EthereumPoolBuilder::default()`
        // - `components_builder.build_components()`
        // - `pool_builder.build_pool(&ctx)`
        let transaction_pool = {
            let data_dir = self.node_config.datadir();
            let pool_config = self.node_config.txpool.pool_config();
            let blob_store = DiskFileBlobStore::open(data_dir.blobstore(), Default::default())?;
            let validator =
                TransactionValidationTaskExecutor::eth_builder(self.blockchain_db.chain_spec())
                    .with_head_timestamp(head.timestamp)
                    .kzg_settings(EnvKzgSettings::Default)
                    .with_local_transactions_config(pool_config.local_transactions_config.clone())
                    .with_additional_tasks(self.node_config.txpool.additional_validation_tasks)
                    .build_with_tasks(
                        self.blockchain_db.clone(),
                        task_manager.get_spawner(),
                        blob_store.clone(),
                    );

            let transaction_pool =
                reth_transaction_pool::Pool::eth_pool(validator, blob_store, pool_config);

            info!(target: "tn::execution", "Transaction pool initialized");

            /* TODO: replace this functionality to save and load the txn pool on start/stop
               The reth function backup_local_tranractions_task's shutdown param can not be easily created.
               The internal functions are not easy to just copy.
               Basically this interface does not work when using your own TaskManager.  Best solution may be to
               open a PR with Reth to fix this.
            let transactions_path = data_dir.txpool_transactions();
            let transactions_backup_config =
                reth_transaction_pool::maintain::LocalTransactionBackupConfig::with_local_txs_backup(transactions_path);

            // spawn task to backup local transaction pool in case of restarts
            ctx.task_executor().spawn_critical_with_graceful_shutdown_signal(
                "local transactions backup task",
                |shutdown| {
                    reth_transaction_pool::maintain::backup_local_transactions_task(
                        shutdown,
                        transaction_pool.clone(),
                        transactions_backup_config,
                    )
                },
            );
            */

            transaction_pool
        };

        // TODO: WorkerNetwork is basically noop and missing some functionality
        let network = WorkerNetwork::new(self.node_config.chain.clone());
        use reth_transaction_pool::TransactionPoolExt as _;
        let mut tx_pool_latest = transaction_pool.block_info();
        tx_pool_latest.pending_basefee = MIN_PROTOCOL_BASE_FEE;
        tx_pool_latest.last_seen_block_hash = self
            .blockchain_db
            .finalized_block_hash()?
            .unwrap_or_else(|| self.tn_config.chain_spec().sealed_genesis_header().hash());
        tx_pool_latest.last_seen_block_number =
            self.blockchain_db.finalized_block_number()?.unwrap_or_default();
        transaction_pool.set_block_info(tx_pool_latest);

        let tip = match tx_pool_latest.last_seen_block_number {
            // use genesis on startup
            0 => SealedBlockWithSenders::new(
                SealedBlock::new(
                    self.tn_config.chain_spec().sealed_genesis_header(),
                    BlockBody::default(),
                ),
                vec![],
            )
            .ok_or_else(|| eyre!("Failed to create genesis block for starting tx pool"))?,
            // retrieve from database
            _ => self
                .blockchain_db
                .sealed_block_with_senders(
                    tx_pool_latest.last_seen_block_hash.into(),
                    TransactionVariant::NoHash,
                )?
                .ok_or_else(|| {
                    eyre!(
                        "Failed to find sealed block during block builder startup! ({} - {:?}) ",
                        tx_pool_latest.last_seen_block_number,
                        tx_pool_latest.last_seen_block_hash,
                    )
                })?,
        };

        let latest_canon_state = LastCanonicalUpdate {
            tip: tip.block,
            pending_block_base_fee: tx_pool_latest.pending_basefee,
            pending_block_blob_fee: tx_pool_latest.pending_blob_fee,
        };

        let batch_builder = BatchBuilder::new(
            self.blockchain_db.clone(),
            transaction_pool.clone(),
            self.blockchain_db.canonical_state_stream(),
            latest_canon_state,
            block_provider_sender,
            self.address,
            self.tn_config.parameters.max_batch_delay,
        );

        // spawn block builder task
        task_manager.spawn_task("batch builder", async move {
            tokio::select!(
                _ = &rx_shutdown => {
                }
                res = batch_builder => {
                    info!(target: "tn::execution", ?res, "batch builder task exited");
                }
            )
        });

        // spawn RPC
        let tn_execution = Arc::new(TNExecution {});
        let rpc_builder = RpcModuleBuilder::default()
            .with_provider(self.blockchain_db.clone())
            .with_pool(transaction_pool.clone())
            .with_network(network)
            .with_executor(task_manager.get_spawner())
            .with_evm_config(self.evm_config.clone())
            .with_events(self.blockchain_db.clone())
            .with_block_executor(self.evm_executor.clone())
            .with_consensus(tn_execution.clone());

        //.node_configure namespaces
        let modules_config = self.node_config.rpc.transport_rpc_module_config();
        let mut server =
            rpc_builder.build(modules_config, Box::new(EthApi::with_spawner), tn_execution);

        // TODO: rpc hook here
        // server.merge.node_configured(rpc_ext)?;

        // extend TN namespace
        let engine_to_primary = (); // TODO: pass client/server here
        let tn_ext = TelcoinNetworkRpcExt::new(self.blockchain_db.chain_spec(), engine_to_primary);
        if let Err(e) = server.merge_configured(tn_ext.into_rpc()) {
            error!(target: "tn::execution", "Error merging TN rpc module: {e:?}");
        }

        info!(target: "tn::execution", "tn rpc extension successfully merged");

        // extend faucet namespace if included
        if let Some(faucet_args) = self.opt_faucet_args.take() {
            // create extension from CLI args
            match faucet_args
                .create_rpc_extension(self.blockchain_db.clone(), transaction_pool.clone())
            {
                Ok(faucet_ext) => {
                    // add faucet module
                    if let Err(e) = server.merge_configured(faucet_ext.into_rpc()) {
                        error!(target: "faucet", "Error merging faucet rpc module: {e:?}");
                    }

                    info!(target: "tn::execution", "faucet rpc extension successfully merged");
                }
                Err(e) => {
                    error!(target: "faucet", "Error creating faucet rpc module: {e:?}");
                }
            }
        }

        // start the RPC server
        let server_config = self.node_config.rpc.rpc_server_config();
        let rpc_handle = server_config.start(&server).await?;

        // take ownership of worker components
        let components = WorkerComponents::new(rpc_handle, transaction_pool);
        self.workers.insert(worker_id, components);

        Ok(())
    }

    /// Create a new block validator.
    pub(super) fn new_batch_validator(&self) -> Arc<dyn BatchValidation> {
        // batch validator
        Arc::new(BatchValidator::<N>::new(self.blockchain_db.clone()))
    }

    /// Fetch the last executed state from the database.
    ///
    /// This method is called when the primary spawns to retrieve
    /// the last committed sub dag from it's database in the case
    /// of the node restarting.
    ///
    /// This returns the hash of the last executed ConsensusHeader on the consensus chain.
    /// since the execution layer is confirming the last executing block.
    pub(super) fn last_executed_output(&self) -> eyre::Result<B256> {
        // NOTE: The payload_builder only extends canonical tip and sets finalized after
        // entire output is successfully executed. This ensures consistent recovery state.
        //
        // For example: consensus round 8 sends an output with 5 blocks, but only 2 blocks are
        // executed before the node restarts. The provider never finalized the round, so the
        // `finalized_block_number` would point to the last block of round 7. The primary
        // would then re-send consensus output for round 8.
        //
        // recover finalized block's nonce: this is the last subdag index from consensus (round)
        let finalized_block_num =
            self.blockchain_db.database_provider_ro()?.last_finalized_block_number()?.unwrap_or(0);
        let last_round_of_consensus = self
            .blockchain_db
            .database_provider_ro()?
            .header_by_number(finalized_block_num)?
            .map(|opt| opt.parent_beacon_block_root.unwrap_or_default())
            .unwrap_or_else(Default::default);

        Ok(last_round_of_consensus)
    }

    /// Return a vector of the last 'number' executed block headers.
    pub(super) fn last_executed_blocks(&self, number: u64) -> eyre::Result<Vec<ExecHeader>> {
        let finalized_block_num =
            self.blockchain_db.database_provider_ro()?.last_finalized_block_number()?.unwrap_or(0);
        let start_num = finalized_block_num.saturating_sub(number);
        let mut result = Vec::with_capacity(number as usize);
        if start_num < finalized_block_num {
            for block_num in start_num + 1..=finalized_block_num {
                if let Some(header) =
                    self.blockchain_db.database_provider_ro()?.header_by_number(block_num)?
                {
                    result.push(header);
                }
            }
        }

        Ok(result)
    }

    /// Return a vector of the last 'number' executed block headers.
    /// These are the execution blocks finalized after consensus output, i.e. it
    /// skips all the "intermediate" blocks and is just the final block from a consensus output.
    pub(super) fn last_executed_output_blocks(
        &self,
        number: u64,
    ) -> eyre::Result<Vec<SealedHeader>> {
        let finalized_block_num =
            self.blockchain_db.database_provider_ro()?.last_block_number().unwrap_or(0);
        let mut result = Vec::with_capacity(number as usize);
        if number > 0 {
            let mut block_num = finalized_block_num;
            let mut last_nonce;
            if let Some(header) =
                self.blockchain_db.database_provider_ro()?.sealed_header(block_num)?
            {
                last_nonce = header.nonce;
                result.push(header);
            } else {
                return Err(eyre::Error::msg(format!("Unable to read block {block_num}")));
            }
            let mut blocks = 1;
            while blocks < number {
                if block_num == 0 {
                    break;
                }
                block_num -= 1;
                if let Some(header) =
                    self.blockchain_db.database_provider_ro()?.sealed_header(block_num)?
                {
                    if header.nonce != last_nonce {
                        last_nonce = header.nonce;
                        result.push(header);
                        blocks += 1;
                    }
                } else {
                    return Err(eyre::Error::msg(format!("Unable to read block {block_num}")));
                }
            }
        }
        result.reverse();
        Ok(result)
    }

    /// Return an database provider.
    pub(super) fn get_provider(&self) -> BlockchainProvider<N> {
        self.blockchain_db.clone()
    }

    /// Return the node's evm-based block executor
    pub(super) fn get_evm_config(&self) -> N::EvmConfig {
        self.evm_config.clone()
    }

    /// Return the node's evm-based block executor
    pub(super) fn get_batch_executor(&self) -> N::Executor {
        self.evm_executor.clone()
    }

    /// Return a worker's RpcServerHandle if the RpcServer exists.
    pub(super) fn worker_rpc_handle(&self, worker_id: &WorkerId) -> eyre::Result<&RpcServerHandle> {
        let handle = self
            .workers
            .get(worker_id)
            .ok_or(ExecutionError::WorkerNotFound(worker_id.to_owned()))?
            .rpc_handle();
        Ok(handle)
    }

    /// Return a worker's HttpClient if the RpcServer exists.
    pub(super) fn worker_http_client(
        &self,
        worker_id: &WorkerId,
    ) -> eyre::Result<Option<HttpClient>> {
        let handle = self.worker_rpc_handle(worker_id)?.http_client();
        Ok(handle)
    }

    /// Return a worker's transaction pool if it exists.
    pub(super) fn get_worker_transaction_pool(
        &self,
        worker_id: &WorkerId,
    ) -> eyre::Result<WorkerTxPool<N>> {
        let tx_pool = self
            .workers
            .get(worker_id)
            .ok_or(ExecutionError::WorkerNotFound(worker_id.to_owned()))?
            .pool();

        Ok(tx_pool)
    }

    /// Return a worker's local Http address if the RpcServer exists.
    pub(super) fn worker_http_local_address(
        &self,
        worker_id: &WorkerId,
    ) -> eyre::Result<Option<SocketAddr>> {
        let addr = self.worker_rpc_handle(worker_id)?.http_local_addr();
        Ok(addr)
    }
}
