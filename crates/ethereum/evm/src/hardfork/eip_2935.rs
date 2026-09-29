//! EIP-2935 (Prague) — `HISTORY_STORAGE` activation.
//!
//! Shared by the pipe's `execute_ordered_block` and by RPC replay of committed blocks, so both
//! see a single definition of "irregular state changes at the Prague boundary".
//!
//! EIP-2935 `HISTORY_STORAGE_ADDRESS` deployment fires exactly on the Prague activation block.
//! Mainnet-aligned alloc: nonce=1, balance=0, `code=HISTORY_STORAGE_CODE`, no storage prefill.
//! The upstream `SystemCaller` then drives the per-block SSTORE inside the executor's
//! `apply_pre_execution_changes`.

use super::common::HardforkState;
use alloc::format;
use alloy_eips::eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE};
use alloy_primitives::{keccak256, Address, Bytes, U256};
use reth_chainspec::{ChainSpec, EthereumHardfork, Hardforks};
use reth_evm::execute::BlockExecutionError;
use revm::{
    bytecode::Bytecode,
    state::{Account, AccountInfo, AccountStatus, EvmState},
};
use tracing::info;

/// Apply EIP-2935 boundary state changes for `block_number`.
///
/// Idempotency comes from `transitions_at_timestamp` — `parent_ts < pragueTime`
/// is history-immutable, so the deployment branch fires exactly on the
/// activation block and is naturally reorg-safe.
pub fn apply_state_changes_for_block<S: HardforkState + ?Sized>(
    executor: &mut S,
    chain_spec: &ChainSpec,
    current_ts: u64,
    parent_ts: u64,
    block_number: u64,
) -> Result<(), BlockExecutionError> {
    if chain_spec.fork(EthereumHardfork::Prague).transitions_at_timestamp(current_ts, parent_ts) {
        deploy_contract(executor, HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone()).map_err(
            |e| {
                BlockExecutionError::msg(format!(
                    "HISTORY_STORAGE deployment failed at Prague activation: {e:?}"
                ))
            },
        )?;
        info!(target: "execute_ordered_block",
            number=?block_number,
            "deployed EIP-2935 HISTORY_STORAGE contract on Prague activation block"
        );
    }
    Ok(())
}

/// Deploy a contract at `address` with `code` via the executor's
/// [`HardforkState::apply_state_change`] irregular-state-change channel.
///
/// Mainnet-aligned alloc shape: nonce=1, balance=0, given code, no storage prefill.
/// The `Created | Touched` status routes the diff through `ParallelState`'s
/// `newly_created` path so the contract code is recorded and a proper transition
/// lands in the bundle.
fn deploy_contract<S: HardforkState + ?Sized>(
    executor: &mut S,
    address: Address,
    code: Bytes,
) -> Result<(), BlockExecutionError> {
    let code_hash = keccak256(&code);
    let info = AccountInfo {
        nonce: 1,
        balance: U256::ZERO,
        code_hash,
        code: Some(Bytecode::new_raw(code)),
        ..Default::default()
    };

    let mut state_diff = EvmState::default();
    let mut account = Account::from(info);
    account.status = AccountStatus::Created | AccountStatus::Touched;
    state_diff.insert(address, account);
    executor.apply_state_change(state_diff)
}
