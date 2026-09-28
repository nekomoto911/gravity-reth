//! Replay check run after every committed block.
//!
//! Every RPC endpoint that re-executes a persisted block must reproduce what the pipe
//! committed. The committed result is read back from the node's storage (header,
//! receipts, changesets, historical state); the endpoints are called over HTTP, as on a
//! mainnet RPC node. Any difference, including an endpoint error, is recorded and the
//! timeline continues.

mod block_traces;
mod committed;
mod contract_creator;
mod intermediate_roots;
mod state_diff;
mod transaction;

use crate::{node::CommittedBlock, report::BlockReport, rpc::RpcClient, timeline::Phase};
use alloy_primitives::{Bytes, B256, U256};
use alloy_rpc_types_trace::geth::CallFrame;
use block_traces::{check_call_traces, check_opcode_gas, check_parity_traces};
use committed::Committed;
use contract_creator::check_contract_creators;
use intermediate_roots::check_intermediate_roots;
use reth_ethereum_primitives::{Block, Receipt};
use reth_provider::{BlockReader, ChangeSetReader, StateProviderFactory, StorageChangeSetReader};
use serde_json::{json, Value};
use state_diff::check_replayed_transactions;
use transaction::check_transaction;

/// Replays a committed block through every replay endpoint, whole-block and per
/// transaction, and records each difference from the committed result.
pub(crate) fn check_block<P>(
    provider: &P,
    rpc: &RpcClient,
    block: &CommittedBlock,
    phase: Phase,
    report: &mut BlockReport<'_>,
) where
    P: BlockReader<Block = Block, Receipt = Receipt>
        + StateProviderFactory
        + ChangeSetReader
        + StorageChangeSetReader,
{
    // Step 1: what the pipe committed.
    let committed = Committed::read(provider, block, phase);
    let number_hex = format!("{:#x}", block.number);

    // Step 2: whole-block traces. Geth-style traces expose each transaction's gas and
    // success; parity-style traces expose success, and the state diffs of all
    // transactions together must reach the committed post-block state.
    for (endpoint, block) in [
        ("debug_traceBlockByHash", json!(committed.hash)),
        ("debug_traceBlockByNumber", json!(number_hex)),
        ("debug_traceBlock", json!(Bytes::from(alloy_rlp::encode(&committed.block)))),
    ] {
        let response = rpc.call(endpoint, json!([block, call_tracer_options()]));
        check_call_traces(report, endpoint, &committed, response);
    }
    for (endpoint, params) in [
        ("trace_block", json!([number_hex])),
        ("trace_filter", json!([{ "fromBlock": number_hex, "toBlock": number_hex }])),
    ] {
        check_parity_traces(report, endpoint, &committed, rpc.call(endpoint, params));
    }
    let endpoint = "trace_replayBlockTransactions";
    let response = rpc.call(endpoint, json!([number_hex, ["trace", "stateDiff"]]));
    check_replayed_transactions(report, endpoint, &committed, response);
    let endpoint = "trace_blockOpcodeGas";
    check_opcode_gas(report, endpoint, &committed, rpc.call(endpoint, json!([number_hex])));

    // Step 3: the state root after each transaction ends at the committed state root.
    let endpoint = "debug_intermediateRoots";
    let response = rpc.call(endpoint, json!([committed.hash]));
    check_intermediate_roots(report, endpoint, &committed, response);

    // Step 4: every transaction replayed on its own matches its receipt.
    for index in 0..committed.tx_hashes.len() {
        check_transaction(report, rpc, &committed, index);
    }

    // Step 5: every contract the block created names its creating transaction.
    check_contract_creators(report, rpc, &committed);
}

/// Geth-style traces use the call tracer: its root frame carries the transaction's gas
/// used and error. The default struct-log tracer carries the same two values but logs
/// every opcode, which makes a replay of a system transaction take seconds.
fn call_tracer_options() -> Value {
    json!({ "tracer": "callTracer" })
}

/// A call-trace root frame carries the transaction's gas used and its error.
fn check_call_frame(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    tx_index: usize,
    frame: &CallFrame,
) {
    let gas_used = U256::from(committed.gas_used(tx_index));
    report.check_eq(endpoint, Some(tx_index), "gas used", gas_used, frame.gas_used);
    let success = frame.error.is_none();
    let expected = committed.receipts[tx_index].success;
    report.check_eq(endpoint, Some(tx_index), "success", expected, success);
}

/// Returns the result, or records the endpoint's error and returns `None`.
fn result_or_record<T>(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    tx_index: Option<usize>,
    response: Result<T, String>,
) -> Option<T> {
    response.map_err(|error| report.record(endpoint, tx_index, "response", "a result", error)).ok()
}

/// The endpoint reported exactly the block's transactions, in order.
fn check_tx_hashes(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    reported: &[Option<B256>],
) {
    report.check_eq(endpoint, None, "transaction count", committed.tx_hashes.len(), reported.len());
    for (index, (expected, actual)) in committed.tx_hashes.iter().zip(reported).enumerate() {
        report.check_eq(endpoint, Some(index), "transaction hash", Some(*expected), *actual);
    }
}
