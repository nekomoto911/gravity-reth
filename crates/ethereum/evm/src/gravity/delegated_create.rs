//! CREATE/CREATE2 guard for EIP-7702 delegated execution contexts.
//!
//! Copied from grevm `ee9d4e2` `src/delegated_safety/instructions.rs:14-46`, which the pipe
//! enables for every user transaction of a committed block (`DelegatedSafetyConfig::enabled()`).
//! Keep it identical to that version. The one difference: transactions from `SYSTEM_CALLER` are
//! not guarded, because the pipe executes them outside grevm, where the guard never applied.

use crate::is_gravity_system_caller;
use revm::{
    bytecode::opcode::{CREATE, CREATE2},
    handler::instructions::EthInstructions,
    interpreter::{
        instructions::contract,
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, InterpreterTypes, RuntimeFlag},
        Host, Instruction, InstructionContext, InstructionExecResult, InstructionResult,
    },
    primitives::hardfork::SpecId,
};

/// Mainnet instructions with the CREATE/CREATE2 delegated-context guard.
pub(crate) fn gravity_instructions<CTX>(spec: SpecId) -> EthInstructions<EthInterpreter, CTX>
where
    CTX: Host,
{
    let mut instructions = EthInstructions::new_mainnet_with_spec(spec);
    instructions.insert_instruction(CREATE, Instruction::new(guarded_create::<false, _, _>), 0);
    instructions.insert_instruction(CREATE2, Instruction::new(guarded_create::<true, _, _>), 0);
    instructions
}

fn guarded_create<const IS_CREATE2: bool, WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) -> InstructionExecResult {
    if is_gravity_system_caller(context.host.caller()) {
        return contract::create::<IS_CREATE2, WIRE, H>(context)
    }

    if context.interpreter.runtime_flag.is_static() {
        return Err(InstructionResult::StateChangeDuringStaticCall)
    }

    if IS_CREATE2 && !context.interpreter.runtime_flag.spec_id().is_enabled_in(SpecId::PETERSBURG) {
        return Err(InstructionResult::NotActivated)
    }

    // `target_address` is the account owning this execution context. For a 7702 call it remains
    // the delegated EOA even though the interpreter executes bytecode loaded from its delegate.
    let recipient = context.interpreter.input.target_address();
    let Some(load) = context.host.load_account_delegated(recipient) else {
        return Err(InstructionResult::FatalExternalError)
    };

    // `Some(coldness)` means the target has an EIP-7702 delegation designator; the boolean itself
    // only reports whether loading the delegate was cold and is irrelevant to this policy.
    if load.is_delegate_account_cold.is_some() {
        return Err(InstructionResult::NotActivated)
    }

    contract::create::<IS_CREATE2, WIRE, H>(context)
}

/// The guard must behave exactly like grevm's: every case runs once through grevm's sequential
/// executor with the pipe's CREATE policy and once through [`GravityEvm`](super::GravityEvm).
#[cfg(test)]
mod tests {
    use crate::{
        gravity::test_utils::gravity_chain_spec, parallel_execute::GrevmExecutor, EthEvmConfig,
        GravityEvmConfig, SYSTEM_CALLER,
    };
    use alloc::{sync::Arc, vec, vec::Vec};
    use alloy_consensus::{Header, Signed, TxEip1559, TxEip7702};
    use alloy_eips::{eip7685::EMPTY_REQUESTS_HASH, eip7702::Authorization};
    use alloy_evm::{Evm, EvmFactory};
    use alloy_primitives::{address, Address, Signature, TxKind, B256, U256};
    use grevm::{DelegatedSafetyConfig, GrevmConfig};
    use reth_ethereum_primitives::{Block, BlockBody, TransactionSigned};
    use reth_evm::{parallel_execute::ParallelExecutor, ConfigureEvm};
    use reth_primitives_traits::{crypto::secp256k1::sign_message, Recovered, RecoveredBlock};
    use revm::{
        bytecode::Bytecode,
        database::{CacheDB, EmptyDB},
        state::AccountInfo,
    };

    const CAROL: Address = address!("00000000000000000000000000000000000ca201");
    /// Runtime: `CREATE(0, 0, 0)`.
    const FACTORY: Address = address!("00000000000000000000000000000000000fac70");
    /// Delegated to [`FACTORY`].
    const ERIN: Address = address!("00000000000000000000000000000000000e2100");
    /// Runtime: `DELEGATECALL` into [`FACTORY`].
    const FORWARDER: Address = address!("00000000000000000000000000000000000f0a2d");
    /// Delegated to [`FORWARDER`].
    const DAN: Address = address!("00000000000000000000000000000000000da400");
    /// Runtime: `CALL` into [`ERIN`].
    const CALLER: Address = address!("00000000000000000000000000000000000ca11e");

    fn push20_then(address: Address, tail: &[u8]) -> Vec<u8> {
        let mut code = vec![0x73];
        code.extend_from_slice(address.as_slice());
        code.extend_from_slice(tail);
        code
    }

