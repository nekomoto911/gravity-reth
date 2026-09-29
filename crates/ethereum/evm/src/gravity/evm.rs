//! Per-transaction Gravity execution rules.

use super::{delegated_create::gravity_instructions, randomness::HeaderRandomnessProvider};
use crate::{is_gravity_system_caller, is_system_tx_gas_exempt, GravityChainReader};
use alloc::{string::String, sync::Arc};
use alloy_evm::{
    eth::{EthEvmBuilder, EthEvmContext},
    precompiles::{DynPrecompile, PrecompilesMap},
    Database, EthEvm, Evm, EvmEnv, EvmFactory,
};
use alloy_primitives::{Address, Bytes};
use gravity_precompiles::{
    bls_pop_verify::{create_bls_pop_verify_precompile, BLS_PRECOMPILE_ADDR},
    mint::{create_mint_token_precompile, NATIVE_MINT_PRECOMPILE_ADDR},
    randomness_by_height::{
        create_randomness_by_height_precompile, randomness_by_height_gas_policy_at_block,
        RANDOMNESS_BY_HEIGHT_PRECOMPILE_ADDR,
    },
};
use reth_chainspec::ChainSpec;
use revm::{
    context::{BlockEnv, CfgEnv, ContextTr, DBErrorMarker, JournalTr, TxEnv},
    context_interface::result::{EVMError, HaltReason, ResultAndState},
    handler::PrecompileProvider,
    inspector::NoOpInspector,
    primitives::hardfork::SpecId,
    Inspector,
};

/// Creates [`GravityEvm`]s.
#[derive(Debug, Clone)]
pub struct GravityEvmFactory {
    chain_spec: Arc<ChainSpec>,
    reader: Arc<dyn GravityChainReader>,
}

impl GravityEvmFactory {
    /// Creates a factory for `chain_spec` that reads canonical headers through `reader`.
    pub fn new(chain_spec: Arc<ChainSpec>, reader: Arc<dyn GravityChainReader>) -> Self {
        Self { chain_spec, reader }
    }

    fn create<DB: Database, I: Inspector<EthEvmContext<DB>>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
        inspect: bool,
    ) -> GravityEvm<DB, I> {
        let spec = input.cfg_env.spec;
        let block_number: u64 = input.block_env.number.saturating_to();
        let gas_exempt = is_system_tx_gas_exempt(
            self.chain_spec.as_ref(),
            input.block_env.timestamp.saturating_to(),
        );
        // Randomness-by-height is an Alpha precompile, like the system transactions' gas
        // exemption.
        let randomness = gas_exempt.then(|| {
            create_randomness_by_height_precompile(Arc::new(HeaderRandomnessProvider::new(
                self.reader.clone(),
                block_number,
                input.block_env.prevrandao,
                randomness_by_height_gas_policy_at_block(self.chain_spec.as_ref(), block_number),
            )))
        });

        let mut evm = EthEvmBuilder::new(db, input)
            .inspector(inspector)
            .set_inspect(inspect)
            .build()
            .into_inner();
        if spec.is_enabled_in(SpecId::PRAGUE) {
            evm.instruction = gravity_instructions(spec);
        }
        let mut evm = EthEvm::new(evm, inspect);

        // An EVM starts with the user precompile table; see `GravityEvm::enter_phase`.
        let precompiles = evm.precompiles_mut();
        let bls = create_bls_pop_verify_precompile();
        precompiles.apply_precompile(&BLS_PRECOMPILE_ADDR, move |_| Some(bls));
        if let Some(randomness) = randomness.clone() {
            precompiles
                .apply_precompile(&RANDOMNESS_BY_HEIGHT_PRECOMPILE_ADDR, move |_| Some(randomness));
        }

        GravityEvm { inner: evm, phase: Phase::Fresh, gas_exempt, randomness }
    }
}

