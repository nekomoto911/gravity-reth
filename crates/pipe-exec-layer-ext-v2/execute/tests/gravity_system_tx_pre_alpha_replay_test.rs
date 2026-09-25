#![allow(missing_docs)]

//! T4 / §3.3 — Pre-Alpha RPC block & single-tx replay regression test.
//!
//! Pins the "the gate is keyed on the replayed block's timestamp" invariant: for
//! any pre-Alpha block the RPC replay path (trace_block / debug_traceBlock /
//! trace_transaction / debug_traceTransaction) must reproduce the canonical
//! execution byte-for-byte. In particular, `is_system_tx_gas_exempt(chain_spec,
//! B'.timestamp)` returns **false** for pre-Alpha blocks, so the cfg-side
//! `disable_base_fee` / `disable_balance_check` levers MUST NOT activate on
//! replay — pre-Alpha SYSTEM_CALLER traverses the standard fee path, paying
//! `gas_used × base_fee` out of its sentinel-sized genesis alloc balance.
//!
//! Acceptance matrix row: §3.3 (must-pass).
//!
//! Also covers gravity-reth#438: a pre-Alpha DKG epoch-change block, whose body
//! omits the `onBlockStart` metadata transaction the pipe executed, must still
//! replay through every RPC entry family.
//!
//! Note on location: §3.3 nominally targets `crates/rpc/rpc/tests/` but the
//! reth-rpc crate has no `tests/` directory and pulling in `reth-node-builder`
//! + `reth-cli` dev-deps purely to bootstrap an RPC mock would be invasive
//! churn (much larger than the test itself). The pipe-exec-layer dev harness
//! already exposes the full RPC registry (`handle.node.rpc_registry.trace_api()`
//! / `.debug_api()`) — same RPC surface, smaller blast radius. Tests live here
//! to match `gravity_eip2935_test.rs` / `gravity_bls_precompile_test.rs` which
//! also exercise RPC paths from the pipe harness for the same reason.

