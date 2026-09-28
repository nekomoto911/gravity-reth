//! Simulation endpoints and `SYSTEM_CALLER`.
//!
//! No committed block can hold a forged system transaction, and a simulation never reaches the
//! chain, so a simulated call from `SYSTEM_CALLER` gets the chain's semantics rather than an
//! anti-spoofing rule: from Alpha on, each endpoint also skips the base-fee and balance checks
//! for it, on top of what the endpoint already skips; before Alpha it is an address like any
//! other. Every other sender keeps the checks reth configures for the endpoint.
//!
//! Two probes make each check observable on its own. Both call `Reconfiguration.currentEpoch()`
//! (a contract, so `eth_estimateGas` cannot shortcut it as a plain transfer) with a gas price and
//! without a gas limit, the sender's balance set by a state override:
//! - zero balance, gas price above the base fee: only a balance check can fail it;
//! - ample balance, gas price below the base fee: only a base-fee check can fail it.

use crate::{
    hardfork::Chain,
    node::{CommittedBlock, TestAccount},
    report::BlockReport,
};
use alloy_primitives::{uint, Address, Bytes, U256, U64};
use alloy_rpc_types_eth::AccessListResult;
use alloy_rpc_types_trace::geth::CallFrame;
use alloy_sol_types::SolCall;
use reth_pipe_exec_layer_ext_v2::onchain_config::{
    epoch::Reconfiguration, EPOCH_MANAGER_ADDR, SYSTEM_CALLER,
};
use serde_json::{json, Value};
use std::fmt;

const SOURCE: &str = "scenario: simulation semantics";

/// 10^6 ether: pays for any probe many times over.
const AMPLE_BALANCE: U256 = uint!(1_000_000_000_000_000_000_000_000_U256);

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
    for probe in Probe::ALL {
        for endpoint in Endpoint::ALL {
            let ordinary = endpoint.reth_outcome(probe);
            let system_caller = if alpha_active { Outcome::Success } else { ordinary };
            let senders = [
                ("an ordinary sender", TestAccount::Unfunded.address(), ordinary),
                ("SYSTEM_CALLER", SYSTEM_CALLER, system_caller),
            ];
            for (sender_name, sender, expected) in senders {
                let actual = endpoint.call(chain, block, probe.request(sender, base_fee));
                if !expected.matches(&actual) {
                    let actual = actual
                        .map_or_else(|error| format!("error: {error}"), |()| "success".to_string());
                    let field = format!("{} from {sender_name}, {probe}", endpoint.name());
                    report.record(SOURCE, None, field, expected, actual);
                }
            }
        }
    }
}

/// A call built so that exactly one check can reject it.
#[derive(Debug, Clone, Copy)]
enum Probe {
    ZeroBalance,
    BelowBaseFee,
}

impl Probe {
    const ALL: [Self; 2] = [Self::ZeroBalance, Self::BelowBaseFee];

    /// The call from `sender`, and the state override setting its balance, on top of a block
    /// with `base_fee`.
    fn request(self, sender: Address, base_fee: u64) -> ProbeRequest {
        // The next block's base fee, which `eth_simulateV1` uses, moves by at most 1/8 from this
        // block's; twice and half the base fee stay on their side of both.
        let (balance, gas_price) = match self {
            Self::ZeroBalance => (U256::ZERO, u128::from(base_fee) * 2),
            Self::BelowBaseFee => (AMPLE_BALANCE, u128::from(base_fee) / 2),
        };
        let input = Bytes::from(Reconfiguration::currentEpochCall {}.abi_encode());
        ProbeRequest {
            call: json!({
                "from": sender,
                "to": EPOCH_MANAGER_ADDR,
                "input": input,
                "gasPrice": U256::from(gas_price),
            }),
            state_override: json!({ sender.to_string(): { "balance": balance } }),
        }
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

struct ProbeRequest {
    call: Value,
    state_override: Value,
}

#[derive(Debug, Clone, Copy)]
enum Endpoint {
    EthCall,
    DebugTraceCall,
    EstimateGas,
    CreateAccessList,
    SimulateV1 { validation: bool },
}

impl Endpoint {
    const ALL: [Self; 6] = [
        Self::EthCall,
        Self::DebugTraceCall,
        Self::EstimateGas,
        Self::CreateAccessList,
        Self::SimulateV1 { validation: false },
        Self::SimulateV1 { validation: true },
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::EthCall => "eth_call",
            Self::DebugTraceCall => "debug_traceCall",
            Self::EstimateGas => "eth_estimateGas",
            Self::CreateAccessList => "eth_createAccessList",
            Self::SimulateV1 { validation: false } => "eth_simulateV1 (validation off)",
            Self::SimulateV1 { validation: true } => "eth_simulateV1 (validation on)",
        }
    }

    /// What reth answers an ordinary sender, as its code decides.
    ///
    /// `eth_call` and `debug_traceCall` (through `prepare_call_env`), `eth_estimateGas` and
    /// `eth_createAccessList` turn off the base-fee check and the fee charge, and with it revm's
    /// balance check. Their balance check is reth's own: without a gas limit in the request, a
    /// gas price caps the gas at what the balance pays for, zero for a zero balance. The
    /// transaction then cannot pay its intrinsic gas, which `eth_estimateGas` reports as
    /// exceeding the allowance. `eth_simulateV1` keeps revm's fee charge and balance check; with
    /// validation off it turns off the base-fee check (and zeroes the block's base fee), with
    /// validation on it keeps it.
    const fn reth_outcome(self, probe: Probe) -> Outcome {
        match (self, probe) {
            (Self::EthCall | Self::DebugTraceCall | Self::CreateAccessList, Probe::ZeroBalance) => {
                Outcome::Error("intrinsic gas too low")
            }
            (Self::EstimateGas, Probe::ZeroBalance) => {
                Outcome::Error("gas required exceeds allowance (0)")
            }
            (Self::SimulateV1 { .. }, Probe::ZeroBalance) => {
                Outcome::Error("insufficient funds for gas * price + value")
            }
            (Self::SimulateV1 { validation: true }, Probe::BelowBaseFee) => {
                Outcome::Error("max fee per gas less than block base fee")
            }
            (_, Probe::BelowBaseFee) => Outcome::Success,
        }
    }

    /// Runs the probe on the state after `block`: success, or the endpoint's error. A call
    /// that runs but fails counts as an error too.
    fn call(
        self,
        chain: &Chain<'_>,
        block: &CommittedBlock,
        probe: ProbeRequest,
    ) -> Result<(), String> {
        let ProbeRequest { call, state_override } = probe;
        let number = U64::from(block.number);
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
                let result = &blocks[0]["calls"][0];
                if result["status"] == json!(U64::from(1)) {
                    Ok(())
                } else {
                    Err(result["error"].to_string())
                }
            }
        }
    }
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
