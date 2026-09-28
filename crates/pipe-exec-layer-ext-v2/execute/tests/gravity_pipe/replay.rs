//! Replay check run after every committed block.
//!
//! Every RPC endpoint that re-executes a persisted block must reproduce what the pipe
//! committed. The committed result is read back from the node's storage (header,
//! receipts, changesets, historical state); the endpoints are called over HTTP, as on a
//! mainnet RPC node. Any difference, including an endpoint error, is recorded and the
//! timeline continues.

use crate::{report::BlockReport, rpc::RpcClient};
use alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_trace::{
    common::TraceResult,
    geth::CallFrame,
    opcode::BlockOpcodeGas,
    parity::{Delta, LocalizedTransactionTrace, StateDiff, TraceResultsWithTransactionHash},
};
use reth_ethereum_primitives::{Block, Receipt};
use reth_pipe_exec_layer_ext_v2::onchain_config::{
    NATIVE_ORACLE_ADDR, ORACLE_TASK_CONFIG_ADDR, TIMESTAMP_ADDR,
};
use reth_provider::{
    BlockReader, ChangeSetReader, StateProviderBox, StateProviderFactory, StorageChangeSetReader,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Accounts the chain writes outside any transaction of the block, so no per-transaction
/// state diff can show those writes:
/// - the EIP-2935 block hash contract: deployed on the Prague activation block, then written by a
///   system call before the first transaction of every block;
/// - `Timestamp`: updated by the `onBlockStart` call that a pre-Alpha DKG epoch-change block
///   executes but leaves out of its body;
/// - `NativeOracle` and `OracleTaskConfig`: runtime code replaced after the last transaction of the
///   Gamma activation block.
const WRITTEN_OUTSIDE_TRANSACTIONS: [Address; 4] =
    [HISTORY_STORAGE_ADDRESS, TIMESTAMP_ADDR, NATIVE_ORACLE_ADDR, ORACLE_TASK_CONFIG_ADDR];

/// Replays committed block `number` through every whole-block endpoint and records each
/// difference from the committed result.
pub(crate) fn check_block<P>(
    provider: &P,
    rpc: &RpcClient,
    number: u64,
    report: &mut BlockReport<'_>,
) where
    P: BlockReader<Block = Block, Receipt = Receipt>
        + StateProviderFactory
        + ChangeSetReader
        + StorageChangeSetReader,
{
    // Step 1: what the pipe committed.
    let committed = Committed::read(provider, number);
    let number_hex = format!("{number:#x}");

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
}

/// A block as the pipe committed it, read back from the node's storage.
struct Committed {
    hash: B256,
    state_root: B256,
    block: Block,
    tx_hashes: Vec<B256>,
    receipts: Vec<Receipt>,
    /// Accounts the block changed, with the storage slots it changed.
    changed_accounts: BTreeMap<Address, BTreeSet<B256>>,
    /// State after the block.
    post_state: StateProviderBox,
}

impl Committed {
    /// Panics when the node cannot serve its own committed block: that is a harness
    /// failure, not a replay mismatch.
    fn read<P>(provider: &P, number: u64) -> Self
    where
        P: BlockReader<Block = Block, Receipt = Receipt>
            + StateProviderFactory
            + ChangeSetReader
            + StorageChangeSetReader,
    {
        let hash = provider.block_hash(number).unwrap().expect("committed block hash");
        let block = provider.block_by_number(number).unwrap().expect("committed block");
        let receipts =
            provider.receipts_by_block(number.into()).unwrap().expect("committed receipts");
        let tx_hashes: Vec<B256> = block.body.transactions.iter().map(|tx| *tx.tx_hash()).collect();
        assert_eq!(receipts.len(), tx_hashes.len(), "block {number}: one receipt per transaction");

        // Changesets hold the pre-block values of whatever the block changed; the current
        // values come from the state after the block.
        let mut changed_accounts: BTreeMap<Address, BTreeSet<B256>> = provider
            .account_block_changeset(number)
            .unwrap()
            .into_iter()
            .map(|change| (change.address, BTreeSet::new()))
            .collect();
        for (key, entry) in provider.storage_changeset(number).unwrap() {
            changed_accounts.entry(key.address()).or_default().insert(entry.key);
        }

        Self {
            hash,
            state_root: block.header.state_root,
            block,
            tx_hashes,
            receipts,
            changed_accounts,
            post_state: provider.history_by_block_number(number).unwrap(),
        }
    }

    fn gas_used(&self, tx_index: usize) -> u64 {
        let before = tx_index.checked_sub(1).map_or(0, |i| self.receipts[i].cumulative_gas_used);
        self.receipts[tx_index].cumulative_gas_used - before
    }

    fn post_account(&self, address: &Address) -> PostAccount {
        let account = self.post_state.basic_account(address).unwrap().unwrap_or_default();
        let code = self.post_state.account_code(address).unwrap();
        PostAccount {
            balance: account.balance,
            nonce: account.nonce,
            code: code.map(|code| code.original_bytes()).unwrap_or_default(),
        }
    }

    fn post_storage(&self, address: &Address, slot: &B256) -> B256 {
        self.post_state.storage(*address, *slot).unwrap().unwrap_or_default().into()
    }
}

/// An account after the block; a missing account reads as empty.
struct PostAccount {
    balance: U256,
    nonce: u64,
    code: Bytes,
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

/// Geth-style traces use the call tracer: its root frame carries the transaction's gas
/// used and error. The default struct-log tracer carries the same two values but logs
/// every opcode, which makes a replay of a system transaction take seconds.
fn call_tracer_options() -> Value {
    json!({ "tracer": "callTracer" })
}

/// Returns the result, or records the endpoint's error and returns `None`.
fn result_or_record<T>(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    response: Result<T, String>,
) -> Option<T> {
    response.map_err(|error| report.record(endpoint, None, "response", "a result", error)).ok()
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

fn check_call_traces(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    response: Result<Vec<TraceResult<CallFrame, String>>, String>,
) {
    let Some(traces) = result_or_record(report, endpoint, response) else { return };
    let hashes: Vec<_> = traces.iter().map(TraceResult::tx_hash).collect();
    check_tx_hashes(report, endpoint, committed, &hashes);

    for (index, (trace, receipt)) in traces.iter().zip(&committed.receipts).enumerate() {
        match trace {
            TraceResult::Success { result, .. } => {
                let gas_used = U256::from(committed.gas_used(index));
                report.check_eq(endpoint, Some(index), "gas used", gas_used, result.gas_used);
                let success = result.error.is_none();
                report.check_eq(endpoint, Some(index), "success", receipt.success, success);
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
fn check_parity_traces(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    response: Result<Vec<LocalizedTransactionTrace>, String>,
) {
    let Some(traces) = result_or_record(report, endpoint, response) else { return };
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

fn check_replayed_transactions(
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

    // Rule 2: everything the block's transactions changed shows up in some diff.
    let tx_changes = committed
        .changed_accounts
        .iter()
        .filter(|(address, _)| !WRITTEN_OUTSIDE_TRANSACTIONS.contains(address));
    for (address, slots) in tx_changes {
        let Some(account) = diffed.get(address) else {
            let field = format!("{address}");
            report.record(endpoint, None, field, "changed by the block", "in no state diff");
            continue;
        };
        for slot in slots.iter().filter(|slot| !account.storage.contains_key(*slot)) {
            let expected = committed.post_storage(address, slot);
            let field = format!("{address} slot {slot}");
            report.record(endpoint, None, field, expected, "in no state diff");
        }
    }
}

/// Opcode gas excludes intrinsic gas and refunds, so only the transaction list is
/// comparable.
fn check_opcode_gas(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    response: Result<Option<BlockOpcodeGas>, String>,
) {
    let Some(opcode_gas) = result_or_record(report, endpoint, response) else { return };
    let Some(opcode_gas) = opcode_gas else {
        report.record(endpoint, None, "response", "a block", "null");
        return;
    };
    report.check_eq(endpoint, None, "block hash", committed.hash, opcode_gas.block_hash);
    let hashes: Vec<_> =
        opcode_gas.transactions.iter().map(|tx| Some(tx.transaction_hash)).collect();
    check_tx_hashes(report, endpoint, committed, &hashes);
}

fn check_intermediate_roots(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    response: Result<Vec<B256>, String>,
) {
    let Some(roots) = result_or_record(report, endpoint, response) else { return };
    report.check_eq(endpoint, None, "root count", committed.tx_hashes.len(), roots.len());
    if let Some(last) = roots.last() {
        let index = roots.len() - 1;
        report.check_eq(endpoint, Some(index), "state root", committed.state_root, *last);
    }
}
