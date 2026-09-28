//! Beta: EIP-7702 unlock and skip-and-continue gas packing.
//!
//! Until Beta the pipe's transaction filter locks EIP-7702 down: it drops every EIP-7702
//! transaction, and every transaction sent to or from an account that is already delegated (only
//! genesis can delegate one then: `TestAccount::Delegated`). Its gas packing budgets the gas
//! limits of the block's transactions in order and, at the first that does not fit, drops that one
//! and every later one. From Beta on the lockdown is gone, and packing drops only the transactions
//! that do not fit and goes on with the next.
//!
//! Before activation, one block each: the lockdown (an EIP-7702 transaction, one to and one from
//! the delegated account, between valid ones), and packing (a transaction that fits, one that
//! overflows the budget, and a small one after it: only the first is included).
//!
//! The activation block already runs under Beta: Carol deploys a contract whose code runs
//! `CREATE`, Alice's EIP-7702 transaction delegates Erin to it, and the transactions to and from
//! the delegated account are included now.
//!
//! After activation: a call to Erin runs that contract in her context, where the pipe forbids
//! `CREATE`: the call halts and Erin's nonce stays. Packing again, now including the small
//! transaction. The base block of invalid transactions again, where the EIP-7702 transaction is
//! valid and EIP-7702 transactions without an authorization or below their intrinsic gas are
//! dropped.

use super::{
    base::{check_tx_success, invalid_transactions_block, Inclusion, GAS_LIMIT},
    Chain, ScenarioBlock,
};
use crate::{
    node::{
        delegation_code, eip1559_tx, eip7702_tx, CommittedBlock, SignedTx, TestAccount, DELEGATE,
        TRANSFER_GAS,
    },
    report::BlockReport,
    timeline::{Fork, Phase},
};
use alloy_consensus::TxEip1559;
use alloy_primitives::{hex, Address, TxKind, B256};
use gravity_primitives::PIPE_BLOCK_GAS_LIMIT;

/// Blocks planned before activation, in the order they run.
const BEFORE_ACTIVATION_BLOCKS: [PlanFn; 2] = [Beta::lockdown, Beta::packing_before_activation];

/// Blocks planned after activation, in the order they run.
const AFTER_ACTIVATION_BLOCKS: [PlanFn; 3] =
    [Beta::delegated_create, Beta::packing_after_activation, Beta::invalid_transactions];

/// Init code deploying the runtime `CREATE(0, 0, 0) POP STOP`:
/// `MSTORE(0, runtime) RETURN(23, 9)`.
const CREATE_CONTRACT_INIT_CODE: [u8; 18] = hex!("68600060006000f0500060005260096017f3");

/// Gas limit of the two large packing transactions. The budget of a block's user transactions
/// is the block gas limit minus what its system transactions used, so either one fits and both
/// together do not. Packing budgets gas limits, so the transactions stay cheap to execute and
/// replay.
const PACKING_GAS_LIMIT: u64 = PIPE_BLOCK_GAS_LIMIT / 2;

