//! Alpha: gas-exempt system transactions, and Gravity's precompiles for user transactions.
//!
//! Before Alpha, system transactions pay the block's base fee out of `SYSTEM_CALLER`'s genesis
//! balance. The activation block zeroes that balance before its first transaction and keeps the
//! account's nonce and code; from then on system transactions carry gas price 0 and the balance
//! stays zero. Every committed block is checked against this rule.
//!
//! The module's own blocks: one before activation, where `SYSTEM_CALLER` pays exactly its
//! transactions' fees and simulation endpoints treat it like any address; the activation block,
//! where the balance goes to zero; and after activation, a block of user transactions calling
//! the BLS and randomness-by-height precompiles, where simulation endpoints exempt
//! `SYSTEM_CALLER`, and a block that replays stop inside of (`mid_block`).

mod mid_block;
mod simulation;

use super::{
    base::{BLS_ENOUGH_GAS, VALID_BLS_POP_INPUT},
    Chain, ScenarioBlock,
};
use crate::{
    node::{eip1559_tx, CommittedBlock, TestAccount, TRANSFER_GAS},
    report::BlockReport,
    timeline::{Fork, Phase},
};
use alloy_consensus::TxEip1559;
use alloy_primitives::{Bytes, TxKind, B256, U256};
use alloy_rpc_types_eth::TransactionReceipt;
use gravity_precompiles::bls_pop_verify::{BLS_PRECOMPILE_ADDR, POP_VERIFY_GAS};
use mid_block::MidBlock;
use reth_pipe_exec_layer_ext_v2::{
    onchain_config::SYSTEM_CALLER,
    randomness_precompile::{
        RANDOMNESS_BY_HEIGHT_PRECOMPILE_ADDR, RANDOMNESS_BY_HEIGHT_RECENT_GAS,
    },
};
use simulation::check_simulations;

/// Blocks planned after activation, in the order they run.
const AFTER_ACTIVATION_BLOCKS: [PlanFn; 2] = [Alpha::precompile_calls, Alpha::mid_block];

/// Gas limit of a user transaction calling the randomness-by-height precompile; ample.
const RANDOMNESS_CALL_GAS: u64 = 100_000;

