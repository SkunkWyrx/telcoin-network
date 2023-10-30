use std::sync::Arc;

// Copyright (c) Telcoin, LLC
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use async_trait::async_trait;
use execution_lattice_consensus::LatticeConsensusEngineHandle;
use lattice_executor::ExecutionState;
use tn_adapters::NetworkAdapter;
use tn_types::consensus::{BatchAPI, ConsensusOutput};
use tokio::sync::mpsc::Sender;
use tracing::debug;

/// A simple/dumb execution engine.
pub struct SimpleExecutionState {
    tx_transaction_confirmation: Sender<Vec<u8>>,
}

impl SimpleExecutionState {
    pub fn new(tx_transaction_confirmation: Sender<Vec<u8>>) -> Self {
        Self { tx_transaction_confirmation }
    }
}

#[async_trait]
impl ExecutionState for SimpleExecutionState {
    async fn handle_consensus_output(&self, consensus_output: ConsensusOutput) {
        for (_, batches) in consensus_output.batches {
            for batch in batches {
                for transaction in batch.transactions().iter() {
                    if let Err(err) =
                        self.tx_transaction_confirmation.send(transaction.clone()).await
                    {
                        eprintln!("Failed to send txn in SimpleExecutionState: {}", err);
                    }
                }
            }
        }
    }

    async fn last_executed_sub_dag_index(&self) -> u64 {
        0
    }
}

/// Client sender for passing completed certificates to the Execution Engine.
///
/// This is passed to the Node for Primary.start()
pub struct LatticeExecutionState {
    adapter: Arc<NetworkAdapter>,
}

impl LatticeExecutionState {
    pub fn new(adapter: Arc<NetworkAdapter>) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl ExecutionState for LatticeExecutionState {
    async fn handle_consensus_output(&self, consensus_output: ConsensusOutput) {
        let _res = self.adapter.handle_consensus_output(consensus_output).await;
        debug!(target: "consensu::execution_state", ?_res, "send output to adapter: ");
    }

    async fn last_executed_sub_dag_index(&self) -> u64 {
        // TODO: call db to get this value
        //
        // needed to recover
        0
    }
}
