//! Whole-block traces: each transaction's trace must match its committed receipt.

use super::{check_call_frame, check_tx_hashes, committed::Committed, result_or_record};
use crate::report::BlockReport;
use alloy_rpc_types_trace::{
    common::TraceResult, geth::CallFrame, opcode::BlockOpcodeGas, parity::LocalizedTransactionTrace,
};

pub(super) fn check_call_traces(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    response: Result<Vec<TraceResult<CallFrame, String>>, String>,
) {
    let Some(traces) = result_or_record(report, endpoint, None, response) else { return };
    let hashes: Vec<_> = traces.iter().map(TraceResult::tx_hash).collect();
    check_tx_hashes(report, endpoint, committed, &hashes);

    for (index, trace) in traces.iter().take(committed.receipts.len()).enumerate() {
        match trace {
            TraceResult::Success { result, .. } => {
                check_call_frame(report, endpoint, committed, index, result)
            }
            TraceResult::Error { error, .. } => {
                report.record(endpoint, Some(index), "trace", "a trace", error)
            }
        }
    }
}

/// Parity-style traces list every call frame; a transaction's root frame has an empty
/// trace address. The root's `gasUsed` excludes intrinsic gas and refunds, so it is not
/// the receipt's gas and only success is compared.
pub(super) fn check_parity_traces(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    response: Result<Vec<LocalizedTransactionTrace>, String>,
) {
    let Some(traces) = result_or_record(report, endpoint, None, response) else { return };
    check_parity_roots(report, endpoint, committed, &traces);
}

/// `traces` are the parity traces an endpoint reported for the committed block.
pub(super) fn check_parity_roots(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    traces: &[LocalizedTransactionTrace],
) {
    let roots: Vec<_> = traces
        .iter()
        .filter(|trace| {
            trace.transaction_position.is_some() && trace.trace.trace_address.is_empty()
        })
        .collect();
    let hashes: Vec<_> = roots.iter().map(|root| root.transaction_hash).collect();
    check_tx_hashes(report, endpoint, committed, &hashes);

    for (index, (root, receipt)) in roots.iter().zip(&committed.receipts).enumerate() {
        let success = root.trace.error.is_none();
        report.check_eq(endpoint, Some(index), "success", receipt.success, success);
    }
}

/// Opcode gas excludes intrinsic gas and refunds, so only the transaction list is
/// comparable.
pub(super) fn check_opcode_gas(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    response: Result<Option<BlockOpcodeGas>, String>,
) {
    let Some(opcode_gas) = result_or_record(report, endpoint, None, response) else { return };
    let Some(opcode_gas) = opcode_gas else {
        report.record(endpoint, None, "response", "a block", "null");
        return;
    };
    report.check_eq(endpoint, None, "block hash", committed.hash, opcode_gas.block_hash);
    let hashes: Vec<_> =
        opcode_gas.transactions.iter().map(|tx| Some(tx.transaction_hash)).collect();
    check_tx_hashes(report, endpoint, committed, &hashes);
}
