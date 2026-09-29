//! Gravity Alpha hardfork — one-shot `SYSTEM_CALLER` balance migration.
//!
//! On the Alpha activation block we zero the `SYSTEM_CALLER` account balance
//! (the historical sentinel ~1.158×10⁵⁸ G allocated in genesis to cover the
//! per-block system-tx base-fee bill). With the gas-exempt design active from
//! Alpha onwards, the sentinel balance is no longer needed and would otherwise
//! pollute total-supply accounting.
//!
//! Shared by the pipe (before its system transactions) and by RPC replay of the
//! activation block:
//!   - Idempotency gated by `transitions_at_timestamp(current_ts, parent_ts)` — fires exactly on
//!     the activation block, reorg-safe (Gravity has immediate finality but the predicate is robust
//!     anyway).
//!   - Routes the diff through [`HardforkState::apply_state_change`], which the serial, grevm and
//!     replay backends all implement, so every backend commits the same diff.
//!
//! Crucially:
//!   - **nonce is preserved** (`SYSTEM_CALLER` auto-increments per block — clearing it would break
//!     the per-block construction sequence post-Alpha).
//!   - **code / `code_hash` are preserved** (defensive symmetry — the historical `SYSTEM_CALLER`
//!     alloc has no code, but treat the read result as ground truth so future migrations that touch
//!     a coded variant stay correct).
//!   - Only `balance` is set to `U256::ZERO`.
//!
//! With `nonce > 0`, the EIP-161 `is_empty` predicate stays false post-migration
//! and the account is never pruned by state-clear.

use super::common::HardforkState;
use alloc::format;
use alloy_primitives::U256;
use reth_chainspec::{ChainSpec, EthChainSpec, GravityHardfork, SYSTEM_CALLER};
use reth_evm::execute::BlockExecutionError;
use revm::state::{Account, AccountInfo, AccountStatus, EvmState};
use tracing::info;

/// Apply Gravity Alpha boundary state changes for `block_number`.
///
/// On the Alpha activation block (the unique block whose timestamp transitions
/// across `alphaTime`), zero the `SYSTEM_CALLER` balance while preserving its
/// nonce and code. No-op on every other block.
///
/// The hook reads `SYSTEM_CALLER`'s current `AccountInfo` via [`HardforkState::basic`],
/// so callers stay decoupled from the hook's data needs and non-activation blocks
/// pay nothing beyond the gating check.
pub fn apply_state_changes_for_block<S: HardforkState + ?Sized>(
    executor: &mut S,
    chain_spec: &ChainSpec,
    current_ts: u64,
    parent_ts: u64,
    block_number: u64,
) -> Result<(), BlockExecutionError> {
    if !chain_spec
        .gravity_hardforks()
        .fork(GravityHardfork::Alpha)
        .transitions_at_timestamp(current_ts, parent_ts)
    {
        return Ok(());
    }

    // `unwrap_or_default` covers degenerate test fixtures where the genesis alloc
    // omits SYSTEM_CALLER — we still wind up writing balance=0 with nonce=0 and
    // no code, which is the natural "empty" terminal state.
    let prev = executor
        .basic(SYSTEM_CALLER)
        .map_err(|e| {
            BlockExecutionError::msg(format!(
                "Alpha migration: failed to read SYSTEM_CALLER account: {e:?}"
            ))
        })?
        .unwrap_or_default();
    let prev_balance = prev.balance;
    let prev_nonce = prev.nonce;

    let new_info = AccountInfo {
        balance: U256::ZERO,
        nonce: prev.nonce,
        code_hash: prev.code_hash,
        code: prev.code,
        account_id: prev.account_id,
    };

    let mut state_diff = EvmState::default();
    let mut account = Account::default();
    account.info = new_info;
    account.status = AccountStatus::Touched;
    state_diff.insert(SYSTEM_CALLER, account);

    executor.apply_state_change(state_diff).map_err(|e| {
        BlockExecutionError::msg(format!(
            "Alpha migration: SYSTEM_CALLER balance zeroing failed: {e:?}"
        ))
    })?;

    info!(target: "execute_ordered_block",
        number = block_number,
        ?prev_balance,
        prev_nonce,
        "Gravity Alpha: zeroed SYSTEM_CALLER balance (nonce/code preserved)"
    );
    Ok(())
}
