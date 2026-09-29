//! Prague: the EIP-2935 history storage contract.
//!
//! Gravity reaches Prague by timestamp on a running chain, so the pipe deploys the contract
//! itself on the activation block: nonce 1, the EIP's code, no storage. From then on, the
//! EIP-2935 system call of every block stores the parent's id at slot
//! `(number - 1) % HISTORY_SERVE_WINDOW`, and the id is the one gravity-sdk gave the parent,
//! not the parent's sealed hash. An epoch-change block is the exception: it runs only its
//! system transactions, never the executor that makes that call, so it stores nothing.
//!
//! One genesis-phase block shows the contract absent: no code, zero storage, and a call to it
//! returns nothing without reverting. The activation block deploys it and writes the first
//! slot. Two blocks after activation show that the deployment never fires again (code and
//! nonce unchanged, every slot since activation as written) and that `eth_call` serves the
//! stored id of a number inside the window and reverts for a number outside it. The chain is
//! shorter than the window, so only the numbers at or above the current block are outside.

use super::{Chain, ScenarioBlock};
use crate::{
    node::CommittedBlock,
    report::BlockReport,
    timeline::{Fork, Phase},
};
use alloy_eips::eip2935::{HISTORY_SERVE_WINDOW, HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE};
use alloy_primitives::{Bytes, B256, U256};
use std::collections::BTreeMap;

/// Blocks checked after activation. The second also shows that the slot written by the first
/// survives later blocks, not only the activation block's slot.
const AFTER_ACTIVATION_BLOCKS: usize = 2;

/// The deployment sets the contract's nonce, like a contract created by a transaction.
const DEPLOYED_NONCE: u64 = 1;

#[derive(Debug, Default)]
pub(super) struct Prague {
    before_activation_planned: bool,
    /// Number of the activation block, once planned.
    activation: Option<u64>,
    after_activation_planned: usize,
    /// What the block planned last must show once committed.
    expected: Option<Expected>,
    /// What the contract holds for each block number from the activation block's parent on,
    /// known once the block's child is committed: the block's id, or zero.
    stored_ids: BTreeMap<u64, B256>,
}

impl Prague {
    pub(super) fn next_block(&mut self, phase: Phase, parent: u64) -> Option<ScenarioBlock> {
        let expected = match phase {
            Phase::Genesis if !self.before_activation_planned => {
                self.before_activation_planned = true;
                Expected::BeforeActivation
            }
            Phase::Activation(Fork::Prague) => {
                self.activation = Some(parent + 1);
                Expected::Activation
            }
            Phase::After(Fork::Prague)
                if self.after_activation_planned < AFTER_ACTIVATION_BLOCKS =>
            {
                self.after_activation_planned += 1;
                // An activation block never changes the epoch, so it always reaches the
                // scenarios.
                let activation = self.activation.expect("the Prague activation block was planned");
                Expected::AfterActivation { activation }
            }
            _ => return None,
        };
        self.expected = Some(expected);
        // Only the protocol's own system call writes the contract: the block needs no content.
        Some(ScenarioBlock::default())
    }

    pub(super) fn after_commit(
        &mut self,
        chain: &Chain<'_>,
        block: &CommittedBlock,
        report: &mut BlockReport<'_>,
    ) {
        if self.activation.is_some_and(|activation| block.number >= activation) {
            let stored = eip2935_parent_id(block).unwrap_or(B256::ZERO);
            self.stored_ids.insert(block.number - 1, stored);
        }
        if let Some(expected) = self.expected.take() {
            expected.check(chain, block, &self.stored_ids, report);
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
            (true, true, AFTER_ACTIVATION_BLOCKS),
            "Prague scenario blocks (before activation, activation, after activation) did not \
             all run"
        );
    }
}

/// What `block`'s EIP-2935 system call stores for its parent once Prague is active: the parent's
/// consensus id. An epoch-change block runs only its system transactions, never the executor
/// that makes the call, so it stores nothing.
pub(crate) const fn eip2935_parent_id(block: &CommittedBlock) -> Option<B256> {
    if block.epoch_changed {
        None
    } else {
        Some(block.parent_id)
    }
}

/// What a Prague scenario block must show once committed.
#[derive(Debug)]
enum Expected {
    BeforeActivation,
    Activation,
    AfterActivation { activation: u64 },
}

