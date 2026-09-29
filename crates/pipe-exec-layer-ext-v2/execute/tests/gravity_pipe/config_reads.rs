//! gravity-sdk's reads of the on-chain configuration through the pipe (`ConfigStorage`),
//! checked on every epoch-change block, where gravity-sdk reads them to start the new epoch.
//!
//! Each read is a set of `eth_call`s the pipe makes from `SYSTEM_CALLER` to the system
//! contracts, through the node's RPC. Most readers return `None` when a call fails. The two JWK
//! readers drop the entries of a failed call instead, so they are checked through the bridge
//! oracle source genesis registers: its entry must be there, carrying the nonce the oracle state
//! reader returns.

use crate::report::BlockReport;
use alloy_primitives::bytes::Bytes;
use gravity_api_types::{
    config_storage::{BlockNumber, ConfigStorage, OnChainConfig},
    on_chain_config::{
        jwks::{JWKConsensusConfig, ObservedJWKs},
        oracle_state::OracleSourceState,
    },
};

const SOURCE: &str = "ConfigStorage::fetch_config_bytes";

/// Task URI prefix of the bridge oracle source: source type 0 (blockchain events), source id 1
/// (Ethereum).
const BRIDGE_TASK_URI_PREFIX: &str = "gravity://0/1/";

/// Checks every configuration the pipe serves, as of block `number`, which changed the epoch to
/// `epoch`.
pub(crate) fn check_config_reads(
    storage: &impl ConfigStorage,
    number: u64,
    epoch: u64,
    report: &mut BlockReport<'_>,
) {
    // Step 1: the readers that return `None` on a failed call.
    for config in [
        OnChainConfig::ConsensusConfig,
        OnChainConfig::ValidatorSet,
        OnChainConfig::DKGState,
        OnChainConfig::RandomnessConfig,
        OnChainConfig::ValidatorPerformances,
    ] {
        read(storage, config, number, report);
    }
    let read_epoch = storage
        .fetch_config_bytes(OnChainConfig::Epoch, BlockNumber::Number(number))
        .map(|epoch| -> u64 { epoch.try_into().expect("epoch is a BCS u64") });
    report.check_eq(SOURCE, None, "Epoch", Some(epoch), read_epoch);

    // Step 2: the bridge source's nonce, from the oracle state reader.
    let Some(bytes) = read(storage, OnChainConfig::OracleState, number, report) else { return };
    let states: Vec<OracleSourceState> = bcs::from_bytes(&bytes).expect("OracleState is BCS");
    let Some(bridge) = states.iter().find(|state| (state.source_type, state.source_id) == (0, 1))
    else {
        report.record(SOURCE, None, "OracleState bridge source", "present", "absent");
        return
    };
    let nonce = Some(bridge.latest_nonce as u64);

    // Step 3: the JWK readers list the bridge task with that nonce.
    if let Some(bytes) = read(storage, OnChainConfig::JWKConsensusConfig, number, report) {
        let config: JWKConsensusConfig =
            bcs::from_bytes(&bytes).expect("JWKConsensusConfig is BCS");
        let provider = config
            .oidc_providers
            .iter()
            .find(|provider| provider.name.starts_with(BRIDGE_TASK_URI_PREFIX));
        report.check_eq(
            SOURCE,
            None,
            "JWKConsensusConfig bridge provider nonce",
            Some(nonce),
            provider.map(|provider| provider.onchain_nonce),
        );
    }
    if let Some(bytes) = read(storage, OnChainConfig::ObservedJWKs, number, report) {
        let observed: ObservedJWKs = bcs::from_bytes(&bytes).expect("ObservedJWKs is BCS");
        let entry = observed
            .jwks
            .entries
            .iter()
            .find(|entry| entry.issuer.starts_with(BRIDGE_TASK_URI_PREFIX.as_bytes()));
        report.check_eq(
            SOURCE,
            None,
            "ObservedJWKs bridge entry version",
            Some(nonce),
            entry.map(|entry| Some(entry.version)),
        );
    }
}

/// Reads `config` as of block `number`, recording a mismatch when the reader returns nothing.
fn read(
    storage: &impl ConfigStorage,
    config: OnChainConfig,
    number: u64,
    report: &mut BlockReport<'_>,
) -> Option<Bytes> {
    let name = format!("{config:?}");
    let bytes = storage
        .fetch_config_bytes(config, BlockNumber::Number(number))
        .map(|bytes| -> Bytes { bytes.try_into().expect("config bytes") });
    if bytes.is_none() {
        report.record(SOURCE, None, name, "a value", "None");
    }
    bytes
}
