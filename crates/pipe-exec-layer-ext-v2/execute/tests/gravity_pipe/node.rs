//! Test node and mock consensus: launches reth with the pipe execution layer, builds
//! ordered blocks the way gravity-sdk does, and commits them one at a time.

use alloy_eips::BlockId;
use alloy_primitives::{keccak256, Address, TxKind, B256, U256};
use alloy_rpc_types_eth::{state::EvmOverrides, TransactionInput, TransactionRequest};
use alloy_sol_types::SolCall;
use gravity_api_types::{
    account::ExternalAccountAddress,
    config_storage::{BlockNumber, ConfigStorage, OnChainConfig},
    events::contract_event::GravityEvent,
    on_chain_config::dkg::{DKGTranscript, DKGTranscriptMetadata},
    ExtraDataType,
};
use gravity_storage::GravityStorage;
use reth_chainspec::ChainSpec;
use reth_cli_commands::{launcher::FnLauncher, NodeCommand};
use reth_cli_runner::CliRunner;
use reth_db::DatabaseEnv;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_ethereum_primitives::TransactionSigned;
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_pipe_exec_layer_ext_v2::{
    onchain_config::{types::getActiveValidatorsCall, VALIDATOR_MANAGER_ADDR},
    ExecutionResult, OrderedBlock, PipeExecLayerApi,
};
use reth_rpc_eth_api::{helpers::EthCall, RpcTypes};
use reth_tracing::{
    tracing_subscriber::filter::LevelFilter, LayerInfo, LogFormat, RethTracer, Tracer,
};
use std::{future::Future, io::ErrorKind, sync::Arc, time::Duration};

/// Mainnet runs 7 validators; `onBlockStart` reverts for a proposer index outside `0..7`.
const PROPOSER_INDEX: u64 = 0;

pub(crate) type Builder = WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, ChainSpec>>;

/// Boots a fresh node on `genesis_json` and runs `run_fn` against it until it returns.
pub(crate) fn run_node<F, Fut>(genesis_json: &str, datadir: &str, run_fn: F)
where
    F: FnOnce(Builder) -> Fut + Send + 'static,
    Fut: Future<Output = eyre::Result<()>> + Send + 'static,
{
    init_panic_hook_and_tracer();

    // The pipe starts from genesis, so leftovers of an earlier run must not survive.
    if let Err(err) = std::fs::remove_dir_all(datadir) {
        assert_eq!(err.kind(), ErrorKind::NotFound, "failed to clear {datadir}: {err}");
    }

    let runner = CliRunner::try_default_runtime().unwrap();
    let command: NodeCommand<EthereumChainSpecParser> = NodeCommand::try_parse_args_from([
        "reth",
        "--chain",
        genesis_json,
        "--with-unused-ports",
        "--dev",
        "--datadir",
        datadir,
        // Replays are checked over HTTP with every namespace, like a mainnet RPC node.
        "--http",
        "--http.api",
        "all",
    ])
    .unwrap();
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

    // Let the engine thread notice the closed pipe channel before the process tears down;
    // otherwise it can abort on a destroyed pthread lock.
    std::thread::sleep(Duration::from_secs(2));
}

/// What the test puts into the next block besides the protocol system transactions.
#[derive(Debug, Default)]
pub(crate) struct BlockInput {
    pub(crate) timestamp_us: u64,
    pub(crate) transactions: Vec<TransactionSigned>,
    pub(crate) senders: Vec<Address>,
    pub(crate) extra_data: Vec<ExtraDataType>,
    /// Whether a pending DKG transcript may be delivered in this block. An epoch-change
    /// block drops its user transactions, and mainnet never changes epoch on a hardfork
    /// activation block.
    pub(crate) may_change_epoch: bool,
}

/// A block the pipe executed, committed, and persisted.
#[derive(Debug)]
pub(crate) struct CommittedBlock {
    pub(crate) number: u64,
    /// Seconds, as in the header.
    pub(crate) timestamp: u64,
    pub(crate) result: ExecutionResult,
    pub(crate) epoch_changed: bool,
}

/// Mock consensus in front of the pipe execution layer.
pub(crate) struct Node<Storage, EthApi> {
    pipe: PipeExecLayerApi<Storage, EthApi>,
    coinbase: Address,
    epoch: u64,
    /// A DKG session is open; the next block allowed to change the epoch delivers its
    /// transcript.
    dkg_in_progress: bool,
    parent_number: u64,
    parent_id: B256,
    parent_timestamp: u64,
}

