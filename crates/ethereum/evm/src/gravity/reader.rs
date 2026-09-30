//! Chain reads the Gravity execution rules need beyond the EVM database.

use alloc::vec::Vec;
use alloy_primitives::{Address, B256, U256};
use core::fmt::Debug;
use reth_ethereum_primitives::Receipt;
use reth_storage_errors::provider::ProviderResult;

/// Read access to the canonical chain for [`GravityEvmConfig`](super::GravityEvmConfig).
///
/// The EVM database only holds the state a block executes on. Replaying a committed Gravity
/// block also needs headers, receipts and the state after the block; the node implements this
/// over its provider.
pub trait GravityChainReader: Debug + Send + Sync {
    /// Returns the `mix_hash` of the canonical header at `number`.
    fn mix_hash_by_number(&self, number: u64) -> ProviderResult<Option<B256>>;

    /// Returns the hash of the canonical block at `number`.
    fn canonical_hash(&self, number: u64) -> ProviderResult<Option<B256>>;

    /// Returns the timestamp of the header with `hash`.
    fn header_timestamp(&self, hash: B256) -> ProviderResult<Option<u64>>;

    /// Returns the receipts of the block with `hash`.
    fn receipts_by_block_hash(&self, hash: B256) -> ProviderResult<Option<Vec<Receipt>>>;

    /// Returns the value of `slot` of `address` in the state after the block with `hash`.
    fn storage_after_block(
        &self,
        hash: B256,
        address: Address,
        slot: B256,
    ) -> ProviderResult<Option<U256>>;
}
