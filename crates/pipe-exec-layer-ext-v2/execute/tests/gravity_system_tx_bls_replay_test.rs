#![allow(
    missing_docs,
    clippy::doc_markdown,
    clippy::doc_lazy_continuation,
    clippy::missing_const_for_fn
)]

//! §3.5 — BLS pop-verify precompile RPC replay byte-equal canonical (must-pass).
//!
//! Pins three claims from the acceptance matrix (§3.5 in
//! `acceptance-tests-2026-06-26.md`), which gate commit `23a55587c4`
//! ("fix(rpc): register BLS pop-verify precompile unconditionally"):
//!
//!   1. **Post-Alpha block, block-family replay**: a historical block whose timestamp is
//!      Alpha-active and that contains a user tx calling `BLS_PRECOMPILE_ADDR = 0x…625f5001`, when
//!      re-traced via `trace_block` / `debug_traceBlock` (callTracer), must reproduce
//!      **byte-equal** to canonical execution:
//!        - top-level `CallFrame.to       == BLS_PRECOMPILE_ADDR`,
//!        - `CallFrame.gas_used`            == canonical receipt per-tx gas,
//!        - `CallFrame.output`              == canonical 32-byte BLS result,
//!        - `CallFrame.error               == None` (no `not-precompile` / OOG divergence).
//!      A divergence on any of these would prove the RPC layer is treating the
//!      precompile address as an empty account during replay.
//!
//!   2. **Pre-Alpha block, block-family replay** (**critical**): same fixture shape but on a block
//!      whose timestamp predates Alpha activation. Commit `23a55587c4` registers BLS
//!      *unconditionally* (no Alpha gate) — mirroring the pipe execution layer's
//!      `pre_alpha_precompiles`. Without that fix, RPC replay of any pre-Alpha block that called
//!      BLS would route the call to an empty account (no precompile dispatch, no 110_000 gas
//!      charge), diverging from canonical for the entire pre-Alpha history. This test is the
//!      long-term regression guard against re-gating BLS behind Alpha.
//!
//!   3. **Single-tx `debug_traceTransaction` replay**: same fixture, but the single-tx endpoint
//!      (`debug_traceTransaction` with callTracer) is exercised on the BLS tx hash directly. Same
//!      byte-equality assertions — pinning that the single-tx replay code path also dispatches the
//!      unconditional BLS registration (covers the `call.rs:733-807` `replay_transactions_until`
//!      family of `inspect` callers).
//!
//!   4. **#441 — replays outside the trace family**: on a block `[onBlockStart, BLS call,
//!      transfer]`, `debug_accountAt` / `debug_accountInfoAt`, `eth_callMany`,
//!      `spawn_replay_transaction` (behind `ots_getTransactionError`), `debug_intermediateRoots`
//!      and `debug_traceCallMany` must reproduce canonical execution before and after Alpha.
//!
//! Location note: §3.5 nominally targets `crates/rpc/rpc/tests/` but the
//! reth-rpc crate has no `tests/` directory and the pipe-exec-layer harness
//! already exposes the full RPC registry (`handle.node.rpc_registry.debug_api()`
//! / `.trace_api()`). The sibling tests `gravity_system_tx_pre_alpha_replay_test.rs`
//! and `gravity_system_tx_post_alpha_trace_test.rs` co-locate here for the same
//! reason.
//!
//! BLS fixture note: the test reuses the *same* fixture style as the unit test
//! in `gravity_precompiles::bls_pop_verify::tests` — 144 bytes of input. Since
//! the byte-equality assertion compares pipe canonical execution against RPC
//! replay (both run the same handler), the *content* of the input does not need
//! to be a valid PoP: 144 zero bytes deterministically returns a 32-byte
//! `0x00..00` and a `gas_used == POP_VERIFY_GAS = 110_000`. This mirrors
//! `gravity_bls_precompile_test.rs::POISON_GAS_LIMIT` style: any 144-byte
//! buffer is enough to drive the precompile through its full code path.

