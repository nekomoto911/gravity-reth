//! `ots_getContractCreator`: every contract the block created names the transaction that
//! created it and its creator.

use super::{committed::Committed, result_or_record};
use crate::{report::BlockReport, rpc::RpcClient};
use alloy_consensus::{transaction::SignerRecoverable, Transaction};
use alloy_primitives::Address;
use alloy_rpc_types_trace::otterscan::ContractCreator;
use serde_json::json;

pub(super) fn check_contract_creators(
    report: &mut BlockReport<'_>,
    rpc: &RpcClient,
    committed: &Committed,
) {
    let endpoint = "ots_getContractCreator";
    for contract in committed.created_contracts() {
        let response = rpc.call::<Option<ContractCreator>>(endpoint, json!([contract]));
        let Some(creator) = result_or_record(report, endpoint, None, response) else { continue };
        let Some(creator) = creator else {
            report.record(endpoint, None, format!("{contract} creator"), "a creator", "null");
            continue;
        };

        // A deployment transaction is identified by its sender and nonce. An inner CREATE
        // leaves no record in the committed receipts or state of which frame ran it, so there
        // only the creating transaction's membership in this block is required.
        let tx_field = format!("{contract} creating transaction");
        match deployment(committed, contract) {
            Some((index, sender)) => {
                let field = format!("{contract} creator");
                report.check_eq(endpoint, Some(index), field, sender, creator.creator);
                let hash = committed.tx_hashes[index];
                report.check_eq(endpoint, Some(index), tx_field, hash, creator.hash);
            }
            None if !committed.tx_hashes.contains(&creator.hash) => {
                let expected = "a transaction of this block";
                report.record(endpoint, None, tx_field, expected, creator.hash);
            }
            None => {}
        }
    }
}

/// The index and sender of the contract-creation transaction that deployed `contract`.
fn deployment(committed: &Committed, contract: Address) -> Option<(usize, Address)> {
    committed.block.body.transactions.iter().enumerate().find_map(|(index, tx)| {
        if !tx.kind().is_create() {
            return None;
        }
        // Creation transactions are user transactions, whose signatures always recover.
        let sender = tx.recover_signer().expect("committed user transaction signature");
        (sender.create(tx.nonce()) == contract).then_some((index, sender))
    })
}
