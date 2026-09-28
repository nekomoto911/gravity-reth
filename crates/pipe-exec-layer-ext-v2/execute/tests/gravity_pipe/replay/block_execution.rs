//! Whole-block execution.
//!
//! `reth_getBlockExecutionOutcome` re-executes blocks as a unit, so it must reproduce their
//! receipts and every state change, including what the chain wrote outside transactions.
//!
//! Execution witnesses are not offered: the randomness precompile reads block headers from the
//! node's database, outside anything a witness records, so a block that used it cannot be
//! re-executed from its witness. Both witness endpoints must refuse every block as unsupported.

use super::{
    committed::{ChangedState, Committed},
    result_or_record,
    revealed_state::{check_revealed_state, RevealedAccount},
};
use crate::{report::BlockReport, rpc::RpcClient};
use alloy_primitives::{map::B256Map, Address, B256};
use reth_ethereum_primitives::Receipt;
use reth_execution_types::ExecutionOutcome;
use revm::{database::BundleAccount, state::Bytecode};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(super) fn check_block_execution(
    report: &mut BlockReport<'_>,
    rpc: &RpcClient,
    committed: &Committed,
) {
    let number_hex = format!("{:#x}", committed.number);

    // Step 1: the block re-executed on its own reproduces what the pipe committed.
    let endpoint = "reth_getBlockExecutionOutcome";
    let response = rpc.call(endpoint, json!([number_hex]));
    let (number, state) = (committed.number, &committed.state);
    let receipts = std::slice::from_ref(&committed.receipts);
    check_execution_outcome(report, endpoint, number, receipts, state, response);

    // Step 2: no block has an execution witness.
    for (endpoint, block) in [
        ("debug_executionWitness", json!(number_hex)),
        ("debug_executionWitnessByBlockHash", json!(committed.hash)),
    ] {
        check_refused_as_unsupported(report, endpoint, rpc.call(endpoint, json!([block])));
    }
}

/// `receipts` holds the committed receipts of each block from `first_block` on; `committed`
/// is what those blocks changed and the state after the last of them.
pub(super) fn check_execution_outcome(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    first_block: u64,
    receipts: &[Vec<Receipt>],
    committed: &ChangedState,
    response: Result<Option<ExecutionOutcome<Receipt>>, String>,
) {
    let Some(outcome) = result_or_record(report, endpoint, None, response) else { return };
    let Some(outcome) = outcome else {
        report.record(endpoint, None, "response", "an execution outcome", "null");
        return;
    };

    // Step 1: one receipt list per block, each equal to the committed receipts.
    report.check_eq(endpoint, None, "first block", first_block, outcome.first_block);
    report.check_eq(endpoint, None, "block count", receipts.len(), outcome.receipts.len());
    let replayed = &outcome.receipts;
    for (number, (expected, actual)) in (first_block..).zip(receipts.iter().zip(replayed)) {
        // Name the block only when the outcome spans several.
        let field = |name| match receipts.len() {
            1 => format!("receipt {name}"),
            _ => format!("block {number} receipt {name}"),
        };
        report.check_eq(endpoint, None, field("count"), expected.len(), actual.len());
        for (index, (expected, actual)) in expected.iter().zip(actual).enumerate() {
            let index = Some(index);
            report.check_eq(endpoint, index, field("type"), expected.tx_type, actual.tx_type);
            report.check_eq(endpoint, index, field("success"), expected.success, actual.success);
            report.check_eq(
                endpoint,
                index,
                field("cumulative gas used"),
                expected.cumulative_gas_used,
                actual.cumulative_gas_used,
            );
            report.check_eq(endpoint, index, field("logs"), &expected.logs, &actual.logs);
        }
    }

    // Step 2: the bundle holds every changed account's final info and changed slots. A
    // whole-block execution includes the block-level writes, so nothing is exempt.
    let bundle = &outcome.bundle;
    let revealed: BTreeMap<Address, RevealedAccount> = bundle
        .state
        .iter()
        .map(|(address, account)| (*address, revealed_account(account, &bundle.contracts)))
        .collect();
    check_revealed_state(report, endpoint, committed, &revealed, &[]);
}

/// A bundle account carries its complete final info; a destroyed account has none and reads
/// as empty. Its code is inline, or among the bundle's new contracts; code the execution
/// never loaded is not revealed.
fn revealed_account(account: &BundleAccount, contracts: &B256Map<Bytecode>) -> RevealedAccount {
    let info = account.info.clone().unwrap_or_default();
    let code = info.code.or_else(|| contracts.get(&info.code_hash).cloned());
    RevealedAccount {
        balance: Some(info.balance),
        nonce: Some(info.nonce),
        code: code.map(|code| code.original_bytes()),
        storage: account
            .storage
            .iter()
            .map(|(slot, value)| (B256::from(*slot), B256::from(value.present_value)))
            .collect(),
    }
}

/// A refusal is a JSON-RPC error whose message says the witness is unsupported or unavailable;
/// a witness, or any other error such as a failed re-execution, is a mismatch.
fn check_refused_as_unsupported(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    response: Result<Value, String>,
) {
    let expected = "an unsupported error";
    let error = match response {
        Ok(_) => return report.record(endpoint, None, "response", expected, "a witness"),
        Err(error) => error,
    };
    let message = serde_json::from_str::<Value>(&error)
        .ok()
        .and_then(|error| error["message"].as_str().map(str::to_lowercase))
        .unwrap_or_default();
    if !(message.contains("unsupported") || message.contains("unavailable")) {
        report.record(endpoint, None, "response", expected, error);
    }
}
