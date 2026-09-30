//! Randomness-by-height lookups for replayed and simulated blocks.

use super::GravityChainReader;
use alloc::sync::Arc;
use alloy_primitives::B256;
use gravity_precompiles::randomness_by_height::{
    RandomnessByHeightGasPolicy, RandomnessByHeightLookup, RandomnessByHeightProvider,
};
use reth_storage_errors::provider::ProviderError;

/// Reads randomness from canonical headers, anchored at the block the EVM executes.
///
/// The executing block's own randomness comes from the EVM block environment (`prevrandao`),
/// which for a replayed block is its header `mix_hash`, as the pipe's `ExecutionRandomnessProvider`
/// reads it from the ordered block.
#[derive(Clone, Debug)]
pub struct HeaderRandomnessProvider {
    reader: Arc<dyn GravityChainReader>,
    reference_number: u64,
    current_randomness: Option<B256>,
    gas_policy: RandomnessByHeightGasPolicy,
}

impl HeaderRandomnessProvider {
    /// Creates a provider for an EVM executing block `reference_number`, whose own randomness
    /// is `current_randomness`.
    pub const fn new(
        reader: Arc<dyn GravityChainReader>,
        reference_number: u64,
        current_randomness: Option<B256>,
        gas_policy: RandomnessByHeightGasPolicy,
    ) -> Self {
        Self { reader, reference_number, current_randomness, gas_policy }
    }
}

impl RandomnessByHeightProvider for HeaderRandomnessProvider {
    type Error = ProviderError;

    fn randomness_by_height(&self, height: u64) -> Result<RandomnessByHeightLookup, Self::Error> {
        if height == self.reference_number && self.current_randomness.is_some() {
            return Ok(self.gas_policy.recent(self.current_randomness));
        }

        if height > self.reference_number {
            return Ok(self.gas_policy.recent(None));
        }

        let is_recent = self.reference_number - height <= self.gas_policy.recent_window;
        self.reader.mix_hash_by_number(height).map(|value| {
            if is_recent {
                self.gas_policy.recent(value)
            } else {
                self.gas_policy.storage(value)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gravity::test_utils::MockChainReader;

    fn provider(
        reader: MockChainReader,
        reference_number: u64,
        current_randomness: Option<B256>,
    ) -> HeaderRandomnessProvider {
        HeaderRandomnessProvider::new(
            Arc::new(reader),
            reference_number,
            current_randomness,
            RandomnessByHeightGasPolicy { recent_window: 10, recent_gas: 4, lookup_gas: 20 },
        )
    }

    fn reader() -> MockChainReader {
        MockChainReader::default()
            .with_mix_hash(100, B256::with_last_byte(100))
            .with_mix_hash(140, B256::with_last_byte(140))
    }

    #[test]
    fn uses_current_randomness_from_evm_env() {
        let lookup = provider(reader(), 150, Some(B256::with_last_byte(1)))
            .randomness_by_height(150)
            .unwrap();

        assert_eq!(
            lookup,
            RandomnessByHeightLookup { value: Some(B256::with_last_byte(1)), gas_used: 4 }
        );
    }

    #[test]
    fn falls_back_to_header_for_current_height() {
        let lookup = provider(
            MockChainReader::default().with_mix_hash(150, B256::with_last_byte(150)),
            150,
            None,
        )
        .randomness_by_height(150)
        .unwrap();

        assert_eq!(
            lookup,
            RandomnessByHeightLookup { value: Some(B256::with_last_byte(150)), gas_used: 4 }
        );
    }

    #[test]
    fn charges_recent_tier_for_recent_headers_and_future_misses() {
        let provider = provider(reader(), 150, Some(B256::with_last_byte(1)));

        assert_eq!(
            provider.randomness_by_height(140).unwrap(),
            RandomnessByHeightLookup { value: Some(B256::with_last_byte(140)), gas_used: 4 }
        );
        assert_eq!(
            provider.randomness_by_height(151).unwrap(),
            RandomnessByHeightLookup { value: None, gas_used: 4 }
        );
    }

    #[test]
    fn charges_lookup_tier_for_older_headers_and_misses() {
        let provider = provider(reader(), 150, Some(B256::with_last_byte(1)));

        assert_eq!(
            provider.randomness_by_height(100).unwrap(),
            RandomnessByHeightLookup { value: Some(B256::with_last_byte(100)), gas_used: 20 }
        );
        assert_eq!(
            provider.randomness_by_height(99).unwrap(),
            RandomnessByHeightLookup { value: None, gas_used: 20 }
        );
    }
}
