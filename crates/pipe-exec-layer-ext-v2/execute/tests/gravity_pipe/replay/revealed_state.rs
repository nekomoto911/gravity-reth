//! What a replay reveals about the state after one or more blocks, compared with the committed
//! state: every revealed value must be the committed one, and every committed change must be
//! revealed.

use super::committed::{ChangedState, Field};
use crate::report::BlockReport;
use alloy_primitives::{Address, Bytes, B256, U256};
use std::collections::BTreeMap;

/// What an account reached after the replayed blocks or transactions, as far as the replay
/// reveals it: a field is `None` when the replay says nothing about it.
#[derive(Debug, Default)]
pub(super) struct RevealedAccount {
    pub(super) balance: Option<U256>,
    pub(super) nonce: Option<u64>,
    pub(super) code: Option<Bytes>,
    pub(super) storage: BTreeMap<B256, B256>,
}

/// `unrevealable` lists the account fields the replay cannot reveal by design, such as
/// block-level writes for a replay made of per-transaction diffs.
pub(super) fn check_revealed_state(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &ChangedState,
    revealed: &BTreeMap<Address, RevealedAccount>,
    unrevealable: &[(Address, Field)],
) {
    // Rule 1: every value the replay reveals is the committed value.
    for (address, account) in revealed {
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

    // Rule 2: every field the blocks changed is revealed, unless it is unrevealable.
    let nothing_revealed = RevealedAccount::default();
    for (address, changed) in &committed.changed_accounts {
        let revealed = revealed.get(address).unwrap_or(&nothing_revealed);
        let post = committed.post_account(address);
        let revealable = |field| !unrevealable.contains(&(*address, field));
        let mut unrevealed = Vec::new();
        if changed.pre.balance != post.balance &&
            revealable(Field::Balance) &&
            revealed.balance.is_none()
        {
            unrevealed.push(("balance".to_string(), post.balance.to_string()));
        }
        if changed.pre.nonce != post.nonce && revealable(Field::Nonce) && revealed.nonce.is_none() {
            unrevealed.push(("nonce".to_string(), post.nonce.to_string()));
        }
        if changed.pre.get_bytecode_hash() != post.code_hash &&
            revealable(Field::Code) &&
            revealed.code.is_none()
        {
            unrevealed.push(("code hash".to_string(), post.code_hash.to_string()));
        }
        if revealable(Field::Storage) {
            for slot in changed.slots.iter().filter(|slot| !revealed.storage.contains_key(*slot)) {
                let value = committed.post_storage(address, slot);
                unrevealed.push((format!("slot {slot}"), value.to_string()));
            }
        }
        for (field, expected) in unrevealed {
            report.record(endpoint, None, format!("{address} {field}"), expected, "not revealed");
        }
    }
}
