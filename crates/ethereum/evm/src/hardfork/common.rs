//! Common types and traits for Gravity hardfork state changes.
//!
//! Each hardfork module defines a set of system contract bytecode
//! upgrades. This module provides a trait that standardizes how upgrade
//! tables are exposed, enabling generic verification logic in tests and
//! a single `apply_hardfork_upgrades` entry point for the executor.

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use reth_evm::{execute::BlockExecutionError, ParallelDatabase};
use revm::{
    bytecode::Bytecode,
    state::{Account, AccountInfo, AccountStatus, EvmState, EvmStorageSlot, TransactionId},
    DatabaseCommit, DatabaseRef,
};

/// A single bytecode replacement: target address → new runtime bytecode.
pub type BytecodeUpgrade = (Address, &'static [u8]);

/// A single storage patch: target address, slot, value.
pub type StoragePatch = (Address, B256, U256);

/// A batch storage patch: same (slot, value) applied to multiple addresses.
/// Used for multi-address storage initialization across `StakePool` instances.
pub type BatchStoragePatch = (&'static [Address], B256, U256);

/// Trait implemented by each hardfork module to describe its upgrades.
///
/// This enables generic test helpers that work across any hardfork:
/// ```ignore
/// fn verify_hardfork_applied<H: HardforkUpgrades>(provider: &P, block: u64) { ... }
/// ```
pub trait HardforkUpgrades {
    /// The human-readable name of this hardfork (e.g. "Alpha").
    fn name(&self) -> &'static str;

    /// System contract bytecode upgrades: `(address, new_bytecode)` pairs.
    fn system_upgrades(&self) -> &'static [BytecodeUpgrade];

    /// Additional non-system contract bytecode upgrades (e.g. `StakePool` instances).
    /// Returns `(address, new_bytecode)` pairs.
    fn extra_upgrades(&self) -> &'static [BytecodeUpgrade] {
        &[]
    }

    /// Storage patches to apply alongside bytecode replacements
    /// (e.g. `ReentrancyGuard` initialization, Governance owner).
    fn storage_patches(&self) -> &'static [StoragePatch] {
        &[]
    }

    /// Batch storage patches: apply the same (slot, value) to multiple addresses.
    /// Used for multi-address storage initialization across `StakePool` instances.
    fn batch_storage_patches(&self) -> &'static [BatchStoragePatch] {
        &[]
    }
}

/// Apply a hardfork's upgrades to the parallel state.
///
/// This is the single entry point used by `GrevmExecutor::apply_post_execution_changes`.
/// It replaces bytecodes, writes storage patches, and commits all changes atomically.
pub fn apply_hardfork_upgrades<H: HardforkUpgrades, DB: ParallelDatabase>(
    hardfork: &H,
    state: &mut grevm::ParallelState<DB>,
) -> Result<(), BlockExecutionError> {
    use reth_evm::execute::BlockValidationError;

    let mut hardfork_changes: EvmState = EvmState::default();

    // 1. Apply all bytecode upgrades (system + extra)
    let all_upgrades = hardfork.system_upgrades().iter().chain(hardfork.extra_upgrades().iter());

    for (addr, bytecode_bytes) in all_upgrades {
        let new_bytecode = Bytecode::new_raw(Bytes::from_static(bytecode_bytes));
        let code_hash = keccak256(bytecode_bytes);

        let account = state
            .load_mut_cache_account(*addr)
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        if let Some(ref info) = account.account {
            let mut new_info = info.clone();
            new_info.code_hash = code_hash;
            new_info.code = Some(new_bytecode.clone());
            let mut acc = Account::default();
            acc.info = new_info;
            acc.status = AccountStatus::Touched;
            hardfork_changes.insert(*addr, acc);
        }

        state.cache.contracts.insert(code_hash, new_bytecode);
    }

    // 2. Apply storage patches
    for (addr, slot, value) in hardfork.storage_patches() {
        // Ensure account is loaded
        state
            .load_mut_cache_account(*addr)
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        let slot_key = U256::from_be_bytes(slot.0);
        let original_value = state
            .storage_ref(*addr, slot_key)
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        let entry = hardfork_changes.entry(*addr).or_insert_with(|| {
            // Account already loaded above; create a minimal touched entry
            let info = state
                .cache
                .accounts
                .get(addr)
                .and_then(|a| a.value().account.clone())
                .unwrap_or_default();
            let mut acc = Account::default();
            acc.info = info;
            acc.status = AccountStatus::Touched;
            acc
        });

        entry.storage.insert(
            slot_key,
            EvmStorageSlot::new_changed(original_value, *value, TransactionId::default()),
        );
    }

    // 3. Apply batch storage patches (same slot+value to multiple addresses)
    for (addresses, slot, value) in hardfork.batch_storage_patches() {
        let slot_key = U256::from_be_bytes(slot.0);
        for addr in *addresses {
            state
                .load_mut_cache_account(*addr)
                .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

            let original_value = state
                .storage_ref(*addr, slot_key)
                .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

            let entry = hardfork_changes.entry(*addr).or_insert_with(|| {
                let info = state
                    .cache
                    .accounts
                    .get(addr)
                    .and_then(|a| a.value().account.clone())
                    .unwrap_or_default();
                let mut acc = Account::default();
                acc.info = info;
                acc.status = AccountStatus::Touched;
                acc
            });

            entry.storage.insert(
                slot_key,
                EvmStorageSlot::new_changed(original_value, *value, TransactionId::default()),
            );
        }
    }

    // 4. Commit all changes atomically
    state.commit(hardfork_changes);
    Ok(())
}

/// Account reads and irregular state writes used by the one-shot Gravity hardfork changes.
///
/// Both the pipe (through its grevm
/// [`ParallelExecutor`](reth_evm::parallel_execute::ParallelExecutor)) and the RPC replay executor
/// (through [`DbHardforkState`]) apply the same functions through this interface, so a committed
/// block and its replay share one implementation.
pub trait HardforkState {
    /// Reads the current account info of `address`.
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, BlockExecutionError>;

    /// Commits `state_diff` outside of any transaction.
    fn apply_state_change(&mut self, state_diff: EvmState) -> Result<(), BlockExecutionError>;
}

impl<T> HardforkState for T
where
    T: reth_evm::parallel_execute::ParallelExecutor<
            Primitives = reth_ethereum_primitives::EthPrimitives,
            Error = BlockExecutionError,
        > + ?Sized,
{
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, BlockExecutionError> {
        reth_evm::parallel_execute::ParallelExecutor::basic(self, address)
    }

    fn apply_state_change(&mut self, state_diff: EvmState) -> Result<(), BlockExecutionError> {
        reth_evm::parallel_execute::ParallelExecutor::apply_state_change(self, state_diff)
    }
}

/// [`HardforkState`] over a plain database, as used by a block executor's EVM.
#[derive(Debug)]
pub struct DbHardforkState<'a, DB>(pub &'a mut DB);

impl<DB> HardforkState for DbHardforkState<'_, DB>
where
    DB: alloy_evm::Database + DatabaseCommit,
{
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, BlockExecutionError> {
        self.0.basic(address).map_err(BlockExecutionError::other)
    }

    fn apply_state_change(&mut self, state_diff: EvmState) -> Result<(), BlockExecutionError> {
        self.0.commit(state_diff);
        Ok(())
    }
}
