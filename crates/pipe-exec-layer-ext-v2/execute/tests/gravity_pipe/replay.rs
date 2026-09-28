//! Replay check run after every committed block.
//!
//! Every RPC endpoint that re-executes a persisted block must reproduce what the pipe
//! committed. The committed result is read back from the node's storage (header,
//! receipts, changesets, historical state); the endpoints are called over HTTP, as on a
//! mainnet RPC node. Any difference, including an endpoint error, is recorded and the
//! timeline continues.

use crate::{
    node::CommittedBlock,
    report::BlockReport,
    rpc::RpcClient,
    timeline::{Fork, Phase},
};
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
    NATIVE_ORACLE_ADDR, ORACLE_TASK_CONFIG_ADDR, SYSTEM_CALLER, TIMESTAMP_ADDR,
};
use reth_primitives_traits::Account;
use reth_provider::{
    BlockReader, ChangeSetReader, StateProviderBox, StateProviderFactory, StorageChangeSetReader,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Replays a committed block through every whole-block endpoint and records each
/// difference from the committed result.
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
}

/// A block as the pipe committed it, read back from the node's storage.
struct Committed {
    hash: B256,
    state_root: B256,
    block: Block,
    tx_hashes: Vec<B256>,
    receipts: Vec<Receipt>,
    /// Accounts the block changed, keyed by address.
    changed_accounts: BTreeMap<Address, ChangedAccount>,
    /// Account fields the block wrote outside its transactions.
    outside_writes: Vec<(Address, Field)>,
    /// State after the block.
    post_state: StateProviderBox,
}

impl Committed {
    /// Panics when the node cannot serve its own committed block: that is a harness
    /// failure, not a replay mismatch.
    fn read<P>(provider: &P, pipe_block: &CommittedBlock, phase: Phase) -> Self
    where
        P: BlockReader<Block = Block, Receipt = Receipt>
            + StateProviderFactory
            + ChangeSetReader
            + StorageChangeSetReader,
    {
        let number = pipe_block.number;
        let hash = provider.block_hash(number).unwrap().expect("committed block hash");
        let block = provider.block_by_number(number).unwrap().expect("committed block");
        let receipts =
            provider.receipts_by_block(number.into()).unwrap().expect("committed receipts");
        let tx_hashes: Vec<B256> = block.body.transactions.iter().map(|tx| *tx.tx_hash()).collect();
        assert_eq!(receipts.len(), tx_hashes.len(), "block {number}: one receipt per transaction");

        // Changesets hold the pre-block values of whatever the block changed; the current
        // values come from the state after the block. An account whose info has no changeset
        // row only had its storage changed.
        let post_state = provider.history_by_block_number(number).unwrap();
        let mut changed_accounts: BTreeMap<Address, ChangedAccount> = provider
            .account_block_changeset(number)
            .unwrap()
            .into_iter()
            .map(|change| {
                let pre = change.info.unwrap_or_default();
                (change.address, ChangedAccount { pre, slots: BTreeSet::new() })
            })
            .collect();
        for (key, entry) in provider.storage_changeset(number).unwrap() {
            let address = key.address();
            changed_accounts
                .entry(address)
                .or_insert_with(|| ChangedAccount {
                    pre: post_state.basic_account(&address).unwrap().unwrap_or_default(),
                    slots: BTreeSet::new(),
                })
                .slots
                .insert(entry.key);
        }

        Self {
            hash,
            state_root: block.header.state_root,
            block,
            tx_hashes,
            receipts,
            changed_accounts,
            outside_writes: written_outside_transactions(phase, pipe_block.epoch_changed),
            post_state,
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
            code_hash: account.get_bytecode_hash(),
            code: code.map(|code| code.original_bytes()).unwrap_or_default(),
        }
    }

    fn post_storage(&self, address: &Address, slot: &B256) -> B256 {
        self.post_state.storage(*address, *slot).unwrap().unwrap_or_default().into()
    }
}

/// An account the block changed.
struct ChangedAccount {
    /// Account info before the block; a missing account reads as empty.
    pre: Account,
    /// Storage slots the block changed.
    slots: BTreeSet<B256>,
}

/// A part of an account that the chain may write outside the block's transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Balance,
    Nonce,
    Code,
    Storage,
}

/// An account after the block; a missing account reads as empty.
struct PostAccount {
    balance: U256,
    nonce: u64,
    code_hash: B256,
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

/// Account fields the chain writes outside the block's transactions, so no per-transaction
/// state diff can reveal them. Each write is scoped to the blocks that make it.
fn written_outside_transactions(phase: Phase, epoch_changed: bool) -> Vec<(Address, Field)> {
    let mut writes = Vec::new();
    // `eip_2935::apply_state_changes_for_block` deploys the block hash contract (nonce 1,
    // code) before the first transaction of the Prague activation block.
    if phase == Phase::Activation(Fork::Prague) {
        writes.extend([
            (HISTORY_STORAGE_ADDRESS, Field::Nonce),
            (HISTORY_STORAGE_ADDRESS, Field::Code),
        ]);
    }
    // From then on the executor's pre-execution system call stores the parent id in the block
    // hash contract. An epoch-change block is assembled from its system transactions alone and
    // never runs the executor, so it writes no slot.
    if phase.has_activated(Fork::Prague) && !epoch_changed {
        writes.push((HISTORY_STORAGE_ADDRESS, Field::Storage));
    }
    // A pre-Alpha DKG epoch-change block executes `onBlockStart`, which updates the global
    // time, but keeps only the DKG transaction in its body.
    if epoch_changed && !phase.has_activated(Fork::Alpha) {
        writes.push((TIMESTAMP_ADDR, Field::Storage));
    }
    // `system_caller_migration` zeroes SYSTEM_CALLER's balance before the first transaction
    // of the Alpha activation block; its gas-exempt transactions leave it unchanged.
    if phase == Phase::Activation(Fork::Alpha) {
        writes.push((SYSTEM_CALLER, Field::Balance));
    }
    // The Gamma hook replaces two oracle runtimes after the last transaction of the first
    // executed block at or after gammaTime, which is the activation block: activation blocks
    // never change epoch.
    if phase == Phase::Activation(Fork::Gamma) {
        writes.extend([(NATIVE_ORACLE_ADDR, Field::Code), (ORACLE_TASK_CONFIG_ADDR, Field::Code)]);
    }
    writes
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