use alloy_consensus::{BlockHeader, SignableTransaction, Transaction as _, TxEip1559};
use alloy_eips::BlockId;
use alloy_primitives::{address, b256, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_rpc_types_eth::{
    state::{AccountOverride, StateOverride},
    Bundle, Index, StateContext, TransactionIndex, TransactionInput, TransactionRequest,
};
use alloy_rpc_types_trace::geth::{
    call::CallConfig, CallFrame, GethDebugTracingCallOptions, GethDebugTracingOptions, GethTrace,
    TraceResult,
};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_macro::sol;
use alloy_sol_types::SolCall;
use gravity_api_types::{
    config_storage::{BlockNumber, ConfigStorage, OnChainConfig},
    events::contract_event::GravityEvent,
};
use gravity_precompiles::bls_pop_verify::POP_VERIFY_GAS;
use gravity_storage::{block_view_storage::BlockViewStorage, GravityStorage};
use reth_chainspec::ChainSpec;
use reth_cli_commands::{launcher::FnLauncher, NodeCommand};
use reth_cli_runner::CliRunner;
use reth_db::DatabaseEnv;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_ethereum_primitives::{Transaction, TransactionSigned};
use reth_node_builder::{EngineNodeLauncher, NodeBuilder, WithLaunchContext};
use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
use reth_pipe_exec_layer_ext_v2::{
    new_pipe_exec_layer_api,
    onchain_config::{SYSTEM_CALLER, TIMESTAMP_ADDR},
    ExecutionArgs, OrderedBlock, PipeExecLayerApi,
};
use reth_provider::{
    providers::BlockchainProvider, BlockHashReader, BlockNumReader, BlockReader,
    DatabaseProviderFactory, HeaderProvider, ReceiptProvider, StateProviderFactory,
    TransactionVariant,
};
use reth_rpc_eth_api::{
    helpers::{Call, EthCall},
    RpcTypes,
};
use reth_tracing::{
    tracing_subscriber::filter::LevelFilter, LayerInfo, LogFormat, RethTracer, Tracer,
};
use revm::Database as _;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// chainId from the embedded `gravity_hardfork.json`.
const CHAIN_ID: u64 = 7771625;

/// `BLS_PRECOMPILE_ADDR` from `gravity_precompiles::bls_pop_verify`. Re-declared
/// here as a local constant to avoid pulling `gravity-precompiles` into this
/// crate's dev-deps just for the literal (sibling test
/// `gravity_bls_precompile_test.rs` uses the same approach).
const BLS_PRECOMPILE_ADDR: Address = address!("00000000000000000000000000000001625f5001");

/// Input length the BLS precompile expects: pubkey(48) + pop(96) = 144 bytes.
/// Must match exactly to pass the handler's length check and traverse the full
/// verification path (the content does not need to be a valid PoP for the
/// byte-equal canonical assertion to be meaningful — see file-level docs).
const BLS_INPUT_LEN: usize = 144;

/// gas_limit for the BLS user tx: intrinsic (~21k base + ~580 for 144 zero
/// calldata bytes) + `POP_VERIFY_GAS` headroom. 200_000 is well above and not
/// dropped by `filter_invalid_txs`.
const BLS_TX_GAS_LIMIT: u64 = 200_000;

/// Anvil account 0 — pre-funded in `gravity_hardfork.json` (matches
/// `gravity_bls_precompile_test::FUNDED_PRIVKEY_HEX`).
const FUNDED_PRIVKEY_HEX: &[u8; 32] = &[
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
];

const TS_BASE: u64 = 2_000_000_000;
const ALPHA_TIME_ALWAYS: u64 = 1;
/// `alphaTime` far in the future so no block in the test transitions into
/// Alpha — every pushed block timestamp is strictly pre-Alpha.
const ALPHA_TIME_NEVER: u64 = 9_999_999_999;
/// Block at which the BLS user tx is injected. We push empty blocks 1..(N-1)
/// first so the chain has stabilised, then inject at this block number.
const BLS_BLOCK_NUMBER: u64 = 5;
/// Anvil account 1. Only `gravity_alpha_chainspec_with_second_sender` funds it, so the transfer
/// it sends after the BLS call never touches the BLS sender's account.
const SECOND_PRIVKEY: B256 =
    b256!("59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d");
const SECOND_SENDER_ALLOC_KEY: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";
/// Recipient of the 1-wei transfer that closes the #441 fixture block.
const TRANSFER_RECIPIENT: Address = address!("0000000000000000000000000000000000c0ffee");
/// Address that the state override turns into a "return the target's balance" probe.
const BALANCE_PROBE: Address = address!("0000000000000000000000000000000000ba1a0e");

sol! {
    function nowMicroseconds() external view returns (uint64);
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn gravity_alpha_chainspec(alpha_time: u64) -> String {
    let mut json: serde_json::Value =
        serde_json::from_str(include_str!("../gravity_hardfork.json"))
            .expect("gravity_hardfork.json must parse as JSON");
    json["config"]["alphaTime"] = serde_json::json!(alpha_time);
    json.to_string()
}

fn funded_signer() -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::from(*FUNDED_PRIVKEY_HEX))
        .expect("funded test key must parse")
}

/// Same chain spec, plus a funded Anvil account 1 that sends the #441 fixture's transfer.
fn gravity_alpha_chainspec_with_second_sender(alpha_time: u64) -> String {
    let mut json: serde_json::Value = serde_json::from_str(&gravity_alpha_chainspec(alpha_time))
        .expect("chain spec must round-trip as JSON");
    json["alloc"][SECOND_SENDER_ALLOC_KEY] =
        serde_json::json!({ "balance": "0x3635c9adc5dea00000" });
    json.to_string()
}

fn second_signer() -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&SECOND_PRIVKEY).expect("second test key must parse")
}

fn mock_block_id(block_number: u64) -> B256 {
    B256::left_padding_from(&block_number.to_be_bytes())
}

fn ts_us(block_number: u64) -> u64 {
    (TS_BASE + block_number) * 1_000_000
}

fn sign_eip1559(sender: &PrivateKeySigner, tx: TxEip1559) -> TransactionSigned {
    let signature: Signature =
        sender.sign_hash_sync(&tx.signature_hash()).expect("tx signing must succeed");
    TransactionSigned::new_unhashed(Transaction::Eip1559(tx), signature)
}

/// Build and sign an EIP-1559 transaction calling `BLS_PRECOMPILE_ADDR` with
/// `BLS_INPUT_LEN` bytes of input. EIP-1559 (London) is active at block 0 per
/// `gravity_hardfork.json` (`londonBlock = 0`); no fork-activation orchestration
/// required.
fn build_bls_call_tx(
    sender: &PrivateKeySigner,
    nonce: u64,
    input: Bytes,
) -> (TransactionSigned, Address) {
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce,
        gas_limit: BLS_TX_GAS_LIMIT,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(BLS_PRECOMPILE_ADDR),
        value: U256::ZERO,
        access_list: Default::default(),
        input,
    };
    (sign_eip1559(sender, tx), sender.address())
}