impl EvmFactory for GravityEvmFactory {
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>>> = GravityEvm<DB, I>;
    type Context<DB: Database> = EthEvmContext<DB>;
    type Tx = TxEnv;
    type Error<DBError: DBErrorMarker> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
        self.create(db, input, NoOpInspector {}, false)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        self.create(db, input, inspector, true)
    }
}

/// An [`EthEvm`] that applies Gravity's per-transaction rules, keyed on the transaction's caller.
///
/// - **Precompiles.** Besides the standard ones, a transaction from `SYSTEM_CALLER` gets mint and
///   BLS; any other caller gets BLS and, from Alpha, randomness-by-height. This matches what the
///   pipe installs for system and user transactions.
/// - **Gas exemption.** From Alpha, a `SYSTEM_CALLER` transaction runs with the base-fee and
///   balance checks disabled, as the pipe's `transact_system_txn` does; the previous settings are
///   restored afterwards.
/// - **Delegated CREATE.** From Prague, CREATE/CREATE2 in an EIP-7702 delegated context halts, as
///   grevm's delegated-safety policy does for the pipe's user transactions.
///
/// Gravity blocks put every system transaction before the first user transaction, so one EVM goes
/// through at most two phases: system, then user.
///
/// RPC requests are keyed the same way: a request from `SYSTEM_CALLER` runs under the system
/// rules. At a position no system transaction can take on chain (after a user transaction) the
/// result is only a hypothetical; a `SYSTEM_CALLER` request after a user one in the same EVM
/// (`eth_simulateV1`) fails.
#[expect(missing_debug_implementations)]
pub struct GravityEvm<DB: Database, I> {
    inner: EthEvm<DB, I, PrecompilesMap>,
    phase: Phase,
    gas_exempt: bool,
    randomness: Option<DynPrecompile>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// No transaction executed yet; the table holds the user precompiles.
    Fresh,
    System,
    User,
}

impl<DB: Database, I: Inspector<EthEvmContext<DB>>> GravityEvm<DB, I> {
    /// Switches the precompile table to the phase of a transaction from `caller`.
    ///
    /// Only the two differing addresses are added or removed, so changes made to the table after
    /// creation (e.g. `eth_simulateV1` precompile moves) are kept.
    fn enter_phase(&mut self, caller: Address) -> Result<(), EVMError<DB::Error>> {
        let is_system = is_gravity_system_caller(caller);
        match (self.phase, is_system) {
            (Phase::Fresh, true) => {
                // Nothing has executed, so the journal has not warmed any precompile yet; revm
                // warms the system table at the start of this transaction.
                let precompiles = self.inner.precompiles_mut();
                let mint = create_mint_token_precompile();
                precompiles.apply_precompile(&NATIVE_MINT_PRECOMPILE_ADDR, move |_| Some(mint));
                precompiles.apply_precompile(&RANDOMNESS_BY_HEIGHT_PRECOMPILE_ADDR, |_| None);
                self.phase = Phase::System;
            }
            (Phase::System, false) => {
                let precompiles = self.inner.precompiles_mut();
                precompiles.apply_precompile(&NATIVE_MINT_PRECOMPILE_ADDR, |_| None);
                if let Some(randomness) = self.randomness.clone() {
                    precompiles
                        .apply_precompile(&RANDOMNESS_BY_HEIGHT_PRECOMPILE_ADDR, move |_| {
                            Some(randomness)
                        });
                }
                // revm warms precompile addresses only for the first transaction of an EVM and
                // keeps them across transactions. Without re-warming, user transactions would
                // see mint warm and randomness-by-height cold.
                let addresses =
                    <PrecompilesMap as PrecompileProvider<EthEvmContext<DB>>>::warm_addresses(
                        self.inner.precompiles(),
                    )
                    .clone();
                self.inner.ctx_mut().journal_mut().warm_precompiles(&addresses);
                self.phase = Phase::User;
            }
            (Phase::Fresh, false) => self.phase = Phase::User,
            (Phase::User, true) => {
                return Err(EVMError::Custom(String::from(
                    "a SYSTEM_CALLER transaction follows a user transaction; Gravity blocks put \
                     system transactions first",
                )))
            }
            (Phase::System, true) | (Phase::User, false) => {}
        }
        Ok(())
    }
}

