//! Simulation endpoints and `SYSTEM_CALLER`.
//!
//! No committed block can hold a forged system transaction, and a simulation never reaches the
//! chain, so a simulated call from `SYSTEM_CALLER` gets the chain's semantics rather than an
//! anti-spoofing rule: from Alpha on, each endpoint also skips the base-fee and balance checks
//! for it, on top of what the endpoint already skips; before Alpha it is an address like any
//! other. Every other sender keeps the checks reth configures for the endpoint.
//!
//! Two probes make each check observable on its own. Both call `Reconfiguration.currentEpoch()`
//! (a contract, so `eth_estimateGas` cannot shortcut it as a plain transfer) with a gas price and,
//! where the request allows it, without a gas limit:
//! - zero balance, gas price above the base fee: only a balance check can reject it;
//! - ample balance, gas price below the base fee: only a base-fee check can reject it.
//!
//! Every pure simulation endpoint is probed. How the sender gets its balance depends on what the
//! endpoint accepts: a state override where it takes one; a setup call before the probe in
//! `trace_callMany`, which takes none; and for the endpoints that only take signed transactions
//! (`trace_rawTransaction`, `eth_callBundle`, `mev_simBundle`), an account whose balance already
//! fits: the never-funded test account or a funded one. `SYSTEM_CALLER` has no key, so those three
//! are probed from ordinary senders only.

use crate::{
    hardfork::Chain,
    node::{legacy_tx, CommittedBlock, TestAccount},
    report::BlockReport,
};
use alloy_consensus::TxLegacy;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{uint, Address, Bytes, TxKind, U256, U64};
use alloy_rpc_types_eth::AccessListResult;
use alloy_rpc_types_trace::{geth::CallFrame, parity::TraceResults};
use alloy_sol_types::SolCall;
use reth_pipe_exec_layer_ext_v2::onchain_config::{
    epoch::Reconfiguration, EPOCH_MANAGER_ADDR, SYSTEM_CALLER,
};
use serde_json::{json, Value};
use std::fmt;

const SOURCE: &str = "scenario: simulation semantics";

/// 1 000 ether: pays for any probe many times over, and a funded test account can give it away.
const AMPLE_BALANCE: U256 = uint!(1_000_000_000_000_000_000_000_U256);

/// Funds the sender in `trace_callMany`'s setup call.
const FUNDER: TestAccount = TestAccount::Alice;
/// Receives the sender's balance in `trace_callMany`'s setup call.
const BALANCE_SINK: Address = Address::repeat_byte(0xa3);

/// Gas limit of the signed probes; the call needs far less.
const SIGNED_PROBE_GAS: u64 = 100_000;

/// Checks every simulation endpoint with both probes, from an ordinary sender and from
/// `SYSTEM_CALLER`, on the state after `block`. `alpha_active` is whether Alpha is active in
/// `block`.
pub(super) fn check_simulations(
    chain: &Chain<'_>,
    block: &CommittedBlock,
    alpha_active: bool,
    report: &mut BlockReport<'_>,
) {
    let base_fee = chain.block(block.number).header.base_fee_per_gas.expect("London is active");
    let probe_at = |endpoint: Endpoint, probe, sender| {
        endpoint.call(chain, &ProbeContext { block, base_fee, probe, sender })
    };
    for probe in Probe::ALL {
        for endpoint in Endpoint::ALL {
            let ordinary = endpoint.reth_outcome(probe);
            let actual = probe_at(endpoint, probe, Sender::Ordinary);
            check(report, endpoint, Sender::Ordinary, probe, ordinary, actual);

            if endpoint.takes_signed_transactions() {
                continue;
            }
            let system_caller = if alpha_active { Outcome::Success } else { ordinary };
            let actual = probe_at(endpoint, probe, Sender::SystemCaller);
            check(report, endpoint, Sender::SystemCaller, probe, system_caller, actual);
        }
    }
}

fn check(
    report: &mut BlockReport<'_>,
    endpoint: Endpoint,
    sender: Sender,
    probe: Probe,
    expected: Outcome,
    actual: Result<(), String>,
) {
    if expected.matches(&actual) {
        return;
    }
    let actual = actual.map_or_else(|error| format!("error: {error}"), |()| "success".to_string());
    let field = format!("{} from {sender}, {probe}", endpoint.name());
    report.record(SOURCE, None, field, expected, actual);
}