impl Expected {
    fn check(
        self,
        chain: &Chain<'_>,
        block: &CommittedBlock,
        stored_ids: &BTreeMap<u64, B256>,
        report: &mut BlockReport<'_>,
    ) {
        let number = block.number;
        match self {
            Self::BeforeActivation => {
                let source = "scenario: EIP-2935 before Prague";
                check_absent(chain, report, source, number);
                // The slot this block's system call would have written, and the ring's last.
                for slot_of in [number - 1, HISTORY_SERVE_WINDOW as u64 - 1] {
                    check_slot(chain, report, source, number, slot_of, B256::ZERO);
                }
                // Calling an account without code succeeds and returns nothing.
                report.check_eq(
                    source,
                    None,
                    format!("eth_call(n = {}) output", number - 1),
                    Some(Bytes::new()),
                    get(chain, number - 1, number),
                );
            }
            Self::Activation => {
                let source = "scenario: EIP-2935 Prague activation";
                check_absent(chain, report, source, number - 1);
                check_deployed(chain, report, source, number);
                check_slot(chain, report, source, number, number - 1, block.parent_id);
                // The next block's slot waits for the next block.
                check_slot(chain, report, source, number, number, B256::ZERO);
            }
            Self::AfterActivation { activation } => {
                let source = "scenario: EIP-2935 after Prague";
                check_deployed(chain, report, source, number);
                for (&slot_of, &stored) in stored_ids.range(activation - 1..number) {
                    check_slot(chain, report, source, number, slot_of, stored);
                }

                // Inside the window: the first and the latest stored id, and a number whose
                // slot no block wrote (its block predates Prague).
                for n in [activation - 1, number - 1] {
                    let id = Bytes::copy_from_slice(stored_ids[&n].as_slice());
                    let field = format!("eth_call(n = {n}) output");
                    report.check_eq(source, None, field, Some(id), get(chain, n, number));
                }
                let zero = Bytes::copy_from_slice(B256::ZERO.as_slice());
                report.check_eq(
                    source,
                    None,
                    "eth_call(n = 0) output",
                    Some(zero),
                    get(chain, 0, number),
                );

                // Outside the window: the current block, a future one, the largest number.
                for n in [number, number + 1, u64::MAX] {
                    let field = format!("eth_call(n = {n}) output (None = reverted)");
                    report.check_eq(source, None, field, None, get(chain, n, number));
                }
            }
        }
    }
}

/// The contract was not deployed by block `number`: no code, and not the nonce the deployment
/// sets.
fn check_absent(
    chain: &Chain<'_>,
    report: &mut BlockReport<'_>,
    source: &'static str,
    number: u64,
) {
    let code = chain.code(HISTORY_STORAGE_ADDRESS, number);
    report.check_eq(source, None, format!("code after block {number}"), Bytes::new(), code);
    let nonce = chain.nonce(HISTORY_STORAGE_ADDRESS, number);
    report.check_eq(source, None, format!("nonce after block {number}"), 0, nonce);
}

/// The contract holds the EIP's code and the deployment's nonce after block `number`.
fn check_deployed(
    chain: &Chain<'_>,
    report: &mut BlockReport<'_>,
    source: &'static str,
    number: u64,
) {
    let code = chain.code(HISTORY_STORAGE_ADDRESS, number);
    report.check_eq(source, None, "code", HISTORY_STORAGE_CODE.clone(), code);
    let nonce = chain.nonce(HISTORY_STORAGE_ADDRESS, number);
    report.check_eq(source, None, "nonce", DEPLOYED_NONCE, nonce);
}

/// After block `number`, the slot that keeps the id of block `slot_of` holds `expected`.
fn check_slot(
    chain: &Chain<'_>,
    report: &mut BlockReport<'_>,
    source: &'static str,
    number: u64,
    slot_of: u64,
    expected: B256,
) {
    let slot = U256::from(slot_of % HISTORY_SERVE_WINDOW as u64);
    let actual = chain.storage(HISTORY_STORAGE_ADDRESS, slot, number);
    report.check_eq(source, None, format!("slot of block {slot_of}"), expected, actual);
}

/// The contract's `get` for block `n`, called on the state after block `number`: its output,
/// or `None` if it reverted.
fn get(chain: &Chain<'_>, n: u64, number: u64) -> Option<Bytes> {
    let input = Bytes::copy_from_slice(B256::from(U256::from(n)).as_slice());
    chain.call_or_revert(HISTORY_STORAGE_ADDRESS, input, number)
}
