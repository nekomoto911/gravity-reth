//! Test doubles for the Gravity execution rules.

use super::GravityChainReader;
use alloc::{collections::BTreeMap, vec::Vec};
use alloy_primitives::{Address, B256, U256};
use reth_ethereum_primitives::Receipt;
use reth_storage_errors::provider::ProviderResult;

/// [`GravityChainReader`] over in-memory maps.
#[derive(Debug, Default, Clone)]
pub(crate) struct MockChainReader {
    mix_hashes: BTreeMap<u64, B256>,
}

impl MockChainReader {
    pub(crate) fn with_mix_hash(mut self, number: u64, mix_hash: B256) -> Self {
        self.mix_hashes.insert(number, mix_hash);
        self
    }
}

impl GravityChainReader for MockChainReader {
    fn mix_hash_by_number(&self, number: u64) -> ProviderResult<Option<B256>> {
        Ok(self.mix_hashes.get(&number).copied())
    }

    fn canonical_hash(&self, _number: u64) -> ProviderResult<Option<B256>> {
        Ok(None)
    }

    fn header_timestamp(&self, _hash: B256) -> ProviderResult<Option<u64>> {
        Ok(None)
    }

    fn receipts_by_block_hash(&self, _hash: B256) -> ProviderResult<Option<Vec<Receipt>>> {
        Ok(None)
    }

    fn storage_after_block(
        &self,
        _hash: B256,
        _address: Address,
        _slot: B256,
    ) -> ProviderResult<Option<U256>> {
        Ok(None)
    }
}

/// Alpha activation time of [`gravity_chain_spec`].
pub(crate) const ALPHA_TIME: u64 = 100;

/// Mainnet with every Ethereum fork through Prague active and Gravity Alpha at [`ALPHA_TIME`].
pub(crate) fn gravity_chain_spec() -> alloc::sync::Arc<reth_chainspec::ChainSpec> {
    use reth_chainspec::{
        ChainHardforks, ChainSpecBuilder, ForkCondition, GravityHardfork, MAINNET,
    };

    let mut spec = ChainSpecBuilder::from(&*MAINNET)
        .shanghai_activated()
        .cancun_activated()
        .prague_activated()
        .build();
    spec.gravity_hardforks =
        ChainHardforks::from([(GravityHardfork::Alpha, ForkCondition::Timestamp(ALPHA_TIME))]);
    alloc::sync::Arc::new(spec)
}

/// A factory over [`gravity_chain_spec`] with an empty [`MockChainReader`].
pub(crate) fn gravity_evm_factory() -> super::GravityEvmFactory {
    super::GravityEvmFactory::new(
        gravity_chain_spec(),
        alloc::sync::Arc::new(MockChainReader::default()),
    )
}
