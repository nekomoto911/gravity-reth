//! Test node and mock consensus: launches reth with the pipe execution layer, builds
//! ordered blocks the way gravity-sdk does, and commits them one at a time.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEip7702, TxLegacy};
use alloy_eips::{
    eip7702::{
        constants::{EIP7702_DELEGATION_DESIGNATOR, PER_EMPTY_ACCOUNT_COST},
        Authorization, SignedAuthorization,
    },
    BlockId,
};
use alloy_primitives::{address, keccak256, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_rpc_types_eth::{state::EvmOverrides, TransactionInput, TransactionRequest};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue};
use gravity_api_types::{
    account::ExternalAccountAddress,
    config_storage::{BlockNumber, ConfigStorage, OnChainConfig},
    events::contract_event::GravityEvent,
    on_chain_config::{
        dkg::{DKGTranscript, DKGTranscriptMetadata},
        jwks::{JWKStruct, ProviderJWKs},
    },
    ExtraDataType,
};
use gravity_storage::GravityStorage;
use reth_chainspec::ChainSpec;
use reth_cli_commands::{launcher::FnLauncher, NodeCommand};
use reth_cli_runner::CliRunner;
use reth_db::DatabaseEnv;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_ethereum_primitives::{Transaction, TransactionSigned};
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_pipe_exec_layer_ext_v2::{
    onchain_config::{types::getActiveValidatorsCall, VALIDATOR_MANAGER_ADDR},
    OrderedBlock, PipeExecLayerApi,
};
use reth_rpc_eth_api::{helpers::EthCall, RpcTypes};
use reth_tracing::{
    tracing_subscriber::filter::LevelFilter, LayerInfo, LogFormat, RethTracer, Tracer,
};
use std::{future::Future, io::ErrorKind, sync::Arc, time::Duration};

/// Mainnet runs 7 validators; `onBlockStart` reverts for a proposer index outside `0..7`.
const PROPOSER_INDEX: u64 = 0;

/// The pipe gives up on a block whose parent never arrives after 2 s; a stale block is
/// discarded then, so waiting a little longer shows it was never executed.
const OLD_EPOCH_BLOCK_WAIT: Duration = Duration::from_secs(3);

/// Twice mainnet's minimum base fee (50 gwei), which empty test blocks never raise much.
pub(crate) const MAX_FEE_PER_GAS: u128 = 100_000_000_000;
const MAX_PRIORITY_FEE_PER_GAS: u128 = 1_000_000_000;
pub(crate) const TRANSFER_GAS: u64 = 21_000;

/// An EIP-7702 delegation target without code: a call to an account delegated to it runs
/// nothing.
pub(crate) const DELEGATE: Address = Address::repeat_byte(0xd3);

