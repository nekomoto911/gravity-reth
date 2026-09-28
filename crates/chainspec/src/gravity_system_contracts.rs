//! Gravity system contract addresses and ABI shared by the pipe execution layer and RPC
//! replay.
//!
//! Addresses follow `gravity_chain_core_contracts/src/foundation/SystemAddresses.sol`. Only the
//! items both layers need live here; the pipe keeps the rest in its `onchain_config` module.

use alloy_primitives::{address, Address};
use alloy_sol_types::sol;

/// `Timestamp`: the on-chain microsecond clock that `onBlockStart` advances.
pub const TIMESTAMP_ADDR: Address = address!("00000000000000000000000000000001625f1000");
/// `ValidatorManagement`: owns the active validator set.
pub const VALIDATOR_MANAGER_ADDR: Address = address!("00000000000000000000000000000001625f2001");
/// `Reconfiguration`: epoch transitions, including the DKG `finishTransition`.
pub const RECONFIGURATION_ADDR: Address = address!("00000000000000000000000000000001625f2003");
/// `Blocker`: target of the per-block `onBlockStart` metadata transaction.
pub const BLOCK_ADDR: Address = address!("00000000000000000000000000000001625f2004");

/// Proposer index of a NIL block, i.e. a block consensus produced without a proposer
/// (`NIL_PROPOSER_INDEX = type(uint64).max` in `Blocker.sol`).
pub const NIL_PROPOSER_INDEX: u64 = u64::MAX;

/// Gas limit of the protocol system transactions (metadata, DKG and JWK) the pipe injects.
pub const SYSTEM_TXN_GAS_LIMIT: u64 = 30_000_000;

sol! {
    /// Validator consensus info (`Types.sol`), returned by the `ValidatorManagement` getters.
    struct ValidatorConsensusInfo {
        /// Validator identity (stake pool) address.
        address validator;
        /// BLS public key for consensus.
        bytes consensusPubkey;
        /// Proof of possession for the BLS key.
        bytes consensusPop;
        /// Voting power derived from the bond, in wei.
        uint256 votingPower;
        /// Index in the active validator array.
        uint64 validatorIndex;
        /// Network addresses for P2P communication.
        bytes networkAddresses;
        /// Fullnode addresses for sync.
        bytes fullnodeAddresses;
    }

    /// `ValidatorManagement.getActiveValidators()`.
    function getActiveValidators() external view returns (ValidatorConsensusInfo[] memory);

    /// `ValidatorManagement.getPendingActiveValidators()`.
    function getPendingActiveValidators() external view returns (ValidatorConsensusInfo[] memory);

    /// `ValidatorManagement.getPendingInactiveValidators()`.
    function getPendingInactiveValidators() external view returns (ValidatorConsensusInfo[] memory);

    /// `Reconfiguration.NewEpochEvent`, emitted when an epoch transition completes.
    event NewEpochEvent(
        uint64 indexed newEpoch,
        ValidatorConsensusInfo[] validatorSet,
        uint256 totalVotingPower,
        uint64 transitionTime
    );

    /// `Blocker.onBlockStart`, the per-block metadata call: records proposer performance,
    /// advances the global time, and may start an epoch transition.
    function onBlockStart(
        uint64 proposerIndex,
        uint64[] calldata failedProposerIndices,
        uint64 timestampMicros
    );

    /// `Reconfiguration.finishTransition`, called with the DKG transcript to complete an
    /// epoch transition.
    function finishTransition(bytes calldata dkgResult) external;
}
