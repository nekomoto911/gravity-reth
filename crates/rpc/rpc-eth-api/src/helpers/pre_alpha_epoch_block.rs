//! Replay support for pre-Alpha DKG epoch-change blocks.
//!
//! Before the Alpha hardfork, when a block's DKG `finishTransition` emitted `NewEpoch`, the pipe
//! executed `Blocker.onBlockStart` and then `finishTransition`, but assembled only
//! `finishTransition` into the body. The committed state covers both calls, so replaying the body
//! alone starts one `SYSTEM_CALLER` nonce short and fails with `nonce too high`. These blocks are
//! published and cannot change, so RPC replay re-executes the omitted `onBlockStart`, rebuilt from
//! chain state (see `Call::apply_pre_execution_changes`).

use alloy_consensus::{BlockHeader, Signed, Transaction, TxEnvelope, TxLegacy};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use reth_chainspec::{
    gravity_system_contracts::{
        finishTransitionCall, getActiveValidatorsCall, onBlockStartCall, BLOCK_ADDR,
        RECONFIGURATION_ADDR, SYSTEM_TXN_GAS_LIMIT, TIMESTAMP_ADDR,
    },
    is_gravity_system_caller, EthChainSpec, GravityHardfork,
};
use reth_errors::RethError;
use reth_primitives_traits::{Block, RecoveredBlock};
use reth_rpc_eth_types::EthApiError;
use reth_storage_api::StateProvider;
use revm::context::result::ExecutionResult;