    fn db() -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        let rich = AccountInfo { balance: U256::from(10u128.pow(24)), ..Default::default() };
        let code = |bytes: Vec<u8>| {
            let code = Bytecode::new_raw(bytes.into());
            AccountInfo { code_hash: code.hash_slow(), code: Some(code), ..Default::default() }
        };
        let delegated = |to| {
            let code = Bytecode::new_eip7702(to);
            AccountInfo { code_hash: code.hash_slow(), code: Some(code), ..Default::default() }
        };
        db.insert_account_info(CAROL, rich.clone());
        db.insert_account_info(SYSTEM_CALLER, rich);
        db.insert_account_info(FACTORY, code(vec![0x60, 0, 0x60, 0, 0x60, 0, 0xf0, 0x00]));
        db.insert_account_info(ERIN, delegated(FACTORY));
        // DELEGATECALL(gas, FACTORY, 0, 0, 0, 0)
        let forwarder =
            [vec![0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0], push20_then(FACTORY, &[0x5a, 0xf4, 0x00])];
        db.insert_account_info(FORWARDER, code(forwarder.concat()));
        db.insert_account_info(DAN, delegated(FORWARDER));
        // CALL(gas, ERIN, 0, 0, 0, 0, 0)
        let caller = [
            vec![0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0],
            push20_then(ERIN, &[0x5a, 0xf1, 0x00]),
        ];
        db.insert_account_info(CALLER, code(caller.concat()));
        db
    }

    fn header() -> Header {
        Header {
            number: 1,
            timestamp: 1,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1),
            requests_hash: Some(EMPTY_REQUESTS_HASH),
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        }
    }

    fn call(to: Address) -> TransactionSigned {
        let tx = TxEip1559 {
            chain_id: 1,
            gas_limit: 100_000,
            max_fee_per_gas: 1,
            to: TxKind::Call(to),
            ..Default::default()
        };
        TransactionSigned::Eip1559(Signed::new_unhashed(tx, Signature::test_signature()))
    }

    /// Delegates a fresh key's account to [`FACTORY`] and calls it in the same transaction.
    fn delegate_and_call() -> TransactionSigned {
        let key = B256::repeat_byte(0x42);
        let authorization = Authorization { chain_id: U256::from(1), address: FACTORY, nonce: 0 };
        let signature = sign_message(key, authorization.signature_hash()).unwrap();
        let authorization = authorization.into_signed(signature);
        let authority = authorization.recover_authority().unwrap();
        let tx = TxEip7702 {
            chain_id: 1,
            gas_limit: 100_000,
            max_fee_per_gas: 1,
            to: authority,
            authorization_list: vec![authorization],
            ..Default::default()
        };
        TransactionSigned::Eip7702(Signed::new_unhashed(tx, Signature::test_signature()))
    }

    /// `(success, gas used)` under grevm with the pipe's CREATE policy.
    fn grevm(sender: Address, tx: TransactionSigned) -> (bool, u64) {
        let chain_spec = gravity_chain_spec();
        let evm_config = EthEvmConfig::new(chain_spec.clone());
        let mut config =
            GrevmConfig::from_env().with_delegated_safety(DelegatedSafetyConfig::create_only());
        config.force_sequential = true;
        let mut executor =
            GrevmExecutor::new_with_runtime_config(chain_spec, &evm_config, db(), config);
        let block = RecoveredBlock::new_unhashed(
            Block {
                header: header(),
                body: BlockBody { transactions: vec![tx], ..Default::default() },
            },
            vec![sender],
        );
        let receipt = executor.execute(&block).unwrap().result.receipts.remove(0);
        (receipt.success, receipt.cumulative_gas_used)
    }

    /// `(success, gas used)` under [`GravityEvm`](super::super::GravityEvm).
    fn gravity(sender: Address, tx: TransactionSigned) -> (bool, u64) {
        let config = GravityEvmConfig::new(
            gravity_chain_spec(),
            Arc::new(crate::gravity::test_utils::MockChainReader::default()),
        );
        let mut evm = config.evm_with_env(db(), config.evm_env(&header()).unwrap());
        let result = evm.transact(Recovered::new_unchecked(&tx, sender)).unwrap().result;
        (result.is_success(), result.tx_gas_used())
    }

    #[test]
    fn matches_grevm_on_delegated_create() {
        let cases = [
            ("call a delegated account whose code creates", CAROL, call(ERIN), false),
            ("delegated code delegatecalls a creator", CAROL, call(DAN), true),
            ("delegation set in the same transaction", CAROL, delegate_and_call(), false),
            ("inner call into a delegated account", CAROL, call(CALLER), true),
            ("plain contract creates", CAROL, call(FACTORY), true),
        ];
        for (label, sender, tx, success) in cases {
            let expected = grevm(sender, tx.clone());
            assert_eq!(gravity(sender, tx), expected, "{label}");
            assert_eq!(expected.0, success, "{label}: unexpected grevm outcome");
        }
        // Halting consumes the whole gas limit.
        assert_eq!(gravity(CAROL, call(ERIN)), (false, 100_000));
    }

    /// The pipe runs system transactions outside grevm, so the guard never applied to them.
    #[test]
    fn system_caller_is_not_guarded() {
        let tx = call(ERIN);
        let header = header();
        let env = EthEvmConfig::new(gravity_chain_spec()).evm_env(&header).unwrap();
        let unguarded = alloy_evm::EthEvmFactory::default()
            .create_evm(db(), env)
            .transact(Recovered::new_unchecked(&tx, SYSTEM_CALLER))
            .unwrap()
            .result;

        assert_eq!(gravity(SYSTEM_CALLER, tx), (true, unguarded.tx_gas_used()));
        assert!(unguarded.is_success());
    }
}
