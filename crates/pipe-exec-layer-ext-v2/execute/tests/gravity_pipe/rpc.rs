//! Blocking JSON-RPC client for the node's HTTP endpoint, the same interface a mainnet
//! RPC node serves.

use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};

/// Every call blocks the timeline, so a hung endpoint must fail its call before it pushes later
/// blocks past their phase boundaries or the run past nextest's ceiling. Still generous for a
/// local node, whose slowest calls re-execute a hundred blocks.
const TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct RpcClient {
    agent: ureq::Agent,
    url: String,
}

impl RpcClient {
    pub(crate) fn new(addr: SocketAddr) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
        Self { agent, url: format!("http://{addr}") }
    }

    /// Calls `method` and decodes its result.
    ///
    /// Every failure (transport, JSON-RPC error, or a result of an unexpected shape)
    /// comes back as text: callers record it as a mismatch and keep going.
    pub(crate) fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, String> {
        let request = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut response: Value = self
            .agent
            .post(&self.url)
            .send_json(request)
            .map_err(|err| format!("transport error: {err}"))?
            .into_json()
            .map_err(|err| format!("unreadable response: {err}"))?;
        if let Some(error) = response.get("error") {
            return Err(error.to_string());
        }
        serde_json::from_value(response["result"].take())
            .map_err(|err| format!("undecodable result: {err}"))
    }
}
