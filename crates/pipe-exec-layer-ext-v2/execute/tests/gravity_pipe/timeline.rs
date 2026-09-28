//! Hardfork schedule of the test chain, and the genesis it is injected into.
//!
//! Every hardfork is scheduled at a wall-clock offset from the test start. Blocks carry
//! the real time at which they are built, so a block activates a fork when
//! `parent_ts < fork_time <= block_ts`, never by block number.

use crate::node::{delegation_code, TestAccount, DELEGATE};
use alloy_primitives::{address, Address, B256, U256};
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardfork, GravityHardfork, Hardforks};
use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Mainnet genesis, byte-identical to `gravity-sdk/genesis/mainnet/genesis.json`.
const MAINNET_GENESIS: &str = include_str!("mainnet_genesis.json");

/// Offset of each hardfork from the test start, in chain order.
///
/// Each phase must see at least one epoch change. Scenario blocks never delay one (a pending
/// DKG transcript takes the next block that may change the epoch), so the latest a phase's
/// epoch change can land is one epoch interval plus about two blocks after the phase starts;
/// the rest of the phase is margin against slow blocks (a block with a DKG start and several
/// transactions takes up to ~7 s to execute and replay).
const FORK_OFFSETS: [(Fork, Duration); 4] = [
    (Fork::Prague, Duration::from_secs(40)),
    (Fork::Alpha, Duration::from_secs(80)),
    (Fork::Beta, Duration::from_secs(120)),
    (Fork::Gamma, Duration::from_secs(160)),
];

/// Mainnet reconfigures every 2 hours; the test shortens it so that epoch changes
/// happen throughout the timeline.
const EPOCH_INTERVAL: Duration = Duration::from_secs(10);

/// How long the last phase may run before the timeline is declared stuck.
const LAST_PHASE_BUDGET: Duration = Duration::from_secs(60);

/// `EpochConfig` packs `epochIntervalMicros` (low 8 bytes), the pending interval, the
/// pending flag, and the `_initialized` flag (byte 17) into slot 0.
const EPOCH_CONFIG_ADDR: Address = address!("00000000000000000000000000000001625f1005");
const EPOCH_CONFIG_INITIALIZED_BIT: usize = 136;
const MAINNET_EPOCH_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);

/// 10^6 ether per test account: a transaction's worst case (30M gas at 100 gwei) costs 3 ether.
const TEST_ACCOUNT_BALANCE_WEI: u128 = 1_000_000 * 10u128.pow(18);

/// Hardforks the test chain walks through, in activation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Fork {
    Prague,
    Alpha,
    Beta,
    Gamma,
}

impl Fork {
    pub(crate) const ALL: [Self; 4] = [Self::Prague, Self::Alpha, Self::Beta, Self::Gamma];

    fn genesis_key(self) -> &'static str {
        match self {
            Self::Prague => "pragueTime",
            Self::Alpha => "alphaTime",
            Self::Beta => "betaTime",
            Self::Gamma => "gammaTime",
        }
    }

    /// Whether the chain spec itself sees this block as the fork's activation block.
    pub(crate) fn transitions_at(
        self,
        chain_spec: &ChainSpec,
        block_ts: u64,
        parent_ts: u64,
    ) -> bool {
        let condition = match self {
            Self::Prague => chain_spec.fork(EthereumHardfork::Prague),
            Self::Alpha => chain_spec.gravity_hardforks().fork(GravityHardfork::Alpha),
            Self::Beta => chain_spec.gravity_hardforks().fork(GravityHardfork::Beta),
            Self::Gamma => chain_spec.gravity_hardforks().fork(GravityHardfork::Gamma),
        };
        condition.transitions_at_timestamp(block_ts, parent_ts)
    }
}

/// Where a block sits relative to the hardfork schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Phase {
    /// Before the first hardfork of the timeline.
    Genesis,
    /// The first block at or after `fork`'s time.
    Activation(Fork),
    /// After `fork`'s activation block, before the next fork.
    After(Fork),
}

