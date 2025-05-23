//! State required for participation in consensus by the node.

use super::metrics::NodeStateMetrics;
use super::{NodeState, NodeStateStore};
use crate::bus::{command_response::Query, BusClientSender, BusMessage};
use crate::data_availability::DataEvent;
use crate::log_error;
use crate::model::Contract;
use crate::model::{Block, BlockHeight, CommonRunContext, ContractName};
use crate::module_handle_messages;
use crate::utils::conf::SharedConf;
use crate::utils::modules::{module_bus_client, Module};
use anyhow::{Context, Result};
use borsh::{BorshDeserialize, BorshSerialize};
use hyle_model::{TxHash, UnsettledBlobTransaction};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

/// NodeStateModule maintains a NodeState,
/// listens to DA, and sends events when it has processed blocks.
/// Node state module is separate from DataAvailabiliity
/// mostly to run asynchronously.
pub struct NodeStateModule {
    config: SharedConf,
    bus: NodeStateBusClient,
    inner: NodeState,
}

#[derive(Debug, Serialize, Deserialize, Clone, BorshSerialize, BorshDeserialize)]
pub enum NodeStateEvent {
    NewBlock(Box<Block>),
}
impl BusMessage for NodeStateEvent {}

#[derive(Clone)]
pub struct QueryBlockHeight {}

#[derive(Clone)]
pub struct QueryUnsettledTx(pub TxHash);

module_bus_client! {
#[derive(Debug)]
pub struct NodeStateBusClient {
    sender(NodeStateEvent),
    receiver(DataEvent),
    receiver(Query<ContractName, Contract>),
    receiver(Query<QueryBlockHeight , BlockHeight>),
    receiver(Query<QueryUnsettledTx, UnsettledBlobTransaction>),
}
}

impl Module for NodeStateModule {
    type Context = Arc<CommonRunContext>;

    async fn build(ctx: Self::Context) -> Result<Self> {
        let bus = NodeStateBusClient::new_from_bus(ctx.bus.new_handle()).await;

        let api = super::api::api(&ctx).await;
        if let Ok(mut guard) = ctx.router.lock() {
            if let Some(router) = guard.take() {
                guard.replace(router.nest("/v1/", api));
            }
        }
        let metrics = NodeStateMetrics::global(ctx.config.id.clone(), "node_state");

        let store = Self::load_from_disk_or_default::<NodeStateStore>(
            ctx.config.data_directory.join("node_state.bin").as_path(),
        );

        for name in store.contracts.keys() {
            info!("📝 Loaded contract state for {}", name);
        }

        let node_state = NodeState { store, metrics };

        Ok(Self {
            config: ctx.config.clone(),
            bus,
            inner: node_state,
        })
    }

    async fn run(&mut self) -> Result<()> {
        module_handle_messages! {
            on_bus self.bus,
            command_response<QueryBlockHeight, BlockHeight> _ => {
                Ok(self.inner.current_height)
            }
            command_response<ContractName, Contract> cmd => {
                self.inner.contracts.get(cmd).cloned().context("Contract not found")
            }
            command_response<QueryUnsettledTx, UnsettledBlobTransaction> tx_hash => {
                match self.inner.unsettled_transactions.get(&tx_hash.0) {
                    Some(tx) => Ok(tx.clone()),
                    None => Err(anyhow::anyhow!("Transaction not found")),
                }
            }
            listen<DataEvent> block => {
                match block {
                    DataEvent::OrderedSignedBlock(block) => {
                        // TODO: If we are in a broken state, this will likely kill the node every time.
                        let node_state_block = self.inner.handle_signed_block(&block)?;
                        _ = log_error!(self
                            .bus
                            .send(NodeStateEvent::NewBlock(Box::new(node_state_block))), "Sending DataEvent while processing SignedBlock");
                    }
                }
            }
        };

        let _ = log_error!(
            Self::save_on_disk::<NodeStateStore>(
                self.config.data_directory.join("node_state.bin").as_path(),
                &self.inner,
            ),
            "Saving node state"
        );

        Ok(())
    }
}