/// Plans one block after activation; the block is checked against the returned expectation
/// once committed. `parent` is the last committed block.
type PlanFn = fn(&Chain<'_>, u64) -> (ScenarioBlock, Expected);

#[derive(Debug, Default)]
pub(super) struct Alpha {
    before_activation_planned: bool,
    /// Number of the activation block, once planned.
    activation: Option<u64>,
    after_activation_planned: usize,
    /// What the block planned last must show once committed.
    expected: Option<Expected>,
}

impl Alpha {
    pub(super) fn next_block(
        &mut self,
        chain: &Chain<'_>,
        phase: Phase,
        parent: u64,
    ) -> Option<ScenarioBlock> {
        let (block, expected) = match phase {
            // Early in the phase, so that the simulated next block is still before Alpha.
            Phase::After(Fork::Prague) if !self.before_activation_planned => {
                self.before_activation_planned = true;
                (ScenarioBlock::default(), Expected::BeforeActivation)
            }
            Phase::Activation(Fork::Alpha) => {
                self.activation = Some(parent + 1);
                (ScenarioBlock::default(), Expected::Activation)
            }
            Phase::After(Fork::Alpha) => {
                let plan = AFTER_ACTIVATION_BLOCKS.get(self.after_activation_planned)?;
                self.after_activation_planned += 1;
                plan(chain, parent)
            }
            _ => return None,
        };
        self.expected = Some(expected);
        Some(block)
    }

    pub(super) fn after_commit(
        &mut self,
        chain: &Chain<'_>,
        block: &CommittedBlock,
        report: &mut BlockReport<'_>,
    ) {
        // An activation block never changes the epoch, so it always reaches the scenarios: every
        // block from the planned one on runs under Alpha.
        let alpha_active = self.activation.is_some_and(|activation| block.number >= activation);
        check_system_transactions(chain, block, alpha_active, report);
        if let Some(expected) = self.expected.take() {
            expected.check(chain, block, report);
        }
    }

    pub(super) fn assert_all_ran(&self) {
        let ran = (
            self.before_activation_planned,
            self.activation.is_some(),
            self.after_activation_planned,
        );
        assert_eq!(
            ran,
            (true, true, AFTER_ACTIVATION_BLOCKS.len()),
            "Alpha scenario blocks (before activation, activation, after activation) did not \
             all run"
        );
    }

    /// Alice calls the BLS precompile and Bob the randomness-by-height precompile, for the
    /// parent's height. The chain installs both for user transactions from Alpha on.
    fn precompile_calls(chain: &Chain<'_>, parent: u64) -> (ScenarioBlock, Expected) {
        let call = |account: TestAccount, to, gas_limit, input| {
            account.sign(TxEip1559 {
                gas_limit,
                input,
                ..eip1559_tx(
                    chain.chain_id,
                    chain.nonce(account.address(), parent),
                    TxKind::Call(to),
                )
            })
        };
        let bls = call(
            TestAccount::Alice,
            BLS_PRECOMPILE_ADDR,
            BLS_ENOUGH_GAS,
            Bytes::from(VALID_BLS_POP_INPUT),
        );
        // The input is one ABI word: the block number.
        let randomness = call(
            TestAccount::Bob,
            RANDOMNESS_BY_HEIGHT_PRECOMPILE_ADDR,
            RANDOMNESS_CALL_GAS,
            Bytes::copy_from_slice(B256::from(U256::from(parent)).as_slice()),
        );
        let expected = Expected::PrecompileCalls { bls: bls.hash(), randomness: randomness.hash() };
        (ScenarioBlock { transactions: vec![bls, randomness], ..Default::default() }, expected)
    }

    fn mid_block(chain: &Chain<'_>, parent: u64) -> (ScenarioBlock, Expected) {
        let (block, mid_block) = MidBlock::plan(chain, parent);
        (block, Expected::MidBlock(mid_block))
    }
}

/// What an Alpha scenario block must show once committed.
#[derive(Debug)]
enum Expected {
    BeforeActivation,
    Activation,
    PrecompileCalls { bls: B256, randomness: B256 },
    MidBlock(MidBlock),
}

impl Expected {
    fn check(self, chain: &Chain<'_>, block: &CommittedBlock, report: &mut BlockReport<'_>) {
        let number = block.number;
        match self {
            Self::BeforeActivation => {
                let source = "scenario: SYSTEM_CALLER before Alpha";
                // The block is no epoch-change block, so its body holds every system
                // transaction it executed.
                let fees: U256 = system_receipts(chain, number)
                    .iter()
                    .map(|receipt| {
                        U256::from(receipt.gas_used) * U256::from(receipt.effective_gas_price)
                    })
                    .sum();
                let (before, after) = (
                    chain.balance(SYSTEM_CALLER, number - 1),
                    chain.balance(SYSTEM_CALLER, number),
                );
                let field = "balance decrease (the system transactions' fees)";
                report.check_eq(source, None, field, Some(fees), before.checked_sub(after));
                report.check_eq(source, None, "balance is non-zero", true, !after.is_zero());
                check_simulations(chain, block, false, report);
            }
            Self::Activation => {
                let source = "scenario: SYSTEM_CALLER on the Alpha activation block";
                let before = number - 1;
                let balance_before = chain.balance(SYSTEM_CALLER, before);
                let field = format!("balance after block {before} is non-zero");
                report.check_eq(source, None, field, true, !balance_before.is_zero());
                report.check_eq(
                    source,
                    None,
                    "balance",
                    U256::ZERO,
                    chain.balance(SYSTEM_CALLER, number),
                );
                // The migration keeps the nonce; only the block's own system transactions
                // move it.
                let sent = system_receipts(chain, number).len() as u64;
                let nonce_before = chain.nonce(SYSTEM_CALLER, before);
                report.check_eq(
                    source,
                    None,
                    "nonce",
                    nonce_before + sent,
                    chain.nonce(SYSTEM_CALLER, number),
                );
                let code_before = chain.code(SYSTEM_CALLER, before);
                report.check_eq(
                    source,
                    None,
                    "code",
                    code_before,
                    chain.code(SYSTEM_CALLER, number),
                );
            }
            Self::PrecompileCalls { bls, randomness } => {
                let source = "scenario: user calls to Gravity precompiles after Alpha";
                // Without the precompile, the call would hit an empty account and use only its
                // intrinsic gas.
                check_precompile_call(chain, report, source, bls, TRANSFER_GAS + POP_VERIFY_GAS);
                check_precompile_call(
                    chain,
                    report,
                    source,
                    randomness,
                    TRANSFER_GAS + RANDOMNESS_BY_HEIGHT_RECENT_GAS,
                );
                check_simulations(chain, block, true, report);
            }
            Self::MidBlock(mid_block) => mid_block.check(chain, block, report),
        }
    }
}

/// Every committed block: its system transactions succeed and pay the base fee before Alpha,
/// nothing from Alpha on, when `SYSTEM_CALLER`'s balance stays zero.
fn check_system_transactions(
    chain: &Chain<'_>,
    block: &CommittedBlock,
    alpha_active: bool,
    report: &mut BlockReport<'_>,
) {
    let source = "scenario: system transactions' gas";
    let receipts = system_receipts(chain, block.number);
    report.check_eq(source, None, "has system transactions", true, !receipts.is_empty());
    let gas_price = if alpha_active {
        0
    } else {
        let base_fee = chain.block(block.number).header.base_fee_per_gas.expect("London is active");
        u128::from(base_fee)
    };
    for receipt in &receipts {
        let index = receipt.transaction_index.map(|index| index as usize);
        report.check_eq(source, index, "success", true, receipt.status());
        report.check_eq(
            source,
            index,
            "effective gas price",
            gas_price,
            receipt.effective_gas_price,
        );
    }
    if alpha_active {
        let balance = chain.balance(SYSTEM_CALLER, block.number);
        report.check_eq(source, None, "SYSTEM_CALLER balance", U256::ZERO, balance);
    }
}

/// Receipts of the block's system transactions.
fn system_receipts(chain: &Chain<'_>, number: u64) -> Vec<TransactionReceipt> {
    chain.receipts(number).into_iter().filter(|receipt| receipt.from == SYSTEM_CALLER).collect()
}

/// The user transaction succeeded and used at least `min_gas_used`.
fn check_precompile_call(
    chain: &Chain<'_>,
    report: &mut BlockReport<'_>,
    source: &'static str,
    tx: B256,
    min_gas_used: u64,
) {
    let receipt = chain.receipt(tx);
    let success = receipt.as_ref().map(|receipt| receipt.status());
    report.check_eq(source, None, format!("{tx} included, success"), Some(true), success);
    let gas_used = receipt.map_or(0, |receipt| receipt.gas_used);
    if gas_used < min_gas_used {
        report.record(
            source,
            None,
            format!("{tx} gas used"),
            format!("≥ {min_gas_used}"),
            gas_used,
        );
    }
}