/// `GBridgeSender` on Ethereum: the only sender `GBridgeReceiver` mints for.
const ETHEREUM_BRIDGE: Address = address!("0xE82c61Ac9Ec2041b493118051afa4F18a55dC876");
/// Oracle source of the bridge: source type 0 (blockchain events), source id 1 (Ethereum),
/// the one `GBridgeReceiver` is registered for.
const BRIDGE_ORACLE_SOURCE: &[u8] = b"gravity://0/1/events";
/// The JWK type the pipe routes to `NativeOracle.recordBatch`.
const UNSUPPORTED_JWK_TYPE: &str = "0x1::jwks::Unsupported_JWK";

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
    /// gravity-sdk block id of the parent.
    pub(crate) parent_id: B256,
    /// Block hash the pipe reported for this block.
    pub(crate) hash: B256,
    /// Seconds, as in the header.
    pub(crate) timestamp: u64,
    /// Microseconds, as passed to `onBlockStart`; the header keeps only seconds.
    pub(crate) timestamp_us: u64,
    /// Epoch after this block.
    pub(crate) epoch: u64,
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

    /// Whether a DKG session is open, so the next block allowed to change the epoch will.
    pub(crate) const fn dkg_in_progress(&self) -> bool {
        self.dkg_in_progress
    }

    /// Number of the last committed block.
    pub(crate) const fn parent_number(&self) -> u64 {
        self.parent_number
    }

    /// Timestamp of the last committed block, in seconds.
    pub(crate) fn parent_timestamp(&self) -> u64 {
        self.parent_timestamp
    }

    pub(crate) async fn produce_block(&mut self, mut input: BlockInput) -> CommittedBlock {
        // Step 1: build the ordered block, delivering a pending DKG transcript if allowed.
        let delivers_transcript = self.dkg_in_progress && input.may_change_epoch;
        if delivers_transcript {
            input.extra_data.push(self.dkg_transcript());
        }
        let block = self.next_ordered_block(input);
        let (number, id, timestamp_us) = (block.number, block.id, block.timestamp_us);

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

        let committed = CommittedBlock {
            number,
            parent_id: self.parent_id,
            hash: result.block_hash,
            timestamp: timestamp_us / 1_000_000,
            timestamp_us,
            epoch: self.epoch,
            epoch_changed,
        };
        self.parent_number = number;
        self.parent_id = id;
        self.parent_timestamp = committed.timestamp;
        committed
    }

    /// Pushes a block that still carries the epoch before the last epoch change, and asserts
    /// that the pipe never executes it.
    ///
    /// The epoch-change block released its successor under the new epoch, so a block of the
    /// old epoch times out waiting for its parent and is discarded as stale. Executing it
    /// would take the height of the next real block, so the timeline could not continue: a
    /// result panics instead of being recorded.
    pub(crate) async fn assert_old_epoch_block_rejected(&self) {
        let block = OrderedBlock {
            epoch: self.epoch - 1,
            // Distinct from the id of the real block at this height, so neither can be taken
            // for the other.
            id: keccak256(b"gravity_pipe old-epoch block"),
            ..self.next_ordered_block(BlockInput {
                timestamp_us: (self.parent_timestamp + 1) * 1_000_000,
                ..Default::default()
            })
        };
        let number = block.number;
        self.pipe.push_ordered_block(block).unwrap();
        let result =
            tokio::time::timeout(OLD_EPOCH_BLOCK_WAIT, self.pipe.pull_executed_block_hash()).await;
        if let Ok(result) = result {
            panic!("block {number} of the previous epoch was executed: {result:?}");
        }
    }

    /// The ordered block gravity-sdk would send next, on top of the last committed block.
    fn next_ordered_block(&self, input: BlockInput) -> OrderedBlock {
        let number = self.parent_number + 1;
        let randomness = keccak256(number.to_be_bytes());
        OrderedBlock {
            epoch: self.epoch,
            parent_id: self.parent_id,
            id: mock_block_id(number),
            number,
            timestamp_us: input.timestamp_us,
            coinbase: self.coinbase,
            prev_randao: randomness,
            withdrawals: Default::default(),
            transactions: input.transactions,
            senders: input.senders,
            proposer_index: Some(PROPOSER_INDEX),
            failed_proposer_indices: vec![],
            extra_data: input.extra_data,
            randomness: randomness.into(),
        }
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

/// Accounts the test signs user transactions for. Keys derive from fixed seeds, so every
/// run signs the same transactions; the [`Self::FUNDED`] ones are funded in genesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestAccount {
    Alice,
    Bob,
    Carol,
    Dave,
    /// Also delegated to [`DELEGATE`] in genesis: until Beta no transaction can delegate an
    /// account, yet the lockdown before Beta needs one to reject transactions to and from.
    Delegated,
    /// Not funded; signs the delegation Beta's scenarios make once EIP-7702 is unlocked.
    Erin,
    /// Never funded, so it stays absent from state and cannot pay for anything.
    Unfunded,
}

impl TestAccount {
    pub(crate) const FUNDED: [Self; 5] =
        [Self::Alice, Self::Bob, Self::Carol, Self::Dave, Self::Delegated];

    pub(crate) fn address(self) -> Address {
        self.signer().address()
    }

    /// Signs `tx` as given, so scenarios can also build transactions the pipe must reject.
    pub(crate) fn sign<T>(self, tx: T) -> SignedTx
    where
        T: SignableTransaction<Signature>,
        Transaction: From<T>,
    {
        let signature = self.signer().sign_hash_sync(&tx.signature_hash()).unwrap();
        SignedTx {
            tx: TransactionSigned::new_unhashed(tx.into(), signature),
            sender: self.address(),
        }
    }

    /// An EIP-7702 authorization delegating this account's code to `delegate`.
    pub(crate) fn authorize(
        self,
        chain_id: u64,
        delegate: Address,
        nonce: u64,
    ) -> SignedAuthorization {
        let authorization =
            Authorization { chain_id: U256::from(chain_id), address: delegate, nonce };
        let signature = self.signer().sign_hash_sync(&authorization.signature_hash()).unwrap();
        authorization.into_signed(signature)
    }

    fn signer(self) -> PrivateKeySigner {
        let seed = keccak256(format!("gravity_pipe test account {self:?}"));
        PrivateKeySigner::from_bytes(&seed).unwrap()
    }
}

