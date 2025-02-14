use alloy_primitives::Address;
use reth_grevm::ParallelState;
use revm::db::{
    states::{bundle_state::BundleRetention, CacheAccount},
    BundleState,
};

pub trait State {
    fn bundle_size_hint(&self) -> usize;

    fn take_bundle(&mut self) -> BundleState;

    fn merge_transitions(&mut self, retention: BundleRetention);

    fn for_each_accounts(&self, f: &mut dyn FnMut((&Address, &CacheAccount)));
}

impl<DB> State for revm::db::states::State<DB>
where
    DB: crate::Database,
{
    fn bundle_size_hint(&self) -> usize {
        self.bundle_size_hint()
    }

    fn take_bundle(&mut self) -> BundleState {
        self.take_bundle()
    }

    fn merge_transitions(&mut self, retention: BundleRetention) {
        self.merge_transitions(retention);
    }

    fn for_each_accounts(&self, f: &mut dyn FnMut((&Address, &CacheAccount))) {
        self.cache.accounts.iter().for_each(f);
    }
}

impl<DB> State for ParallelState<DB>
where
    DB: crate::ParallelDatabase,
{
    fn bundle_size_hint(&self) -> usize {
        self.bundle_size_hint()
    }

    fn take_bundle(&mut self) -> BundleState {
        self.take_bundle()
    }

    fn merge_transitions(&mut self, retention: BundleRetention) {
        self.merge_transitions(retention);
    }

    fn for_each_accounts(&self, f: &mut dyn FnMut((&Address, &CacheAccount))) {
        for entry in self.cache.accounts.iter() {
            f((entry.key(), entry.value()));
        }
    }
}
