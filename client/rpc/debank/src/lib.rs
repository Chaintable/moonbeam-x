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

//! Implementation of the DeBank simulation RPC (`debank_*`).
//!
//! - `debank_simulateTransactions`: replays a batch of calls through
//!   `DebugRuntimeApi::trace_call` on a *single, reused* `ApiRef`, so each call
//!   observes the overlay state changes of the previous ones (approve → swap).
//!   The per-call call-tree is collected by the debank `Listener` and formatted
//!   into `DebankTrace` / `DebankEvent`.
//! - `debank_contractMultiCall`: runs each call through
//!   `EthereumRuntimeRPCApi::call` on a *fresh* `ApiRef` (isolated, no
//!   accumulation), returning raw output bytes + gas.
//! - `debank_estimateGas`: delegates to the shared `Eth` instance's
//!   `estimate_gas` (binary search + EstimateGasAdapter) via `EthDataProvider`.

use std::{marker::PhantomData, sync::Arc};

use ethereum_types::{H160, H256, U256};
use evm::{ExitError, ExitReason};
use jsonrpsee::{
	core::{async_trait, RpcResult},
	types::{error::INTERNAL_ERROR_CODE, ErrorObject, ErrorObjectOwned},
};

use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT};

use fp_rpc::EthereumRuntimeRPCApi;
use moonbeam_client_evm_tracing::{
	formatters::debank::{
		frame_to_debank, DebankEvent as FmtEvent, DebankTrace as FmtTrace,
	},
	listeners::debank::{CallFrame, Listener as DebankListener},
};
use moonbeam_rpc_primitives_debug::DebugRuntimeApi;
use moonbeam_rpc_trace::EthDataProvider;

pub use moonbeam_rpc_core_debank::DebankApiServer;
use moonbeam_rpc_core_debank::{
	DebankBlockContext, DebankEvent, DebankMultiCallResp, DebankMultiCallStats, DebankSimulateResp,
	DebankSimulateStats, DebankSingleCallResult, DebankSingleSimulateResult, DebankTrace,
	DEBANK_ERR_GAS_EXHAUSTED, DEBANK_ERR_INSUFFICIENT_BALANCE, DEBANK_ERR_REVERTED,
	DEBANK_ERR_UNKNOWN,
};

use fc_rpc_core::types::{BlockNumberOrHash, TransactionRequest};

/// Default cap on batch size for simulate / multiCall, mirroring leafage's
/// `debankBatchMaxSize`. Prevents a single request from monopolising the node.
pub const DEFAULT_MAX_BATCH_SIZE: usize = 50;

/// RPC handler for the `debank_*` methods.
pub struct Debank<B, C> {
	client: Arc<C>,
	frontier_backend: Arc<dyn fc_api::Backend<B>>,
	eth_provider: Arc<dyn EthDataProvider>,
	max_batch_size: usize,
	_phantom: PhantomData<B>,
}

impl<B, C> Clone for Debank<B, C> {
	fn clone(&self) -> Self {
		Self {
			client: Arc::clone(&self.client),
			frontier_backend: Arc::clone(&self.frontier_backend),
			eth_provider: Arc::clone(&self.eth_provider),
			max_batch_size: self.max_batch_size,
			_phantom: PhantomData,
		}
	}
}

impl<B, C> Debank<B, C>
where
	B: BlockT<Hash = H256>,
	C: HeaderBackend<B> + 'static,
{
	pub fn new(
		client: Arc<C>,
		frontier_backend: Arc<dyn fc_api::Backend<B>>,
		eth_provider: Arc<dyn EthDataProvider>,
		max_batch_size: usize,
	) -> Self {
		Self {
			client,
			frontier_backend,
			eth_provider,
			max_batch_size,
			_phantom: PhantomData,
		}
	}

	/// Resolve a DeBank block context to a Substrate block hash.
	async fn resolve_hash(
		&self,
		number_or_hash: Option<BlockNumberOrHash>,
	) -> RpcResult<B::Hash> {
		let id = fc_rpc::frontier_backend_client::native_block_id::<B, C>(
			self.client.as_ref(),
			self.frontier_backend.as_ref(),
			number_or_hash,
		)
		.await?;
		match id {
			Some(block_id) => self
				.client
				.expect_block_hash_from_id(&block_id)
				.map_err(|e| internal_err(format!("debank: block not found: {:?}", e))),
			None => Ok(self.client.info().best_hash),
		}
	}
}

