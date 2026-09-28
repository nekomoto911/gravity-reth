//! Replays that stop inside the block: the state after a given transaction, and calls made on
//! top of the block's first transactions.
//!
//! Each endpoint counts its position differently:
//! - `debug_accountAt`, `debug_accountInfoAt`: the state includes transaction `i`;
//! - `debug_traceCall` with `txIndex = i`: the call runs before transaction `i`;
//! - `eth_callMany`, `debug_traceCallMany` with `transactionIndex = n`: the first `n` transactions
//!   are replayed; with `n` equal to the transaction count nothing is replayed and the call runs on
//!   the committed post-block state instead.

use super::{
    call_tracer_options,
    committed::{Committed, Field},
    result_or_record,
};
use crate::{report::BlockReport, rpc::RpcClient};
use alloy_consensus::TrieAccount;
use alloy_eips::BlockId;
use alloy_primitives::{Bytes, TxKind, U64};
use alloy_rpc_types_eth::{
    AccountInfo, Bundle, EthCallResponse, Index, StateContext, TransactionIndex, TransactionInput,
    TransactionRequest,
};
use alloy_rpc_types_trace::geth::CallFrame;
use alloy_sol_types::{sol, SolCall};
use reth_pipe_exec_layer_ext_v2::onchain_config::TIMESTAMP_ADDR;
use serde_json::json;

sol! {
    function nowMicroseconds() external view returns (uint64);
}

pub(super) fn check_mid_block(
    report: &mut BlockReport<'_>,
    rpc: &RpcClient,
    committed: &Committed,
) {
    let Some(last_index) = committed.tx_hashes.len().checked_sub(1) else { return };

    // Step 1: the state including the last transaction is the committed state of every
    // account the block changed.
    check_accounts_at(report, rpc, committed, last_index);
    check_account_infos_at(report, rpc, committed, last_index);

    // Step 2: on top of every transaction but the last, the global time is the block's: the
    // first transaction, `onBlockStart`, wrote it. With a single transaction there is no such
    // position past `onBlockStart`.
    if last_index > 0 {
        check_time_before_last_transaction(report, rpc, committed, last_index);
    }
}

/// `debug_accountAt` exposes balance, nonce, code hash and storage root.
fn check_accounts_at(
    report: &mut BlockReport<'_>,
    rpc: &RpcClient,
    committed: &Committed,
    last_index: usize,
) {
    let endpoint = "debug_accountAt";
    let tx_index = Some(last_index);
    for address in committed.state.changed_accounts.keys() {
        let params = json!([committed.hash, Index::from(last_index), address]);
        let response = rpc.call::<Option<TrieAccount>>(endpoint, params);
        // An error comes from replaying the block, whatever the account: recording it once
        // keeps one failing block from flooding the report.
        let Some(account) = result_or_record(report, endpoint, tx_index, response) else { return };
        let post = committed.state.post_account(address);
        let field = format!("{address} exists");
        report.check_eq(endpoint, tx_index, field, post.exists, account.is_some());
        let Some(account) = account else { continue };
        // What the chain writes after the last transaction is not part of the state at it.
        let at_last_transaction =
            |field| !committed.writes_after_transactions.contains(&(*address, field));
        if at_last_transaction(Field::Balance) {
            let field = format!("{address} balance");
            report.check_eq(endpoint, tx_index, field, post.balance, account.balance);
        }
        if at_last_transaction(Field::Nonce) {
            let field = format!("{address} nonce");
            report.check_eq(endpoint, tx_index, field, post.nonce, account.nonce);
        }
        if at_last_transaction(Field::Code) {
            let field = format!("{address} code hash");
            report.check_eq(endpoint, tx_index, field, post.code_hash, account.code_hash);
        }
        if at_last_transaction(Field::Storage) {
            let expected = committed.state.post_storage_root(*address);
            let field = format!("{address} storage root");
            report.check_eq(endpoint, tx_index, field, expected, account.storage_root);
        }
    }
}

