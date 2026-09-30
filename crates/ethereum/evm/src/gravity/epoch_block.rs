//! Facts about committed Gravity blocks that replay needs and the block body alone does not show.
//!
//! Before the Alpha hardfork, when a block's DKG `finishTransition` emitted `NewEpoch`, the pipe
//! executed `Blocker.onBlockStart` and then `finishTransition`, but assembled only
//! `finishTransition` into the body. The committed state covers both calls, so replaying the body
//! alone starts one `SYSTEM_CALLER` nonce short and fails with `nonce too high`. These blocks are
//! published and cannot change, so replay re-executes the omitted `onBlockStart`, rebuilt from
//! chain state.

use alloc::{format, vec};
use alloy_consensus::{transaction::SignerRecoverable, BlockHeader, Signed, Transaction, TxLegacy};
use alloy_primitives::{Address, Signature, TxKind, U256};
use alloy_sol_types::{SolCall, SolEvent};
use reth_chainspec::{
    gravity_system_contracts::{
        finishTransitionCall, getActiveValidatorsCall, onBlockStartCall, NewEpochEvent, BLOCK_ADDR,
        RECONFIGURATION_ADDR, SYSTEM_TXN_GAS_LIMIT,
    },
    is_gravity_system_caller, EthChainSpec, GravityHardfork, SYSTEM_CALLER,
};
use reth_ethereum_primitives::{Block, Receipt, TransactionSigned};
use reth_evm::execute::BlockExecutionError;
use reth_primitives_traits::SealedBlock;
use revm::context::result::ExecutionResult;

/// What the body of a committed block says about how the pipe executed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BodyFacts {
    /// Every transaction is from `SYSTEM_CALLER`. An epoch change drops all user transactions,
    /// so only such a block can be an epoch-change block.
    pub(crate) only_system_txs: bool,
    /// See [`is_pre_alpha_dkg_epoch_block`].
    pub(crate) is_pre_alpha_dkg_epoch_block: bool,
}

impl BodyFacts {
    /// Reads the facts from `block`, recovering senders the way committed blocks are read:
    /// a system transaction has no recoverable signature and counts as `SYSTEM_CALLER`.
    ///
    /// System transactions come first, so recovery stops at the first user transaction.
    pub(crate) fn of<ChainSpec: EthChainSpec>(
        chain_spec: &ChainSpec,
        block: &SealedBlock<Block>,
    ) -> Self {
        let transactions = &block.body().transactions;
        let system_prefix = transactions
            .iter()
            .take_while(|tx| {
                is_gravity_system_caller(tx.recover_signer_unchecked().unwrap_or(SYSTEM_CALLER))
            })
            .count();
        Self {
            only_system_txs: system_prefix == transactions.len(),
            is_pre_alpha_dkg_epoch_block: is_pre_alpha_dkg_epoch_block(
                chain_spec,
                block.header().timestamp(),
                transactions[..system_prefix].first(),
            ),
        }
    }
}

/// Returns whether a block at `timestamp` whose first system transaction is
/// `first_system_tx` is a pre-Alpha DKG epoch-change block, whose body omits the `onBlockStart`
/// the pipe executed before its `finishTransition`.
///
/// Every other pre-Alpha body starts with `onBlockStart`, including an epoch change that
/// `onBlockStart` itself triggers, so a body that starts with a system `finishTransition`
/// identifies exactly these blocks without reading state.
fn is_pre_alpha_dkg_epoch_block<ChainSpec: EthChainSpec>(
    chain_spec: &ChainSpec,
    timestamp: u64,
    first_system_tx: Option<&TransactionSigned>,
) -> bool {
    if chain_spec.gravity_hardforks().is_fork_active_at_timestamp(GravityHardfork::Alpha, timestamp)
    {
        return false
    }

    first_system_tx.is_some_and(|tx| {
        tx.to() == Some(RECONFIGURATION_ADDR) &&
            tx.input().starts_with(&finishTransitionCall::SELECTOR)
    })
}

/// Returns whether `receipts` contain the `NewEpochEvent` of an epoch change, which is how the
/// pipe decides a block changed the epoch.
pub(crate) fn changes_epoch(receipts: &[Receipt]) -> bool {
    receipts.iter().flat_map(|receipt| &receipt.logs).any(|log| {
        log.address == RECONFIGURATION_ADDR &&
            log.topics().first() == Some(&NewEpochEvent::SIGNATURE_HASH)
    })
}