/// Returns whether `block` is a pre-Alpha DKG epoch-change block, whose body omits the
/// `onBlockStart` the pipe executed before its `finishTransition`.
///
/// Every other pre-Alpha body starts with `onBlockStart`, including an epoch change that
/// `onBlockStart` itself triggers, so a body that starts with a system `finishTransition`
/// identifies exactly these blocks without reading state.
pub fn is_pre_alpha_dkg_epoch_block<ChainSpec, B>(
    chain_spec: &ChainSpec,
    block: &RecoveredBlock<B>,
) -> bool
where
    ChainSpec: EthChainSpec,
    B: Block,
{
    if chain_spec
        .gravity_hardforks()
        .is_fork_active_at_timestamp(GravityHardfork::Alpha, block.header().timestamp())
    {
        return false
    }

    block.transactions_recovered().next().is_some_and(|tx| {
        is_gravity_system_caller(tx.signer()) &&
            tx.to() == Some(RECONFIGURATION_ADDR) &&
            tx.input().starts_with(&finishTransitionCall::SELECTOR)
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
) -> Result<u64, EthApiError> {
    let ExecutionResult::Success { output, .. } = active_validators else {
        return Err(EthApiError::Internal(RethError::msg(format!(
            "getActiveValidators() failed: {active_validators:?}"
        ))))
    };
    getActiveValidatorsCall::abi_decode_returns(output.data())
        .map_err(|err| {
            EthApiError::Internal(RethError::msg(format!(
                "invalid getActiveValidators() output: {err}"
            )))
        })?
        .into_iter()
        .find(|info| info.validator == validator)
        .map(|info| info.validatorIndex)
        .ok_or_else(|| {
            EthApiError::Internal(RethError::msg(format!(
                "block beneficiary {validator} is not an active validator"
            )))
        })
}

/// Returns the global time `onBlockStart` wrote, read from the state after the block.
///
/// `Timestamp.microseconds` is that contract's only state variable (slot 0), and nothing after
/// `onBlockStart` in an epoch-change block writes it.
pub(crate) fn written_timestamp_micros(
    post_block_state: &dyn StateProvider,
) -> Result<u64, EthApiError> {
    let value = post_block_state.storage(TIMESTAMP_ADDR, B256::ZERO)?.unwrap_or_default();
    u64::try_from(value).map_err(|_| {
        EthApiError::Internal(RethError::msg(format!("Timestamp slot 0 holds non-u64 {value}")))
    })
}

/// Builds the omitted `onBlockStart` exactly as the pipe did: an unsigned legacy transaction
/// that pays the block base fee, with an empty failed-proposer list.
///
/// The failed-proposer list is not recorded on chain. Every entry adds gas, and the empty list
/// reproduces the committed state of the affected mainnet and testnet blocks.
///
/// RPC helpers are generic over the node's transaction type, so the transaction is encoded as an
/// Ethereum legacy envelope and decoded into `T`.
pub(crate) fn on_block_start_txn<T: Decodable2718>(
    nonce: u64,
    base_fee: u64,
    proposer_index: u64,
    timestamp_micros: u64,
) -> Result<T, EthApiError> {
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
    let encoded = TxEnvelope::Legacy(Signed::new_unhashed(tx, unsigned)).encoded_2718();
    T::decode_2718_exact(&encoded).map_err(|err| EthApiError::Internal(RethError::other(err)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Block as AlloyBlock, BlockBody, Header};
    use alloy_primitives::Bytes;
    use reth_chainspec::{
        gravity_system_contracts::ValidatorConsensusInfo, ChainHardforks, ChainSpec, ForkCondition,
        Hardfork, SYSTEM_CALLER,
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

    fn system_txn(nonce: u64, to: Address, input: Vec<u8>) -> TxEnvelope {
        let tx = TxLegacy {
            nonce,
            gas_limit: SYSTEM_TXN_GAS_LIMIT,
            to: TxKind::Call(to),
            input: input.into(),
            ..Default::default()
        };
        TxEnvelope::Legacy(Signed::new_unhashed(tx, Signature::new(U256::ZERO, U256::ZERO, false)))
    }

    fn on_block_start() -> TxEnvelope {
        let call = onBlockStartCall {
            proposerIndex: 0,
            failedProposerIndices: vec![],
            timestampMicros: 1,
        };
        system_txn(0, BLOCK_ADDR, call.abi_encode())
    }

    fn finish_transition() -> TxEnvelope {
        let call = finishTransitionCall { dkgResult: Bytes::from_static(&[0xab]) };
        system_txn(1, RECONFIGURATION_ADDR, call.abi_encode())
    }

    fn block(
        timestamp: u64,
        txs: Vec<(Address, TxEnvelope)>,
    ) -> RecoveredBlock<AlloyBlock<TxEnvelope>> {
        let (senders, transactions) = txs.into_iter().unzip();
        let block = AlloyBlock {
            header: Header { timestamp, ..Default::default() },
            body: BlockBody { transactions, ..Default::default() },
        };
        RecoveredBlock::new_unhashed(block, senders)
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
        let block = block(ALPHA_TIME - 1, vec![(SYSTEM_CALLER, finish_transition())]);
        assert!(is_pre_alpha_dkg_epoch_block(&chain_spec(), &block));
    }

    #[test]
    fn ignores_every_other_block_shape() {
        let cases = [
            ("post-Alpha", block(ALPHA_TIME, vec![(SYSTEM_CALLER, finish_transition())])),
            (
                "body starting with onBlockStart",
                block(
                    ALPHA_TIME - 1,
                    vec![(SYSTEM_CALLER, on_block_start()), (SYSTEM_CALLER, finish_transition())],
                ),
            ),
            ("non-system sender", block(ALPHA_TIME - 1, vec![(BLOCK_ADDR, finish_transition())])),
            ("empty body", block(ALPHA_TIME - 1, vec![])),
        ];
        for (label, block) in cases {
            assert!(!is_pre_alpha_dkg_epoch_block(&chain_spec(), &block), "{label}");
        }
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
        let tx: TxEnvelope = on_block_start_txn(7, 50, 3, 1_234).unwrap();

        let TxEnvelope::Legacy(signed) = tx else { panic!("expected a legacy transaction") };
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