/// A call built so that exactly one check can reject it.
#[derive(Debug, Clone, Copy)]
enum Probe {
    ZeroBalance,
    BelowBaseFee,
}

impl Probe {
    const ALL: [Self; 2] = [Self::ZeroBalance, Self::BelowBaseFee];

    const fn balance(self) -> U256 {
        match self {
            Self::ZeroBalance => U256::ZERO,
            Self::BelowBaseFee => AMPLE_BALANCE,
        }
    }

    /// Endpoints that simulate the next block (`eth_simulateV1`) see its base fee, which moves
    /// by at most 1/8 from this block's; twice and half the base fee stay on their side of both.
    fn gas_price(self, base_fee: u64) -> u128 {
        match self {
            Self::ZeroBalance => u128::from(base_fee) * 2,
            Self::BelowBaseFee => u128::from(base_fee) / 2,
        }
    }

    /// The unsigned probe call from `sender`.
    fn call(self, sender: Address, base_fee: u64) -> Value {
        json!({
            "from": sender,
            "to": EPOCH_MANAGER_ADDR,
            "input": probe_input(),
            "gasPrice": U256::from(self.gas_price(base_fee)),
        })
    }

    /// Gives `sender` the probe's balance, as a state override.
    fn state_override(self, sender: Address) -> Value {
        json!({ sender.to_string(): { "balance": self.balance() } })
    }

    /// Gives `sender` the probe's balance, as a call made before the probe on the state after
    /// block `number`: the sender sends all it has away, or the funder sends it the ample
    /// balance. Gas price 0 keeps the call itself free.
    fn setup_call(self, chain: &Chain<'_>, sender: Address, number: u64) -> Value {
        let (from, to, value) = match self {
            Self::ZeroBalance => (sender, BALANCE_SINK, chain.balance(sender, number)),
            Self::BelowBaseFee => (FUNDER.address(), sender, AMPLE_BALANCE),
        };
        json!({ "from": from, "to": to, "value": value, "gasPrice": U256::ZERO })
    }

    /// The probe signed by an ordinary account whose balance on the state after block `number`
    /// already fits: the never-funded account, or a funded one.
    fn signed(self, chain: &Chain<'_>, number: u64, base_fee: u64) -> Bytes {
        let account = match self {
            Self::ZeroBalance => TestAccount::Unfunded,
            Self::BelowBaseFee => TestAccount::Alice,
        };
        let nonce = chain.nonce(account.address(), number);
        let tx = account.sign(TxLegacy {
            gas_price: self.gas_price(base_fee),
            gas_limit: SIGNED_PROBE_GAS,
            input: probe_input(),
            ..legacy_tx(chain.chain_id, nonce, TxKind::Call(EPOCH_MANAGER_ADDR))
        });
        tx.tx.encoded_2718().into()
    }
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroBalance => write!(f, "zero balance, gas price above the base fee"),
            Self::BelowBaseFee => write!(f, "ample balance, gas price below the base fee"),
        }
    }
}

fn probe_input() -> Bytes {
    Reconfiguration::currentEpochCall {}.abi_encode().into()
}

#[derive(Debug, Clone, Copy)]
enum Sender {
    /// A test account; which one depends on how the endpoint sets the balance.
    Ordinary,
    SystemCaller,
}

impl Sender {
    /// The sender of unsigned probes, whose balance the endpoint sets.
    fn address(self) -> Address {
        match self {
            Self::Ordinary => TestAccount::Unfunded.address(),
            Self::SystemCaller => SYSTEM_CALLER,
        }
    }
}

impl fmt::Display for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ordinary => write!(f, "an ordinary sender"),
            Self::SystemCaller => write!(f, "SYSTEM_CALLER"),
        }
    }
}

/// One probe from one sender, on the state after `block`.
struct ProbeContext<'a> {
    block: &'a CommittedBlock,
    base_fee: u64,
    probe: Probe,
    sender: Sender,
}