use alloy_consensus::{BlockHeader, Transaction};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, TxKind, B256, U256};
use alloy_rpc_types_eth::{
    state::EvmOverrides, Bundle, Index, StateContext, TransactionIndex, TransactionInput,
    TransactionRequest,
};
use alloy_rpc_types_trace::geth::{GethDebugTracingOptions, GethTrace, TraceResult};
use alloy_sol_macro::sol;
use alloy_sol_types::SolCall;
use gravity_api_types::{
    account::ExternalAccountAddress,
    config_storage::{BlockNumber, ConfigStorage, OnChainConfig},
    events::contract_event::GravityEvent,
    on_chain_config::dkg::{DKGTranscript, DKGTranscriptMetadata},
    ExtraDataType,
};
use gravity_storage::{block_view_storage::BlockViewStorage, GravityStorage};
use reth_chainspec::ChainSpec;
use reth_cli_commands::{launcher::FnLauncher, NodeCommand};
use reth_cli_runner::CliRunner;
use reth_db::DatabaseEnv;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_node_builder::{EngineNodeLauncher, NodeBuilder, WithLaunchContext};
use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
use reth_pipe_exec_layer_ext_v2::{
    new_pipe_exec_layer_api,
    onchain_config::{
        types::getActiveValidatorsCall, RANDOMNESS_CONFIG_ADDR, RECONFIGURATION_ADDR,
        SYSTEM_CALLER, TIMESTAMP_ADDR, VALIDATOR_MANAGER_ADDR,
    },
    ExecutionArgs, OrderedBlock, PipeExecLayerApi,
};
use reth_provider::{
    providers::BlockchainProvider, BlockHashReader, BlockNumReader, BlockReader,
    DatabaseProviderFactory, HeaderProvider, ReceiptProvider, StateProviderFactory,
    TransactionVariant,
};
use reth_rpc_eth_api::{helpers::EthCall, RpcTypes};
use reth_tracing::{
    tracing_subscriber::filter::LevelFilter, LayerInfo, LogFormat, RethTracer, Tracer,
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

// ---------------------------------------------------------------------------
// Test parameters
// ---------------------------------------------------------------------------

const PRE_ALPHA_TS_BASE: u64 = 1_000_000_000;
/// `alphaTime` so distant that no pushed block can transition into Alpha
/// activation — the entire run is strictly pre-fork.
const ALPHA_TS_NEVER: u64 = 9_999_999_999;
const PRE_ALPHA_BLOCK_COUNT: u64 = 10;
const SAMPLE_TRACE_BLOCK: u64 = 5;

/// Patch `alphaTime` on the embedded `gravity_hardfork.json`, returning JSON
/// the CLI parser accepts as `--chain`. Pattern lifted from
/// `gravity_eip2935_test::gravity_prague_chainspec`.
fn gravity_alpha_chainspec(alpha_time: u64) -> String {
    let mut json: serde_json::Value =
        serde_json::from_str(include_str!("../gravity_hardfork.json"))
            .expect("gravity_hardfork.json must parse as JSON");
    json["config"]["alphaTime"] = serde_json::json!(alpha_time);
    json.to_string()
}

fn mock_block_id(block_number: u64) -> B256 {
    B256::left_padding_from(&block_number.to_be_bytes())
}

fn pre_alpha_ts_us(block_number: u64) -> u64 {
    (PRE_ALPHA_TS_BASE + block_number) * 1_000_000
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

// ---------------------------------------------------------------------------
// MockConsensus — drives empty blocks (one metadata system tx each).
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
// Core T4 runner — pre-Alpha block replay regression.
// ---------------------------------------------------------------------------

async fn run_pre_alpha_replay_regression(
    builder: WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, ChainSpec>>,
    label: &'static str,
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

    assert_eq!(
        latest_block_number, 0,
        "[pre_alpha_replay {label}] runner expects a fresh datadir (latest must be genesis=0)"
    );

    let storage = BlockViewStorage::new(provider.clone());

    let (tx, rx) = tokio::sync::oneshot::channel();
    let pipeline_api = new_pipe_exec_layer_api(
        chain_spec.clone(),
        storage,
        latest_block_header,
        latest_block_hash,
        rx,
        eth_api,
    );
    tx.send(ExecutionArgs { block_number_to_block_id: BTreeMap::new() }).unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    let mut epoch: u64 = pipeline_api
        .fetch_config_bytes(OnChainConfig::Epoch, BlockNumber::Latest)
        .unwrap()
        .try_into()
        .unwrap();

    let consensus = MockConsensus::new(pipeline_api, Box::new(pre_alpha_ts_us));
    consensus.push_empty_range(&mut epoch, 1, PRE_ALPHA_BLOCK_COUNT).await;
    let pipeline_api = consensus.into_inner();
    pipeline_api.wait_for_block_persistence(PRE_ALPHA_BLOCK_COUNT).await.unwrap();
    drop(pipeline_api);

    println!(
        "[pre_alpha_replay {label}] pushed {PRE_ALPHA_BLOCK_COUNT} pre-Alpha blocks; sweeping RPC replay endpoints"
    );

    // -----------------------------------------------------------------
    // Phase 1 — trace_block / debug_trace_block / replay_block_transactions
    // over every pre-Alpha block. Each call MUST return Ok, and the trace
    // tx-count MUST match the canonical receipt count for that block. A
    // mismatch (e.g. silent system-tx skipping or duplicate emission) would
    // surface here.
    // -----------------------------------------------------------------
    for n in 1..=PRE_ALPHA_BLOCK_COUNT {
        // Canonical receipts: provider walks the persisted block body.
        let canonical_receipts = provider
            .receipts_by_block(alloy_eips::BlockHashOrNumber::Number(n))
            .expect("provider read must succeed")
            .unwrap_or_else(|| panic!("block {n} must have persisted receipts"));
        assert!(
            !canonical_receipts.is_empty(),
            "[pre_alpha_replay {label}] block {n} must contain >= 1 receipt (the metadata system tx)"
        );

        // RPC trace_block — Reth's parity-style block-family endpoint.
        let traces = trace_api
            .trace_block(BlockId::Number(n.into()))
            .await
            .unwrap_or_else(|e| panic!("[{label}] trace_block({n}) must not error: {e:?}"))
            .unwrap_or_else(|| panic!("[{label}] trace_block({n}) must return Some(_)"));
        // Pre-Alpha block uses normal coinbase reward attribution; sanity-only
        // check is `traces.len() >= canonical_receipts.len()` since
        // `extract_reward_traces` may append a separate reward trace.
        assert!(
            traces.len() >= canonical_receipts.len(),
            "[pre_alpha_replay {label}] block {n} trace_block returned {} traces; expected >= {} per-tx traces (canonical receipts)",
            traces.len(),
            canonical_receipts.len()
        );

        // debug_trace_block — geth-style block-family endpoint with default
        // tracer config (callTracer). Every entry MUST be `Success`. If the
        // gate were incorrectly active pre-Alpha, the cfg-side disable_* flags
        // would still produce a successful trace (no observable diff at the
        // result variant level when balance >> fee), so the strongest assertion
        // here is "no Err variant escapes".
        let debug_traces = debug_api
            .debug_trace_block(BlockId::Number(n.into()), GethDebugTracingOptions::default())
            .await
            .unwrap_or_else(|e| panic!("[{label}] debug_trace_block({n}) must not error: {e:?}"));
        assert!(
            !debug_traces.is_empty(),
            "[pre_alpha_replay {label}] debug_trace_block({n}) must return >= 1 trace entry"
        );
        for (i, entry) in debug_traces.iter().enumerate() {
            assert!(
                matches!(entry, TraceResult::Success { .. }),
                "[pre_alpha_replay {label}] debug_trace_block({n})[{i}] must be Success, got {entry:?}"
            );
        }
    }

    // -----------------------------------------------------------------
    // Phase 2 — single-tx family on a sampled pre-Alpha block.
    // -----------------------------------------------------------------
    let block_5 = provider
        .recovered_block(SAMPLE_TRACE_BLOCK.into(), TransactionVariant::WithHash)
        .expect("recovered_block must succeed")
        .unwrap_or_else(|| panic!("block {SAMPLE_TRACE_BLOCK} must be persisted"));
    let txs = block_5.body().transactions.as_slice();
    assert!(
        !txs.is_empty(),
        "[pre_alpha_replay {label}] block {SAMPLE_TRACE_BLOCK} must contain >= 1 tx (metadata system tx)"
    );

    let metadata_tx_hash: B256 = *txs[0].hash();
    println!(
        "[pre_alpha_replay {label}] sampling metadata system tx at block {SAMPLE_TRACE_BLOCK}: hash={metadata_tx_hash:?}"
    );

    // trace_transaction — must not Err, must return Some(traces) with >= 1 trace.
    let single_traces = trace_api
        .trace_transaction(metadata_tx_hash)
        .await
        .unwrap_or_else(|e| panic!("[{label}] trace_transaction must not error: {e:?}"))
        .unwrap_or_else(|| {
            panic!("[{label}] trace_transaction for pre-Alpha metadata system tx must return Some")
        });
    assert!(
        !single_traces.is_empty(),
        "[pre_alpha_replay {label}] trace_transaction must return >= 1 trace per metadata system tx"
    );

    // debug_trace_transaction — geth-style single-tx tracer. Must not Err.
    let debug_single = debug_api
        .debug_trace_transaction(metadata_tx_hash, GethDebugTracingOptions::default())
        .await
        .unwrap_or_else(|e| panic!("[{label}] debug_trace_transaction must not error: {e:?}"));
    // Sanity: the trace returned a non-null geth payload. Any
    // `GasPriceLessThanBasefee` / `InsufficientFunds` style failure on replay
    // would surface as an Err above; reaching this point is the load-bearing
    // gate assertion.
    let s = format!("{debug_single:?}");
    assert!(
        !s.contains("InsufficientFunds") && !s.contains("GasPriceLessThanBasefee"),
        "[pre_alpha_replay {label}] debug_trace_transaction payload must not contain fee-error markers: {s}"
    );

    // -----------------------------------------------------------------
    // Phase 3 — sanity: pre-Alpha block ts is < alphaTime, so the gate must
    // return false for every replayed block timestamp. We verify by
    // reading the persisted header ts and comparing to the chain_spec's
    // alphaTime fork condition; this guards against accidental gate-on-tip
    // regressions (design §3.5.2).
    // -----------------------------------------------------------------
    for n in 1..=PRE_ALPHA_BLOCK_COUNT {
        let header = provider
            .header_by_number(n)
            .expect("provider read must succeed")
            .unwrap_or_else(|| panic!("block {n} header must be persisted"));
        let ts = header.timestamp;
        assert!(
            !reth_chainspec::is_system_tx_gas_exempt(chain_spec.as_ref(), ts),
            "[pre_alpha_replay {label}] is_system_tx_gas_exempt must return false at pre-Alpha block {n} ts={ts}"
        );
    }

    println!(
        "[pre_alpha_replay {label}] ✅ all {PRE_ALPHA_BLOCK_COUNT} pre-Alpha block / single-tx replays passed (no false exemption)"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// gravity-reth#438 — pre-Alpha DKG epoch-change block replay.
//
// Before Alpha, when the DKG `finishTransition` emits `NewEpoch`, the pipe runs
// `onBlockStart` (nonce N) and `finishTransition` (nonce N+1) but assembles only
// `finishTransition` into the body. RPC replay has to re-execute the omitted
// `onBlockStart`, otherwise the body fails with `nonce too high`.
// ---------------------------------------------------------------------------

const DKG_START_BLOCK: u64 = 1;
const DKG_EPOCH_BLOCK: u64 = 2;

sol! {
    function nowMicroseconds() external view returns (uint64);
}

/// Pre-Alpha chainspec with DKG enabled, so the epoch transition started by
/// `onBlockStart` waits for `finishTransition` instead of completing in place.
fn gravity_pre_alpha_dkg_chainspec() -> String {
    let mut json: serde_json::Value =
        serde_json::from_str(&gravity_alpha_chainspec(ALPHA_TS_NEVER)).unwrap();
    let randomness_config = json["alloc"]
        .get_mut(format!("{RANDOMNESS_CONFIG_ADDR:#x}"))
        .expect("genesis alloc must contain RandomnessConfig");
    // `RandomnessConfig._currentConfig.variant` sits in slot 0; 1 is `ConfigVariant.V2`.
    randomness_config["storage"]["0x00"] = serde_json::json!("0x01");
    json.to_string()
}

async fn run_pre_alpha_dkg_epoch_block_replay(
    builder: WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, ChainSpec>>,
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
    assert_eq!(db_provider.best_block_number().unwrap(), 0, "runner expects a fresh datadir");
    let genesis_hash = db_provider.block_hash(0).unwrap().unwrap();
    let genesis_header = db_provider.header_by_number(0).unwrap().unwrap();
    drop(db_provider);

    let (tx, rx) = tokio::sync::oneshot::channel();
    let pipeline_api = new_pipe_exec_layer_api(
        chain_spec,
        BlockViewStorage::new(provider.clone()),
        genesis_header,
        genesis_hash,
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
    let consensus = MockConsensus::new(pipeline_api, Box::new(pre_alpha_ts_us));

    // Step 1: the first block's `onBlockStart` opens a DKG session.
    let start_block = empty_ordered_block(
        epoch,
        DKG_START_BLOCK,
        mock_block_id(DKG_START_BLOCK),
        mock_block_id(0),
        pre_alpha_ts_us(DKG_START_BLOCK),
    );
    let result = consensus.push_one(&mut epoch, start_block).await;
    assert!(
        result.gravity_events.iter().any(|event| matches!(event, GravityEvent::DKG(_))),
        "block {DKG_START_BLOCK} must start a DKG session, got {:?}",
        result.gravity_events
    );

    // Step 2: the next block carries the transcript, and its `finishTransition` emits
    // `NewEpoch`. The coinbase is what gravity-sdk sets for proposer index 0; the active
    // set only changes in `finishTransition`, so genesis already holds it.
    let mut epoch_block = empty_ordered_block(
        epoch,
        DKG_EPOCH_BLOCK,
        mock_block_id(DKG_EPOCH_BLOCK),
        mock_block_id(DKG_START_BLOCK),
        pre_alpha_ts_us(DKG_EPOCH_BLOCK),
    );
    epoch_block.coinbase = active_validator_address(&eth_api, 0, 0).await;
    let transcript = DKGTranscript {
        metadata: DKGTranscriptMetadata { epoch, author: ExternalAccountAddress::new([0; 32]) },
        transcript_bytes: vec![0xab; 32],
    };
    epoch_block.extra_data = vec![ExtraDataType::DKG(bcs::to_bytes(&transcript).unwrap())];
    let result = consensus.push_one(&mut epoch, epoch_block).await;
    assert!(
        result.gravity_events.iter().any(|event| matches!(event, GravityEvent::NewEpoch(..))),
        "block {DKG_EPOCH_BLOCK} must change the epoch, got {:?}",
        result.gravity_events
    );
    drop(consensus);

    // Step 3: the committed block has the #438 shape — a lone `finishTransition` whose
    // nonce skips the one the omitted `onBlockStart` consumed.
    let block = provider
        .recovered_block(DKG_EPOCH_BLOCK.into(), TransactionVariant::WithHash)
        .unwrap()
        .expect("epoch-change block must be persisted");
    let parent_state = provider.history_by_block_number(DKG_START_BLOCK).unwrap();
    let post_state = provider.history_by_block_number(DKG_EPOCH_BLOCK).unwrap();
    let parent_nonce = parent_state.account_nonce(&SYSTEM_CALLER).unwrap().unwrap_or_default();
    let txs: Vec<_> = block.transactions_recovered().collect();
    assert_eq!(txs.len(), 1, "epoch-change body must hold only finishTransition");
    let finish_tx = txs[0];
    assert_eq!(finish_tx.signer(), SYSTEM_CALLER);
    assert_eq!(finish_tx.to(), Some(RECONFIGURATION_ADDR));
    assert_eq!(finish_tx.nonce(), parent_nonce + 1);
    let finish_tx_hash = *finish_tx.hash();
    let finish_receipt = provider
        .receipts_by_block(DKG_EPOCH_BLOCK.into())
        .unwrap()
        .expect("epoch-change block must have receipts")
        .remove(0);
    let epoch_block_id = BlockId::Number(DKG_EPOCH_BLOCK.into());

    // Step 4: block- and tx-level tracing replay the body on top of `onBlockStart`.
    let debug_traces = debug_api
        .debug_trace_block(epoch_block_id, GethDebugTracingOptions::default())
        .await
        .expect("debug_trace_block must replay the epoch-change block");
    let [TraceResult::Success { result: GethTrace::Default(frame), .. }] = debug_traces.as_slice()
    else {
        panic!("expected one struct-log trace, got {debug_traces:?}");
    };
    assert!(!frame.failed);
    assert_eq!(frame.gas, finish_receipt.cumulative_gas_used);
    trace_api
        .trace_block(epoch_block_id)
        .await
        .expect("trace_block must replay the epoch-change block")
        .expect("trace_block must find the epoch-change block");
    debug_api
        .debug_trace_transaction(finish_tx_hash, GethDebugTracingOptions::default())
        .await
        .expect("debug_trace_transaction must replay finishTransition");
    trace_api
        .trace_transaction(finish_tx_hash)
        .await
        .expect("trace_transaction must replay finishTransition")
        .expect("trace_transaction must find finishTransition");
    let roots = debug_api
        .intermediate_roots(block.hash())
        .await
        .expect("intermediate_roots must replay the epoch-change block");
    assert_eq!(roots, vec![block.header().state_root()], "replayed state must match the block");

    // Step 5: executor-driven replays see the same state.
    let system_caller = debug_api
        .debug_account_info_at(epoch_block_id, Index::from(0), SYSTEM_CALLER)
        .await
        .expect("debug_account_info_at must replay the epoch-change block")
        .expect("block must exist");
    assert_eq!(system_caller.nonce, post_state.account_nonce(&SYSTEM_CALLER).unwrap().unwrap());
    assert_eq!(system_caller.balance, post_state.account_balance(&SYSTEM_CALLER).unwrap().unwrap());
    // Before any body tx the global time is already the one `onBlockStart` wrote.
    let now_call = TransactionRequest {
        to: Some(TxKind::Call(TIMESTAMP_ADDR)),
        input: TransactionInput::new(nowMicrosecondsCall {}.abi_encode().into()),
        ..Default::default()
    };
    let responses = eth_api
        .call_many(
            vec![Bundle { transactions: vec![now_call], block_override: None }],
            Some(StateContext {
                block_number: Some(epoch_block_id),
                transaction_index: Some(TransactionIndex::Index(0)),
            }),
            None,
        )
        .await
        .expect("call_many must replay the epoch-change block prologue");
    let now_output = responses[0][0].value.clone().expect("nowMicroseconds must succeed");
    assert_eq!(
        nowMicrosecondsCall::abi_decode_returns(&now_output).unwrap(),
        pre_alpha_ts_us(DKG_EPOCH_BLOCK)
    );

    // Step 6: a whole-block re-execution cannot include the omitted call, so the
    // execution witness is refused with an explicit reason.
    let witness_err = debug_api
        .debug_execution_witness(BlockNumberOrTag::Number(DKG_EPOCH_BLOCK))
        .await
        .expect_err("execution witness must be refused for the epoch-change block");
    // The reason travels in the JSON-RPC error message; `Display` only says "unsupported".
    let witness_err = format!("{witness_err:?}");
    assert!(witness_err.contains("onBlockStart"), "unexpected error: {witness_err}");

    // Step 7: the DKG-start block still takes the generic pre-execution path.
    let start_block = provider
        .recovered_block(DKG_START_BLOCK.into(), TransactionVariant::WithHash)
        .unwrap()
        .expect("DKG-start block must be persisted");
    let roots = debug_api
        .intermediate_roots(start_block.hash())
        .await
        .expect("intermediate_roots must replay the DKG-start block");
    assert_eq!(roots.last(), Some(&start_block.header().state_root()));

    Ok(())
}

/// Returns the `validator` address of the active validator with `validator_index`,
/// as of `block_number`.
async fn active_validator_address<EthApi>(
    eth_api: &EthApi,
    block_number: u64,
    validator_index: u64,
) -> Address
where
    EthApi: EthCall,
    EthApi::NetworkTypes: RpcTypes<TransactionRequest = TransactionRequest>,
{
    let request = TransactionRequest {
        to: Some(TxKind::Call(VALIDATOR_MANAGER_ADDR)),
        input: TransactionInput::new(getActiveValidatorsCall {}.abi_encode().into()),
        ..Default::default()
    };
    let output = eth_api
        .call(request, Some(BlockId::number(block_number)), EvmOverrides::default())
        .await
        .unwrap_or_else(|err| {
            panic!("getActiveValidators failed at block {block_number}: {err:?}")
        });
    getActiveValidatorsCall::abi_decode_returns(&output)
        .unwrap()
        .into_iter()
        .find(|validator| validator.validatorIndex == validator_index)
        .unwrap_or_else(|| panic!("no active validator with index {validator_index}"))
        .validator
}

// ---------------------------------------------------------------------------
// Test entry points — grevm + serial backends both exercised for the same
// pre-Alpha invariants (regression in either path is independent).
// ---------------------------------------------------------------------------

#[test]
fn test_rpc_pre_alpha_replay_regression_grevm() {
    run_pipe_e2e_test(
        &gravity_alpha_chainspec(ALPHA_TS_NEVER),
        "data/gravity_system_tx_pre_alpha_replay_grevm",
        false,
        |b| run_pre_alpha_replay_regression(b, "grevm"),
    );
}

#[test]
fn test_rpc_pre_alpha_replay_regression_disable_grevm() {
    run_pipe_e2e_test(
        &gravity_alpha_chainspec(ALPHA_TS_NEVER),
        "data/gravity_system_tx_pre_alpha_replay_disable_grevm",
        true,
        |b| run_pre_alpha_replay_regression(b, "disable_grevm"),
    );
}

#[test]
fn test_rpc_pre_alpha_dkg_epoch_block_replay() {
    run_pipe_e2e_test(
        &gravity_pre_alpha_dkg_chainspec(),
        "data/gravity_system_tx_pre_alpha_dkg_epoch_replay",
        false,
        run_pre_alpha_dkg_epoch_block_replay,
    );
}

// ---------------------------------------------------------------------------
// Shared CLI harness (mirrors gravity_bls_precompile_test::run_pipe_e2e_test).
// ---------------------------------------------------------------------------

fn run_pipe_e2e_test<F, Fut>(
    chain_spec: &str,
    datadir: &'static str,
    disable_grevm: bool,
    run_fn: F,
) where
    F: FnOnce(WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, ChainSpec>>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = eyre::Result<()>> + Send + 'static,
{
    init_panic_hook_and_tracer();

    let runner = CliRunner::try_default_runtime().unwrap();
    let mut args: Vec<&str> =
        vec!["reth", "--chain", chain_spec, "--with-unused-ports", "--dev", "--datadir", datadir];
    if disable_grevm {
        args.push("--gravity.disable-grevm");
    }
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
