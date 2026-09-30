//! Gravity node assembly: the Ethereum node with [`GravityEvmConfig`].

use crate::{
    EthereumConsensusBuilder, EthereumNetworkBuilder, EthereumNode, EthereumPayloadBuilder,
    EthereumPoolBuilder,
};
use alloy_consensus::Header;
use alloy_primitives::{Address, B256, U256};
use reth_chainspec::ChainSpec;
use reth_ethereum_engine_primitives::{EthBuiltPayload, EthPayloadAttributes};
use reth_ethereum_primitives::{EthPrimitives, Receipt};
use reth_evm_ethereum::{GravityChainReader, GravityEvmConfig};
use reth_node_builder::{
    components::{BasicPayloadServiceBuilder, ComponentsBuilder, ExecutorBuilder},
    node::{FullNodeTypes, NodeTypes},
    BuilderContext,
};
use reth_payload_primitives::PayloadTypes;
use reth_provider::{
    BlockHashReader, HeaderProvider, ProviderResult, ReceiptProvider, StateProviderFactory,
};
use std::{fmt::Debug, sync::Arc};

impl EthereumNode {
    /// Returns a [`ComponentsBuilder`] configured for a Gravity node: the Ethereum components
    /// with [`GravityExecutorBuilder`].
    ///
    /// The node's RPC then re-executes committed blocks with Gravity's rules. The pipe keeps
    /// executing blocks with its own Ethereum configuration.
    pub fn gravity_components<Node>() -> ComponentsBuilder<
        Node,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        GravityExecutorBuilder,
        EthereumConsensusBuilder,
    >
    where
        Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
        <Node::Types as NodeTypes>::Payload:
            PayloadTypes<BuiltPayload = EthBuiltPayload, PayloadAttributes = EthPayloadAttributes>,
    {
        Self::components().executor(GravityExecutorBuilder)
    }
}

/// Builds [`GravityEvmConfig`] over the node's provider.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct GravityExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for GravityExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
{
    type EVM = GravityEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        // The provider shares the in-memory canonical chain with RPC and the engine, so blocks
        // the pipe made canonical are readable before they are persisted.
        let reader = ProviderChainReader(ctx.provider().clone());
        Ok(GravityEvmConfig::new(ctx.chain_spec(), Arc::new(reader)))
    }
}

/// [`GravityChainReader`] over a node provider.
#[derive(Debug, Clone)]
pub struct ProviderChainReader<P>(pub P);

impl<P> GravityChainReader for ProviderChainReader<P>
where
    P: HeaderProvider<Header = Header>
        + BlockHashReader
        + ReceiptProvider<Receipt = Receipt>
        + StateProviderFactory
        + Debug
        + Send
        + Sync,
{
    fn mix_hash_by_number(&self, number: u64) -> ProviderResult<Option<B256>> {
        Ok(self.0.header_by_number(number)?.map(|header| header.mix_hash))
    }

    fn canonical_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        self.0.block_hash(number)
    }

    fn header_timestamp(&self, hash: B256) -> ProviderResult<Option<u64>> {
        Ok(self.0.header(&hash)?.map(|header| header.timestamp))
    }

    fn receipts_by_block_hash(&self, hash: B256) -> ProviderResult<Option<Vec<Receipt>>> {
        self.0.receipts_by_block(hash.into())
    }

    fn storage_after_block(
        &self,
        hash: B256,
        address: Address,
        slot: B256,
    ) -> ProviderResult<Option<U256>> {
        self.0.state_by_block_hash(hash)?.storage(address, slot)
    }
}