/// Plans one block; the block is checked against the returned expectation once committed.
/// `parent` is the last committed block.
type PlanFn = fn(&Chain<'_>, u64) -> (ScenarioBlock, Expected);

#[derive(Debug, Default)]
pub(super) struct Beta {
    before_activation_planned: usize,
    activation_planned: bool,
    after_activation_planned: usize,
    /// What the block planned last must show once committed.
    expected: Option<Expected>,
}

impl Beta {
    pub(super) fn next_block(
        &mut self,
        chain: &Chain<'_>,
        phase: Phase,
        parent: u64,
    ) -> Option<ScenarioBlock> {
        let plan = match phase {
            Phase::After(Fork::Alpha) => {
                next_plan(&BEFORE_ACTIVATION_BLOCKS, &mut self.before_activation_planned)?
            }
            Phase::Activation(Fork::Beta) => {
                self.activation_planned = true;
                Self::eip7702_unlocked
            }
            Phase::After(Fork::Beta) => {
                next_plan(&AFTER_ACTIVATION_BLOCKS, &mut self.after_activation_planned)?
            }
            _ => return None,
        };
        let (block, expected) = plan(chain, parent);
        self.expected = Some(expected);
        Some(block)
    }

    pub(super) fn after_commit(
        &mut self,
        chain: &Chain<'_>,
        block: &CommittedBlock,
        report: &mut BlockReport<'_>,
    ) {
        if let Some(expected) = self.expected.take() {
            expected.check(chain, block, report);
        }
    }

    pub(super) fn assert_all_ran(&self) {
        let ran = (
            self.before_activation_planned,
            self.activation_planned,
            self.after_activation_planned,
        );
        assert_eq!(
            ran,
            (BEFORE_ACTIVATION_BLOCKS.len(), true, AFTER_ACTIVATION_BLOCKS.len()),
            "Beta scenario blocks (before activation, activation, after activation) did not all \
             run"
        );
    }

    /// Alice's valid transfers around an EIP-7702 transaction, a transaction to the delegated
    /// account and one from it. Each of the three is valid but for its EIP-7702 aspect.
    fn lockdown(chain: &Chain<'_>, parent: u64) -> (ScenarioBlock, Expected) {
        let (alice, chain_id) = (TestAccount::Alice, chain.chain_id);
        let nonce = chain.nonce(alice.address(), parent);
        let bob = TestAccount::Bob.address();
        let erin = TestAccount::Erin;

        // The rejected transactions from Alice take the nonce her last valid one then uses.
        let set_code = alice.sign(eip7702_tx(
            chain_id,
            nonce + 1,
            bob,
            vec![erin.authorize(chain_id, DELEGATE, chain.nonce(erin.address(), parent))],
        ));
        let [to_delegated, from_delegated] = delegated_transfers(chain, parent, alice, nonce + 1);
        let (block, inclusion) = Inclusion::plan(vec![
            (alice.sign(eip1559_tx(chain_id, nonce, TxKind::Call(bob))), true),
            (set_code, false),
            (to_delegated, false),
            (from_delegated, false),
            (alice.sign(eip1559_tx(chain_id, nonce + 1, TxKind::Call(bob))), true),
        ]);
        (
            block,
            Expected::Inclusion { source: "scenario: EIP-7702 lockdown before Beta", inclusion },
        )
    }

    fn packing_before_activation(chain: &Chain<'_>, parent: u64) -> (ScenarioBlock, Expected) {
        let (block, inclusion) = packing(chain, parent, false);
        (block, Expected::Inclusion { source: "scenario: gas packing before Beta", inclusion })
    }

    /// Carol deploys a contract whose code runs `CREATE`; Alice's EIP-7702 transaction delegates
    /// Erin to it; the delegated account sends a transfer and Dave sends one to it. All are
    /// included and succeed.
    fn eip7702_unlocked(chain: &Chain<'_>, parent: u64) -> (ScenarioBlock, Expected) {
        let chain_id = chain.chain_id;
        let nonce = |account: TestAccount| chain.nonce(account.address(), parent);
        let (carol, alice, erin) = (TestAccount::Carol, TestAccount::Alice, TestAccount::Erin);

        let deploy = carol.sign(TxEip1559 {
            gas_limit: GAS_LIMIT,
            input: CREATE_CONTRACT_INIT_CODE.into(),
            ..eip1559_tx(chain_id, nonce(carol), TxKind::Create)
        });
        let create_contract = carol.address().create(nonce(carol));
        // The call goes to Bob, not to Erin: running the contract belongs to the next block.
        let delegation = alice.sign(eip7702_tx(
            chain_id,
            nonce(alice),
            TestAccount::Bob.address(),
            vec![erin.authorize(chain_id, create_contract, nonce(erin))],
        ));
        let dave = TestAccount::Dave;
        let [to_delegated, from_delegated] = delegated_transfers(chain, parent, dave, nonce(dave));

        let (block, inclusion) = Inclusion::plan(
            [deploy, delegation, to_delegated, from_delegated].map(|tx| (tx, true)).into(),
        );
        let expected =
            Expected::Unlocked { inclusion, authority: erin.address(), delegate: create_contract };
        (block, expected)
    }

    /// Carol calls Erin, whose delegate's code runs `CREATE` in Erin's context.
    fn delegated_create(chain: &Chain<'_>, parent: u64) -> (ScenarioBlock, Expected) {
        let carol = TestAccount::Carol;
        let authority = TestAccount::Erin.address();
        let call = carol.sign(TxEip1559 {
            gas_limit: GAS_LIMIT,
            ..eip1559_tx(
                chain.chain_id,
                chain.nonce(carol.address(), parent),
                TxKind::Call(authority),
            )
        });
        let expected =
            Expected::DelegatedCreate { call: call.hash(), gas_limit: GAS_LIMIT, authority };
        (ScenarioBlock { transactions: vec![call], ..Default::default() }, expected)
    }

    fn packing_after_activation(chain: &Chain<'_>, parent: u64) -> (ScenarioBlock, Expected) {
        let (block, inclusion) = packing(chain, parent, true);
        (block, Expected::Inclusion { source: "scenario: gas packing after Beta", inclusion })
    }

    fn invalid_transactions(chain: &Chain<'_>, parent: u64) -> (ScenarioBlock, Expected) {
        let (block, inclusion) = invalid_transactions_block(chain, parent, true);
        let source = "scenario: invalid transactions after Beta";
        (block, Expected::Inclusion { source, inclusion })
    }
}

/// What a Beta scenario block must show once committed.
#[derive(Debug)]
enum Expected {
    /// Of the submitted transactions, exactly the included ones are in the block, in order.
    Inclusion { source: &'static str, inclusion: Inclusion },
    /// Every submitted transaction is included and succeeds, and `authority` is delegated to
    /// `delegate`.
    Unlocked { inclusion: Inclusion, authority: Address, delegate: Address },
    /// `call` ran `CREATE` in the context of `authority`, a delegated account.
    DelegatedCreate { call: B256, gas_limit: u64, authority: Address },
}

impl Expected {
    fn check(self, chain: &Chain<'_>, block: &CommittedBlock, report: &mut BlockReport<'_>) {
        let number = block.number;
        match self {
            Self::Inclusion { source, inclusion } => inclusion.check(chain, block, report, source),
            Self::Unlocked { inclusion, authority, delegate } => {
                let source = "scenario: EIP-7702 on the Beta activation block";
                inclusion.check(chain, block, report, source);
                for &tx in &inclusion.included {
                    check_tx_success(chain, report, source, tx, true);
                }
                let code = chain.code(authority, number);
                report.check_eq(
                    source,
                    None,
                    format!("{authority} code"),
                    delegation_code(delegate),
                    code,
                );
                // Applying the authorization bumps the authority's nonce.
                let nonce_before = chain.nonce(authority, number - 1);
                let nonce = chain.nonce(authority, number);
                report.check_eq(
                    source,
                    None,
                    format!("{authority} nonce"),
                    nonce_before + 1,
                    nonce,
                );
            }
            Self::DelegatedCreate { call, gas_limit, authority } => {
                let source = "scenario: CREATE in a delegated account";
                check_tx_success(chain, report, source, call, false);
                // A halt uses all the gas; a revert would return the rest.
                let gas_used = chain.receipt(call).map(|receipt| receipt.gas_used);
                report.check_eq(
                    source,
                    None,
                    format!("{call} gas used"),
                    Some(gas_limit),
                    gas_used,
                );
                // A CREATE would bump the authority's nonce and give the created account nonce 1.
                let nonce_before = chain.nonce(authority, number - 1);
                let nonce = chain.nonce(authority, number);
                report.check_eq(source, None, format!("{authority} nonce"), nonce_before, nonce);
                let created = authority.create(nonce_before);
                let field = format!("nonce of {created}, the address CREATE would use");
                report.check_eq(source, None, field, 0, chain.nonce(created, number));
            }
        }
    }
}

/// Returns the next of `plans` and counts it as planned, or `None` once all are.
fn next_plan(plans: &[PlanFn], planned: &mut usize) -> Option<PlanFn> {
    let plan = *plans.get(*planned)?;
    *planned += 1;
    Some(plan)
}

/// A transaction from `sender` to the delegated account, and one from the delegated account.
fn delegated_transfers(
    chain: &Chain<'_>,
    parent: u64,
    sender: TestAccount,
    sender_nonce: u64,
) -> [SignedTx; 2] {
    let (delegated, chain_id) = (TestAccount::Delegated, chain.chain_id);
    // Calling a delegated account also loads its delegate, which a transfer's gas does not cover.
    let to_delegated = sender.sign(TxEip1559 {
        gas_limit: GAS_LIMIT,
        ..eip1559_tx(chain_id, sender_nonce, TxKind::Call(delegated.address()))
    });
    let from_delegated = delegated.sign(eip1559_tx(
        chain_id,
        chain.nonce(delegated.address(), parent),
        TxKind::Call(TestAccount::Bob.address()),
    ));
    [to_delegated, from_delegated]
}

/// Alice's and Bob's transactions with [`PACKING_GAS_LIMIT`] each, then Carol's transfer. Only
/// Bob's overflows the budget; before Beta it cuts Carol's too, from Beta on it does not.
fn packing(chain: &Chain<'_>, parent: u64, beta_active: bool) -> (ScenarioBlock, Inclusion) {
    let to_dave = TxKind::Call(TestAccount::Dave.address());
    let transfer = |account: TestAccount, gas_limit| {
        account.sign(TxEip1559 {
            gas_limit,
            ..eip1559_tx(chain.chain_id, chain.nonce(account.address(), parent), to_dave)
        })
    };
    Inclusion::plan(vec![
        (transfer(TestAccount::Alice, PACKING_GAS_LIMIT), true),
        (transfer(TestAccount::Bob, PACKING_GAS_LIMIT), false),
        (transfer(TestAccount::Carol, TRANSFER_GAS), beta_active),
    ])
}
