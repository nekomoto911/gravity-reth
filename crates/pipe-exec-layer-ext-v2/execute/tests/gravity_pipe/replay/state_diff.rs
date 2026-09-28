//! `trace_replayBlockTransactions` state diffs: folded in block order, they must reach the
//! committed post-block state and reveal every field the block's transactions changed.

use super::{
    check_tx_hashes,
    committed::{Committed, Field},
    result_or_record,
};
use crate::report::BlockReport;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_trace::parity::{Delta, StateDiff, TraceResultsWithTransactionHash};
use std::collections::BTreeMap;

pub(super) fn check_replayed_transactions(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    response: Result<Vec<TraceResultsWithTransactionHash>, String>,
) {
    let Some(replays) = result_or_record(report, endpoint, response) else { return };
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
    check_state_diffs(report, endpoint, committed, &fold_state_diffs(diffs));
}

/// What an account reached after the block's transactions, as far as their state diffs
/// reveal it: a field is `None` when every diff only says it did not change.
#[derive(Debug, Default)]
struct DiffedAccount {
    balance: Option<U256>,
    nonce: Option<u64>,
    code: Option<Bytes>,
    storage: BTreeMap<B256, B256>,
}

/// Folds per-transaction state diffs, in block order, into each account's final values.
fn fold_state_diffs<'a>(
    diffs: impl IntoIterator<Item = &'a StateDiff>,
) -> BTreeMap<Address, DiffedAccount> {
    let mut accounts = BTreeMap::<Address, DiffedAccount>::new();
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

fn check_state_diffs(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    diffed: &BTreeMap<Address, DiffedAccount>,
) {
    // Rule 1: every value the diffs reveal is the committed post-block value.
    for (address, account) in diffed {
        let post = committed.post_account(address);
        if let Some(balance) = account.balance {
            report.check_eq(endpoint, None, format!("{address} balance"), post.balance, balance);
        }
        if let Some(nonce) = account.nonce {
            report.check_eq(endpoint, None, format!("{address} nonce"), post.nonce, nonce);
        }
        if let Some(code) = &account.code {
            report.check_eq(endpoint, None, format!("{address} code"), &post.code, code);
        }
        for (slot, value) in &account.storage {
            let expected = committed.post_storage(address, slot);
            report.check_eq(endpoint, None, format!("{address} slot {slot}"), expected, *value);
        }
    }

    // Rule 2: every field the block changed is revealed by some diff, unless the chain wrote
    // it outside the block's transactions. A diff that says "unchanged" reveals nothing.
    let nothing_revealed = DiffedAccount::default();
    for (address, changed) in &committed.changed_accounts {
        let revealed = diffed.get(address).unwrap_or(&nothing_revealed);
        let post = committed.post_account(address);
        let written_by_transactions =
            |field| !committed.outside_writes.contains(&(*address, field));
        let mut unrevealed = Vec::new();
        if changed.pre.balance != post.balance &&
            written_by_transactions(Field::Balance) &&
            revealed.balance.is_none()
        {
            unrevealed.push(("balance".to_string(), post.balance.to_string()));
        }
        if changed.pre.nonce != post.nonce &&
            written_by_transactions(Field::Nonce) &&
            revealed.nonce.is_none()
        {
            unrevealed.push(("nonce".to_string(), post.nonce.to_string()));
        }
        if changed.pre.get_bytecode_hash() != post.code_hash &&
            written_by_transactions(Field::Code) &&
            revealed.code.is_none()
        {
            unrevealed.push(("code hash".to_string(), post.code_hash.to_string()));
        }
        if written_by_transactions(Field::Storage) {
            for slot in changed.slots.iter().filter(|slot| !revealed.storage.contains_key(*slot)) {
                let value = committed.post_storage(address, slot);
                unrevealed.push((format!("slot {slot}"), value.to_string()));
            }
        }
        for (field, expected) in unrevealed {
            let field = format!("{address} {field}");
            report.record(endpoint, None, field, expected, "not revealed by any state diff");
        }
    }
}