impl Phase {
    /// Whether `fork` is active in a block of this phase.
    pub(crate) fn has_activated(self, fork: Fork) -> bool {
        match self {
            Self::Genesis => false,
            Self::Activation(latest) | Self::After(latest) => latest >= fork,
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Genesis => write!(f, "genesis"),
            Self::Activation(fork) => write!(f, "{fork:?} activation"),
            Self::After(fork) => write!(f, "after {fork:?}"),
        }
    }
}

/// Wall-clock hardfork schedule, fixed when the test starts.
#[derive(Debug)]
pub(crate) struct Timeline {
    /// `(fork, activation timestamp in seconds)`, in activation order.
    forks: Vec<(Fork, u64)>,
}

impl Timeline {
    pub(crate) fn starting_now() -> Self {
        let start = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let forks = FORK_OFFSETS
            .iter()
            .map(|(fork, offset)| (*fork, (start + *offset).as_secs()))
            .collect();
        Self { forks }
    }

    /// Mainnet genesis with the test's hardfork times, epoch interval and funded accounts, one
    /// of them delegated.
    pub(crate) fn genesis_json(&self) -> String {
        let mut genesis: serde_json::Value = serde_json::from_str(MAINNET_GENESIS).unwrap();

        for (fork, time) in &self.forks {
            genesis["config"][fork.genesis_key()] = serde_json::json!(time);
        }

        let slot = &mut genesis["alloc"][format!("{EPOCH_CONFIG_ADDR:#x}")]["storage"]["0x00"];
        let mainnet_value: U256 = slot.as_str().unwrap().parse().unwrap();
        assert_eq!(
            mainnet_value,
            epoch_config_slot(MAINNET_EPOCH_INTERVAL),
            "EpochConfig slot 0 no longer has the layout this test rewrites"
        );
        *slot = serde_json::json!(B256::from(epoch_config_slot(EPOCH_INTERVAL)));

        // Mainnet genesis funds no ordinary account, so user transactions need their own.
        for account in TestAccount::FUNDED {
            let entry = &mut genesis["alloc"][format!("{:#x}", account.address())];
            assert!(entry.is_null(), "{account:?} collides with a mainnet genesis account");
            *entry = serde_json::json!({ "balance": format!("{TEST_ACCOUNT_BALANCE_WEI:#x}") });
        }
        // EIP-7702 is locked down until Beta, so only genesis can delegate an account before it.
        let delegated = format!("{:#x}", TestAccount::Delegated.address());
        genesis["alloc"][delegated]["code"] = serde_json::json!(delegation_code(DELEGATE));

        genesis.to_string()
    }

    /// Classifies a block by its own and its parent's timestamp.
    ///
    /// Panics when one block would activate more than one fork: the schedule leaves
    /// each phase enough room, so that only happens if block production stalled.
    pub(crate) fn phase(&self, block_ts: u64, parent_ts: u64) -> Phase {
        let activated: Vec<_> = self
            .forks
            .iter()
            .filter(|(_, time)| parent_ts < *time && *time <= block_ts)
            .map(|(fork, _)| *fork)
            .collect();
        match activated.as_slice() {
            [] => {}
            [fork] => return Phase::Activation(*fork),
            forks => panic!(
                "block at {block_ts} (parent {parent_ts}) would activate {forks:?} at once; \
                 block production is too slow for the schedule"
            ),
        }
        self.forks
            .iter()
            .rev()
            .find(|(_, time)| *time <= block_ts)
            .map_or(Phase::Genesis, |(fork, _)| Phase::After(*fork))
    }

    /// Wall-clock second after which the timeline must have finished.
    pub(crate) fn deadline(&self) -> u64 {
        self.forks.last().unwrap().1 + LAST_PHASE_BUDGET.as_secs()
    }
}

fn epoch_config_slot(interval: Duration) -> U256 {
    (U256::from(1) << EPOCH_CONFIG_INITIALIZED_BIT) | U256::from(interval.as_micros())
}
