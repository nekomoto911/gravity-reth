//! Reads of the committed chain for scenarios, over the node's JSON-RPC.
//!
//! These are state, block and receipt lookups, not replays: they return what the pipe
//! committed. A failed read means the harness cannot see the chain at all, so it panics.

use crate::rpc::RpcClient;
use alloy_primitives::{Address, Bytes, B256, U256, U64};
use alloy_rpc_types_eth::{Block, TransactionReceipt};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

/// JSON-RPC error code reth gives a reverted call, and nothing else.
const REVERT_ERROR_CODE: i64 = 3;

/// The committed chain as scenarios see it, and the chain id they sign for.
pub(crate) struct Chain<'a> {
    rpc: &'a RpcClient,
    pub(crate) chain_id: u64,
}

impl<'a> Chain<'a> {
    pub(crate) const fn new(rpc: &'a RpcClient, chain_id: u64) -> Self {
        Self { rpc, chain_id }
    }

    /// Nonce of `address` after block `number`.
    pub(crate) fn nonce(&self, address: Address, number: u64) -> u64 {
        self.read::<U64>("eth_getTransactionCount", json!([address, U64::from(number)])).to()
    }

    /// Balance of `address` after block `number`.
    pub(crate) fn balance(&self, address: Address, number: u64) -> U256 {
        self.read("eth_getBalance", json!([address, U64::from(number)]))
    }

    /// Code of `address` after block `number`.
    pub(crate) fn code(&self, address: Address, number: u64) -> Bytes {
        self.read("eth_getCode", json!([address, U64::from(number)]))
    }

    /// Storage `slot` of `address` after block `number`.
    pub(crate) fn storage(&self, address: Address, slot: U256, number: u64) -> B256 {
        self.read("eth_getStorageAt", json!([address, slot, U64::from(number)]))
    }

    /// Header and transaction hashes of block `number`.
    pub(crate) fn block(&self, number: u64) -> Block {
        self.read("eth_getBlockByNumber", json!([U64::from(number), false]))
    }

    pub(crate) fn receipts(&self, number: u64) -> Vec<TransactionReceipt> {
        self.read("eth_getBlockReceipts", json!([U64::from(number)]))
    }

    /// Receipt of a transaction, `None` if no committed block includes it.
    pub(crate) fn receipt(&self, tx_hash: B256) -> Option<TransactionReceipt> {
        self.read("eth_getTransactionReceipt", json!([tx_hash]))
    }

    /// Output of calling `to` with `input` on the state after block `number`.
    pub(crate) fn call(&self, to: Address, input: Bytes, number: u64) -> Bytes {
        self.call_or_revert(to, input, number)
            .unwrap_or_else(|| panic!("eth_call to {to} reverted at block {number}"))
    }

    /// Like [`Self::call`], but a revert is an answer too: `None`.
    pub(crate) fn call_or_revert(&self, to: Address, input: Bytes, number: u64) -> Option<Bytes> {
        let params = json!([{ "to": to, "input": input }, U64::from(number)]);
        match self.rpc.call("eth_call", params.clone()) {
            Ok(output) => Some(output),
            Err(error) if is_revert(&error) => None,
            Err(error) => panic!("eth_call {params} failed: {error}"),
        }
    }

    fn read<T: DeserializeOwned>(&self, method: &str, params: Value) -> T {
        self.rpc
            .call(method, params.clone())
            .unwrap_or_else(|error| panic!("{method} {params} failed: {error}"))
    }
}

/// Whether a JSON-RPC error is reth's answer to a reverted call.
fn is_revert(error: &str) -> bool {
    serde_json::from_str::<Value>(error).is_ok_and(|error| error["code"] == REVERT_ERROR_CODE)
}