#[async_trait]
impl<B, C> DebankApiServer for Debank<B, C>
where
	B: BlockT<Hash = H256> + Send + Sync + 'static,
	B::Header: HeaderT<Number = u32>,
	C: ProvideRuntimeApi<B> + HeaderBackend<B> + Send + Sync + 'static,
	C::Api: EthereumRuntimeRPCApi<B> + DebugRuntimeApi<B>,
{
	async fn simulate_transactions(
		&self,
		requests: Vec<TransactionRequest>,
		block_ctx: Option<DebankBlockContext>,
		_block_overrides: Option<serde_json::Value>,
	) -> RpcResult<DebankSimulateResp> {
		if requests.len() > self.max_batch_size {
			return Err(internal_err(format!(
				"debank: simulateTransactions accepts at most {} calls",
				self.max_batch_size
			)));
		}
		let number_or_hash = block_ctx.map(|c| c.to_block_number_or_hash());
		let substrate_hash = self.resolve_hash(number_or_hash).await?;
		let client = Arc::clone(&self.client);

		tokio::task::spawn_blocking(move || simulate_blocking::<B, C>(client, substrate_hash, requests))
			.await
			.map_err(|e| internal_err(format!("debank: simulate task panicked: {:?}", e)))?
	}

	async fn contract_multi_call(
		&self,
		requests: Vec<TransactionRequest>,
		block_ctx: Option<DebankBlockContext>,
		_block_overrides: Option<serde_json::Value>,
		state_override: Option<std::collections::BTreeMap<H160, fc_rpc_core::types::CallStateOverride>>,
		fast_fail: Option<bool>,
		_use_parallel: Option<bool>,
		_disable_cache: Option<bool>,
	) -> RpcResult<DebankMultiCallResp> {
		if requests.len() > self.max_batch_size {
			return Err(internal_err(format!(
				"debank: contractMultiCall accepts at most {} calls",
				self.max_batch_size
			)));
		}
		// State overrides are advertised by the standard multiCall signature but
		// not yet wired through here; reject rather than silently returning
		// results computed against un-overridden chain state.
		if state_override.as_ref().map_or(false, |s| !s.is_empty()) {
			return Err(internal_err(
				"debank: contractMultiCall state_override is not yet supported",
			));
		}
		let number_or_hash = block_ctx.map(|c| c.to_block_number_or_hash());
		let substrate_hash = self.resolve_hash(number_or_hash).await?;
		let client = Arc::clone(&self.client);
		let fast_fail = fast_fail.unwrap_or(false);

		tokio::task::spawn_blocking(move || {
			multicall_blocking::<B, C>(client, substrate_hash, requests, fast_fail)
		})
		.await
		.map_err(|e| internal_err(format!("debank: multiCall task panicked: {:?}", e)))?
	}

	async fn estimate_gas(
		&self,
		request: TransactionRequest,
		block_ctx: Option<DebankBlockContext>,
		_block_overrides: Option<serde_json::Value>,
	) -> RpcResult<U256> {
		let number_or_hash = block_ctx.map(|c| c.to_block_number_or_hash());
		self.eth_provider.estimate_gas(request, number_or_hash).await
	}
}

