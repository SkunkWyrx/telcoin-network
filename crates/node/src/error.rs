//! Error types for spawning a full node

use eyre::ErrReport;
use reth_beacon_consensus::BeaconForkChoiceUpdateError;
use reth_provider::ProviderError;
use thiserror::Error;
use tn_executor::SubscriberError;
use tn_types::WorkerId;

#[derive(Debug, Error)]
pub enum NodeError {
    #[error("Failure while booting node: {0}")]
    NodeBootstrapError(#[from] SubscriberError),

    /// Error when creating a new registry
    #[error(transparent)]
    Registry(#[from] prometheus::Error),

    /// Error types when creating the execution layer for node.
    #[error(transparent)]
    Execution(#[from] ExecutionError),
}

/// Error types when spawning the ExecutionNode
#[derive(Debug, Error)]
pub enum ExecutionError {
    /// Error creating temp db
    #[error(transparent)]
    Tempdb(#[from] std::io::Error),

    #[error(transparent)]
    Report(#[from] ErrReport),

    /// Creating blockchain provider
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// Forkchoice updated to genesis when the node spawns.
    #[error(transparent)]
    FinalizeGenesis(#[from] BeaconForkChoiceUpdateError),

    /// Worker id is not included in the execution node's known worker hashmap.
    #[error("Worker not found: {0:?}")]
    WorkerNotFound(WorkerId),
}