/// A user transaction together with the sender gravity-sdk recovers for it.
#[derive(Debug, Clone)]
pub(crate) struct SignedTx {
    pub(crate) tx: TransactionSigned,
    pub(crate) sender: Address,
}

impl SignedTx {
    pub(crate) fn hash(&self) -> B256 {
        *self.tx.hash()
    }
}

/// An EIP-1559 transfer of nothing to `to`, with 21 000 gas and fees that clear mainnet's
/// minimum base fee. Scenarios override the fields they care about.
pub(crate) fn eip1559_tx(chain_id: u64, nonce: u64, to: TxKind) -> TxEip1559 {
    TxEip1559 {
        chain_id,
        nonce,
        gas_limit: TRANSFER_GAS,
        max_fee_per_gas: MAX_FEE_PER_GAS,
        max_priority_fee_per_gas: MAX_PRIORITY_FEE_PER_GAS,
        to,
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::new(),
    }
}

/// An EIP-7702 call of `to` without value or calldata, carrying `authorization_list`, with
/// exactly its intrinsic gas and the fees of [`eip1559_tx`].
pub(crate) fn eip7702_tx(
    chain_id: u64,
    nonce: u64,
    to: Address,
    authorization_list: Vec<SignedAuthorization>,
) -> TxEip7702 {
    TxEip7702 {
        chain_id,
        nonce,
        gas_limit: TRANSFER_GAS + PER_EMPTY_ACCOUNT_COST * authorization_list.len() as u64,
        max_fee_per_gas: MAX_FEE_PER_GAS,
        max_priority_fee_per_gas: MAX_PRIORITY_FEE_PER_GAS,
        to,
        value: U256::ZERO,
        access_list: Default::default(),
        authorization_list,
        input: Bytes::new(),
    }
}

/// Code of an account delegated to `delegate`: the EIP-7702 designator.
pub(crate) fn delegation_code(delegate: Address) -> Bytes {
    [EIP7702_DELEGATION_DESIGNATOR.as_slice(), delegate.as_slice()].concat().into()
}

/// The legacy (EIP-155) counterpart of [`eip1559_tx`].
pub(crate) const fn legacy_tx(chain_id: u64, nonce: u64, to: TxKind) -> TxLegacy {
    TxLegacy {
        chain_id: Some(chain_id),
        nonce,
        gas_price: MAX_FEE_PER_GAS,
        gas_limit: TRANSFER_GAS,
        to,
        value: U256::ZERO,
        input: Bytes::new(),
    }
}

/// Deposits on the Ethereum bridge, delivered as block extra data the way the relayer's
/// oracle observations reach the pipe: a JWK entry the pipe turns into a
/// `NativeOracle.recordBatch` system transaction, whose callback into `GBridgeReceiver` mints
/// the deposit.
#[derive(Debug)]
pub(crate) struct BridgeDeposits {
    /// `NativeOracle` accepts only the nonce after the last recorded one.
    next_nonce: u128,
}

impl Default for BridgeDeposits {
    fn default() -> Self {
        // Mainnet genesis has recorded nothing for the bridge source yet.
        Self { next_nonce: 1 }
    }
}

impl BridgeDeposits {
    /// Extra data minting `amount` wei to `recipient`.
    pub(crate) fn deposit(&mut self, recipient: Address, amount: U256) -> ExtraDataType {
        let nonce = self.next_nonce;
        self.next_nonce += 1;

        // GBridgeSender's message, wrapped in a portal message from the trusted bridge.
        let message = (amount, recipient).abi_encode_params();
        let payload = [ETHEREUM_BRIDGE.as_slice(), &nonce.to_be_bytes(), &message].concat();

        // The relayer's canonical wrapper: `(nonce, source position, payload)`. The source
        // position is the Ethereum block of the deposit; `NativeOracle` only stores it.
        let data = (nonce, U256::from(nonce), payload.as_slice()).abi_encode();
        let observation = ProviderJWKs {
            issuer: BRIDGE_ORACLE_SOURCE.to_vec(),
            // The pipe does not read the version.
            version: 1,
            jwks: vec![JWKStruct { type_name: UNSUPPORTED_JWK_TYPE.to_string(), data }],
        };
        ExtraDataType::JWK(bcs::to_bytes(&observation).unwrap())
    }
}

/// gravity-sdk block ids are opaque; the test derives them from the block number. Hashed rather
/// than the number itself, so a stored id never reads like a stored number, and salted, so it
/// differs from the block's randomness (`keccak256(number)`).
fn mock_block_id(number: u64) -> B256 {
    keccak256([b"gravity_pipe block id".as_slice(), &number.to_be_bytes()].concat())
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