/// Runs the full simulate batch on one reused `ApiRef` so state accumulates.
fn simulate_blocking<B, C>(
	client: Arc<C>,
	substrate_hash: B::Hash,
	requests: Vec<TransactionRequest>,
) -> RpcResult<DebankSimulateResp>
where
	B: BlockT<Hash = H256>,
	B::Header: HeaderT<Number = u32>,
	C: ProvideRuntimeApi<B> + HeaderBackend<B>,
	C::Api: EthereumRuntimeRPCApi<B> + DebugRuntimeApi<B>,
{
	let header = client
		.header(substrate_hash)
		.map_err(|e| internal_err(format!("debank: header fetch failed: {:?}", e)))?
		.ok_or_else(|| internal_err("debank: block header not found"))?;
	let parent_hash = *header.parent_hash();
	let height = *header.number() as u64;

	// Single ApiRef reused across all calls -> overlay state accumulates.
	let api = client.runtime_api();

	let (block_gas_limit, block_time, block_hash) =
		block_context::<B, C::Api>(&*api, substrate_hash);

	let mut results: Vec<DebankSingleSimulateResult> = Vec::with_capacity(requests.len());
	let mut short_circuit: Option<DebankSingleSimulateResult> = None;

	for (idx, req) in requests.into_iter().enumerate() {
		if let Some(prev) = &short_circuit {
			results.push(prev.clone());
			continue;
		}
		let tx_id = H256::from_low_u64_be((idx as u64) + 1);
		let result =
			simulate_one::<B, C::Api>(&*api, parent_hash, &header, req, tx_id, block_gas_limit);
		if result.code != 0 {
			short_circuit = Some(result.clone());
		}
		results.push(result);
	}

	let success = results.iter().all(|r| r.code == 0);
	Ok(DebankSimulateResp {
		results,
		stats: DebankSimulateStats {
			block_num: height,
			block_hash,
			block_time,
			success,
		},
	})
}

/// Simulate one call via `trace_call`; the call-tree is collected via the
/// debank `Listener`. State written here persists in `api`'s overlay for the
/// next call in the batch.
fn simulate_one<B, A>(
	api: &A,
	parent_hash: B::Hash,
	header: &B::Header,
	req: TransactionRequest,
	tx_id: H256,
	block_gas_limit: U256,
) -> DebankSingleSimulateResult
where
	B: BlockT<Hash = H256>,
	A: DebugRuntimeApi<B>,
{
	let to = match req.to {
		Some(t) => t,
		None => {
			return err_simulate(
				DEBANK_ERR_UNKNOWN,
				"debank: contract creation not supported in simulateTransactions",
			)
		}
	};
	let from = req.from.unwrap_or_default();
	let data = req.data.into_bytes().map(|b| b.0).unwrap_or_default();
	let value = req.value.unwrap_or_default();
	let gas_limit = req.gas.unwrap_or(block_gas_limit);
	let (max_fee, max_prio) =
		normalize_fees(req.gas_price, req.max_fee_per_gas, req.max_priority_fee_per_gas);
	let nonce = req.nonce;
	let access_list = req
		.access_list
		.map(|l| l.into_iter().map(|i| (i.address, i.storage_keys)).collect());

	let mut listener = DebankListener::new();
	let api_result = listener.using(|| {
		api.trace_call(
			parent_hash,
			header,
			from,
			to,
			data,
			value,
			gas_limit,
			max_fee,
			max_prio,
			nonce,
			access_list,
			None,
		)
	});
	listener.finish_transaction();

	match api_result {
		Ok(Ok(())) => {}
		Ok(Err(e)) => {
			return err_simulate(DEBANK_ERR_UNKNOWN, format!("debank: dispatch error: {:?}", e))
		}
		Err(e) => {
			return err_simulate(
				DEBANK_ERR_UNKNOWN,
				format!("debank: trace_call api error: {:?}", e),
			)
		}
	}

	let frame = match listener.completed_frames.into_iter().next() {
		Some(f) => f,
		None => return err_simulate(DEBANK_ERR_UNKNOWN, "debank: no trace produced"),
	};

	let (code, err) = frame_code(&frame);
	let gas_used = frame.gas_used;
	let (fmt_traces, fmt_events) = frame_to_debank(&frame, tx_id);
	let traces = fmt_traces.into_iter().map(to_wire_trace).collect();
	// Reverted/halted txs emit no logs (Ethereum semantics), matching leafage.
	let events = if code == 0 {
		fmt_events.into_iter().map(|e| to_wire_event(e, tx_id)).collect()
	} else {
		Vec::new()
	};

	DebankSingleSimulateResult {
		code,
		err,
		gas_used,
		traces,
		events,
	}
}