impl<DB, I> Evm for GravityEvm<DB, I>
where
    DB: Database,
    I: Inspector<EthEvmContext<DB>>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        self.inner.block()
    }

    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        self.inner.cfg_env()
    }

    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.enter_phase(tx.caller)?;
        if !(self.gas_exempt && is_gravity_system_caller(tx.caller)) {
            return self.inner.transact_raw(tx)
        }

        // `disable_nonce_check` stays as it is: SYSTEM_CALLER's nonce sequence is part of the
        // protocol.
        let cfg = &mut self.inner.ctx_mut().cfg;
        let saved = (cfg.disable_base_fee, cfg.disable_balance_check);
        cfg.disable_base_fee = true;
        cfg.disable_balance_check = true;
        let result = self.inner.transact_raw(tx);
        let cfg = &mut self.inner.ctx_mut().cfg;
        (cfg.disable_base_fee, cfg.disable_balance_check) = saved;
        result
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.transact_system_call(caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        self.inner.finish()
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inner.set_inspector_enabled(enabled)
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        self.inner.components()
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        self.inner.components_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        gravity::test_utils::{gravity_evm_factory, ALPHA_TIME},
        SYSTEM_CALLER,
    };
    use alloc::vec;
    use alloy_primitives::{address, U256};
    use revm::{
        context::result::{EVMError, InvalidTransaction},
        database::{CacheDB, EmptyDB},
        primitives::TxKind,
        state::AccountInfo,
    };

    const USER: Address = address!("00000000000000000000000000000000000a11ce");

    fn env(timestamp: u64, basefee: u64) -> EvmEnv {
        EvmEnv {
            cfg_env: CfgEnv::new_with_spec(SpecId::PRAGUE).with_chain_id(1),
            block_env: BlockEnv {
                number: U256::from(10),
                timestamp: U256::from(timestamp),
                basefee,
                gas_limit: 30_000_000,
                ..Default::default()
            },
        }
    }

    fn db() -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        let rich = AccountInfo { balance: U256::from(10u128.pow(24)), ..Default::default() };
        db.insert_account_info(USER, rich);
        db
    }

    fn tx(caller: Address, nonce: u64, to: Address, gas_price: u128) -> TxEnv {
        TxEnv {
            caller,
            nonce,
            kind: TxKind::Call(to),
            gas_limit: 100_000,
            gas_price,
            data: Bytes::from(vec![0u8; 32]),
            chain_id: Some(1),
            ..Default::default()
        }
    }

    fn has(evm: &GravityEvm<CacheDB<EmptyDB>, NoOpInspector>, address: Address) -> bool {
        evm.precompiles().get(&address).is_some()
    }

    #[test]
    fn precompile_table_follows_the_transaction_phase() {
        let mut evm = gravity_evm_factory().create_evm(db(), env(ALPHA_TIME, 0));
        assert!(has(&evm, BLS_PRECOMPILE_ADDR));
        assert!(has(&evm, RANDOMNESS_BY_HEIGHT_PRECOMPILE_ADDR));
        assert!(!has(&evm, NATIVE_MINT_PRECOMPILE_ADDR));

        evm.transact(tx(SYSTEM_CALLER, 0, USER, 0)).unwrap();
        assert!(has(&evm, BLS_PRECOMPILE_ADDR));
        assert!(!has(&evm, RANDOMNESS_BY_HEIGHT_PRECOMPILE_ADDR));
        assert!(has(&evm, NATIVE_MINT_PRECOMPILE_ADDR));

        evm.transact(tx(USER, 0, USER, 0)).unwrap();
        assert!(has(&evm, BLS_PRECOMPILE_ADDR));
        assert!(has(&evm, RANDOMNESS_BY_HEIGHT_PRECOMPILE_ADDR));
        assert!(!has(&evm, NATIVE_MINT_PRECOMPILE_ADDR));
    }

    #[test]
    fn pre_alpha_user_table_has_no_randomness() {
        let evm = gravity_evm_factory().create_evm(db(), env(ALPHA_TIME - 1, 0));
        assert!(has(&evm, BLS_PRECOMPILE_ADDR));
        assert!(!has(&evm, RANDOMNESS_BY_HEIGHT_PRECOMPILE_ADDR));
    }

    /// A user transaction after the system phase must see the randomness precompile warm, as in
    /// an EVM that only ran user transactions: a cold access would cost 2600 instead of 100.
    #[test]
    fn user_phase_rewarms_the_user_precompiles() {
        let call_randomness = tx(USER, 0, RANDOMNESS_BY_HEIGHT_PRECOMPILE_ADDR, 0);

        let mut user_only = gravity_evm_factory().create_evm(db(), env(ALPHA_TIME, 0));
        let expected = user_only.transact(call_randomness.clone()).unwrap().result.tx_gas_used();

        let mut after_system = gravity_evm_factory().create_evm(db(), env(ALPHA_TIME, 0));
        after_system.transact_commit(tx(SYSTEM_CALLER, 0, USER, 0)).unwrap();
        let gas = after_system.transact(call_randomness).unwrap().result.tx_gas_used();

        assert_eq!(gas, expected);
        let warm = after_system.inner.ctx().journaled_state.precompile_addresses();
        assert!(warm.contains(&RANDOMNESS_BY_HEIGHT_PRECOMPILE_ADDR));
        assert!(!warm.contains(&NATIVE_MINT_PRECOMPILE_ADDR));
    }

    #[test]
    fn system_caller_is_gas_exempt_from_alpha_and_settings_are_restored() {
        // SYSTEM_CALLER has no balance and pays gas price 0 under a non-zero base fee.
        let system_tx = tx(SYSTEM_CALLER, 0, USER, 0);

        let mut evm = gravity_evm_factory().create_evm(db(), env(ALPHA_TIME, 7));
        assert!(evm.transact(system_tx.clone()).unwrap().result.is_success());
        assert!(!evm.cfg_env().disable_base_fee);
        assert!(!evm.cfg_env().disable_balance_check);

        let mut evm = gravity_evm_factory().create_evm(db(), env(ALPHA_TIME - 1, 7));
        assert!(matches!(
            evm.transact(system_tx),
            Err(EVMError::Transaction(InvalidTransaction::GasPriceLessThanBasefee))
        ));
    }

    #[test]
    fn gas_exemption_keeps_settings_the_caller_disabled() {
        let mut env = env(ALPHA_TIME, 7);
        env.cfg_env.disable_base_fee = true;
        let mut evm = gravity_evm_factory().create_evm(db(), env);
        evm.transact(tx(SYSTEM_CALLER, 0, USER, 0)).unwrap();
        assert!(evm.cfg_env().disable_base_fee);
        assert!(!evm.cfg_env().disable_balance_check);
    }

    #[test]
    fn user_transactions_do_not_get_the_gas_exemption() {
        let mut evm = gravity_evm_factory().create_evm(db(), env(ALPHA_TIME, 7));
        assert!(matches!(
            evm.transact(tx(USER, 0, USER, 0)),
            Err(EVMError::Transaction(InvalidTransaction::GasPriceLessThanBasefee))
        ));
    }

    #[test]
    fn rejects_a_system_transaction_after_a_user_transaction() {
        let mut evm = gravity_evm_factory().create_evm(db(), env(ALPHA_TIME, 0));
        evm.transact_commit(tx(USER, 0, USER, 0)).unwrap();
        assert!(matches!(evm.transact(tx(SYSTEM_CALLER, 0, USER, 0)), Err(EVMError::Custom(_))));
    }
}