impl<Storage, EthApi> Node<Storage, EthApi>
where
    Storage: GravityStorage,
    EthApi: EthCall,
    EthApi::NetworkTypes: RpcTypes<TransactionRequest = TransactionRequest>,
{
    pub(crate) async fn new(
        pipe: PipeExecLayerApi<Storage, EthApi>,
        eth_api: &EthApi,
        genesis_timestamp: u64,
    ) -> Self {
        let epoch = pipe
            .fetch_config_bytes(OnChainConfig::Epoch, BlockNumber::Latest)
            .unwrap()
            .try_into()
            .unwrap();
        // gravity-sdk pays the proposer's validator address; the active set only changes
        // in `finishTransition`, and mainnet genesis forbids set changes.
        let coinbase = active_validator_address(eth_api, PROPOSER_INDEX).await;
        Self {
            pipe,
            coinbase,
            epoch,
            dkg_in_progress: false,
            parent_number: 0,
            parent_id: mock_block_id(0),
            parent_timestamp: genesis_timestamp,
        }
    }

    /// Timestamp of the last committed block, in seconds.
    pub(crate) fn parent_timestamp(&self) -> u64 {
        self.parent_timestamp
    }

    pub(crate) async fn produce_block(&mut self, input: BlockInput) -> CommittedBlock {
        // Step 1: build the ordered block, delivering a pending DKG transcript if allowed.
        let number = self.parent_number + 1;
        let id = mock_block_id(number);
        let randomness = keccak256(number.to_be_bytes());
        let mut extra_data = input.extra_data;
        let delivers_transcript = self.dkg_in_progress && input.may_change_epoch;
        if delivers_transcript {
            extra_data.push(self.dkg_transcript());
        }
        let block = OrderedBlock {
            epoch: self.epoch,
            parent_id: self.parent_id,
            id,
            number,
            timestamp_us: input.timestamp_us,
            coinbase: self.coinbase,
            prev_randao: randomness,
            withdrawals: Default::default(),
            transactions: input.transactions,
            senders: input.senders,
            proposer_index: Some(PROPOSER_INDEX),
            failed_proposer_indices: vec![],
            extra_data,
            randomness: randomness.into(),
        };

        // Step 2: execute, commit, and wait until the block is in the database, so that
        // RPC replays read persisted history like a mainnet RPC node does.
        self.pipe.push_ordered_block(block).unwrap();
        let result = self.pipe.pull_executed_block_hash().await.unwrap();
        assert_eq!((result.block_number, result.block_id), (number, id));
        self.pipe.commit_executed_block_hash(id, Some(result.block_hash)).unwrap();
        self.pipe.wait_for_block_persistence(number).await.unwrap();

        // Step 3: follow the DKG session and the epoch.
        let mut epoch_changed = false;
        for event in &result.gravity_events {
            match event {
                GravityEvent::DKG(_) => self.dkg_in_progress = true,
                GravityEvent::NewEpoch(new_epoch, _) => {
                    assert_eq!(*new_epoch, self.epoch + 1, "block {number} skipped an epoch");
                    self.epoch = *new_epoch;
                    self.dkg_in_progress = false;
                    epoch_changed = true;
                }
                GravityEvent::ObservedJWKsUpdated(..) => {}
            }
        }
        assert_eq!(
            epoch_changed, delivers_transcript,
            "block {number}: a delivered DKG transcript must change the epoch, and only it"
        );

        let timestamp = input.timestamp_us / 1_000_000;
        self.parent_number = number;
        self.parent_id = id;
        self.parent_timestamp = timestamp;
        CommittedBlock { number, timestamp, result, epoch_changed }
    }

    /// gravity-sdk's DKG transcript. The pipe only forwards the bytes to
    /// `Reconfiguration.finishTransition`, which stores them without verification.
    fn dkg_transcript(&self) -> ExtraDataType {
        let transcript = DKGTranscript {
            metadata: DKGTranscriptMetadata {
                epoch: self.epoch,
                author: ExternalAccountAddress::new([0; 32]),
            },
            transcript_bytes: vec![0xab; 32],
        };
        ExtraDataType::DKG(bcs::to_bytes(&transcript).unwrap())
    }
}

/// gravity-sdk block ids are opaque; the test derives them from the block number.
fn mock_block_id(number: u64) -> B256 {
    B256::left_padding_from(&number.to_be_bytes())
}

async fn active_validator_address<EthApi>(eth_api: &EthApi, validator_index: u64) -> Address
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
        .call(request, Some(BlockId::number(0)), EvmOverrides::default())
        .await
        .unwrap_or_else(|err| panic!("getActiveValidators failed: {err:?}"));
    getActiveValidatorsCall::abi_decode_returns(&output)
        .unwrap()
        .into_iter()
        .find(|validator| validator.validatorIndex == validator_index)
        .unwrap_or_else(|| panic!("no active validator with index {validator_index}"))
        .validator
}

fn init_panic_hook_and_tracer() {
    // A panic on a node task must fail the test instead of hanging the pipe.
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