/// Runs each multiCall on a fresh `ApiRef` so calls stay isolated.
fn multicall_blocking<B, C>(
	client: Arc<C>,
	substrate_hash: B::Hash,
	requests: Vec<TransactionRequest>,
	fast_fail: bool,
) -> RpcResult<DebankMultiCallResp>
where
	B: BlockT<Hash = H256>,
	B::Header: HeaderT<Number = u32>,
	C: ProvideRuntimeApi<B> + HeaderBackend<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
{
	let header = client
		.header(substrate_hash)
		.map_err(|e| internal_err(format!("debank: header fetch failed: {:?}", e)))?
		.ok_or_else(|| internal_err("debank: block header not found"))?;
	let height = *header.number() as u64;

	let (block_gas_limit, block_time, block_hash) = {
		let api = client.runtime_api();
		block_context::<B, C::Api>(&*api, substrate_hash)
	};

	let mut results: Vec<DebankSingleCallResult> = Vec::with_capacity(requests.len());
	let mut short_circuit: Option<DebankSingleCallResult> = None;

	for req in requests.into_iter() {
		if fast_fail {
			if let Some(prev) = &short_circuit {
				results.push(prev.clone());
				continue;
			}
		}
		// Fresh ApiRef per call -> each call reads the block-end state, isolated.
		let api = client.runtime_api();
		let result = multicall_one::<B, C::Api>(&*api, substrate_hash, req, block_gas_limit);
		if fast_fail && result.code != 0 {
			short_circuit = Some(result.clone());
		}
		results.push(result);
	}

	let success = results.iter().all(|r| r.code == 0);
	Ok(DebankMultiCallResp {
		results,
		stats: DebankMultiCallStats {
			block_num: height,
			block_hash,
			block_time,
			success,
			cache_enabled: false,
		},
	})
}

fn multicall_one<B, A>(
	api: &A,
	at: B::Hash,
	req: TransactionRequest,
	block_gas_limit: U256,
) -> DebankSingleCallResult
where
	B: BlockT<Hash = H256>,
	A: EthereumRuntimeRPCApi<B>,
{
	let to = match req.to {
		Some(t) => t,
		None => return err_call(DEBANK_ERR_UNKNOWN, "debank: 'to' address required"),
	};
	let from = req.from.unwrap_or_default();
	let data = req.data.into_bytes().map(|b| b.0).unwrap_or_default();
	let value = req.value.unwrap_or_default();
	let gas_limit = req.gas.unwrap_or(block_gas_limit);
	let (max_fee, max_prio) =
		normalize_fees(req.gas_price, req.max_fee_per_gas, req.max_priority_fee_per_gas);
	let nonce = req.nonce;
	let access_list = req
		.access_list
		.map(|l| l.into_iter().map(|i| (i.address, i.storage_keys)).collect());

	let info = api.call(
		at,
		from,
		to,
		data,
		value,
		gas_limit,
		max_fee,
		max_prio,
		nonce,
		false,
		access_list,
		None,
	);

	match info {
		Ok(Ok(call_info)) => {
			let (code, err) = exit_reason_to_code(&call_info.exit_reason);
			DebankSingleCallResult {
				code,
				err,
				from_cache: false,
				result: call_info.value,
				gas_used: call_info.used_gas.standard.low_u64() as i64,
				time_cost: 0.0,
			}
		}
		Ok(Err(e)) => err_call(DEBANK_ERR_UNKNOWN, format!("debank: dispatch error: {:?}", e)),
		Err(e) => err_call(DEBANK_ERR_UNKNOWN, format!("debank: call api error: {:?}", e)),
	}
}

/// Fetch (gas_limit, block_time_secs, eth_block_hash) of the ethereum block at
/// `at`, with fallbacks. `block_time` is converted ms -> s to match DeBank.
fn block_context<B, A>(api: &A, at: B::Hash) -> (U256, u64, H256)
where
	B: BlockT<Hash = H256>,
	A: EthereumRuntimeRPCApi<B>,
{
	match api.current_block(at) {
		Ok(Some(block)) => (
			block.header.gas_limit,
			// pallet_ethereum stores the timestamp in milliseconds (set from
			// pallet_timestamp); DeBank stats expect seconds, matching the
			// existing trace_debankBlock path which divides by 1000.
			block.header.timestamp / 1000,
			// DeBank/eth clients correlate on the Ethereum block hash, not the
			// Substrate block hash; compute it from the ethereum header.
			block.header.hash(),
		),
		// Fall back to the substrate hash only when the eth block is unavailable.
		_ => (U256::from(u64::MAX), 0, at),
	}
}

