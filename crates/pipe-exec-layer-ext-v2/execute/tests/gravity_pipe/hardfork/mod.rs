//! Scenarios, one module per hardfork. Each module decides what goes into the blocks
//! of its phases (before activation, the activation block, after activation) and what
//! to assert once they are committed.
//!
//! The timeline asks for the content of every block before building it, and shows every
//! committed block to every module afterwards. A module plans one block at a time and keeps
//! what that block must show until it is committed; a phase can take several blocks that
//! way. Scenario assertions record into the mismatch report, so the timeline goes on.

mod alpha;
mod base;
mod beta;
mod chain;
mod gamma;
mod prague;

pub(crate) use chain::Chain;

use crate::{
    node::{BlockInput, CommittedBlock, SignedTx},
    report::BlockReport,
    timeline::Phase,
};
use gravity_api_types::ExtraDataType;

/// What a scenario puts into the next block besides the protocol system transactions.
#[derive(Debug, Default)]
pub(crate) struct ScenarioBlock {
    pub(crate) transactions: Vec<SignedTx>,
    pub(crate) extra_data: Vec<ExtraDataType>,
}

impl ScenarioBlock {
    /// The block's content; the caller still sets its timestamp and whether it may change
    /// the epoch.
    pub(crate) fn into_input(self) -> BlockInput {
        let (transactions, senders) =
            self.transactions.into_iter().map(|signed| (signed.tx, signed.sender)).unzip();
        BlockInput { transactions, senders, extra_data: self.extra_data, ..Default::default() }
    }
}

/// The scenarios of every module.
#[derive(Debug, Default)]
pub(crate) struct Scenarios {
    prague: prague::Prague,
    base: base::Base,
}

impl Scenarios {
    /// What the block after `parent` carries for the scenarios, or `None` if no scenario
    /// needs it.
    pub(crate) fn next_block(
        &mut self,
        chain: &Chain<'_>,
        phase: Phase,
        parent: u64,
    ) -> Option<ScenarioBlock> {
        // Hardfork modules come first: their phases are short, while base scenarios can take
        // any block before Alpha.
        self.prague.next_block(phase, parent).or_else(|| self.base.next_block(chain, phase, parent))
    }

    /// Asserts what `block` must show, now that it is committed.
    pub(crate) fn after_commit(
        &mut self,
        chain: &Chain<'_>,
        block: &CommittedBlock,
        report: &mut BlockReport<'_>,
    ) {
        self.prague.after_commit(chain, block, report);
        self.base.after_commit(chain, block, report);
    }

    /// Panics if a scenario never got its block: the phase it needs ended first.
    pub(crate) fn assert_all_ran(&self) {
        self.prague.assert_all_ran();
        self.base.assert_all_ran();
    }
}