/// Signs a 1-wei EIP-1559 transfer to `TRANSFER_RECIPIENT`.
fn build_transfer_tx(sender: &PrivateKeySigner, nonce: u64) -> (TransactionSigned, Address) {
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(TRANSFER_RECIPIENT),
        value: U256::from(1),
        access_list: Default::default(),
        input: Bytes::new(),
    };
    (sign_eip1559(sender, tx), sender.address())
}

/// Runtime code that returns `target`'s balance as one 32-byte word:
/// PUSH20 target, BALANCE, PUSH1 0, MSTORE, PUSH1 32, PUSH1 0, RETURN.
fn balance_probe_override(target: Address) -> StateOverride {
    let mut code = vec![0x73];
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[0x31, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]);
    let mut overrides = StateOverride::default();
    overrides
        .insert(BALANCE_PROBE, AccountOverride { code: Some(code.into()), ..Default::default() });
    overrides
}

/// One bundle: read the probed balance, then `Timestamp.nowMicroseconds()`.
fn probe_bundle() -> Vec<Bundle<TransactionRequest>> {
    let probe = TransactionRequest { to: Some(TxKind::Call(BALANCE_PROBE)), ..Default::default() };
    let now = TransactionRequest {
        to: Some(TxKind::Call(TIMESTAMP_ADDR)),
        input: TransactionInput::new(nowMicrosecondsCall {}.abi_encode().into()),
        ..Default::default()
    };
    vec![Bundle { transactions: vec![probe, now], block_override: None }]
}

fn empty_ordered_block(
    epoch: u64,
    block_number: u64,
    block_id: B256,
    parent_block_id: B256,
    timestamp_us: u64,
) -> OrderedBlock {
    OrderedBlock {
        failed_proposer_indices: vec![],
        epoch,
        parent_id: parent_block_id,
        id: block_id,
        number: block_number,
        timestamp_us,
        coinbase: Address::ZERO,
        prev_randao: B256::ZERO,
        withdrawals: Default::default(),
        transactions: vec![],
        senders: vec![],
        proposer_index: Some(0),
        extra_data: vec![],
        randomness: U256::ZERO,
    }
}

fn ordered_block_with_txs(
    epoch: u64,
    block_number: u64,
    block_id: B256,
    parent_block_id: B256,
    timestamp_us: u64,
    transactions: Vec<TransactionSigned>,
    senders: Vec<Address>,
) -> OrderedBlock {
    OrderedBlock {
        failed_proposer_indices: vec![],
        epoch,
        parent_id: parent_block_id,
        id: block_id,
        number: block_number,
        timestamp_us,
        coinbase: Address::ZERO,
        prev_randao: B256::ZERO,
        withdrawals: Default::default(),
        transactions,
        senders,
        proposer_index: Some(0),
        extra_data: vec![],
        randomness: U256::ZERO,
    }
}

// ---------------------------------------------------------------------------
// MockConsensus (mirrors `gravity_bls_precompile_test::MockConsensus` to keep
// the harness self-contained — the existing tests are intentionally
// non-sharing under Cargo's tests-as-binaries model).
// ---------------------------------------------------------------------------

type TimestampFn = Box<dyn Fn(u64) -> u64 + Send + Sync>;

struct MockConsensus<Storage, EthApi> {
    pipeline_api: PipeExecLayerApi<Storage, EthApi>,
    ts_for_block: TimestampFn,
}