/// `debug_accountInfoAt` exposes balance, nonce and code; a missing account reads as empty.
fn check_account_infos_at(
    report: &mut BlockReport<'_>,
    rpc: &RpcClient,
    committed: &Committed,
    last_index: usize,
) {
    let endpoint = "debug_accountInfoAt";
    let tx_index = Some(last_index);
    for address in committed.state.changed_accounts.keys() {
        let params = json!([committed.hash, Index::from(last_index), address]);
        let response = rpc.call::<Option<AccountInfo>>(endpoint, params);
        // Same as `debug_accountAt`: an error is the block's, recorded once.
        let Some(info) = result_or_record(report, endpoint, tx_index, response) else { return };
        let Some(info) = info else {
            report.record(endpoint, tx_index, format!("{address} info"), "an account", "null");
            continue;
        };
        let post = committed.state.post_account(address);
        // What the chain writes after the last transaction is not part of the state at it.
        let at_last_transaction =
            |field| !committed.writes_after_transactions.contains(&(*address, field));
        if at_last_transaction(Field::Balance) {
            let field = format!("{address} balance");
            report.check_eq(endpoint, tx_index, field, post.balance, info.balance);
        }
        if at_last_transaction(Field::Nonce) {
            let field = format!("{address} nonce");
            report.check_eq(endpoint, tx_index, field, post.nonce, info.nonce);
        }
        if at_last_transaction(Field::Code) {
            let field = format!("{address} code");
            report.check_eq(endpoint, tx_index, field, &post.code, &info.code);
        }
    }
}

/// Calls `Timestamp.nowMicroseconds()` on top of the block's first `last_index` transactions
/// through every endpoint that places a call inside a block.
fn check_time_before_last_transaction(
    report: &mut BlockReport<'_>,
    rpc: &RpcClient,
    committed: &Committed,
    last_index: usize,
) {
    let block = BlockId::hash(committed.hash);
    let probe = TransactionRequest {
        to: Some(TxKind::Call(TIMESTAMP_ADDR)),
        input: TransactionInput::new(nowMicrosecondsCall {}.abi_encode().into()),
        ..Default::default()
    };
    let bundles = [Bundle { transactions: vec![probe.clone()], block_override: None }];
    let state_context = StateContext {
        block_number: Some(block),
        transaction_index: Some(TransactionIndex::Index(last_index)),
    };
    let tx_index = Some(last_index);
    let check_time = |report: &mut BlockReport<'_>, endpoint, output: Option<&Bytes>| {
        let time = output.and_then(|output| nowMicrosecondsCall::abi_decode_returns(output).ok());
        report.check_eq(endpoint, tx_index, "global time", Some(committed.timestamp_us), time);
    };

    let endpoint = "eth_callMany";
    let response = rpc.call::<Vec<Vec<EthCallResponse>>>(endpoint, json!([bundles, state_context]));
    if let Some(results) = result_or_record(report, endpoint, tx_index, response) {
        let result = results.first().and_then(|bundle| bundle.first());
        if let Some(error) = result.and_then(|result| result.error.as_ref()) {
            report.record(endpoint, tx_index, "call", "a result", error);
        }
        check_time(report, endpoint, result.and_then(|result| result.value.as_ref()));
    }

    let endpoint = "debug_traceCallMany";
    let params = json!([bundles, state_context, call_tracer_options()]);
    let response = rpc.call::<Vec<Vec<CallFrame>>>(endpoint, params);
    if let Some(frames) = result_or_record(report, endpoint, tx_index, response) {
        let frame = frames.first().and_then(|bundle| bundle.first());
        check_time(report, endpoint, frame.and_then(|frame| frame.output.as_ref()));
    }

    let endpoint = "debug_traceCall";
    let mut options = call_tracer_options();
    options["txIndex"] = json!(U64::from(last_index));
    let response = rpc.call::<CallFrame>(endpoint, json!([probe, block, options]));
    if let Some(frame) = result_or_record(report, endpoint, tx_index, response) {
        check_time(report, endpoint, frame.output.as_ref());
    }
}
