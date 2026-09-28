//! What the pipe committed for a block, read back from the node's storage: the baseline
//! every replay is compared against.

use crate::{
    node::CommittedBlock,
    timeline::{Fork, Phase},
};
use alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS;
use alloy_primitives::{Address, Bytes, B256, KECCAK256_EMPTY, U256};
use reth_ethereum_primitives::{Block, Receipt};
use reth_pipe_exec_layer_ext_v2::onchain_config::{
    NATIVE_ORACLE_ADDR, ORACLE_TASK_CONFIG_ADDR, SYSTEM_CALLER, TIMESTAMP_ADDR,
};
use reth_primitives_traits::Account;
use reth_provider::{
    BlockReader, ChangeSetReader, StateProviderBox, StateProviderFactory, StorageChangeSetReader,
};
use std::collections::{BTreeMap, BTreeSet};

/// A block as the pipe committed it, read back from the node's storage.
pub(super) struct Committed {
    pub(super) hash: B256,
    pub(super) state_root: B256,
    pub(super) block: Block,
    pub(super) tx_hashes: Vec<B256>,
    pub(super) receipts: Vec<Receipt>,
    /// Accounts the block changed, keyed by address.
    pub(super) changed_accounts: BTreeMap<Address, ChangedAccount>,
    /// Account fields the block wrote outside its transactions.
    pub(super) outside_writes: Vec<(Address, Field)>,
    /// State after the block.
    post_state: StateProviderBox,
}

impl Committed {
    /// Panics when the node cannot serve its own committed block: that is a harness
    /// failure, not a replay mismatch.
    pub(super) fn read<P>(provider: &P, pipe_block: &CommittedBlock, phase: Phase) -> Self
    where
        P: BlockReader<Block = Block, Receipt = Receipt>
            + StateProviderFactory
            + ChangeSetReader
            + StorageChangeSetReader,
    {
        let number = pipe_block.number;
        let hash = provider.block_hash(number).unwrap().expect("committed block hash");
        let block = provider.block_by_number(number).unwrap().expect("committed block");
        let receipts =
            provider.receipts_by_block(number.into()).unwrap().expect("committed receipts");
        let tx_hashes: Vec<B256> = block.body.transactions.iter().map(|tx| *tx.tx_hash()).collect();
        assert_eq!(receipts.len(), tx_hashes.len(), "block {number}: one receipt per transaction");

        // Changesets hold the pre-block values of whatever the block changed; the current
        // values come from the state after the block. An account whose info has no changeset
        // row only had its storage changed.
        let post_state = provider.history_by_block_number(number).unwrap();
        let mut changed_accounts: BTreeMap<Address, ChangedAccount> = provider
            .account_block_changeset(number)
            .unwrap()
            .into_iter()
            .map(|change| {
                let pre = change.info.unwrap_or_default();
                (change.address, ChangedAccount { pre, slots: BTreeSet::new() })
            })
            .collect();
        for (key, entry) in provider.storage_changeset(number).unwrap() {
            let address = key.address();
            changed_accounts
                .entry(address)
                .or_insert_with(|| ChangedAccount {
                    pre: post_state.basic_account(&address).unwrap().unwrap_or_default(),
                    slots: BTreeSet::new(),
                })
                .slots
                .insert(entry.key);
        }

        Self {
            hash,
            state_root: block.header.state_root,
            block,
            tx_hashes,
            receipts,
            changed_accounts,
            outside_writes: written_outside_transactions(phase, pipe_block.epoch_changed),
            post_state,
        }
    }

    pub(super) fn gas_used(&self, tx_index: usize) -> u64 {
        let before = tx_index.checked_sub(1).map_or(0, |i| self.receipts[i].cumulative_gas_used);
        self.receipts[tx_index].cumulative_gas_used - before
    }

