//! `debug_intermediateRoots`: the state root after each transaction ends at the committed
//! state root.

use super::{committed::Committed, result_or_record};
use crate::report::BlockReport;
use alloy_primitives::B256;

pub(super) fn check_intermediate_roots(
    report: &mut BlockReport<'_>,
    endpoint: &'static str,
    committed: &Committed,
    response: Result<Vec<B256>, String>,
) {
    let Some(roots) = result_or_record(report, endpoint, None, response) else { return };
    report.check_eq(endpoint, None, "root count", committed.tx_hashes.len(), roots.len());
    // The endpoint only computes a root after each transaction, so on a block the chain writes
    // to after its last transaction the last root can never be the header's.
    if !committed.writes_after_transactions.is_empty() {
        return;
    }
    if let Some(last) = roots.last() {
        let index = roots.len() - 1;
        report.check_eq(endpoint, Some(index), "state root", committed.state_root, *last);
    }
}