/// Returns the `validatorIndex` of `validator` in a `getActiveValidators()` result.
///
/// gravity-sdk sets the block beneficiary to the `validator` address it maps the proposer index
/// to, so this inverts that mapping. A beneficiary missing from the set is an error: guessing an
/// index would replay a different `onBlockStart` than the one the chain executed.
pub(crate) fn active_validator_index<H: core::fmt::Debug>(
    validator: Address,
    active_validators: &ExecutionResult<H>,
) -> Result<u64, BlockExecutionError> {
    let ExecutionResult::Success { output, .. } = active_validators else {
        return Err(BlockExecutionError::msg(format!(
            "getActiveValidators() failed: {active_validators:?}"
        )))
    };
    getActiveValidatorsCall::abi_decode_returns(output.data())
        .map_err(|err| {
            BlockExecutionError::msg(format!("invalid getActiveValidators() output: {err}"))
        })?
        .into_iter()
        .find(|info| info.validator == validator)
        .map(|info| info.validatorIndex)
        .ok_or_else(|| {
            BlockExecutionError::msg(format!(
                "block beneficiary {validator} is not an active validator"
            ))
        })
}

/// Builds the omitted `onBlockStart` exactly as the pipe did: an unsigned legacy transaction
/// that pays the block base fee, with an empty failed-proposer list.
///
/// The failed-proposer list is not recorded on chain. Every entry adds gas, and the empty list
/// reproduces the committed state of the affected mainnet and testnet blocks.
pub(crate) fn on_block_start_txn(
    nonce: u64,
    base_fee: u64,
    proposer_index: u64,
    timestamp_micros: u64,
) -> TransactionSigned {
    let call = onBlockStartCall {
        proposerIndex: proposer_index,
        failedProposerIndices: vec![],
        timestampMicros: timestamp_micros,
    };
    let tx = TxLegacy {
        chain_id: None,
        nonce,
        gas_price: base_fee.into(),
        gas_limit: SYSTEM_TXN_GAS_LIMIT,
        to: TxKind::Call(BLOCK_ADDR),
        value: U256::ZERO,
        input: call.abi_encode().into(),
    };
    let unsigned = Signature::new(U256::ZERO, U256::ZERO, false);
    TransactionSigned::Legacy(Signed::new_unhashed(tx, unsigned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use alloy_consensus::{BlockBody, Header};
    use alloy_primitives::{Bytes, Log, LogData};
    use reth_chainspec::{
        gravity_system_contracts::ValidatorConsensusInfo, ChainHardforks, ChainSpec, ForkCondition,
        Hardfork,
    };
    use revm::context::result::{HaltReason, Output, ResultGas, SuccessReason};

    const ALPHA_TIME: u64 = 1_000;

    fn chain_spec() -> ChainSpec {
        ChainSpec {
            gravity_hardforks: ChainHardforks::new(vec![(
                GravityHardfork::Alpha.boxed(),
                ForkCondition::Timestamp(ALPHA_TIME),
            )]),
            ..Default::default()
        }
    }

    fn system_txn(nonce: u64, to: Address, input: Vec<u8>) -> TransactionSigned {
        let tx = TxLegacy {
            nonce,
            gas_limit: SYSTEM_TXN_GAS_LIMIT,
            to: TxKind::Call(to),
            input: input.into(),
            ..Default::default()
        };
        TransactionSigned::Legacy(Signed::new_unhashed(
            tx,
            Signature::new(U256::ZERO, U256::ZERO, false),
        ))
    }

    fn user_txn(to: Address, input: Vec<u8>) -> TransactionSigned {
        let tx = TxLegacy { to: TxKind::Call(to), input: input.into(), ..Default::default() };
        reth_testing_utils::generators::sign_tx_with_random_key_pair(
            &mut reth_testing_utils::generators::rng(),
            tx.into(),
        )
    }

    fn on_block_start() -> TransactionSigned {
        let call = onBlockStartCall {
            proposerIndex: 0,
            failedProposerIndices: vec![],
            timestampMicros: 1,
        };
        system_txn(0, BLOCK_ADDR, call.abi_encode())
    }

    fn finish_transition_input() -> Vec<u8> {
        finishTransitionCall { dkgResult: Bytes::from_static(&[0xab]) }.abi_encode()
    }

    fn finish_transition() -> TransactionSigned {
        system_txn(1, RECONFIGURATION_ADDR, finish_transition_input())
    }

    fn block(timestamp: u64, transactions: Vec<TransactionSigned>) -> SealedBlock<Block> {
        SealedBlock::seal_slow(Block {
            header: Header { timestamp, ..Default::default() },
            body: BlockBody { transactions, ..Default::default() },
        })
    }

    fn validator(address: Address, index: u64) -> ValidatorConsensusInfo {
        ValidatorConsensusInfo {
            validator: address,
            consensusPubkey: Bytes::new(),
            consensusPop: Bytes::new(),
            votingPower: U256::ZERO,
            validatorIndex: index,
            networkAddresses: Bytes::new(),
            fullnodeAddresses: Bytes::new(),
        }
    }

    fn call_output(output: Vec<u8>) -> ExecutionResult<HaltReason> {
        ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: ResultGas::default(),
            logs: vec![],
            output: Output::Call(output.into()),
        }
    }

    #[test]
    fn detects_pre_alpha_body_starting_with_system_finish_transition() {
        let facts = BodyFacts::of(&chain_spec(), &block(ALPHA_TIME - 1, vec![finish_transition()]));
        assert_eq!(facts, BodyFacts { only_system_txs: true, is_pre_alpha_dkg_epoch_block: true });
    }

    #[test]
    fn ignores_every_other_block_shape() {
        let cases = [
            ("post-Alpha", block(ALPHA_TIME, vec![finish_transition()])),
            (
                "body starting with onBlockStart",
                block(ALPHA_TIME - 1, vec![on_block_start(), finish_transition()]),
            ),
            (
                "non-system sender",
                block(
                    ALPHA_TIME - 1,
                    vec![user_txn(RECONFIGURATION_ADDR, finish_transition_input())],
                ),
            ),
            ("empty body", block(ALPHA_TIME - 1, vec![])),
        ];
        for (label, block) in cases {
            assert!(!BodyFacts::of(&chain_spec(), &block).is_pre_alpha_dkg_epoch_block, "{label}");
        }
    }

    #[test]
    fn only_system_txs_stops_at_the_first_user_transaction() {
        let system_only = block(ALPHA_TIME, vec![on_block_start(), finish_transition()]);
        assert!(BodyFacts::of(&chain_spec(), &system_only).only_system_txs);

        let with_user = block(ALPHA_TIME, vec![on_block_start(), user_txn(BLOCK_ADDR, vec![])]);
        assert!(!BodyFacts::of(&chain_spec(), &with_user).only_system_txs);
    }

    #[test]
    fn detects_new_epoch_event_from_reconfiguration_only() {
        let log = |address| Log {
            address,
            data: LogData::new_unchecked(vec![NewEpochEvent::SIGNATURE_HASH], Bytes::new()),
        };
        let receipt = |logs| Receipt { logs, ..Default::default() };

        assert!(changes_epoch(&[receipt(vec![]), receipt(vec![log(RECONFIGURATION_ADDR)])]));
        assert!(!changes_epoch(&[receipt(vec![log(BLOCK_ADDR)])]));
        assert!(!changes_epoch(&[]));
    }

    #[test]
    fn finds_beneficiary_index_in_active_validators() {
        let beneficiary = Address::repeat_byte(0xbb);
        let validators = vec![validator(Address::repeat_byte(0xaa), 0), validator(beneficiary, 1)];
        let result = call_output(getActiveValidatorsCall::abi_encode_returns(&validators));

        assert_eq!(active_validator_index(beneficiary, &result).unwrap(), 1);
        assert!(active_validator_index(Address::repeat_byte(0xcc), &result).is_err());
    }

    #[test]
    fn rejects_failed_active_validators_call() {
        let reverted: ExecutionResult<HaltReason> = ExecutionResult::Revert {
            gas: ResultGas::default(),
            logs: vec![],
            output: Bytes::new(),
        };
        assert!(active_validator_index(Address::repeat_byte(0xbb), &reverted).is_err());
    }

    #[test]
    fn builds_on_block_start_like_the_pipe() {
        let TransactionSigned::Legacy(signed) = on_block_start_txn(7, 50, 3, 1_234) else {
            panic!("expected a legacy transaction")
        };
        let (tx, signature) = (signed.tx(), signed.signature());
        assert_eq!((tx.chain_id, tx.nonce, tx.gas_price), (None, 7, 50));
        assert_eq!(
            (tx.gas_limit, tx.to, tx.value),
            (SYSTEM_TXN_GAS_LIMIT, TxKind::Call(BLOCK_ADDR), U256::ZERO)
        );
        assert_eq!((signature.r(), signature.s()), (U256::ZERO, U256::ZERO));
        let call = onBlockStartCall::abi_decode(&tx.input).unwrap();
        assert_eq!(
            (call.proposerIndex, call.failedProposerIndices, call.timestampMicros),
            (3, vec![], 1_234)
        );
    }
}
