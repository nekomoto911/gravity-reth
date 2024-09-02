//! Adaptor for https://github.com/AshinGau/pevm/tree/rise_pevm

use core::fmt::Display;
use std::collections::HashMap;
use std::num::NonZeroUsize;

use alloy_rpc_types::BlockTransactions;
use revm_primitives::{Account, AccountInfo, AccountStatus as OAccountStatus, Bytecode, EvmStorageSlot, KECCAK_EMPTY};
use revm_primitives::db::DatabaseRef;
use rise_pevm::{EvmAccount, Pevm, PevmTxExecutionResult};
use rise_pevm::chain::PevmEthereum;

use reth_evm::execute::BlockExecutionError;
use reth_execution_types::BlockExecutionOutput;
use reth_primitives::{Address, BlockWithSenders, Receipt, U256};
use reth_revm::{TransitionAccount, TransitionState};
use reth_revm::db::{AccountStatus, BundleState};
use reth_revm::db::states::bundle_state::BundleRetention;

struct PevmState {
    pub transition_state: TransitionState,
    pub bundle_state: BundleState,
}

impl PevmState {
    pub fn new() -> Self {
        Self {
            transition_state: TransitionState::default(),
            bundle_state: BundleState::default(),
        }
    }

    fn from_evm_account(account: EvmAccount) -> Account {
        let info = AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.code_hash.unwrap_or(KECCAK_EMPTY),
            code: account.code.map(Bytecode::from),
        };
        let storage: HashMap<U256, EvmStorageSlot> = account.storage.iter().map(|(address, value)| (*address, EvmStorageSlot {
            original_value: *value, // todo: pevm didn't store the origin value
            present_value: *value,
            is_cold: false,
        })).collect();
        // todo: pevm didn't mark the account status
        let status = OAccountStatus::Touched;
        Account { info, storage, status }
    }

    fn apply_account(&mut self, address: Address, account: EvmAccount) {
        let Account { info, storage, status } = Self::from_evm_account(account);
        let storage = storage.into_iter().map(|(address, slot)| (address, slot.into())).collect();
        let transition = TransitionAccount {
            info: Some(info),
            status: AccountStatus::Changed,
            previous_info: None,
            previous_status: AccountStatus::Loaded,
            storage,
            storage_was_destroyed: false,
        };
        self.transition_state.add_transitions(vec![(address, transition)]);
    }

    fn take_bundle(mut self) -> BundleState {
        self.bundle_state
            .apply_transitions_and_create_reverts(self.transition_state, BundleRetention::Reverts);
        self.bundle_state
    }
}

pub fn pevm_executor<DB>(
    database: &DB,
    block: &BlockWithSenders,
    total_difficulty: U256) -> Result<BlockExecutionOutput<Receipt>, BlockExecutionError>
where
    DB: DatabaseRef<Error: Display> + Send + Sync,
{
    if block.senders.is_empty() {
        Ok(BlockExecutionOutput {
            state: BundleState::default(),
            receipts: vec![],
            requests: vec![],
            gas_used: 0,
        })
    } else {
        let mut pevm = Pevm::default();
        let tx_types = block.body.iter().map(|tx| tx.tx_type());
        let mut block: alloy_rpc_types::Block = block.clone().try_into()
            .map_err(|err| { BlockExecutionError::other(err) })?;
        let chain = match &block.transactions {
            BlockTransactions::Full(txs) =>
                txs[0].chain_id.map(PevmEthereum::new).unwrap_or(PevmEthereum::mainnet()),
            _ => PevmEthereum::mainnet(),
        };
        block.header.total_difficulty = Some(total_difficulty);
        let result = pevm.execute(database, &chain, block, NonZeroUsize::new(8).unwrap(), false)
            .map_err(|err| BlockExecutionError::msg(format!("PEVM execute failed: {:?}", err)))?;
        let mut pvm_state = PevmState::new();
        let mut cumulative_gas_used: u64 = 0;
        let mut receipts = Vec::with_capacity(result.len());
        for (tx_result, tx_type) in result.into_iter().zip(tx_types) {
            let PevmTxExecutionResult { receipt, state } = tx_result;
            cumulative_gas_used = receipt.cumulative_gas_used.try_into().map_err(|err| BlockExecutionError::msg("Failed to convert u128 -> u64"))?;
            receipts.push(Receipt {
                tx_type,
                success: true, // todo: success or failed
                cumulative_gas_used,
                logs: receipt.logs,
                #[cfg(feature = "optimism")]
                deposit_nonce: None,
                #[cfg(feature = "optimism")]
                deposit_receipt_version: None,
            });
            for (address, account) in state {
                if let Some(account) = account {
                    pvm_state.apply_account(address, account);
                }
            }
        }

        Ok(BlockExecutionOutput {
            state: pvm_state.take_bundle(),
            receipts,
            requests: vec![],
            gas_used: cumulative_gas_used.into(),
        })
    }
}
