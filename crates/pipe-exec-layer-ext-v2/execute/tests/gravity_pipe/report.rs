//! Discrepancies between what the pipe committed and what the node reports afterwards,
//! collected over the whole run.
//!
//! A discrepancy must not stop the timeline: every later block and scenario still runs,
//! so one run lists all of them. The test fails once, at the end, if any was recorded.

use crate::timeline::Phase;
use std::fmt::{self, Debug, Display};

/// Every mismatch recorded during the run, in the order they were found.
#[derive(Debug, Default)]
pub(crate) struct MismatchReport {
    mismatches: Vec<Mismatch>,
}

impl MismatchReport {
    /// Scopes recording to one committed block.
    pub(crate) const fn for_block(&mut self, number: u64, phase: Phase) -> BlockReport<'_> {
        BlockReport { report: self, scope: Scope::Block { number, phase } }
    }

    /// Scopes recording to a range of committed blocks, for endpoints that replay several
    /// blocks in one call.
    pub(crate) const fn for_blocks(&mut self, first: u64, last: u64) -> BlockReport<'_> {
        BlockReport { report: self, scope: Scope::Blocks { first, last } }
    }

    pub(crate) const fn len(&self) -> usize {
        self.mismatches.len()
    }

    /// Fails the test with every recorded mismatch.
    pub(crate) fn assert_empty(&self) {
        if self.mismatches.is_empty() {
            return;
        }
        let entries: Vec<String> = self.mismatches.iter().map(Mismatch::to_string).collect();
        panic!("{} mismatches:\n{}", entries.len(), entries.join("\n"));
    }
}

/// One value the node reported that differs from the committed result.
#[derive(Debug)]
pub(crate) struct Mismatch {
    pub(crate) scope: Scope,
    /// RPC endpoint or scenario that produced the value.
    pub(crate) source: &'static str,
    pub(crate) tx_index: Option<usize>,
    pub(crate) field: String,
    pub(crate) expected: String,
    pub(crate) actual: String,
}

impl Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { scope, source, tx_index, field, expected, actual } = self;
        match scope {
            Scope::Block { number, phase } => write!(f, "block {number} ({phase}) {source}")?,
            Scope::Blocks { first, last } => write!(f, "blocks {first}..={last} {source}")?,
        }
        if let Some(index) = tx_index {
            write!(f, " tx {index}")?;
        }
        write!(f, " {field}: expected {expected}, actual {actual}")
    }
}

/// The committed blocks a mismatch is about.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Scope {
    Block { number: u64, phase: Phase },
    Blocks { first: u64, last: u64 },
}

/// Records mismatches of one committed block, or of a range of them.
pub(crate) struct BlockReport<'a> {
    report: &'a mut MismatchReport,
    scope: Scope,
}

impl BlockReport<'_> {
    pub(crate) fn record(
        &mut self,
        source: &'static str,
        tx_index: Option<usize>,
        field: impl Into<String>,
        expected: impl Display,
        actual: impl Display,
    ) {
        self.report.mismatches.push(Mismatch {
            scope: self.scope,
            source,
            tx_index,
            field: field.into(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }

    /// Records a mismatch unless `actual` equals `expected`.
    pub(crate) fn check_eq<T: PartialEq + Debug>(
        &mut self,
        source: &'static str,
        tx_index: Option<usize>,
        field: impl Into<String>,
        expected: T,
        actual: T,
    ) {
        if expected != actual {
            self.record(source, tx_index, field, format!("{expected:?}"), format!("{actual:?}"));
        }
    }
}
