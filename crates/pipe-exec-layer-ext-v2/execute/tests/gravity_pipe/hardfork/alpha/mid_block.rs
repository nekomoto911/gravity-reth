//! Replays that stop between the user transactions of one block.
//!
//! The block holds its system transaction, then account A calling the BLS precompile, then
//! account B transferring to an account nobody else touches. B leaves A alone, so at every
//! position inside the block A is either as the parent block left it (A's transaction not yet
//! replayed) or as the block left it (replayed). A paid for its transaction, so the two states
//! differ in balance and nonce. A replay that dropped the precompile would also leave A a
//! different balance: the call's gas would differ.
//!
//! Each endpoint counts positions its own way (`replay::mid_block` lists them); A is queried
//! at the positions of A's and of B's transaction. Calls read A's balance through a reader
//! contract that a state override installs at an address nobody uses.

use crate::{
    hardfork::{
        base::{BLS_ENOUGH_GAS, VALID_BLS_POP_INPUT},
        Chain, ScenarioBlock,
    },
    node::{eip1559_tx, CommittedBlock, TestAccount},
    report::BlockReport,
};
use alloy_consensus::{TrieAccount, TxEip1559};
use alloy_eips::BlockId;
use alloy_primitives::{hex, Address, Bytes, TxKind, B256, U256, U64};
use alloy_rpc_types_eth::{AccountInfo, EthCallResponse, StateContext, TransactionIndex};
use alloy_rpc_types_trace::geth::CallFrame;
use gravity_precompiles::bls_pop_verify::BLS_PRECOMPILE_ADDR;
use serde_json::json;

const SOURCE: &str = "scenario: Alpha mid-block replay";

const ACCOUNT_A: TestAccount = TestAccount::Carol;
const ACCOUNT_B: TestAccount = TestAccount::Dave;
/// Receives B's transfer; no other transaction touches it.
const B_RECIPIENT: Address = Address::repeat_byte(0xa1);

/// Installed by a state override: returns the balance of the address in its calldata.
/// `PUSH1 0 CALLDATALOAD BALANCE PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN`.
const BALANCE_READER_CODE: [u8; 12] = hex!("600035316000526020" "6000f3");
const BALANCE_READER: Address = Address::repeat_byte(0xa2);

/// The transactions of the mid-block replay block, to find their positions once it is
/// committed.
#[derive(Debug)]
pub(super) struct MidBlock {
    a_tx: B256,
    b_tx: B256,
}

impl MidBlock {
    pub(super) fn plan(chain: &Chain<'_>, parent: u64) -> (ScenarioBlock, Self) {
        let chain_id = chain.chain_id;
        let calls_bls = ACCOUNT_A.sign(TxEip1559 {
            gas_limit: BLS_ENOUGH_GAS,
            input: Bytes::from(VALID_BLS_POP_INPUT),
            ..eip1559_tx(
                chain_id,
                chain.nonce(ACCOUNT_A.address(), parent),
                TxKind::Call(BLS_PRECOMPILE_ADDR),
            )
        });
        let transfer = ACCOUNT_B.sign(TxEip1559 {
            value: U256::from(1),
            ..eip1559_tx(
                chain_id,
                chain.nonce(ACCOUNT_B.address(), parent),
                TxKind::Call(B_RECIPIENT),
            )
        });
        let planned = Self { a_tx: calls_bls.hash(), b_tx: transfer.hash() };
        (ScenarioBlock { transactions: vec![calls_bls, transfer], ..Default::default() }, planned)
    }

    pub(super) fn check(
        self,
        chain: &Chain<'_>,
        block: &CommittedBlock,
        report: &mut BlockReport<'_>,
    ) {
        // Step 1: the block is laid out as planned, right after its one system transaction.
        let hashes: Vec<B256> = chain.block(block.number).transactions.hashes().collect();
        let position = |tx| hashes.iter().position(|hash| *hash == tx);
        let (a_index, b_index) = (position(self.a_tx), position(self.b_tx));
        report.check_eq(
            SOURCE,
            None,
            "positions of A's and B's transactions",
            (Some(1), Some(2)),
            (a_index, b_index),
        );
        let (Some(a_index), Some(b_index)) = (a_index, b_index) else { return };

        // Step 2: A before and after its own transaction, as committed.
        let a = ACCOUNT_A.address();
        let before = AccountState::read(chain, a, block.number - 1);
        let after = AccountState::read(chain, a, block.number);
        report.check_eq(SOURCE, None, "A changed in the block", true, before != after);

        // Step 3: every endpoint at both positions.
        for index in [a_index, b_index] {
            // The state including transaction `index`.
            let including = if index >= a_index { &after } else { &before };
            check_account_at(chain, block, index, including, report);
            check_account_info_at(chain, block, index, including, report);
            // On top of the first `index` transactions.
            let on_top = if index > a_index { &after } else { &before };
            check_calls_at(chain, block, index, on_top.balance, report);
        }
    }
}