impl<Storage, EthApi> MockConsensus<Storage, EthApi>
where
    Storage: GravityStorage,
    EthApi: EthCall,
    EthApi::NetworkTypes: RpcTypes<TransactionRequest = TransactionRequest>,
{
    fn new(pipeline_api: PipeExecLayerApi<Storage, EthApi>, ts_for_block: TimestampFn) -> Self {
        Self { pipeline_api, ts_for_block }
    }

    async fn push_empty_range(&self, epoch: &mut u64, start: u64, end: u64) {
        for n in start..=end {
            let block = empty_ordered_block(
                *epoch,
                n,
                mock_block_id(n),
                mock_block_id(n - 1),
                (self.ts_for_block)(n),
            );
            self.push_one(epoch, block).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn push_one(
        &self,
        epoch: &mut u64,
        block: OrderedBlock,
    ) -> reth_pipe_exec_layer_ext_v2::ExecutionResult {
        let block_id = block.id;
        let block_number = block.number;
        self.pipeline_api.push_ordered_block(block).unwrap();
        let result = self.pipeline_api.pull_executed_block_hash().await.unwrap();
        assert_eq!(result.block_number, block_number);
        assert_eq!(result.block_id, block_id);
        self.pipeline_api.commit_executed_block_hash(block_id, Some(result.block_hash)).unwrap();

        for event in &result.gravity_events {
            if let GravityEvent::NewEpoch(new_epoch, _) = event {
                assert_eq!(*new_epoch, *epoch + 1);
                self.pipeline_api.wait_for_block_persistence(block_number).await.unwrap();
                self.pipeline_api
                    .push_ordered_block(empty_ordered_block(
                        *epoch,
                        block_number + 1,
                        mock_block_id(block_number + 1),
                        block_id,
                        (self.ts_for_block)(block_number + 1),
                    ))
                    .unwrap();
                *epoch = *new_epoch;
            }
        }
        result
    }

    fn into_inner(self) -> PipeExecLayerApi<Storage, EthApi> {
        self.pipeline_api
    }
}

// ---------------------------------------------------------------------------
// Canonical per-tx gas: receipts hold cumulative_gas_used; the user BLS tx is
// at idx 1 (after the protocol-injected metadata system tx at idx 0).
// ---------------------------------------------------------------------------

fn canonical_per_tx_gas<R>(receipts: &[R], tx_idx: usize) -> u64
where
    R: HasCumulativeGas,
{
    let cum_at = receipts[tx_idx].cumulative_gas_used_u64();
    if tx_idx == 0 {
        cum_at
    } else {
        cum_at - receipts[tx_idx - 1].cumulative_gas_used_u64()
    }
}

trait HasCumulativeGas {
    fn cumulative_gas_used_u64(&self) -> u64;
}

impl HasCumulativeGas for reth_ethereum_primitives::Receipt {
    fn cumulative_gas_used_u64(&self) -> u64 {
        self.cumulative_gas_used
    }
}

/// Extract the top-level `CallFrame` from a `TraceResult::Success` produced by
/// `debug_traceBlock` with the `callTracer`. Panics with a useful message on
/// `TraceResult::Error` or mismatching variant — both would point at a regression
/// (either fee/precompile dispatch failure, or accidental tracer mux).
fn expect_call_frame(entry: &TraceResult, ctx: &str) -> CallFrame {
    match entry {
        TraceResult::Success { result, .. } => match result {
            GethTrace::CallTracer(frame) => frame.clone(),
            other => panic!("[{ctx}] expected GethTrace::CallTracer variant, got {other:?}",),
        },
        TraceResult::Error { error, .. } => {
            panic!("[{ctx}] debug_traceBlock returned Error variant: {error}");
        }
    }
}

/// Recurse into a CallFrame tree to find the frame whose `to` equals
/// `BLS_PRECOMPILE_ADDR`. Returns `None` if BLS is never reached (which is the
/// pre-fix pre-Alpha failure mode — call routed to an empty account → no
/// precompile dispatch → no nested BLS frame produced).
fn find_bls_frame(frame: &CallFrame) -> Option<CallFrame> {
    if frame.to == Some(BLS_PRECOMPILE_ADDR) {
        return Some(frame.clone());
    }
    for child in &frame.calls {
        if let Some(found) = find_bls_frame(child) {
            return Some(found);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Core runner: push (BLS_BLOCK_NUMBER - 1) empty blocks then a block containing
// one BLS user tx; then assert byte-equal canonical via the requested endpoint
// family.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ReplayEndpoint {
    /// §3.5 must-pass row #1/#2 — block-family `debug_traceBlock` w/ callTracer.
    BlockFamilyDebugTraceBlock,
    /// §3.5 must-pass row #3 — single-tx `debug_traceTransaction` w/ callTracer.
    SingleTxDebugTraceTransaction,
    /// #441 — replays outside the trace family: executor-driven ones (`debug_accountAt`,
    /// `eth_callMany`, `ots_getTransactionError`), `debug_intermediateRoots` and
    /// `debug_traceCallMany`.
    ExecutorFamily,
}

async fn run_bls_replay(
    builder: WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, ChainSpec>>,
    label: &'static str,
    endpoint: ReplayEndpoint,
) -> eyre::Result<()> {
    let handle = builder
        .with_types_and_provider::<EthereumNode, BlockchainProvider<_>>()
        .with_components(EthereumNode::components())
        .with_add_ons(EthereumAddOns::default())
        .launch_with_fn(|builder| {
            let launcher = EngineNodeLauncher::new(
                builder.task_executor().clone(),
                builder.config().datadir(),
                reth_engine_primitives::TreeConfig::default(),
            );
            builder.launch_with(launcher)
        })
        .await?;

    let chain_spec = handle.node.chain_spec();
    let eth_api = handle.node.rpc_registry.eth_api().clone();
    let trace_api = handle.node.rpc_registry.trace_api();
    let debug_api = handle.node.rpc_registry.debug_api();
    let provider = handle.node.provider;

    let db_provider = provider.database_provider_ro().unwrap();
    let latest_block_number = db_provider.best_block_number().unwrap();
    let latest_block_hash = db_provider.block_hash(latest_block_number).unwrap().unwrap();
    let latest_block_header = db_provider.header_by_number(latest_block_number).unwrap().unwrap();
    drop(db_provider);

    assert_eq!(latest_block_number, 0, "[bls_replay {label}] runner expects a fresh datadir");

    let storage = BlockViewStorage::new(provider.clone());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let pipeline_api = new_pipe_exec_layer_api(
        chain_spec.clone(),
        storage,
        latest_block_header,
        latest_block_hash,
        rx,
        eth_api.clone(),
    );
    tx.send(ExecutionArgs { block_number_to_block_id: BTreeMap::new() }).unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    let mut epoch: u64 = pipeline_api
        .fetch_config_bytes(OnChainConfig::Epoch, BlockNumber::Latest)
        .unwrap()
        .try_into()
        .unwrap();

    let consensus = MockConsensus::new(pipeline_api, Box::new(ts_us));
    // Pre-roll empty blocks 1..BLS_BLOCK_NUMBER-1, then inject the BLS user tx
    // at BLS_BLOCK_NUMBER. Both empty pre-roll and BLS block are within the
    // same Alpha regime determined by the chain spec's `alphaTime`.
    consensus.push_empty_range(&mut epoch, 1, BLS_BLOCK_NUMBER - 1).await;

    let sender = funded_signer();
    let (bls_tx, sender_addr) =
        build_bls_call_tx(&sender, 0, Bytes::from(vec![0u8; BLS_INPUT_LEN]));
    let bls_tx_hash: B256 = *bls_tx.hash();

    // #441 appends a transfer from a second account, so that `transaction_index = 2` still
    // replays the BLS call while the BLS sender's final state stays the one after that call.
    let transfer = matches!(endpoint, ReplayEndpoint::ExecutorFamily)
        .then(|| build_transfer_tx(&second_signer(), 0));
    let mut block_txs = vec![bls_tx];
    let mut block_senders = vec![sender_addr];
    if let Some((transfer_tx, transfer_sender)) = &transfer {
        block_txs.push(transfer_tx.clone());
        block_senders.push(*transfer_sender);
    }

    let block = ordered_block_with_txs(
        epoch,
        BLS_BLOCK_NUMBER,
        mock_block_id(BLS_BLOCK_NUMBER),
        mock_block_id(BLS_BLOCK_NUMBER - 1),
        ts_us(BLS_BLOCK_NUMBER),
        block_txs,
        block_senders,
    );
    let result = consensus.push_one(&mut epoch, block).await;
    let pipeline_api = consensus.into_inner();
    pipeline_api.wait_for_block_persistence(BLS_BLOCK_NUMBER).await.unwrap();
    drop(pipeline_api);

    println!(
        "[bls_replay {label}] pushed BLS-call user tx at block {BLS_BLOCK_NUMBER}: tx={bls_tx_hash:?}, exec_hash={:?}",
        result.block_hash
    );

    // -------- Canonical: persisted receipts for the BLS block --------
    let receipts = provider
        .receipts_by_block(alloy_eips::BlockHashOrNumber::Number(BLS_BLOCK_NUMBER))
        .expect("provider receipts read")
        .unwrap_or_else(|| panic!("[{label}] block {BLS_BLOCK_NUMBER} must have receipts"));
    assert!(
        receipts.len() >= 2,
        "[bls_replay {label}] block {BLS_BLOCK_NUMBER} must contain at least 2 receipts (metadata system tx + BLS user tx), got {}",
        receipts.len()
    );
    // BLS tx idx is 1: [0] is the protocol-injected metadata system tx, [1] is
    // the user tx we injected via OrderedBlock.transactions.
    let bls_tx_idx = 1;
    let canonical_bls_gas = canonical_per_tx_gas(&receipts, bls_tx_idx);
    let canonical_bls_success = receipts[bls_tx_idx].success;
    assert!(
        canonical_bls_success,
        "[bls_replay {label}] canonical BLS receipt must be success (input is exactly 144 bytes so the precompile reaches its Ok branch). receipts[{bls_tx_idx}]={:?}",
        receipts[bls_tx_idx]
    );
    // Sanity floor: per-tx gas must include the flat BLS charge; otherwise
    // the canonical side itself didn't run the precompile (would point at a
    // pipe-side regression rather than an RPC one). intrinsic + 110_000 ≥ 131k.
    assert!(
        canonical_bls_gas >= POP_VERIFY_GAS,
        "[bls_replay {label}] canonical BLS tx gas {canonical_bls_gas} must be >= POP_VERIFY_GAS={POP_VERIFY_GAS}"
    );

    // -------- RPC replay --------
    let tracing_opts = GethDebugTracingOptions::call_tracer(CallConfig::default());

    match endpoint {
        ReplayEndpoint::BlockFamilyDebugTraceBlock => {
            let block_id = BlockId::Number(BLS_BLOCK_NUMBER.into());

            // (a) `debug_traceBlock` w/ callTracer — byte-equal canonical
            let debug_blk = debug_api
                .debug_trace_block(block_id, tracing_opts.clone())
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "[bls_replay {label}] debug_trace_block({BLS_BLOCK_NUMBER}) errored: {e:?}"
                    )
                });
            assert!(
                debug_blk.len() == receipts.len(),
                "[bls_replay {label}] debug_trace_block returned {} entries; expected {} (one per receipt)",
                debug_blk.len(),
                receipts.len()
            );

            let bls_entry = &debug_blk[bls_tx_idx];
            let top_frame = expect_call_frame(bls_entry, label);

            assert_eq!(
                top_frame.to,
                Some(BLS_PRECOMPILE_ADDR),
                "[bls_replay {label}] callTracer top-frame.to must equal BLS_PRECOMPILE_ADDR"
            );
            assert!(
                top_frame.error.is_none() && top_frame.revert_reason.is_none(),
                "[bls_replay {label}] callTracer top-frame must not carry an error: error={:?}, revert={:?}",
                top_frame.error,
                top_frame.revert_reason
            );
            assert_eq!(
                top_frame.gas_used,
                U256::from(canonical_bls_gas),
                "[bls_replay {label}] callTracer top-frame.gas_used must byte-equal canonical receipt gas. trace={}, canonical={}",
                top_frame.gas_used,
                canonical_bls_gas
            );

            let output = top_frame.output.as_ref().expect(
                "[bls_replay] callTracer top-frame.output must be set for direct precompile call",
            );
            assert_eq!(
                output.len(),
                32,
                "[bls_replay {label}] BLS precompile output must be exactly 32 bytes; got len={}",
                output.len()
            );

            // (b) `trace_block` w/ parity tracer — count parity guard; per-tx
            // gas_used parity-side is asserted via the canonical receipt above.
            let blk_traces = trace_api
                .trace_block(block_id)
                .await
                .unwrap_or_else(|e| panic!("[bls_replay {label}] trace_block errored: {e:?}"))
                .unwrap_or_else(|| panic!("[bls_replay {label}] trace_block returned None"));
            assert!(
                blk_traces.len() >= receipts.len(),
                "[bls_replay {label}] trace_block returned {} traces; expected >= {} per canonical",
                blk_traces.len(),
                receipts.len()
            );
        }

        ReplayEndpoint::SingleTxDebugTraceTransaction => {
            // §3.5 row #3 — debug_traceTransaction targeted at the BLS user tx.
            let trace = debug_api
                .debug_trace_transaction(bls_tx_hash, tracing_opts.clone())
                .await
                .unwrap_or_else(|e| {
                    panic!("[bls_replay {label}] debug_trace_transaction errored: {e:?}")
                });
            let top_frame = match trace {
                GethTrace::CallTracer(frame) => frame,
                other => {
                    panic!("[bls_replay {label}] expected GethTrace::CallTracer, got {other:?}",)
                }
            };

            // The tx is *directly* to the BLS precompile, so the top frame
            // either represents the BLS call itself, or the trace router
            // produced a synthetic root above it. Walk the tree to find the
            // BLS-targeted frame either way.
            let bls_frame = find_bls_frame(&top_frame).unwrap_or_else(|| {
                panic!(
                    "[bls_replay {label}] callTracer tree must contain a frame whose `to == BLS_PRECOMPILE_ADDR`; tree top={top_frame:?}"
                )
            });
            assert_eq!(
                bls_frame.to,
                Some(BLS_PRECOMPILE_ADDR),
                "[bls_replay {label}] BLS frame.to must equal BLS_PRECOMPILE_ADDR"
            );
            assert!(
                bls_frame.error.is_none() && bls_frame.revert_reason.is_none(),
                "[bls_replay {label}] BLS frame must not carry an error: error={:?}, revert={:?}",
                bls_frame.error,
                bls_frame.revert_reason
            );

            // top_frame is the tx-level frame; its gas_used must equal
            // canonical per-tx gas (intrinsic + precompile flat charge).
            assert_eq!(
                top_frame.gas_used,
                U256::from(canonical_bls_gas),
                "[bls_replay {label}] top-frame.gas_used must byte-equal canonical receipt gas. trace={}, canonical={}",
                top_frame.gas_used,
                canonical_bls_gas
            );

            // The BLS frame itself records the per-call gas; in geth's
            // callTracer for a direct precompile call this is the same value.
            // Assert output bytes are present and 32-byte sized regardless of
            // which level surfaced them.
            let output = bls_frame
                .output
                .as_ref()
                .or(top_frame.output.as_ref())
                .expect("[bls_replay] BLS frame output must be set");
            assert_eq!(
                output.len(),
                32,
                "[bls_replay {label}] BLS precompile output must be exactly 32 bytes; got len={}",
                output.len()
            );
        }

        ReplayEndpoint::ExecutorFamily => {
            let (transfer_tx, transfer_sender) =
                transfer.as_ref().expect("ExecutorFamily always appends the transfer");
            assert_eq!(
                format!("{transfer_sender:#x}"),
                SECOND_SENDER_ALLOC_KEY,
                "[{label}] second key and its genesis alloc entry must match"
            );
            let block_id = BlockId::Number(BLS_BLOCK_NUMBER.into());
            let block = provider
                .recovered_block(BLS_BLOCK_NUMBER.into(), TransactionVariant::WithHash)
                .expect("provider block read")
                .unwrap_or_else(|| panic!("[{label}] block {BLS_BLOCK_NUMBER} must be persisted"));
            let header = block.header();
            let base_fee = header.base_fee_per_gas().expect("London is active from genesis");
            let exempt =
                reth_chainspec::is_system_tx_gas_exempt(chain_spec.as_ref(), header.timestamp());

            // Fixture sanity: [onBlockStart, BLS call, transfer], and the system tx is priced the
            // way the regime under test expects.
            let senders: Vec<Address> =
                block.transactions_recovered().map(|tx| tx.signer()).collect();
            assert_eq!(
                senders,
                vec![SYSTEM_CALLER, sender_addr, *transfer_sender],
                "[{label}] block shape"
            );
            let tx_hashes: Vec<B256> =
                block.body().transactions.iter().map(|tx| *tx.hash()).collect();
            assert_eq!(
                tx_hashes[1..],
                [bls_tx_hash, *transfer_tx.hash()],
                "[{label}] user tx order"
            );
            let system_tx_price = block.body().transactions[0].max_fee_per_gas();
            if exempt {
                assert!(
                    base_fee > 0 && system_tx_price == 0,
                    "[{label}] post-Alpha fixture needs a zero-price system tx under a non-zero \
                     base fee, got price={system_tx_price} base_fee={base_fee}"
                );
            } else {
                assert_eq!(
                    system_tx_price,
                    u128::from(base_fee),
                    "[{label}] pre-Alpha system tx pays the base fee"
                );
            }

            // Guard: the trace family already replays correctly without #441. A divergence here
            // means the fixture hits a gap outside #441 (for example the mint precompile, #372):
            // stop and report instead of weakening the checks below.
            let traces = debug_api
                .debug_trace_block(
                    block_id,
                    GethDebugTracingOptions::call_tracer(CallConfig::default()),
                )
                .await
                .unwrap_or_else(|e| panic!("[{label}] debug_trace_block errored: {e:?}"));
            for (idx, entry) in traces.iter().enumerate() {
                assert_eq!(
                    expect_call_frame(entry, label).gas_used,
                    U256::from(canonical_per_tx_gas(&receipts, idx)),
                    "[{label}] debug_traceBlock tx {idx} already diverges from canonical"
                );
            }

            let post_state =
                provider.history_by_block_number(BLS_BLOCK_NUMBER).expect("post-block state");
            let sender_balance =
                post_state.account_balance(&sender_addr).unwrap().expect("BLS sender exists");
            let sender_nonce =
                post_state.account_nonce(&sender_addr).unwrap().expect("BLS sender exists");
            // Right before the transfer: onBlockStart and the BLS call are replayed.
            let before_transfer = Some(StateContext {
                block_number: Some(block_id),
                transaction_index: Some(TransactionIndex::Index(2)),
            });

            // (1) debug_accountInfoAt / debug_accountAt through tx 1: the executor replays
            // onBlockStart (gas exemption) and the BLS call (precompile).
            let info = debug_api
                .debug_account_info_at(block_id, Index::from(1), sender_addr)
                .await
                .unwrap_or_else(|e| panic!("[{label}] debug_account_info_at errored: {e:?}"))
                .expect("block exists");
            assert_eq!(
                (info.balance, info.nonce),
                (sender_balance, sender_nonce),
                "[{label}] debug_accountInfoAt"
            );
            let account = debug_api
                .debug_account_at(block_id, Index::from(1), sender_addr)
                .await
                .unwrap_or_else(|e| panic!("[{label}] debug_account_at errored: {e:?}"))
                .expect("block exists");
            assert_eq!(
                (account.balance, account.nonce),
                (sender_balance, sender_nonce),
                "[{label}] debug_accountAt"
            );

            // (2) eth_callMany right before the transfer.
            let responses = eth_api
                .call_many(
                    probe_bundle(),
                    before_transfer,
                    Some(balance_probe_override(sender_addr)),
                )
                .await
                .unwrap_or_else(|e| panic!("[{label}] eth_callMany errored: {e:?}"));
            let probed = responses[0][0].value.as_ref().expect("probe call must succeed");
            assert_eq!(
                U256::from_be_slice(probed),
                sender_balance,
                "[{label}] eth_callMany replayed the BLS call"
            );
            let now = responses[0][1].value.as_ref().expect("nowMicroseconds must succeed");
            assert_eq!(
                nowMicrosecondsCall::abi_decode_returns(now).unwrap(),
                ts_us(BLS_BLOCK_NUMBER),
                "[{label}] eth_callMany replayed onBlockStart"
            );

            // (3) spawn_replay_transaction (behind ots_getTransactionError) for every target; the
            // callback's database shows the replayed prefix before the transfer.
            for (idx, hash) in tx_hashes.iter().copied().enumerate() {
                let (gas, sender_balance_before) = eth_api
                    .spawn_replay_transaction(hash, move |_, res, mut db| {
                        let before = db
                            .basic(sender_addr)
                            .expect("state read")
                            .expect("BLS sender exists")
                            .balance;
                        Ok((res.result.tx_gas_used(), before))
                    })
                    .await
                    .unwrap_or_else(|e| {
                        panic!("[{label}] spawn_replay_transaction({idx}) errored: {e:?}")
                    })
                    .expect("tx exists");
                assert_eq!(
                    gas,
                    canonical_per_tx_gas(&receipts, idx),
                    "[{label}] replayed gas of tx {idx}"
                );
                if idx == 2 {
                    assert_eq!(
                        sender_balance_before, sender_balance,
                        "[{label}] replay prefix before the transfer"
                    );
                }
            }

            // (4) debug_intermediateRoots: one EVM per tx, each needs both rules.
            let roots = debug_api
                .intermediate_roots(block.hash())
                .await
                .unwrap_or_else(|e| panic!("[{label}] debug_intermediateRoots errored: {e:?}"));
            assert_eq!(roots.len(), tx_hashes.len(), "[{label}] one root per tx");
            assert_eq!(
                roots.last(),
                Some(&header.state_root()),
                "[{label}] replayed state must match the header"
            );

            // (5) debug_traceCallMany right before the transfer, same prefix as eth_callMany.
            let call_opts = GethDebugTracingCallOptions {
                // Moved, not cloned: this arm is the last user of `tracing_opts`
                // (workspace clippy has `redundant_clone = "warn"`).
                tracing_options: tracing_opts,
                state_overrides: Some(balance_probe_override(sender_addr)),
                ..Default::default()
            };
            let traced = debug_api
                .debug_trace_call_many(probe_bundle(), before_transfer, Some(call_opts))
                .await
                .unwrap_or_else(|e| panic!("[{label}] debug_traceCallMany errored: {e:?}"));
            let outputs: Vec<Bytes> = traced[0]
                .iter()
                .map(|trace| match trace {
                    GethTrace::CallTracer(frame) => frame.output.clone().expect("call output"),
                    other => panic!("[{label}] expected callTracer output, got {other:?}"),
                })
                .collect();
            assert_eq!(
                U256::from_be_slice(&outputs[0]),
                sender_balance,
                "[{label}] debug_traceCallMany replayed the BLS call"
            );
            assert_eq!(
                nowMicrosecondsCall::abi_decode_returns(&outputs[1]).unwrap(),
                ts_us(BLS_BLOCK_NUMBER),
                "[{label}] debug_traceCallMany replayed onBlockStart"
            );
        }
    }

    println!(
        "[bls_replay {label}] ✅ BLS RPC replay byte-equal canonical (gas={canonical_bls_gas}, success={canonical_bls_success})"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Test entry points — §3.5 must-pass rows.
// ---------------------------------------------------------------------------

/// §3.5 row #1 — post-Alpha block-family BLS replay byte-equal canonical.
#[test]
fn test_rpc_bls_call_replay_byte_equal_canonical() {
    run_pipe_e2e_test(
        &gravity_alpha_chainspec(ALPHA_TIME_ALWAYS),
        "data/gravity_system_tx_bls_replay_post_alpha",
        |b| {
            run_bls_replay(b, "post_alpha_block_family", ReplayEndpoint::BlockFamilyDebugTraceBlock)
        },
    );
}

/// §3.5 row #2 — pre-Alpha BLS replay must remain byte-equal canonical. This
/// is the unconditional-registration guard for commit `23a55587c4`: pre-fix,
/// BLS was only registered when Alpha was active, so RPC replay of pre-Alpha
/// blocks containing BLS calls would route to an empty account and diverge
/// from canonical.
#[test]
fn test_rpc_bls_call_pre_alpha_replay_unchanged() {
    run_pipe_e2e_test(
        &gravity_alpha_chainspec(ALPHA_TIME_NEVER),
        "data/gravity_system_tx_bls_replay_pre_alpha",
        |b| run_bls_replay(b, "pre_alpha_block_family", ReplayEndpoint::BlockFamilyDebugTraceBlock),
    );
}

/// §3.5 row #3 — single-tx `debug_traceTransaction` BLS replay byte-equal.
#[test]
fn test_rpc_bls_call_debug_trace_transaction_byte_equal() {
    run_pipe_e2e_test(
        &gravity_alpha_chainspec(ALPHA_TIME_ALWAYS),
        "data/gravity_system_tx_bls_replay_single_tx",
        |b| {
            run_bls_replay(
                b,
                "single_tx_debug_trace",
                ReplayEndpoint::SingleTxDebugTraceTransaction,
            )
        },
    );
}

/// #441 — post-Alpha: the system tx at index 0 needs the gas exemption, the BLS call at index 1
/// needs the precompile.
#[test]
fn test_rpc_executor_replay_post_alpha() {
    run_pipe_e2e_test(
        &gravity_alpha_chainspec_with_second_sender(ALPHA_TIME_ALWAYS),
        "data/gravity_system_tx_bls_replay_executor_post_alpha",
        |b| run_bls_replay(b, "executor_post_alpha", ReplayEndpoint::ExecutorFamily),
    );
}

/// #441 — pre-Alpha: the system tx pays the base fee, only the BLS precompile decides the result.
#[test]
fn test_rpc_executor_replay_pre_alpha() {
    run_pipe_e2e_test(
        &gravity_alpha_chainspec_with_second_sender(ALPHA_TIME_NEVER),
        "data/gravity_system_tx_bls_replay_executor_pre_alpha",
        |b| run_bls_replay(b, "executor_pre_alpha", ReplayEndpoint::ExecutorFamily),
    );
}

// ---------------------------------------------------------------------------
// Shared CLI harness (mirrors `gravity_system_tx_pre_alpha_replay_test`).
// ---------------------------------------------------------------------------

fn run_pipe_e2e_test<F, Fut>(chain_spec: &str, datadir: &'static str, run_fn: F)
where
    F: FnOnce(WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, ChainSpec>>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = eyre::Result<()>> + Send + 'static,
{
    init_panic_hook_and_tracer();

    let runner = CliRunner::try_default_runtime().unwrap();
    let args: Vec<&str> =
        vec!["reth", "--chain", chain_spec, "--with-unused-ports", "--dev", "--datadir", datadir];
    let command: NodeCommand<EthereumChainSpecParser> =
        NodeCommand::try_parse_args_from(args).unwrap();

    runner
        .run_command_until_exit(|ctx| {
            command.execute(
                ctx,
                FnLauncher::new::<EthereumChainSpecParser, _>(|builder, _| async move {
                    run_fn(builder).await
                }),
            )
        })
        .unwrap();

    std::thread::sleep(Duration::from_secs(2));
}

fn init_panic_hook_and_tracer() {
    std::panic::set_hook(Box::new(|panic_info| {
        let backtrace = std::backtrace::Backtrace::capture();
        eprintln!("Panic occurred: {panic_info}\nBacktrace:\n{backtrace}");
        std::process::exit(1);
    }));

    let _ = RethTracer::new()
        .with_stdout(LayerInfo::new(
            LogFormat::Terminal,
            LevelFilter::INFO.to_string(),
            String::new(),
            Some("always".to_string()),
        ))
        .init();
}
