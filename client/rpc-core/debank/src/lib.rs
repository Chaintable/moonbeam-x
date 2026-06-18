// Copyright 2019-2025 PureStake Inc.
// This file is part of Moonbeam.

// Moonbeam is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Moonbeam is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Moonbeam.  If not, see <http://www.gnu.org/licenses/>.

//! DeBank simulation RPC: `debank_simulateTransactions`,
//! `debank_contractMultiCall`, `debank_estimateGas`.
//!
//! Wire-compatible with the DeBank RPC standard. The canonical reference is
//! leafage-evm's `crates/leafage-evm-types/src/rpc/debank.rs`. Field names and
//! JSON tags must match exactly so `nodex-proxy` can forward requests/responses
//! without chain-specific knowledge.
//!
//! Method names carry the `debank_` prefix: this node sits behind `noderpcx`,
//! whose `cosmosMethodRewrite` renames the bare `simulateTransactions` /
//! `contractMultiCall` / `estimateGas` into the `debank_`-prefixed form before
//! forwarding here.

use ethereum_types::{H160, H256, U256};
use fc_rpc_core::types::{BlockNumberOrHash, CallStateOverride, TransactionRequest};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// DeBank wire error codes (mirrors leafage `DebankErrorCode`). `0` means success.
pub const DEBANK_ERR_REVERTED: i32 = -39000; // EvmRevert
pub const DEBANK_ERR_GAS_EXHAUSTED: i32 = -39001; // GasExhausted
pub const DEBANK_ERR_INSUFFICIENT_BALANCE: i32 = -39002; // BalanceExhausted
pub const DEBANK_ERR_NONCE: i32 = -39003; // NonceError
pub const DEBANK_ERR_UNKNOWN: i32 = -39004; // EvmFailed
pub const DEBANK_ERR_UNSUPPORTED_PRECOMPILE: i32 = -39008; // UnsupportedPrecompile

/// How `block_id` in [`DebankBlockContext`] is interpreted.
///
/// Serializes/deserializes to the PascalCase strings `"Equals"` / `"Contains"`
/// to match leafage's `BlockType`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum BlockType {
	/// Resolve state at exactly `block_id`.
	#[default]
	Equals,
	/// Any block containing the state; resolves to latest.
	Contains,
}

/// DeBank block context: `{ "block_id": <eth block id>, "type": "Equals"|"Contains" }`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DebankBlockContext {
	#[serde(default)]
	pub block_id: BlockNumberOrHash,
	#[serde(rename = "type", default)]
	pub block_type: BlockType,
}

impl DebankBlockContext {
	/// Resolve to an eth `BlockNumberOrHash`. `Contains` (and any non-`Equals`
	/// value) falls back to latest, matching leafage semantics.
	pub fn to_block_number_or_hash(self) -> BlockNumberOrHash {
		match self.block_type {
			BlockType::Equals => self.block_id,
			BlockType::Contains => BlockNumberOrHash::Latest,
		}
	}
}

/// Per-call trace entry. Field names match leafage's `DebankTrace`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DebankTrace {
	pub id: String,
	pub from_addr: H160,
	pub gas_limit: u64,
	#[serde(serialize_with = "serialize_bytes")]
	pub input: Vec<u8>,
	pub to_addr: H160,
	pub value: U256,
	pub gas_used: u64,
	#[serde(serialize_with = "serialize_bytes")]
	pub output: Vec<u8>,
	#[serde(rename = "type")]
	pub call_create_type: String,
	pub call_type: String,
	pub tx_id: H256,
	pub parent_trace_id: String,
	pub pos_in_parent_trace: usize,
	pub self_storage_change: bool,
	pub storage_change: bool,
}

/// Per-log entry. Field names match leafage's `DebankEvent`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DebankEvent {
	pub id: String,
	pub contract_id: H160,
	pub selector: String,
	pub topics: Vec<String>,
	#[serde(serialize_with = "serialize_bytes")]
	pub data: Vec<u8>,
	pub tx_id: H256,
	pub parent_trace_id: String,
	pub pos_in_parent_trace: usize,
}

/// One simulated transaction's result.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DebankSingleSimulateResult {
	pub code: i32,
	pub err: String,
	pub gas_used: u64,
	pub traces: Vec<DebankTrace>,
	pub events: Vec<DebankEvent>,
}

/// Stats block returned alongside `debank_simulateTransactions` results.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DebankSimulateStats {
	pub block_num: u64,
	pub block_hash: H256,
	pub block_time: u64,
	pub success: bool,
}

/// `debank_simulateTransactions` response.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DebankSimulateResp {
	pub results: Vec<DebankSingleSimulateResult>,
	pub stats: DebankSimulateStats,
}

/// One `debank_contractMultiCall` result.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DebankSingleCallResult {
	pub code: i32,
	pub err: String,
	pub from_cache: bool,
	#[serde(serialize_with = "serialize_bytes")]
	pub result: Vec<u8>,
	pub gas_used: i64,
	pub time_cost: f64,
}

/// Stats block returned alongside `debank_contractMultiCall` results.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DebankMultiCallStats {
	pub block_num: u64,
	pub block_hash: H256,
	pub block_time: u64,
	pub success: bool,
	pub cache_enabled: bool,
}

/// `debank_contractMultiCall` response.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DebankMultiCallResp {
	pub results: Vec<DebankSingleCallResult>,
	pub stats: DebankMultiCallStats,
}

#[rpc(server)]
#[jsonrpsee::core::async_trait]
pub trait DebankApi {
	/// Batch-simulate transactions where each tx sees the state changes of the
	/// previous ones (e.g. approve → swap). Returns per-tx call-tree + events.
	#[method(name = "debank_simulateTransactions")]
	async fn simulate_transactions(
		&self,
		requests: Vec<TransactionRequest>,
		block_ctx: Option<DebankBlockContext>,
		block_overrides: Option<serde_json::Value>,
	) -> RpcResult<DebankSimulateResp>;

	/// Batch read-only calls, each isolated against the same block state.
	#[method(name = "debank_contractMultiCall")]
	async fn contract_multi_call(
		&self,
		requests: Vec<TransactionRequest>,
		block_ctx: Option<DebankBlockContext>,
		block_overrides: Option<serde_json::Value>,
		state_override: Option<BTreeMap<H160, CallStateOverride>>,
		fast_fail: Option<bool>,
		use_parallel: Option<bool>,
		disable_cache: Option<bool>,
	) -> RpcResult<DebankMultiCallResp>;

	/// Estimate gas for a single call. Returns the gas amount as a U256 quantity.
	#[method(name = "debank_estimateGas")]
	async fn estimate_gas(
		&self,
		request: TransactionRequest,
		block_ctx: Option<DebankBlockContext>,
		block_overrides: Option<serde_json::Value>,
	) -> RpcResult<U256>;
}

/// Serialize bytes as a `0x`-prefixed lowercase hex string.
fn serialize_bytes<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
	S: serde::Serializer,
{
	serializer.serialize_str(&format!("0x{}", hex::encode(bytes)))
}