/// What the endpoints can show of account A.
#[derive(Debug, PartialEq, Eq)]
struct AccountState {
    balance: U256,
    nonce: u64,
}

impl AccountState {
    fn read(chain: &Chain<'_>, address: Address, number: u64) -> Self {
        Self { balance: chain.balance(address, number), nonce: chain.nonce(address, number) }
    }
}

/// `debug_accountAt` includes transaction `index`.
fn check_account_at(
    chain: &Chain<'_>,
    block: &CommittedBlock,
    index: usize,
    expected: &AccountState,
    report: &mut BlockReport<'_>,
) {
    let endpoint = "debug_accountAt";
    let params = json!([block.hash, U64::from(index), ACCOUNT_A.address()]);
    let actual = chain.request::<Option<TrieAccount>>(endpoint, params).map(|account| {
        account.map(|account| AccountState { balance: account.balance, nonce: account.nonce })
    });
    check_state(report, endpoint, index, expected, actual);
}

/// `debug_accountInfoAt` includes transaction `index`.
fn check_account_info_at(
    chain: &Chain<'_>,
    block: &CommittedBlock,
    index: usize,
    expected: &AccountState,
    report: &mut BlockReport<'_>,
) {
    let endpoint = "debug_accountInfoAt";
    let params = json!([block.hash, U64::from(index), ACCOUNT_A.address()]);
    let actual = chain
        .request::<Option<AccountInfo>>(endpoint, params)
        .map(|info| info.map(|info| AccountState { balance: info.balance, nonce: info.nonce }));
    check_state(report, endpoint, index, expected, actual);
}

fn check_state(
    report: &mut BlockReport<'_>,
    endpoint: &str,
    index: usize,
    expected: &AccountState,
    actual: Result<Option<AccountState>, String>,
) {
    let field = format!("{endpoint}: A (balance, nonce)");
    match actual {
        Ok(actual) => report.check_eq(SOURCE, Some(index), field, Some(expected), actual.as_ref()),
        Err(error) => report.record(SOURCE, Some(index), field, format!("{expected:?}"), error),
    }
}

/// `debug_traceCall` with `txIndex = index`, and `eth_callMany` / `debug_traceCallMany` with
/// `transactionIndex = index`, call on top of the block's first `index` transactions.
fn check_calls_at(
    chain: &Chain<'_>,
    block: &CommittedBlock,
    index: usize,
    expected_balance: U256,
    report: &mut BlockReport<'_>,
) {
    let read_a = json!({
        "to": BALANCE_READER,
        "input": B256::left_padding_from(ACCOUNT_A.address().as_slice()),
    });
    let reader =
        json!({ BALANCE_READER.to_string(): { "code": Bytes::from(BALANCE_READER_CODE) } });
    let at = StateContext {
        block_number: Some(BlockId::hash(block.hash)),
        transaction_index: Some(TransactionIndex::Index(index)),
    };

    let endpoint = "debug_traceCall";
    let options =
        json!({ "tracer": "callTracer", "stateOverrides": reader, "txIndex": U64::from(index) });
    let output = chain
        .request::<CallFrame>(endpoint, json!([read_a, block.hash, options]))
        .map(|frame| frame.output);
    check_balance(report, endpoint, index, expected_balance, output);

    let endpoint = "eth_callMany";
    let bundles = json!([{ "transactions": [read_a] }]);
    let output = chain
        .request::<Vec<Vec<EthCallResponse>>>(endpoint, json!([bundles, at, reader]))
        .map(|mut results| {
            results.pop().and_then(|mut bundle| bundle.pop()).and_then(|result| result.value)
        });
    check_balance(report, endpoint, index, expected_balance, output);

    let endpoint = "debug_traceCallMany";
    let options = json!({ "tracer": "callTracer", "stateOverrides": reader });
    let output = chain.request::<Vec<Vec<CallFrame>>>(endpoint, json!([bundles, at, options])).map(
        |mut frames| {
            frames.pop().and_then(|mut bundle| bundle.pop()).and_then(|frame| frame.output)
        },
    );
    check_balance(report, endpoint, index, expected_balance, output);
}

fn check_balance(
    report: &mut BlockReport<'_>,
    endpoint: &str,
    index: usize,
    expected: U256,
    output: Result<Option<Bytes>, String>,
) {
    let field = format!("{endpoint}: A balance");
    match output {
        Ok(output) => {
            let actual = output
                .filter(|output| output.len() == 32)
                .map(|output| U256::from_be_slice(&output));
            report.check_eq(SOURCE, Some(index), field, Some(expected), actual);
        }
        Err(error) => report.record(SOURCE, Some(index), field, expected, error),
    }
}
