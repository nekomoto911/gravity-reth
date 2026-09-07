//! `TestnetOwnerFix` hardfork — forced `Ownable2Step` `transferOwnership` migration.
//!
//! Longevity Testnet (`chain_id == 7771625`) genesis `StakePool`s used Aptos-era
//! identity material as `owner`. Those addresses look like EOAs but have no
//! recoverable secp256k1 private key, so `onlyOwner` admin paths are stuck.
//!
//! On the unique Longevity block that crosses `testnetOwnerFixTime`
//! (`transitions_at_timestamp`, same one-shot gate as Alpha / EIP-2935), the
//! pipe layer injects four synthetic top-level txs (one per genesis pool) with
//! `from = old_owner`, calling `transferOwnership(new_owner)`. Gas reuses the
//! existing Alpha system-tx levers (`gas_price = 0` + `transact_system_txn`
//! basefee/balance disable). The txs are written into the block body with
//! `TransactionSenders = old_owner`.
//!
//! This module owns the hardcoded migration table and calldata encoding.
//! There is no slot precheck: any forced-tx revert panics at execution time.
//! It intentionally contains **no** RPC debug/trace sender or basefee
//! special-casing.

use alloy_primitives::{address, Address, Bytes};
use alloy_sol_types::{sol, SolCall};

sol! {
    /// `Ownable2Step.transferOwnership(address newOwner)`
    function transferOwnership(address newOwner);
}

/// One genesis `StakePool` ownership migration row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationRow {
    /// Human label (node1 / node2 / node3 / node5).
    pub label: &'static str,
    /// `StakePool` contract address.
    pub stake_pool: Address,
    /// Current (unrecoverable) owner EOA.
    pub old_owner: Address,
    /// Ceremony-generated replacement EOA (address only; no private key here).
    pub new_owner: Address,
}

/// Hardcoded Longevity Testnet migration table (node1 → node2 → node3 → node5).
///
/// `new_owner` MUST be `cast wallet address --private-key <ceremony PK>` for
/// each node. Private keys never enter this binary.
pub const MIGRATION_TABLE: [MigrationRow; 4] = [
    MigrationRow {
        label: "node1",
        stake_pool: address!("743d93845745e01a23f9afbb990bbc7c87aae6c8"),
        old_owner: address!("CE128222Bd84D67672f863424a03D114CD1253C5"),
        new_owner: address!("91a59bae639a3cef0c41e4c61268aa54c71de1ba"),
    },
    MigrationRow {
        label: "node2",
        stake_pool: address!("419ad62f796a0f3971bd1212f208942c3c435b99"),
        old_owner: address!("78F595Fb25D03a742338Fb32AcfD544BdC63D814"),
        new_owner: address!("6a0da8def2ccd134119c0293ad470d1aa1d6129a"),
    },
    MigrationRow {
        label: "node3",
        stake_pool: address!("93e5acbcdd50767f7fd19ab4a2efc259d9a8bdd1"),
        old_owner: address!("891299fE364088ead65ABa911ea17DD5d968Cd81"),
        new_owner: address!("5a1ba49d261e1e58dd1b8cf0aeeb1976d04ac6bd"),
    },
    MigrationRow {
        label: "node5",
        stake_pool: address!("298136ce84d442d2c0c594f5734a20afc60de244"),
        old_owner: address!("B99AA922Eb5CaE399b79ADC87621E72f66d5A976"),
        new_owner: address!("2326795e2033d209ea12b1022c50ae592ac2b720"),
    },
];

/// ABI-encode `transferOwnership(new_owner)`.
#[inline]
pub fn transfer_ownership_calldata(new_owner: Address) -> Bytes {
    Bytes::from(transferOwnershipCall { newOwner: new_owner }.abi_encode())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_ownership_selector_matches_oz() {
        let data = transfer_ownership_calldata(MIGRATION_TABLE[0].new_owner);
        assert_eq!(&data[..4], &[0xf2, 0xfd, 0xe3, 0x8b]);
        assert_eq!(data.len(), 4 + 32);
    }
}