/// Map a root call frame's failure state to a DeBank error code + message.
fn frame_code(frame: &CallFrame) -> (i32, String) {
	if !frame.failed {
		return (0, String::new());
	}
	let code = match frame.error.as_str() {
		"execution reverted" => DEBANK_ERR_REVERTED,
		"out of gas" => DEBANK_ERR_GAS_EXHAUSTED,
		"out of funds" => DEBANK_ERR_INSUFFICIENT_BALANCE,
		_ => DEBANK_ERR_UNKNOWN,
	};
	(code, frame.error.clone())
}

/// Map an EVM `ExitReason` to a DeBank error code + message.
fn exit_reason_to_code(reason: &ExitReason) -> (i32, String) {
	match reason {
		ExitReason::Succeed(_) => (0, String::new()),
		ExitReason::Revert(_) => (DEBANK_ERR_REVERTED, "execution reverted".to_string()),
		ExitReason::Error(e) => {
			let code = match e {
				ExitError::OutOfGas => DEBANK_ERR_GAS_EXHAUSTED,
				ExitError::OutOfFund => DEBANK_ERR_INSUFFICIENT_BALANCE,
				_ => DEBANK_ERR_UNKNOWN,
			};
			(code, format!("{:?}", e))
		}
		ExitReason::Fatal(e) => (DEBANK_ERR_UNKNOWN, format!("{:?}", e)),
	}
}

/// Legacy gas_price collapses into (max_fee, max_priority); EIP-1559 fields pass
/// through. Mirrors frontier's `Eth::call` fee handling.
fn normalize_fees(
	gas_price: Option<U256>,
	max_fee: Option<U256>,
	max_prio: Option<U256>,
) -> (Option<U256>, Option<U256>) {
	match (gas_price, max_fee, max_prio) {
		(gp, None, None) => {
			let gp = gp.filter(|v| !v.is_zero());
			(gp, gp)
		}
		(_, mf, mp) => (mf, mp),
	}
}

fn to_wire_trace(t: FmtTrace) -> DebankTrace {
	DebankTrace {
		id: t.id,
		from_addr: t.from_addr,
		gas_limit: t.gas_limit,
		input: t.input,
		to_addr: t.to_addr,
		value: t.value,
		gas_used: t.gas_used,
		output: t.output,
		call_create_type: t.call_create_type,
		call_type: t.call_type,
		tx_id: t.tx_id,
		parent_trace_id: t.parent_trace_id,
		pos_in_parent_trace: t.pos_in_parent_trace,
		self_storage_change: t.self_storage_change,
		storage_change: t.storage_change,
	}
}

fn to_wire_event(e: FmtEvent, tx_id: H256) -> DebankEvent {
	DebankEvent {
		id: e.id,
		contract_id: e.contract_id,
		selector: e.selector,
		topics: e.topics,
		data: e.data,
		tx_id,
		parent_trace_id: e.parent_trace_id,
		pos_in_parent_trace: e.pos_in_parent_trace,
	}
}

fn internal_err(msg: impl Into<String>) -> ErrorObjectOwned {
	ErrorObject::owned(INTERNAL_ERROR_CODE, msg.into(), None::<()>)
}

fn err_simulate(code: i32, msg: impl Into<String>) -> DebankSingleSimulateResult {
	DebankSingleSimulateResult {
		code,
		err: msg.into(),
		gas_used: 0,
		traces: Vec::new(),
		events: Vec::new(),
	}
}

fn err_call(code: i32, msg: impl Into<String>) -> DebankSingleCallResult {
	DebankSingleCallResult {
		code,
		err: msg.into(),
		from_cache: false,
		result: Vec::new(),
		gas_used: 0,
		time_cost: 0.0,
	}
}