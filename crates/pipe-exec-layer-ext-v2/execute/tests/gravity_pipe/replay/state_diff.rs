//! `trace_replayBlockTransactions` state diffs: folded in block order, they must reach the
//! committed post-block state and reveal every field the block's transactions changed, except
//! the code of an EIP-7702 delegation, which these diffs cannot show.

use super::{
    check_tx_hashes,
    committed::{Committed, Field},
    result_or_record,
    revealed_state::{check_revealed_state, RevealedAccount},
};
use crate::report::BlockReport;
use alloy_eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
use alloy_primitives::Address;
use alloy_rpc_types_trace::parity::{Delta, StateDiff, TraceResultsWithTransactionHash};
use std::collections::BTreeMap;

pub(super) fn check_replayed_transactions(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    response: Result<Vec<TraceResultsWithTransactionHash>, String>,
) {
    let Some(replays) = result_or_record(report, endpoint, None, response) else { return };
    let hashes: Vec<_> = replays.iter().map(|replay| Some(replay.transaction_hash)).collect();
    check_tx_hashes(report, endpoint, committed, &hashes);

    let mut diffs = Vec::with_capacity(replays.len());
    for (index, (replay, receipt)) in replays.iter().zip(&committed.receipts).enumerate() {
        let success = replay.full_trace.trace.first().map(|root| root.error.is_none());
        report.check_eq(endpoint, Some(index), "success", Some(receipt.success), success);
        match &replay.full_trace.state_diff {
            Some(diff) => diffs.push(diff),
            None => report.record(endpoint, Some(index), "stateDiff", "a state diff", "none"),
        }
    }
    // Per-transaction diffs cannot contain what the chain wrote outside the transactions, nor
    // the code an EIP-7702 authorization installed.
    let revealed = fold_state_diffs(diffs);
    let unrevealable: Vec<(Address, Field)> =
        committed.outside_writes.iter().copied().chain(delegation_codes(committed)).collect();
    check_revealed_state(report, endpoint, &committed.state, &revealed, &unrevealable);
}

/// The code field of every account the block left holding an EIP-7702 designator.
///
/// revm-inspectors 0.40.4 (`src/tracing/builder/parity.rs:532-555`) diffs an account's code only
/// when the account `is_created()`, and applying an authorization sets code without marking the
/// account created, so the parity state diff never shows a delegation. Only the code is exempt:
/// the delegated account's balance, nonce and storage, and any other code, must still be revealed.
fn delegation_codes(committed: &Committed) -> impl Iterator<Item = (Address, Field)> + '_ {
    committed
        .state
        .changed_accounts
        .keys()
        .filter(|address| is_delegation(&committed.state.post_account(address).code))
        .map(|address| (*address, Field::Code))
}

/// Whether `code` is an EIP-7702 designator: `0xef0100 || address`.
fn is_delegation(code: &[u8]) -> bool {
    code.len() == EIP7702_DELEGATION_DESIGNATOR.len() + Address::len_bytes() &&
        code.starts_with(&EIP7702_DELEGATION_DESIGNATOR)
}

/// Folds per-transaction state diffs, in block order, into each account's final values; a
/// diff that only says a field did not change reveals nothing.
fn fold_state_diffs<'a>(
    diffs: impl IntoIterator<Item = &'a StateDiff>,
) -> BTreeMap<Address, RevealedAccount> {
    let mut accounts = BTreeMap::<Address, RevealedAccount>::new();
    for (address, diff) in diffs.into_iter().flat_map(|diff| diff.iter()) {
        let account = accounts.entry(*address).or_default();
        if let Some(balance) = value_after(&diff.balance) {
            account.balance = Some(balance);
        }
        if let Some(nonce) = value_after(&diff.nonce) {
            account.nonce = Some(nonce.to());
        }
        if let Some(code) = value_after(&diff.code) {
            account.code = Some(code);
        }
        for (slot, delta) in &diff.storage {
            if let Some(value) = value_after(delta) {
                account.storage.insert(*slot, value);
            }
        }
    }
    accounts
}

/// The value after the transaction, or `None` when the diff only says it did not change.
fn value_after<T: Clone + Default>(delta: &Delta<T>) -> Option<T> {
    match delta {
        Delta::Unchanged => None,
        Delta::Added(value) => Some(value.clone()),
        // A destroyed account or a cleared slot reads as empty afterwards.
        Delta::Removed(_) => Some(T::default()),
        Delta::Changed(change) => Some(change.to.clone()),
    }
}
