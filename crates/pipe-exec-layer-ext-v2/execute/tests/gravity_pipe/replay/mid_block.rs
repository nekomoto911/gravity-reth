//! Replays that stop inside the block: the state after a given transaction, and calls made on
//! top of the block's first transactions.
//!
//! Each endpoint counts its position differently:
//! - `debug_accountAt`, `debug_accountInfoAt`: the state includes transaction `i`;
//! - `debug_traceCall` with `txIndex = i`: the call runs before transaction `i`;
//! - `eth_callMany`, `debug_traceCallMany` with `transactionIndex = n`: the first `n` transactions
//!   are replayed; with `n` equal to the transaction count nothing is replayed and the call runs on
//!   the committed post-block state instead.
//!
//! The call endpoints return only the call placed on top, not the replayed transactions, so two
//! kinds of calls probe the replayed prefix: `Timestamp.nowMicroseconds()` shows that
//! `onBlockStart` ran, and every user transaction re-run as a call at its own position must
//! reproduce its receipt, on top of a replay of every transaction before it.

use super::{
    call_tracer_options,
    committed::{Committed, Field},
    result_or_record,
};
use crate::{report::BlockReport, rpc::RpcClient};
use alloy_consensus::{transaction::SignerRecoverable, TrieAccount};
use alloy_eips::BlockId;
use alloy_primitives::{Bytes, Log, TxKind, U256, U64};
use alloy_rpc_types_eth::{
    AccountInfo, Bundle, EthCallResponse, Index, StateContext, TransactionIndex, TransactionInput,
    TransactionRequest,
};
use alloy_rpc_types_trace::geth::{CallFrame, CallLogFrame};
use alloy_sol_types::{sol, SolCall};
use reth_pipe_exec_layer_ext_v2::onchain_config::{SYSTEM_CALLER, TIMESTAMP_ADDR};
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

    // Step 3: every user transaction, re-run as a call on top of the transactions before it,
    // reproduces its receipt. System transactions are not signed (their signature is zero) and
    // are not a sender's call, so they have no call to re-run.
    for (index, tx) in committed.block.body.transactions.iter().enumerate() {
        let Ok(sender) = tx.recover_signer() else { continue };
        if sender != SYSTEM_CALLER {
            let call = TransactionRequest::from_transaction_with_sender(tx.clone(), sender);
            check_transaction_as_call(report, rpc, committed, index, call);
        }
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

/// Runs `call`, transaction `index` as a request, on top of the block's first `index`
/// transactions: the state the chain executed the transaction on. The call must then use the
/// transaction's gas, succeed or fail alike and emit the receipt's logs; a prefix replayed
/// differently shows here as soon as the difference reaches the transaction.
///
/// `call` copies the transaction's gas limit, fees, access list and authorization list. An
/// explicit gas limit matters: without one reth caps the call's gas by the sender's balance.
/// These endpoints run the call with `eth_call` semantics: no fee is charged and the nonce is not
/// checked, so a contract reading its sender's balance would see more than on chain. The test's
/// scenario transactions do not depend on that.
fn check_transaction_as_call(
    report: &mut BlockReport<'_>,
    rpc: &RpcClient,
    committed: &Committed,
    index: usize,
    call: TransactionRequest,
) {
    let block = BlockId::hash(committed.hash);
    let tx_index = Some(index);
    let mut options = call_tracer_options();
    options["tracerConfig"] = json!({ "withLog": true });

    let endpoint = "debug_traceCall";
    let mut at_index = options.clone();
    at_index["txIndex"] = json!(U64::from(index));
    let response = rpc.call::<CallFrame>(endpoint, json!([call, block, at_index]));
    if let Some(frame) = result_or_record(report, endpoint, tx_index, response) {
        check_frame_as_transaction(report, endpoint, committed, index, &frame);
    }

    let bundles = [Bundle { transactions: vec![call], block_override: None }];
    let state_context = StateContext {
        block_number: Some(block),
        transaction_index: Some(TransactionIndex::Index(index)),
    };

    let endpoint = "debug_traceCallMany";
    let params = json!([bundles, state_context, options]);
    let response = rpc.call::<Vec<Vec<CallFrame>>>(endpoint, params);
    if let Some(frames) = result_or_record(report, endpoint, tx_index, response) {
        match frames.first().and_then(|bundle| bundle.first()) {
            Some(frame) => check_frame_as_transaction(report, endpoint, committed, index, frame),
            None => report.record(endpoint, tx_index, "re-run as call: trace", "a trace", "none"),
        }
    }

    // `eth_callMany` returns only the output or the error, a revert and a halt alike.
    let endpoint = "eth_callMany";
    let response = rpc.call::<Vec<Vec<EthCallResponse>>>(endpoint, json!([bundles, state_context]));
    if let Some(results) = result_or_record(report, endpoint, tx_index, response) {
        let result = results.first().and_then(|bundle| bundle.first());
        let success = result.map(|result| result.error.is_none());
        let expected = Some(committed.receipts[index].success);
        report.check_eq(endpoint, tx_index, "re-run as call: success", expected, success);
    }
}

/// The root frame of transaction `index` re-run as a call carries the transaction's gas used and
/// status. Its logs are compared only when both succeeded: the call tracer keeps a failed root
/// frame's own logs, while a failed transaction's receipt has none.
fn check_frame_as_transaction(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    index: usize,
    frame: &CallFrame,
) {
    let tx_index = Some(index);
    let receipt = &committed.receipts[index];
    let gas_used = U256::from(committed.gas_used(index));
    report.check_eq(endpoint, tx_index, "re-run as call: gas used", gas_used, frame.gas_used);
    let success = frame.error.is_none();
    report.check_eq(endpoint, tx_index, "re-run as call: success", receipt.success, success);
    if receipt.success && success {
        let logs = call_frame_logs(frame);
        report.check_eq(endpoint, tx_index, "re-run as call: logs", &receipt.logs, &logs);
    }
}

/// The call's logs in emission order. The call tracer (revm-inspectors) attaches each log to the
/// frame that emitted it and, like the receipt, drops the logs of a failed frame and of every
/// frame under it; each log's `index` counts all logs emitted before it in the call.
fn call_frame_logs(root: &CallFrame) -> Vec<Log> {
    let mut logs: Vec<CallLogFrame> = Vec::new();
    let mut frames = vec![root];
    while let Some(frame) = frames.pop() {
        logs.extend(frame.logs.iter().cloned());
        frames.extend(&frame.calls);
    }
    logs.sort_by_key(|log| log.index);
    logs.into_iter().map(CallLogFrame::into_log).collect()
}