#[derive(Debug, Clone, Copy)]
enum Endpoint {
    EthCall,
    DebugTraceCall,
    EstimateGas,
    CreateAccessList,
    SimulateV1 { validation: bool },
    TraceCall,
    TraceCallMany,
    TraceRawTransaction,
    EthCallBundle,
    MevSimBundle,
}

impl Endpoint {
    const ALL: [Self; 11] = [
        Self::EthCall,
        Self::DebugTraceCall,
        Self::EstimateGas,
        Self::CreateAccessList,
        Self::SimulateV1 { validation: false },
        Self::SimulateV1 { validation: true },
        Self::TraceCall,
        Self::TraceCallMany,
        Self::TraceRawTransaction,
        Self::EthCallBundle,
        Self::MevSimBundle,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::EthCall => "eth_call",
            // Without a transaction index: the call runs on the state after the block.
            Self::DebugTraceCall => "debug_traceCall",
            Self::EstimateGas => "eth_estimateGas",
            Self::CreateAccessList => "eth_createAccessList",
            Self::SimulateV1 { validation: false } => "eth_simulateV1 (validation off)",
            Self::SimulateV1 { validation: true } => "eth_simulateV1 (validation on)",
            Self::TraceCall => "trace_call",
            Self::TraceCallMany => "trace_callMany",
            Self::TraceRawTransaction => "trace_rawTransaction",
            Self::EthCallBundle => "eth_callBundle",
            Self::MevSimBundle => "mev_simBundle",
        }
    }

    const fn takes_signed_transactions(self) -> bool {
        matches!(self, Self::TraceRawTransaction | Self::EthCallBundle | Self::MevSimBundle)
    }

    /// What reth answers an ordinary sender, as its code decides.
    ///
    /// `eth_call`, `debug_traceCall`, `trace_call` and `trace_callMany` (all through
    /// `prepare_call_env`), `eth_estimateGas` and `eth_createAccessList` turn off the base-fee
    /// check and the fee charge, and with it revm's balance check. Their balance check is reth's
    /// own: without a gas limit in the request, a gas price caps the gas at what the balance pays
    /// for, zero for a zero balance. The transaction then cannot pay its intrinsic gas, which
    /// `eth_estimateGas` reports as exceeding the allowance. `eth_simulateV1` keeps revm's fee
    /// charge and balance check; with validation off it turns off the base-fee check (and zeroes
    /// the block's base fee), with validation on it keeps it. The endpoints taking signed
    /// transactions execute them in the block's environment unchanged, with every check on.
    const fn reth_outcome(self, probe: Probe) -> Outcome {
        match (self, probe) {
            (
                Self::EthCall |
                Self::DebugTraceCall |
                Self::CreateAccessList |
                Self::TraceCall |
                Self::TraceCallMany,
                Probe::ZeroBalance,
            ) => Outcome::Error("intrinsic gas too low"),
            (Self::EstimateGas, Probe::ZeroBalance) => {
                Outcome::Error("gas required exceeds allowance (0)")
            }
            // `eth_simulateV1` and the signed-transaction endpoints.
            (_, Probe::ZeroBalance) => Outcome::Error("insufficient funds for gas * price + value"),
            (
                Self::SimulateV1 { validation: true } |
                Self::TraceRawTransaction |
                Self::EthCallBundle |
                Self::MevSimBundle,
                Probe::BelowBaseFee,
            ) => Outcome::Error("max fee per gas less than block base fee"),
            (_, Probe::BelowBaseFee) => Outcome::Success,
        }
    }

    /// Runs the probe: success, or the endpoint's error. A call that runs but fails counts as
    /// an error too, and so does a response without the probe's result.
    fn call(self, chain: &Chain<'_>, context: &ProbeContext<'_>) -> Result<(), String> {
        let ProbeContext { block, base_fee, probe, sender } = *context;
        let number = U64::from(block.number);
        let sender = sender.address();
        let call = probe.call(sender, base_fee);
        let state_override = probe.state_override(sender);
        match self {
            Self::EthCall => {
                chain.request::<Bytes>("eth_call", json!([call, number, state_override])).map(drop)
            }
            Self::DebugTraceCall => {
                let options = json!({ "tracer": "callTracer", "stateOverrides": state_override });
                let frame: CallFrame =
                    chain.request("debug_traceCall", json!([call, number, options]))?;
                frame.error.map_or(Ok(()), Err)
            }
            Self::EstimateGas => chain
                .request::<U64>("eth_estimateGas", json!([call, number, state_override]))
                .map(drop),
            Self::CreateAccessList => {
                let params = json!([call, number, state_override]);
                let result: AccessListResult = chain.request("eth_createAccessList", params)?;
                result.error.map_or(Ok(()), Err)
            }
            Self::SimulateV1 { validation } => {
                // The simulated block follows `block` by a second rather than reth's default of
                // twelve, so it stays in `block`'s phase.
                let time = U64::from(block.timestamp + 1);
                let payload = json!({
                    "blockStateCalls": [{
                        "blockOverrides": { "time": time },
                        "stateOverrides": state_override,
                        "calls": [call],
                    }],
                    "validation": validation,
                });
                let blocks: Vec<Value> =
                    chain.request("eth_simulateV1", json!([payload, number]))?;
                let result = blocks
                    .first()
                    .and_then(|block| block["calls"].get(0))
                    .ok_or_else(|| format!("no call result in {blocks:?}"))?;
                if result["status"] == json!(U64::from(1)) {
                    Ok(())
                } else {
                    Err(result["error"].to_string())
                }
            }
            Self::TraceCall => {
                let params = json!([call, ["trace"], number, state_override]);
                trace_succeeded(chain.request("trace_call", params)?)
            }
            Self::TraceCallMany => {
                let setup = probe.setup_call(chain, sender, block.number);
                let params = json!([[[setup, ["trace"]], [call, ["trace"]]], number]);
                let results: Vec<TraceResults> = chain.request("trace_callMany", params)?;
                let [_, probe_result] = <[TraceResults; 2]>::try_from(results)
                    .map_err(|results| format!("{} results for 2 calls", results.len()))?;
                trace_succeeded(probe_result)
            }
            Self::TraceRawTransaction => {
                let raw = probe.signed(chain, block.number, base_fee);
                let params = json!([raw, ["trace"], number]);
                trace_succeeded(chain.request("trace_rawTransaction", params)?)
            }
            Self::EthCallBundle => {
                let bundle = json!({
                    "txs": [probe.signed(chain, block.number, base_fee)],
                    "blockNumber": U64::from(block.number + 1),
                    "stateBlockNumber": number,
                });
                let response: Value = chain.request("eth_callBundle", json!([bundle]))?;
                let result = response["results"]
                    .get(0)
                    .ok_or_else(|| format!("no transaction result in {response}"))?;
                match result.get("revert").filter(|revert| !revert.is_null()) {
                    Some(revert) => Err(format!("reverted: {revert}")),
                    None => Ok(()),
                }
            }
            Self::MevSimBundle => {
                let bundle = json!({
                    "version": "v0.1",
                    "inclusion": { "block": number },
                    "body": [{ "tx": probe.signed(chain, block.number, base_fee), "canRevert": false }],
                });
                let overrides = json!({ "parentBlock": number });
                let response: Value = chain.request("mev_simBundle", json!([bundle, overrides]))?;
                if response["success"] == json!(true) {
                    Ok(())
                } else {
                    Err(response["error"].to_string())
                }
            }
        }
    }
}

/// A parity trace of a single call succeeded if its root frame has no error.
fn trace_succeeded(results: TraceResults) -> Result<(), String> {
    let root = results.trace.first().ok_or("no root trace")?;
    root.error.clone().map_or(Ok(()), Err)
}

/// How an endpoint answers a probe.
#[derive(Debug, Clone, Copy)]
enum Outcome {
    Success,
    /// An error whose message contains this text.
    Error(&'static str),
}

impl Outcome {
    fn matches(self, actual: &Result<(), String>) -> bool {
        match (self, actual) {
            (Self::Success, Ok(())) => true,
            (Self::Error(text), Err(error)) => error.contains(text),
            _ => false,
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => write!(f, "success"),
            Self::Error(text) => write!(f, "an error containing {text:?}"),
        }
    }
}
