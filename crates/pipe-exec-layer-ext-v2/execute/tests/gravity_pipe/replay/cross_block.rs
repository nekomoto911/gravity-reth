//! Replays that span several committed blocks in one call, run once the timeline is done.

use super::{
    block_execution::check_execution_outcome,
    block_traces::check_parity_roots,
    committed::{ChangedState, Committed},
    result_or_record,
};
use crate::{node::CommittedBlock, report::MismatchReport, rpc::RpcClient, timeline::Phase};
use alloy_primitives::U64;
use alloy_rpc_types_trace::parity::LocalizedTransactionTrace;
use reth_ethereum_primitives::{Block, Receipt};
use reth_provider::{BlockReader, ChangeSetReader, StateProviderFactory, StorageChangeSetReader};
use serde_json::json;

/// The node's default `--rpc.max-trace-filter-blocks` caps `toBlock - fromBlock`.
const TRACE_FILTER_MAX_SPAN: usize = 100;

/// `reth_getBlockExecutionOutcome` executes at most this many blocks per call.
const EXECUTION_OUTCOME_MAX_BLOCKS: usize = 128;

/// `blocks` are every committed block of the timeline, in order, with the phase each was
/// built in.
pub(crate) fn check_blocks<P>(
    provider: &P,
    rpc: &RpcClient,
    blocks: &[(CommittedBlock, Phase)],
    report: &mut MismatchReport,
) where
    P: BlockReader<Block = Block, Receipt = Receipt>
        + StateProviderFactory
        + ChangeSetReader
        + StorageChangeSetReader,
{
    // Step 1: `trace_filter` over all blocks, in as few calls as the node allows; each block's
    // root traces match its receipts.
    let endpoint = "trace_filter";
    for chunk in blocks.chunks(TRACE_FILTER_MAX_SPAN) {
        let (first, last) = span(chunk);
        let params = json!([{ "fromBlock": U64::from(first), "toBlock": U64::from(last) }]);
        let response = rpc.call::<Vec<LocalizedTransactionTrace>>(endpoint, params);
        let mut range_report = report.for_blocks(first, last);
        let Some(traces) = result_or_record(&mut range_report, endpoint, None, response) else {
            continue
        };
        for (block, phase) in chunk {
            let block_traces: Vec<_> = traces
                .iter()
                .filter(|trace| trace.block_number == Some(block.number))
                .cloned()
                .collect();
            let committed = Committed::read(provider, block, *phase);
            let mut block_report = report.for_block(block.number, *phase);
            check_parity_roots(&mut block_report, endpoint, &committed, &block_traces);
        }
    }

    // Step 2: one `reth_getBlockExecutionOutcome` over as many blocks as it takes reproduces
    // every block's receipts and the state after the last one.
    let endpoint = "reth_getBlockExecutionOutcome";
    let range = &blocks[..blocks.len().min(EXECUTION_OUTCOME_MAX_BLOCKS)];
    let (first, last) = span(range);
    let receipts: Vec<Vec<Receipt>> = range
        .iter()
        .map(|(block, phase)| Committed::read(provider, block, *phase).receipts)
        .collect();
    let state = ChangedState::read(provider, first..=last);
    let response = rpc.call(endpoint, json!([U64::from(first), U64::from(range.len())]));
    let mut range_report = report.for_blocks(first, last);
    check_execution_outcome(&mut range_report, endpoint, first, &receipts, &state, response);
}

/// First and last block number of a non-empty run of consecutive blocks.
const fn span(blocks: &[(CommittedBlock, Phase)]) -> (u64, u64) {
    (blocks[0].0.number, blocks[blocks.len() - 1].0.number)
}