    pub(super) fn post_account(&self, address: &Address) -> PostAccount {
        let account = self.post_state.basic_account(address).unwrap().unwrap_or_default();
        let code = self.post_state.account_code(address).unwrap();
        PostAccount {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.get_bytecode_hash(),
            code: code.map(|code| code.original_bytes()).unwrap_or_default(),
        }
    }

    pub(super) fn post_storage(&self, address: &Address, slot: &B256) -> B256 {
        self.post_state.storage(*address, *slot).unwrap().unwrap_or_default().into()
    }

    /// Contracts the block's transactions created: accounts that had no code before the
    /// block and hold a contract after it. An EIP-7702 delegation designator also gives an
    /// account code but creates no contract, and code the chain installs outside
    /// transactions has no creating transaction.
    pub(super) fn created_contracts(&self) -> Vec<Address> {
        self.changed_accounts
            .iter()
            .filter(|(address, changed)| {
                changed.pre.get_bytecode_hash() == KECCAK256_EMPTY &&
                    !self.outside_writes.contains(&(**address, Field::Code))
            })
            .filter(|(address, _)| {
                self.post_state
                    .account_code(address)
                    .unwrap()
                    .is_some_and(|code| !code.original_bytes().is_empty() && !code.is_eip7702())
            })
            .map(|(address, _)| *address)
            .collect()
    }
}

/// An account the block changed.
pub(super) struct ChangedAccount {
    /// Account info before the block; a missing account reads as empty.
    pub(super) pre: Account,
    /// Storage slots the block changed.
    pub(super) slots: BTreeSet<B256>,
}

/// A part of an account that the chain may write outside the block's transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Field {
    Balance,
    Nonce,
    Code,
    Storage,
}

/// An account after the block; a missing account reads as empty.
pub(super) struct PostAccount {
    pub(super) balance: U256,
    pub(super) nonce: u64,
    pub(super) code_hash: B256,
    pub(super) code: Bytes,
}

/// Account fields the chain writes outside the block's transactions, so no per-transaction
/// state diff can reveal them. Each write is scoped to the blocks that make it.
fn written_outside_transactions(phase: Phase, epoch_changed: bool) -> Vec<(Address, Field)> {
    let mut writes = Vec::new();
    // `eip_2935::apply_state_changes_for_block` deploys the block hash contract (nonce 1,
    // code) before the first transaction of the Prague activation block.
    if phase == Phase::Activation(Fork::Prague) {
        writes.extend([
            (HISTORY_STORAGE_ADDRESS, Field::Nonce),
            (HISTORY_STORAGE_ADDRESS, Field::Code),
        ]);
    }
    // From then on the executor's pre-execution system call stores the parent id in the block
    // hash contract. An epoch-change block is assembled from its system transactions alone and
    // never runs the executor, so it writes no slot.
    if phase.has_activated(Fork::Prague) && !epoch_changed {
        writes.push((HISTORY_STORAGE_ADDRESS, Field::Storage));
    }
    // A pre-Alpha DKG epoch-change block executes `onBlockStart`, which updates the global
    // time, but keeps only the DKG transaction in its body.
    if epoch_changed && !phase.has_activated(Fork::Alpha) {
        writes.push((TIMESTAMP_ADDR, Field::Storage));
    }
    // `system_caller_migration` zeroes SYSTEM_CALLER's balance before the first transaction
    // of the Alpha activation block; its gas-exempt transactions leave it unchanged.
    if phase == Phase::Activation(Fork::Alpha) {
        writes.push((SYSTEM_CALLER, Field::Balance));
    }
    // The Gamma hook replaces two oracle runtimes after the last transaction of the first
    // executed block at or after gammaTime, which is the activation block: activation blocks
    // never change epoch.
    if phase == Phase::Activation(Fork::Gamma) {
        writes.extend([(NATIVE_ORACLE_ADDR, Field::Code), (ORACLE_TASK_CONFIG_ADDR, Field::Code)]);
    }
    writes
}
